//! Retry pacing shared by the channel bridges: a doubling [`Backoff`] for
//! loops that poll or redeliver for the life of a run, and [`retry_send`]
//! for one outbound request that gets a few quick tries before the bridge
//! gives up and redelivers it later.

use anyhow::{Result, anyhow};
use std::future::Future;
use std::time::Duration;

/// A doubling delay between retries, from [`Backoff::FIRST`] up to
/// [`Backoff::MAX`]. [`Backoff::reset`] starts over after a success.
#[derive(Debug, Clone)]
pub(crate) struct Backoff {
    next: Duration,
}

impl Backoff {
    /// The first delay, and the delay again after [`Backoff::reset`].
    pub(crate) const FIRST: Duration = Duration::from_secs(1);
    /// The longest delay: a platform that stays down is still retried every
    /// minute.
    pub(crate) const MAX: Duration = Duration::from_secs(60);

    pub(crate) fn new() -> Self {
        Self { next: Self::FIRST }
    }

    /// The delay to wait now. Each call doubles the next one, up to
    /// [`Backoff::MAX`].
    pub(crate) fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(Self::MAX);
        delay
    }

    /// Sleep for [`Backoff::next_delay`].
    pub(crate) async fn wait(&mut self) {
        tokio::time::sleep(self.next_delay()).await;
    }

    pub(crate) fn reset(&mut self) {
        self.next = Self::FIRST;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

/// How many times [`retry_send`] tries one request.
pub(crate) const SEND_ATTEMPTS: u32 = 3;

/// The result of one try at an outbound request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Attempt<T> {
    /// The platform accepted it.
    Done(T),
    /// The platform refused it; sending the same request again cannot help.
    Refused(String),
    /// A transport, rate-limit, token, or server failure worth retrying.
    Retry(String),
}

/// What [`retry_send`] logs and returns, so each platform keeps its wording.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SendLabels<'a> {
    /// Logged with the reason when the platform refuses, such as
    /// `"Feishu refused a reply"`.
    pub(crate) refused: &'a str,
    /// Logged with the reason after each transient failure, such as
    /// `"Feishu reply send failed"`.
    pub(crate) failed: &'a str,
    /// The error once every try failed, such as
    /// `"Feishu could not deliver the reply"`.
    pub(crate) gave_up: &'a str,
}

/// Try one outbound request up to [`SEND_ATTEMPTS`] times, waiting 1 s and
/// then 2 s between tries. Each transient failure is logged and reported to
/// `report` as unhealthy. A refusal ends at once with `Ok(None)`, because
/// resending cannot help; exhausting the tries is an error, so the caller's
/// delivery loop keeps the reply and redelivers it later.
///
/// `attempt` must be safe to repeat: callers resend with the same
/// idempotency key (a client id or `uuid`) so the platform delivers once.
pub(crate) async fn retry_send<T, F, Fut>(
    labels: SendLabels<'_>,
    report: &(dyn Fn(bool) + Send + Sync),
    mut attempt: F,
) -> Result<Option<T>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Attempt<T>>,
{
    let mut backoff = Backoff::new();
    for attempt_number in 0..SEND_ATTEMPTS {
        match attempt().await {
            Attempt::Done(value) => return Ok(Some(value)),
            Attempt::Refused(reason) => {
                tracing::warn!("{} ({reason}); not retrying", labels.refused);
                return Ok(None);
            }
            Attempt::Retry(reason) => {
                tracing::warn!(attempt = attempt_number, "{}: {reason}", labels.failed);
            }
        }
        report(false);
        if attempt_number + 1 < SEND_ATTEMPTS {
            backoff.wait().await;
        }
    }
    Err(anyhow!("{}", labels.gave_up))
}

#[cfg(test)]
mod tests;
