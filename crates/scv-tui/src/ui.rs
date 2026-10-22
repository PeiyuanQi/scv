//! The dashboard around the chat: tabs (each a chat session in its own
//! workspace, or an agent view), one or two panes, the sidebar, overlays,
//! and the keys and slash commands that move between them. A chat tab's own
//! keys still go to [`crate::input`]; this decides where a key goes first.

use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use scv_protocol::{AgentState, DaemonCommand, SendMode, ServerEvent};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    app::App,
    client::{Client, LaunchOptions},
    input::{byte_index, handle_key},
    keys::{Action, KeyMap},
    poller::{Request, Tag},
    transcript::{TranscriptItem, bounded_text},
    workbench::{Item, Target, Workbench, mark, short_path},
};

/// How long a destructive sidebar key waits to be pressed again.
const CONFIRM_WINDOW: Duration = Duration::from_secs(3);

/// A chat tab: its own daemon session in its own workspace.
pub(crate) struct ChatTab {
    pub(crate) client: Client,
    pub(crate) app: App,
    /// The workspace the TUI asked for, for reconnecting.
    pub(crate) cwd: PathBuf,
}

pub(crate) enum TabKind {
    Chat(Box<ChatTab>),
    Agent(Target),
}

pub(crate) struct Tab {
    pub(crate) id: u64,
    pub(crate) kind: TabKind,
}

/// Where keys go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    Sidebar,
    /// The pane at this index.
    Pane(usize),
}

/// A full-screen overlay over the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Overlay {
    Help,
    Notifications,
    Worktrees,
}

/// A chat tab to open: its workspace, and a stopped session whose queue it
/// restores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Opening {
    pub(crate) cwd: PathBuf,
    pub(crate) restore: Option<String>,
    /// The tab it replaces, when it reconnects one.
    pub(crate) tab: Option<u64>,
    /// Where keys went when it was asked for (focus, and the focused
    /// pane's tab): a new tab takes the pane only if they still go there.
    pub(crate) from: (Focus, Option<u64>),
}

pub(crate) struct Ui {
    pub(crate) tabs: Vec<Tab>,
    next_tab: u64,
    /// The tab each pane shows: one pane, or two side by side.
    pub(crate) panes: Vec<u64>,
    pub(crate) focus: Focus,
    /// The pane keys were on before the sidebar took them. A chat approval
    /// that was covering the dashboard stays that chat's when a dashboard
    /// key moves focus here, instead of becoming the first pane's.
    modal_pane: Option<usize>,
    pub(crate) sidebar: bool,
    pub(crate) workbench: Workbench,
    pub(crate) keys: KeyMap,
    pub(crate) overlay: Option<Overlay>,
    pub(crate) overlay_scroll: u16,
    pub(crate) quit: bool,
    /// Requests for the dashboard's own connection.
    pub(crate) requests: Option<UnboundedSender<Request>>,
    /// Chat tabs to connect: new ones and reconnecting ones.
    pub(crate) openings: Vec<Opening>,
    /// A destructive sidebar key waiting to be pressed again: the key, the
    /// entry it was pressed on ([`Item::key`]), and when.
    confirm: Option<(char, String, Instant)>,
    /// Chat tabs reconnecting now; each reconnects once at a time.
    reconnecting: HashSet<u64>,
    /// Quitting (`0`) or closing chat tab `id` while chats have work
    /// running, waiting to be asked again.
    confirm_leave: Option<(u64, Instant)>,
    /// The terminal's size at the last draw, which decides what is shown
    /// and so where keys can go.
    pub(crate) size: (u16, u16),
    pub(crate) options: LaunchOptions,
}

impl Ui {
    /// A dashboard showing the chat tab `tab`.
    pub(crate) fn new(
        tab: ChatTab,
        keys: &BTreeMap<String, String>,
        options: LaunchOptions,
    ) -> Self {
        let (keymap, warnings) = KeyMap::with(keys);
        let mut ui = Self {
            tabs: Vec::new(),
            next_tab: 0,
            panes: Vec::new(),
            focus: Focus::Pane(0),
            modal_pane: Some(0),
            sidebar: true,
            workbench: Workbench::default(),
            keys: keymap,
            overlay: None,
            overlay_scroll: 0,
            quit: false,
            requests: None,
            openings: Vec::new(),
            confirm: None,
            reconnecting: HashSet::new(),
            confirm_leave: None,
            size: (u16::MAX, u16::MAX),
            options,
        };
        let id = ui.add_tab(TabKind::Chat(Box::new(tab)));
        ui.panes.push(id);
        for warning in warnings {
            ui.workbench.notify(warning, false);
        }
        ui
    }

    fn add_tab(&mut self, kind: TabKind) -> u64 {
        self.next_tab += 1;
        let id = self.next_tab;
        self.tabs.push(Tab { id, kind });
        id
    }

    /// The tab with ID `id`.
    pub(crate) fn tab(&self, id: u64) -> Option<&Tab> {
        self.tabs.iter().find(|tab| tab.id == id)
    }

