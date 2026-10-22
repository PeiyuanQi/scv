//! The background job ledger: a durable record of every job, so jobs stay
//! visible after their session, a restart, or a crash, and an interrupted
//! one can be resumed.
//!
//! Each job is one file, `<jobs>/<session>/<job>.json` (directories 0700,
//! files 0600, replaced atomically), written when the job starts, when a
//! queued job's turn begins, when it ends, and when its result is reported.
//! A job its session dropped unfinished is recorded `interrupted` then; one
//! whose SCV process died without a word is found by [`settle_orphans`],
//! which the daemon runs at startup and periodically. Bookkeeping never
//! fails a job: a write that fails is logged and skipped.

use std::{
    collections::HashMap,
    io::Read as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use scv_protocol::{AgentState, JobOrigin, WorktreeRef};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::delegate::records::{ProcessIdentity, write_private_json};

/// Longest prompt kept for a resume, in bytes.
const MAX_PROMPT_BYTES: usize = 16 * 1024;
/// Longest reply or error kept, in bytes.
const MAX_REPLY_BYTES: usize = 8 * 1024;
/// Largest record file read. A prompt, reply, and error at their bounds
/// expand when JSON escapes control characters (`\u00XX` is six bytes), so
/// the cap is above that expansion. [`fit_record`] still shrinks a record
/// that would not fit, or a later read would skip it and the job would
/// vanish.
const MAX_RECORD_BYTES: u64 = 512 * 1024;

/// Held around every write of a record, and around every change that reads
/// one first (dismiss, update, settle, prune), so those changes are not
/// reordered with each other. A runtime snapshot is taken earlier, outside
/// this lock; [`write`] is what keeps that snapshot from undoing a
/// dismissal or a resume link. Nothing that holds the lock takes it again.
static WRITES: Mutex<()> = Mutex::new(());

fn writing() -> MutexGuard<'static, ()> {
    crate::sync::lock(&WRITES)
}

/// One background job, as the ledger keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRecord {
    pub v: u32,
    pub session: String,
    pub job: String,
    pub agent: String,
    pub task: String,
    /// The delegated prompt, bounded.
    pub prompt: String,
    /// The call's other arguments (`cwd`, `model`, `effort`, `session`,
    /// `worktree`, ...), without the prompt.
    #[serde(default)]
    pub arguments: Value,
    pub workspace: PathBuf,
    /// Who the session serves, such as `TUI` or `workbench`.
    pub label: String,
    #[serde(default)]
    pub started_by: JobOrigin,
    #[serde(default)]
    pub reviewed: bool,
    /// The SCV process that runs it.
    pub owner: ProcessIdentity,
    pub state: AgentState,
    pub created_unix: u64,
    pub updated_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_unix: Option<u64>,
    /// The conversation it ran a turn of, once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The result reached the model or the client.
    #[serde(default)]
    pub reported: bool,
    /// Why it stopped before finishing, for an interrupted job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<String>,
    /// The job it was resumed as, `<session>/<job>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_as: Option<String>,
    /// The interrupted job this one resumes, `<session>/<job>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
    /// The owner dismissed it: what is left only keeps its number taken
    /// until it is pruned, and nothing lists or loads it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dismissed: bool,
}

impl JobRecord {
    /// Whether the owner can resume it: interrupted, not a reviewed job
    /// (its journal is the record; a new reviewed call restates it), and
    /// not resumed already.
    pub fn resumable(&self) -> bool {
        self.state == AgentState::Interrupted && !self.reviewed && self.resumed_as.is_none()
    }
}

/// A job about to be recorded.
pub(crate) struct NewJob<'a> {
    pub(crate) job: &'a str,
    pub(crate) agent: &'a str,
    pub(crate) task: &'a str,
    /// The call's arguments, prompt included.
    pub(crate) arguments: &'a Value,
    pub(crate) started_by: JobOrigin,
    pub(crate) reviewed: bool,
    /// It waits behind its conversation's running turn.
    pub(crate) queued: bool,
}

/// Where one session's jobs are recorded.
#[derive(Debug, Clone)]
pub struct JobLedger {
    root: PathBuf,
    session: String,
    label: String,
    workspace: PathBuf,
    owner: Option<ProcessIdentity>,
    /// The newest revision of each job's record written, so writes made
    /// outside the job table's lock never put an older one back.
    written: Arc<Mutex<HashMap<String, u64>>>,
}

