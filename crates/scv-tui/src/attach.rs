//! An agent view attached to a conversation taken over in its agent's own
//! terminal ([`scv_protocol::AttachPage`]): the screen the daemon keeps,
//! drawn cell by cell, and the keys typed into it, encoded as a terminal
//! sends them. The daemon sends styled text, never the agent's escape
//! sequences, so nothing here interprets agent output; text is still
//! cleaned of control characters before it is drawn.

use std::{
    cell::Cell,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt as _;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use scv_protocol::{AttachColor, AttachPage, AttachSpan, DaemonCommand};

use crate::{
    poller::{self, Answer, Tag},
    terminal::TerminalGuard,
    workbench::Target,
};

/// The key that detaches, as the help names it.
pub(crate) const DETACH_KEY: &str = "Ctrl+]";
/// How often an attached view reads its screen when nothing is typed.
const READ_EVERY: Duration = Duration::from_millis(50);
/// The most input one request carries; more waits for the next.
const MAX_SENT: usize = 32 * 1024;

/// A view's hold on a taken-over terminal, and the screen it shows.
pub(crate) struct Attached {
    /// The hold the daemon gave; reads and keys name it.
    pub(crate) id: String,
    pub(crate) conversation: String,
    pub(crate) agent: String,
    generation: u64,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    /// Each row's spans; a row past its last span is blank.
    screen: Vec<Vec<AttachSpan>>,
    pub(crate) cursor: Option<[u16; 2]>,
    application_cursor: bool,
    bracketed_paste: bool,
    /// Keys typed and not sent yet.
    pending: String,
    in_flight: bool,
    last_read: Option<Instant>,
    /// The size the view was last drawn at, which the terminal follows.
    pub(crate) drawn: Cell<(u16, u16)>,
    /// The size last asked for.
    asked: (u16, u16),
}

impl Attached {
    /// A view of the screen `page` shows.
    pub(crate) fn new(page: AttachPage) -> Self {
        let mut attached = Self {
            id: page.id.clone(),
            conversation: page.conversation.clone(),
            agent: page.agent.clone(),
            generation: 0,
            cols: page.cols,
            rows: page.rows,
            screen: Vec::new(),
            cursor: None,
            application_cursor: false,
            bracketed_paste: false,
            pending: String::new(),
            in_flight: false,
            last_read: None,
            drawn: Cell::new((page.cols, page.rows)),
            asked: (page.cols, page.rows),
        };
        attached.apply(page);
        attached
    }

    /// Take in a page of the screen.
    pub(crate) fn apply(&mut self, page: AttachPage) {
        self.in_flight = false;
        if page.full || self.screen.len() != usize::from(page.rows) {
            self.screen = vec![Vec::new(); usize::from(page.rows)];
        }
        for line in page.lines {
            if let Some(row) = self.screen.get_mut(usize::from(line.row)) {
                *row = line.spans;
            }
        }
        self.generation = page.generation;
        self.cols = page.cols;
        self.rows = page.rows;
        self.cursor = page.cursor;
        self.application_cursor = page.application_cursor;
        self.bracketed_paste = page.bracketed_paste;
    }

    /// The request that went unanswered failed: the next one may go.
    pub(crate) fn unanswered(&mut self) {
        self.in_flight = false;
    }

    /// Type `key` into the terminal, when it is one a terminal sends.
    pub(crate) fn type_key(&mut self, key: KeyEvent) {
        if let Some(bytes) = encode_key(key, self.application_cursor) {
            self.pending.push_str(&bytes);
        }
    }

    /// Paste `text` into the terminal, bracketed when the agent asked.
    pub(crate) fn paste(&mut self, text: &str) {
        self.pending
            .push_str(&encode_paste(text, self.bracketed_paste));
    }

    /// The next read of the screen, with what was typed, when one is due:
    /// one at a time, at once when keys wait, else every [`READ_EVERY`].
    pub(crate) fn due(&mut self, now: Instant) -> Option<DaemonCommand> {
        if self.in_flight {
            return None;
        }
        let drawn = self.drawn.get();
        let resized = drawn != self.asked && drawn.0 > 0 && drawn.1 > 0;
        let quiet = self
            .last_read
            .is_some_and(|last| now.duration_since(last) < READ_EVERY);
        if self.pending.is_empty() && !resized && quiet {
            return None;
        }
        let mut cut = self.pending.len().min(MAX_SENT);
        while !self.pending.is_char_boundary(cut) {
            cut -= 1;
        }
        let input: String = self.pending.drain(..cut).collect();
        self.in_flight = true;
        self.last_read = Some(now);
        let (cols, rows) = if resized {
            self.asked = drawn;
            (Some(drawn.0), Some(drawn.1))
        } else {
            (None, None)
        };
        Some(DaemonCommand::AttachIo {
            id: self.id.clone(),
            after: self.generation,
            input,
            cols,
            rows,
        })
    }

    /// The screen as lines to draw, cleaned of control characters.
    pub(crate) fn lines(&self) -> Vec<Line<'static>> {
        self.screen
            .iter()
            .map(|row| {
                Line::from(
                    row.iter()
                        .map(|span| Span::styled(crate::render::clean(&span.text), style(span)))
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    }
}

fn style(span: &AttachSpan) -> Style {
    let mut style = Style::default();
    if let Some(fg) = span.fg {
        style = style.fg(color(fg));
    }
    if let Some(bg) = span.bg {
        style = style.bg(color(bg));
    }
    for (on, modifier) in [
        (span.bold, Modifier::BOLD),
        (span.italic, Modifier::ITALIC),
        (span.underline, Modifier::UNDERLINED),
        (span.inverse, Modifier::REVERSED),
    ] {
        if on {
            style = style.add_modifier(modifier);
        }
    }
    style
}

fn color(color: AttachColor) -> Color {
    match color {
        AttachColor::Index(index) => Color::Indexed(index),
        AttachColor::Rgb([red, green, blue]) => Color::Rgb(red, green, blue),
    }
}

/// Whether `key` is the detach key, `Ctrl+]`: terminals without enhanced
/// keyboard reporting send it as the byte crossterm reads as `Ctrl+5`.
pub(crate) fn is_detach(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char(']' | '5'))
}

/// The bytes a terminal sends for `key`, with the cursor keys in
/// application mode when the agent asked for it; `None` for a key a
/// terminal does not send.
pub(crate) fn encode_key(key: KeyEvent, application_cursor: bool) -> Option<String> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // xterm's modifier parameter: 1 plus shift 1, alt 2, control 4.
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(control);
    let cursor = |letter: char| {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{letter}")
        } else if application_cursor {
            format!("\x1bO{letter}")
        } else {
            format!("\x1b[{letter}")
        }
    };
    let tilde = |number: u8| {
        if modifier > 1 {
            format!("\x1b[{number};{modifier}~")
        } else {
            format!("\x1b[{number}~")
        }
    };
    let bytes = match key.code {
        KeyCode::Char(character) if control => {
            let byte = match character.to_ascii_lowercase() {
                letter @ 'a'..='z' => letter as u8 - b'a' + 1,
                '@' | ' ' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '-' | '7' => 0x1f,
                '?' | '8' => 0x7f,
                _ => return None,
            };
            char::from(byte).to_string()
        }
        KeyCode::Char(character) => character.to_string(),
        KeyCode::Enter => "\r".into(),
        KeyCode::Tab => "\t".into(),
        KeyCode::BackTab => "\x1b[Z".into(),
        KeyCode::Backspace => "\x7f".into(),
        KeyCode::Esc => "\x1b".into(),
        KeyCode::Up => cursor('A'),
        KeyCode::Down => cursor('B'),
        KeyCode::Right => cursor('C'),
        KeyCode::Left => cursor('D'),
        KeyCode::Home => cursor('H'),
        KeyCode::End => cursor('F'),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(number @ 1..=4) => {
            let letter = char::from(b'P' + number - 1);
            if modifier > 1 {
                format!("\x1b[1;{modifier}{letter}")
            } else {
                format!("\x1bO{letter}")
            }
        }
        KeyCode::F(number @ 5..=12) => tilde(match number {
            5 => 15,
            6 => 17,
            7 => 18,
            8 => 19,
            9 => 20,
            10 => 21,
            11 => 23,
            _ => 24,
        }),
        _ => return None,
    };
    // Alt sends Escape first, except where it is in the sequence already.
    let sequenced = bytes.starts_with('\x1b') && bytes.len() > 1;
    Some(if alt && !sequenced {
        format!("\x1b{bytes}")
    } else {
        bytes
    })
}

