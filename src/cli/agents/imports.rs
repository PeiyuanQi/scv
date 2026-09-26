//! What `scv agents import` copied into an agent home, and whether its source
//! has changed since.
//!
//! Delegated agents run from private copies so they never read or alter the
//! user's own setup. A copy made by `scv agents import` would otherwise go
//! stale silently, so each import records a digest of what it copied in
//! `state/imports/<agent>.json`, and `scv agents status` and `scv config show`
//! compare it with the source as it is now.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use scv_client::Layout;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Where an import copied from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Source {
    /// Files from a directory, such as `config.toml` from `~/.grok`.
    Files { dir: PathBuf, files: Vec<String> },
    /// SCV's own active provider.
    ScvProvider,
}

#[derive(Serialize, Deserialize)]
struct Record {
    source: Source,
    /// SHA-256 of what was copied; it names no secret.
    digest: String,
    imported_unix_seconds: u64,
}

/// An import compared with its source now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportStatus {
    pub(crate) source: Source,
    pub(crate) imported_unix_seconds: u64,
    pub(crate) freshness: Freshness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Freshness {
    /// The source still matches the copy.
    Current,
    /// The source changed after the import.
    Changed,
    /// The source cannot be read now, such as a key variable that is not set.
    Unknown,
}

impl ImportStatus {
    /// One display line, without secrets.
    pub(crate) fn describe(&self, agent: &str, now_unix_seconds: u64) -> String {
        let source = match &self.source {
            Source::Files { dir, files } => {
                format!("{} from {}", files.join(" and "), dir.display())
            }
            Source::ScvProvider => "SCV's provider".into(),
        };
        let age = age(now_unix_seconds.saturating_sub(self.imported_unix_seconds));
        match self.freshness {
            Freshness::Current => format!("copy of {source}, imported {age} ago, up to date"),
            Freshness::Changed => format!(
                "copy of {source}, imported {age} ago; the source has changed since, \
                 run `scv agents import {agent}` to refresh it"
            ),
            Freshness::Unknown => format!(
                "copy of {source}, imported {age} ago; the source cannot be read here to compare"
            ),
        }
    }
}

fn age(seconds: u64) -> String {
    match seconds {
        0..120 => format!("{seconds}s"),
        120..7200 => format!("{}m", seconds / 60),
        7200..172_800 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// Digest of `files` in `dir`, in order; a missing file counts as absent.
pub(crate) fn digest_files(dir: &Path, files: &[String]) -> Result<String> {
    let mut hasher = Sha256::new();
    for file in files {
        hasher.update(file.as_bytes());
        match std::fs::read(dir.join(file)) {
            Ok(bytes) => {
                hasher.update([1]);
                hasher.update((bytes.len() as u64).to_le_bytes());
                hasher.update(&bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => hasher.update([0]),
            Err(error) => {
                return Err(anyhow!(error).context(format!("read {}", dir.join(file).display())));
            }
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Digest of a value SCV copied, such as its own provider settings and key.
pub(crate) fn digest_value(value: &impl Serialize) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).context("encode import digest")?)
    ))
}

fn path(layout: &Layout, agent: &str) -> PathBuf {
    layout.imports().join(format!("{agent}.json"))
}

/// Record that `agent`'s copy now matches `source` with `digest`.
pub(crate) fn record(layout: &Layout, agent: &str, source: Source, digest: String) -> Result<()> {
    let record = Record {
        source,
        digest,
        imported_unix_seconds: now(),
    };
    private_directory(layout.home(), &layout.imports())?;
    scv_client::fs::replace_private(
        &path(layout, agent),
        serde_json::to_string(&record)?.as_bytes(),
    )
    .map_err(|error| anyhow!("atomic state replace failed: {error}"))
}

/// Create `directory` and make it and its parents up to `home` private
/// (mode 0700), so the records never sit in a directory others can list.
fn private_directory(home: &Path, directory: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(directory)?;
    for path in directory.ancestors() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        if path == home || !path.starts_with(home) {
            break;
        }
    }
    Ok(())
}

/// Compare `agent`'s last import with its source now. `current` digests a
/// [`Source::ScvProvider`] import, returning `None` when it cannot be read.
pub(crate) fn check(
    layout: &Layout,
    agent: &str,
    current: impl FnOnce() -> Option<String>,
) -> Result<Option<ImportStatus>> {
    let text = match std::fs::read_to_string(path(layout, agent)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let record: Record = serde_json::from_str(&text)
        .with_context(|| format!("parse {}", path(layout, agent).display()))?;
    let now = match &record.source {
        Source::Files { dir, files } => digest_files(dir, files).ok(),
        Source::ScvProvider => current(),
    };
    let freshness = match now {
        Some(digest) if digest == record.digest => Freshness::Current,
        Some(_) => Freshness::Changed,
        None => Freshness::Unknown,
    };
    Ok(Some(ImportStatus {
        source: record.source,
        imported_unix_seconds: record.imported_unix_seconds,
        freshness,
    }))
}

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests;