    pub(crate) fn tab_mut(&mut self, id: u64) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    /// Remember the focused pane, so a later sidebar focus still knows which
    /// chat's approval was on screen.
    fn remember_pane(&mut self) {
        if let Focus::Pane(index) = self.focus {
            self.modal_pane = Some(index);
        }
    }

    /// The chat whose approval prompt is drawn: the focused pane, or the
    /// pane focus left when a dashboard key opened the sidebar.
    pub(crate) fn covering_tab(&self) -> Option<u64> {
        let index = match self.focus {
            Focus::Pane(index) => index,
            Focus::Sidebar => self.modal_pane?,
        };
        let index = index.min(self.panes.len().saturating_sub(1));
        let id = *self.panes.get(index)?;
        self.tab(id).and_then(|tab| match &tab.kind {
            TabKind::Chat(chat) if chat.app.pending_approval.is_some() => Some(id),
            _ => None,
        })
    }

    /// The pane keys go to, or the first when the sidebar has focus.
    pub(crate) fn active_pane(&self) -> usize {
        match self.focus {
            Focus::Pane(index) => index.min(self.panes.len().saturating_sub(1)),
            Focus::Sidebar => 0,
        }
    }

    /// The tab the focused pane shows.
    pub(crate) fn active_tab(&self) -> Option<u64> {
        self.panes.get(self.active_pane()).copied()
    }

    /// Show tab `id` in the focused pane and focus it.
    pub(crate) fn show(&mut self, id: u64) {
        let pane = self.active_pane();
        if let Some(slot) = self.panes.get_mut(pane) {
            *slot = id;
        }
        self.focus = Focus::Pane(pane);
    }

    /// A chat tab connected: a new one, or one reconnecting. Returns the
    /// client of a tab closed while it reconnected, to be shut down.
    pub(crate) fn opened(
        &mut self,
        opening: &Opening,
        client: Client,
        session: crate::client::SessionInfo,
    ) -> Option<Client> {
        if let Some(id) = opening.tab {
            self.reconnecting.remove(&id);
            let Some(Tab {
                kind: TabKind::Chat(chat),
                ..
            }) = self.tab_mut(id)
            else {
                return Some(client);
            };
            chat.client = client;
            chat.app.reconnect(session);
            note_restored(&mut chat.app);
            return None;
        }
        let mut app = App::new(session);
        note_restored(&mut app);
        let id = self.add_tab(TabKind::Chat(Box::new(ChatTab {
            client,
            app,
            cwd: opening.cwd.clone(),
        })));
        // A session can take a while to start; keys the owner sent
        // elsewhere meanwhile stay there.
        if opening.from == (self.focus, self.active_tab()) {
            self.show(id);
        } else {
            self.workbench.notify(
                format!(
                    "Tab {} is open: a session in {}.",
                    self.tabs.len(),
                    opening.cwd.display()
                ),
                false,
            );
        }
        None
    }

    /// A new chat tab to open in `cwd`, restoring the queue of session
    /// `restore`, asked for now.
    fn opening(&self, cwd: PathBuf, restore: Option<String>) -> Opening {
        Opening {
            cwd,
            restore,
            tab: None,
            from: (self.focus, self.active_tab()),
        }
    }

    /// A chat tab could not be opened.
    pub(crate) fn open_failed(&mut self, opening: &Opening, error: &anyhow::Error) {
        if let Some(id) = opening.tab {
            self.reconnecting.remove(&id);
        }
        self.workbench.notify(
            format!(
                "Could not open a session in {}: {error:#}",
                opening.cwd.display()
            ),
            true,
        );
    }

    /// Queue a workbench request on the dashboard's connection.
    pub(crate) fn request(&self, tag: Tag, command: DaemonCommand) {
        if let Some(requests) = &self.requests {
            let _ = requests.send(Request { tag, command });
        }
    }

    /// The requests the workbench needs now.
    pub(crate) fn tick(&mut self) {
        // Views of tabs that were closed stop reading.
        let open: Vec<Target> = self
            .tabs
            .iter()
            .filter_map(|tab| match &tab.kind {
                TabKind::Agent(target) => Some(target.clone()),
                TabKind::Chat(_) => None,
            })
            .collect();
        self.workbench
            .views
            .retain(|target, _| open.contains(target));
        for request in self.workbench.due(Instant::now()) {
            self.request(request.tag, request.command);
        }
    }

