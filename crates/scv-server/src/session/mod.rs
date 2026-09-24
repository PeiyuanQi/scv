//! A connection's session: its configuration, runtime, history, and queue,
//! built by [`build`], queued by [`queue`], and run a turn at a time by
//! [`turn`].

pub(crate) mod build;
mod queue;
pub(crate) mod turn;

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use scv_core::{AgentRuntime, Message, TurnInput};
use scv_protocol::{Attachment, QueueEntry};
use scv_tools::background::BackgroundJobs;
use tokio::sync::Mutex;

use crate::{attachments, config::Config};

pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) workspace: PathBuf,
    pub(crate) config: Config,
    pub(crate) runtime: Arc<AgentRuntime>,
    pub(crate) history: Arc<Mutex<Vec<Message>>>,
    pub(crate) seq: Arc<AtomicU64>,
    pub(crate) queue: Arc<Mutex<VecDeque<QueueEntry>>>,
    pub(crate) paused: Arc<std::sync::atomic::AtomicBool>,
    /// Background agent jobs, shared with the session's tools.
    pub(crate) background: Option<Arc<BackgroundJobs>>,
    /// Whether the model has tools, and so may see attachments' paths.
    pub(crate) tools: bool,
}

impl Session {
    /// The model's input for a client's prompt and attachments.
    pub(crate) fn turn_input(&self, prompt: &str, attachments: &[Attachment]) -> TurnInput {
        attachments::turn_input(prompt, attachments, self.tools)
    }
}

#[derive(Clone)]
pub(crate) struct TurnMeta {
    pub(crate) request_id: String,
    pub(crate) session_id: String,
    pub(crate) turn_id: String,
    pub(crate) seq: Arc<AtomicU64>,
    pub(crate) max_server_frame: usize,
}

pub(crate) fn next_seq(sequence: &AtomicU64) -> u64 {
    sequence.fetch_add(1, Ordering::Relaxed) + 1
}

/// What a client declared about itself in `session.start`.
#[derive(Debug, Default)]
pub(crate) struct SessionClient {
    /// The chat channel the session answers on, such as `WeChat`.
    pub(crate) channel: Option<String>,
    /// The client approves every approval request without asking anyone.
    pub(crate) auto_approve: bool,
}

pub(crate) fn valid_channel_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.len() <= scv_protocol::MAX_CHANNEL_NAME_BYTES
        && !name.chars().any(char::is_control)
}
