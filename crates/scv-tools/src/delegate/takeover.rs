//! The owner's takeover of a delegated conversation in its agent's own
//! terminal: what the workbench's `attach` runs.
//!
//! A delegated agent runs headless (one CLI process per turn, or an ACP
//! server speaking JSON-RPC on its standard streams), so it has no terminal
//! to mirror. Taking a conversation over opens the agent's own interactive
//! UI instead, on the conversation's own agent session, in a
//! pseudo-terminal the daemon owns ([`super::pty`]): `codex resume
//! <thread>`, in the conversation's directory, private home, and agent
//! environment, recorded and cleaned up as a delegated run of its own. While
//! it runs, the conversation runs no turn of SCV's: calls to it wait or
//! queue as behind a running turn, and its live ACP server, if it kept one,
//! is stopped first, so two processes never drive one agent session. When
//! the terminal closes, the next turn continues the same session, with what
//! was done in it.
//!
//! [`launch`] decides whether an agent's terminal may be offered at all. A
//! terminal is offered only where it asks nothing SCV's approval gate
//! decides, so a key typed into it can never answer an approval: the
//! adapter must declare the CLI's interactive resume and the switches under
//! which it never asks ([`adapters::Takeover`]), and the agent must run
//! with `permissions = "full"`, where SCV's own runs of it relay no
//! approval either.
//!
//! The daemon keeps the screen itself (with the `vt100` parser): it reads
//! the terminal whether or not anyone watches, so the agent never blocks on
//! its output, and it answers the terminal queries a UI makes as it starts
//! (cursor position, device attributes). The output is untrusted, so a
//! [`Scanner`] bounds what it can cost the parser first, and a parser
//! failure ends the takeover rather than leave the agent writing to a
//! terminal nobody reads. A client reads [`Takeover::page`]s: the
//! rows that changed, as styled text with values that look like
//! credentials masked by the activity log's own rules
//! ([`progress::scrubbed_ranges`]). Raw terminal output never leaves this
//! module and nothing of the screen is written to disk; the screen goes
//! with the takeover.

use std::{
    ffi::OsString,
    ops::Range,
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use scv_protocol::{AttachColor, AttachPage, AttachRow, AttachSpan};
use tokio::{sync::watch, time::Instant};

use crate::{
    DelegationContext,
    delegate::{
        activity::{RunActivity, RunHeader, RunTransport},
        adapters::{self, Takeover as Offer},
        background::lane::Place,
        conversation::TakeoverHold,
        progress,
        pty::{self, Pty, PtySpec},
        records::{self, DEPTH_VARIABLE, STOP_GRACE, group_exists},
    },
    process::ProcessGroup,
    sync::lock,
};

/// The terminal's size when a client names none, and the bounds of any.
pub(crate) const DEFAULT_SIZE: (u16, u16) = (100, 30);
const MIN_SIZE: (u16, u16) = (20, 5);
const MAX_SIZE: (u16, u16) = (400, 200);
/// The most input one request may carry.
pub(crate) const MAX_INPUT_BYTES: usize = 64 * 1024;
/// How long typed input may wait for an agent that does not read it.
const INPUT_GRACE: Duration = Duration::from_secs(2);
/// What SCV answers a terminal's primary device attributes query with: a
/// VT220-class terminal with ANSI color.
const DEVICE_ATTRIBUTES: &[u8] = b"\x1b[?62;22c";

/// How to open an agent's terminal on one of its sessions: checked and
/// resolved when its backend is built, so `attach` refuses at once.
#[derive(Debug, Clone)]
pub(crate) struct TakeoverLaunch {
    pub(crate) agent: String,
    executable: PathBuf,
    /// Arguments; `{session}` is the agent's session ID.
    args: Vec<String>,
    /// The adapter environment the agent's own runs get.
    environment: Vec<(OsString, OsString)>,
    delegation: Option<DelegationContext>,
}

/// Whether agent `name`, run as `command` (searched in `search_dirs`) with
/// `environment` and, with `full`, `permissions = "full"`, can be taken over
/// in its terminal; the error says why not, for the owner.
pub(crate) fn launch(
    name: &str,
    command: &str,
    search_dirs: &[PathBuf],
    full: bool,
    environment: &[(OsString, OsString)],
    delegation: Option<&DelegationContext>,
) -> Result<TakeoverLaunch, String> {
    let Some(adapter) = adapters::adapter(name) else {
        return Err(format!("{name} has no terminal SCV can open"));
    };
    let (args, full_args) = match adapter.takeover {
        Offer::Unsupported => {
            return Err(format!(
                "{} has no interactive mode SCV has verified to resume a conversation by its \
                 session ID, so it cannot be taken over in its terminal; follow its activity \
                 view and send it prompts instead",
                adapter.product
            ));
        }
        Offer::Refused(why) => {
            return Err(format!(
                "{name} cannot be taken over in its terminal: {why}. Follow its activity view \
                 and send it prompts instead"
            ));
        }
        Offer::Supported { args, full_args } => (args, full_args),
    };
    if !full {
        return Err(format!(
            "{name} asks for approval in its own terminal unless [agents.{name}] permissions = \
             \"full\": SCV relays those requests to its approval gate when it runs {name}, so a \
             terminal whose keys would answer them is not offered"
        ));
    }
    let executable = adapters::resolve_agent_executable(command, search_dirs).ok_or_else(|| {
        format!(
            "{name}'s CLI {command:?} was not found on PATH or in the user's install directories"
        )
    })?;
    Ok(TakeoverLaunch {
        agent: name.to_owned(),
        executable,
        args: args
            .iter()
            .chain(full_args)
            .map(|arg| (*arg).to_owned())
            .collect(),
        environment: environment.to_vec(),
        delegation: delegation.cloned(),
    })
}

/// A terminal size within bounds: `cols` by `rows`, each defaulted.
pub(crate) fn clamp_size(cols: Option<u16>, rows: Option<u16>) -> (u16, u16) {
    (
        cols.unwrap_or(DEFAULT_SIZE.0).clamp(MIN_SIZE.0, MAX_SIZE.0),
        rows.unwrap_or(DEFAULT_SIZE.1).clamp(MIN_SIZE.1, MAX_SIZE.1),
    )
}

/// Where a takeover's process is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Life {
    Running,
    /// The leader exited and was collected; cleanup is under way.
    Exited,
    /// Its group is stopped, its record removed, and the conversation free.
    Finished,
}

