//! iLink requests: authentication headers, checks on login results and
//! origins, and sending replies and uploading files with retries.

use anyhow::{Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use scv_channels::SendOutcome;
use scv_channels::retry::{Attempt, SendLabels, retry_send};
use serde_json::Value;
use uuid::Uuid;

pub fn auth_headers(token: &str, uin: u32) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "AuthorizationType",
        HeaderValue::from_static("ilink_bot_token"),
    );
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))
            .unwrap_or_else(|_| HeaderValue::from_static("Bearer")),
    );
    headers.insert(
        "X-WECHAT-UIN",
        HeaderValue::from_str(&STANDARD.encode(uin.to_le_bytes()))
            .expect("base64 is a valid header"),
    );
    headers
}

pub async fn send_reply(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    to_user_id: &str,
    context_token: &str,
    reply: &str,
    max_bytes: usize,
) -> Result<()> {
    // One message per context token: later parts go out unprompted.
    for (index, chunk) in scv_channels::split_utf8(reply, max_bytes)
        .iter()
        .enumerate()
    {
        send_reply_chunk(
            client,
            token,
            base_url,
            to_user_id,
            if index == 0 { context_token } else { "" },
            chunk,
            &Uuid::new_v4().to_string(),
        )
        .await?;
    }
    Ok(())
}

/// Deliver one reply chunk using a caller-owned client ID. The caller can
/// persist that ID before sending and reuse it after a process restart.
pub async fn send_reply_chunk(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    to_user_id: &str,
    context_token: &str,
    chunk: &str,
    client_id: &str,
) -> Result<()> {
    match send_reply_request(
        client,
        token,
        base_url,
        &reply_body(to_user_id, context_token, chunk, client_id),
        &|_| {},
    )
    .await?
    {
        SendOutcome::Delivered => Ok(()),
        SendOutcome::Rejected => bail!("iLink API rejected request"),
    }
}

pub(crate) fn reply_body(
    to_user_id: &str,
    context_token: &str,
    chunk: &str,
    client_id: &str,
) -> Value {
    item_body(
        to_user_id,
        context_token,
        serde_json::json!({"type":1,"text_item":{"text":chunk}}),
        client_id,
    )
}

/// A `sendmessage` body carrying one item: text, or a file on the CDN.
pub(crate) fn item_body(
    to_user_id: &str,
    context_token: &str,
    item: Value,
    client_id: &str,
) -> Value {
    let mut body = serde_json::json!({"msg":{"from_user_id":"","to_user_id":to_user_id,"client_id":client_id,"message_type":2,"message_state":2,"item_list":[item]},"base_info":{"channel_version":"1.0.0"}});
    // Without a context token the message is unprompted: it answers nothing.
    if !context_token.is_empty() {
        body["msg"]["context_token"] = context_token.into();
    }
    body
}

/// Upload an encrypted file for `to_user_id`: ask `getuploadurl` where,
/// then post it to the CDN. Returns the CDN's download parameter for the
/// message, or `None` when iLink or the CDN refuses the file outright.
/// Transient failures are retried, then fail.
pub(crate) async fn upload(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    to_user_id: &str,
    upload: &crate::media::Upload,
    report: &(dyn Fn(bool) + Send + Sync),
) -> Result<Option<String>> {
    let labels = SendLabels {
        refused: "ClawBot file upload refused",
        failed: "ClawBot file upload failed",
        gave_up: "ClawBot could not upload the file",
    };
    retry_send(labels, report, || async {
        let result = async {
            let response = client
                .post(format!("{base_url}/ilink/bot/getuploadurl"))
                .headers(auth_headers(token, rand_u32()))
                .json(&upload.request(to_user_id))
                .timeout(crate::REQUEST_TIMEOUT)
                .send()
                .await?;
            let status = response.status();
            if status.is_client_error() && !retryable_client_error(status) {
                return Ok(Attempt::Refused(format!(
                    "upload address HTTP status {}",
                    status.as_u16()
                )));
            }
            let body = crate::response_body(response).await?;
            if let Err(rejection) = crate::check_send_ack(&body) {
                return Ok(Attempt::Refused(format!(
                    "upload address refused: {rejection}"
                )));
            }
            let reply: Value = serde_json::from_slice(&body)
                .map_err(|_| anyhow!("invalid upload address response"))?;
            let target = match upload.target(&reply) {
                Ok(target) => target,
                Err(error) => return Ok(Attempt::Refused(error.to_string())),
            };
            let response = client
                .post(target)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(upload.encrypted.clone())
                .timeout(crate::CDN_TIMEOUT)
                .send()
                .await
                .map_err(|_| anyhow!("CDN upload failed"))?;
            let status = response.status();
            if status.is_client_error() {
                return Ok(Attempt::Refused(format!(
                    "CDN upload HTTP status {}",
                    status.as_u16()
                )));
            }
            if !status.is_success() {
                bail!("CDN upload HTTP status {}", status.as_u16())
            }
            let param = response
                .headers()
                .get("x-encrypted-param")
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow!("CDN upload reply omitted its download parameter"))?;
            Ok::<_, anyhow::Error>(Attempt::Done(param.to_owned()))
        }
        .await;
        result.unwrap_or_else(|error| Attempt::Retry(error.to_string()))
    })
    .await
}

