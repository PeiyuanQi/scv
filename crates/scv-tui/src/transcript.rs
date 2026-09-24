//! What the transcript shows, and [`BoundedLog`], which keeps it (and the
//! prompt history) within the limits the server sets.

use std::collections::VecDeque;

#[derive(Clone)]
pub(crate) enum TranscriptItem {
    User(String),
    Assistant {
        content: String,
        streaming: bool,
    },
    Tool {
        call_id: String,
        name: String,
        status: ToolStatus,
        arguments: String,
        output: String,
        /// The latest progress line while the tool runs.
        progress: String,
        expanded: bool,
    },
    System(String),
    Error(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolStatus {
    Proposed,
    Approval,
    Running,
    Success,
    Denied,
    Cancelled,
    Failed,
}

/// The bytes an entry counts against a [`BoundedLog`]'s byte limit.
pub(crate) trait Measure {
    fn size(&self) -> usize;
}

impl Measure for String {
    fn size(&self) -> usize {
        self.len()
    }
}

impl Measure for TranscriptItem {
    fn size(&self) -> usize {
        match self {
            Self::User(value) | Self::System(value) | Self::Error(value) => value.len(),
            Self::Assistant { content, .. } => content.len(),
            Self::Tool {
                call_id,
                name,
                arguments,
                output,
                progress,
                ..
            } => call_id.len() + name.len() + arguments.len() + output.len() + progress.len(),
        }
    }
}

/// A log that drops its oldest entries to stay within an entry count and a
/// byte total. Every change goes through a method that keeps the byte total
/// right, so entries are edited with the `update_*` methods.
pub(crate) struct BoundedLog<T> {
    entries: VecDeque<T>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}

impl<T: Measure> BoundedLog<T> {
    pub(crate) fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            max_entries,
            max_bytes,
        }
    }

    /// New limits, applied by the next [`push`](Self::push) or
    /// [`trim`](Self::trim).
    pub(crate) fn set_limits(&mut self, max_entries: usize, max_bytes: usize) {
        self.max_entries = max_entries;
        self.max_bytes = max_bytes;
    }

    pub(crate) fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Append `entry`, then drop the oldest entries while over a limit.
    /// Returns whether any were dropped.
    pub(crate) fn push(&mut self, entry: T) -> bool {
        self.bytes = self.bytes.saturating_add(entry.size());
        self.entries.push_back(entry);
        self.trim()
    }

    /// Put `entry` first without enforcing the limits, such as a marker that
    /// older entries were dropped.
    pub(crate) fn push_front(&mut self, entry: T) {
        self.bytes += entry.size();
        self.entries.push_front(entry);
    }

    /// Drop the oldest entries while over a limit; returns whether any were.
    pub(crate) fn trim(&mut self) -> bool {
        let mut trimmed = false;
        while self.entries.len() > self.max_entries || self.bytes > self.max_bytes {
            let Some(removed) = self.entries.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(removed.size());
            trimmed = true;
        }
        trimmed
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn front(&self) -> Option<&T> {
        self.entries.front()
    }

    pub(crate) fn back(&self) -> Option<&T> {
        self.entries.back()
    }

    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.entries.iter()
    }

    /// Change the newest entry.
    pub(crate) fn update_back<R>(&mut self, change: impl FnOnce(&mut T) -> R) -> Option<R> {
        let index = self.entries.len().checked_sub(1)?;
        self.update(index, change)
    }

    /// Change the newest entry that `matches`.
    pub(crate) fn update_last<R>(
        &mut self,
        matches: impl Fn(&T) -> bool,
        change: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        let index = self.entries.iter().rposition(matches)?;
        self.update(index, change)
    }

    /// Change every entry.
    pub(crate) fn update_all(&mut self, mut change: impl FnMut(&mut T)) {
        for entry in &mut self.entries {
            change(entry);
        }
        self.bytes = self.entries.iter().map(Measure::size).sum();
    }

    fn update<R>(&mut self, index: usize, change: impl FnOnce(&mut T) -> R) -> Option<R> {
        let entry = self.entries.get_mut(index)?;
        let before = entry.size();
        let result = change(entry);
        self.bytes = self
            .bytes
            .saturating_sub(before)
            .saturating_add(entry.size());
        Some(result)
    }
}

impl<T> std::ops::Index<usize> for BoundedLog<T> {
    type Output = T;

    fn index(&self, index: usize) -> &T {
        &self.entries[index]
    }
}

/// `value` cut to `max_chars` characters, with `…` when it was cut.
pub(crate) fn bounded_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_owned()
    } else {
        let mut output: String = value.chars().take(max_chars).collect();
        output.push('…');
        output
    }
}
