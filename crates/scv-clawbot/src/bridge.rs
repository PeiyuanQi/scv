//! iLink bridge primitives shared by the CLI adapter and integration tests.

use anyhow::{Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::Value;
use std::time::Duration;
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
    for chunk in crate::split_utf8(reply, max_bytes) {
        send_reply_chunk(
            client,
            token,
            base_url,
            to_user_id,
            context_token,
            &chunk,
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
    send_reply_request(
        client,
        token,
        base_url,
        &reply_body(to_user_id, context_token, chunk, client_id),
        &|_| {},
    )
    .await
}

pub(crate) fn reply_body(
    to_user_id: &str,
    context_token: &str,
    chunk: &str,
    client_id: &str,
) -> Value {
    serde_json::json!({"msg":{"from_user_id":"","to_user_id":to_user_id,"client_id":client_id,"message_type":2,"message_state":2,"context_token":context_token,"item_list":[{"type":1,"text_item":{"text":chunk}}]},"base_info":{"channel_version":"1.0.0"}})
}

pub(crate) async fn send_reply_request(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    body: &Value,
    report: &(dyn Fn(bool) + Send + Sync),
) -> Result<()> {
    let mut delay = Duration::from_secs(1);
    for attempt in 0..3 {
        let result = async {
            let response = client
                .post(format!("{base_url}/ilink/bot/sendmessage"))
                .headers(auth_headers(token, rand_u32()))
                .json(body)
                .timeout(Duration::from_secs(20))
                .send()
                .await?;
            let response = crate::response_json(response).await?;
            crate::check_send_ack(&response)
        }
        .await;
        if result.is_ok() {
            return Ok(());
        }
        report(false);
        if attempt == 2 {
            return Err(anyhow!("ClawBot could not deliver the reply"));
        }
        tokio::time::sleep(delay).await;
        delay *= 2;
    }
    unreachable!("delivery loop returns after success or final attempt")
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
mod tests {
    use super::*;
    #[test]
    fn login_requires_ids() {
        assert!(validate_confirmed_login(&serde_json::json!({"bot_token":"t"})).is_err());
    }
    #[test]
    fn origin_allows_returned_regional_host() {
        assert!(
            validate_origin_pair(
                "https://ilinkai.weixin.qq.com",
                "https://region.weixin.qq.com"
            )
            .is_ok()
        );
    }
    #[test]
    fn origin_pins_tls_port() {
        assert!(validate_origin_pair("https://x.test:443", "https://x.test:444").is_err());
    }
    #[test]
    fn origin_rejects_untrusted_host() {
        assert!(validate_origin_pair("https://login.test", "https://attacker.test").is_err());
    }
    #[test]
    fn fake_response_requires_success_ret() {
        assert!(parse_response(br#"{"ret":0,"msgs":[]}"#).is_ok());
        assert!(parse_response(br#"{"ret":-14,"errmsg":"expired"}"#).is_err());
    }

    #[test]
    fn parse_response_rejects_oversized_body() {
        assert!(parse_response(&vec![b' '; crate::MAX_RESPONSE_BYTES + 1]).is_err());
    }
}
