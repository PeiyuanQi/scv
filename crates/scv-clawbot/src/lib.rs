//! Safe, testable primitives for the WeChat iLink ClawBot adapter.

use anyhow::{Result, anyhow, bail};

pub mod bridge;
pub mod protocol;
pub mod state;

use serde_json::Value;
use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub async fn login(base: &str, account: &str) -> Result<()> {
    state::validate_name(account)?;
    let client = http_client()?;
    let base = normalize_base_url(base)?;
    let qr = response_json(
        client
            .get(format!("{base}/ilink/bot/get_bot_qrcode?bot_type=3"))
            .timeout(Duration::from_secs(20))
            .send()
            .await?,
    )
    .await?;
    check_envelope(&qr)?;
    let code = qr
        .get("qrcode")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("login response omitted qrcode"))?;
    println!(
        "Scan this ClawBot QR code in WeChat:\n{}",
        qr.get("qrcode_img_content")
            .and_then(Value::as_str)
            .unwrap_or(code)
    );
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if Instant::now() >= deadline {
            bail!("ClawBot QR login timed out; run `scv clawbot login` again")
        }
        let status = response_json(
            client
                .get(format!("{base}/ilink/bot/get_qrcode_status"))
                .query(&[("qrcode", code)])
                .timeout(Duration::from_secs(50))
                .send()
                .await?,
        )
        .await?;
        check_envelope(&status)?;
        match status
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
        {
            "confirmed" => {
                let (token, bot_id, user_id) = bridge::validate_confirmed_login(&status)?;
                let host = normalize_base_url(
                    status
                        .get("baseurl")
                        .and_then(Value::as_str)
                        .unwrap_or(&base),
                )?;
                bridge::validate_origin_pair(&base, &host)?;
                state::save_account(
                    account,
                    &state::Account {
                        token: token.into(),
                        base_url: host.clone(),
                        bot_id: Some(bot_id.into()),
                        user_id: Some(user_id.into()),
                    },
                )?;
                println!("ClawBot login confirmed for {bot_id} at {host}.");
                return Ok(());
            }
            "expired" => bail!("ClawBot QR code expired; run `scv clawbot login` again"),
            _ => {}
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

const MAX_REPLY_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_MESSAGE_ID_BYTES: usize = 256;
const MAX_BATCH_MESSAGES: usize = 4096;
const FAILURE_REPLY: &str = "SCV could not complete that request.";
const TURN_TIMEOUT: Duration = Duration::from_secs(300);
/// Owner turns may run tools and delegated agents, which take longer.
const OWNER_TURN_TIMEOUT: Duration = Duration::from_secs(1800);
/// Model time an owner turn keeps beyond its longest single tool call.
const OWNER_TURN_MARGIN: Duration = Duration::from_secs(300);

/// The account owner granted remote tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOwner {
    /// The owner's authenticated iLink user ID.
    pub user_id: String,
    /// How long one owner turn may run; see [`owner_turn_timeout`].
    pub turn_timeout: Duration,
}

/// An owner turn outlasts the longest tool call the session allows
/// (`tools.max_timeout_seconds`) by a margin for the model's own work, and
/// never runs shorter than 30 minutes.
pub fn owner_turn_timeout(max_tool_timeout: Duration) -> Duration {
    max_tool_timeout
        .saturating_add(OWNER_TURN_MARGIN)
        .max(OWNER_TURN_TIMEOUT)
}

/// Compatibility entry point. Connects to the existing daemon; launches no process.
pub async fn run(token: &str, base_url: &str, account: &str, workspace: &Path) -> Result<()> {
    run_supervised(
        token,
        base_url,
        account,
        workspace,
        &scv_client::default_socket_path()?,
        None,
        CancellationToken::new(),
        Arc::new(|_| {}),
    )
    .await
}

