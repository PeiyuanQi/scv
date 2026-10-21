//! Each delegated run's activity log and current state, which the
//! workbench shows as the agent's live view.
//!
//! A run (one delegation record: a CLI process for one turn, or an ACP
//! server or nested SCV for a whole conversation) writes two files under
//! [`Layout::runs`](scv_client::Layout::runs):
//!
//! - `<handle>.jsonl`, append-only: one [`RunEvent`] per line (the run's
//!   start, each turn's prompt, the agent's message lines, its tool calls as
//!   the same short redacted lines `tool.progress` shows, approval requests
//!   and answers, prompts steered into a turn, and each turn's end). Never
//!   tool output. At most [`MAX_LOG_BYTES`]; past that, only turns,
//!   approvals, notes, and the run's end are written.
//! - `<handle>.state.json`, rewritten atomically: what the run is doing now
//!   ([`RunState`]), on every change of state and at most every few seconds
//!   while it reports activity.
//!
//! Both files are private (mode 0600, directory 0700) and written by the
//! SCV process that owns the run, so every process of an instance (the
//! daemon, `scv exec`, a nested SCV's parent) is visible to the workbench,
//! including after it stopped. Bookkeeping never fails a run: a write that
//! fails is logged and skipped.

use std::{
    fs::File,
    io::{BufRead as _, Read as _, Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use scv_protocol::{AgentState, RunEvent, RunEventKind, RunLogPage};
use serde::{Deserialize, Serialize};

use crate::{
    delegate::{
        progress::{redact, scrub},
        records::{ProcessIdentity, write_private_json, write_private_json_unsynced},
    },
    sync::lock,
};

/// The largest activity log; past it only state-changing events are kept.
pub const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;
/// Longest text of one event, in characters.
const MAX_EVENT_CHARS: usize = 2000;
/// Longest prompt kept in a `turn_started` or `steered` event, in characters.
const MAX_PROMPT_CHARS: usize = 4000;
/// How often activity alone rewrites the state file.
const STATE_REFRESH: Duration = Duration::from_secs(5);
/// Longest state file read.
const MAX_STATE_BYTES: u64 = 64 * 1024;
/// Events one page of a log returns at most, and their total bytes.
pub const MAX_PAGE_EVENTS: usize = 500;
const MAX_PAGE_BYTES: usize = 256 * 1024;
/// A working run that has reported nothing for this long is shown as
/// `unknown`: SCV cannot tell whether it is thinking or stuck.
pub const QUIET_AFTER: Duration = Duration::from_secs(5 * 60);
/// Jobs a run remembers having served, newest last.
const MAX_RUN_JOBS: usize = 32;

/// How SCV reaches a run's agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunTransport {
    /// One CLI process per turn.
    Cli,
    /// An Agent Client Protocol server for the whole conversation.
    Acp,
    /// A nested `scv server --stdio` for the whole conversation.
    Scv,
}

impl RunTransport {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Acp => "acp",
            Self::Scv => "scv",
        }
    }
}

/// What a run starts with.
pub(crate) struct RunHeader {
    pub(crate) handle: String,
    pub(crate) agent: String,
    pub(crate) session: String,
    pub(crate) transport: RunTransport,
    pub(crate) cwd: PathBuf,
    pub(crate) conversation: Option<(String, u32)>,
    /// The agent's output carries live events (structured events, or text
    /// streamed as it prints); a run whose output says nothing until the
    /// end is never shown as quiet.
    pub(crate) streams: bool,
}

/// What a run is doing now: `<handle>.state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    pub v: u32,
    pub handle: String,
    pub agent: String,
    pub session: String,
    /// `cli`, `acp`, or `scv`.
    pub transport: String,
    pub cwd: PathBuf,
    /// The SCV process that owns the run.
    pub owner: ProcessIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    /// The background job the current or last turn serves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    /// Every background job its turns served, oldest first (at most 32): a
    /// live conversation's run serves the turns of several.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<String>,
    pub state: AgentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub started_unix: u64,
    /// When `state` began.
    pub since_unix: u64,
    pub last_activity_unix: u64,
    /// The newest message or tool line, for a quiet run's explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_line: Option<String>,
    /// The agent's own session ID once known, so an interrupted
    /// conversation can be resumed through the agent's own resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(default)]
    pub steerable: bool,
    #[serde(default)]
    pub streams: bool,
    /// The run's process ended; nothing more is logged.
    #[serde(default)]
    pub ended: bool,
    /// When it ended, which is what its age counts from once it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_unix: Option<u64>,
    /// The last event's position and the log's size.
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub bytes: u64,
}

