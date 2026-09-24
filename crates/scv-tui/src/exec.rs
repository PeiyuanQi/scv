//! `scv exec`: one prompt, answered by a private `scv server --stdio`, with
//! the answer on stdout and everything else on stderr.

use std::{
    collections::HashSet,
    io::{Write as _, stdout},
    path::Path,
};

use anyhow::{Result, anyhow, bail};
use scv_protocol::{ClientMessage, ServerEvent};

use crate::client::{Client, LaunchOptions, new_id};

/// Run `prompt` in `cwd` and print the answer. Tool approvals are answered
/// with `approve_risky`; background jobs the turn starts are waited for.
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
        // are reported, since closing the session cancels them.
        let mut background: HashSet<String> = HashSet::new();
        let mut reporting: HashSet<String> = HashSet::new();
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
                    ..
                } => {
                    let update = scv_protocol::background_job_update(&output);
                    background.extend(update.started);
                    for job in update.settled {
                        background.remove(&job);
                    }
                    if !success {
                        eprintln!("\n{name} failed: {output}");
                    }
                }
                ServerEvent::TurnStarted {
                    request_id: started,
                    origin: Some(origin),
                    ..
                } => {
                    for job in &origin.jobs {
                        background.remove(job);
                    }
                    if printed_delta {
                        println!();
                        printed_delta = false;
                    }
                    eprintln!("[background report: {}]", origin.jobs.join(", "));
                    reporting.insert(started);
                }
                ServerEvent::TurnCompleted {
                    request_id: finished,
                    ..
                } => {
                    if printed_delta {
                        println!();
                        printed_delta = false;
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
                    ..
                }
                | ServerEvent::TurnFailed {
                    request_id: finished,
                    ..
                } => {
                    eprintln!("[background report did not complete]");
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