/// A conversation taken over in its agent's terminal.
pub(crate) struct Takeover {
    pub(crate) agent: String,
    /// The SCV session that holds the conversation.
    pub(crate) session: String,
    pub(crate) conversation: String,
    /// Its delegated run's handle, when it is recorded.
    pub(crate) run: Option<String>,
    pid: u32,
    pty: Pty,
    screen: StdMutex<Screen>,
    life: watch::Receiver<Life>,
    /// How the terminal closed, once it has.
    ended: StdMutex<Option<String>>,
    /// The owner, the session's end, or SCV's shutdown asked it to end.
    stopping: AtomicBool,
    /// Why SCV ended it on its own, such as its screen failing.
    failed: StdMutex<Option<String>>,
    activity: Option<RunActivity>,
}

impl std::fmt::Debug for Takeover {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Takeover")
            .field("conversation", &self.conversation)
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

/// Open `launch`'s terminal on the conversation `hold` holds, sized `cols`
/// by `rows`, for SCV session `session`. `place` is the takeover's place at
/// the front of the conversation's lane, so calls queued behind it wait.
/// Both are let go when the terminal closes. The conversation's kept
/// process must already be stopped.
pub(in crate::delegate) async fn start(
    launch: &TakeoverLaunch,
    hold: TakeoverHold,
    place: Place,
    session: &str,
    (cols, rows): (u16, u16),
) -> Result<Arc<Takeover>, String> {
    let args: Vec<String> = launch
        .args
        .iter()
        .map(|arg| arg.replace("{session}", &hold.vendor))
        .collect();
    let pending = launch.delegation.as_ref().map(|delegation| {
        delegation.registry.begin_at(
            delegation.owner_depth(),
            &launch.agent,
            &delegation.session,
            &hold.cwd,
            Some((hold.handle.as_str(), hold.turns)),
        )
    });
    let mut environment = launch.environment.clone();
    match &pending {
        Some(pending) => environment.extend(pending.environment.iter().cloned()),
        None => environment.push((
            DEPTH_VARIABLE.into(),
            (records::current_depth() + 1).to_string().into(),
        )),
    }
    let project =
        crate::project_environment::resolve(&hold.cwd, &environment, std::future::pending::<()>())
            .await
            .map_err(|error| format!("{error:#}"))?;
    environment.extend(project.environment);
    // The daemon's own terminal settings mean nothing here: the screen SCV
    // keeps speaks xterm's sequences.
    environment.push(("TERM".into(), "xterm-256color".into()));
    environment.push(("COLORTERM".into(), "truecolor".into()));
    let spawned = pty::spawn(&PtySpec {
        executable: &launch.executable,
        args: &args,
        cwd: &hold.cwd,
        environment: &environment,
        cols,
        rows,
    })
    .map_err(|error| format!("could not open {}'s terminal: {error}", launch.agent))?;
    let pid = spawned.pid;
    records::track_spawned(pid);
    let (run, guard, activity) = match (pending, &launch.delegation) {
        (Some(pending), Some(delegation)) => {
            let run = pending.handle.clone();
            let activity = delegation.registry.open_activity(RunHeader {
                handle: pending.handle.clone(),
                agent: launch.agent.clone(),
                session: delegation.session.clone(),
                transport: RunTransport::Terminal,
                cwd: hold.cwd.clone(),
                conversation: Some((hold.handle.clone(), hold.turns)),
                streams: true,
            });
            // Recording never fails the takeover: an unrecorded process is
            // still tagged for later sweeps. No SCV turn runs in it, so it is
            // recorded idle: a planned restart does not wait for it.
            let guard = delegation.registry.register(pending, pid).ok();
            if let Some(guard) = &guard {
                guard.set_idle();
            }
            (Some(run), guard, activity)
        }
        _ => (None, None, None),
    };
    if let Some(activity) = &activity {
        activity.set_vendor(&hold.vendor);
        activity.terminal(
            &format!(
                "taken over by the owner in {}'s terminal ({})",
                launch.agent,
                args.join(" ")
            ),
            ATTACHED,
        );
    }
    let (life_tx, life) = watch::channel(Life::Running);
    let takeover = Arc::new(Takeover {
        agent: launch.agent.clone(),
        session: session.to_owned(),
        conversation: hold.handle.clone(),
        run,
        pid,
        pty: spawned.pty,
        screen: StdMutex::new(Screen::new(cols, rows)),
        life,
        ended: StdMutex::new(None),
        stopping: AtomicBool::new(false),
        failed: StdMutex::new(None),
        activity,
    });
    tokio::spawn(reap(
        Arc::clone(&takeover),
        spawned.child,
        guard,
        (hold, place),
        life_tx,
    ));
    // The terminal's I/O has a thread of its own: see [`Pty::run_io`].
    let io = Arc::clone(&takeover);
    let runtime = tokio::runtime::Handle::current();
    let started = std::thread::Builder::new()
        .name("scv-terminal".into())
        .spawn(move || {
            let result = io.pty.run_io(|output| io.take_output(output));
            // Its I/O failed, or its screen did: nobody reads the terminal
            // any more, so an agent that still runs would soon block
            // writing to it. End the takeover. (When the agent closes its
            // terminal, it is exiting, and the reaper sees to it.)
            let failed = match result {
                Err(error) => {
                    lock(&io.failed).get_or_insert_with(|| {
                        format!("its terminal failed ({error}), so it was ended")
                    });
                    true
                }
                Ok(()) => lock(&io.failed).is_some(),
            };
            if failed && io.is_running() {
                let takeover = Arc::clone(&io);
                runtime.spawn(async move { takeover.end().await });
            }
        });
    if let Err(error) = started {
        // The reaper lets the conversation go once the process is gone.
        takeover.kill_now();
        return Err(format!(
            "could not start {}'s terminal: {error}",
            takeover.agent
        ));
    }
    Ok(takeover)
}

/// The detail a takeover's run shows while the owner watches it, and while
/// nobody does.
const ATTACHED: &str = "taken over in its terminal; the owner is attached";
const DETACHED: &str = "taken over in its terminal; nobody is attached";

impl Takeover {
    /// Whether the agent's process still runs.
    pub(crate) fn is_running(&self) -> bool {
        *self.life.borrow() == Life::Running
    }