/// One line of the log file: an event and the format version.
#[derive(Serialize, Deserialize)]
struct Line {
    v: u32,
    #[serde(flatten)]
    event: RunEvent,
}

/// A run's activity writer. Clones share one log.
#[derive(Clone)]
pub(crate) struct RunActivity(Arc<Mutex<Writer>>);

impl std::fmt::Debug for RunActivity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RunActivity")
    }
}

struct Writer {
    dir: PathBuf,
    log: Option<File>,
    state: RunState,
    /// Message text that has not ended its line yet.
    pending: String,
    truncated: bool,
    last_state_write: Instant,
    /// The process ended mid-turn (`exited`, `killed`, `lost`): the run's
    /// end is logged after that turn's, so a reader that stops at the end
    /// has seen why the turn failed.
    ending: Option<String>,
    /// How its last turn ended, which a run that ends between turns ends as.
    last_turn: Option<AgentState>,
    /// A turn has started and its end is not logged yet.
    turn_open: bool,
}

impl RunActivity {
    /// Start the log of a run, or `None` when its files cannot be created
    /// (or `handle` is not a run handle).
    pub(crate) fn open(dir: &Path, owner: ProcessIdentity, header: RunHeader) -> Option<Self> {
        if !valid_handle(&header.handle) {
            return None;
        }
        let file = match create_log(dir, &header.handle) {
            Ok(file) => file,
            Err(error) => {
                tracing::debug!(handle = %header.handle, %error, "could not start a run's activity log");
                return None;
            }
        };
        let now = unix_now();
        let (conversation, turn) = header
            .conversation
            .map_or((None, None), |(handle, turn)| (Some(handle), Some(turn)));
        let activity = Self(Arc::new(Mutex::new(Writer {
            dir: dir.to_owned(),
            log: Some(file),
            state: RunState {
                v: 1,
                handle: header.handle,
                agent: header.agent,
                session: header.session,
                transport: header.transport.as_str().to_owned(),
                cwd: header.cwd,
                owner,
                conversation,
                turn,
                job: None,
                jobs: Vec::new(),
                state: AgentState::Starting,
                detail: None,
                started_unix: now,
                since_unix: now,
                last_activity_unix: now,
                last_line: None,
                vendor: None,
                steerable: false,
                streams: header.streams,
                ended: false,
                ended_unix: None,
                seq: 0,
                bytes: 0,
            },
            pending: String::new(),
            truncated: false,
            last_state_write: Instant::now(),
            ending: None,
            last_turn: None,
            turn_open: false,
        })));
        {
            let mut writer = lock(&activity.0);
            let text = format!(
                "{} over {} in {}",
                writer.state.agent,
                writer.state.transport,
                writer.state.cwd.display()
            );
            writer.event(RunEventKind::RunStarted, text, |_| {});
            writer.save();
        }
        Some(activity)
    }

    /// A turn of the run's conversation began with `prompt`, for `job` when
    /// it serves one.
    pub(crate) fn turn_started(
        &self,
        conversation: Option<&str>,
        turn: u32,
        prompt: &str,
        job: Option<&str>,
    ) {
        let mut writer = lock(&self.0);
        writer.flush_text();
        if let Some(conversation) = conversation {
            writer.state.conversation = Some(conversation.to_owned());
        }
        writer.state.turn = Some(turn);
        writer.state.job = job.map(str::to_owned);
        writer.turn_open = true;
        if let Some(job) = job
            && !writer.state.jobs.iter().any(|served| served == job)
        {
            if writer.state.jobs.len() >= MAX_RUN_JOBS {
                writer.state.jobs.remove(0);
            }
            writer.state.jobs.push(job.to_owned());
        }
        writer.event(
            RunEventKind::TurnStarted,
            bounded_chars(&redact(prompt), MAX_PROMPT_CHARS),
            |event| {
                event.turn = Some(turn);
                event.job = job.map(str::to_owned);
            },
        );
        writer.transition(AgentState::Working, None);
    }

    /// A delta of the agent's message, which may end mid-line.
    pub(crate) fn text(&self, chunk: &str) {
        let mut writer = lock(&self.0);
        writer.pending.push_str(chunk);
        while let Some(end) = writer.pending.find('\n') {
            let line: String = writer.pending.drain(..=end).collect();
            writer.text_line(line.trim_end_matches(['\n', '\r']));
        }
        if writer.pending.chars().count() > MAX_EVENT_CHARS {
            let line = std::mem::take(&mut writer.pending);
            writer.text_line(&line);
        }
        writer.active();
    }

