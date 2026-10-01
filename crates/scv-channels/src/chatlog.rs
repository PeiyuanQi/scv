//! The bridge's side of the chat log (see [`scv_client::history`]): the
//! account owner's direct chat, and each thread in it, is recorded as a
//! conversation of its own, in the host's local time, and the owner's
//! sessions name their conversation's log so the server can reload its open
//! episode.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use scv_client::history::{self, Entry, FileRef, LocalTime, Log, Role};

/// Logs are kept at most this many years.
const KEEP_YEARS: i64 = 120;
/// Conversations whose newest episode the log remembers at once. Past it,
/// it forgets all but the direct chat's, and the others find theirs again
/// on their next record.
const MAX_REMEMBERED: usize = 64;

/// Where an account's chat log lives and how its episodes are cut.
#[derive(Debug, Clone)]
pub(crate) struct LogOptions {
    /// `<history>/<channel>/<account>`.
    pub(crate) root: PathBuf,
    /// The channel as paths name it (`wechat`).
    pub(crate) channel: &'static str,
    pub(crate) account: String,
    /// Quiet time after which the next message starts a new episode.
    pub(crate) gap: Duration,
}

#[cfg(test)]
impl LogOptions {
    /// The log of account `default` of `channel` in the instance at `home`.
    pub(crate) fn test(home: &std::path::Path, channel: &'static str) -> Self {
        Self {
            root: home.join("history").join(channel).join("default"),
            channel,
            account: "default".into(),
            gap: Duration::from_secs(2 * 60 * 60),
        }
    }
}

/// The owner's chats on one account: their direct chat and each thread in
/// it.
pub(crate) struct ChatLog {
    options: LogOptions,
    /// The owner's sender ID, which keys their direct chat.
    owner: String,
    /// The conversations recorded lately, by key, each knowing its newest
    /// episode.
    logs: Mutex<HashMap<String, Log>>,
}

impl ChatLog {
    /// The log of `owner`'s chats; `None`, with a warning, when the
    /// account's name is too long to name the log in `session.start`.
    pub(crate) fn new(options: LogOptions, owner: &str) -> Option<Self> {
        // Every conversation's directory is a digest of the same length, so
        // the direct chat's stands for all of them.
        let conversation = crate::conversation_dir(owner);
        if history::conversation_path(options.channel, &options.account, &conversation).is_none() {
            tracing::warn!("the account's name is too long for a chat log; its chat is not logged");
            return None;
        }
        Some(Self {
            options,
            owner: owner.to_owned(),
            logs: Mutex::new(HashMap::new()),
        })
    }

    /// The conversation keyed `key`, with the sender `peer`, when it is one
    /// recorded: the owner's direct chat or a thread in it. Groups, the
    /// owner's own messages there included, and other senders are not.
    pub(crate) fn conversation(&self, key: &str, peer: &str) -> Option<Logged<'_>> {
        (peer == self.owner && crate::intake::chat_of(key) == self.owner).then(|| Logged {
            log: self,
            key: key.to_owned(),
            dir: crate::conversation_dir(key),
        })
    }

    /// Remove years past the retention, in every conversation.
    pub(crate) fn prune(&self) {
        let (_, local) = local_now();
        let removed = history::prune_years(&self.options.root, local.year() - KEEP_YEARS);
        if removed > 0 {
            tracing::info!(removed, "removed chat log years past their retention");
        }
    }
}

/// One recorded conversation: the owner's direct chat, or a thread in it.
pub(crate) struct Logged<'a> {
    log: &'a ChatLog,
    key: String,
    /// The directory it is kept in: a digest of `key`, which the media
    /// directory shares, so sender and thread IDs never become paths.
    dir: String,
}

impl Logged<'_> {
    /// The log as `session.start` names it.
    pub(crate) fn reference(&self) -> scv_protocol::ChatLog {
        scv_protocol::ChatLog {
            channel: self.log.options.channel.to_owned(),
            account: self.log.options.account.clone(),
            conversation: self.dir.clone(),
        }
    }

    /// Record `entry` now. A failure is logged and otherwise ignored: the
    /// chat keeps working without its log.
    pub(crate) fn record(&self, mut entry: Entry) {
        let (at, local) = local_now();
        entry.at = at;
        if let Err(error) = self.with_log(|log| log.append(entry, &local)) {
            tracing::warn!(error = %error, "could not write to the chat log");
        }
    }

    /// A message of SCV's own, such as a notice.
    pub(crate) fn system(&self, text: &str) {
        self.record(Entry {
            role: Role::System,
            text: text.to_owned(),
            ..Entry::default()
        });
    }

    /// End the open episode (`/new`).
    pub(crate) fn end(&self) {
        let (at, local) = local_now();
        if let Err(error) = self.with_log(|log| log.end(at, &local)) {
            tracing::warn!(error = %error, "could not end an episode of the chat log");
        }
    }

    fn with_log<T>(&self, write: impl FnOnce(&mut Log) -> T) -> T {
        let mut logs = self.log.logs.lock().unwrap_or_else(PoisonError::into_inner);
        if logs.len() >= MAX_REMEMBERED && !logs.contains_key(&self.key) {
            logs.retain(|key, _| *key == self.log.owner);
        }
        let log = logs.entry(self.key.clone()).or_insert_with(|| {
            Log::new(self.log.options.root.join(&self.dir), self.log.options.gap)
        });
        write(log)
    }
}

/// An owner message's files as the log records them, from what the turn
/// received.
pub(crate) fn received(attachments: &[scv_protocol::Attachment]) -> Vec<FileRef> {
    attachments
        .iter()
        .map(|attachment| FileRef {
            kind: attachment.kind.clone(),
            name: attachment.name.clone(),
            path: attachment.path.clone(),
            mime: attachment.mime.clone(),
            size: attachment.size,
            transcript: attachment.transcript.clone().unwrap_or_default(),
        })
        .collect()
}

/// Files a message carried but no turn received, by kind and name.
pub(crate) fn announced(media: &[crate::Media]) -> Vec<FileRef> {
    media
        .iter()
        .map(|media| FileRef {
            kind: media.kind.as_str().to_owned(),
            name: crate::media::safe_name(&media.name),
            transcript: media.transcript.clone().unwrap_or_default(),
            ..FileRef::default()
        })
        .collect()
}

/// Now, in Unix milliseconds and on the host's clock.
pub(crate) fn local_now() -> (u64, LocalTime) {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX);
    let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    (ms, LocalTime::at(seconds, utc_offset(seconds)))
}

/// Seconds east of UTC of the host's time zone at `unix`, as the C library
/// sees it (`TZ`, else `/etc/localtime`); 0 when it cannot tell.
pub(crate) fn utc_offset(unix: i64) -> i32 {
    // `time_t` is 64 bits on every platform SCV supports.
    let time = unix as libc::time_t;
    // SAFETY: an all-zero `tm` is a valid value of the plain C struct.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call; `localtime_r` writes only
    // to `tm` and keeps no pointer to either.
    let result = unsafe { libc::localtime_r(&raw const time, &raw mut tm) };
    if result.is_null() {
        return 0;
    }
    i32::try_from(tm.tm_gmtoff).unwrap_or(0)
}

#[cfg(test)]
mod tests;