/// The bytes a terminal sends for pasted `text`: bracketed when the agent
/// asked, with any bracket end inside it removed so the paste cannot end
/// early and be read as typed keys; otherwise with newlines as Enter.
pub(crate) fn encode_paste(text: &str, bracketed: bool) -> String {
    if bracketed {
        format!("\x1b[200~{}\x1b[201~", text.replace("\x1b[201~", ""))
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r")
    }
}

/// Attach this terminal to conversation `conversation` of `session`, taken
/// over in its agent's own terminal, until `Ctrl+]` detaches or the
/// terminal closes: `scv jobs attach`. Returns SCV's sentence about how it
/// ended. The terminal is restored however it ends.
pub async fn run_attach(socket: &Path, session: &str, conversation: &str) -> Result<String> {
    let (requests, mut answers) = poller::spawn(socket.to_owned());
    let mut guard = TerminalGuard::enter()?;
    let size = guard.terminal.size()?;
    // The poller's tags name views; this one has a single terminal.
    let target = Target::Conversation {
        session: session.to_owned(),
        conversation: conversation.to_owned(),
    };
    let send = |tag: Tag, command: DaemonCommand| {
        let _ = requests.send(poller::Request { tag, command });
    };
    send(
        Tag::Attach(target.clone()),
        DaemonCommand::Attach {
            session: session.to_owned(),
            conversation: conversation.to_owned(),
            cols: Some(size.width),
            rows: Some(size.height.saturating_sub(1).max(1)),
        },
    );
    let mut attached = match answers.recv().await {
        Some(Answer::Status(_, status)) => match status.attach {
            Some(page) => Attached::new(page),
            None => bail!("the daemon answered without a terminal"),
        },
        Some(Answer::Refused(_, message)) => bail!("{message}"),
        Some(Answer::Unsupported) => {
            bail!("this daemon cannot attach to an agent's terminal; restart it into this release")
        }
        _ => bail!("the daemon did not answer; is it running (`scv start`)?"),
    };
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(25));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Drawn again only when the screen or the terminal's size changed.
    let mut shown = None;
    loop {
        let now_shown = (attached.generation, attached.cursor, guard.terminal.size()?);
        if shown != Some(now_shown) {
            shown = Some(now_shown);
            guard.terminal.draw(|frame| {
                crate::render::render_terminal(frame, frame.area(), &attached, true);
            })?;
        }
        if let Some(command) = attached.due(Instant::now()) {
            send(Tag::AttachIo(target.clone()), command);
        }
        tokio::select! {
            _ = tick.tick() => {}
            answer = answers.recv() => match answer {
                Some(Answer::Status(_, status)) => match status.attach {
                    Some(page) if page.ended => {
                        return Ok(format!(
                            "{conversation}'s terminal closed ({}).",
                            page.exit.as_deref().unwrap_or("closed")
                        ));
                    }
                    Some(page) => attached.apply(page),
                    None => attached.unanswered(),
                },
                Some(Answer::Refused(_, message)) => bail!("{message}"),
                _ => bail!("the connection to the daemon broke; the agent may still run in its terminal"),
            },
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    if is_detach(key) {
                        let id = attached.id.clone();
                        send(Tag::AttachDone, DaemonCommand::AttachDetach { id });
                        // The detach is answered before the connection goes,
                        // after any read still in flight.
                        let _ = tokio::time::timeout(Duration::from_secs(2), async {
                            while let Some(answer) = answers.recv().await {
                                if matches!(
                                    answer,
                                    Answer::Status(Tag::AttachDone, _)
                                        | Answer::Refused(Tag::AttachDone, _)
                                        | Answer::Disconnected(Tag::AttachDone)
                                        | Answer::TimedOut(Tag::AttachDone)
                                        | Answer::Unsupported
                                ) {
                                    break;
                                }
                            }
                        })
                        .await;
                        return Ok(format!(
                            "Detached from {conversation}; it keeps running in its terminal \
                             (`scv jobs attach` again, or `--end` to end it)."
                        ));
                    }
                    attached.type_key(key);
                }
                Some(Ok(Event::Paste(text))) => attached.paste(&text),
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.into()),
                None => bail!("the terminal closed"),
            },
        }
    }
}

#[cfg(test)]
mod tests;