/// Run one account until cancelled. Only a validated authenticated getupdates
/// response reports healthy. Cancellation drops all owned I/O and sessions;
/// no adapter tasks are spawned. The caller supplies any external stop timeout.
///
/// `tool_owner` is the authenticated owner when the account grants its owner
/// remote tools; every other sender stays tool-free.
#[allow(clippy::too_many_arguments)]
pub async fn run_supervised(
    token: &str,
    base_url: &str,
    account: &str,
    workspace: &Path,
    socket: &Path,
    tool_owner: Option<&ToolOwner>,
    cancellation: CancellationToken,
    report: Arc<dyn Fn(bool) + Send + Sync>,
) -> Result<()> {
    until_cancelled(cancellation, async {
        state::validate_name(account)?;
        let base_url = normalize_base_url(base_url)?;
        let store = state::Store::new(state::root()?);
        let result = run_loop(
            token,
            &base_url,
            account,
            workspace,
            socket,
            tool_owner,
            &store,
            report.as_ref(),
        )
        .await;
        if result.is_err() {
            report(false);
        }
        result
    })
    .await
}

async fn until_cancelled(
    cancellation: CancellationToken,
    work: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Ok(()),
        result = work => result,
    }
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

async fn response_json(response: reqwest::Response) -> Result<Value> {
    let body = response_body(response).await?;
    serde_json::from_slice(&body).map_err(|_| anyhow!("invalid ClawBot response"))
}

async fn response_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if !response.status().is_success() {
        bail!(
            "ClawBot HTTP request failed with status {}",
            response.status()
        )
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        bail!("ClawBot response exceeds limit")
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            bail!("ClawBot response exceeds limit")
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    token: &str,
    base_url: &str,
    account: &str,
    workspace: &Path,
    socket: &Path,
    tool_owner: Option<&ToolOwner>,
    store: &state::Store,
    report: &(dyn Fn(bool) + Send + Sync),
) -> Result<()> {
    let _lock = store.lock(account)?;
    let client = http_client()?;
    let mut state = store.bind_state(account, token, base_url)?;
    let mut sessions: HashMap<String, protocol::Session> = HashMap::new();
    let mut backoff = Duration::from_secs(1);
    let delivery = Delivery {
        client: &client,
        token,
        base_url,
        account,
        store,
        report,
    };
    recover_interrupted(store, account, &mut state)?;
    delivery.deliver_pending(&mut state).await?;
    // Use the durable state's seen list directly, including recovered deliveries.
    loop {
        let response = async {
            let response = client.post(format!("{base_url}/ilink/bot/getupdates"))
                .headers(bridge::auth_headers(token, u32::from_le_bytes(*Uuid::new_v4().as_bytes().first_chunk::<4>().unwrap())))
                .json(&serde_json::json!({"get_updates_buf":state.cursor,"base_info":{"channel_version":"1.0.0"}}))
                .timeout(Duration::from_secs(50)).send().await?;
            let value = response_json(response).await?;
            validate_updates(&value)?;
            Ok::<_, anyhow::Error>(value)
        }.await;
        let response = match response {
            Ok(response) => {
                report(true);
                response
            }
            Err(error) => {
                tracing::warn!("ClawBot poll failed: {error}");
                report(false);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };
        backoff = Duration::from_secs(1);
        sessions.retain(|_, s| s.last_used.elapsed() < Duration::from_secs(1800));
        for msg in response
            .get("msgs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = message_id(msg) else {
                continue;
            };
            if state.seen.iter().any(|seen| seen == &id) {
                // Keep all IDs encountered in this bounded batch until its cursor
                // commits, including IDs recovered from the preceding run.
                mark_seen(&mut state, &id);
                store.save_state(account, &state)?;
                continue;
            }
            if msg.get("message_type").and_then(Value::as_i64) != Some(1) {
                mark_seen(&mut state, &id);
                store.save_state(account, &state)?;
                continue;
            }
            let Some(text) = msg
                .get("item_list")
                .and_then(Value::as_array)
                .and_then(|xs| {
                    xs.iter()
                        .find_map(|x| x.get("text_item")?.get("text")?.as_str())
                })
                .filter(|text| !text.trim().is_empty())
            else {
                mark_seen(&mut state, &id);
                store.save_state(account, &state)?;
                continue;
            };
            let Some(sender) = msg
                .get("from_user_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                mark_seen(&mut state, &id);
                store.save_state(account, &state)?;
                continue;
            };
            let Some(ctx) = msg
                .get("context_token")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                mark_seen(&mut state, &id);
                store.save_state(account, &state)?;
                continue;
            };
            // Group messages never carry owner authority and never share the
            // sender's direct-chat session, whose history may hold tool output.
            let group = match msg.get("group_id") {
                None | Some(Value::Null) => None,
                Some(Value::String(group)) if group.is_empty() => None,
                Some(Value::String(group)) => Some(group.clone()),
                Some(other) => Some(other.to_string()),
            };
            let key = group
                .as_ref()
                .map_or_else(|| sender.to_owned(), |group| format!("{group}\0{sender}"));
            if !sessions.contains_key(&key)
                && sessions.len() >= 32
                && let Some(oldest) = sessions
                    .iter()
                    .min_by_key(|(_, session)| session.last_used)
                    .map(|(key, _)| key.clone())
            {
                sessions.remove(&oldest);
            }
            state.in_flight = Some(state::InFlight {
                message_id: id.clone(),
                to_user_id: sender.into(),
                context_token: ctx.into(),
            });
            store.save_state(account, &state)?;
            let owner = group.is_none()
                && tool_owner.is_some_and(|tool_owner| tool_owner.user_id == sender);
            let limit = match tool_owner {
                Some(tool_owner) if owner => tool_owner.turn_timeout,
                _ => TURN_TIMEOUT,
            };
            let result = tokio::time::timeout(limit, async {
                let session = match sessions.entry(key.clone()) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        if owner {
                            tracing::info!("ClawBot owner session starts with remote tools");
                        }
                        e.insert(protocol::Session::connect(socket, workspace, owner).await?)
                    }
                };
                session.turn(text, MAX_REPLY_BYTES).await
            })
            .await;
            let reply = match result {
                Ok(Ok(reply)) => reply,
                Ok(Err(_)) | Err(_) => {
                    sessions.remove(&key);
                    FAILURE_REPLY.into()
                }
            };
            let reply = if reply.trim().is_empty() {
                "SCV completed without a text response.".into()
            } else {
                reply
            };
            state.pending = Some(new_pending(&id, sender, ctx, &reply, MAX_REPLY_BYTES));
            state.in_flight = None;
            store.save_state(account, &state)?;
            delivery.deliver_pending(&mut state).await?;
        }
        if let Some(next) = response.get("get_updates_buf").and_then(Value::as_str) {
            state.cursor = next.into();
        }
        store.save_state(account, &state)?;
        // Also yield for immediately-ready mocked transports and empty batches.
        tokio::task::yield_now().await;
    }
}