    /// Whether the terminal has closed and everything it held is let go.
    pub(crate) fn is_finished(&self) -> bool {
        *self.life.borrow() == Life::Finished
    }

    /// Record that the owner attached to the terminal, or detached from it.
    pub(crate) fn note_attached(&self, attached: bool) {
        if let Some(activity) = &self.activity
            && self.is_running()
        {
            if attached {
                activity.terminal("the owner attached", ATTACHED);
            } else {
                activity.terminal("the owner detached", DETACHED);
            }
        }
    }

    /// Type `input` into the terminal. Returns SCV's note when it could not
    /// all be delivered.
    pub(crate) async fn input(&self, input: &[u8]) -> Option<String> {
        if input.is_empty() {
            return None;
        }
        if !self.is_running() {
            return Some("The terminal has closed; nothing was typed into it.".into());
        }
        let taken = self
            .pty
            .type_input(input, Instant::now() + INPUT_GRACE)
            .await;
        (taken < input.len()).then(|| {
            format!(
                "{} did not read its input within {} seconds; {} of {} bytes typed were dropped.",
                self.agent,
                INPUT_GRACE.as_secs(),
                input.len() - taken,
                input.len()
            )
        })
    }

    /// Give the terminal a new size, within bounds.
    pub(crate) fn resize(&self, cols: Option<u16>, rows: Option<u16>) {
        if cols.is_none() && rows.is_none() {
            return;
        }
        let mut screen = lock(&self.screen);
        let (current_rows, current_cols) = screen.parser.screen().size();
        let (cols, rows) = clamp_size(cols.or(Some(current_cols)), rows.or(Some(current_rows)));
        if (cols, rows) == (current_cols, current_rows) {
            return;
        }
        screen.parser.set_size(rows, cols);
        screen.generation += 1;
        screen.resized = screen.generation;
        screen.rows.clear();
        screen.dirty = true;
        drop(screen);
        if let Err(error) = self.pty.resize(cols, rows) {
            tracing::debug!(%error, "could not resize a takeover's terminal");
        }
    }

