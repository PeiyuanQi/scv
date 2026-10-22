//! Drawing the terminal UI: the dashboard ([`render_ui`]) with its header,
//! tab bar, sidebar, panes, and overlays, and one chat on its own
//! ([`render`]). Nothing drawn interprets terminal escapes from model, agent,
//! or tool text: control characters are replaced first.

use std::time::Instant;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use scv_protocol::{AgentState, RunEvent, RunEventKind};

use crate::{
    app::{App, PendingApproval},
    keys::Action,
    transcript::{ToolStatus, TranscriptItem, bounded_text},
    ui::{Focus, Overlay, TabKind, Ui},
    workbench::{AgentView, Availability, Item, Target, mark, short_path},
};

/// The sidebar's width.
const SIDEBAR_WIDTH: u16 = 34;

/// Draw one chat on the whole screen: header, transcript, composer, and
/// status line.
pub(crate) fn render(frame: &mut Frame<'_>, app: &App) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(frame.area());
    let header =
        Paragraph::new(header_line(app, None)).block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(header, layout[0]);
    render_chat(frame, layout[1], app, true);
    frame.render_widget(chat_footer(app), layout[2]);
    if let Some(approval) = &app.pending_approval {
        render_approval(frame, approval);
    }
}

/// What the dashboard draws in a terminal of a given size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shown {
    /// Too small for the dashboard: the active chat is drawn on its own.
    pub(crate) alone: bool,
    pub(crate) sidebar: bool,
    /// Panes drawn: one, or two side by side.
    pub(crate) panes: usize,
}

/// What `ui` shows in a `width` by `height` terminal; keys go only where
/// it does.
pub(crate) fn shown(ui: &Ui, width: u16, height: u16) -> Shown {
    let active_chat = matches!(
        ui.active_tab()
            .and_then(|id| ui.tab(id))
            .map(|tab| &tab.kind),
        Some(TabKind::Chat(_))
    );
    let alone = (height < 12 || width < 50) && ui.overlay.is_none() && active_chat;
    let sidebar = !alone && ui.sidebar && width >= 70;
    let body = if sidebar {
        width.saturating_sub(SIDEBAR_WIDTH)
    } else {
        width
    };
    let panes = if !alone && ui.panes.len() > 1 && body >= 60 {
        2
    } else {
        1
    };
    Shown {
        alone,
        sidebar,
        panes,
    }
}

/// Draw the dashboard. A terminal too small for it shows the focused chat
/// on its own.
pub(crate) fn render_ui(frame: &mut Frame<'_>, ui: &Ui) {
    let area = frame.area();
    let shown = shown(ui, area.width, area.height);
    if shown.alone
        && let Some(TabKind::Chat(chat)) = ui
            .active_tab()
            .and_then(|id| ui.tab(id))
            .map(|tab| &tab.kind)
    {
        render(frame, &chat.app);
        return;
    }
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    let active = ui.active_tab();
    let active_app = active.and_then(|id| match ui.tab(id).map(|tab| &tab.kind) {
        Some(TabKind::Chat(chat)) => Some(&chat.app),
        _ => None,
    });
    frame.render_widget(Paragraph::new(header_line_ui(ui, active_app)), layout[0]);
    frame.render_widget(Paragraph::new(tab_bar(ui)), layout[1]);

    // The sidebar gives way on a narrow terminal.
    let sidebar = shown.sidebar;
    let body = if sidebar {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(20)])
            .split(layout[2]);
        render_sidebar(frame, columns[0], ui);
        columns[1]
    } else {
        layout[2]
    };
    let panes = if shown.panes > 1 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(body)
            .to_vec()
    } else {
        vec![body]
    };
    for (index, area) in panes.iter().enumerate() {
        let Some(id) = ui.panes.get(index) else {
            continue;
        };
        let focused = ui.focus == Focus::Pane(index);
        match ui.tab(*id).map(|tab| &tab.kind) {
            Some(TabKind::Chat(chat)) => render_chat(frame, *area, &chat.app, focused),
            Some(TabKind::Agent(target)) => {
                if let Some(view) = ui.workbench.views.get(target) {
                    render_agent(frame, *area, ui, view, focused);
                }
            }
            None => {}
        }
    }
    frame.render_widget(footer(ui, active_app), layout[3]);

    if let Some(id) = ui.covering_tab()
        && let Some(TabKind::Chat(chat)) = ui.tab(id).map(|tab| &tab.kind)
        && let Some(approval) = &chat.app.pending_approval
    {
        render_approval(frame, approval);
    }
    if let Some(overlay) = ui.overlay {
        render_overlay(frame, ui, overlay);
    }
}