fn message_id(msg: &Value) -> Option<String> {
    let value = msg.get("message_id").or_else(|| msg.get("msg_id"))?;
    match value {
        Value::String(id) if !id.is_empty() && id.len() <= MAX_MESSAGE_ID_BYTES => Some(id.clone()),
        Value::Number(id) if id.as_u64().is_some() => Some(id.to_string()),
        _ => None,
    }
}

fn validate_updates(value: &Value) -> Result<()> {
    // Current iLink getupdates responses omit `ret` on success, while error
    // responses and older servers use the common envelope. Accept both forms.
    if value.get("ret").is_some() || value.get("errcode").is_some() {
        check_envelope(value)?;
    } else if !value.get("msgs").is_some_and(Value::is_array)
        || !value.get("get_updates_buf").is_some_and(Value::is_string)
    {
        bail!("iLink updates response omitted success fields")
    }
    if value
        .get("msgs")
        .and_then(Value::as_array)
        .is_some_and(|msgs| msgs.len() > MAX_BATCH_MESSAGES)
    {
        bail!("ClawBot updates batch exceeds limit")
    }
    if value.get("msgs").is_some_and(|msgs| !msgs.is_array())
        || value
            .get("get_updates_buf")
            .is_some_and(|cursor| !cursor.is_string())
    {
        bail!("invalid ClawBot updates response")
    }
    Ok(())
}

fn recover_interrupted(
    store: &state::Store,
    account: &str,
    state: &mut state::BridgeState,
) -> Result<()> {
    if let Some(interrupted) = state.in_flight.take() {
        if state.pending.is_some() {
            bail!("inconsistent ClawBot delivery state")
        }
        state.pending = Some(new_pending(
            &interrupted.message_id,
            &interrupted.to_user_id,
            &interrupted.context_token,
            FAILURE_REPLY,
            MAX_REPLY_BYTES,
        ));
        store.save_state(account, state)?;
    }
    Ok(())
}

