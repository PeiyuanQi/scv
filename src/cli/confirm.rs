//! `scv confirm`: ask the owner a yes/no question in chat and wait for the
//! answer, such as before a delegated agent publishes SCV to crates.io.
//!
//! The daemon sends the question and holds it (see `scv-server`'s
//! `confirm`); this command asks with `confirm_ask`, then follows the
//! question with `confirm_status` until it is answered or runs out. The
//! exit status is the answer.

use anyhow::Result;
use scv_client::{ControlError, Layout};
use scv_protocol::{ConfirmState, DaemonCommand, ErrorCode};
use std::time::Duration;

use super::control;

/// The owner said no, or did not answer in time.
const NO: i32 = 1;
/// The question could not be asked, or its answer not learned.
const UNASKED: i32 = 2;
/// How often the question is followed; following it also keeps it alive.
const POLL: Duration = Duration::from_secs(2);
/// How long past its deadline a question's outcome may take to be learned
/// while the daemon is slow to answer.
const LATE_SECONDS: u64 = 120;

/// Ask `question` and exit with the answer: 0 yes, 1 no or no answer, 2
/// not asked.
pub(crate) async fn confirm(layout: &Layout, question: String, timeout: u64) -> Result<()> {
    match ask(layout, question, timeout).await {
        0 => Ok(()),
        code => std::process::exit(code),
    }
}

async fn ask(layout: &Layout, question: String, timeout: u64) -> i32 {
    let parent = std::env::var(scv_tools::delegation::PARENT_VARIABLE)
        .ok()
        .filter(|chain| !chain.trim().is_empty());
    let asked = control(
        layout,
        DaemonCommand::ConfirmAsk {
            question,
            parent,
            timeout_seconds: Some(timeout),
        },
    )
    .await;
    let mut info = match asked.map(|status| status.confirm) {
        Ok(Some(info)) => info,
        Ok(None) => {
            eprintln!("scv confirm: the daemon did not take the question");
            return UNASKED;
        }
        Err(error) => {
            eprintln!("scv confirm: {}", not_asked(&error));
            return UNASKED;
        }
    };
    eprintln!(
        "Asked the owner on {}; waiting up to {} for yes or no.",
        info.chat,
        minutes(timeout)
    );
    let give_up = info.deadline_unix_seconds + LATE_SECONDS;
    loop {
        if let Some(code) = exit_code(info.state) {
            match info.state {
                ConfirmState::Yes => println!("The owner said yes."),
                ConfirmState::No => println!("The owner said no."),
                ConfirmState::Expired => println!("No answer in time, which counts as no."),
                _ => eprintln!(
                    "scv confirm: the question on {} ended without an answer ({:?})",
                    info.chat, info.state
                ),
            }
            return code;
        }
        tokio::time::sleep(POLL).await;
        let followed = control(
            layout,
            DaemonCommand::ConfirmStatus {
                id: info.id.clone(),
            },
        )
        .await;
        match followed.map(|status| status.confirm) {
            Ok(Some(next)) => info = next,
            Ok(None) => {
                eprintln!("scv confirm: the daemon no longer reports the question");
                return UNASKED;
            }
            Err(error) => {
                let busy = matches!(
                    error.downcast_ref::<ControlError>(),
                    Some(ControlError::TimedOut | ControlError::Protocol(_))
                );
                if busy && unix_now() < give_up {
                    continue;
                }
                eprintln!("scv confirm: lost the question while waiting: {error:#}");
                return UNASKED;
            }
        }
    }
}

/// The exit status a question in `state` ends with, or `None` while it
/// waits.
fn exit_code(state: ConfirmState) -> Option<i32> {
    match state {
        ConfirmState::Pending => None,
        ConfirmState::Yes => Some(0),
        ConfirmState::No | ConfirmState::Expired => Some(NO),
        ConfirmState::Withdrawn | ConfirmState::Failed | ConfirmState::Unknown => Some(UNASKED),
    }
}

/// Why the question could not be asked, for the caller.
fn not_asked(error: &anyhow::Error) -> String {
    match error.downcast_ref::<ControlError>() {
        Some(ControlError::Unavailable(_)) => {
            "no SCV daemon is running to ask the owner; nothing was asked".into()
        }
        // A daemon that predates the request cannot parse it.
        Some(ControlError::Server {
            code: ErrorCode::InvalidJson,
            ..
        }) => "the running SCV daemon is too old to ask the owner; nothing was asked".into(),
        _ => format!("{error:#}"),
    }
}

fn minutes(seconds: u64) -> String {
    match seconds.div_ceil(60) {
        1 => "1 minute".into(),
        minutes => format!("{minutes} minutes"),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests;
