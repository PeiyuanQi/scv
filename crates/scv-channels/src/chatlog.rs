//! The bridge's side of the chat log (see [`scv_client::history`]): the
//! account owner's direct chat is recorded, in the host's local time, and
//! the owner's sessions name it so the server can reload its open episode.

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use scv_client::history::{self, Entry, FileRef, LocalTime, Log, Role};

/// Logs are kept at most this many years.
const KEEP_YEARS: i64 = 120;

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

/// The owner's direct chat on one account.
pub(crate) struct ChatLog {
    options: LogOptions,
    /// The owner's sender ID, which keys their direct chat.
    owner: String,
    conversation: String,
    log: Mutex<Log>,
}

impl ChatLog {
    /// The log of `owner`'s direct chat; `None`, with a warning, when the
    /// account's name is too long to name the log in `session.start`.
    pub(crate) fn new(options: LogOptions, owner: &str) -> Option<Self> {
        let conversation = crate::conversation_dir(owner);
        if history::conversation_path(options.channel, &options.account, &conversation).is_none() {
            tracing::warn!("the account's name is too long for a chat log; its chat is not logged");
            return None;
        }
        let log = Log::new(options.root.join(&conversation), options.gap);
        Some(Self {
            options,
            owner: owner.to_owned(),
            conversation,
            log: Mutex::new(log),
        })
    }

    /// Whether the conversation keyed `key` is the one recorded.
    pub(crate) fn logs(&self, key: &str) -> bool {
        key == self.owner
    }

    /// The log as `session.start` names it.
    pub(crate) fn reference(&self) -> scv_protocol::ChatLog {
        scv_protocol::ChatLog {
            channel: self.options.channel.to_owned(),
            account: self.options.account.clone(),
            conversation: self.conversation.clone(),
        }
    }

    /// Record `entry` now. A failure is logged and otherwise ignored: the
    /// chat keeps working without its log.
    pub(crate) fn record(&self, mut entry: Entry) {
        let (at, local) = local_now();
        entry.at = at;
        let result = self
            .log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .append(entry, &local);
        if let Err(error) = result {
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
        let result = self
            .log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .end(at, &local);
        if let Err(error) = result {
            tracing::warn!(error = %error, "could not end an episode of the chat log");
        }
    }

    /// Remove years past the retention.
    pub(crate) fn prune(&self) {
        let (_, local) = local_now();
        let removed = history::prune_years(&self.options.root, local.year() - KEEP_YEARS);
        if removed > 0 {
            tracing::info!(removed, "removed chat log years past their retention");
        }
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
fn utc_offset(unix: i64) -> i32 {
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
