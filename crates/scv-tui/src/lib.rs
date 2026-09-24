//! SCV's clients for people: the terminal UI ([`run_tui`]), which attaches to
//! the daemon socket, and the headless one-prompt client behind `scv exec`
//! ([`run_exec`]), which starts its own `scv server --stdio`. Both render
//! server events and keep no authoritative conversation state.
//!
//! The terminal UI is split by concern: `client` talks to the
//! server, `app` holds what is shown and applies server events, `input`
//! handles keys, `render` draws, and `transcript` bounds what is kept.

#![forbid(unsafe_code)]

mod app;
mod client;
mod exec;
mod input;
mod render;
mod terminal;
mod transcript;

use std::{path::Path, time::Duration};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{
    app::App,
    client::{Client, reconnect_client},
    input::handle_key,
    render::render,
    terminal::TerminalGuard,
};

pub use client::LaunchOptions;
pub use exec::run_exec;

/// Run the terminal UI against the daemon, reconnecting if it restarts.
pub async fn run_tui(cwd: &Path, options: LaunchOptions) -> Result<()> {
    let (mut client, session) = Client::connect(cwd, &options).await?;
    let mut terminal = TerminalGuard::enter()?;
    let mut app = App::new(session);
    let result = run_event_loop(&mut terminal.terminal, &mut client, &mut app, cwd, &options).await;
    client.shutdown().await;
    result
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    client: &mut Client,
    app: &mut App,
    cwd: &Path,
    options: &LaunchOptions,
) -> Result<()> {
    let mut terminal_events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let socket_path = scv_client::default_socket_path()?;
    let mut reconnect = Box::pin(reconnect_client(&socket_path, cwd, options));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    loop {
        terminal.draw(|frame| render(frame, app))?;
        if app.quit {
            return Ok(());
        }
        tokio::select! {
            _ = tick.tick() => {}
            (new_client, session) = &mut reconnect, if !app.connected => {
                *client = new_client;
                app.reconnect(session);
                reconnect = Box::pin(reconnect_client(&socket_path, cwd, options));
            }
            _ = terminate.recv() => app.quit = true,
            _ = hangup.recv() => app.quit = true,
            terminal_event = terminal_events.next() => {
                match terminal_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        handle_key(client, app, key).await?;
                        if !app.connected {
                            client.shutdown().await;
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => {},
                    Some(Ok(_)) => {},
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(()),
                }
            }
            server_event = client.read_event(), if app.connected => {
                app.handle_server_result(server_event)?;
                if !app.connected {
                    client.shutdown().await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
