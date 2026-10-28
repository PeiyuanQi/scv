//! `scv jobs` and `scv agents worktrees`: the workbench from the command
//! line. Listing and reading logs work from the records on disk when no
//! daemon runs; every action asks the daemon, which owns the jobs.
//! Delegated runs may look but not act: an agent must not start, steer,
//! approve, stop, wait on, or follow the owner's jobs.
//!
//! `--json` prints the protocol's own types, one object per line, for
//! scripts. Automation drives the workbench like the owner at a terminal:
//! it never asks for the workbench as a watching dashboard (`watch`), and
//! approving stays text-only, one request at a time.

use std::{
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use scv_client::{ControlError, Layout};
use scv_protocol::{
    AgentState, DaemonCommand, JobResult, MAX_JOB_RESULT_BYTES, RunEvent, RunEventKind, SendMode,
    WorkbenchJob, WorkbenchSnapshot,
};
use serde::Serialize;

use super::{args::JobsCommand, control};

mod events;

/// How often `--follow` reads the log again.
const FOLLOW_EVERY: Duration = Duration::from_millis(500);
/// How often `scv jobs wait` asks about its job again.
const WAIT_EVERY: Duration = Duration::from_secs(1);
/// `scv jobs wait`: the job ended, but not completed.
const WAIT_ENDED_OTHERWISE: i32 = 1;
/// `scv jobs wait`: the job had not ended when the wait timed out.
const WAIT_TIMED_OUT: i32 = 2;
/// `scv jobs wait`: refused (inside a delegated run, or by the daemon), no
/// such job, or no daemon answer to go on.
const WAIT_REFUSED: i32 = 3;
/// How many times in a row `scv jobs wait` asks again after the daemon was
/// slow or the exchange broke off, before it gives up.
const WAIT_RETRIES: u32 = 3;

pub(crate) async fn run(layout: &Layout, command: Option<JobsCommand>, cwd: &Path) -> Result<()> {
    let command = command.unwrap_or(JobsCommand::List {
        all: false,
        json: false,
    });
    // A refused wait still exits with its own status.
    let refused = match (acts(&command).then(refuse_delegated), &command) {
        (Some(Err(error)), JobsCommand::Wait { .. }) => Some(error),
        (Some(Err(error)), _) => return Err(error),
        _ => None,
    };
    match command {
        JobsCommand::List { all, json } => {
            let snapshot = listed(snapshot(layout).await?, all);
            if json {
                print_json(&snapshot)?;
            } else {
                print_snapshot(&snapshot, all);
            }
            Ok(())
        }
        JobsCommand::Log {
            target,
            follow,
            json,
        } => log(layout, &target, follow, json).await,
        JobsCommand::Start {
            agent,
            cwd: directory,
            model,
            effort,
            worktree,
            json,
            prompt,
        } => {
            let workspace = std::fs::canonicalize(cwd)
                .context("resolve the current directory")?
                .display()
                .to_string();
            act(
                layout,
                DaemonCommand::WorkbenchStart {
                    workspace,
                    agent,
                    prompt,
                    cwd: directory,
                    model,
                    effort,
                    worktree,
                },
                json,
            )
            .await
        }
        JobsCommand::Send {
            job,
            prompt,
            queue,
            steer,
            json,
        } => {
            let snapshot = snapshot(layout).await?;
            let job = find_job(&snapshot, &job)?;
            let conversation = job
                .conversation
                .clone()
                .with_context(|| format!("{} has no conversation to send to yet", job.job))?;
            act(
                layout,
                DaemonCommand::WorkbenchSend {
                    session: job.session.clone(),
                    conversation,
                    prompt,
                    mode: if queue {
                        SendMode::Queue
                    } else if steer {
                        SendMode::Steer
                    } else {
                        SendMode::Auto
                    },
                },
                json,
            )
            .await
        }
        JobsCommand::Cancel { job, json } => {
            let job = find_job(&snapshot(layout).await?, &job)?;
            act(
                layout,
                DaemonCommand::WorkbenchCancel {
                    session: job.session,
                    job: job.job,
                },
                json,
            )
            .await
        }
        JobsCommand::Resume { job, json } => {
            let job = find_job(&snapshot(layout).await?, &job)?;
            act(
                layout,
                DaemonCommand::WorkbenchResume {
                    session: job.session,
                    job: job.job,
                },
                json,
            )
            .await
        }
        JobsCommand::Dismiss { job, json } => {
            let job = find_job(&snapshot(layout).await?, &job)?;
            act(
                layout,
                DaemonCommand::WorkbenchDismiss {
                    session: job.session,
                    job: job.job,
                },
                json,
            )
            .await
        }
        JobsCommand::Wait { job, timeout, json } => {
            let code = match refused {
                Some(error) => {
                    eprintln!("Error: {error:#}");
                    WAIT_REFUSED
                }
                None => wait(layout, &job, timeout, json).await,
            };
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        JobsCommand::Result { job, json } => result(layout, &job, json).await,
        JobsCommand::Events { follow, json } => events::run(layout, follow, json).await,
        JobsCommand::Approve { id } => {
            act(
                layout,
                DaemonCommand::WorkbenchApprove { id, approved: true },
                false,
            )
            .await
        }
        JobsCommand::Deny { id } => {
            act(
                layout,
                DaemonCommand::WorkbenchApprove {
                    id,
                    approved: false,
                },
                false,
            )
            .await
        }
        JobsCommand::Attach { job, end } => {
            let job = find_job(&snapshot(layout).await?, &job)?;
            let conversation = job
                .conversation
                .clone()
                .with_context(|| format!("{} has no conversation to attach to yet", job.job))?;
            if end {
                return act(
                    layout,
                    DaemonCommand::AttachEnd {
                        session: job.session,
                        conversation,
                    },
                    false,
                )
                .await;
            }
            use std::io::IsTerminal as _;
            if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                bail!("`scv jobs attach` needs a terminal; `--end` ends a takeover without one");
            }
            let note = scv_tui::run_attach(&layout.socket(), &job.session, &conversation).await?;
            println!("{}", inert(&note));
            Ok(())
        }
    }
}

/// `scv agents worktrees`: list them, or remove one.
pub(crate) async fn worktrees(layout: &Layout, remove: Option<&Path>, force: bool) -> Result<()> {
    if let Some(path) = remove {
        refuse_delegated()?;
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        // A daemon removes it with what it knows of its own jobs; without
        // one, the records on disk say what still works in it.
        let command = DaemonCommand::WorktreeRemove {
            path: path.display().to_string(),
            force,
        };
        match control(layout, command).await {
            Ok(status) => {
                if let Some(note) = status.workbench_note {
                    println!("{}", inert(&note));
                }
                return Ok(());
            }
            Err(error) if unavailable(&error) => {}
            Err(error) => return Err(error),
        }
        let registry = std::sync::Arc::new(scv_tools::delegation::DelegationRegistry::new(layout));
        scv_tools::workbench::Workbench::new(layout, registry)
            .check_worktree_unused(&path)
            .map_err(anyhow::Error::msg)?;
        let note = scv_tools::worktree::remove(&layout.worktrees(), &path, force)
            .await
            .map_err(anyhow::Error::msg)?;
        // The note names the worktree's branch, which its agent could set.
        println!("{}", inert(&note));
        return Ok(());
    }
    let found = scv_tools::worktree::list(&layout.worktrees()).await;
    if found.is_empty() {
        println!("SCV has created no worktrees.");
        return Ok(());
    }
    let records = scv_tools::background::ledger::list(&layout.jobs());
    for info in found {
        let dirty = match info.dirty {
            Some(true) => "uncommitted changes",
            Some(false) => "clean",
            None => "status unknown",
        };
        println!("{} ({dirty})", inert(&info.worktree.branch));
        println!("  {:?}", info.worktree.path);
        println!("  from {:?}", info.worktree.repo);
        let jobs: Vec<String> = records
            .iter()
            .filter(|record| {
                record
                    .worktree
                    .as_ref()
                    .is_some_and(|worktree| worktree.path == info.worktree.path)
            })
            .map(|record| format!("{}/{}", short(&record.session), record.job))
            .collect();
        if !jobs.is_empty() {
            println!("  jobs {}", jobs.join(", "));
        }
    }
    Ok(())
}

/// Whether `command` is refused inside a delegated run: everything but
/// listing and reading logs. Waiting on, reading the result of, and
/// following the owner's jobs are automation for the owner, so they are
/// refused too.
fn acts(command: &JobsCommand) -> bool {
    !matches!(command, JobsCommand::List { .. } | JobsCommand::Log { .. })
}

/// The workbench acts for the owner; a delegated agent (or an SCV it
/// started) may only look. This is cooperative, like the daemon rules.
fn refuse_delegated() -> Result<()> {
    let depth = scv_tools::delegation::current_depth();
    if depth > 0 {
        bail!(
            "the workbench acts for the owner, so this is refused inside a delegated agent run \
             (delegation depth {depth}); `scv jobs list` and `scv jobs log` still read"
        );
    }
    Ok(())
}

/// The daemon's snapshot, or one built from the records on disk when no
/// daemon runs.
async fn snapshot(layout: &Layout) -> Result<WorkbenchSnapshot> {
    match control(layout, DaemonCommand::Workbench { watch: false }).await {
        Ok(status) => status
            .workbench
            .context("the daemon answered without a workbench"),
        Err(error) if unavailable(&error) => {
            let registry =
                std::sync::Arc::new(scv_tools::delegation::DelegationRegistry::new(layout));
            let workbench = scv_tools::workbench::Workbench::new(layout, registry);
            let mut snapshot = workbench.snapshot();
            snapshot.restorable = scv_server::restorable_queues(layout);
            Ok(snapshot)
        }
        Err(error) => Err(error),
    }
}

/// Whether the daemon is not running, or predates the workbench.
fn unavailable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ControlError>()
        .is_some_and(|error| match error {
            ControlError::Unavailable(_) => true,
            ControlError::Server { code, .. } => *code == scv_protocol::ErrorCode::InvalidJson,
            _ => false,
        })
}

