//! Unit tests for `src/cli/policy.rs`, against a local stand-in for
//! crates.io's API: nothing reaches the network.

use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;

/// A server answering each request path from `pages` (status and body),
/// 404 for any other, and recording the paths asked for.
async fn crates_io(pages: Vec<(&'static str, u16, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&asked);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                }
            }
            let request = String::from_utf8_lossy(&request).into_owned();
            let path = request
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_owned();
            assert!(
                request.to_ascii_lowercase().contains("user-agent: scv/"),
                "{request}"
            );
            seen.lock().unwrap().push(path.clone());
            let (status, body) = pages
                .iter()
                .find(|(page, _, _)| *page == path)
                .map_or((404, String::from("{}")), |(_, status, body)| {
                    (*status, body.clone())
                });
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    (format!("http://{address}/api/v1/crates"), asked)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .unwrap()
}

/// 2026-10-30T12:00:00Z, and the day before it.
const NOW: u64 = 1_793_361_600;
const SINCE: u64 = NOW - publish::WINDOW_SECONDS;

#[tokio::test]
async fn versions_are_counted_over_the_last_day_across_pages() {
    let first = json!({
        "versions": [
            {"num": "0.4.9", "created_at": "2026-10-30T11:00:00Z", "yanked": false},
            {"num": "0.4.8", "created_at": "2026-10-30T10:00:00Z", "yanked": true}
        ],
        "meta": {"next_page": "?per_page=100&sort=date&seek=abc"}
    });
    let second = json!({
        "versions": [
            {"num": "0.4.7", "created_at": "2026-10-29T13:00:00Z", "yanked": false},
            {"num": "0.4.6", "created_at": "2026-10-28T13:00:00Z", "yanked": false}
        ],
        // Older versions were reached, so this page is the last read.
        "meta": {"next_page": "?per_page=100&sort=date&seek=def"}
    });
    let (api, asked) = crates_io(vec![
        (
            "/api/v1/crates/scv-core/versions?per_page=100&sort=date",
            200,
            first.to_string(),
        ),
        (
            "/api/v1/crates/scv-core/versions?per_page=100&sort=date&seek=abc",
            200,
            second.to_string(),
        ),
    ])
    .await;
    assert_eq!(
        recent_versions(&client(), &api, "scv-core", SINCE).await,
        Ok(3)
    );
    assert_eq!(asked.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_crate_crates_io_does_not_know_has_none_and_a_failure_is_unknown() {
    let (api, _) = crates_io(vec![
        (
            "/api/v1/crates/scv-down/versions?per_page=100&sort=date",
            503,
            "{}".into(),
        ),
        (
            "/api/v1/crates/scv-odd/versions?per_page=100&sort=date",
            200,
            "[1, 2]".into(),
        ),
    ])
    .await;
    assert_eq!(
        recent_versions(&client(), &api, "scv-new", SINCE).await,
        Ok(0)
    );
    let down = recent_versions(&client(), &api, "scv-down", SINCE).await;
    assert!(down.is_err_and(|error| error.contains("503")));
    assert!(
        recent_versions(&client(), &api, "scv-odd", SINCE)
            .await
            .is_err()
    );
}

#[test]
fn only_crate_names_reach_the_url() {
    assert!(valid_crate_name("scv-provider-openai"));
    assert!(valid_crate_name("serde_json"));
    assert!(!valid_crate_name(""));
    assert!(!valid_crate_name("../../me"));
    assert!(!valid_crate_name("scv?x=1"));
}
