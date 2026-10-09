//! A reviewed job's journal: one append-only JSONL file per review in
//! `$SCV_HOME/state/reviews`, named by the review's ID, synced after every
//! event. The job running the review is its only writer; people read it with
//! `cat`, `less`, or `jq`. A journal without `review.finished` was cut off by
//! a crash or restart. Files older than 30 days are removed when a review
//! starts.

use std::{
    fs::{File, OpenOptions},
    io::Write as _,
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value};

/// How long a journal is kept.
pub(crate) const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// The journal format's version, in every event.
const VERSION: u64 = 1;

/// One review's journal, open for appending.
#[derive(Debug)]
pub(crate) struct Journal {
    id: String,
    path: PathBuf,
    file: File,
    seq: u64,
    /// `review.finished` was written.
    finished: bool,
    /// For tests: appends fail from this event on.
    #[cfg(test)]
    pub(crate) fail_from: Option<u64>,
}

impl Journal {
    /// Create a new journal in `dir`, first removing journals there older
    /// than [`RETENTION`]. The directory is made private (`0700`) and the
    /// file is created private (`0600`) and new, never through a symlink.
    pub(crate) fn create(dir: &Path) -> std::io::Result<Self> {
        if std::fs::symlink_metadata(dir).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(std::io::Error::other(format!(
                "{} is a symlink",
                dir.display()
            )));
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        prune(dir, RETENTION, SystemTime::now());
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let id = format!(
            "rev-{seconds}-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..6]
        );
        let path = dir.join(format!("{id}.jsonl"));
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        Ok(Self {
            id,
            path,
            file,
            seq: 0,
            finished: false,
            #[cfg(test)]
            fail_from: None,
        })
    }

    /// The review's ID, such as `rev-1759961234-3fa9c1`.
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// Whether any event was written.
    pub(crate) fn is_started(&self) -> bool {
        self.seq > 0
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.finished
    }

    /// Append one event with `data`'s fields and sync it to disk.
    pub(crate) fn append(&mut self, event: &str, data: Value) -> std::io::Result<()> {
        #[cfg(test)]
        if self.fail_from.is_some_and(|from| self.seq + 1 >= from) {
            return Err(std::io::Error::other("injected journal failure"));
        }
        let mut line = Map::new();
        line.insert("v".into(), VERSION.into());
        line.insert("seq".into(), (self.seq + 1).into());
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        line.insert(
            "ts".into(),
            u64::try_from(millis).unwrap_or(u64::MAX).into(),
        );
        line.insert("event".into(), event.into());
        if let Value::Object(fields) = data {
            for (key, value) in fields {
                line.entry(key).or_insert(value);
            }
        }
        let mut bytes = serde_json::to_vec(&Value::Object(line)).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        self.file.write_all(&bytes)?;
        self.file.sync_data()?;
        self.seq += 1;
        if event == "review.finished" {
            self.finished = true;
        }
        Ok(())
    }

    /// Remove the journal if nothing was written to it, as for a call that
    /// failed before its job started.
    pub(crate) fn remove_empty(&self) {
        if !self.is_started() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Remove the journals in `dir` last modified at least `older_than` before
/// `now`. Only regular `rev-*.jsonl` files go; symlinks are never followed.
pub(crate) fn prune(dir: &Path, older_than: Duration, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let Some(cutoff) = now.checked_sub(older_than) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !(name.starts_with("rev-") && name.ends_with(".jsonl")) {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if metadata.is_file() && metadata.modified().is_ok_and(|modified| modified <= cutoff) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests;