impl JobLedger {
    /// The ledger of session `session` under `root` (the instance's
    /// [`Layout::jobs`](scv_client::Layout::jobs)); `label` says who the
    /// session serves. `None` for a session ID that cannot name a directory.
    pub fn new(root: &Path, session: &str, label: &str, workspace: &Path) -> Option<Self> {
        valid_name(session).then(|| Self {
            root: root.to_owned(),
            session: session.to_owned(),
            label: label.to_owned(),
            workspace: workspace.to_owned(),
            owner: ProcessIdentity::current(),
            written: Arc::default(),
        })
    }

    /// A new record of the job `new` describes, not yet written.
    pub(crate) fn record(&self, new: &NewJob<'_>) -> Option<JobRecord> {
        let owner = self.owner?;
        let now = unix_now();
        let mut rest = new.arguments.clone();
        let prompt = rest
            .as_object_mut()
            .and_then(|object| object.remove("prompt"))
            .and_then(|prompt| prompt.as_str().map(str::to_owned))
            .unwrap_or_default();
        Some(JobRecord {
            v: 1,
            session: self.session.clone(),
            job: new.job.to_owned(),
            agent: new.agent.to_owned(),
            task: new.task.to_owned(),
            prompt: bounded(&prompt, MAX_PROMPT_BYTES),
            arguments: rest,
            workspace: self.workspace.clone(),
            label: self.label.clone(),
            started_by: new.started_by,
            reviewed: new.reviewed,
            owner,
            state: if new.queued {
                AgentState::Queued
            } else {
                AgentState::Working
            },
            created_unix: now,
            updated_unix: now,
            finished_unix: None,
            conversation: None,
            worktree: None,
            reply: None,
            error: None,
            reported: false,
            interrupted: None,
            resumed_as: None,
            resumed_from: None,
            dismissed: false,
        })
    }

    /// The highest `job-<n>` this session already has a record of, so a
    /// session that comes back under its ID (a daemon's workbench host)
    /// numbers its jobs after them instead of over them.
    pub(crate) fn last_number(&self) -> u64 {
        let Ok(entries) = std::fs::read_dir(self.root.join(&self.session)) else {
            return 0;
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()?
                    .strip_suffix(".json")?
                    .strip_prefix("job-")?
                    .parse::<u64>()
                    .ok()
            })
            .max()
            .unwrap_or(0)
    }

    /// The conversation handles this session's records name, so a session
    /// that comes back under its ID numbers new conversations after them;
    /// `None` when the session has no records, so it was not here before.
    pub(crate) fn conversations(&self) -> Option<Vec<String>> {
        let entries = std::fs::read_dir(self.root.join(&self.session)).ok()?;
        let handles = entries
            .filter_map(Result::ok)
            .filter_map(|entry| read_record(&entry.path()))
            .flat_map(|record| {
                let continued = record
                    .arguments
                    .get("session")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                record.conversation.into_iter().chain(continued)
            })
            .collect();
        Some(handles)
    }

    /// Save `record`.
    pub(crate) fn save(&self, record: &JobRecord) {
        if let Err(error) = save(&self.root, record) {
            tracing::debug!(job = %record.job, %error, "could not record a background job");
        }
    }

    /// Save revision `revision` of `record`, unless a newer one of the job
    /// was written already. Callers write after letting go of the job
    /// table, so writes may arrive out of order; this keeps the newest.
    pub(crate) fn save_revision(&self, record: &JobRecord, revision: u64) {
        let mut written = crate::sync::lock(&self.written);
        if written
            .get(&record.job)
            .is_some_and(|newest| *newest >= revision)
        {
            return;
        }
        self.save(record);
        written.insert(record.job.clone(), revision);
    }
}

/// Write `record` in its session's directory under `root`.
pub fn save(root: &Path, record: &JobRecord) -> std::io::Result<()> {
    let _writing = writing();
    write(root, record)
}