/// Send `command` and print what the daemon did: its note, or with
/// `json` its whole reply.
async fn act(layout: &Layout, command: DaemonCommand, json: bool) -> Result<()> {
    let status = control(layout, command).await.map_err(|error| {
        if unavailable(&error) {
            error.context("the workbench needs a running daemon of this release (`scv start`)")
        } else {
            error
        }
    })?;
    if json {
        return print_json(&status);
    }
    // A note can quote an agent's text, such as an approval's summary.
    if let Some(note) = status.workbench_note {
        println!("{}", inert(&note));
    }
    Ok(())
}

/// `scv jobs wait`: follow the job `reference` names until it ends or
/// `timeout` seconds pass, print where it stands, and return the exit
/// status ([`wait_exit`], [`WAIT_TIMED_OUT`], [`WAIT_REFUSED`]).
async fn wait(layout: &Layout, reference: &str, timeout: Option<u64>, json: bool) -> i32 {
    let deadline = timeout.map(|seconds| Instant::now() + Duration::from_secs(seconds));
    let timed_out = || deadline.is_some_and(|deadline| Instant::now() >= deadline);
    let mut job = None;
    let mut last = None;
    let mut retries = 0;
    loop {
        // No request outlasts the wait by more than a pause; `None` when
        // one did not answer by then.
        let asked = match deadline {
            Some(deadline) => tokio::time::timeout(
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(WAIT_EVERY),
                ask(layout, reference, &mut job),
            )
            .await
            .ok(),
            None => Some(ask(layout, reference, &mut job).await),
        };
        match asked {
            None => {}
            Some(Ok(result)) => {
                if let Some(code) = wait_exit(result.state) {
                    print_wait(&result, json);
                    return code;
                }
                last = Some(result);
                retries = 0;
            }
            // A slow daemon, or an exchange that broke off, is asked again
            // a few times in a row.
            Some(Err(error)) if transient(&error) && retries < WAIT_RETRIES => retries += 1,
            Some(Err(error)) => {
                eprintln!("Error: {error:#}");
                return WAIT_REFUSED;
            }
        }
        if timed_out() {
            if let Some(result) = &last {
                print_wait(result, json);
            }
            eprintln!("Error: {reference} had not ended when the wait timed out");
            return WAIT_TIMED_OUT;
        }
        let pause = deadline.map_or(WAIT_EVERY, |deadline| {
            WAIT_EVERY.min(deadline.saturating_duration_since(Instant::now()))
        });
        tokio::time::sleep(pause).await;
    }
}

