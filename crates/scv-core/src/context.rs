//! Choosing the history that fits the model's context window.
//!
//! The runtime keeps the whole canonical history; before every model request
//! a [`ContextPolicy`] picks what the model sees. [`BudgetContextPolicy`], the
//! default, keeps the newest whole turns that fit a token budget and replaces
//! everything older with one summary note.

use std::collections::VecDeque;

use thiserror::Error;

use crate::{
    Message, ToolSpec,
    history::{char_tail, truncate_chars},
};

/// The token budget of [`BudgetContextPolicy`]. Tokens are estimated from
/// serialized bytes, not counted by a tokenizer.
#[derive(Debug, Clone)]
pub struct ContextConfig {
    /// The model's context window.
    pub max_tokens: usize,
    /// Room kept free for the model's answer.
    pub reserve_output_tokens: usize,
    /// Extra room for estimation error.
    pub safety_margin_tokens: usize,
    /// Bytes of serialized text counted as one token.
    pub bytes_per_token: usize,
    /// Longest summary note that stands in for compacted history.
    pub summary_max_chars: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_tokens: 128_000,
            reserve_output_tokens: 8_192,
            safety_margin_tokens: 2_048,
            bytes_per_token: 3,
            summary_max_chars: 6_000,
        }
    }
}

/// The messages chosen for one model request, and what compaction cost.
#[derive(Debug, Clone)]
pub struct ContextSelection {
    pub messages: Vec<Message>,
    /// Estimated tokens of the full request before compaction.
    pub before_tokens: usize,
    /// Estimated tokens of the request as sent.
    pub after_tokens: usize,
    /// History messages left out (replaced by a summary note).
    pub removed_messages: usize,
}

/// Why no selection fits: the budget is too small even for the newest turn,
/// or for the system prompt and tool schemas alone.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct ContextError(pub String);

/// Decides which history messages go into each model request.
pub trait ContextPolicy: Send + Sync {
    /// Choose the messages for a request with this system prompt and these
    /// tools. The newest message is the user's current input and must be kept.
    fn select(
        &self,
        history: &[Message],
        system_prompt: &str,
        tools: &[ToolSpec],
    ) -> Result<ContextSelection, ContextError>;
}

/// Keeps the newest whole turns that fit [`ContextConfig`]'s budget and
/// replaces older history with one bounded summary note.
///
/// A turn group is a user message and everything after it up to the next
/// user message, so an assistant tool call is never separated from its
/// result.
pub struct BudgetContextPolicy {
    config: ContextConfig,
}

impl BudgetContextPolicy {
    /// A policy for `config`, which must leave room for history.
    pub fn new(config: ContextConfig) -> Result<Self, ContextError> {
        if config.bytes_per_token == 0 {
            return Err(ContextError(
                "context.bytes_per_token must be positive".into(),
            ));
        }
        if config
            .reserve_output_tokens
            .saturating_add(config.safety_margin_tokens)
            >= config.max_tokens
        {
            return Err(ContextError(
                "context reserve and safety margin consume the model window".into(),
            ));
        }
        Ok(Self { config })
    }

    fn string_tokens(&self, value: &str) -> usize {
        value.len().div_ceil(self.config.bytes_per_token)
    }

    /// Estimated tokens of `messages`.
    fn cost(&self, messages: &[Message]) -> usize {
        messages
            .iter()
            .map(|message| message.estimated_tokens(self.config.bytes_per_token))
            .sum()
    }

    /// Split `history` into turn groups: each starts at a user message (or at
    /// the first message) and runs to the next user message.
    fn group_messages(history: &[Message]) -> Vec<&[Message]> {
        let mut groups = Vec::new();
        let mut start = 0;
        for (index, message) in history.iter().enumerate() {
            if index > start && matches!(message, Message::User { .. }) {
                groups.push(&history[start..index]);
                start = index;
            }
        }
        if start < history.len() {
            groups.push(&history[start..]);
        }
        groups
    }

    fn summarize(&self, messages: &[Message]) -> String {
        let mut output = format!(
            "[SCV compacted {} earlier messages. Bounded extracts follow.]\n",
            messages.len()
        );
        for message in messages {
            let (label, content) = match message {
                Message::User { content, .. } => ("user", content.as_str()),
                Message::Assistant { content, .. } => ("assistant", content.as_str()),
                Message::Tool {
                    name,
                    content,
                    is_error,
                    ..
                } => {
                    let status = if *is_error { "failed" } else { "ok" };
                    output.push_str(&format!("tool {name} ({status}): "));
                    ("", content.as_str())
                }
                Message::HistoryNote { content } => ("earlier", content.as_str()),
            };
            if !label.is_empty() {
                output.push_str(label);
                output.push_str(": ");
            }
            let tail = char_tail(content, 240);
            output.push_str(&tail.replace('\n', " "));
            output.push('\n');
            if output.chars().count() >= self.config.summary_max_chars {
                break;
            }
        }
        truncate_chars(&output, self.config.summary_max_chars)
    }
}

