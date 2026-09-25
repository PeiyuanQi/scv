//! The outbound side of a connection: a queue bounded in frames and bytes
//! that applies backpressure to turns, and encoding of protocol events.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use scv_core::AgentError;
use scv_protocol::ServerEvent;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

pub(crate) const OUTPUT_QUEUE_CAPACITY: usize = 256;
pub(crate) const OUTPUT_QUEUE_MIN_BYTES: usize = 16 * 1024 * 1024;
/// How long a control send waits out backpressure, and how long shutdown
/// waits for a turn or the writer before aborting it.
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

pub(crate) struct OutboundFrame {
    pub(crate) bytes: Vec<u8>,
    pub(crate) _byte_permit: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub(crate) struct OutboundSender {
    pub(crate) frames: mpsc::Sender<OutboundFrame>,
    pub(crate) budget: Arc<Semaphore>,
    pub(crate) capacity: Arc<AtomicUsize>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OutboundSendError {
    Cancelled,
    Closed,
    TimedOut,
    FrameExceedsQueue { frame_bytes: usize, capacity: usize },
}

impl std::fmt::Display for OutboundSendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("outbound send cancelled"),
            Self::Closed => formatter.write_str("protocol client disconnected"),
            Self::TimedOut => formatter.write_str("outbound send timed out under backpressure"),
            Self::FrameExceedsQueue {
                frame_bytes,
                capacity,
            } => write!(
                formatter,
                "outbound frame uses {frame_bytes} bytes but queue capacity is {capacity} bytes"
            ),
        }
    }
}

impl std::error::Error for OutboundSendError {}

pub(crate) fn outbound_channel(capacity: usize) -> (OutboundSender, mpsc::Receiver<OutboundFrame>) {
    let (frames, receiver) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
    (
        OutboundSender {
            frames,
            budget: Arc::new(Semaphore::new(capacity)),
            capacity: Arc::new(AtomicUsize::new(capacity)),
        },
        receiver,
    )
}

impl OutboundSender {
    pub(crate) fn ensure_capacity(&self, required: usize) -> Result<()> {
        if required > Semaphore::MAX_PERMITS {
            return Err(anyhow!(
                "outbound queue capacity {required} exceeds runtime limit {}",
                Semaphore::MAX_PERMITS
            ));
        }
        let current = self.capacity.load(Ordering::Acquire);
        if required > current {
            self.budget.add_permits(required - current);
            self.capacity.store(required, Ordering::Release);
        }
        Ok(())
    }

    pub(crate) async fn send(
        &self,
        bytes: Vec<u8>,
        cancellation: Option<&CancellationToken>,
    ) -> std::result::Result<(), OutboundSendError> {
        self.send_with_timeout(bytes, cancellation, SHUTDOWN_GRACE)
            .await
    }

    pub(crate) async fn send_with_timeout(
        &self,
        bytes: Vec<u8>,
        cancellation: Option<&CancellationToken>,
        control_timeout: Duration,
    ) -> std::result::Result<(), OutboundSendError> {
        let frame_bytes =
            bytes
                .len()
                .checked_add(1)
                .ok_or(OutboundSendError::FrameExceedsQueue {
                    frame_bytes: usize::MAX,
                    capacity: self.capacity.load(Ordering::Acquire),
                })?;
        let capacity = self.capacity.load(Ordering::Acquire);
        let permits =
            u32::try_from(frame_bytes).map_err(|_| OutboundSendError::FrameExceedsQueue {
                frame_bytes,
                capacity,
            })?;
        if frame_bytes > capacity {
            return Err(OutboundSendError::FrameExceedsQueue {
                frame_bytes,
                capacity,
            });
        }

        let control_deadline = tokio::time::Instant::now() + control_timeout;
        let acquire = Arc::clone(&self.budget).acquire_many_owned(permits);
        let permit = if let Some(cancellation) = cancellation {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(OutboundSendError::Cancelled),
                permit = acquire => permit.map_err(|_| OutboundSendError::Closed)?,
            }
        } else {
            tokio::time::timeout_at(control_deadline, acquire)
                .await
                .map_err(|_| OutboundSendError::TimedOut)?
                .map_err(|_| OutboundSendError::Closed)?
        };
        let frame = OutboundFrame {
            bytes,
            _byte_permit: permit,
        };
        if let Some(cancellation) = cancellation {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => Err(OutboundSendError::Cancelled),
                result = self.frames.send(frame) => result.map_err(|_| OutboundSendError::Closed),
            }
        } else {
            tokio::time::timeout_at(control_deadline, self.frames.send(frame))
                .await
                .map_err(|_| OutboundSendError::TimedOut)?
                .map_err(|_| OutboundSendError::Closed)
        }
    }
}

pub(crate) fn output_queue_bytes(max_frame_bytes: usize) -> Result<usize> {
    let required = max_frame_bytes
        .checked_add(1)
        .and_then(|bytes| bytes.checked_mul(2))
        .ok_or_else(|| anyhow!("configured server frame limit is too large"))?
        .max(OUTPUT_QUEUE_MIN_BYTES);
    if required > Semaphore::MAX_PERMITS {
        return Err(anyhow!(
            "configured server frame limit requires an outbound queue larger than the runtime supports"
        ));
    }
    Ok(required)
}

pub(crate) async fn send_event(
    output: &OutboundSender,
    event: ServerEvent,
    max_bytes: usize,
) -> Result<()> {
    let bytes = encode_event(&event, max_bytes)?;
    output.send(bytes, None).await.map_err(anyhow::Error::new)
}

pub(crate) async fn send_turn_event(
    output: &OutboundSender,
    event: ServerEvent,
    max_bytes: usize,
    cancellation: &CancellationToken,
) -> Result<(), AgentError> {
    let bytes = encode_event(&event, max_bytes)
        .map_err(|error| AgentError::ResponseLimit(error.to_string()))?;
    match output.send(bytes, Some(cancellation)).await {
        Ok(()) => Ok(()),
        Err(OutboundSendError::Cancelled) => Err(AgentError::Cancelled),
        Err(error) => Err(AgentError::Internal(error.to_string())),
    }
}

pub(crate) fn encode_event(event: &ServerEvent, max_bytes: usize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(event).context("serialize protocol event")?;
    if bytes.len() > max_bytes {
        return Err(anyhow!("server event exceeds configured frame limit"));
    }
    Ok(bytes)
}

pub(crate) async fn send_error(
    output: &OutboundSender,
    request_id: &str,
    code: &str,
    message: &str,
    fatal: bool,
    max_bytes: usize,
) -> Result<()> {
    send_event(
        output,
        ServerEvent::Error {
            request_id: (!request_id.is_empty()).then(|| request_id.to_owned()),
            code: code.into(),
            message: message.into(),
            fatal,
        },
        max_bytes,
    )
    .await
}

#[cfg(test)]
mod tests;
