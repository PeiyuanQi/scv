//! `scv jobs` and `scv agents worktrees`: the workbench from the command
//! line. Listing and reading logs work from the records on disk when no
//! daemon runs; every action asks the daemon, which owns the jobs.
//! Delegated runs may look but not act: an agent must not start, steer,
//! approve, or stop the owner's jobs.

use std::{path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use scv_client::{ControlError, Layout};
use scv_protocol::{
    AgentState, DaemonCommand, RunEvent, RunEventKind, SendMode, WorkbenchJob, WorkbenchSnapshot,
};

use super::{args::JobsCommand, control};

/// How often `--follow` reads the log again.
const FOLLOW_EVERY: Duration = Duration::from_millis(500);

pub(crate) async fn run(layout: &Layout, command: Option<JobsCommand>, cwd: &Path) -> Result<()> {
    let command = command.unwrap_or(JobsCommand::List { all: false });
    if acts(&command) {
        refuse_delegated()?;
    }
    match command {
        JobsCommand::List { all } => {
            let snapshot = snapshot(layout).await?;
            print_snapshot(&snapshot, all);
            Ok(())
        }
        JobsCommand::Log { target, follow } => log(layout, &target, follow).await,
        JobsCommand::Start {
            agent,
            cwd: directory,
            model,
            effort,
            worktree,
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
            )
            .await
        }
        JobsCommand::Send {
            job,
            prompt,
            queue,
            steer,
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
            )
            .await
        }
        JobsCommand::Cancel { job } => {
            let job = find_job(&snapshot(layout).await?, &job)?;
            act(
                layout,
                DaemonCommand::WorkbenchCancel {
                    session: job.session,
                    job: job.job,
                },
            )
            .await
        }
        JobsCommand::Resume { job } => {
            let job = find_job(&snapshot(layout).await?, &job)?;
            act(
                layout,
                DaemonCommand::WorkbenchResume {
                    session: job.session,
                    job: job.job,
                },
            )
            .await
        }
        JobsCommand::Dismiss { job } => {
            let job = find_job(&snapshot(layout).await?, &job)?;
            act(
                layout,
                DaemonCommand::WorkbenchDismiss {
                    session: job.session,
                    job: job.job,
                },
            )
            .await
        }
        JobsCommand::Approve { id } => {
            act(
                layout,
                DaemonCommand::WorkbenchApprove { id, approved: true },
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
            )
            .await
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

/// Whether `command` acts on jobs rather than reading them.
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
             (delegation depth {depth}); `scv jobs` and `scv jobs log` still read"
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

async fn act(layout: &Layout, command: DaemonCommand) -> Result<()> {
    let status = control(layout, command).await.map_err(|error| {
        if unavailable(&error) {
            error.context("the workbench needs a running daemon of this release (`scv start`)")
        } else {
            error
        }
    })?;
    // A note can quote an agent's text, such as an approval's summary.
    if let Some(note) = status.workbench_note {
        println!("{}", inert(&note));
    }
    Ok(())
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

async fn log(layout: &Layout, target: &str, follow: bool) -> Result<()> {
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
                println!("{}", describe(event));
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
    let jobs: Vec<&WorkbenchJob> = snapshot
        .jobs
        .iter()
        .filter(|job| all || !job.state.is_final() || job.resumable)
        .collect();
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
    let runs: Vec<_> = snapshot.runs.iter().filter(|run| all || run.live).collect();
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
