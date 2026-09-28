//! Keeping the account's local files bounded.
//!
//! The janitor runs only once the run has taken over its state, and changes
//! state only through the ledger, so it never races another writer. It
//! prunes the ledger's rings, empties the triage sessions' working
//! directory, removes temporary files a crash left behind (only its own
//! account's, in the directory the accounts share), and watches the free
//! space of the disk holding the state: below the floor, or when the free
//! space cannot be told, new mail is counted instead of reported, so
//! nothing more is written about it. Its file system work runs off the
//! async threads.

use anyhow::{Context as _, Result};
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use super::Clock;
use super::ledger::Ledger;
use super::plan::Class;
use super::settings::MailSettings;

/// A temporary file older than this was left by a crash.
const STALE_TEMPORARY: Duration = Duration::from_secs(3600);

/// Whether the disk holding the account's state is below its floor.
#[derive(Debug, Default)]
pub(crate) struct LowSpace(AtomicBool);

impl LowSpace {
    pub(crate) fn is_low(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub(crate) fn set(&self, low: bool) {
        self.0.store(low, Ordering::Relaxed);
    }
}

pub(crate) struct Janitor<'a> {
    pub(crate) ledger: &'a Ledger,
    pub(crate) settings: &'a MailSettings,
    pub(crate) clock: &'a dyn Clock,
    pub(crate) low_space: &'a LowSpace,
    /// `state/mail/<account>`, holding `empty/`.
    pub(crate) private: &'a Path,
    /// `state/channels/email`, holding every account's state file.
    pub(crate) state_dir: &'a Path,
    pub(crate) account: &'a str,
    /// Bytes free for the user on the disk holding a path; `None` when it
    /// cannot be told. [`free_bytes`] outside tests.
    pub(crate) free_space: fn(&Path) -> Option<u64>,
}

impl Janitor<'_> {
    /// Sweep now and then every `sweep_minutes` for the life of the run.
    pub(crate) async fn run(&self) -> Result<()> {
        let every = Duration::from_secs(self.settings.retention.sweep_minutes * 60);
        loop {
            self.sweep().await?;
            tokio::time::sleep(every).await;
        }
    }

    pub(crate) async fn sweep(&self) -> Result<()> {
        let now = self.clock.now();
        self.ledger.prune(now).await?;
        let private = self.private.to_owned();
        let state_dir = self.state_dir.to_owned();
        let own = own_temporaries(self.state_dir, self.account);
        let free_space = self.free_space;
        let (emptied, stale, free) = tokio::task::spawn_blocking(move || {
            let emptied = empty_directory(&private.join("empty"));
            let at = SystemTime::now();
            let stale = remove_stale_temporaries(&private, None, at)
                + remove_stale_temporaries(&state_dir, Some(&own), at);
            (emptied, stale, free_space(&private))
        })
        .await
        .context("the mail janitor's sweep stopped")?;
        if emptied > 0 {
            tracing::warn!(
                emptied,
                "removed files from the mail sessions' empty directory"
            );
        }
        if stale > 0 {
            tracing::info!(
                stale,
                "removed temporary files left by an interrupted write"
            );
        }
        let floor = self.settings.retention.min_free_mib * 1024 * 1024;
        // Space that cannot be told counts as too little.
        let low = free.is_none_or(|free| free < floor);
        if low && !self.low_space.is_low() {
            let text = if free.is_some() {
                tracing::warn!("the disk holding mail state is below its floor; counting new mail");
                "The disk holding SCV's mail state is nearly full, so new mail is counted but \
                 not reported until space is freed."
            } else {
                tracing::warn!(
                    "could not tell the free space of the disk holding mail state; counting new mail"
                );
                "SCV cannot tell how much space is free on the disk holding its mail state, so \
                 new mail is counted but not reported until it can."
            };
            let today = self.clock.today(now);
            self.ledger
                .note(
                    &format!("system:disk:{today}"),
                    Class::System,
                    text.to_owned(),
                    now,
                )
                .await?;
        }
        self.low_space.set(low);
        Ok(())
    }
}

/// How the temporary files of `account`'s state file in `state_dir` start.
fn own_temporaries(state_dir: &Path, account: &str) -> OsString {
    scv_client::fs::temporary_prefix(&state_dir.join(format!("{account}.json")))
}

/// Whether `name` is a temporary file's: one starting with `owner`, when
/// the directory is shared, else any `.<name>.<random>.tmp` or older
/// `.tmp<random>`.
fn temporary(name: &[u8], owner: Option<&[u8]>) -> bool {
    match owner {
        Some(owner) => name.starts_with(owner) && name.ends_with(b".tmp"),
        None => name.starts_with(b".tmp") || (name.starts_with(b".") && name.ends_with(b".tmp")),
    }
}

/// Remove everything inside `directory`, following no link, and say how
/// many entries there were.
pub(crate) fn empty_directory(directory: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let result = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(&path),
            Ok(_) => std::fs::remove_file(&path),
            Err(error) => Err(error),
        };
        if result.is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Remove temporary files ([`temporary`]) directly in `directory`, only
/// those starting with `owner` when it is given, that are regular files
/// older than an hour, as atomic writes leave them after a crash. Links and
/// subdirectories are left alone.
pub(crate) fn remove_stale_temporaries(
    directory: &Path,
    owner: Option<&OsString>,
    now: SystemTime,
) -> usize {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return 0;
    };
    let owner = owner.map(|owner| owner.as_bytes());
    let mut removed = 0;
    for entry in entries.flatten() {
        if !temporary(entry.file_name().as_bytes(), owner) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let old = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > STALE_TEMPORARY);
        if metadata.is_file() && old && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Bytes an unprivileged user can still write on `path`'s file system;
/// `None` when that cannot be told.
pub(crate) fn free_bytes(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|ancestor| ancestor.exists())?;
    let name = std::ffi::CString::new(existing.as_os_str().as_bytes()).ok()?;
    // SAFETY: an all-zero `statvfs` is a valid value of the plain C struct.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is a NUL-terminated path and `stat` a valid struct to
    // fill; neither pointer is kept after the call.
    if unsafe { libc::statvfs(name.as_ptr(), &raw mut stat) } != 0 {
        return None;
    }
    #[allow(
        clippy::useless_conversion,
        reason = "the fields are narrower than u64 on some platforms"
    )]
    Some(u64::from(stat.f_bavail).saturating_mul(u64::from(stat.f_frsize)))
}

#[cfg(test)]
mod tests;