/// The exit status of a wait on a job in `state`: 0 completed, 1 any other
/// end, `None` while it goes on.
fn wait_exit(state: AgentState) -> Option<i32> {
    state
        .is_final()
        .then_some(if state == AgentState::Completed {
            0
        } else {
            WAIT_ENDED_OTHERWISE
        })
}

/// Print where a waited-on job stands. The exit status says how the wait
/// ended, so a failure to print is only reported.
fn print_wait(result: &JobResult, json: bool) {
    if json {
        if let Err(error) = print_json(result) {
            eprintln!("Error: {error:#}");
        }
        return;
    }
    println!(
        "{}",
        inert(&format!(
            "{}/{} {}{}",
            short(&result.session),
            result.job,
            result.state,
            result
                .detail
                .as_deref()
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default()
        ))
    );
}

/// Whether asking again may well succeed: the daemon was slow, or the
/// exchange broke off.
fn transient(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<ControlError>(),
        Some(ControlError::TimedOut | ControlError::Protocol(_))
    )
}

/// Where the job `reference` names stands; the first ask finds it.
async fn ask(
    layout: &Layout,
    reference: &str,
    job: &mut Option<WorkbenchJob>,
) -> Result<JobResult> {
    let job = match job {
        Some(job) => job,
        None => job.insert(find_job(&snapshot(layout).await?, reference)?),
    };
    job_result(layout, job).await
}