pub(crate) async fn send_reply_request(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    body: &Value,
    report: &(dyn Fn(bool) + Send + Sync),
) -> Result<SendOutcome> {
    let labels = SendLabels {
        refused: "ClawBot reply rejected by iLink",
        failed: "ClawBot reply send failed",
        gave_up: "ClawBot could not deliver the reply",
    };
    let sent = retry_send(labels, report, || async {
        let result = async {
            let response = client
                .post(format!("{base_url}/ilink/bot/sendmessage"))
                .headers(auth_headers(token, rand_u32()))
                .json(body)
                .timeout(crate::REQUEST_TIMEOUT)
                .send()
                .await?;
            let status = response.status();
            // A permanent client error cannot succeed on resend; auth,
            // timeout and rate-limit statuses stay retryable.
            if status.is_client_error() && !retryable_client_error(status) {
                return Ok(Attempt::Refused(format!("HTTP status {}", status.as_u16())));
            }
            let body = crate::response_body(response).await?;
            Ok::<_, anyhow::Error>(match crate::check_send_ack(&body) {
                Ok(()) => Attempt::Done(()),
                Err(rejection) => Attempt::Refused(rejection),
            })
        }
        .await;
        result.unwrap_or_else(|error| Attempt::Retry(error.to_string()))
    })
    .await?;
    Ok(match sent {
        Some(()) => SendOutcome::Delivered,
        None => SendOutcome::Rejected,
    })
}

/// Auth, timeout, and rate-limit statuses can succeed on resend; any other
/// client error cannot.
fn retryable_client_error(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 401 | 408 | 429)
}

fn rand_u32() -> u32 {
    u32::from_le_bytes(
        *Uuid::new_v4()
            .as_bytes()
            .first_chunk::<4>()
            .expect("uuid has four bytes"),
    )
}

pub fn validate_confirmed_login(status: &Value) -> Result<(&str, &str, &str)> {
    let token = status
        .get("bot_token")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("confirmed login omitted bot_token"))?;
    let bot_id = status
        .get("ilink_bot_id")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("confirmed login omitted ilink_bot_id"))?;
    let user_id = status
        .get("ilink_user_id")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("confirmed login omitted ilink_user_id"))?;
    Ok((token, bot_id, user_id))
}

pub fn validate_origin_pair(expected: &str, returned: &str) -> Result<()> {
    let expected = reqwest::Url::parse(expected)?;
    let returned = reqwest::Url::parse(returned)?;
    // iLink may direct a successful login to a regional host. Keep the
    // returned origin constrained to HTTPS and the normal TLS port while
    // allowing that documented host switch.
    let expected_host = expected.host_str().unwrap_or_default();
    let returned_host = returned.host_str().unwrap_or_default();
    let trusted_host = returned_host == expected_host
        || (expected_host.ends_with(".weixin.qq.com") && returned_host.ends_with(".weixin.qq.com"));
    if expected.scheme() != "https"
        || returned.scheme() != "https"
        || !trusted_host
        || !returned.username().is_empty()
        || returned.password().is_some()
        || returned.port_or_known_default() != Some(443)
        || (returned.path() != "/" && !returned.path().is_empty())
        || returned.query().is_some()
        || returned.fragment().is_some()
    {
        bail!("ClawBot login returned an unexpected API origin")
    }
    Ok(())
}

/// Parse and validate a fake or real iLink JSON response without exposing
/// bearer tokens or server diagnostics to callers.
pub fn parse_response(body: &[u8]) -> Result<Value> {
    if body.len() > crate::MAX_RESPONSE_BYTES {
        bail!("iLink response exceeds limit")
    }
    let value: Value =
        serde_json::from_slice(body).map_err(|e| anyhow!("invalid iLink JSON: {e}"))?;
    crate::check_envelope(&value)?;
    Ok(value)
}

#[cfg(test)]
mod tests;