    /// A chat tab's server sent `event`, or its connection ended or
    /// failed. A failure is that tab's alone: it disconnects and reconnects
    /// as when its connection is lost, and the other tabs keep running.
    pub(crate) fn chat_event(&mut self, id: u64, result: Result<Option<ServerEvent>>) {
        let focused = self.active_tab() == Some(id);
        let index = self.tabs.iter().position(|tab| tab.id == id);
        let Some(Tab {
            kind: TabKind::Chat(chat),
            ..
        }) = index.and_then(|index| self.tabs.get_mut(index))
        else {
            return;
        };
        let approval = match &result {
            Ok(Some(ServerEvent::ApprovalRequested { summary, .. })) if !focused => {
                Some(bounded_text(summary, 120))
            }
            _ => None,
        };
        let failed = chat.app.handle_server_result(result).err();
        if let Some(error) = &failed {
            connection_failed(&mut chat.app, error);
        }
        let reconnect =
            (!chat.app.connected).then(|| (chat.cwd.clone(), chat.app.session_id.clone()));
        let number = index.map_or(0, |index| index + 1);
        if let Some(summary) = approval {
            self.workbench
                .notify(format!("Tab {number} asks for approval: {summary}"), true);
        }
        if let Some(error) = failed {
            self.workbench.notify(
                format!("Tab {number}'s connection failed ({error:#}); it reconnects."),
                true,
            );
        }
        if let Some((cwd, session)) = reconnect {
            self.reconnect(id, cwd, session);
        }
    }

    /// Reconnect chat tab `id`, restoring its session's queue, unless it is
    /// reconnecting already: a second reconnection would end the first's
    /// session, and the queue it restored with it.
    fn reconnect(&mut self, id: u64, cwd: PathBuf, session: String) {
        if self.reconnecting.insert(id) {
            self.openings.push(Opening {
                tab: Some(id),
                ..self.opening(cwd, Some(session))
            });
        }
    }

