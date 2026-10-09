//! `scv exec`: one prompt, answered by a private `scv server --stdio`, with
//! the answer on stdout and everything else on stderr.

use std::{
    collections::HashSet,
    io::{Write as _, stdout},
    path::Path,
};

use anyhow::{Result, anyhow, bail};
use scv_protocol::{ClientMessage, ServerEvent, describe_reports, outcome_notice};

use crate::client::{Client, LaunchOptions, new_id};

/// Run `prompt` in `cwd` and print the answer. Tool approvals are answered
/// with `approve_risky`; background jobs the turn starts are waited for, and
/// their reports printed: the model's, or the agent's own reply when the
/// model could not report them.
pub async fn run_exec(
    cwd: &Path,
    prompt: String,
    approve_risky: bool,
    options: LaunchOptions,
) -> Result<()> {
    let (mut client, session) = Client::spawn(cwd, &options).await?;
    let result: Result<()> = async {
        let request_id = new_id();
        client
            .send(&ClientMessage::TurnStart {
                request_id: request_id.clone(),
                session_id: session.id.clone(),
                prompt,
                attachments: Vec::new(),
            })
            .await?;
        let mut printed_delta = false;
        let mut own_done = false;
        // Background jobs started in this session keep it open until they
        // are settled, since closing the session cancels them.
        let mut background: HashSet<String> = HashSet::new();
        let mut reporting: HashSet<String> = HashSet::new();
        // Reviewed jobs whose final outcome a `background.updated` gave.
        let mut updated: HashSet<String> = HashSet::new();
        loop {
            let event = client
                .read_event()
                .await?
                .ok_or_else(|| anyhow!("server disconnected before turn completion"))?;
            match event {
                ServerEvent::AssistantDelta { content, .. } => {
                    print!("{content}");
                    stdout().flush()?;
                    printed_delta = true;
                }
                ServerEvent::ApprovalRequested {
                    approval_id,
                    name,
                    summary,
                    ..
                } => {
                    if !printed_delta {
                        eprintln!("tool approval: {name}: {summary}");
                    }
                    client
                        .send(&ClientMessage::ApprovalResolve {
                            request_id: new_id(),
                            session_id: session.id.clone(),
                            approval_id,
                            approved: approve_risky,
                        })
                        .await?;
                }
                ServerEvent::ToolProgress { text, .. } => {
                    for line in text.lines() {
                        eprintln!("  ↳ {line}");
                    }
                }
                ServerEvent::ToolCompleted {
                    name,
                    success,
                    output,
                    jobs,
                    ..
                } => {
                    if !success {
                        eprintln!("\n{name} failed: {output}");
                    }
                    for change in jobs {
                        // A job whose journal is still open is settled by
                        // its `background.updated`, unless that came first.
                        let pending = change
                            .outcome
                            .as_ref()
                            .is_some_and(|outcome| outcome.review.journal_pending);
                        if pending && updated.contains(&change.job) {
                            continue;
                        }
                        if let Some(outcome) = &change.outcome {
                            eprintln!("{}", outcome_notice(outcome));
                        }
                        if change.started() {
                            background.insert(change.job);
                        } else if !pending {
                            background.remove(&change.job);
                        }
                    }
                }
                ServerEvent::TurnStarted {
                    request_id: started,
                    origin: Some(origin),
                    ..
                } => {
                    if printed_delta {
                        println!();
                        printed_delta = false;
                    }
                    eprintln!("[background report: {}]", origin.jobs.join(", "));
                    for outcome in &origin.outcomes {
                        eprintln!("{}", outcome_notice(outcome));
                    }
                    reporting.insert(started);
                }
                ServerEvent::BackgroundReported {
                    code,
                    message,
                    reports,
                    ..
                } => {
                    if printed_delta {
                        println!();
                        printed_delta = false;
                    }
                    let follows = if reports.iter().any(|report| report.outcome.is_some()) {
                        "SCV's Review and Landing lines and the agent's reply follow"
                    } else {
                        "the agent's reply follows"
                    };
                    eprintln!(
                        "[background report could not be written by the model ({code}): {message}; \
                         {follows}]"
                    );
                    print!("{}", describe_reports(&reports));
                    stdout().flush()?;
                    for report in &reports {
                        background.remove(&report.job);
                    }
                }
                ServerEvent::BackgroundUpdated { outcomes, .. } => {
                    for outcome in &outcomes {
                        eprintln!("{}", outcome_notice(outcome));
                        background.remove(&outcome.job);
                        updated.insert(outcome.job.clone());
                    }
                    if own_done && background.is_empty() && reporting.is_empty() {
                        break;
                    }
                }
                ServerEvent::TurnCompleted {
                    request_id: finished,
                    origin,
                    ..
                } => {
                    if printed_delta {
                        println!();
                        printed_delta = false;
                    }
                    for job in origin.iter().flat_map(|origin| &origin.jobs) {
                        background.remove(job);
                    }
                    reporting.remove(&finished);
                    if finished == request_id {
                        own_done = true;
                        if !background.is_empty() {
                            eprintln!(
                                "scv exec: waiting for {} background job(s) to report",
                                background.len()
                            );
                        }
                    }
                    if own_done && background.is_empty() && reporting.is_empty() {
                        break;
                    }
                }
                ServerEvent::TurnCancelled {
                    request_id: finished,
                    ..
                } if finished == request_id => bail!("turn cancelled"),
                ServerEvent::TurnFailed {
                    request_id: finished,
                    code,
                    message,
                    ..
                } if finished == request_id => {
                    bail!("turn failed ({code}): {message}")
                }
                ServerEvent::TurnCancelled {
                    request_id: finished,
                    origin,
                    ..
                } => {
                    eprintln!("[background report did not complete]");
                    for job in origin.iter().flat_map(|origin| &origin.jobs) {
                        background.remove(job);
                    }
                    reporting.remove(&finished);
                    if own_done && background.is_empty() && reporting.is_empty() {
                        break;
                    }
                }
                ServerEvent::TurnFailed {
                    request_id: finished,
                    code,
                    message,
                    origin,
                    ..
                } => {
                    if let Some(seconds) = origin.as_ref().and_then(|origin| origin.retry_seconds) {
                        eprintln!(
                            "[background report failed ({code}): {message}; SCV tries again in \
                             about {seconds} seconds]"
                        );
                    } else {
                        eprintln!("[background report did not complete ({code}): {message}]");
                        for job in origin.iter().flat_map(|origin| &origin.jobs) {
                            background.remove(job);
                        }
                    }
                    reporting.remove(&finished);
                    if own_done && background.is_empty() && reporting.is_empty() {
                        break;
                    }
                }
                ServerEvent::Error {
                    request_id: failed,
                    code,
                    message,
                    ..
                } if failed.is_none() || failed.as_deref() == Some(request_id.as_str()) => {
                    bail!("server error ({code}): {message}")
                }
                _ => {}
            }
        }
        Ok(())
    }
    .await;
    client.shutdown().await;
    result
}