fn new_pending(
    message_id: &str,
    to_user_id: &str,
    context_token: &str,
    reply: &str,
    max_bytes: usize,
) -> state::PendingDelivery {
    let chunks = split_utf8(reply, max_bytes);
    state::PendingDelivery {
        message_id: message_id.to_owned(),
        to_user_id: to_user_id.to_owned(),
        context_token: context_token.to_owned(),
        reply: reply.to_owned(),
        client_ids: chunks.iter().map(|_| Uuid::new_v4().to_string()).collect(),
        next_chunk: 0,
    }
}

struct Delivery<'a> {
    client: &'a reqwest::Client,
    token: &'a str,
    base_url: &'a str,
    account: &'a str,
    store: &'a state::Store,
    report: &'a (dyn Fn(bool) + Send + Sync),
}

impl Delivery<'_> {
    async fn deliver_pending(&self, state: &mut state::BridgeState) -> Result<()> {
        let Some(mut pending) = state.pending.take() else {
            return Ok(());
        };
        let chunks = split_utf8(&pending.reply, MAX_REPLY_BYTES);
        while pending.client_ids.len() < chunks.len() {
            pending.client_ids.push(Uuid::new_v4().to_string());
        }
        if pending.next_chunk > chunks.len() {
            pending.next_chunk = 0;
        }
        state.pending = Some(pending.clone());
        self.store.save_state(self.account, state)?;
        while pending.next_chunk < chunks.len() {
            let index = pending.next_chunk;
            let body = bridge::reply_body(
                &pending.to_user_id,
                &pending.context_token,
                &chunks[index],
                &pending.client_ids[index],
            );
            let outcome = bridge::send_reply_request(
                self.client,
                self.token,
                self.base_url,
                &body,
                self.report,
            )
            .await?;
            if outcome == bridge::SendOutcome::Rejected {
                // Explicit refusal is final; drop the rest of this reply.
                break;
            }
            pending.next_chunk += 1;
            state.pending = Some(pending.clone());
            self.store.save_state(self.account, state)?;
        }
        if !pending.message_id.is_empty() {
            mark_seen(state, &pending.message_id);
        }
        state.pending = None;
        self.store.save_state(self.account, state)?;
        Ok(())
    }
}

pub fn normalize_base_url(value: &str) -> Result<String> {
    let url =
        reqwest::Url::parse(value.trim()).map_err(|e| anyhow!("invalid ClawBot base URL: {e}"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || (url.path() != "/" && !url.path().is_empty())
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("ClawBot base URL must be an HTTPS origin")
    }
    Ok(value.trim().trim_end_matches('/').to_owned())
}

fn mark_seen(state: &mut state::BridgeState, id: &str) {
    if let Some(index) = state.seen.iter().position(|seen| seen == id) {
        state.seen.remove(index);
    }
    state.seen.push(id.to_owned());
    let excess = state.seen.len().saturating_sub(4096);
    state.seen.drain(..excess);
}

pub fn check_envelope(value: &serde_json::Value) -> Result<()> {
    let ret = value
        .get("ret")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| anyhow!("iLink response omitted ret"))?;
    if ret != 0 || value.get("errcode").is_some_and(|v| v.as_i64() != Some(0)) {
        bail!("iLink API rejected request")
    }
    Ok(())
}

/// Classify a 2xx iLink sendmessage body. Live acknowledgements omit `ret`
/// and need not be JSON, so only an explicit non-zero `ret` or `errcode`
/// rejects; the error is a bounded diagnostic without message content or
/// non-integer code values.
pub fn check_send_ack(body: &[u8]) -> std::result::Result<(), String> {
    let Ok(Value::Object(value)) = serde_json::from_slice::<Value>(body) else {
        return Ok(());
    };
    let code = |key: &str| {
        value
            .get(key)
            .filter(|v| !v.is_null() && v.as_i64() != Some(0))
            .map(|v| {
                v.as_i64()
                    .map_or_else(|| "non-integer".into(), |n| n.to_string())
            })
    };
    let (ret, errcode) = (code("ret"), code("errcode"));
    if ret.is_none() && errcode.is_none() {
        return Ok(());
    }
    let errmsg: String = value
        .get("errmsg")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(120)
        .collect();
    Err(format!(
        "ret={} errcode={} errmsg={errmsg:?}",
        ret.as_deref().unwrap_or("-"),
        errcode.as_deref().unwrap_or("-")
    ))
}