    /// A whole message of the agent, line by line.
    pub(crate) fn message(&self, text: &str) {
        let mut writer = lock(&self.0);
        writer.flush_text();
        for line in text.lines() {
            writer.text_line(line);
        }
        writer.active();
    }

    /// A tool call or command the agent started, as a progress line. Lines
    /// are scrubbed here whoever made them: a nested SCV relays its own.
    pub(crate) fn tool(&self, line: &str) {
        let line = scrub(line);
        let line = line.as_str();
        let mut writer = lock(&self.0);
        writer.flush_text();
        let failed = line.ends_with(" failed") || line.starts_with("exit ");
        let kind = if failed {
            RunEventKind::ToolFailed
        } else {
            RunEventKind::Tool
        };
        writer.state.last_line = Some(bounded_chars(line, 200));
        writer.event(kind, bounded_chars(line, MAX_EVENT_CHARS), |_| {});
        writer.active();
    }

    /// The agent's current plan step.
    pub(crate) fn plan(&self, step: &str) {
        let mut writer = lock(&self.0);
        writer.flush_text();
        writer.event(
            RunEventKind::Plan,
            bounded_chars(&scrub(step), MAX_EVENT_CHARS),
            |_| {},
        );
        writer.active();
    }

    /// The agent asked for approval; it waits for an answer. The summary
    /// is scrubbed here, as a nested SCV's is its own tool's.
    pub(crate) fn approval_requested(&self, risk: &str, summary: &str) {
        let mut writer = lock(&self.0);
        writer.flush_text();
        let summary = bounded_chars(&scrub(summary), MAX_EVENT_CHARS);
        writer.event(RunEventKind::ApprovalRequested, summary.clone(), |event| {
            event.risk = Some(risk.to_owned());
        });
        writer.transition(
            AgentState::AwaitingInput,
            Some(format!(
                "waiting for approval: {}",
                bounded_chars(&summary, 200)
            )),
        );
    }

    /// The approval request was answered.
    pub(crate) fn approval_resolved(&self, approved: bool) {
        let mut writer = lock(&self.0);
        let status = if approved { "approved" } else { "denied" };
        writer.event(RunEventKind::ApprovalResolved, String::new(), |event| {
            event.status = Some(status.to_owned());
        });
        writer.transition(AgentState::Working, None);
    }

    /// A later prompt went into the running turn.
    pub(crate) fn steered(&self, prompt: &str) {
        let mut writer = lock(&self.0);
        writer.flush_text();
        writer.event(
            RunEventKind::Steered,
            bounded_chars(&redact(prompt), MAX_PROMPT_CHARS),
            |_| {},
        );
        writer.active();
    }

    /// The current turn ended with `status` (`completed`, `failed`, ...).
    pub(crate) fn turn_ended(&self, status: &str, error: Option<&str>) {
        let mut writer = lock(&self.0);
        writer.flush_text();
        let turn = writer.state.turn;
        writer.event(
            RunEventKind::TurnEnded,
            error
                .map(|error| bounded_chars(&redact(error), MAX_EVENT_CHARS))
                .unwrap_or_default(),
            |event| {
                event.turn = turn;
                event.status = Some(status.to_owned());
            },
        );
        let state = match status {
            "completed" => AgentState::Completed,
            "declined" => AgentState::Declined,
            "timeout" => AgentState::Timeout,
            "cancelled" => AgentState::Cancelled,
            _ => AgentState::Failed,
        };
        writer.last_turn = Some(state);
        writer.turn_open = false;
        writer.transition(state, error.map(|error| bounded_chars(&redact(error), 300)));
        if let Some(how) = writer.ending.take() {
            writer.end(&how);
            writer.save();
        }
    }

    /// A live child finished its turn, or the call serving it let go, and
    /// waits for the next prompt: no turn is open.
    pub(crate) fn idle(&self) {
        let mut writer = lock(&self.0);
        writer.turn_open = false;
        writer.transition(AgentState::Idle, None);
    }

    /// The call serving a turn let go of it after the run's process ended:
    /// if the turn's own end was never logged (the call was dropped), the
    /// run's end is logged now.
    pub(crate) fn turn_released(&self) {
        let mut writer = lock(&self.0);
        writer.turn_open = false;
        if let Some(how) = writer.ending.take() {
            writer.end(&how);
            writer.save();
        }
    }

