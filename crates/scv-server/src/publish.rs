//! The publish rule `scv policy check publish` applies before a release's
//! first `cargo publish`.
//!
//! Publishing a crate to crates.io needs no question unless it would take
//! that crate past `[policy] publish_crate_versions_per_day` versions in the
//! last 24 hours. crates.io's own limit is 20 new versions per crate over a
//! rolling 24 hours, and SCV counts the same way: for each crate about to be
//! published, the versions crates.io lists (`GET
//! /api/v1/crates/<crate>/versions`, yanked ones included) whose
//! `created_at` falls in the last 24 hours. A count that cannot be fetched
//! counts as over the line. The whole release is checked before anything is
//! published, so a release that would cross the line partway never starts.
//! Over the line, the owner decides; the command line asks with
//! `confirm_ask`, where no answer in time is no. A crate already at
//! crates.io's own limit blocks the release without a question, whatever
//! the configured line: crates.io would refuse it whatever the owner
//! answered, after the crates before it were published.

use serde_json::Value;

/// crates.io's window: a rolling day.
pub const WINDOW_SECONDS: u64 = 24 * 60 * 60;
/// crates.io's own limit, which holds whatever the owner answers.
pub const CRATES_IO_LIMIT: u64 = 20;

/// One crate about to be published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub name: String,
    /// How many of its versions crates.io lists as created in the last 24
    /// hours, or why that could not be learned.
    pub recent: Result<u64, String>,
}

/// What the rule says about a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Within the limit: publish without a question.
    Go,
    /// Over the line, or a count could not be learned: ask the owner this.
    Ask(String),
    /// A crate is at crates.io's own limit, so the release could only be
    /// half-published: stop it without a question, for this reason.
    Blocked(String),
}

/// The rule for publishing `version` of `crates` under `limit` versions per
/// crate in 24 hours: each crate gains one version.
pub fn decide(limit: u64, version: &str, crates: &[Pending]) -> Verdict {
    let names: Vec<&str> = crates.iter().map(|pending| pending.name.as_str()).collect();
    let full: Vec<(&str, u64)> = crates
        .iter()
        .filter_map(|pending| match pending.recent {
            Ok(recent) if recent >= CRATES_IO_LIMIT => Some((pending.name.as_str(), recent)),
            _ => None,
        })
        .collect();
    if !full.is_empty() {
        let mut reason = format!(
            "Publishing {version} of {} is blocked: crates.io takes at most {CRATES_IO_LIMIT} \
             new versions of a crate in any 24 hours, and would refuse",
            names.join(", ")
        );
        for (name, recent) in full {
            reason.push_str(&format!("\n- {name}: {recent} in the last 24 hours"));
        }
        reason.push_str(
            "\nNothing is published until the oldest of those versions is a day old, so the \
             release is not left half-published.",
        );
        return Verdict::Blocked(reason);
    }
    let over: Vec<&Pending> = crates
        .iter()
        .filter(|pending| {
            pending
                .recent
                .as_ref()
                .map_or(true, |recent| *recent >= limit)
        })
        .collect();
    if over.is_empty() {
        return Verdict::Go;
    }
    let mut question = format!(
        "Publishing {version} of {} would pass the limit of {limit} versions of a crate in any \
         24 hours:",
        names.join(", ")
    );
    for pending in &over {
        match &pending.recent {
            Ok(recent) => question.push_str(&format!(
                "\n- {}: {recent} in the last 24 hours",
                pending.name
            )),
            Err(error) => question.push_str(&format!(
                "\n- {}: its count could not be read from crates.io ({error}), which counts as \
                 over",
                pending.name
            )),
        }
    }
    question.push_str("\nPublish anyway?");
    Verdict::Ask(question)
}

/// One page of crates.io's version list of a crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionsPage {
    /// Versions created at or after the cutoff, yanked ones included.
    pub recent: u64,
    /// The page lists a version created before the cutoff.
    pub older: bool,
    /// The query of the next page (`meta.next_page`), if any.
    pub next_page: Option<String>,
}

/// Read one page of `GET /api/v1/crates/<crate>/versions`, counting the
/// versions created at or after `since` (Unix seconds). A version whose
/// `created_at` cannot be read makes the count unknown.
pub fn read_versions_page(body: &Value, since: u64) -> Result<VersionsPage, String> {
    let versions = body
        .get("versions")
        .and_then(Value::as_array)
        .ok_or("the response lists no versions")?;
    let mut page = VersionsPage {
        recent: 0,
        older: false,
        next_page: body
            .pointer("/meta/next_page")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    for version in versions {
        let created = version
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(unix_seconds)
            .ok_or("a version has no readable created_at")?;
        if created >= since {
            page.recent += 1;
        } else {
            page.older = true;
        }
    }
    Ok(page)
}

/// Unix seconds of an RFC 3339 time such as `2026-10-30T09:15:02.123456Z`
/// or `…+00:00`.
fn unix_seconds(time: &str) -> Option<u64> {
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let text = time.get(range)?;
        if !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        text.parse().ok()
    };
    let bytes = time.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    // Skip a fraction of a second, then read the offset.
    let mut zone = 19;
    if bytes.get(zone) == Some(&b'.') {
        zone += 1;
        while bytes.get(zone).is_some_and(u8::is_ascii_digit) {
            zone += 1;
        }
    }
    let offset = match bytes.get(zone)? {
        b'Z' | b'z' if zone + 1 == bytes.len() => 0,
        sign @ (b'+' | b'-') if zone + 6 == bytes.len() && bytes[zone + 3] == b':' => {
            let minutes = number(zone + 1..zone + 3)? * 60 + number(zone + 4..zone + 6)?;
            if *sign == b'+' {
                minutes * 60
            } else {
                -minutes * 60
            }
        }
        _ => return None,
    };
    // Days since 1970-01-01 of the civil date (Howard Hinnant's algorithm).
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second - offset).ok()
}

#[cfg(test)]
mod tests;
