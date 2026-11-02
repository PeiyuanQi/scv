//! The permission mode a delegated agent runs in: what SCV's policy expects
//! of it, how a Codex run's rollout records it, and what an approval summary
//! and a result say about it.
//!
//! Under policy mode `auto` an agent must report its full mode; under any
//! other mode a report of the full mode is a mismatch too, which catches an
//! agent home that turned its own prompts off. A mismatch, or a missing
//! report from an agent that should send one, stops the run. An agent that
//! reports nothing SCV reads still runs, and says `mode unverified`.

use std::{
    io::{BufRead as _, Seek as _},
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::args::bounded;

/// Claude Code's full mode, as its init event's `permissionMode` names it.
pub(crate) const CLAUDE_FULL_MODE: &str = "bypassPermissions";
/// How Codex reports its full mode in a rollout's `turn_context`.
pub(crate) const CODEX_FULL_MODE: &str = "approval_policy never, sandbox danger-full-access";

/// The longest rollout line read: a `turn_context` is short, and longer
/// lines (such as a tool result) are skipped.
const MAX_ROLLOUT_LINE_BYTES: usize = 1024 * 1024;

/// What the policy expects of the mode one agent reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Expected {
    /// The agent, such as `claude`, for the error.
    pub(crate) agent: String,
    /// The policy mode in effect for it, such as `auto`.
    pub(crate) policy: String,
    /// Its full mode as it reports it, such as `bypassPermissions`.
    pub(crate) full_mode: String,
    /// The policy starts it in its full mode (`auto`).
    pub(crate) full: bool,
}

impl Expected {
    /// Whether `reported` is a mode the policy accepts.
    pub(crate) fn accepts(&self, reported: &str) -> bool {
        (reported == self.full_mode) == self.full
    }

    fn expects(&self) -> String {
        if self.full {
            self.full_mode.clone()
        } else {
            format!("a mode other than {}", self.full_mode)
        }
    }

    /// The error for a run that reported `reported`, which
    /// [`accepts`](Self::accepts) refused.
    pub(crate) fn mismatch(&self, reported: &str) -> String {
        format!(
            "{} reported permission mode {}, but policy mode {} expects {}; the run was stopped",
            self.agent,
            bounded(reported, 80),
            self.policy,
            self.expects()
        )
    }

    /// The error for a run that should have reported its mode and did not.
    pub(crate) fn missing(&self) -> String {
        format!(
            "{} did not report its permission mode, and policy mode {} expects {}; the run was \
             stopped",
            self.agent,
            self.policy,
            self.expects()
        )
    }
}

/// When SCV verifies an agent's mode, for its approval summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Check {
    /// From its first event, before any tool runs (Claude Code's CLI).
    BeforeTools,
    /// From its rollout, shortly after its turn starts (Codex's CLI).
    AfterStart,
    /// From its ACP session, before the prompt is sent.
    BeforePrompt,
    /// It has a full-autonomy switch, but reports nothing SCV reads.
    Unverified,
    /// It has no full-autonomy switch to verify.
    NotApplicable,
}

impl Check {
    fn clause(self) -> &'static str {
        match self {
            Self::BeforeTools => "; SCV verifies the mode it reports before any tool runs",
            Self::AfterStart => "; SCV verifies the mode it reports shortly after its turn starts",
            Self::BeforePrompt => "; SCV verifies the mode it reports before sending the prompt",
            Self::Unverified => "; mode unverified: it reports none SCV can read",
            Self::NotApplicable => "",
        }
    }
}

/// The approval summary's sentence on the agent's permissions: the policy
/// mode, whether SCV verifies the mode, and what the agent may do. `relays`
/// says its own permission requests come back to SCV's gate (over ACP).
pub(crate) fn summary(policy: &str, full: bool, check: Check, relays: bool) -> String {
    let full_clause = check.clause();
    if full {
        format!(
            " FULL PERMISSIONS (policy mode {policy}{full_clause}): the agent's own approval \
             prompts and sandbox are off, so it edits files, runs commands, and uses the \
             network without asking."
        )
    } else if relays {
        format!(
            " Policy mode {policy}{full_clause}. Its own permission requests come back here for \
             approval."
        )
    } else if check == Check::NotApplicable {
        format!(
            " Policy mode {policy}. It has no full-autonomy switch, so its own configuration \
             decides what it asks."
        )
    } else {
        format!(
            " Policy mode {policy}{full_clause}. It starts without its full-autonomy switches, \
             so its own configuration decides what it asks."
        )
    }
}

/// What a result says about the mode of a run that reported none SCV
/// verified: `None` when the agent has no switch to verify.
pub(crate) fn unverified(check: Check, reported: Option<&str>) -> Option<String> {
    match (check, reported) {
        (Check::NotApplicable, _) => None,
        (_, Some(reported)) => Some(format!("unverified (it reports {})", bounded(reported, 80))),
        (_, None) => Some("unverified".into()),
    }
}

/// What a result says about a mode SCV verified.
pub(crate) fn verified(reported: &str) -> String {
    format!("verified {}", bounded(reported, 80))
}

/// The rollout of Codex thread `thread` in the agent home `home`:
/// `sessions/<year>/<month>/<day>/rollout-<time>-<thread>.jsonl`.
pub(crate) fn codex_rollout(home: &Path, thread: &str) -> Option<PathBuf> {
    fn find(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
        let entries = std::fs::read_dir(dir).ok()?;
        let mut dirs = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_file()
                && path
                    .file_name()
                    .and_then(|file| file.to_str())
                    .is_some_and(|file| file.ends_with(name))
            {
                return Some(path);
            }
            if metadata.is_dir() && depth > 0 {
                dirs.push(path);
            }
        }
        // The newest day first: a thread's file is under the day it started.
        dirs.sort();
        dirs.into_iter()
            .rev()
            .find_map(|dir| find(&dir, name, depth - 1))
    }
    find(&home.join("sessions"), &format!("{thread}.jsonl"), 3)
}

/// The mode the last `turn_context` written at or after byte `offset` of
/// the rollout `file` records, as `approval_policy <policy>, sandbox <type>`;
/// `None` until there is one.
pub(crate) fn codex_turn_mode(file: &Path, offset: u64) -> Option<String> {
    let mut file = std::fs::File::open(file).ok()?;
    file.seek(std::io::SeekFrom::Start(offset)).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut mode = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // A line still being written ends without a newline.
        if line.len() > MAX_ROLLOUT_LINE_BYTES || line.last() != Some(&b'\n') {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) != Some("turn_context") {
            continue;
        }
        let context = event.get("payload").unwrap_or(&event);
        let approval = context.get("approval_policy").and_then(Value::as_str);
        let sandbox = context
            .pointer("/sandbox_policy/type")
            .or_else(|| context.get("sandbox_policy"))
            .and_then(Value::as_str);
        if let (Some(approval), Some(sandbox)) = (approval, sandbox) {
            mode = Some(format!("approval_policy {approval}, sandbox {sandbox}"));
        }
    }
    mode
}

#[cfg(test)]
mod tests;
