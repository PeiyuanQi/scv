//! SCV's clients for people: the terminal UI ([`run_tui`]), which attaches to
//! the daemon socket, and the headless one-prompt client behind `scv exec`
//! ([`run_exec`]), which starts its own `scv server --stdio`. Both render
//! server events and keep no authoritative conversation state.
//!
//! The terminal UI is a dashboard of tabs: chat tabs, each its own daemon
//! session in its own workspace, and agent views of the daemon's workbench
//! (delegated agents' live logs, background jobs, approvals). It is split by
//! concern: `client` talks to a chat's server, `poller` is the dashboard's
//! own connection for the workbench, `app` holds a chat's display state and
//! applies server events, `workbench` holds the workbench's, `ui` routes
//! keys between tabs, panes, and the sidebar (`keys` holds the bindings),
//! `input` handles a chat's keys, `render` draws, and `transcript` bounds
//! what is kept.

#![forbid(unsafe_code)]

mod app;
mod attach;
mod client;
mod exec;
mod input;
mod keys;
mod poller;
mod render;
mod terminal;
mod transcript;
mod ui;
mod workbench;

use std::{future::Future, path::Path, pin::Pin, time::Duration};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::{StreamExt, future::select_all};
use ratatui::{Terminal, backend::CrosstermBackend};
use scv_protocol::ServerEvent;

use crate::{
    app::App,
    client::{Client, SessionInfo, reconnect_restoring},
    render::render_ui,
    terminal::TerminalGuard,
    ui::{ChatTab, Opening, TabKind, Ui},
};

pub use attach::run_attach;
pub use client::LaunchOptions;
pub use exec::run_exec;

/// A chat tab's connection to come.
type Opened = Pin<Box<dyn Future<Output = Result<(Client, SessionInfo)>> + Send>>;

/// A chat tab being connected: its opening, and the connection to come.
type Connecting = (Opening, Opened);

