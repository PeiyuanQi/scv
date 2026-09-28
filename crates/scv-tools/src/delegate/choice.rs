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