    /// One key press.
    pub(crate) async fn key(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(overlay) = self.overlay {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                KeyCode::Up => self.overlay_scroll = self.overlay_scroll.saturating_sub(1),
                KeyCode::Down => self.overlay_scroll = self.overlay_scroll.saturating_add(1),
                KeyCode::PageUp => self.overlay_scroll = self.overlay_scroll.saturating_sub(10),
                KeyCode::PageDown => self.overlay_scroll = self.overlay_scroll.saturating_add(10),
                KeyCode::Char('r') if overlay == Overlay::Worktrees => {
                    self.request(Tag::Worktrees, DaemonCommand::Worktrees);
                }
                _ => {}
            }
            return Ok(());
        }
        self.normalize_focus();
        self.remember_pane();
        if let Some(action) = self.keys.action(key) {
            self.act(action);
            self.normalize_focus();
            self.remember_pane();
            return Ok(());
        }
        // Asked to quit while work runs: Ctrl+C confirms, as the notice
        // says. It must not fall through into the chat, where Ctrl+C
        // cancels the running turn.
        if self.quit_confirm_pending()
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('c')
        {
            self.leave(0);
            return Ok(());
        }
        // A chat's approval prompt covers the dashboard: every other key is
        // the prompt's, wherever focus is, and what it does not answer with
        // does nothing, so nothing behind it moves or opens. Dashboard keys
        // above may move focus; the prompt stays the chat that was showing it.
        if let Some(id) = self.covering_tab() {
            return self.pane_key_for(Some(id), key).await;
        }
        match self.focus {
            Focus::Sidebar => {
                self.sidebar_key(key);
                Ok(())
            }
            Focus::Pane(_) => self.pane_key(key).await,
        }
    }

    /// Quit was asked and has not expired: Ctrl+C confirms it.
    fn quit_confirm_pending(&self) -> bool {
        self.confirm_leave.is_some_and(|(target, at)| {
            target == 0 && Instant::now().duration_since(at) < CONFIRM_WINDOW
        })
    }

    /// Keep focus on what the terminal shows: a sidebar or second pane too
    /// narrow to draw passes its keys to the first pane, and says so when
    /// the owner asked for it. A chat drawn alone is the focused pane's and
    /// keeps them. Runs before every draw too, so a resize never sends keys
    /// to a chat that is not drawn.
    pub(crate) fn normalize_focus(&mut self) {
        let shown = crate::render::shown(self, self.size.0, self.size.1);
        let hidden = match self.focus {
            Focus::Sidebar => !shown.sidebar,
            Focus::Pane(index) => !shown.alone && index >= shown.panes,
        };
        if hidden {
            self.focus = Focus::Pane(0);
        }
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::FocusSidebar => {
                self.focus = if self.focus == Focus::Sidebar {
                    Focus::Pane(0)
                } else {
                    self.sidebar = true;
                    Focus::Sidebar
                };
                if self.focus == Focus::Sidebar
                    && !crate::render::shown(self, self.size.0, self.size.1).sidebar
                {
                    self.workbench.notify(
                        "The sidebar needs a terminal at least 70 columns wide (and 12 rows)."
                            .into(),
                        false,
                    );
                }
            }
            Action::ToggleSidebar => {
                self.sidebar = !self.sidebar;
                if !self.sidebar && self.focus == Focus::Sidebar {
                    self.focus = Focus::Pane(0);
                }
            }
            Action::NextTab | Action::PrevTab => {
                let Some(current) = self.active_tab() else {
                    return;
                };
                let index = self
                    .tabs
                    .iter()
                    .position(|tab| tab.id == current)
                    .unwrap_or(0);
                let count = self.tabs.len().max(1);
                let next = if action == Action::NextTab {
                    (index + 1) % count
                } else {
                    (index + count - 1) % count
                };
                if let Some(id) = self.tabs.get(next).map(|tab| tab.id) {
                    self.show(id);
                }
            }
            Action::Tab(number) => {
                if let Some(id) = self.tabs.get(usize::from(number) - 1).map(|tab| tab.id) {
                    self.show(id);
                }
            }
            Action::NewTab => {
                let Some(app) = self.focused_chat_app() else {
                    return;
                };
                if app.input.is_empty() {
                    app.input = "/open ".into();
                    app.cursor = app.input.chars().count();
                } else {
                    self.workbench.notify(
                        "The composer holds a draft; send or clear it, then open a tab with \
                         /open <directory>."
                            .into(),
                        false,
                    );
                }
            }
            Action::CloseTab => self.close_tab(),
            Action::Split => {
                if self.panes.len() > 1 {
                    self.panes.truncate(1);
                    self.focus = Focus::Pane(0);
                } else {
                    // The second pane starts on the next tab.
                    let current = self.active_tab();
                    let other = self
                        .tabs
                        .iter()
                        .map(|tab| tab.id)
                        .find(|id| Some(*id) != current)
                        .or(current);
                    if let Some(other) = other {
                        self.panes.push(other);
                        self.focus = Focus::Pane(1);
                        if crate::render::shown(self, self.size.0, self.size.1).panes < 2 {
                            // Keys stay with the pane on screen.
                            self.focus = Focus::Pane(0);
                            self.workbench.notify(
                                "Two panes need 60 columns beside the sidebar; the second shows \
                                 when the terminal is wider."
                                    .into(),
                                false,
                            );
                        }
                    }
                }
            }
            Action::FocusOtherPane => {
                if self.panes.len() > 1 {
                    self.focus = Focus::Pane(1 - self.active_pane().min(1));
                }
            }
            Action::Notifications => self.open_overlay(Overlay::Notifications),
            Action::Help => self.open_overlay(Overlay::Help),
        }
    }

    fn open_overlay(&mut self, overlay: Overlay) {
        self.overlay = Some(overlay);
        self.overlay_scroll = 0;
        if overlay == Overlay::Worktrees {
            self.request(Tag::Worktrees, DaemonCommand::Worktrees);
        }
    }

    /// Close the focused pane's tab; the last chat tab stays.
    fn close_tab(&mut self) {
        let Some(current) = self.active_tab() else {
            return;
        };
        let chats = self
            .tabs
            .iter()
            .filter(|tab| matches!(tab.kind, TabKind::Chat(_)))
            .count();
        let is_chat = matches!(
            self.tab(current).map(|tab| &tab.kind),
            Some(TabKind::Chat(_))
        );
        if is_chat && chats <= 1 {
            self.workbench.notify(
                "The last chat tab stays open; /quit leaves SCV.".into(),
                false,
            );
            return;
        }
        if is_chat && !self.leave(current) {
            return;
        }
        self.tabs.retain(|tab| tab.id != current);
        let fallback = self.tabs.first().map(|tab| tab.id);
        for pane in &mut self.panes {
            if *pane == current
                && let Some(fallback) = fallback
            {
                *pane = fallback;
            }
        }
        self.panes.dedup();
        if self.panes.len() < 2 {
            self.focus = Focus::Pane(0);
        }
    }

    fn focused_chat_app(&mut self) -> Option<&mut App> {
        let id = self.active_tab()?;
        match &mut self.tab_mut(id)?.kind {
            TabKind::Chat(chat) => Some(&mut chat.app),
            TabKind::Agent(_) => None,
        }
    }

    async fn pane_key(&mut self, key: KeyEvent) -> Result<()> {
        self.pane_key_for(self.active_tab(), key).await
    }

    async fn pane_key_for(&mut self, id: Option<u64>, key: KeyEvent) -> Result<()> {
        let Some(id) = id else {
            return Ok(());
        };
        let target = match &self.tab(id).map(|tab| &tab.kind) {
            Some(TabKind::Agent(target)) => Some(target.clone()),
            _ => None,
        };
        if let Some(target) = target {
            self.agent_key(&target, key);
            return Ok(());
        }
        // A dashboard command typed into this chat's composer. The chat is
        // `id`, which is the covering approval's chat when that prompt has
        // focus and not necessarily the pane the sidebar would call first.
        if key.code == KeyCode::Enter
            && key.modifiers == KeyModifiers::NONE
            && let Some(line) = self.tab(id).and_then(|tab| match &tab.kind {
                TabKind::Chat(chat)
                    if chat.app.pending_approval.is_none() && chat.app.queue_editing.is_none() =>
                {
                    Some(chat.app.input.trim().to_owned())
                }
                _ => None,
            })
            && self.command(id, &line)
        {
            if let Some(Tab {
                kind: TabKind::Chat(chat),
                ..
            }) = self.tab_mut(id)
            {
                crate::input::clear_input(&mut chat.app);
            }
            return Ok(());
        }
        let Some(Tab {
            kind: TabKind::Chat(chat),
            ..
        }) = self.tab_mut(id)
        else {
            return Ok(());
        };
        let failed = handle_key(&mut chat.client, &mut chat.app, key).await.err();
        if let Some(error) = &failed {
            connection_failed(&mut chat.app, error);
        }
        let quit = std::mem::take(&mut chat.app.quit);
        let reconnect =
            (!chat.app.connected).then(|| (chat.cwd.clone(), chat.app.session_id.clone()));
        if !chat.app.connected {
            chat.client.shutdown().await;
        }
        if quit && !self.leave(0) {
            // The notice says to ask again. The `/quit` that asked stays
            // out of the composer, so the confirming Ctrl+C is not eaten
            // as clearing that text.
            if let Some(Tab {
                kind: TabKind::Chat(chat),
                ..
            }) = self.tab_mut(id)
            {
                crate::input::clear_input(&mut chat.app);
            }
        }
        if let Some(error) = failed {
            self.workbench.notify(
                format!("This tab's connection failed ({error:#}); it reconnects."),
                true,
            );
        }
        if let Some((cwd, session)) = reconnect {
            self.reconnect(id, cwd, session);
        }
        Ok(())
    }

    /// What quitting (`only` = `None`) or closing chat tab `only` would
    /// stop: chats running a turn, their sessions' unfinished background
    /// jobs, and the prompts queued in them (a restored, paused queue too).
    fn running_work(&self, only: Option<u64>) -> Vec<String> {
        let mut work = Vec::new();
        for (index, tab) in self.tabs.iter().enumerate() {
            let TabKind::Chat(chat) = &tab.kind else {
                continue;
            };
            if only.is_some_and(|only| only != tab.id) {
                continue;
            }
            let jobs = self
                .workbench
                .snapshot
                .iter()
                .flat_map(|snapshot| snapshot.jobs.iter())
                .filter(|job| job.session == chat.app.session_id && !job.state.is_final())
                .count();
            let mut what = Vec::new();
            if chat.app.turn.is_some() {
                what.push("runs a turn".to_owned());
            }
            if jobs > 0 {
                what.push(format!("has {jobs} background job(s)"));
            }
            if !chat.app.queue.is_empty() {
                what.push(format!("has {} queued prompt(s)", chat.app.queue.len()));
            }
            if !what.is_empty() {
                work.push(format!("tab {} {}", index + 1, what.join(" and ")));
            }
        }
        work
    }

    /// Quit (`target` 0) or close chat tab `target`: at once when that
    /// stops nothing, else when asked again within [`CONFIRM_WINDOW`].
    fn leave(&mut self, target: u64) -> bool {
        let work = self.running_work((target != 0).then_some(target));
        let now = Instant::now();
        let again = self
            .confirm_leave
            .is_some_and(|(asked, at)| asked == target && now.duration_since(at) < CONFIRM_WINDOW);
        if work.is_empty() || again {
            self.confirm_leave = None;
            if target == 0 {
                self.quit = true;
            }
            return true;
        }
        self.confirm_leave = Some((target, now));
        let what = if target == 0 {
            "Quitting stops it; ask again (Ctrl+C or /quit) to quit"
        } else {
            "Closing the tab stops it; ask again to close it"
        };
        self.workbench
            .notify(format!("{}. {what}.", work.join("; ")), false);
        false
    }

    /// Run a dashboard command typed into chat tab `id`'s composer; false
    /// when `line` is not one, so the chat takes it.
    fn command(&mut self, id: u64, line: &str) -> bool {
        let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();
        let cwd = match self.tab(id).map(|tab| &tab.kind) {
            Some(TabKind::Chat(chat)) => PathBuf::from(&chat.app.cwd),
            _ => return false,
        };
        match name {
            "/open" => {
                if rest.is_empty() {
                    self.workbench
                        .notify("Usage: /open <workspace directory>".into(), false);
                } else {
                    let opening = self.opening(expand(rest, &cwd), None);
                    self.openings.push(opening);
                }
            }
            "/close" => self.close_tab(),
            "/jobs" => {
                self.sidebar = true;
                self.focus = Focus::Sidebar;
            }
            "/help" => self.open_overlay(Overlay::Help),
            "/notifications" => self.open_overlay(Overlay::Notifications),
            "/worktrees" => self.open_overlay(Overlay::Worktrees),
            "/job" => match parse_job(rest) {
                Ok(job) => self.request(
                    Tag::Action,
                    DaemonCommand::WorkbenchStart {
                        workspace: cwd.display().to_string(),
                        agent: job.agent,
                        prompt: job.prompt,
                        cwd: job.cwd,
                        model: job.model,
                        effort: job.effort,
                        worktree: job.worktree,
                    },
                ),
                Err(usage) => self.workbench.notify(usage, false),
            },
            "/restore" => self.restore(rest),
            _ => return false,
        }
        true
    }

    /// Open a chat tab that restores the queue of the stopped session whose
    /// ID starts with `prefix`.
    fn restore(&mut self, prefix: &str) {
        let found: Vec<_> = self
            .workbench
            .snapshot
            .iter()
            .flat_map(|snapshot| snapshot.restorable.iter())
            .filter(|session| !prefix.is_empty() && session.id.starts_with(prefix))
            .cloned()
            .collect();
        match found.as_slice() {
            [session] => {
                let opening =
                    self.opening(PathBuf::from(&session.workspace), Some(session.id.clone()));
                self.openings.push(opening);
            }
            [] => self.workbench.notify(
                "No restorable queue has that ID; the sidebar lists them.".into(),
                false,
            ),
            _ => self.workbench.notify(
                "More than one queue matches; type more of its ID.".into(),
                false,
            ),
        }
    }

    /// Open (or show) the agent view following `target`.
    pub(crate) fn open_view(&mut self, target: Target) {
        self.workbench.open(target.clone());
        let existing = self.tabs.iter().find_map(|tab| match &tab.kind {
            TabKind::Agent(open) if *open == target => Some(tab.id),
            _ => None,
        });
        let id = existing.unwrap_or_else(|| self.add_tab(TabKind::Agent(target)));
        // Opening from the sidebar keeps the sidebar out of the panes.
        if self.focus == Focus::Sidebar {
            self.reveal(id);
        } else {
            self.show(id);
        }
    }

    /// Show tab `id` from the sidebar: focus the second pane when it shows
    /// the tab already and is drawn, else show it in the first.
    fn reveal(&mut self, id: u64) {
        if self.panes.get(1) == Some(&id)
            && crate::render::shown(self, self.size.0, self.size.1).panes > 1
        {
            self.focus = Focus::Pane(1);
        } else {
            self.focus = Focus::Pane(0);
            self.show(id);
        }
    }

    fn agent_key(&mut self, target: &Target, key: KeyEvent) {
        let approval = self.workbench.approval(target).cloned();
        let job = self.workbench.job(target).cloned();
        let conversation = self.workbench.conversation(target);
        let Some(view) = self.workbench.views.get_mut(target) else {
            return;
        };
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            // Plain y and n are text: a prompt that starts with "yes" or
            // "no" never answers an approval its writer did not read.
            KeyCode::Char('y' | 'n') if control && approval.is_some() => {
                let approved = key.code == KeyCode::Char('y');
                if let Some(approval) = approval {
                    self.request(
                        Tag::Action,
                        DaemonCommand::WorkbenchApprove {
                            id: approval.id,
                            approved,
                        },
                    );
                }
            }
            KeyCode::Char('x') if control => {
                let Some(job) = job.filter(|job| !job.state.is_final()) else {
                    return;
                };
                let now = Instant::now();
                if view
                    .confirm_cancel
                    .is_some_and(|at| now.duration_since(at) < CONFIRM_WINDOW)
                {
                    view.confirm_cancel = None;
                    self.request(
                        Tag::Action,
                        DaemonCommand::WorkbenchCancel {
                            session: job.session,
                            job: job.job,
                        },
                    );
                } else {
                    view.confirm_cancel = Some(now);
                    self.workbench
                        .notify(format!("Press Ctrl+X again to stop {}.", job.job), false);
                }
            }
            KeyCode::Char('r') if control => {
                if let Some(job) = job.filter(|job| job.resumable) {
                    self.request(
                        Tag::Action,
                        DaemonCommand::WorkbenchResume {
                            session: job.session,
                            job: job.job,
                        },
                    );
                }
            }
            KeyCode::Char('j') if control => insert(view, '\n'),
            KeyCode::Enter => {
                let prompt = view.input.trim().to_owned();
                if prompt.is_empty() {
                    return;
                }
                // One prompt in flight at a time, so a refusal brings back
                // the prompt it refused.
                if view.sent.is_some() {
                    self.workbench.notify(
                        "Still sending the last prompt; this one stays in the composer.".into(),
                        false,
                    );
                    return;
                }
                let Some((session, conversation)) = conversation else {
                    self.workbench.notify(
                        "This view has no conversation to send to yet; wait for its first turn \
                         to start."
                            .into(),
                        false,
                    );
                    return;
                };
                view.sent = Some(std::mem::take(&mut view.input));
                view.cursor = 0;
                view.follow = true;
                self.request(
                    Tag::Send(target.clone()),
                    DaemonCommand::WorkbenchSend {
                        session,
                        conversation,
                        prompt,
                        mode: SendMode::Auto,
                    },
                );
            }
            KeyCode::Char(character) if !control => insert(view, character),
            KeyCode::Backspace => {
                if view.cursor > 0 {
                    let end = byte_index(&view.input, view.cursor);
                    let start = byte_index(&view.input, view.cursor - 1);
                    view.input.replace_range(start..end, "");
                    view.cursor -= 1;
                }
            }
            KeyCode::Left => view.cursor = view.cursor.saturating_sub(1),
            KeyCode::Right => view.cursor = (view.cursor + 1).min(view.input.chars().count()),
            KeyCode::Home => view.cursor = 0,
            KeyCode::End => {
                view.cursor = view.input.chars().count();
                view.follow = true;
                view.scroll = 0;
            }
            KeyCode::PageUp => {
                view.scroll = view.scroll.saturating_add(10);
                view.follow = false;
            }
            KeyCode::PageDown => {
                view.scroll = view.scroll.saturating_sub(10);
                view.follow = view.scroll == 0;
            }
            KeyCode::Esc => {
                view.input.clear();
                view.cursor = 0;
            }
            _ => {}
        }
    }

    /// The sidebar's entries for this dashboard.
    pub(crate) fn sidebar_items(&self) -> Vec<Item> {
        let tabs: Vec<(u64, String)> = self
            .tabs
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| match &tab.kind {
                TabKind::Chat(chat) => Some((tab.id, tab_line(index + 1, &chat.app))),
                TabKind::Agent(_) => None,
            })
            .collect();
        self.workbench.items(&tabs)
    }

    fn sidebar_key(&mut self, key: KeyEvent) {
        let items = self.sidebar_items();
        let selectable: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.selectable())
            .map(|(index, _)| index)
            .collect();
        if selectable.is_empty() {
            if key.code == KeyCode::Esc {
                self.focus = Focus::Pane(0);
            }
            return;
        }
        let current = self.workbench.selection(&items);
        let position = selectable
            .iter()
            .position(|index| Some(*index) == current)
            .unwrap_or(0);
        // The selected entry went away and the selection fell back to
        // another: a key that acts on it only moves the selection there, so
        // a press meant for the old entry never acts on the new one.
        let acts = match key.code {
            KeyCode::Enter => true,
            KeyCode::Char(letter) => {
                letter != 'w' && !key.modifiers.contains(KeyModifiers::CONTROL)
            }
            _ => false,
        };
        if acts && self.workbench.selection_gone(&items) {
            self.workbench.select(&items, selectable[position]);
            self.workbench.notify(
                "The selected entry is gone and the selection moved; press again to act on \
                 the one selected now."
                    .into(),
                false,
            );
            return;
        }
        let mut moved = |offset: isize| {
            let next = position
                .saturating_add_signed(offset)
                .min(selectable.len() - 1);
            self.workbench.select(&items, selectable[next]);
        };
        match key.code {
            KeyCode::Up => moved(-1),
            KeyCode::Down => moved(1),
            KeyCode::PageUp => moved(-10),
            KeyCode::PageDown => moved(10),
            KeyCode::Home => moved(isize::MIN / 2),
            KeyCode::End => moved(isize::MAX / 2),
            KeyCode::Esc => self.focus = Focus::Pane(0),
            KeyCode::Enter => {
                self.workbench.select(&items, selectable[position]);
                if let Some(item) = items.get(selectable[position]).cloned() {
                    self.open_item(item);
                }
            }
            KeyCode::Char(letter) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.workbench.select(&items, selectable[position]);
                if let Some(item) = items.get(selectable[position]).cloned() {
                    self.item_key(&item, letter);
                }
            }
            _ => {}
        }
    }

    fn open_item(&mut self, item: Item) {
        match item {
            Item::Header(_) => {}
            Item::Tab(id, _) => self.reveal(id),
            Item::Approval(approval) => self.open_view(Target::Job {
                session: approval.session,
                job: approval.job,
            }),
            Item::Job(job) => self.open_view(Target::Job {
                session: job.session,
                job: job.job,
            }),
            Item::Run(run) => self.open_view(match run.conversation {
                Some(conversation) => Target::Conversation {
                    session: run.session,
                    conversation,
                },
                None => Target::Run { handle: run.handle },
            }),
            Item::Restorable(session) => {
                let opening = self.opening(PathBuf::from(session.workspace), Some(session.id));
                self.openings.push(opening);
            }
        }
    }

    fn item_key(&mut self, item: &Item, letter: char) {
        // A second press counts only on the same entry, wherever its row is.
        let entry = item.key().unwrap_or_default();
        let confirmed = |ui: &mut Self, what: &str| {
            let now = Instant::now();
            if ui.confirm.as_ref().is_some_and(|(key, pressed_on, at)| {
                *key == letter && *pressed_on == entry && now.duration_since(*at) < CONFIRM_WINDOW
            }) {
                ui.confirm = None;
                true
            } else {
                ui.confirm = Some((letter, entry.clone(), now));
                ui.workbench
                    .notify(format!("Press {letter} again to {what}."), false);
                false
            }
        };
        let approval = match item {
            Item::Approval(approval) => Some(approval.clone()),
            Item::Job(job) => self
                .workbench
                .snapshot
                .iter()
                .flat_map(|snapshot| snapshot.approvals.iter())
                .find(|approval| approval.session == job.session && approval.job == job.job)
                .cloned(),
            _ => None,
        };
        match (letter, item) {
            ('y' | 'n', _) if approval.is_some() => {
                if let Some(approval) = approval {
                    self.request(
                        Tag::Action,
                        DaemonCommand::WorkbenchApprove {
                            id: approval.id,
                            approved: letter == 'y',
                        },
                    );
                }
            }
            ('x', Item::Job(job)) if !job.state.is_final() => {
                if confirmed(self, &format!("stop {}", job.job)) {
                    self.request(
                        Tag::Action,
                        DaemonCommand::WorkbenchCancel {
                            session: job.session.clone(),
                            job: job.job.clone(),
                        },
                    );
                }
            }
            ('r', Item::Job(job)) if job.resumable => self.request(
                Tag::Action,
                DaemonCommand::WorkbenchResume {
                    session: job.session.clone(),
                    job: job.job.clone(),
                },
            ),
            ('r', Item::Restorable(session)) => {
                let opening =
                    self.opening(PathBuf::from(&session.workspace), Some(session.id.clone()));
                self.openings.push(opening);
            }
            ('d', Item::Job(job)) if job.state.is_final() => {
                if confirmed(self, &format!("forget {}", job.job)) {
                    self.request(
                        Tag::Action,
                        DaemonCommand::WorkbenchDismiss {
                            session: job.session.clone(),
                            job: job.job.clone(),
                        },
                    );
                }
            }
            ('w', _) => self.open_overlay(Overlay::Worktrees),
            _ => {}
        }
    }

    /// A chat tab's line in the sidebar and tab bar.
    pub(crate) fn chat_state(app: &App) -> AgentState {
        if !app.connected {
            AgentState::Unknown
        } else if app.pending_approval.is_some() {
            AgentState::AwaitingInput
        } else if app.turn.is_some() {
            AgentState::Working
        } else {
            AgentState::Idle
        }
    }
}

