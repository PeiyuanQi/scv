//! What both binaries share: the `--approval-policy` values and the instance
//! selection that must happen before the async runtime starts.
//!
//! `src/bin/scv-server.rs` includes this file with `#[path]`, so it depends on
//! nothing else in `cli`.

use anyhow::{Context, Result};
use clap::ValueEnum;
use scv_server::config::ApprovalPolicy;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum ApprovalArg {
    OnRisk,
    Always,
    Never,
}

impl From<ApprovalArg> for ApprovalPolicy {
    fn from(value: ApprovalArg) -> Self {
        match value {
            ApprovalArg::OnRisk => Self::OnRisk,
            ApprovalArg::Always => Self::Always,
            ApprovalArg::Never => Self::Never,
        }
    }
}

/// Select this process's SCV instance: export `SCV_HOME` (created and made
/// absolute) and `SCV_CONFIG` (made absolute) for the libraries and every
/// child process to read.
///
/// # Safety
///
/// Call only while the process has a single thread: before the tokio runtime
/// is built and before any child process or thread is started. Setting an
/// environment variable races with any concurrent read of the environment.
pub(crate) unsafe fn apply_process_config(
    home: Option<&Path>,
    config: Option<&Path>,
    cwd: &Path,
) -> Result<()> {
    if let Some(home) = home {
        let home = absolute_path(home, cwd);
        std::fs::create_dir_all(&home).context("create SCV instance home")?;
        let home = std::fs::canonicalize(home).context("resolve SCV instance home")?;
        // SAFETY: the caller guarantees the process is still single-threaded.
        unsafe { std::env::set_var("SCV_HOME", home) };
    }
    if let Some(config) = config {
        let config = absolute_path(config, cwd);
        // SAFETY: the caller guarantees the process is still single-threaded.
        unsafe { std::env::set_var("SCV_CONFIG", config) };
    }
    Ok(())
}

pub(crate) fn absolute_path(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}
