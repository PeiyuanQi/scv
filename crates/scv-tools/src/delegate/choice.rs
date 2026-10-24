//! What lets the model choose between delegated agents: the `agent` tool
//! lists each agent with its product, what that harness offers, what it
//! takes, the user's own `use_for` note, the defaults a call that leaves out
//! `model` or `effort` runs with, and the effort for a hard task, and, when a
//! call fails because the agent is missing, signed out, or its provider
//! returned an error, names the other agents to call instead.
//!
//! That fallback is decided from the structured failure alone: the result's
//! `status` and the `error` the agent reported, read against a list of
//! phrases, or SCV's own error when SCV classed it as
//! [`ToolFailure::Unavailable`]; never from the agent's reply or the wording
//! of SCV's other errors. A `declined` result, where the agent's model
//! refused the request, never gets one. When `grok` is among the other
//! agents, the result's `note` tells the calling model to call it; otherwise
//! the calling model tells the user, and only the user may then name another
//! agent.
//!
//! Only a reviewed job also reads a reviewer's reply, for one thing: whether
//! its whole substance is a provider error ([`provider_error_only`]), which
//! makes that reviewer unavailable.

use scv_core::{ToolError, ToolFailure, ToolOutput};
use scv_protocol::JobStatus;
use serde_json::Value;

use crate::{
    AgentDefaults,
    delegate::{
        adapters,
        agent::{Accepts, Offered},
        options::{AgentOptions, Choice},
        output::{self, AgentReply},
    },
};

/// Lowercase fragments of an error an agent reported that another agent
/// could avoid: the agent is missing or died, is signed out, or its provider
/// returned an error.
const UNAVAILABLE: &[&str] = &[
    "not found on path",
    "no such file or directory",
    "the agent exited",
    "acp server exited",
    "not logged in",
    "not signed in",
    "not authenticated",
    "unauthorized",
    "authentication",
    "missing_credential",
    "no api key",
    "auth_required",
    "forbidden",
    "401",
    "403",
    "404",
    "429",
    "502",
    "503",
    "529",
    "rate limit",
    "rate_limit",
    "quota",
    "overloaded",
    "service unavailable",
    "model not found",
    "no such model",
    "does not exist",
    "insufficient",
    "billing",
    "connection refused",
    "connection reset",
];

/// Whether an error an agent reported reads like it was unavailable.
pub(crate) fn reports_unavailable(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    UNAVAILABLE.iter().any(|needle| lower.contains(needle))
}

/// Longest reply that can be nothing but a provider error: an agent's
/// opening line about what it will do, then the error its harness printed.
const PROVIDER_ERROR_REPLY_BYTES: usize = 2048;
/// Longest provider error at the end of such a reply.
const PROVIDER_ERROR_BYTES: usize = 1000;

/// Lowercase beginnings of the errors agent harnesses print in place of a
/// reply when their provider has no capacity or the connection to it fails,
/// as Codex's ACP server does (`Selected model is at capacity…`, `exceeded
/// retry limit, last status: 429…`, `unexpected status 503 Service
/// Unavailable: …NO_AVAILABLE_CHANNEL…`) and Claude Code does (`API Error:
/// 529 …`). `unexpected status ` and `api error: ` count only before a
/// status code.
const PROVIDER_ERRORS: &[&str] = &[
    "selected model is at capacity",
    "model is at capacity",
    "exceeded retry limit",
    "retry limit exceeded",
    "stream disconnected before completion",
    "stream error:",
    "error sending request for url",
    "no_available_channel",
    "api error: request timed out",
    "api error: connection error",
];

/// What, followed by a status code, begins a provider error.
const STATUS_PREFIXES: &[&str] = &["unexpected status ", "api error: "];

/// HTTP reasons that, right after a 429 or 5xx status, make a status line.
const STATUS_REASONS: &[&str] = &[
    "too many requests",
    "service unavailable",
    "internal server error",
    "bad gateway",
    "gateway timeout",
    "no_available_channel",
    "overloaded",
    "{",
];

/// Whether a reply's whole substance is a provider's capacity, rate, or
/// transport error, as when an agent's harness turns a failed request into
/// a completed turn whose text is the error. The reply must end with the
/// error, on one line that starts the reply or a sentence; anything before
/// it is at most a short opening, with no fenced block. A reply that only
/// mentions such an error, or reports on work, does not count.
pub(crate) fn provider_error_only(reply: &str) -> bool {
    let reply = reply.trim();
    if reply.is_empty() || reply.len() > PROVIDER_ERROR_REPLY_BYTES || reply.contains("```") {
        return false;
    }
    let lower = reply.to_ascii_lowercase();
    let from = lower.len().saturating_sub(PROVIDER_ERROR_BYTES);
    (from..lower.len())
        .filter(|&start| lower.is_char_boundary(start) && starts_sentence(&lower[..start]))
        .any(|start| {
            let error = &lower[start..];
            !error.contains('\n') && provider_error_start(error)
        })
}