    /// The screen as client `id` should see it, given that it has
    /// generation `after`: the rows that changed since, or all of them.
    pub(crate) fn page(&self, id: &str, after: u64) -> AttachPage {
        let mut screen = lock(&self.screen);
        screen.refresh();
        let full = after == 0 || after < screen.resized || after > screen.generation;
        let lines = screen.rows_since(if full { None } else { Some(after) });
        let view = screen.parser.screen();
        let (rows, cols) = view.size();
        let cursor = (!view.hide_cursor()).then(|| {
            let (row, col) = view.cursor_position();
            [row, col]
        });
        let page = AttachPage {
            id: id.to_owned(),
            session: self.session.clone(),
            conversation: self.conversation.clone(),
            agent: self.agent.clone(),
            run: self.run.clone(),
            generation: screen.generation,
            cols,
            rows,
            lines,
            full,
            cursor,
            application_cursor: view.application_cursor(),
            bracketed_paste: view.bracketed_paste(),
            ended: self.is_finished(),
            exit: lock(&self.ended).clone(),
            note: None,
        };
        drop(screen);
        page
    }

    /// End the takeover: hang its terminal up, which asks the agent's UI to
    /// save and exit, give it [`STOP_GRACE`], then stop its process group,
    /// and wait until the conversation is let go.
    pub(crate) async fn end(&self) {
        self.stopping.store(true, Ordering::Release);
        let mut life = self.life.clone();
        if *life.borrow() == Life::Running
            && let Some(group) = ProcessGroup::new(self.pid)
        {
            group.signal(libc::SIGHUP);
            group.signal(libc::SIGCONT);
        }
        if tokio::time::timeout(STOP_GRACE, life.wait_for(|state| *state != Life::Running))
            .await
            .is_err()
            && let Some(group) = ProcessGroup::new(self.pid)
        {
            group.signal(libc::SIGKILL);
        }
        let _ = tokio::time::timeout(
            STOP_GRACE * 2 + Duration::from_millis(500),
            life.wait_for(|state| *state == Life::Finished),
        )
        .await;
    }
}

impl Takeover {
    /// Stop the agent's process group at once, without waiting, where
    /// there is no runtime to wait in. A group whose leader was collected
    /// is left alone: its number may be another's by now, and the reaper
    /// stopped what was left of it.
    pub(crate) fn kill_now(&self) {
        self.stopping.store(true, Ordering::Release);
        if self.is_running()
            && let Some(group) = ProcessGroup::new(self.pid)
        {
            group.signal(libc::SIGKILL);
        }
    }