fn header_line(app: &App, extra: Option<Span<'static>>) -> Line<'static> {
    let connection = if app.connected {
        "connected"
    } else {
        "disconnected"
    };
    let mut spans = vec![
        Span::styled(
            " SCV ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(clean(&app.model), Style::default().fg(Color::Cyan)),
        Span::raw("  ·  "),
        Span::raw(short_path(&app.cwd, 60)),
        Span::raw("  ·  "),
        Span::styled(
            connection,
            if app.connected {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Red)
            },
        ),
    ];
    if let Some(extra) = extra {
        spans.push(Span::raw("  ·  "));
        spans.push(extra);
    }
    Line::from(spans)
}

fn header_line_ui(ui: &Ui, app: Option<&App>) -> Line<'static> {
    let workbench = match ui.workbench.availability {
        Availability::Ready => {
            let (working, waiting, interrupted) = ui.workbench.counts();
            let mut text = format!("jobs: {working} running");
            if waiting > 0 {
                text.push_str(&format!(", {waiting} waiting for you"));
            }
            if interrupted > 0 {
                text.push_str(&format!(", {interrupted} interrupted"));
            }
            Span::styled(
                text,
                if waiting > 0 {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            )
        }
        Availability::Unsupported => Span::styled(
            "workbench: daemon too old",
            Style::default().fg(Color::DarkGray),
        ),
        Availability::Disconnected => {
            Span::styled("workbench: reconnecting", Style::default().fg(Color::Red))
        }
        Availability::Unknown => Span::raw(""),
    };
    match app {
        Some(app) => header_line(app, Some(workbench)),
        None => Line::from(vec![
            Span::styled(
                " SCV ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  agent view  ·  "),
            workbench,
        ]),
    }
}

fn tab_bar(ui: &Ui) -> Line<'static> {
    let active = ui.active_tab();
    let mut spans = Vec::new();
    for (index, tab) in ui.tabs.iter().enumerate() {
        let label = match &tab.kind {
            TabKind::Chat(chat) => format!(
                "{} {} {}",
                index + 1,
                mark(Ui::chat_state(&chat.app)),
                short_path(&chat.app.cwd, 18)
            ),
            TabKind::Agent(target) => {
                let (state, _) = ui.workbench.state(target);
                format!("{} {} {}", index + 1, mark(state), target_title(target))
            }
        };
        let label = clean(&label);
        let style = if Some(tab.id) == active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if ui.panes.contains(&tab.id) {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(format!(" {label} "), style));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

/// A short title for an agent view.
pub(crate) fn target_title(target: &Target) -> String {
    clean(match target {
        Target::Job { job, .. } => job,
        Target::Conversation { conversation, .. } => conversation,
        Target::Run { handle } => handle,
    })
}

fn state_color(state: AgentState) -> Color {
    match state {
        AgentState::AwaitingInput => Color::Yellow,
        AgentState::Working | AgentState::Starting => Color::Green,
        AgentState::Failed | AgentState::Timeout | AgentState::Interrupted => Color::Red,
        AgentState::Completed => Color::Cyan,
        AgentState::Unknown => Color::Magenta,
        _ => Color::Gray,
    }
}

fn render_sidebar(frame: &mut Frame<'_>, area: Rect, ui: &Ui) {
    let items = ui.sidebar_items();
    let focused = ui.focus == Focus::Sidebar;
    let width = usize::from(area.width.saturating_sub(3));
    let mut lines = Vec::new();
    let mut selected_line = 0usize;
    let current = ui.workbench.selection(&items);
    for (index, item) in items.iter().enumerate() {
        let selected = focused && Some(index) == current;
        if selected {
            selected_line = lines.len();
        }
        let (text, style) = match item {
            Item::Header(title) => (
                title.clone(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Item::Tab(_, line) => (format!(" {line}"), Style::default()),
            Item::Approval(approval) => (
                format!(" ? {} {}", clean(&approval.job), clean(&approval.summary)),
                Style::default().fg(Color::Yellow),
            ),
            Item::Job(job) => {
                let mut text = format!(
                    " {} {} {} {}",
                    mark(job.state),
                    clean(&job.job),
                    clean(&job.agent),
                    clean(&job.task)
                );
                if let Some(worktree) = &job.worktree {
                    text.push_str(&format!(" [{}]", clean(&worktree.branch)));
                }
                (text, Style::default().fg(state_color(job.state)))
            }
            Item::Run(run) => (
                format!(
                    " {} {} {}",
                    mark(run.state),
                    clean(run.conversation.as_deref().unwrap_or(&run.handle)),
                    clean(&run.agent)
                ),
                Style::default().fg(state_color(run.state)),
            ),
            Item::Restorable(session) => (
                format!(
                    " ↺ {} queued · {}",
                    session.queued,
                    short_path(&session.workspace, 16)
                ),
                Style::default().fg(Color::Gray),
            ),
        };
        let style = if selected {
            style.add_modifier(Modifier::REVERSED)
        } else {
            style
        };
        // Headers and tab lines carry workspace paths, which may hold
        // anything a directory name can.
        lines.push(Line::styled(bounded_text(&clean(&text), width), style));
    }
    if items.is_empty() {
        lines.push(Line::styled(
            "Nothing yet.",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let visible = usize::from(area.height.saturating_sub(2));
    let scroll = selected_line.saturating_sub(visible.saturating_sub(1));
    let title = if focused {
        " workbench · Enter open · y/n · x stop · r resume "
    } else {
        " workbench "
    };
    let widget = Paragraph::new(Text::from(lines))
        .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(if focused {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(Color::DarkGray)
                }),
        );
    frame.render_widget(widget, area);
}

/// Draw a chat's transcript and composer in `area`.
pub(crate) fn render_chat(frame: &mut Frame<'_>, area: Rect, app: &App, focused: bool) {
    let input_lines = app.input.lines().count().max(1) as u16;
    let composer_height = (input_lines + 2).clamp(3, 8);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(composer_height)])
        .split(area);

    let transcript = Paragraph::new(transcript_text(app)).wrap(Wrap { trim: false });
    let scroll = scroll_to(&transcript, layout[0], app.follow_output, app.scroll);
    let transcript_widget = transcript
        .block(Block::default().borders(Borders::LEFT | Borders::RIGHT))
        .scroll((scroll, 0));
    frame.render_widget(transcript_widget, layout[0]);

    let composer_title = if app.turn.is_some() {
        " working · Esc cancels "
    } else {
        " message "
    };
    let border = if !focused {
        Style::default().fg(Color::DarkGray)
    } else if app.turn.is_some() {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Cyan)
    };
    let composer = Paragraph::new(clean_block(&app.input))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(composer_title)
                .border_style(border),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(composer, layout[1]);
    if focused && app.pending_approval.is_none() {
        set_cursor(frame, layout[1], &app.input, app.cursor);
    }
}

fn set_cursor(frame: &mut Frame<'_>, area: Rect, input: &str, cursor: usize) {
    let before_cursor: String = input.chars().take(cursor).collect();
    let cursor_row = before_cursor.matches('\n').count() as u16;
    let cursor_col = before_cursor
        .rsplit('\n')
        .next()
        .unwrap_or("")
        .chars()
        .count() as u16;
    let x = area.x + 1 + cursor_col.min(area.width.saturating_sub(2));
    let y = area.y + 1 + cursor_row.min(area.height.saturating_sub(2));
    frame.set_cursor_position((x, y));
}

fn chat_footer(app: &App) -> Paragraph<'static> {
    let elapsed = app
        .turn
        .as_ref()
        .map(|turn| format!(" · {:.1}s", turn.started_at.elapsed().as_secs_f32()))
        .unwrap_or_default();
    let context = app.context_after_tokens.map_or_else(
        || format!("ctx ≤{}", app.context_max_tokens),
        |tokens| format!("ctx ~{tokens}/{}", app.context_max_tokens),
    );
    Paragraph::new(format!(
        " {}{}  ·  {}  ·  {} queue{}  ·  Enter send  Ctrl+J newline  /help",
        if app.turn.is_some() {
            "working"
        } else {
            "ready"
        },
        elapsed,
        context,
        app.queue.len(),
        if app.queue_paused { " paused" } else { "" },
    ))
    .style(Style::default().fg(Color::DarkGray))
}

fn footer(ui: &Ui, app: Option<&App>) -> Paragraph<'static> {
    if let Some(notice) = ui.workbench.latest_notice(Instant::now()) {
        return Paragraph::new(format!(" {}", clean(&notice.text))).style(if notice.urgent {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        });
    }
    let keys = format!(
        "{} sidebar  {} tabs  {} split  {} help",
        ui.keys.label(Action::FocusSidebar),
        ui.keys.label(Action::NextTab),
        ui.keys.label(Action::Split),
        ui.keys.label(Action::Help),
    );
    match app {
        Some(app) => {
            let elapsed = app
                .turn
                .as_ref()
                .map(|turn| format!(" · {:.1}s", turn.started_at.elapsed().as_secs_f32()))
                .unwrap_or_default();
            Paragraph::new(format!(
                " {}{}  ·  {} queue{}  ·  Enter send  ·  {keys}",
                if app.turn.is_some() { "working" } else { "ready" },
                elapsed,
                app.queue.len(),
                if app.queue_paused { " paused" } else { "" },
            ))
            .style(Style::default().fg(Color::DarkGray))
        }
        None => Paragraph::new(format!(
            " Enter send to the conversation  ·  Ctrl+Y/Ctrl+N approve/deny  ·  Ctrl+X stop  ·  Ctrl+R resume  ·  {keys}"
        ))
        .style(Style::default().fg(Color::DarkGray)),
    }
}

/// Draw an agent view: what the agent is doing, its live log, and the
/// composer that sends it the owner's prompts.
fn render_agent(frame: &mut Frame<'_>, area: Rect, ui: &Ui, view: &AgentView, focused: bool) {
    let approval = ui.workbench.approval(&view.target);
    let job = ui.workbench.job(&view.target);
    let conversation = ui.workbench.conversation(&view.target);
    let (state, detail) = ui.workbench.state(&view.target);
    let banner = if approval.is_some() { 3 } else { 0 };
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(banner),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(area);

    let mut title = vec![Span::styled(
        format!(" {} ", target_title(&view.target)),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if let Some(job) = job {
        title.push(Span::raw(format!(
            "{} · {} ",
            clean(&job.agent),
            clean(&job.task)
        )));
    }
    if let Some((_, conversation)) = &conversation {
        title.push(Span::styled(
            format!("· {} ", clean(conversation)),
            Style::default().fg(Color::Cyan),
        ));
    }
    let mut status = vec![Span::styled(
        format!(" {} {}", mark(state), state),
        Style::default()
            .fg(state_color(state))
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(detail) = detail {
        status.push(Span::raw(format!(": {}", clean(&detail))));
    }
    if let Some(worktree) = job.and_then(|job| job.worktree.as_ref()) {
        status.push(Span::styled(
            format!(
                "  [worktree {} · {}]",
                clean(&worktree.branch),
                short_path(&worktree.path, 30)
            ),
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(
        Paragraph::new(Text::from(vec![Line::from(title), Line::from(status)])),
        layout[0],
    );

    if let Some(approval) = approval {
        let banner = Paragraph::new(format!(
            "{} asks ({}): {}\nCtrl+Y approve · Ctrl+N deny",
            clean(&approval.name),
            clean(&approval.risk),
            clean(&approval.summary)
        ))
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(Color::Yellow))
        .block(Block::default().borders(Borders::TOP | Borders::BOTTOM));
        frame.render_widget(banner, layout[1]);
    }

    let log = Paragraph::new(Text::from(agent_lines(view))).wrap(Wrap { trim: false });
    let scroll = scroll_to(&log, layout[2], view.follow, view.scroll);
    frame.render_widget(
        log.scroll((scroll, 0))
            .block(Block::default().borders(Borders::LEFT | Borders::RIGHT)),
        layout[2],
    );

    let composer_title = match &conversation {
        Some((_, conversation)) if !state.is_final() || state == AgentState::Completed => {
            format!(
                " send to {} (joins its running turn when it can) ",
                clean(conversation)
            )
        }
        Some((_, conversation)) => format!(" send to {} ", clean(conversation)),
        None if job.is_some_and(|job| job.resumable) => " interrupted · Ctrl+R resumes it ".into(),
        None => " no conversation yet ".into(),
    };
    frame.render_widget(
        Paragraph::new(clean_block(&view.input))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(composer_title)
                    .border_style(if focused {
                        Style::default().fg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    }),
            ),
        layout[3],
    );
    if focused {
        set_cursor(frame, layout[3], &view.input, view.cursor);
    }
}

/// Where `text` (wrapping, drawn in `area` between a left and a right
/// border) scrolls to: its last rows when following, else `back` rows above
/// them. Rows are counted after wrapping, so the newest line stays in view.
fn scroll_to(text: &Paragraph<'_>, area: Rect, follow: bool, back: u16) -> u16 {
    let rows = text.line_count(area.width.saturating_sub(2));
    let bottom = u16::try_from(rows)
        .unwrap_or(u16::MAX)
        .saturating_sub(area.height);
    if follow {
        bottom
    } else {
        bottom.saturating_sub(back)
    }
}

/// An agent view's log as display lines.
fn agent_lines(view: &AgentView) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if view.lines.is_empty() {
        lines.push(Line::styled(
            "Waiting for the agent's activity…",
            Style::default().fg(Color::DarkGray),
        ));
    }
    for event in view.lines.iter() {
        lines.extend(event_lines(event));
    }
    lines
}

fn event_lines(event: &RunEvent) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let text = clean(&event.text);
    match event.kind {
        RunEventKind::RunStarted => vec![Line::styled(format!("· started: {text}"), dim)],
        RunEventKind::TurnStarted => {
            let mut lines = vec![Line::styled(
                format!(
                    "── turn {}{} ──",
                    event.turn.unwrap_or(1),
                    event
                        .job
                        .as_deref()
                        .map(|job| format!(" · {}", clean(job)))
                        .unwrap_or_default()
                ),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )];
            lines.extend(event.text.lines().take(6).map(|line| {
                Line::styled(
                    format!("▸ {}", clean(line)),
                    Style::default().fg(Color::Cyan),
                )
            }));
            lines
        }
        RunEventKind::Text => vec![Line::raw(text)],
        RunEventKind::Tool => vec![Line::styled(
            format!("  ● {text}"),
            Style::default().fg(Color::Yellow),
        )],
        RunEventKind::ToolFailed => vec![Line::styled(
            format!("  ✗ {text}"),
            Style::default().fg(Color::Red),
        )],
        RunEventKind::Plan => vec![Line::styled(
            format!("  ☐ {text}"),
            Style::default().fg(Color::Magenta),
        )],
        RunEventKind::ApprovalRequested => vec![Line::styled(
            format!(
                "? approval ({}): {text}",
                clean(event.risk.as_deref().unwrap_or("?"))
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )],
        RunEventKind::ApprovalResolved => vec![Line::styled(
            format!(
                "  → {}",
                clean(event.status.as_deref().unwrap_or("answered"))
            ),
            dim,
        )],
        RunEventKind::Steered => vec![Line::styled(
            format!("you ▸ {text}"),
            Style::default().fg(Color::Cyan),
        )],
        RunEventKind::TurnEnded => {
            let status = clean(event.status.as_deref().unwrap_or("ended"));
            let style = if status == "completed" {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Red)
            };
            let mut lines = vec![Line::styled(format!("── turn {status} ──"), style)];
            if !text.is_empty() {
                lines.push(Line::styled(format!("   {text}"), style));
            }
            lines
        }
        RunEventKind::RunEnded => vec![Line::styled(
            format!(
                "· process {}",
                clean(event.status.as_deref().unwrap_or("ended"))
            ),
            dim,
        )],
        RunEventKind::Note | RunEventKind::Unknown => vec![Line::styled(format!("· {text}"), dim)],
        RunEventKind::Truncated => vec![Line::styled(
            "· the log reached its size limit; only turns and approvals follow",
            dim,
        )],
    }
}

fn render_overlay(frame: &mut Frame<'_>, ui: &Ui, overlay: Overlay) {
    let area = centered_rect(84, 80, frame.area());
    frame.render_widget(Clear, area);
    let (title, lines) = match overlay {
        Overlay::Help => (" keys and commands · Esc closes ", help_lines(ui)),
        Overlay::Notifications => (
            " notifications · Esc closes ",
            ui.workbench
                .notices
                .iter()
                .rev()
                .map(|notice| {
                    Line::styled(
                        clean(&notice.text),
                        if notice.urgent {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default()
                        },
                    )
                })
                .collect(),
        ),
        Overlay::Worktrees => (
            " worktrees · r refreshes · Esc closes ",
            match &ui.workbench.worktrees {
                None => vec![Line::raw("Loading…")],
                Some(list) if list.is_empty() => {
                    vec![Line::raw("SCV has created no worktrees.")]
                }
                Some(list) => list
                    .iter()
                    .flat_map(|info| {
                        let dirty = match info.dirty {
                            Some(true) => "uncommitted changes",
                            Some(false) => "clean",
                            None => "status unknown",
                        };
                        vec![
                            Line::styled(
                                format!("{} · {dirty}", clean(&info.worktree.branch)),
                                Style::default().add_modifier(Modifier::BOLD),
                            ),
                            Line::raw(format!("  {}", clean(&info.worktree.path))),
                            Line::styled(
                                format!(
                                    "  from {}{}",
                                    clean(&info.worktree.repo),
                                    if info.jobs.is_empty() {
                                        String::new()
                                    } else {
                                        format!(" · jobs {}", clean(&info.jobs.join(", ")))
                                    }
                                ),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ]
                    })
                    .collect(),
            },
        ),
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((ui.overlay_scroll, 0))
            .block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn help_lines(ui: &Ui) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines = vec![Line::styled(
        "Dashboard keys ([tui.keys] changes them)",
        bold,
    )];
    for action in Action::all() {
        lines.push(Line::raw(format!(
            "  {:<14} {}",
            ui.keys.label(action),
            action.describe()
        )));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Chat", bold));
    for line in [
        "  Enter          send; while a turn runs, queue it",
        "  Ctrl+J         new line",
        "  Esc            cancel the running turn",
        "  Alt+Up/Down    edit a queued prompt · Ctrl+X remove it · Alt+P pause the queue",
        "  Ctrl+O         show or hide the last tool's output",
        "  PageUp/Down    scroll · End follow",
    ] {
        lines.push(Line::raw(line));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Workbench sidebar", bold));
    for line in [
        "  Enter          open the job, conversation, tab, or restorable queue",
        "  y / n          approve or deny the request it waits on",
        "  x x            stop the job (press twice)",
        "  r              resume an interrupted job, or restore a queue",
        "  d d            forget a finished job (press twice)",
        "  w              list the worktrees SCV created",
    ] {
        lines.push(Line::raw(line));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Agent view", bold));
    for line in [
        "  Enter          send the prompt to the conversation: it joins the running turn when the agent can take it, else queues",
        "  Ctrl+Y/Ctrl+N  approve or deny its approval request (y and n are text)",
        "  Ctrl+X Ctrl+X  stop its job · Ctrl+R resume an interrupted one",
    ] {
        lines.push(Line::raw(line));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Commands", bold));
    for line in [
        "  /open DIR      a chat tab in another workspace",
        "  /job AGENT [--cwd D] [--model M] [--effort E] [--worktree] PROMPT",
        "                 start your own background job, watched here",
        "  /restore ID    a chat tab with a stopped session's queued prompts",
        "  /jobs  /worktrees  /notifications  /close  /clear  /context  /quit",
    ] {
        lines.push(Line::raw(line));
    }
    lines
}

fn transcript_text(app: &App) -> Text<'static> {
    let mut lines = Vec::new();
    if app.items.is_empty() {
        lines.push(Line::styled(
            "Ask SCV to inspect, change, or explain this workspace.",
            Style::default().fg(Color::DarkGray),
        ));
    }
    if !app.queue.is_empty() {
        lines.push(Line::styled(
            format!(
                "queue ({}{})",
                app.queue.len(),
                if app.queue_paused { ", paused" } else { "" }
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        for (index, entry) in app.queue.iter().enumerate() {
            lines.push(Line::styled(
                format!(
                    "  {}. {}",
                    index + 1,
                    clean(&bounded_text(&entry.prompt, 180).replace('\n', " ↵ "))
                ),
                Style::default().fg(Color::Yellow),
            ));
        }
        lines.push(Line::raw(""));
    }
    for item in app.items.iter() {
        match item {
            TranscriptItem::User(content) => push_content(
                &mut lines,
                "you",
                content,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            TranscriptItem::Assistant { content, streaming } => {
                let label = if *streaming { "scv…" } else { "scv" };
                push_content(
                    &mut lines,
                    label,
                    content,
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                );
            }
            TranscriptItem::Tool {
                name,
                status,
                arguments,
                output,
                progress,
                expanded,
                ..
            } => {
                let (symbol, color) = match status {
                    ToolStatus::Proposed => ("○", Color::DarkGray),
                    ToolStatus::Approval => ("?", Color::Yellow),
                    ToolStatus::Running => ("●", Color::Yellow),
                    ToolStatus::Success => ("✓", Color::Green),
                    ToolStatus::Denied => ("⊘", Color::Yellow),
                    ToolStatus::Cancelled => ("■", Color::DarkGray),
                    ToolStatus::Failed => ("✗", Color::Red),
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{symbol} {}", clean(name)),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {}", clean(&bounded_text(arguments, 160))),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
                if *status == ToolStatus::Running && !progress.is_empty() {
                    lines.push(Line::styled(
                        format!("  ↳ {}", clean(progress)),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                if *expanded && !output.is_empty() {
                    for line in bounded_text(output, 4000).lines() {
                        lines.push(Line::styled(
                            format!("  {}", clean(line)),
                            Style::default().fg(Color::Gray),
                        ));
                    }
                }
            }
            TranscriptItem::System(content) => {
                // A note may run over several lines, such as a job's reply.
                let mut marker = "· ";
                for line in content.split('\n') {
                    lines.push(Line::styled(
                        format!("{marker}{}", clean(line)),
                        Style::default().fg(Color::DarkGray),
                    ));
                    marker = "  ";
                }
            }
            TranscriptItem::Error(content) => lines.push(Line::styled(
                format!("! {}", clean(content)),
                Style::default().fg(Color::Red),
            )),
        }
        lines.push(Line::raw(""));
    }
    Text::from(lines)
}

fn push_content(lines: &mut Vec<Line<'static>>, label: &str, content: &str, style: Style) {
    lines.push(Line::styled(label.to_owned(), style));
    if content.is_empty() {
        lines.push(Line::raw(""));
    } else {
        lines.extend(content.lines().map(|line| Line::raw(clean(line))));
    }
}

fn render_approval(frame: &mut Frame<'_>, approval: &PendingApproval) {
    let area = centered_rect(80, 60, frame.area());
    frame.render_widget(Clear, area);
    let content = format!(
        "Tool: {}\nRisk: {}\nWorkspace: {}\n\n{}\n\n[y] allow once    [n/Esc] deny",
        clean(&approval.name),
        clean(&approval.risk),
        clean(&approval.cwd),
        clean_block(&bounded_text(&approval.summary, 4000))
    );
    let widget = Paragraph::new(content)
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" approval required ")
                .border_style(Style::default().fg(Color::Yellow)),
        );
    frame.render_widget(widget, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// One line of display text: control characters, terminal escapes
/// included, become visible and inert, and tabs become spaces.
pub(crate) fn clean(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '\t' => ' ',
            '\n' | '\r' => ' ',
            control if control.is_control() => '\u{fffd}',
            other => other,
        })
        .collect()
}

/// Text that may span lines, cleaned like [`clean`] line by line.
fn clean_block(text: &str) -> String {
    text.split('\n').map(clean).collect::<Vec<_>>().join("\n")
}