    /// The run's process ended: `exited`, `killed`, or `lost`.
    ///
    /// Between turns, the run ends as its last turn did (or failed, when
    /// killed, or before any turn ended). Mid-turn it fails, and its end is
    /// logged after the turn's own end, which says why, or once the turn is
    /// let go of.
    pub(crate) fn run_ended(&self, how: &str) {
        let mut writer = lock(&self.0);
        if writer.state.ended || writer.ending.is_some() {
            return;
        }
        writer.flush_text();
        let state = writer.state.state;
        if writer.turn_open {
            writer.transition(AgentState::Failed, Some(format!("its process {how}")));
            writer.ending = Some(how.to_owned());
        } else if state.is_final() {
            writer.end(how);
            writer.save();
        } else {
            let ended = match writer.last_turn {
                Some(last) if how == "exited" && state == AgentState::Idle => last,
                _ => AgentState::Failed,
            };
            writer.end(how);
            writer.transition(ended, Some(format!("its process {how}")));
        }
    }

    /// The agent's own session ID, for resuming the conversation later.
    pub(crate) fn set_vendor(&self, vendor: &str) {
        let mut writer = lock(&self.0);
        if writer.state.vendor.as_deref() != Some(vendor) {
            writer.state.vendor = Some(vendor.to_owned());
            writer.save();
        }
    }

    /// Whether a running turn of this run can take a further prompt.
    pub(crate) fn set_steerable(&self, steerable: bool) {
        let mut writer = lock(&self.0);
        if writer.state.steerable != steerable {
            writer.state.steerable = steerable;
            writer.save();
        }
    }
}

impl Writer {
    /// Log the run's end: nothing is logged after it.
    fn end(&mut self, how: &str) {
        self.flush_text();
        self.append(RunEventKind::RunEnded, String::new(), |event| {
            event.status = Some(how.to_owned());
        });
        self.state.ended = true;
        self.state.ended_unix = Some(unix_now());
    }

    fn text_line(&mut self, line: &str) {
        let line = redact(line);
        if !line.trim().is_empty() {
            self.state.last_line = Some(bounded_chars(line.trim(), 200));
        }
        self.event(
            RunEventKind::Text,
            bounded_chars(&line, MAX_EVENT_CHARS),
            |_| {},
        );
    }

    fn flush_text(&mut self) {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.text_line(line.trim_end_matches(['\n', '\r']));
        }
    }

    /// Append one event, unless the log is full and it is mere activity.
    fn event(&mut self, kind: RunEventKind, text: String, fill: impl FnOnce(&mut RunEvent)) {
        let keeps_going = matches!(
            kind,
            RunEventKind::Text | RunEventKind::Tool | RunEventKind::ToolFailed | RunEventKind::Plan
        );
        if self.truncated && keeps_going {
            return;
        }
        if !self.truncated && self.state.bytes >= MAX_LOG_BYTES {
            self.truncated = true;
            self.append(RunEventKind::Truncated, String::new(), |_| {});
            if keeps_going {
                return;
            }
        }
        self.append(kind, text, fill);
    }

    fn append(&mut self, kind: RunEventKind, text: String, fill: impl FnOnce(&mut RunEvent)) {
        let Some(file) = self.log.as_mut() else {
            return;
        };
        let mut event = RunEvent {
            seq: self.state.seq + 1,
            ts_ms: unix_ms(),
            kind,
            text: sanitize(&text),
            turn: None,
            status: None,
            job: None,
            risk: None,
            by: None,
        };
        fill(&mut event);
        let Ok(mut bytes) = serde_json::to_vec(&Line { v: 1, event }) else {
            return;
        };
        bytes.push(b'\n');
        match file.write_all(&bytes) {
            Ok(()) => {
                self.state.seq += 1;
                self.state.bytes += bytes.len() as u64;
            }
            Err(error) => {
                tracing::debug!(handle = %self.state.handle, %error, "could not write a run's activity");
                // A log that cannot be written stops here rather than leave
                // a half line for the next event.
                self.log = None;
            }
        }
    }

    /// Activity was reported: refresh the state file now and then.
    fn active(&mut self) {
        self.state.last_activity_unix = unix_now();
        if self.last_state_write.elapsed() >= STATE_REFRESH {
            self.save();
        }
    }

    fn transition(&mut self, state: AgentState, detail: Option<String>) {
        // A run whose process ended only settles on how it ended.
        if (self.state.ended || self.ending.is_some()) && !state.is_final() {
            return;
        }
        let now = unix_now();
        if self.state.state != state {
            self.state.since_unix = now;
        }
        self.state.state = state;
        self.state.detail = detail;
        self.state.last_activity_unix = now;
        self.save();
    }

    fn save(&mut self) {
        self.last_state_write = Instant::now();
        let name = format!("{}.state.json", self.state.handle);
        // Rewritten on the agent's own output path, so never synced: a
        // machine crash at worst loses the latest state, which then settles
        // as interrupted.
        if let Err(error) = write_private_json_unsynced(&self.dir, &name, &self.state) {
            tracing::debug!(handle = %self.state.handle, %error, "could not save a run's state");
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // The run's last owner let go: whatever it was doing has ended.
        if !self.state.ended {
            let how = self.ending.take().unwrap_or_else(|| "exited".to_owned());
            self.end(&how);
            if !self.state.state.is_final() {
                self.state.state = if self.state.state == AgentState::Idle {
                    AgentState::Completed
                } else {
                    AgentState::Failed
                };
                self.state.since_unix = unix_now();
            }
            self.save();
        }
    }
}