    /// Give the agent's output to its screen, a slice at a time so a read
    /// of the screen never waits long, and return SCV's answers to the
    /// queries in it. Should the parser fail on it, the screen takes no
    /// more and the terminal's I/O ends, which ends the takeover.
    fn take_output(&self, output: &[u8]) -> Vec<u8> {
        let mut replies = Vec::new();
        for slice in output.chunks(PARSE_SLICE) {
            let mut screen = lock(&self.screen);
            if screen.broken {
                break;
            }
            let Ok(reply) = std::panic::catch_unwind(AssertUnwindSafe(|| screen.process(slice)))
            else {
                screen.broken = true;
                drop(screen);
                tracing::warn!(
                    agent = %self.agent,
                    "a takeover's terminal output could not be parsed; ending it"
                );
                *lock(&self.failed) =
                    Some("SCV could not keep its screen, so it was ended".to_owned());
                self.pty.stop();
                break;
            };
            replies.extend(reply);
        }
        replies
    }
}

/// Own the agent's process for its whole life: collect it the moment it
/// exits, stop the rest of its group and what is tagged with its run, and
/// only then let the conversation go.
async fn reap(
    takeover: Arc<Takeover>,
    mut child: tokio::process::Child,
    guard: Option<records::DelegationGuard>,
    held: (TakeoverHold, Place),
    life: watch::Sender<Life>,
) {
    let status = child.wait().await;
    let _ = life.send(Life::Exited);
    let failed = lock(&takeover.failed).clone();
    let how = if let Some(failed) = failed {
        failed
    } else if takeover.stopping.load(Ordering::Acquire) {
        "ended from SCV".to_owned()
    } else {
        match status {
            Ok(status) => match (
                status.code(),
                std::os::unix::process::ExitStatusExt::signal(&status),
            ) {
                (Some(code), _) => format!("exit {code}"),
                (None, Some(signal)) => format!("killed by signal {signal}"),
                (None, None) => "exited".to_owned(),
            },
            Err(error) => format!("lost: {error}"),
        }
    };
    // What it started in its terminal goes with it.
    if group_exists(takeover.pid)
        && let Some(group) = ProcessGroup::new(takeover.pid)
    {
        group.signal(libc::SIGKILL);
    }
    records::untrack_spawned(takeover.pid);
    if let Some(guard) = guard {
        guard.finish().await;
    }
    if let Some(activity) = &takeover.activity {
        activity.terminal_closed(&how);
    }
    *lock(&takeover.ended) = Some(how);
    // Its I/O ends with it, even if a stray process keeps the terminal open.
    takeover.pty.stop();
    drop(held);
    let _ = life.send(Life::Finished);
}

/// A terminal query the agent's UI made, which SCV answers as a
/// terminal would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Query {
    /// `CSI 6 n`: where the cursor is.
    CursorPosition,
    /// `CSI 5 n`: whether the terminal is well.
    Status,
    /// `CSI c` or `CSI 0 c`: what kind of terminal this is.
    Attributes,
}

/// Where the agent's output needs SCV's attention between two parts it
/// gives the parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    /// A query to answer as of the output before it.
    Query(Query),
    /// The cursor was restored (`ESC 8`, or leaving the alternate screen),
    /// maybe to where it was before the screen shrank.
    Restored,
}