/// Run the terminal UI against the daemon listening on `socket`,
/// reconnecting if it restarts.
pub async fn run_tui(socket: &Path, cwd: &Path, options: LaunchOptions) -> Result<()> {
    let (client, session) = Client::connect(socket, cwd, &options).await?;
    let keys = session.keys.clone();
    let mut terminal = TerminalGuard::enter()?;
    let mut ui = Ui::new(
        ChatTab {
            client,
            app: App::new(session),
            cwd: cwd.to_owned(),
        },
        &keys,
        options,
    );
    let (requests, mut answers) = poller::spawn(socket.to_owned());
    ui.requests = Some(requests);
    let (terminal_requests, mut terminal_answers) = poller::spawn(socket.to_owned());
    ui.terminal_requests = Some(terminal_requests);
    let (takeover_requests, mut takeover_answers) = poller::spawn(socket.to_owned());
    ui.takeover_requests = Some(takeover_requests);
    let result = run_event_loop(
        &mut terminal.terminal,
        &mut ui,
        &mut answers,
        &mut terminal_answers,
        &mut takeover_answers,
        socket,
    )
    .await;
    for tab in &mut ui.tabs {
        if let TabKind::Chat(chat) = &mut tab.kind {
            chat.client.shutdown().await;
        }
    }
    result
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ui: &mut Ui,
    answers: &mut tokio::sync::mpsc::UnboundedReceiver<poller::Answer>,
    terminal_answers: &mut tokio::sync::mpsc::UnboundedReceiver<poller::Answer>,
    takeover_answers: &mut tokio::sync::mpsc::UnboundedReceiver<poller::Answer>,
    socket: &Path,
) -> Result<()> {
    let mut terminal_events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    // An attached terminal reads its screen more often than the dashboard.
    let mut terminal_tick = tokio::time::interval(Duration::from_millis(25));
    terminal_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut connecting: Vec<Connecting> = Vec::new();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    loop {
        if let Ok(size) = terminal.size() {
            ui.size = (size.width, size.height);
        }
        ui.normalize_focus();
        terminal.draw(|frame| render_ui(frame, ui))?;
        if std::mem::take(&mut ui.workbench.bell) {
            use std::io::Write as _;
            let _ = terminal.backend_mut().write_all(b"\x07");
            let _ = terminal.backend_mut().flush();
        }
        if ui.quit {
            return Ok(());
        }
        for opening in std::mem::take(&mut ui.openings) {
            connecting.push(connect(socket, opening, ui.options.clone()));
        }
        tokio::select! {
            _ = tick.tick() => ui.tick(),
            _ = terminal_tick.tick(), if ui.workbench.attached() => ui.terminal_tick(),
            Some(answer) = terminal_answers.recv() => {
                if let Some(note) = ui.workbench.terminal_answer(answer) {
                    ui.workbench.notify(note, false);
                }
                ui.terminal_tick();
            }
            Some(answer) = takeover_answers.recv() => {
                if let Some(note) = ui.workbench.terminal_answer(answer) {
                    ui.workbench.notify(note, false);
                }
                ui.terminal_tick();
            }
            _ = terminate.recv() => ui.quit = true,
            _ = hangup.recv() => ui.quit = true,
            (opening, result) = next_connection(&mut connecting) => match result {
                Ok((client, session)) => {
                    // A tab closed while it reconnected: its new session ends.
                    if let Some(mut client) = ui.opened(&opening, client, session) {
                        client.shutdown().await;
                    }
                }
                Err(error) => ui.open_failed(&opening, &error),
            },
            Some(answer) = answers.recv() => {
                if ui.focus == ui::Focus::Sidebar {
                    let items = ui.sidebar_items();
                    ui.workbench.note_highlight(&items);
                }
                if let Some(note) = ui.workbench.answer(answer) {
                    ui.workbench.notify(note, false);
                }
            }
            terminal_event = terminal_events.next() => {
                match terminal_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        ui.key(key).await?;
                    }
                    Some(Ok(Event::Paste(text))) => ui.paste(&text),
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(()),
                }
            }
            (id, event) = next_chat_event(ui) => {
                // A tab's failed connection is that tab's: it reconnects.
                ui.chat_event(id, event);
                if let Some(tab) = ui.tab_mut(id)
                    && let TabKind::Chat(chat) = &mut tab.kind
                    && !chat.app.connected
                {
                    chat.client.shutdown().await;
                }
            }
        }
    }
}

/// Connect a chat tab: a reconnecting one retries until the daemon is
/// back, a new one tries once.
fn connect(socket: &Path, opening: Opening, options: LaunchOptions) -> Connecting {
    let socket = socket.to_owned();
    let future: Opened = if opening.tab.is_some() {
        let cwd = opening.cwd.clone();
        let restore = opening.restore.clone();
        Box::pin(async move { Ok(reconnect_restoring(&socket, &cwd, &options, restore).await) })
    } else {
        let cwd = opening.cwd.clone();
        let restore = opening.restore.clone();
        Box::pin(async move { Client::connect_restoring(&socket, &cwd, &options, restore).await })
    };
    (opening, future)
}

/// The next chat tab connection to finish.
async fn next_connection(
    connecting: &mut Vec<Connecting>,
) -> (Opening, Result<(Client, SessionInfo)>) {
    if connecting.is_empty() {
        return std::future::pending().await;
    }
    let (result, index, _) =
        select_all(connecting.iter_mut().map(|(_, future)| future.as_mut())).await;
    let (opening, _) = connecting.remove(index);
    (opening, result)
}

/// The next event of any connected chat tab, with the tab's ID. Reading is
/// cancel-safe: each client keeps a partly read frame.
async fn next_chat_event(ui: &mut Ui) -> (u64, Result<Option<ServerEvent>>) {
    let reads: Vec<_> = ui
        .tabs
        .iter_mut()
        .filter_map(|tab| match &mut tab.kind {
            TabKind::Chat(chat) if chat.app.connected => {
                let id = tab.id;
                Some(Box::pin(
                    async move { (id, chat.client.read_event().await) },
                ))
            }
            _ => None,
        })
        .collect();
    if reads.is_empty() {
        return std::future::pending().await;
    }
    select_all(reads).await.0
}

#[cfg(test)]
mod tests;