/// Listed job `job` and its bounded result, from the daemon. When no daemon
/// of this release runs, its state is the snapshot's (which a daemon before
/// `job_result` still gives, as it sees the job, or else the records on
/// disk) and its reply and error its record's.
async fn job_result(layout: &Layout, job: &WorkbenchJob) -> Result<JobResult> {
    let command = DaemonCommand::JobResult {
        session: job.session.clone(),
        job: job.job.clone(),
    };
    match control(layout, command).await {
        Ok(status) => status
            .job_result
            .context("the daemon answered without the job's result"),
        Err(error) if unavailable(&error) => {
            let listed = snapshot(layout)
                .await?
                .jobs
                .into_iter()
                .find(|listed| listed.session == job.session && listed.job == job.job)
                .with_context(|| {
                    format!("{}/{} is no longer listed", short(&job.session), job.job)
                })?;
            Ok(bounded(recorded(layout, listed)?))
        }
        Err(error) => Err(error),
    }
}

/// Listed job `job` with its reply and error from its record on disk,
/// whole (the ledger keeps up to 8 KiB of each).
fn recorded(layout: &Layout, job: WorkbenchJob) -> Result<JobResult> {
    let record = scv_tools::background::ledger::load(&layout.jobs(), &job.session, &job.job)
        .with_context(|| format!("no record of {}/{}", short(&job.session), job.job))?;
    Ok(JobResult {
        session: job.session,
        job: job.job,
        agent: job.agent,
        state: job.state,
        detail: job.detail,
        reply: record.reply,
        error: record.error,
        truncated: false,
        outcome: None,
        finished_unix_seconds: record.finished_unix,
    })
}

/// `result` with its reply and error cut as `job_result` cuts them.
fn bounded(mut result: JobResult) -> JobResult {
    for text in [&mut result.reply, &mut result.error].into_iter().flatten() {
        let kept = scv_client::text::utf8_prefix(text, MAX_JOB_RESULT_BYTES).len();
        if kept < text.len() {
            text.truncate(kept);
            result.truncated = true;
        }
    }
    result
}

/// `scv jobs result`: the job's state from the workbench, and its reply
/// and error from its record on disk (up to the ledger's 8 KiB each).
async fn result(layout: &Layout, reference: &str, json: bool) -> Result<()> {
    let result = recorded(layout, find_job(&snapshot(layout).await?, reference)?)?;
    if json {
        return print_json(&result);
    }
    let name = format!("{}/{}", short(&result.session), result.job);
    if !result.state.is_final() {
        bail!(
            "{name} is still {}; `scv jobs wait {name}` waits for it to end",
            result.state
        );
    }
    if let Some(reply) = &result.reply {
        println!("{}", inert_text(reply));
    }
    if result.state != AgentState::Completed {
        let why = result.error.as_deref().or(result.detail.as_deref());
        eprintln!(
            "{name} {}{}",
            result.state,
            why.map(|why| format!(": {}", inert_text(why)))
                .unwrap_or_default()
        );
    }
    Ok(())
}

/// The snapshot as `scv jobs list` shows it: unfinished and resumable jobs
/// and live runs, or with `all` everything.
fn listed(mut snapshot: WorkbenchSnapshot, all: bool) -> WorkbenchSnapshot {
    if !all {
        snapshot
            .jobs
            .retain(|job| !job.state.is_final() || job.resumable);
        snapshot.runs.retain(|run| run.live);
    }
    snapshot
}