/// Reads the agent's output on its way to the parser, across reads, in
/// step with the parser's own states (vte's): it finds the queries SCV
/// answers and the cursor restores it checks, and bounds the counts of the
/// sequences whose cost grows with their count (inserting, deleting, or
/// erasing lines and characters, and scrolling) by the screen's size,
/// which changes nothing they do.
#[derive(Debug, Default)]
struct Scanner {
    state: Scan,
    /// The control sequence read so far, `ESC [` and what followed.
    held: Vec<u8>,
    /// The last byte given on was not ASCII, so it may have left a UTF-8
    /// character unfinished.
    high: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Scan {
    #[default]
    Ground,
    /// An `ESC` was read and not given on yet.
    Escape,
    /// An escape sequence's intermediate bytes, given on as they come.
    Intermediate,
    Sequence,
    /// A sequence too long to be any a terminal UI sends: it is dropped
    /// up to its end, its controls kept.
    Dropping,
}

/// The longest control sequence SCV holds. A real one is shorter (vte
/// reads at most 32 parameters); a longer one is dropped.
const MAX_HELD: usize = 256;

/// Whether the parser carries `byte` out as a control wherever it comes
/// (all C0 controls but `CAN`, `SUB`, and `ESC`, which end a sequence).
fn is_control(byte: u8) -> bool {
    matches!(byte, 0x00..=0x17 | 0x19 | 0x1c..=0x1f)
}

impl Scanner {
    /// What of `bytes` goes to the parser, with the marks in it, each with
    /// the position just after its sequence. `size` is the screen's, as
    /// rows and columns.
    fn scan(&mut self, bytes: &[u8], size: (u16, u16)) -> (Vec<u8>, Vec<(usize, Mark)>) {
        let mut out = Vec::with_capacity(bytes.len() + 8);
        let mut marks = Vec::new();
        for &byte in bytes {
            match self.state {
                Scan::Ground if byte == 0x1b => {
                    // The parser drops the byte that cuts a UTF-8
                    // character short: let that be a NUL, which it
                    // otherwise ignores, never the `ESC`.
                    if std::mem::take(&mut self.high) {
                        out.push(0);
                    }
                    self.state = Scan::Escape;
                }
                Scan::Ground => {
                    self.high = byte >= 0x80;
                    out.push(byte);
                }
                Scan::Escape => match byte {
                    b'[' => {
                        self.held.clear();
                        self.held.extend_from_slice(b"\x1b[");
                        self.state = Scan::Sequence;
                    }
                    // Carried out, and the escape goes on.
                    _ if is_control(byte) => out.push(byte),
                    // Ignored, and the escape goes on.
                    0x7f..=0xff => {}
                    0x1b => out.push(0x1b),
                    0x20..=0x2f => {
                        out.extend_from_slice(&[0x1b, byte]);
                        self.state = Scan::Intermediate;
                    }
                    _ => {
                        out.extend_from_slice(&[0x1b, byte]);
                        if byte == b'8' {
                            marks.push((out.len(), Mark::Restored));
                        }
                        self.state = Scan::Ground;
                    }
                },
                Scan::Intermediate => match byte {
                    0x1b => self.state = Scan::Escape,
                    0x18 | 0x1a | 0x30..=0x7e => {
                        out.push(byte);
                        self.state = Scan::Ground;
                    }
                    _ => out.push(byte),
                },
                Scan::Sequence => match byte {
                    0x40..=0x7e => {
                        self.held.push(byte);
                        self.state = Scan::Ground;
                        if let Some(mark) = finish(&self.held, size, &mut out) {
                            marks.push((out.len(), mark));
                        }
                        self.held.clear();
                    }
                    // `ESC`, `CAN`, or `SUB` ends it unfinished: the parser
                    // has it as it came.
                    0x18 | 0x1a | 0x1b => {
                        out.append(&mut self.held);
                        if byte == 0x1b {
                            self.state = Scan::Escape;
                        } else {
                            out.push(byte);
                            self.state = Scan::Ground;
                        }
                    }
                    // Parameters, controls carried out inside it, and
                    // bytes the parser ignores there.
                    _ if self.held.len() < MAX_HELD => self.held.push(byte),
                    _ => {
                        out.extend(self.held.iter().copied().filter(|held| is_control(*held)));
                        self.held.clear();
                        self.state = Scan::Dropping;
                        if is_control(byte) {
                            out.push(byte);
                        }
                    }
                },
                Scan::Dropping => match byte {
                    0x40..=0x7e => self.state = Scan::Ground,
                    0x18 | 0x1a => {
                        out.push(byte);
                        self.state = Scan::Ground;
                    }
                    0x1b => self.state = Scan::Escape,
                    _ if is_control(byte) => out.push(byte),
                    _ => {}
                },
            }
        }
        (out, marks)
    }
}

/// Give on the control sequence `sequence` (`ESC [`, its parameters, and
/// its final byte), its counts bounded by the screen's `(rows, cols)`, and
/// say what it asks of SCV.
fn finish(sequence: &[u8], (rows, cols): (u16, u16), out: &mut Vec<u8>) -> Option<Mark> {
    let (&last, body) = sequence[2..].split_last()?;
    // What the parser reads as its parameters: it carries controls out
    // and ignores `DEL` and bytes past ASCII.
    let params: Vec<u8> = body
        .iter()
        .copied()
        .filter(|byte| (0x20..0x7f).contains(byte))
        .collect();
    let mark = match (params.as_slice(), last) {
        (b"6", b'n') => Some(Mark::Query(Query::CursorPosition)),
        (b"5", b'n') => Some(Mark::Query(Query::Status)),
        (b"" | b"0", b'c') => Some(Mark::Query(Query::Attributes)),
        ([b'?', modes @ ..], b'l')
            if modes
                .split(|byte| *byte == b';')
                .any(|mode| mode == b"1048" || mode == b"1049") =>
        {
            Some(Mark::Restored)
        }
        _ => None,
    };
    let bound = match last {
        b'L' | b'M' | b'S' | b'T' => rows,
        b'@' | b'P' | b'X' => cols,
        _ => {
            out.extend_from_slice(sequence);
            return mark;
        }
    };
    // Its controls first, as the parser carries them out before the
    // sequence takes effect; then each number at most `bound`.
    out.extend(body.iter().copied().filter(|byte| is_control(*byte)));
    out.extend_from_slice(b"\x1b[");
    let mut number: Option<u32> = None;
    for &byte in params.iter().chain(std::iter::once(&last)) {
        if byte.is_ascii_digit() {
            let digit = u32::from(byte - b'0');
            number = Some(number.unwrap_or(0).saturating_mul(10).saturating_add(digit));
            continue;
        }
        if let Some(value) = number.take() {
            out.extend_from_slice(value.min(u32::from(bound)).to_string().as_bytes());
        }
        out.push(byte);
    }
    mark
}

/// Output on which the parser fails, for tests of what follows.
#[cfg(test)]
const FAIL_FOR_TESTS: &[u8] = b"\x1b]scv-test-parser-failure\x07";

/// The most agent output given to the parser at once, so a read of the
/// screen never waits long behind output that is costly to parse (an
/// erase of the whole screen every few bytes).
const PARSE_SLICE: usize = 512;

/// The screen SCV keeps of a takeover's terminal.
struct Screen {
    parser: vt100::Parser,
    scanner: Scanner,
    /// The parser failed on the agent's output: it is given no more.
    broken: bool,
    /// Output arrived since rows were last compared.
    dirty: bool,
    /// Bumped whenever a comparison finds rows changed.
    generation: u64,
    /// Each row as clients see it, and the generation it last changed in.
    rows: Vec<(AttachRow, u64)>,
    /// The generation of the last resize: a client behind it starts over.
    resized: u64,
}

impl Screen {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            scanner: Scanner::default(),
            broken: false,
            dirty: true,
            generation: 0,
            rows: Vec::new(),
            resized: 0,
        }
    }

    /// Take in the agent's output, returning SCV's answers to the queries
    /// in it, each as of the output before it.
    fn process(&mut self, bytes: &[u8]) -> Vec<u8> {
        #[cfg(test)]
        assert!(
            !bytes
                .windows(FAIL_FOR_TESTS.len())
                .any(|window| window == FAIL_FOR_TESTS),
            "a parser failure, for tests"
        );
        let (rows, cols) = self.parser.screen().size();
        let (bytes, marks) = self.scanner.scan(bytes, (rows, cols));
        let mut replies = Vec::new();
        let mut done = 0;
        for (end, mark) in marks {
            self.parser.process(&bytes[done..end]);
            done = end;
            match mark {
                Mark::Query(Query::CursorPosition) => {
                    let (row, col) = self.parser.screen().cursor_position();
                    replies.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
                }
                Mark::Query(Query::Status) => replies.extend_from_slice(b"\x1b[0n"),
                Mark::Query(Query::Attributes) => replies.extend_from_slice(DEVICE_ATTRIBUTES),
                Mark::Restored => self.keep_cursor_on_screen(),
            }
        }
        self.parser.process(&bytes[done..]);
        self.dirty = true;
        replies
    }

    /// Put a cursor restored to where it was before the screen shrank back
    /// on the screen, as a terminal does: vt100 0.15 keeps the saved
    /// position as it was, and the next character drawn there would fail.
    fn keep_cursor_on_screen(&mut self) {
        let view = self.parser.screen();
        let (rows, cols) = view.size();
        let (row, col) = view.cursor_position();
        if row >= rows || col >= cols {
            let back = format!("\x1b[{};{}H", row.min(rows - 1) + 1, col.min(cols - 1) + 1);
            self.parser.process(back.as_bytes());
        }
    }

    /// Compare the rows with what clients were last shown, after output
    /// arrived.
    fn refresh(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let shown = render(self.parser.screen());
        let next = self.generation + 1;
        let mut changed = self.rows.len() != shown.len();
        if changed {
            self.rows = shown.into_iter().map(|row| (row, next)).collect();
        } else {
            for (kept, row) in self.rows.iter_mut().zip(shown) {
                if kept.0 != row {
                    *kept = (row, next);
                    changed = true;
                }
            }
        }
        if changed {
            self.generation = next;
        }
    }

    /// The rows changed since generation `after` (every row with `None`).
    fn rows_since(&self, after: Option<u64>) -> Vec<AttachRow> {
        self.rows
            .iter()
            .filter(|(_, at)| after.is_none_or(|after| *at > after))
            .map(|(row, _)| row.clone())
            .collect()
    }
}

