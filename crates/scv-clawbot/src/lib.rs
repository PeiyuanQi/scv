//! Safe, testable primitives for the WeChat iLink ClawBot adapter.

use anyhow::{Result, anyhow, bail};

pub mod bridge;
pub mod state;
pub mod protocol;

use std::{collections::{HashMap, VecDeque}, path::Path, time::{Duration, Instant}};
use serde_json::Value;
use uuid::Uuid;

pub async fn login(base: &str, account: &str) -> Result<()> {
    let client = reqwest::Client::new(); let base = normalize_base_url(base)?;
    let qr: Value = client.get(format!("{base}/ilink/bot/get_bot_qrcode?bot_type=3")).timeout(Duration::from_secs(20)).send().await?.error_for_status()?.json().await?; check_envelope(&qr)?;
    let code = qr.get("qrcode").and_then(Value::as_str).ok_or_else(|| anyhow!("login response omitted qrcode"))?;
    println!("Scan this ClawBot QR code in WeChat:\n{}", qr.get("qrcode_img_content").and_then(Value::as_str).unwrap_or(code));
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if Instant::now() >= deadline { bail!("ClawBot QR login timed out; run `scv clawbot login` again") }
        let status: Value = client.get(format!("{base}/ilink/bot/get_qrcode_status")).query(&[("qrcode", code)]).timeout(Duration::from_secs(50)).send().await?.error_for_status()?.json().await?; check_envelope(&status)?;
        match status.get("status").and_then(Value::as_str).unwrap_or("unknown") {
            "confirmed" => { let (token, bot_id, _) = bridge::validate_confirmed_login(&status)?; let host = normalize_base_url(status.get("baseurl").and_then(Value::as_str).unwrap_or(&base))?; bridge::validate_origin_pair(&base, &host)?; state::save_account(account, &state::Account { token: token.into(), base_url: host.clone() })?; println!("ClawBot login confirmed for {bot_id} at {host}."); return Ok(()); },
            "expired" => bail!("ClawBot QR code expired; run `scv clawbot login` again"), _ => {}
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

pub async fn run(token: &str, base_url: &str, account: &str, workspace: &Path) -> Result<()> {
    let client = reqwest::Client::new(); let base_url = normalize_base_url(base_url)?; let mut state = state::load_state(account)?; let mut cursor = state.cursor.clone(); let mut seen: VecDeque<String> = state.seen.iter().cloned().collect(); let mut sessions: HashMap<String, protocol::Session> = HashMap::new(); let mut backoff = Duration::from_secs(1);
    if let Some(pending) = state.pending.clone() { bridge::send_reply(&client, token, &base_url, &pending.to_user_id, &pending.context_token, &pending.reply, 16 * 1024).await?; state.pending = None; state::save_state(account, &state)?; }
    loop {
        let response: Value = match client.post(format!("{base_url}/ilink/bot/getupdates")).headers(bridge::auth_headers(token, u32::from_le_bytes(*Uuid::new_v4().as_bytes().first_chunk::<4>().unwrap()))).json(&serde_json::json!({"get_updates_buf":cursor,"base_info":{"channel_version":"1.0.0"}})).timeout(Duration::from_secs(50)).send().await { Ok(r) => match r.json().await { Ok(v) => v, Err(_) => { tokio::time::sleep(backoff).await; backoff = (backoff * 2).min(Duration::from_secs(60)); continue; } }, Err(_) => { tokio::time::sleep(backoff).await; backoff = (backoff * 2).min(Duration::from_secs(60)); continue; } };
        check_envelope(&response)?; backoff = Duration::from_secs(1); sessions.retain(|_, s| s.last_used.elapsed() < Duration::from_secs(1800));
        for msg in response.get("msgs").and_then(Value::as_array).into_iter().flatten() { if msg.get("message_type").and_then(Value::as_i64) != Some(1) { continue; } let id = msg.get("message_id").or_else(|| msg.get("msg_id")).and_then(Value::as_str).unwrap_or(""); if id.is_empty() || seen.iter().any(|x| x == id) { continue; } seen.push_back(id.into()); if seen.len() > 4096 { seen.pop_front(); } let Some(text) = msg.get("item_list").and_then(Value::as_array).and_then(|xs| xs.iter().find_map(|x| x.get("text_item")?.get("text")?.as_str())) else { continue }; let Some(sender) = msg.get("from_user_id").and_then(Value::as_str) else { continue }; let Some(ctx) = msg.get("context_token").and_then(Value::as_str) else { continue }; if !sessions.contains_key(sender) && sessions.len() >= 32 { sessions.clear(); } let session = match sessions.entry(sender.into()) { std::collections::hash_map::Entry::Occupied(e) => e.into_mut(), std::collections::hash_map::Entry::Vacant(e) => e.insert(protocol::Session::spawn(workspace).await?) }; let reply = tokio::time::timeout(Duration::from_secs(300), session.turn(text, 16 * 1024)).await.ok().and_then(Result::ok).unwrap_or_else(|| "SCV could not complete that request.".into()); state.pending = Some(state::PendingDelivery { to_user_id: sender.into(), context_token: ctx.into(), reply: reply.clone() }); state::save_state(account, &state)?; bridge::send_reply(&client, token, &base_url, sender, ctx, &reply, 16 * 1024).await?; state.pending = None; }
        if let Some(next) = response.get("get_updates_buf").and_then(Value::as_str) { cursor = next.into(); } state.cursor = cursor.clone(); state.seen = seen.iter().cloned().collect(); state::save_state(account, &state)?;
    }
}

pub fn normalize_base_url(value: &str) -> Result<String> {
    let url = reqwest::Url::parse(value.trim()).map_err(|e| anyhow!("invalid ClawBot base URL: {e}"))?;
    if url.scheme() != "https" || url.host_str().is_none() || (url.path() != "/" && !url.path().is_empty()) || url.query().is_some() || url.fragment().is_some() { bail!("ClawBot base URL must be an HTTPS origin") }
    Ok(value.trim().trim_end_matches('/').to_owned())
}

pub fn check_envelope(value: &serde_json::Value) -> Result<()> {
    let ret = value.get("ret").and_then(serde_json::Value::as_i64).ok_or_else(|| anyhow!("iLink response omitted ret"))?;
    if ret != 0 || value.get("errcode").and_then(serde_json::Value::as_i64).is_some_and(|v| v != 0) { bail!("iLink API rejected request") }
    Ok(())
}

pub fn split_utf8(value: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new(); let mut rest = value;
    while rest.len() > max { let mut end = max; while !rest.is_char_boundary(end) { end -= 1; } out.push(rest[..end].to_owned()); rest = &rest[end..]; }
    if !rest.is_empty() { out.push(rest.to_owned()); } if out.is_empty() { out.push(String::new()); } out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn validates_origins() { assert!(normalize_base_url("https://example.test").is_ok()); assert!(normalize_base_url("http://example.test").is_err()); }
    #[test] fn chunks_on_utf8_boundaries() { let chunks = split_utf8("a🙂b", 4); assert_eq!(chunks, vec!["a", "🙂", "b"]); }
    #[test] fn validates_ret() { assert!(check_envelope(&serde_json::json!({"ret":0})).is_ok()); assert!(check_envelope(&serde_json::json!({"ret":1})).is_err()); }
}