/// `value` as one line of JSON. JSON escapes the C0 control characters in
/// strings but leaves DEL and the C1 controls as they are, which some
/// terminals obey (U+009B starts an escape sequence), so those are escaped
/// too: a script that prints an agent's reply as it came cannot have its
/// terminal driven by it.
fn json_line(value: &impl Serialize) -> Result<String> {
    let line = serde_json::to_string(value)?;
    // Compact JSON has no control character outside a string, so each is
    // inside one, where `\uXXXX` means the same.
    Ok(line
        .chars()
        .map(|character| {
            if character.is_control() {
                format!("\\u{:04x}", u32::from(character))
            } else {
                character.to_string()
            }
        })
        .collect())
}

fn print_json(value: &impl Serialize) -> Result<()> {
    emit(&json_line(value)?)?;
    Ok(())
}

/// Print `line` on stdout at once; `false` once nothing reads it.
fn emit(line: &str) -> Result<bool> {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    match writeln!(out, "{line}").and_then(|()| out.flush()) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// The job `reference` names: `job-3` when only one session has it, or
/// `<session prefix>/job-3`.
fn find_job(snapshot: &WorkbenchSnapshot, reference: &str) -> Result<WorkbenchJob> {
    let (session, job) = match reference.split_once('/') {
        Some((session, job)) => (Some(session), job),
        None => (None, reference),
    };
    let matches: Vec<&WorkbenchJob> = snapshot
        .jobs
        .iter()
        .filter(|candidate| candidate.job == job)
        .filter(|candidate| session.is_none_or(|session| candidate.session.starts_with(session)))
        .collect();
    match matches.as_slice() {
        [job] => Ok((*job).clone()),
        [] => bail!("no job {reference:?}; `scv jobs list --all` lists them"),
        _ => bail!(
            "more than one session has {job}; name it as <session prefix>/{job}: {}",
            matches
                .iter()
                .map(|job| format!("{}/{}", short(&job.session), job.job))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

async fn log(layout: &Layout, target: &str, follow: bool, json: bool) -> Result<()> {
    let snapshot = snapshot(layout).await?;
    let handles: Vec<String> = if scv_tools::activity::valid_handle(target)
        && (snapshot.runs.iter().any(|run| run.handle == target)
            || layout.runs().join(format!("{target}.jsonl")).is_file())
    {
        vec![target.to_owned()]
    } else {
        let job = find_job(&snapshot, target)?;
        let mut runs: Vec<_> = snapshot
            .runs
            .iter()
            .filter(|run| {
                run.session == job.session
                    && (run.job.as_deref() == Some(&job.job) || run.jobs.contains(&job.job))
            })
            .collect();
        runs.sort_by_key(|run| run.started_unix_seconds);
        runs.into_iter().map(|run| run.handle.clone()).collect()
    };
    if handles.is_empty() {
        bail!("{target} has no run with an activity log yet");
    }
    let dir = layout.runs();
    for handle in handles {
        let mut after = 0;
        loop {
            let page = scv_tools::activity::read_page(&dir, &handle, after, 500)
                .with_context(|| format!("read run {handle}"))?;
            for event in &page.events {
                if !json {
                    println!("{}", describe(event));
                } else if !emit(&json_line(&Logged {
                    run: &handle,
                    event,
                })?)? {
                    return Ok(());
                }
            }
            after = page.next;
            if page.more {
                continue;
            }
            if !follow || page.ended {
                break;
            }
            tokio::select! {
                () = tokio::time::sleep(FOLLOW_EVERY) => {}
                _ = tokio::signal::ctrl_c() => return Ok(()),
            }
        }
    }
    Ok(())
}

/// One event of run `run`'s log, as `scv jobs log --json` prints it.
#[derive(Serialize)]
struct Logged<'a> {
    run: &'a str,
    #[serde(flatten)]
    event: &'a RunEvent,
}

/// One log event as a line of text, control characters removed (from
/// every field: a nested SCV relays its own approvals' risk as it sent it).
fn describe(event: &RunEvent) -> String {
    let text = &event.text;
    inert(&match event.kind {
        RunEventKind::RunStarted => format!("· started: {text}"),
        RunEventKind::TurnStarted => format!(
            "── turn {}{} ── {text}",
            event.turn.unwrap_or(1),
            event
                .job
                .as_deref()
                .map(|job| format!(" · {job}"))
                .unwrap_or_default()
        ),
        RunEventKind::Text => text.clone(),
        RunEventKind::Tool => format!("  ● {text}"),
        RunEventKind::ToolFailed => format!("  ✗ {text}"),
        RunEventKind::Plan => format!("  plan: {text}"),
        RunEventKind::ApprovalRequested => format!(
            "? approval ({}): {text}",
            event.risk.as_deref().unwrap_or("?")
        ),
        RunEventKind::ApprovalResolved => {
            format!("  → {}", event.status.as_deref().unwrap_or("answered"))
        }
        RunEventKind::Steered => format!("▸ steered: {text}"),
        RunEventKind::TurnEnded => format!(
            "── turn {} ──{}",
            event.status.as_deref().unwrap_or("ended"),
            if text.is_empty() {
                String::new()
            } else {
                format!(" {text}")
            }
        ),
        RunEventKind::RunEnded => {
            format!("· process {}", event.status.as_deref().unwrap_or("ended"))
        }
        RunEventKind::Truncated => "· the log reached its size limit".into(),
        RunEventKind::Note | RunEventKind::Unknown => format!("· {text}"),
    })
}

/// `text` safe to print on one terminal line: every control character
/// (escape sequences included, which an agent's text could carry) becomes a
/// space.
fn inert(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// `text` safe to print on a terminal as lines: as [`inert`], but line
/// breaks stay.
fn inert_text(text: &str) -> String {
    text.split('\n').map(inert).collect::<Vec<_>>().join("\n")
}

fn print_snapshot(snapshot: &WorkbenchSnapshot, all: bool) {
    if !snapshot.approvals.is_empty() {
        println!("Waiting for your approval:");
        for approval in &snapshot.approvals {
            println!(
                "  {}  {}/{} ({}): {}",
                approval.id,
                short(&approval.session),
                approval.job,
                inert(&approval.risk),
                inert(&approval.summary)
            );
        }
        println!("  Answer with `scv jobs approve <id>` or `scv jobs deny <id>`.");
    }
    let jobs = &snapshot.jobs;
    if jobs.is_empty() {
        println!(
            "No {}background jobs.",
            if all { "" } else { "running or interrupted " }
        );
    } else {
        println!(
            "{:<18} {:<9} {:<15} {:<10} TASK",
            "JOB", "AGENT", "STATE", "BY"
        );
        for job in jobs {
            println!(
                "{:<18} {:<9} {:<15} {:<10} {}",
                format!("{}/{}", short(&job.session), job.job),
                job.agent,
                job.state.as_str(),
                match job.started_by {
                    scv_protocol::JobOrigin::Owner => "you",
                    _ => "model",
                },
                inert(&job.task)
            );
            if let Some(detail) = &job.detail {
                println!("{:<18} {}", "", inert(detail));
            }
            if let Some(worktree) = &job.worktree {
                println!(
                    "{:<18} worktree {} ({:?})",
                    "",
                    inert(&worktree.branch),
                    worktree.path
                );
            }
            if job.state == AgentState::Interrupted && job.resumable {
                println!(
                    "{:<18} resume with `scv jobs resume {}/{}`",
                    "",
                    short(&job.session),
                    job.job
                );
            }
        }
    }
    let runs = &snapshot.runs;
    if !runs.is_empty() {
        println!();
        println!(
            "{:<16} {:<9} {:<13} {:<15} DETAIL",
            "RUN", "AGENT", "CONVERSATION", "STATE"
        );
        for run in runs {
            println!(
                "{:<16} {:<9} {:<13} {:<15} {}",
                run.handle,
                run.agent,
                inert(run.conversation.as_deref().unwrap_or("-")),
                run.state.as_str(),
                inert(run.detail.as_deref().unwrap_or(""))
            );
        }
    }
    if !snapshot.restorable.is_empty() {
        println!();
        println!("Queues kept from sessions that stopped (the TUI restores them):");
        for session in &snapshot.restorable {
            println!(
                "  {}  {} queued  {:?}",
                short(&session.id),
                session.queued,
                session.workspace
            );
        }
    }
}

/// A session ID short enough to read and long enough to tell apart: eight
/// characters, or the workbench's prefix and six more.
fn short(session: &str) -> String {
    let keep = if session.starts_with("workbench-") {
        16
    } else {
        8
    };
    session.chars().take(keep).collect()
}

#[cfg(test)]
mod tests;