/// The whole screen as clients see it: each row's styled text, with what
/// looks like a credential masked. Masking reads each line as one text
/// across the rows it runs over, and a line that ends in a hidden value,
/// or in a name whose value is still to come, hides the first word of the
/// next: a UI that wraps a long value onto an indented row is masked too.
fn render(view: &vt100::Screen) -> Vec<AttachRow> {
    let (rows, cols) = view.size();
    let mut shown = Vec::with_capacity(usize::from(rows));
    let mut hidden = false;
    for line in logical_lines(view, rows, cols) {
        let (line_rows, ends_hidden) = masked_rows(view, line, cols, hidden);
        shown.extend(line_rows);
        hidden = ends_hidden;
    }
    shown
}

/// Rows that make one line: a row the terminal wrapped into the next, or
/// that runs to its last column into a next row that starts with text (a
/// UI that wraps its own lines), continues on the next. A space printed
/// there is no text: an indented next row is a line of its own, which
/// [`render`] reads as going on from this one where it should.
fn logical_lines(view: &vt100::Screen, rows: u16, cols: u16) -> Vec<Range<u16>> {
    let filled = |row: u16, col: u16| {
        view.cell(row, col)
            .is_some_and(|cell| !cell.contents().trim().is_empty())
    };
    let mut lines = Vec::new();
    let mut start = 0;
    for row in 0..rows {
        let joins = row + 1 < rows
            && (view.row_wrapped(row)
                || (filled(row, cols.saturating_sub(1)) && filled(row + 1, 0)));
        if !joins {
            lines.push(start..row + 1);
            start = row + 1;
        }
    }
    lines
}