/// Whether `handle` is a delegation handle: an agent name, `-`, and six hex
/// digits, so it can name files.
pub fn valid_handle(handle: &str) -> bool {
    let Some((agent, suffix)) = handle.rsplit_once('-') else {
        return false;
    };
    handle.len() <= 64
        && !agent.is_empty()
        && agent
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && agent.as_bytes()[0].is_ascii_lowercase()
        && suffix.len() == 6
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn create_log(dir: &Path, handle: &str) -> std::io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    std::fs::OpenOptions::new()
        .append(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(format!("{handle}.jsonl")))
}

/// The state of run `handle`, as its owner last saved it.
pub fn load_state(dir: &Path, handle: &str) -> Option<RunState> {
    if !valid_handle(handle) {
        return None;
    }
    read_state(&dir.join(format!("{handle}.state.json")))
}

fn read_state(path: &Path) -> Option<RunState> {
    let file = open_nofollow(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES).read_to_end(&mut bytes).ok()?;
    let state: RunState = serde_json::from_slice(&bytes).ok()?;
    // Only a state named after its own handle is trusted.
    let stem = path.file_name()?.to_str()?.strip_suffix(".state.json")?;
    (stem == state.handle).then_some(state)
}

fn open_nofollow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Every run's saved state in `dir`.
pub fn list_states(dir: &Path) -> Vec<RunState> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(".state.json"))
        })
        .filter_map(|entry| read_state(&entry.path()))
        .collect()
}

/// Events of run `handle` after byte position `after` (0 from the start),
/// at most `limit` of them: one page of its log.
pub fn read_page(
    dir: &Path,
    handle: &str,
    after: u64,
    limit: usize,
) -> std::io::Result<RunLogPage> {
    if !valid_handle(handle) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a run handle",
        ));
    }
    let limit = limit.clamp(1, MAX_PAGE_EVENTS);
    let mut file = open_nofollow(&dir.join(format!("{handle}.jsonl")))?;
    let length = file.metadata()?.len();
    let mut position = after.min(length);
    if position > 0 {
        // A position that is not right after a line ending resynchronizes
        // at the next one.
        file.seek(SeekFrom::Start(position - 1))?;
        let mut previous = [0u8; 1];
        file.read_exact(&mut previous)?;
        if previous[0] != b'\n' {
            let mut skipped = Vec::new();
            let mut reader = std::io::BufReader::new(&mut file);
            position += reader.read_until(b'\n', &mut skipped)? as u64;
        }
    }
    file.seek(SeekFrom::Start(position))?;
    let mut reader = std::io::BufReader::new(file);
    let mut events = Vec::new();
    let mut bytes = 0usize;
    let mut more = false;
    let mut ended = false;
    loop {
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 || line.last() != Some(&b'\n') {
            // End of the log, or a line its writer has not finished.
            break;
        }
        if events.len() >= limit || bytes + read > MAX_PAGE_BYTES {
            more = true;
            break;
        }
        position += read as u64;
        bytes += read;
        if let Ok(Line { event, .. }) = serde_json::from_slice::<Line>(&line) {
            ended |= event.kind == RunEventKind::RunEnded;
            events.push(event);
        }
    }
    if !more && !ended {
        ended = load_state(dir, handle).is_some_and(|state| state.ended)
            && position >= reader.get_ref().metadata().map_or(0, |meta| meta.len());
    }
    Ok(RunLogPage {
        handle: handle.to_owned(),
        events,
        next: position,
        ended,
        more,
    })
}

