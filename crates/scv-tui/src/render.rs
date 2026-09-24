//! Drawing the terminal UI from [`App`].

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::{
    app::{App, PendingApproval},
    transcript::{ToolStatus, TranscriptItem, bounded_text},
};

pub(crate) fn render(frame: &mut Frame<'_>, app: &App) {
    let input_lines = app.input.lines().count().max(1) as u16;
    let composer_height = (input_lines + 2).clamp(3, 8);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let connection = if app.connected {
        "connected"
    } else {
        "disconnected"
    };
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " SCV ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(&app.model, Style::default().fg(Color::Cyan)),
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
    ]))
    .block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(header, layout[0]);

    let transcript = transcript_text(app);
    let raw_lines = transcript.lines.len().min(usize::from(u16::MAX)) as u16;
    let visible = layout[1].height.saturating_sub(2);
    let bottom = raw_lines.saturating_sub(visible);
    let scroll = if app.follow_output {
        bottom
    } else {
        bottom.saturating_sub(app.scroll)
    };
    let transcript_widget = Paragraph::new(transcript)
        .block(Block::default().borders(Borders::LEFT | Borders::RIGHT))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(transcript_widget, layout[1]);

    let composer_title = if app.turn.is_some() {
        " working · Esc cancels "
    } else {
        " message "
    };
    let composer = Paragraph::new(app.input.as_str())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(composer_title)
                .border_style(if app.turn.is_some() {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Cyan)
                }),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(composer, layout[2]);
    if app.pending_approval.is_none() {
        let before_cursor: String = app.input.chars().take(app.cursor).collect();
        let cursor_row = before_cursor.matches('\n').count() as u16;
        let cursor_col = before_cursor
            .rsplit('\n')
            .next()
            .unwrap_or("")
            .chars()
            .count() as u16;
        let x = layout[2].x + 1 + cursor_col.min(layout[2].width.saturating_sub(2));
        let y = layout[2].y + 1 + cursor_row.min(layout[2].height.saturating_sub(2));
        frame.set_cursor_position((x, y));
    }

    let elapsed = app
        .turn
        .as_ref()
        .map(|turn| format!(" · {:.1}s", turn.started_at.elapsed().as_secs_f32()))
        .unwrap_or_default();
    let context = app.context_after_tokens.map_or_else(
        || format!("ctx ≤{}", app.context_max_tokens),
        |tokens| format!("ctx ~{tokens}/{}", app.context_max_tokens),
    );
    let footer = Paragraph::new(format!(
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
    .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, layout[3]);

    if let Some(approval) = &app.pending_approval {
        render_approval(frame, approval);
    }
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
                    bounded_text(&entry.prompt, 180).replace('\n', " ↵ ")
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
                        format!("{symbol} {name}"),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {}", bounded_text(arguments, 160)),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
                if *status == ToolStatus::Running && !progress.is_empty() {
                    lines.push(Line::styled(
                        format!("  ↳ {progress}"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                if *expanded && !output.is_empty() {
                    for line in bounded_text(output, 4000).lines() {
                        lines.push(Line::styled(
                            format!("  {line}"),
                            Style::default().fg(Color::Gray),
                        ));
                    }
                }
            }
            TranscriptItem::System(content) => lines.push(Line::styled(
                format!("· {content}"),
                Style::default().fg(Color::DarkGray),
            )),
            TranscriptItem::Error(content) => lines.push(Line::styled(
                format!("! {content}"),
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
        lines.extend(content.lines().map(|line| Line::raw(line.to_owned())));
    }
}

fn render_approval(frame: &mut Frame<'_>, approval: &PendingApproval) {
    let area = centered_rect(80, 60, frame.area());
    frame.render_widget(Clear, area);
    let content = format!(
        "Tool: {}\nRisk: {}\nWorkspace: {}\n\n{}\n\n[y] allow once    [n/Esc] deny",
        approval.name,
        approval.risk,
        approval.cwd,
        bounded_text(&approval.summary, 4000)
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

fn short_path(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_owned()
    } else {
        let tail: String = value
            .chars()
            .rev()
            .take(max_chars.saturating_sub(1))
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!("…{tail}")
    }
}