/// Whether `text` begins with a provider error.
fn provider_error_start(text: &str) -> bool {
    if PROVIDER_ERRORS.iter().any(|error| {
        text.strip_prefix(error)
            .is_some_and(|rest| error.ends_with(':') || error_tail(rest))
    }) {
        return true;
    }
    if STATUS_PREFIXES
        .iter()
        .filter_map(|prefix| text.strip_prefix(prefix))
        .any(|rest| status_code(rest).is_some())
    {
        return true;
    }
    // A bare status line, such as `503 NO_AVAILABLE_CHANNEL` or `Error: 429
    // Too Many Requests`.
    let line = ["error: ", "http ", "status: "]
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix))
        .unwrap_or(text);
    status_code(line).is_some_and(|(code, rest)| {
        (code == "429" || code.starts_with('5'))
            && STATUS_REASONS.iter().any(|reason| {
                rest.strip_prefix(reason)
                    .is_some_and(|tail| *reason == "{" || error_tail(tail))
            })
    })
}

/// Whether `rest`, what follows an error's opening words, ends the line or
/// goes on as an error message does, with punctuation or a bracket, rather
/// than as a sentence about the error (`… at capacity checks are right`).
fn error_tail(rest: &str) -> bool {
    let rest = rest.trim_start_matches(' ');
    rest.is_empty() || rest.starts_with(['.', ',', ':', ';', '!', '(', '[', '{', '-'])
}

/// The three-digit status `text` begins with, and what follows it after
/// spaces or a colon.
fn status_code(text: &str) -> Option<(&str, &str)> {
    let code = text.get(..3)?;
    let rest = &text[3..];
    (code.bytes().all(|byte| byte.is_ascii_digit())
        && !rest.starts_with(|c: char| c.is_ascii_digit()))
    .then(|| (code, rest.trim_start_matches([' ', ':'])))
}

/// Whether text after `before` starts the reply, a line, or a sentence:
/// `before` is empty or ends with a line break or `.`, `!`, `?`, or `:`,
/// spaces aside, since a harness may append its error to the agent's
/// opening line without one.
fn starts_sentence(before: &str) -> bool {
    let before = before.trim_end_matches(' ');
    before.is_empty() || before.ends_with(['.', '!', '?', ':', '\n'])
}

/// `model opus[1m] and effort xhigh`: the model and effort a call to the
/// agent runs with when it leaves them out, if the user set either.
pub fn defaults_phrase(defaults: &AgentDefaults) -> Option<String> {
    match (defaults.model.as_deref(), defaults.effort.as_deref()) {
        (Some(model), Some(effort)) => Some(format!("model {model} and effort {effort}")),
        (Some(model), None) => Some(format!("model {model}")),
        (None, Some(effort)) => Some(format!("effort {effort}")),
        (None, None) => None,
    }
}

/// A `use_for` note without its closing period, to go inside a sentence.
pub fn note_clause(use_for: &str) -> &str {
    use_for.trim().trim_end_matches('.')
}

/// The `use_for` note, the user's defaults, and the effort for a hard task,
/// as a suffix of sentences.
fn choice_note(use_for: Option<&str>, defaults: &AgentDefaults) -> String {
    let mut note = String::new();
    if let Some(use_for) = use_for {
        note.push_str(&format!(
            " The user's note on when to use it: {}.",
            note_clause(use_for)
        ));
    }
    if let Some(phrase) = defaults_phrase(defaults) {
        let (noun, them) = if defaults.model.is_some() && defaults.effort.is_some() {
            ("defaults", "them")
        } else {
            ("default", "it")
        };
        note.push_str(&format!(
            " The user's {noun}, used when a call leaves {them} out: {phrase}."
        ));
    }
    if let Some(effort) = &defaults.hard_task_effort {
        note.push_str(&format!(" For a hard task, pass effort {effort}."));
    }
    note
}

/// `model (one of: a, b; its default is a)`, from what the agent's server
/// listed; `None` when it listed nothing to choose from. The agent's own
/// default is left out when the user set one, since the user's runs instead.
fn listed(label: &str, choice: Option<&Choice>, user_default: bool) -> Option<String> {
    let choice = choice?;
    let values = choice.shown();
    if values.is_empty() {
        return None;
    }
    Some(match choice.named_default().filter(|_| !user_default) {
        Some(default) => format!(
            "{label} (one of: {}; its default is {default})",
            values.join(", ")
        ),
        None => format!("{label} (one of: {})", values.join(", ")),
    })
}

