//! The WeChat channel: a transport for the shared channel bridge over the
//! iLink ClawBot HTTP API.

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use scv_channels::{Batch, Inbound, Message, Outbound, SendOutcome, Transport};
pub use scv_channels::{ToolOwner, owner_turn_timeout};

/// The channel name this crate serves: `scv channels <command> wechat`.
pub const CHANNEL: &str = "wechat";

pub mod bridge;
pub mod state;

use serde_json::Value;
use std::{
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
            bail!("WeChat QR login timed out; run `scv channels login wechat` again")
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
            "expired" => bail!("WeChat QR code expired; run `scv channels login wechat` again"),
            _ => {}
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_MESSAGE_ID_BYTES: usize = 256;
const MAX_BATCH_MESSAGES: usize = 4096;

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
        let store = state::store()?;
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

/// Bridge one account over iLink with the given running credentials.
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
    let transport = Ilink {
        client: http_client()?,
        token,
        base_url,
    };
    scv_channels::run(
        &transport,
        account,
        workspace,
        socket,
        tool_owner,
        store,
        |saved| state::runs_as(saved, token, base_url),
        report,
    )
    .await
}

/// The iLink transport of one account.
struct Ilink<'a> {
    client: reqwest::Client,
    token: &'a str,
    base_url: &'a str,
}

#[async_trait]
impl Transport for Ilink<'_> {
    fn label(&self) -> &'static str {
        "ClawBot"
    }

    /// Long-poll `getupdates` after the opaque cursor.
    async fn receive(&self, cursor: &str) -> Result<Batch> {
        let response = self
            .client
            .post(format!("{}/ilink/bot/getupdates", self.base_url))
            .headers(bridge::auth_headers(
                self.token,
                u32::from_le_bytes(*Uuid::new_v4().as_bytes().first_chunk::<4>().unwrap()),
            ))
            .json(&serde_json::json!({"get_updates_buf":cursor,"base_info":{"channel_version":"1.0.0"}}))
            .timeout(Duration::from_secs(50))
            .send()
            .await?;
        let value = response_json(response).await?;
        validate_updates(&value)?;
        Ok(Batch {
            messages: value
                .get("msgs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(inbound)
                .collect(),
            checkpoint: value
                .get("get_updates_buf")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    /// iLink delivers one message per context token and silently drops any
    /// further send on it, so only the first part answers the message and
    /// later parts go out unprompted.
    async fn send(
        &self,
        message: &Outbound<'_>,
        report: &(dyn Fn(bool) + Send + Sync),
    ) -> Result<SendOutcome> {
        let context_token = if message.part == 0 {
            message.reply_to
        } else {
            ""
        };
        let body = bridge::reply_body(message.to, context_token, message.text, message.client_id);
        bridge::send_reply_request(&self.client, self.token, self.base_url, &body, report).await
    }
}

/// An iLink message the bridge can identify: user text with a sender and a
/// context token to answer, or else one it only marks as seen.
fn inbound(msg: &Value) -> Option<Inbound> {
    let id = message_id(msg)?;
    let text = msg
        .get("item_list")
        .and_then(Value::as_array)
        .and_then(|xs| {
            xs.iter()
                .find_map(|x| x.get("text_item")?.get("text")?.as_str())
        })
        .filter(|text| !text.trim().is_empty());
    let sender = msg
        .get("from_user_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let ctx = msg
        .get("context_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let (Some(text), Some(sender), Some(ctx), Some(1)) = (
        text,
        sender,
        ctx,
        msg.get("message_type").and_then(Value::as_i64),
    ) else {
        return Some(Inbound::Ignored { id });
    };
    let group = match msg.get("group_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(group)) if group.is_empty() => None,
        Some(Value::String(group)) => Some(group.clone()),
        Some(other) => Some(other.to_string()),
    };
    Some(Inbound::Text(Message {
        id,
        sender: sender.into(),
        text: text.into(),
        reply_to: ctx.into(),
        group,
    }))
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