/// A chat tab's sidebar line.
fn tab_line(number: usize, app: &App) -> String {
    format!(
        "{number} {} {}",
        mark(Ui::chat_state(app)),
        short_path(&app.cwd, 24)
    )
}

/// A chat tab's connection failed in a way other than ending (a frame too
/// large or unreadable): say so, and disconnect the tab.
fn connection_failed(app: &mut App, error: &anyhow::Error) {
    app.push_item(TranscriptItem::Error(format!(
        "The connection to the server failed: {error:#}"
    )));
    app.disconnect();
}

/// Say what a session restored from one that stopped.
fn note_restored(app: &mut App) {
    let Some(restored) = app.restored.take() else {
        return;
    };
    let mut text = format!(
        "Restored {} queued prompt(s) from session {} after SCV stopped. The queue is paused: \
         review it, then Alt+P resumes it.",
        restored.queued,
        restored.from.chars().take(8).collect::<String>()
    );
    if let Some(interrupted) = restored.interrupted {
        text.push_str(&format!(
            " The prompt that was running was not sent again (its outcome is unknown): {}",
            bounded_text(&interrupted, 200)
        ));
    }
    app.push_item(TranscriptItem::System(text));
}

fn insert(view: &mut crate::workbench::AgentView, character: char) {
    let index = byte_index(&view.input, view.cursor);
    view.input.insert(index, character);
    view.cursor += 1;
}