pub fn split_utf8(value: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = value;
    let max = max.max(1);
    while rest.len() > max {
        let mut end = max;
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            end = rest
                .char_indices()
                .nth(1)
                .map_or(rest.len(), |(index, _)| index);
        }
        out.push(rest[..end].to_owned());
        rest = &rest[end..];
    }
    if !rest.is_empty() {
        out.push(rest.to_owned());
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_origins() {
        assert!(normalize_base_url("https://example.test").is_ok());
        assert!(normalize_base_url("http://example.test").is_err());
        assert!(normalize_base_url("https://user@example.test").is_err());
    }
    #[test]
    fn chunks_on_utf8_boundaries() {
        let chunks = split_utf8("a🙂b", 4);
        assert_eq!(chunks, vec!["a", "🙂", "b"]);
    }
    #[test]
    fn chunks_make_progress_below_codepoint_size() {
        assert_eq!(split_utf8("🙂", 1), vec!["🙂"]);
        assert_eq!(split_utf8("🙂", 0), vec!["🙂"]);
    }
    #[test]
    fn validates_ret() {
        assert!(check_envelope(&serde_json::json!({"ret":0})).is_ok());
        assert!(check_envelope(&serde_json::json!({"ret":1})).is_err());
    }

    #[test]
    fn accepts_live_send_ack_without_ret() {
        for delivered in [
            &b""[..],
            b"{}",
            br#"{"ret":0}"#,
            br#"{"ret":null}"#,
            b"ok",
            b"[]",
        ] {
            assert!(check_send_ack(delivered).is_ok());
        }
        assert_eq!(
            check_send_ack(br#"{"ret":-2,"errmsg":"prepare failed"}"#).unwrap_err(),
            r#"ret=-2 errcode=- errmsg="prepare failed""#
        );
        assert!(check_send_ack(br#"{"errcode":40001}"#).is_err());
        assert!(check_send_ack(br#"{"ret":"0"}"#).is_err());
        assert_eq!(
            check_send_ack(br#"{"ret":{"detail":"x"},"errcode":7}"#).unwrap_err(),
            r#"ret=non-integer errcode=7 errmsg="""#
        );
    }

    #[test]
    fn accepts_live_getupdates_success_without_ret() {
        assert!(
            validate_updates(&serde_json::json!({
                "msgs": [],
                "sync_buf": "sync",
                "get_updates_buf": "cursor"
            }))
            .is_ok()
        );
    }

    #[test]
    fn rejects_getupdates_error_without_ret() {
        assert!(
            validate_updates(&serde_json::json!({
                "errcode": -14,
                "errmsg": "session timeout"
            }))
            .is_err()
        );
    }

    #[test]
    fn preserves_string_and_unsigned_numeric_message_ids() {
        assert_eq!(
            message_id(&serde_json::json!({"message_id": "string-id"})).as_deref(),
            Some("string-id")
        );
        assert_eq!(
            message_id(&serde_json::json!({"message_id": u64::MAX})).as_deref(),
            Some("18446744073709551615")
        );
        assert_eq!(
            message_id(&serde_json::json!({"msg_id": 42})).as_deref(),
            Some("42")
        );
        assert!(message_id(&serde_json::json!({"message_id": null, "msg_id": 42})).is_none());
        assert!(message_id(&serde_json::json!({"message_id": -1})).is_none());
        assert!(message_id(&serde_json::json!({"message_id": 1.5})).is_none());
        assert!(message_id(&serde_json::from_str(r#"{"message_id":1e3}"#).unwrap()).is_none());
        assert!(
            message_id(&serde_json::from_str(r#"{"message_id":18446744073709551616}"#).unwrap())
                .is_none()
        );
        assert!(
            message_id(&serde_json::json!({
                "message_id": "x".repeat(MAX_MESSAGE_ID_BYTES + 1)
            }))
            .is_none()
        );
    }
}
