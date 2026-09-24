//! Short status lines a running tool reports for display, bounded and paced.

use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

/// Longest progress line a tool can report; longer lines are cut.
pub const MAX_PROGRESS_LINE_BYTES: usize = 200;
/// Largest progress event: the newest lines reported since the previous
/// event, with older ones dropped first.
pub const MAX_PROGRESS_EVENT_BYTES: usize = 512;
/// Minimum spacing of one call's progress events (at most two a second).
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// Where a running tool reports short status lines, such as a delegated
/// agent's commands. Each report becomes one bounded line; the runtime
/// forwards the pending lines to the client at most twice a second and never
/// adds them to the model's history. The default sink discards reports, so a
/// tool may always report.
#[derive(Clone, Default)]
pub struct ProgressSink {
    pending: Option<Arc<Mutex<PendingProgress>>>,
}

impl fmt::Debug for ProgressSink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProgressSink")
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

impl ProgressSink {
    /// A sink that keeps reports until the runtime takes them.
    pub fn buffered() -> Self {
        Self {
            pending: Some(Arc::default()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.pending.is_some()
    }

    /// Report one status line. Control characters and line breaks become
    /// spaces and the line is cut to `MAX_PROGRESS_LINE_BYTES`.
    pub fn report(&self, text: &str) {
        let Some(pending) = &self.pending else {
            return;
        };
        let line = progress_line(text);
        if !line.is_empty() {
            pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(line);
        }
    }

    /// The lines reported since the previous call, as one event text of at
    /// most `MAX_PROGRESS_EVENT_BYTES`, or `None` when nothing is pending.
    pub fn take(&self) -> Option<String> {
        self.pending
            .as_ref()?
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// Marker for lines dropped from the front of an event.
const PROGRESS_ELIDED: &str = "…";

#[derive(Debug, Default)]
struct PendingProgress {
    lines: VecDeque<String>,
    /// Joined length of `lines`, separators included.
    bytes: usize,
    dropped: bool,
}

impl PendingProgress {
    fn push(&mut self, line: String) {
        self.bytes += line.len() + usize::from(!self.lines.is_empty());
        self.lines.push_back(line);
        // Leave room for the elision marker and its separator.
        let budget = MAX_PROGRESS_EVENT_BYTES - PROGRESS_ELIDED.len() - 1;
        while self.bytes > budget && self.lines.len() > 1 {
            if let Some(oldest) = self.lines.pop_front() {
                self.bytes -= oldest.len() + 1;
                self.dropped = true;
            }
        }
    }

    fn take(&mut self) -> Option<String> {
        if self.lines.is_empty() {
            return None;
        }
        let mut text = String::with_capacity(self.bytes + PROGRESS_ELIDED.len() + 1);
        if std::mem::take(&mut self.dropped) {
            text.push_str(PROGRESS_ELIDED);
            text.push('\n');
        }
        for (index, line) in self.lines.drain(..).enumerate() {
            if index > 0 {
                text.push('\n');
            }
            text.push_str(&line);
        }
        self.bytes = 0;
        Some(text)
    }
}

/// One bounded display line: control characters become spaces, runs of
/// whitespace collapse, and the result is cut on a character boundary.
fn progress_line(text: &str) -> String {
    let mut line = String::new();
    for word in text
        .split(|character: char| character.is_whitespace() || character.is_control())
        .filter(|word| !word.is_empty())
    {
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
        if line.len() > MAX_PROGRESS_LINE_BYTES {
            break;
        }
    }
    if line.len() <= MAX_PROGRESS_LINE_BYTES {
        return line;
    }
    let mut end = MAX_PROGRESS_LINE_BYTES - PROGRESS_ELIDED.len();
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line.truncate(end);
    line.push_str(PROGRESS_ELIDED);
    line
}

#[cfg(test)]
mod tests;