/// `path` as typed: `~` is the home directory, and a relative path is
/// relative to `base`.
fn expand(path: &str, base: &Path) -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        if path == "~" {
            return PathBuf::from(home);
        }
        if let Some(rest) = path.strip_prefix("~/") {
            return PathBuf::from(home).join(rest);
        }
    }
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

/// A `/job` command's parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobCommand {
    pub(crate) agent: String,
    pub(crate) prompt: String,
    pub(crate) cwd: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) worktree: bool,
}

const JOB_USAGE: &str =
    "Usage: /job <agent> [--cwd DIR] [--model M] [--effort E] [--worktree] <prompt>";

/// Parse `/job`'s arguments: the agent, options, then the prompt.
pub(crate) fn parse_job(text: &str) -> std::result::Result<JobCommand, String> {
    let mut words = text.split_whitespace().peekable();
    let agent = words.next().ok_or_else(|| JOB_USAGE.to_owned())?.to_owned();
    let mut job = JobCommand {
        agent,
        prompt: String::new(),
        cwd: None,
        model: None,
        effort: None,
        worktree: false,
    };
    while let Some(word) = words.peek().copied() {
        let value = |words: &mut std::iter::Peekable<std::str::SplitWhitespace<'_>>| {
            words.next();
            words
                .next()
                .map(str::to_owned)
                .ok_or_else(|| JOB_USAGE.to_owned())
        };
        match word {
            "--cwd" => job.cwd = Some(value(&mut words)?),
            "--model" => job.model = Some(value(&mut words)?),
            "--effort" => job.effort = Some(value(&mut words)?),
            "--worktree" => {
                words.next();
                job.worktree = true;
            }
            _ => break,
        }
    }
    job.prompt = words.collect::<Vec<_>>().join(" ");
    if job.prompt.is_empty() {
        return Err(JOB_USAGE.to_owned());
    }
    Ok(job)
}

#[cfg(test)]
mod tests;