/// What an agent takes of `model` (the values its server listed, or its
/// hint), `effort`, and `session`.
fn takes(
    accepts: Accepts,
    model_hint: &str,
    offered: Option<&AgentOptions>,
    defaults: &AgentDefaults,
) -> String {
    let mut taken = Vec::new();
    if accepts.model {
        let hint = model_hint.trim().trim_end_matches('.');
        taken.push(
            listed(
                "model",
                offered.and_then(|offered| offered.model.as_ref()),
                defaults.model.is_some(),
            )
            .unwrap_or_else(|| {
                if hint.is_empty() {
                    "model".to_owned()
                } else {
                    format!("model ({hint})")
                }
            }),
        );
    }
    if accepts.effort {
        taken.push(
            listed(
                "effort",
                offered.and_then(|offered| offered.effort.as_ref()),
                defaults.effort.is_some(),
            )
            .unwrap_or_else(|| "effort".to_owned()),
        );
    }
    if accepts.session {
        taken.push("session".to_owned());
    }
    match taken.as_slice() {
        [] => "Takes no model, effort, or session.".to_owned(),
        [one] => format!("Takes {one}."),
        [first, second] => format!("Takes {first} and {second}."),
        [rest @ .., last] => format!("Takes {}, and {last}.", rest.join(", ")),
    }
}

/// One agent's line in the `agent` argument's description: its name,
/// product, and what that harness offers, then what it takes, then the
/// user's note, defaults, and hard-task effort.
pub(crate) fn entry(agent: &Offered) -> String {
    let mut line = match adapters::adapter(&agent.name) {
        Some(descriptor) => format!(
            "{} ({}): {}.",
            agent.name, descriptor.product, descriptor.offers
        ),
        None => format!("{}.", agent.name),
    };
    line.push(' ');
    line.push_str(&takes(
        agent.accepts,
        &agent.model_hint,
        agent.offered.as_ref(),
        &agent.defaults,
    ));
    line.push_str(&choice_note(agent.use_for.as_deref(), &agent.defaults));
    line
}

/// The `fallback` naming `others`, the other agents offered, if any.
fn fallback(others: &[&str]) -> Option<String> {
    (!others.is_empty()).then(|| {
        format!(
            "This agent could not run: it is missing, signed out, or its provider returned an \
             error. Other agents are available: {}. Call agent again with one of them.",
            others.join(", ")
        )
    })
}

/// Whether a failed agent result shows the agent was unavailable, judged by
/// its `status` and structured `error` only. A declined request, or a result
/// without a reported error, never qualifies.
fn unavailable_result(reply: &AgentReply) -> bool {
    reply.status == Some(JobStatus::Failed)
        && reply.error.as_deref().is_some_and(reports_unavailable)
}

/// One agent's call result as the model sees it, given `others`, the other
/// agents this session offers: an availability failure names them as a
/// `fallback`, and a declined request points to `grok` when it is among them.
pub(crate) fn settle(
    result: Result<ToolOutput, ToolError>,
    others: &[&str],
) -> Result<ToolOutput, ToolError> {
    match result {
        Ok(mut result) if result.is_error() => {
            if let Ok(value) = serde_json::from_str::<Value>(&result.content)
                && let Some(reply) = AgentReply::read(&value)
                && let Value::Object(mut content) = value
            {
                if unavailable_result(&reply) {
                    result.failure = Some(ToolFailure::Unavailable);
                    if let Some(fallback) = fallback(others) {
                        content.insert("fallback".into(), fallback.into());
                        result.content = Value::Object(content).to_string();
                    }
                } else if reply.status == Some(JobStatus::Declined) && others.contains(&"grok") {
                    content.insert("note".into(), output::DECLINED_NOTE_TRY_GROK.into());
                    result.content = Value::Object(content).to_string();
                }
            }
            Ok(result)
        }
        // SCV's own errors that it classed as unavailable, such as a
        // missing executable.
        Err(error) if error.kind == ToolFailure::Unavailable => Err(match fallback(others) {
            Some(fallback) => ToolError::unavailable(format!("{} {fallback}", error.message)),
            None => error,
        }),
        other => other,
    }
}

/// The product name of an agent, such as `Codex` for `codex`, or the name.
pub fn product(agent: &str) -> &str {
    adapters::adapter(agent).map_or(agent, |descriptor| descriptor.product)
}

#[cfg(test)]
mod tests;
