//! Keeping room on the disks that hold chat files: the chat log, chat media,
//! and kept files. When one of them has less than `history.min_free_percent`
//! free, the owner is told once and the channel bridges save no new files
//! from chat until there is room again; the chat log keeps being written.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use scv_channels::hub::Hub;
use tokio_util::sync::CancellationToken;

use crate::{config::Instance, restart::Notifier};

/// How often free space is checked; the first check runs at startup.
const CHECK_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Percentage points above the floor a disk must reach before saving files
/// resumes, so a disk hovering at the floor does not repeat the notices.
const RECOVER_MARGIN: u8 = 2;

/// Watch free space for the life of the daemon.
pub(crate) async fn monitor(
    instance: Instance,
    hub: Arc<Hub>,
    notifier: Notifier,
    cancel: CancellationToken,
) {
    let mut interval = tokio::time::interval(CHECK_INTERVAL);
    let mut low = false;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = interval.tick() => {}
        }
        let (floor, paths) = match instance.load_user() {
            Ok(config) => (
                config.history.min_free_percent,
                vec![
                    instance.layout.history(),
                    instance.layout.media(),
                    config.archive_dir(),
                ],
            ),
            Err(error) => {
                tracing::warn!("Disk space check skipped; configuration failed: {error:#}");
                continue;
            }
        };
        let tightest = lowest(&paths);
        let Some((now_low, text)) = step(low, floor, tightest.as_ref()) else {
            continue;
        };
        low = now_low;
        hub.set_low_disk(low);
        tracing::warn!("{text}");
        let notifier = notifier.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { notifier.deliver(None, &text, None, &cancel).await });
    }
}

/// What changes after a check: the new state and the notice to send, or
/// `None` when nothing does. `tightest` is the fullest disk's free share
/// and a path on it.
fn step(low: bool, floor: u8, tightest: Option<&(u8, PathBuf)>) -> Option<(bool, String)> {
    let short = tightest.filter(|(free, _)| floor > 0 && *free < floor);
    let recovered = floor == 0
        || tightest.is_none_or(|(free, _)| *free >= floor.saturating_add(RECOVER_MARGIN));
    match (low, short) {
        (false, Some((free, path))) => Some((
            true,
            format!(
                "The disk holding {} has only {free}% free, below the {floor}% SCV keeps free. \
                 Until there is more room, SCV saves no new files from chat; the chat log is \
                 still written.",
                path.display()
            ),
        )),
        (true, _) if recovered => Some((
            false,
            "There is enough free disk space again, so SCV saves files from chat again.".into(),
        )),
        _ => None,
    }
}

/// The path among `paths` whose disk has the smallest free share, with that
/// share in percent; paths that do not exist yet count by their nearest
/// existing parent.
fn lowest(paths: &[PathBuf]) -> Option<(u8, PathBuf)> {
    paths
        .iter()
        .filter_map(|path| free_percent(path).map(|free| (free, path.clone())))
        .min_by_key(|(free, _)| *free)
}

/// The share of `path`'s file system an unprivileged user can still write.
fn free_percent(path: &Path) -> Option<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    let existing = path.ancestors().find(|ancestor| ancestor.exists())?;
    let name = std::ffi::CString::new(existing.as_os_str().as_bytes()).ok()?;
    // SAFETY: an all-zero `statvfs` is a valid value of the plain C struct.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is a NUL-terminated path and `stat` a valid struct to
    // fill; neither pointer is kept after the call.
    if unsafe { libc::statvfs(name.as_ptr(), &raw mut stat) } != 0 {
        return None;
    }
    let block = u128::from(stat.f_frsize);
    let total = u128::from(stat.f_blocks) * block;
    let free = u128::from(stat.f_bavail) * block;
    (total > 0).then(|| u8::try_from(free * 100 / total).unwrap_or(100))
}

#[cfg(test)]
mod tests;