/// Close the logs of runs whose owning SCV process stopped without ending
/// them (a crash or a kill): note it, end the log, and mark the run
/// interrupted. Returns the handles settled.
pub fn settle_orphans(dir: &Path) -> Vec<String> {
    let mut settled = Vec::new();
    for mut state in list_states(dir) {
        if state.ended || state.owner.is_alive() {
            continue;
        }
        let path = dir.join(format!("{}.jsonl", state.handle));
        let appended = append_existing(
            &path,
            &mut state,
            &[
                (
                    RunEventKind::Note,
                    "interrupted: the SCV process that ran it stopped".into(),
                    None,
                ),
                (RunEventKind::RunEnded, String::new(), Some("lost".into())),
            ],
        );
        if appended.is_err() {
            tracing::debug!(handle = %state.handle, "could not end an interrupted run's log");
        }
        state.ended = true;
        state.ended_unix = Some(unix_now());
        if !state.state.is_final() {
            state.state = AgentState::Interrupted;
            state.detail = Some("the SCV process that ran it stopped".into());
            state.since_unix = unix_now();
        }
        if write_private_json(dir, &format!("{}.state.json", state.handle), &state).is_ok() {
            settled.push(state.handle);
        }
    }
    settled
}

fn append_existing(
    path: &Path,
    state: &mut RunState,
    events: &[(RunEventKind, String, Option<String>)],
) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    for (kind, text, status) in events {
        let event = RunEvent {
            seq: state.seq + 1,
            ts_ms: unix_ms(),
            kind: *kind,
            text: text.clone(),
            turn: None,
            status: status.clone(),
            job: None,
            risk: None,
            by: None,
        };
        let mut bytes = serde_json::to_vec(&Line { v: 1, event }).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        state.seq += 1;
        state.bytes += bytes.len() as u64;
    }
    Ok(())
}

/// Remove the logs of runs that ended more than `keep` ago (a run settled
/// as interrupted counts from then), then the oldest ended ones beyond
/// `max_runs`. Runs still going are never removed.
pub fn prune(dir: &Path, keep: Duration, max_runs: usize) -> usize {
    let now = unix_now();
    let mut ended: Vec<RunState> = list_states(dir)
        .into_iter()
        .filter(|state| state.ended)
        .collect();
    let ended_at = |state: &RunState| state.ended_unix.unwrap_or(state.last_activity_unix);
    ended.sort_by_key(|state| std::cmp::Reverse(ended_at(state)));
    let mut removed = 0;
    for (index, state) in ended.iter().enumerate() {
        let old = now.saturating_sub(ended_at(state)) > keep.as_secs();
        if old || index >= max_runs {
            let _ = std::fs::remove_file(dir.join(format!("{}.jsonl", state.handle)));
            let _ = std::fs::remove_file(dir.join(format!("{}.state.json", state.handle)));
            removed += 1;
        }
    }
    removed
}

/// What a run's saved state means now: the state, and SCV's words for it.
/// A run whose owner stopped without ending it is interrupted; one that has
/// reported nothing for [`QUIET_AFTER`] while working is `unknown`.
pub fn current(state: &RunState, now_unix: u64) -> (AgentState, Option<String>) {
    if !state.ended && !state.owner.is_alive() {
        return (
            AgentState::Interrupted,
            Some("the SCV process that ran it stopped".into()),
        );
    }
    let quiet = now_unix.saturating_sub(state.last_activity_unix);
    match state.state {
        AgentState::Working if quiet >= QUIET_AFTER.as_secs() && state.streams => (
            AgentState::Unknown,
            Some(format!(
                "no activity for {} min{}",
                quiet / 60,
                state
                    .last_line
                    .as_deref()
                    .map(|line| format!("; last: {line}"))
                    .unwrap_or_default()
            )),
        ),
        AgentState::Working if !state.streams => (
            AgentState::Working,
            Some("this agent prints its answer only when it finishes".into()),
        ),
        AgentState::Idle => (AgentState::Idle, Some("waiting for the next prompt".into())),
        other => (other, state.detail.clone()),
    }
}

/// `text` without control characters other than tabs, which become spaces.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character == '\t' {
                ' '
            } else if character.is_control() {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

fn bounded_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests;
