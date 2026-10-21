//! The dashboard around the chat: tabs (each a chat session in its own
//! workspace, or an agent view), one or two panes, the sidebar, overlays,
//! and the keys and slash commands that move between them. A chat tab's own
//! keys still go to [`crate::input`]; this decides where a key goes first.

use std::{
    collections::BTreeMap,
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
}

pub(crate) struct Ui {
    pub(crate) tabs: Vec<Tab>,
    next_tab: u64,
    /// The tab each pane shows: one pane, or two side by side.
    pub(crate) panes: Vec<u64>,
    pub(crate) focus: Focus,
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
    /// A destructive sidebar key waiting to be pressed again.
    confirm: Option<(char, Instant)>,
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
            sidebar: true,
            workbench: Workbench::default(),
            keys: keymap,
            overlay: None,
            overlay_scroll: 0,
            quit: false,
            requests: None,
            openings: Vec::new(),
            confirm: None,
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

    /// A chat tab connected: a new one, or one reconnecting.
    pub(crate) fn opened(
        &mut self,
        opening: &Opening,
        client: Client,
        session: crate::client::SessionInfo,
    ) {
        if let Some(id) = opening.tab
            && let Some(Tab {
                kind: TabKind::Chat(chat),
                ..
            }) = self.tab_mut(id)
        {
            chat.client = client;
            chat.app.reconnect(session);
            note_restored(&mut chat.app);
            return;
        }
        let mut app = App::new(session);
        note_restored(&mut app);
        let id = self.add_tab(TabKind::Chat(Box::new(ChatTab {
            client,
            app,
            cwd: opening.cwd.clone(),
        })));
        self.show(id);
    }

    /// A chat tab could not be opened.
    pub(crate) fn open_failed(&mut self, opening: &Opening, error: &anyhow::Error) {
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

    /// A chat tab's server sent `event`, or its connection ended.
    pub(crate) fn chat_event(
        &mut self,
        id: u64,
        result: Result<Option<ServerEvent>>,
    ) -> Result<()> {
        let focused = self.active_tab() == Some(id);
        let index = self.tabs.iter().position(|tab| tab.id == id);
        let Some(Tab {
            kind: TabKind::Chat(chat),
            ..
        }) = index.and_then(|index| self.tabs.get_mut(index))
        else {
            return Ok(());
        };
        let approval = match &result {
            Ok(Some(ServerEvent::ApprovalRequested { summary, .. })) if !focused => {
                Some(bounded_text(summary, 120))
            }
            _ => None,
        };
        chat.app.handle_server_result(result)?;
        let reconnect = (!chat.app.connected).then(|| Opening {
            cwd: chat.cwd.clone(),
            restore: Some(chat.app.session_id.clone()),
            tab: Some(id),
        });
        let number = index.map_or(0, |index| index + 1);
        if let Some(summary) = approval {
            self.workbench
                .notify(format!("Tab {number} asks for approval: {summary}"), true);
        }
        if let Some(opening) = reconnect
            && !self.openings.contains(&opening)
        {
            self.openings.push(opening);
        }
        Ok(())
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
        if let Some(action) = self.keys.action(key) {
            self.act(action);
            return Ok(());
        }
        match self.focus {
            Focus::Sidebar => {
                self.sidebar_key(key);
                Ok(())
            }
            Focus::Pane(_) => self.pane_key(key).await,
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
                if let Some(app) = self.focused_chat_app() {
                    app.input = "/open ".into();
                    app.cursor = app.input.chars().count();
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
        let Some(id) = self.active_tab() else {
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
        // A dashboard command typed into a chat composer.
        if key.code == KeyCode::Enter
            && key.modifiers == KeyModifiers::NONE
            && let Some(app) = self.focused_chat_app()
            && app.pending_approval.is_none()
            && app.queue_editing.is_none()
        {
            let line = app.input.trim().to_owned();
            if self.command(id, &line) {
                if let Some(app) = self.focused_chat_app() {
                    crate::input::clear_input(app);
                }
                return Ok(());
            }
        }
        let Some(Tab {
            kind: TabKind::Chat(chat),
            ..
        }) = self.tab_mut(id)
        else {
            return Ok(());
        };
        handle_key(&mut chat.client, &mut chat.app, key).await?;
        let quit = chat.app.quit;
        let reconnect = (!chat.app.connected).then(|| Opening {
            cwd: chat.cwd.clone(),
            restore: Some(chat.app.session_id.clone()),
            tab: Some(id),
        });
        if !chat.app.connected {
            chat.client.shutdown().await;
        }
        self.quit |= quit;
        if let Some(opening) = reconnect
            && !self.openings.contains(&opening)
        {
            self.openings.push(opening);
        }
        Ok(())
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
                    let path = expand(rest, &cwd);
                    self.openings.push(Opening {
                        cwd: path,
                        restore: None,
                        tab: None,
                    });
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
            [session] => self.openings.push(Opening {
                cwd: PathBuf::from(&session.workspace),
                restore: Some(session.id.clone()),
                tab: None,
            }),
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
            self.focus = Focus::Pane(0);
        }
        self.show(id);
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
            KeyCode::Char('y' | 'n') if view.input.is_empty() && !control && approval.is_some() => {
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
                let Some((session, conversation)) = conversation else {
                    self.workbench.notify(
                        "This view has no conversation to send to yet; wait for its first turn \
                         to start."
                            .into(),
                        false,
                    );
                    return;
                };
                view.input.clear();
                view.cursor = 0;
                view.follow = true;
                self.request(
                    Tag::Action,
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
        let position = selectable
            .iter()
            .position(|index| *index >= self.workbench.selected)
            .unwrap_or(selectable.len() - 1);
        let mut moved = |offset: isize| {
            let next = position
                .saturating_add_signed(offset)
                .min(selectable.len() - 1);
            self.workbench.selected = selectable[next];
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
                self.workbench.selected = selectable[position];
                if let Some(item) = items.get(self.workbench.selected).cloned() {
                    self.open_item(item);
                }
            }
            KeyCode::Char(letter) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.workbench.selected = selectable[position];
                if let Some(item) = items.get(self.workbench.selected).cloned() {
                    self.item_key(&item, letter);
                }
            }
            _ => {}
        }
    }

    fn open_item(&mut self, item: Item) {
        match item {
            Item::Header(_) => {}
            Item::Tab(id, _) => self.show(id),
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
            Item::Restorable(session) => self.openings.push(Opening {
                cwd: PathBuf::from(session.workspace),
                restore: Some(session.id),
                tab: None,
            }),
        }
    }

    fn item_key(&mut self, item: &Item, letter: char) {
        let confirmed = |ui: &mut Self, what: &str| {
            let now = Instant::now();
            if ui
                .confirm
                .is_some_and(|(key, at)| key == letter && now.duration_since(at) < CONFIRM_WINDOW)
            {
                ui.confirm = None;
                true
            } else {
                ui.confirm = Some((letter, now));
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
            ('r', Item::Restorable(session)) => self.openings.push(Opening {
                cwd: PathBuf::from(&session.workspace),
                restore: Some(session.id.clone()),
                tab: None,
            }),
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
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
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
