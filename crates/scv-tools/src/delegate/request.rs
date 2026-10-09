//! The arguments an `agent` call takes, and their validation.

use std::path::{Path, PathBuf};

use scv_core::ToolError;
use serde::Deserialize;

use crate::BusyBehavior;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentArgs {
    /// Which agent runs the call. The `agent` tool resolves it before a
    /// backend sees the call, so backends ignore it.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) agent: Option<String>,
    pub(crate) prompt: String,
    pub(crate) timeout_seconds: Option<u64>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) session: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) cwd: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) model: Option<String>,
    #[serde(default, deserialize_with = "blank_as_none")]
    pub(crate) effort: Option<String>,
    #[serde(default, deserialize_with = "blank_busy_as_none")]
    pub(crate) on_busy: Option<BusyBehavior>,
}

/// Models often send an optional string they mean to leave unset as `""`, so
/// a blank value selects the default rather than failing the call.
pub(crate) fn blank_as_none<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value.filter(|value| !value.trim().is_empty()))
}

/// `on_busy`, where blank also selects the default.
fn blank_busy_as_none<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<BusyBehavior>, D::Error> {
    blank_as_none(deserializer)?
        .map(|value| BusyBehavior::parse(&value).map_err(serde::de::Error::custom))
        .transpose()
}

/// Longest `cwd` argument accepted, in bytes.
pub(crate) const MAX_AGENT_CWD_BYTES: usize = 4096;

pub(crate) fn validate_agent_cwd(cwd: &str) -> Result<(), ToolError> {
    if cwd.trim().is_empty() || cwd.len() > MAX_AGENT_CWD_BYTES || cwd.contains('\0') {
        return Err(ToolError::invalid_arguments(format!(
            "cwd must be a non-empty directory path of at most {MAX_AGENT_CWD_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Resolve a requested agent directory against the workspace. Resolution
/// follows symlinks, so a link pointing outside the workspace is refused
/// rather than trusted by name.
pub(crate) fn resolve_agent_cwd(workspace: &Path, cwd: Option<&str>) -> Result<PathBuf, ToolError> {
    let root = std::fs::canonicalize(workspace)
        .map_err(|error| ToolError::failed(format!("resolve workspace: {error}")))?;
    let Some(cwd) = cwd else {
        return Ok(root);
    };
    validate_agent_cwd(cwd)?;
    let resolved = std::fs::canonicalize(root.join(cwd))
        .map_err(|error| ToolError::invalid_arguments(format!("cwd {cwd:?}: {error}")))?;
    if !resolved.starts_with(&root) {
        return Err(ToolError::invalid_arguments(format!(
            "cwd {cwd:?} is outside the workspace"
        )));
    }
    if !resolved.is_dir() {
        return Err(ToolError::invalid_arguments(format!(
            "cwd {cwd:?} is not a directory"
        )));
    }
    Ok(resolved)
}

/// Effort values the `agent` tool's schema always lists; an ACP agent's own
/// values are added to them.
pub const AGENT_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Longest effort value accepted, in bytes.
const MAX_EFFORT_BYTES: usize = 32;

/// Model names are passed as one argument, so only reject values that could
/// read as a flag, name an `@file` argument, or carry unexpected characters.
pub fn valid_model_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.starts_with(['-', '@'])
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/@[]-".contains(c))
}

/// Whether `value` can be passed as an effort: 1-32 ASCII letters, digits,
/// `-`, or `_`, starting with a letter or digit so it never reads as a flag.
/// Which values an agent supports is the agent's to check, since its levels
/// change with its releases.
pub fn valid_effort(value: &str) -> bool {
    value.len() <= MAX_EFFORT_BYTES
        && value
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}