/// [`save`], for a caller that holds [`WRITES`].
///
/// The job runtime writes a snapshot it took before this lock. A dismissal
/// already on disk stays, prompt and reply included. A resume link already
/// recorded stays when the snapshot was copied without it. When the job was
/// not dismissed, the snapshot's other fields still land.
fn write(root: &Path, record: &JobRecord) -> std::io::Result<()> {
    if !valid_name(&record.session) || !valid_name(&record.job) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a session or job name",
        ));
    }
    let dir = root.join(&record.session);
    let name = format!("{}.json", record.job);
    let mut record = record.clone();
    if let Some(existing) = read_record(&dir.join(&name)) {
        // The owner's dismissal wins over a completion that was copied first
        // and written second: the tombstone keeps the prompt and the reply
        // off disk.
        if existing.dismissed && !record.dismissed {
            return Ok(());
        }
        // Resume records the link before the runtime's next snapshot, which
        // was copied without it.
        if record.resumed_as.is_none() {
            record.resumed_as = existing.resumed_as;
        }
        if record.resumed_from.is_none() {
            record.resumed_from = existing.resumed_from;
        }
    }
    fit_record(&mut record);
    write_private_json(&dir, &name, &record)
}

/// Shrink `record` until its JSON fits in [`MAX_RECORD_BYTES`]. A file over
/// that cap is not read back, so the job would disappear from the ledger.
fn fit_record(record: &mut JobRecord) {
    let Some(limit) = usize::try_from(MAX_RECORD_BYTES).ok() else {
        return;
    };
    for _ in 0..24 {
        let Ok(bytes) = serde_json::to_vec(record) else {
            return;
        };
        if bytes.len() <= limit {
            return;
        }
        let prompt = record.prompt.len();
        let reply = record.reply.as_ref().map(String::len).unwrap_or(0);
        let error = record.error.as_ref().map(String::len).unwrap_or(0);
        let longest = prompt.max(reply).max(error);
        if longest == 0 {
            record.arguments = Value::Null;
            record.task.clear();
            continue;
        }
        let keep = longest / 2;
        if prompt == longest {
            let next = scv_client::text::utf8_prefix(&record.prompt, keep).to_owned();
            record.prompt = next;
        } else if reply == longest {
            let next = record
                .reply
                .as_deref()
                .map(|text| scv_client::text::utf8_prefix(text, keep).to_owned());
            record.reply = next;
        } else {
            let next = record
                .error
                .as_deref()
                .map(|text| scv_client::text::utf8_prefix(text, keep).to_owned());
            record.error = next;
        }
    }
}

/// The record of `job` in `session`, unless the owner dismissed it.
pub fn load(root: &Path, session: &str, job: &str) -> Option<JobRecord> {
    if !valid_name(session) || !valid_name(job) {
        return None;
    }
    read_record(&root.join(session).join(format!("{job}.json"))).filter(|record| !record.dismissed)
}

/// Forget `job` of `session` for the owner: its record keeps only its
/// name, so the job's number is not given to another job before the
/// record is pruned.
pub fn dismiss(root: &Path, session: &str, job: &str) -> std::io::Result<()> {
    let _writing = writing();
    let mut record = load(root, session, job)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such job record"))?;
    record.dismissed = true;
    // Settled, so it is pruned like any other: nothing settles a dismissed
    // record later.
    if !record.state.is_final() {
        interrupt(&mut record, "dismissed by the owner");
    }
    record.prompt.clear();
    record.task.clear();
    record.arguments = Value::Null;
    record.reply = None;
    record.error = None;
    record.interrupted = None;
    record.updated_unix = unix_now();
    write(root, &record)
}

/// Remove `job`'s record of `session` from disk.
pub fn remove(root: &Path, session: &str, job: &str) -> std::io::Result<()> {
    if !valid_name(session) || !valid_name(job) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a session or job name",
        ));
    }
    std::fs::remove_file(root.join(session).join(format!("{job}.json")))?;
    // The session's directory goes with its last job.
    let _ = std::fs::remove_dir(root.join(session));
    Ok(())
}

/// Change `job`'s record with `change` and save it, when it exists.
pub fn update(
    root: &Path,
    session: &str,
    job: &str,
    change: impl FnOnce(&mut JobRecord),
) -> Option<JobRecord> {
    update_if(root, session, job, |record| {
        change(record);
        true
    })
}

/// Change `job`'s record with `change` and save it, when it exists and
/// `change` says it changed it.
fn update_if(
    root: &Path,
    session: &str,
    job: &str,
    change: impl FnOnce(&mut JobRecord) -> bool,
) -> Option<JobRecord> {
    let _writing = writing();
    let mut record = load(root, session, job)?;
    if !change(&mut record) {
        return None;
    }
    record.updated_unix = unix_now();
    write(root, &record).ok()?;
    Some(record)
}