/// How near its right edge a row's last hidden value must end for SCV to
/// take the next line's first word for the rest of it, wrapped by a UI
/// (which may draw a frame or leave a margin there).
const WRAP_MARGIN: u16 = 3;

/// The rows of one line, with what looks like a credential masked: its
/// first cell shows `…` and the rest are blank. With `hidden`, its first
/// word goes on from the line before: the value of a name that line ended
/// with, or the rest of a hidden value wrapped from it. Also returns
/// whether the next line's first word goes on from this one so.
fn masked_rows(
    view: &vt100::Screen,
    line: Range<u16>,
    cols: u16,
    hidden: bool,
) -> (Vec<AttachRow>, bool) {
    // The line's text, and where each cell's characters are in it.
    let mut text = String::new();
    let mut cells: Vec<(u16, u16, Range<usize>)> = Vec::new();
    for row in line.clone() {
        for col in 0..cols {
            let Some(cell) = view.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let start = text.len();
            if cell.has_contents() {
                text.push_str(&cell.contents());
            } else {
                text.push(' ');
            }
            cells.push((row, col, start..text.len()));
        }
    }
    let (secrets, waiting) = progress::scrubbed_ranges(&text, hidden);
    // A hidden value that runs to the line's last text at its right edge
    // may go on in the next line.
    let shown = text.trim_end_matches(' ').len();
    let wrapped = secrets.last().is_some_and(|secret| {
        shown > 0
            && secret.end >= shown
            && cells.iter().any(|(_, col, range)| {
                range.start < shown && shown <= range.end && *col + WRAP_MARGIN >= cols
            })
    });
    let ends_hidden = waiting || wrapped;
    let masked = |range: &Range<usize>| {
        secrets
            .iter()
            .find(|secret| secret.start < range.end && range.start < secret.end)
    };
    let mut rows: Vec<AttachRow> = line
        .clone()
        .map(|row| AttachRow {
            row,
            spans: Vec::new(),
        })
        .collect();
    for (row, col, range) in &cells {
        let Some(cell) = view.cell(*row, *col) else {
            continue;
        };
        let style = Style::of(cell);
        let shown: String = match masked(range) {
            Some(secret) => {
                let mark = if range.start <= secret.start {
                    "…"
                } else {
                    " "
                };
                if cell.is_wide() {
                    format!("{mark} ")
                } else {
                    mark.to_owned()
                }
            }
            None if cell.has_contents() => cell.contents(),
            None => " ".to_owned(),
        };
        let spans = &mut rows[usize::from(row - line.start)].spans;
        match spans.last_mut() {
            Some(last) if Style::of_span(last) == style => last.text.push_str(&shown),
            _ => spans.push(style.span(shown)),
        }
    }
    for row in &mut rows {
        // Blank cells after the last text are left out.
        while let Some(last) = row.spans.last_mut() {
            if Style::of_span(last) != Style::default() {
                break;
            }
            let trimmed = last.text.trim_end_matches(' ').len();
            last.text.truncate(trimmed);
            if !last.text.is_empty() {
                break;
            }
            row.spans.pop();
        }
    }
    (rows, ends_hidden)
}

/// A cell's look.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
struct Style {
    fg: Option<AttachColor>,
    bg: Option<AttachColor>,
    bold: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl Style {
    fn of(cell: &vt100::Cell) -> Self {
        Self {
            fg: color(cell.fgcolor()),
            bg: color(cell.bgcolor()),
            bold: cell.bold(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }

    fn of_span(span: &AttachSpan) -> Self {
        Self {
            fg: span.fg,
            bg: span.bg,
            bold: span.bold,
            italic: span.italic,
            underline: span.underline,
            inverse: span.inverse,
        }
    }

    fn span(self, text: String) -> AttachSpan {
        AttachSpan {
            text,
            fg: self.fg,
            bg: self.bg,
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
            inverse: self.inverse,
        }
    }
}

fn color(color: vt100::Color) -> Option<AttachColor> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(index) => Some(AttachColor::Index(index)),
        vt100::Color::Rgb(red, green, blue) => Some(AttachColor::Rgb([red, green, blue])),
    }
}

#[cfg(test)]
mod tests;