impl ContextPolicy for BudgetContextPolicy {
    /// The budget for history is the window minus the system prompt, the tool
    /// schemas, the output reserve, and the safety margin. Selection then:
    ///
    /// 1. keeps the newest turn group, failing if it alone is over budget;
    /// 2. walks older groups from newest to oldest, keeping each while it
    ///    fits, and stops at the first that does not (so the kept history is
    ///    always one contiguous suffix);
    /// 3. if anything was left out, puts one summary note of the left-out
    ///    prefix in front. When the note does not fit beside the kept groups,
    ///    the oldest kept group joins the summarized prefix and the note is
    ///    rebuilt; with only the newest group left, the note is cut to the
    ///    room that remains.
    fn select(
        &self,
        history: &[Message],
        system_prompt: &str,
        tools: &[ToolSpec],
    ) -> Result<ContextSelection, ContextError> {
        if history.is_empty() {
            return Ok(ContextSelection {
                messages: Vec::new(),
                before_tokens: 0,
                after_tokens: 0,
                removed_messages: 0,
            });
        }
        let tools_bytes = serde_json::to_vec(tools).map_or(0, |value| value.len());
        let static_tokens = self
            .string_tokens(system_prompt)
            .saturating_add(tools_bytes.div_ceil(self.config.bytes_per_token))
            .saturating_add(self.config.reserve_output_tokens)
            .saturating_add(self.config.safety_margin_tokens);
        if static_tokens >= self.config.max_tokens {
            return Err(ContextError(
                "system prompt and tool schemas exceed context budget".into(),
            ));
        }
        let budget = self.config.max_tokens - static_tokens;
        let before_tokens = static_tokens.saturating_add(self.cost(history));

        let groups = Self::group_messages(history);
        let (newest, older) = groups
            .split_last()
            .expect("a non-empty history has at least one group");
        let mut selected_cost = self.cost(newest);
        if selected_cost > budget {
            return Err(ContextError("newest turn exceeds context budget".into()));
        }
        let mut selected: VecDeque<&[Message]> = VecDeque::from([*newest]);
        for group in older.iter().rev() {
            let cost = self.cost(group);
            if selected_cost.saturating_add(cost) > budget {
                break;
            }
            selected.push_front(group);
            selected_cost += cost;
        }

        let kept_messages: usize = selected.iter().map(|group| group.len()).sum();
        let mut removed_messages = history.len() - kept_messages;
        let selection = |note: Option<Message>,
                         selected: VecDeque<&[Message]>,
                         cost: usize,
                         removed_messages: usize| ContextSelection {
            messages: note
                .into_iter()
                .chain(selected.into_iter().flatten().cloned())
                .collect(),
            before_tokens,
            after_tokens: static_tokens.saturating_add(cost),
            removed_messages,
        };
        if removed_messages == 0 {
            return Ok(selection(None, selected, selected_cost, 0));
        }
        loop {
            let summary = self.summarize(&history[..removed_messages]);
            let note = Message::HistoryNote {
                content: summary.clone(),
            };
            let note_cost = note.estimated_tokens(self.config.bytes_per_token);
            if selected_cost.saturating_add(note_cost) <= budget {
                return Ok(selection(
                    Some(note),
                    selected,
                    selected_cost + note_cost,
                    removed_messages,
                ));
            }
            if selected.len() == 1 {
                let available_tokens = budget.saturating_sub(selected_cost);
                let note =
                    fit_history_note(&summary, available_tokens, self.config.bytes_per_token)
                        .ok_or_else(|| {
                            ContextError("compaction note cannot fit context budget".into())
                        })?;
                let note_cost = note.estimated_tokens(self.config.bytes_per_token);
                return Ok(selection(
                    Some(note),
                    selected,
                    selected_cost + note_cost,
                    removed_messages,
                ));
            }
            let dropped = selected
                .pop_front()
                .expect("more than one group is selected");
            selected_cost = selected_cost.saturating_sub(self.cost(dropped));
            removed_messages += dropped.len();
        }
    }
}

/// The longest prefix of `content`, as a history note, that fits
/// `available_tokens` (binary search over its length in characters).
fn fit_history_note(
    content: &str,
    available_tokens: usize,
    bytes_per_token: usize,
) -> Option<Message> {
    let chars: Vec<char> = content.chars().collect();
    let mut low = 0usize;
    let mut high = chars.len();
    let mut best = None;
    while low <= high {
        let middle = low + (high - low) / 2;
        let candidate = Message::HistoryNote {
            content: chars[..middle].iter().collect(),
        };
        if candidate.estimated_tokens(bytes_per_token) <= available_tokens {
            best = Some(candidate);
            low = middle.saturating_add(1);
        } else if middle == 0 {
            break;
        } else {
            high = middle - 1;
        }
    }
    best
}

#[cfg(test)]
mod tests;
