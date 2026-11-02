//! `scv policy check publish`: SCV's publish rule, for a script about to
//! publish crates to crates.io (`feature-flow`'s `publish.sh`).
//!
//! It counts each crate's versions crates.io lists as created in the last
//! 24 hours, yanked ones included, and decides with the rule in
//! `scv_server::publish`. Within the limit it says go. Over the line, or
//! when a count cannot be fetched, it asks the owner in chat as `scv
//! confirm` does, and no answer in time is no. A crate already at
//! crates.io's own limit is a no without a question. The exit status
//! matches `scv confirm`: 0 go, 1 no, 2 could not decide or ask.

use std::time::Duration;

use anyhow::{Context, Result};
use scv_client::Layout;
use scv_protocol::{DEFAULT_CONFIRM_SECONDS, MAX_CONFIRM_SECONDS};
use scv_server::{
    config::{Config, ConfigOverrides},
    publish::{self, Pending, Verdict},
};

use super::args::{PolicyCommand, PolicyStep};

/// crates.io's API, whose `<crate>/versions` lists every version.
const CRATES_IO_API: &str = "https://crates.io/api/v1/crates";
/// crates.io refuses requests without a user agent that names the client.
const USER_AGENT: &str = concat!(
    "scv/",
    env!("CARGO_PKG_VERSION"),
    " (scv policy check publish)"
);
/// How long one request to crates.io may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Versions per page, newest first.
const PER_PAGE: u32 = 100;
/// The most pages read for one crate; a day's versions fit in the first.
const MAX_PAGES: usize = 10;
/// The rule could not be applied, or the owner could not be asked.
const UNDECIDED: i32 = 2;

pub(crate) async fn run(
    layout: &Layout,
    overrides: &ConfigOverrides,
    cwd: &std::path::Path,
    command: PolicyCommand,
) -> Result<()> {
    let PolicyCommand::Check {
        step: PolicyStep::Publish { version, crates },
    } = command;
    match check_publish(layout, overrides, cwd, &version, &crates).await {
        0 => Ok(()),
        code => std::process::exit(code),
    }
}

async fn check_publish(
    layout: &Layout,
    overrides: &ConfigOverrides,
    cwd: &std::path::Path,
    version: &str,
    crates: &[String],
) -> i32 {
    if let Some(name) = crates.iter().find(|name| !valid_crate_name(name)) {
        eprintln!("scv policy: {name:?} is not a crate name; nothing was decided");
        return UNDECIDED;
    }
    // The configuration files alone, as `scv update` reads them.
    let files = ConfigOverrides {
        config_file: overrides.config_file.clone(),
        ..ConfigOverrides::default()
    };
    let limit = match Config::load(layout, cwd, files) {
        Ok(config) => config.publish_crate_versions_per_day(),
        Err(error) => {
            eprintln!("scv policy: cannot read the configuration: {error:#}; nothing was decided");
            return UNDECIDED;
        }
    };
    let client = match reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            eprintln!("scv policy: cannot reach crates.io: {error}; nothing was decided");
            return UNDECIDED;
        }
    };
    let since = unix_now().saturating_sub(publish::WINDOW_SECONDS);
    let mut pending = Vec::with_capacity(crates.len());
    for name in crates {
        let recent = recent_versions(&client, CRATES_IO_API, name, since).await;
        match &recent {
            Ok(recent) => println!("  {name}: {recent} new in the last 24 hours"),
            Err(error) => println!("  {name}: unknown ({error}), which counts as over"),
        }
        pending.push(Pending {
            name: name.clone(),
            recent,
        });
    }
    match publish::decide(limit, version, &pending) {
        Verdict::Go => {
            println!(
                "Publishing {version} stays within {limit} versions of each crate in 24 hours; \
                 go."
            );
            0
        }
        Verdict::Ask(question) => {
            println!("Past {limit} versions of a crate in 24 hours, so the owner decides.");
            super::confirm::ask(layout, question, confirm_timeout()).await
        }
        Verdict::Blocked(reason) => {
            eprintln!("scv policy: {reason}");
            1
        }
    }
}

/// How long the owner's answer is waited for: `SCV_CONFIRM_TIMEOUT`
/// seconds, as `publish.sh` passes `scv confirm`, else 30 minutes.
fn confirm_timeout() -> u64 {
    std::env::var("SCV_CONFIRM_TIMEOUT")
        .ok()
        .and_then(|seconds| seconds.trim().parse::<u64>().ok())
        .filter(|seconds| (1..=MAX_CONFIRM_SECONDS).contains(seconds))
        .unwrap_or(DEFAULT_CONFIRM_SECONDS)
}

/// A name crates.io could have published, safe in a URL path.
fn valid_crate_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// How many versions of crate `name` the API at `api` lists as created at
/// or after `since`, yanked ones included, reading pages newest first
/// until one reaches older versions. A crate crates.io does not know has
/// none.
async fn recent_versions(
    client: &reqwest::Client,
    api: &str,
    name: &str,
    since: u64,
) -> Result<u64, String> {
    let mut query = format!("?per_page={PER_PAGE}&sort=date");
    let mut recent = 0;
    for _ in 0..MAX_PAGES {
        let url = format!("{api}/{name}/versions{query}");
        let body = match fetch(client, &url).await {
            Ok(Some(body)) => body,
            Ok(None) => return Ok(recent),
            Err(error) => return Err(format!("{error:#}")),
        };
        let page = publish::read_versions_page(&body, since)?;
        recent += page.recent;
        match page.next_page {
            Some(next) if !page.older => query = next,
            _ => return Ok(recent),
        }
    }
    Err(format!("more than {MAX_PAGES} pages of versions"))
}

/// The JSON at `url`, or `None` when crates.io has no such crate.
async fn fetch(client: &reqwest::Client, url: &str) -> Result<Option<serde_json::Value>> {
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .context("request")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let response = response.error_for_status().context("crates.io")?;
    Ok(Some(response.json().await.context("read the response")?))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests;