/// Every job recorded under `root` that the owner has not dismissed.
pub fn list(root: &Path) -> Vec<JobRecord> {
    let mut records = list_all(root);
    records.retain(|record| !record.dismissed);
    records
}

/// Every record under `root`, dismissed ones too.
pub fn list_all(root: &Path) -> Vec<JobRecord> {
    let Ok(sessions) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for session in sessions.filter_map(Result::ok) {
        let Some(name) = session.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !valid_name(&name)
            || !std::fs::symlink_metadata(session.path()).is_ok_and(|meta| meta.is_dir())
        {
            continue;
        }
        let Ok(jobs) = std::fs::read_dir(session.path()) else {
            continue;
        };
        for job in jobs.filter_map(Result::ok) {
            if let Some(record) = read_record(&job.path())
                && record.session == name
            {
                records.push(record);
            }
        }
    }
    records
}

fn read_record(path: &Path) -> Option<JobRecord> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES).read_to_end(&mut bytes).ok()?;
    let record: JobRecord = serde_json::from_slice(&bytes).ok()?;
    // Only a record named after its own job is trusted.
    let stem = path.file_stem()?.to_str()?;
    (stem == record.job).then_some(record)
}

/// Mark interrupted every unfinished job whose SCV process no longer runs
/// (a crash, a kill, or a restart that could not wait). Returns them.
pub fn settle_orphans(root: &Path) -> Vec<JobRecord> {
    let mut settled = Vec::new();
    for record in list(root) {
        if record.state.is_final() || record.owner.is_alive() {
            continue;
        }
        // Checked again on the record as it is now: it may have finished,
        // been dismissed, or been resumed since it was listed.
        if let Some(record) = update_if(root, &record.session, &record.job, |record| {
            let orphaned = !record.state.is_final() && !record.owner.is_alive();
            if orphaned {
                interrupt(record, "the SCV process that ran it stopped");
            }
            orphaned
        }) {
            settled.push(record);
        }
    }
    settled
}

/// Record `record` as interrupted for `reason`.
pub(crate) fn interrupt(record: &mut JobRecord, reason: &str) {
    let now = unix_now();
    record.state = AgentState::Interrupted;
    record.interrupted = Some(reason.to_owned());
    record.finished_unix = Some(now);
    record.updated_unix = now;
}

/// Remove finished jobs whose record has not changed for `keep`, and
/// interrupted and dismissed ones too once they are that old.
pub fn prune(root: &Path, keep: Duration) -> usize {
    let now = unix_now();
    let mut removed = 0;
    let old = |record: &JobRecord| {
        record.state.is_final() && now.saturating_sub(record.updated_unix) > keep.as_secs()
    };
    for record in list_all(root) {
        if !old(&record) {
            continue;
        }
        // Checked again on the record as it is now, which a change since
        // it was listed keeps.
        let _writing = writing();
        let path = root
            .join(&record.session)
            .join(format!("{}.json", record.job));
        if read_record(&path).is_some_and(|record| old(&record))
            && remove(root, &record.session, &record.job).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// The record's reply and error from a finished job's result.
pub(crate) fn finish(record: &mut JobRecord, state: AgentState, result: &Value, reported: bool) {
    let now = unix_now();
    record.state = state;
    record.finished_unix = Some(now);
    record.updated_unix = now;
    record.reported |= reported;
    let text = |key: &str| result.get(key).and_then(Value::as_str).map(str::to_owned);
    record.reply = text("reply").map(|reply| bounded(&reply, MAX_REPLY_BYTES));
    record.error = text("error").map(|error| bounded(&error, MAX_REPLY_BYTES));
    if let Some(conversation) = text("session") {
        record.conversation = Some(conversation);
    }
    if let Some(worktree) = result
        .get("worktree")
        .and_then(|value| serde_json::from_value::<WorktreeRef>(value.clone()).ok())
    {
        record.worktree = Some(worktree);
    }
}

/// Whether `name` can name a ledger file or directory: 1-80 ASCII letters,
/// digits, `-`, or `_`, as session IDs and job handles are.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 80
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn bounded(text: &str, max: usize) -> String {
    scv_client::text::utf8_prefix(text, max).to_owned()
}

/// The current time, as records hold it.
pub(crate) fn now() -> u64 {
    unix_now()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests;
