//! The `model` and `effort` values an agent's ACP server offers, as SCV last
//! saw them.
//!
//! These values belong to the agent and change when it updates, so SCV reads
//! them from the agent instead of shipping a list. Every ACP session SCV
//! opens reports its `configOptions`, and SCV saves them per agent in
//! `state/agent-options/<name>.json`. A session built within [`MAX_AGE`] of
//! that save, with the same ACP server installed, lists the values in the
//! `agent` tool's description and refuses a model the list lacks before a
//! run starts. `scv agents check` refreshes the file on demand.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::delegate::{
    records::write_private_json,
    request::{valid_effort, valid_model_name},
};

/// How long saved values are trusted without a new ACP session to confirm
/// them. Agents also change their lists without a new install, such as when
/// an account gains a model.
pub(crate) const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

/// Config option IDs that agents use for reasoning effort.
pub(crate) const EFFORT_OPTIONS: [&str; 3] = ["effort", "reasoning_effort", "thought_level"];

/// The ACP value that selects the agent's own default. SCV's description
/// leaves it out, since omitting the argument does the same.
const DEFAULT_VALUE: &str = "default";

const FORMAT_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_VALUES: usize = 64;

/// The values of one option and the one currently selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub values: Vec<String>,
    /// The session's selection when it opened: the agent's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
}

impl Choice {
    /// The values to show the model, without `default`.
    pub fn shown(&self) -> Vec<&str> {
        self.values
            .iter()
            .map(String::as_str)
            .filter(|value| *value != DEFAULT_VALUE)
            .collect()
    }

    /// The agent's default, when it names a real value.
    pub fn named_default(&self) -> Option<&str> {
        self.current
            .as_deref()
            .filter(|value| *value != DEFAULT_VALUE)
    }

    pub fn offers(&self, value: &str) -> bool {
        self.values.iter().any(|offered| offered == value)
    }
}

/// What an agent's ACP session offers for `model` and `effort`. The effort
/// values are those of the agent's default model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Choice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Choice>,
}

impl AgentOptions {
    /// The options in an ACP `session/new` result, or `None` when it lists
    /// neither a model nor an effort choice. Values that could not be passed
    /// as an argument are left out.
    pub(crate) fn from_acp(result: &Value) -> Option<Self> {
        let options = result.get("configOptions")?.as_array()?;
        let find = |ids: &[&str], valid: fn(&str) -> bool| {
            options
                .iter()
                .find(|option| {
                    option
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| ids.contains(&id))
                })
                .and_then(|option| choice(option, valid))
        };
        let found = Self {
            model: find(&["model"], valid_model_name),
            effort: find(&EFFORT_OPTIONS, valid_effort),
        };
        (found.model.is_some() || found.effort.is_some()).then_some(found)
    }

    fn sanitized(self) -> Self {
        let clean = |choice: Option<Choice>, valid: fn(&str) -> bool| {
            choice.and_then(|choice| {
                let values: Vec<String> = choice
                    .values
                    .into_iter()
                    .filter(|value| valid(value))
                    .take(MAX_VALUES)
                    .collect();
                (!values.is_empty()).then(|| Choice {
                    current: choice.current.filter(|value| valid(value)),
                    values,
                })
            })
        };
        Self {
            model: clean(self.model, valid_model_name),
            effort: clean(self.effort, valid_effort),
        }
    }
}

fn choice(option: &Value, valid: fn(&str) -> bool) -> Option<Choice> {
    let values: Vec<String> = option
        .get("options")?
        .as_array()?
        .iter()
        .filter_map(|value| value.get("value")?.as_str())
        .filter(|value| valid(value))
        .take(MAX_VALUES)
        .map(str::to_owned)
        .collect();
    if values.is_empty() {
        return None;
    }
    let current = option
        .get("currentValue")
        .and_then(Value::as_str)
        .filter(|value| valid(value))
        .map(str::to_owned);
    Some(Choice { values, current })
}

/// Which ACP server the values came from. A different install, or the same
/// file changed, means they may be out of date.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Installed {
    path: PathBuf,
    size: u64,
    modified_unix_ms: u64,
}

impl Installed {
    fn of(executable: &Path) -> Option<Self> {
        let path = std::fs::canonicalize(executable).ok()?;
        let metadata = std::fs::metadata(&path).ok()?;
        let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        Some(Self {
            path,
            size: metadata.len(),
            modified_unix_ms: u64::try_from(modified.as_millis()).unwrap_or(u64::MAX),
        })
    }
}

/// The file SCV keeps for one agent.
#[derive(Debug, Serialize, Deserialize)]
struct Saved {
    version: u32,
    agent: String,
    executable: Installed,
    seen_unix: u64,
    #[serde(flatten)]
    options: AgentOptions,
}

/// Options an agent listed, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    pub(crate) options: AgentOptions,
    pub(crate) seen: SystemTime,
}

impl Listed {
    /// Whether they are recent enough to go by at `now`.
    pub(crate) fn fresh(&self, now: SystemTime) -> bool {
        now.duration_since(self.seen)
            .map_or(true, |age| age <= MAX_AGE)
    }
}

/// Save `options`, just reported by `agent`'s ACP server at `executable`,
/// as `file`.
pub(crate) fn save(
    file: &Path,
    agent: &str,
    executable: &Path,
    options: &AgentOptions,
) -> std::io::Result<()> {
    let (Some(dir), Some(name)) = (
        file.parent(),
        file.file_name().and_then(|name| name.to_str()),
    ) else {
        return Err(std::io::Error::other("agent options file has no directory"));
    };
    let executable = Installed::of(executable)
        .ok_or_else(|| std::io::Error::other("the ACP server's file could not be read"))?;
    write_private_json(
        dir,
        name,
        &Saved {
            version: FORMAT_VERSION,
            agent: agent.to_owned(),
            executable,
            seen_unix: unix_seconds(SystemTime::now()),
            options: options.clone(),
        },
    )
}

/// The options saved for `agent` in `file`, when they came from the ACP
/// server now installed at `executable` no longer than [`MAX_AGE`] before
/// `now`.
pub(crate) fn load(file: &Path, agent: &str, executable: &Path, now: SystemTime) -> Option<Listed> {
    let saved = read(file)?;
    let listed = Listed {
        options: saved.options,
        seen: UNIX_EPOCH + Duration::from_secs(saved.seen_unix),
    };
    (saved.agent == agent
        && listed.fresh(now)
        && Installed::of(executable).as_ref() == Some(&saved.executable))
    .then_some(listed)
}

/// The options saved in `file`, however old, and when they were seen (Unix
/// seconds): for reports such as `scv agents check`.
pub fn read_saved(file: &Path, agent: &str) -> Option<(AgentOptions, u64)> {
    read(file)
        .filter(|saved| saved.agent == agent)
        .map(|saved| (saved.options, saved.seen_unix))
}

fn read(file: &Path) -> Option<Saved> {
    let mut bytes = Vec::new();
    let opened = std::fs::File::open(file).ok()?;
    std::io::Read::read_to_end(
        &mut std::io::Read::take(opened, MAX_FILE_BYTES + 1),
        &mut bytes,
    )
    .ok()?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
        return None;
    }
    let saved: Saved = serde_json::from_slice(&bytes).ok()?;
    (saved.version == FORMAT_VERSION).then(|| Saved {
        options: saved.options.sanitized(),
        ..saved
    })
}

fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests;
