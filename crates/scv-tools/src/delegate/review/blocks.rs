//! The fenced blocks SCV reads from a reviewed job's delegated replies: the
//! reviewer's `scv-verdict` and `scv-landing-check`, and the builder's
//! `scv-landing`. They are data, never prose: only the last block with the
//! info string counts, its size is bounded, its JSON is checked against a
//! schema, and its text is cleaned and cut. Nothing else in a reply is read.

use serde_json::{Map, Value};

/// A reviewer's verdict block.
pub(crate) const VERDICT_BLOCK: &str = "scv-verdict";
/// A builder's landing report.
pub(crate) const LANDING_BLOCK: &str = "scv-landing";
/// The approving reviewer's landing confirmation.
pub(crate) const CHECK_BLOCK: &str = "scv-landing-check";

const MAX_VERDICT_BYTES: usize = 16 * 1024;
const MAX_LANDING_BYTES: usize = 4 * 1024;
const MAX_FINDINGS: usize = 20;
const MAX_EVIDENCE: usize = 20;
const MAX_CHECK_EVIDENCE: usize = 5;
const MAX_COMMITS: usize = 20;
const SUMMARY_CHARS: usize = 1000;
const NOTE_CHARS: usize = 500;
const TITLE_CHARS: usize = 200;
const DETAIL_CHARS: usize = 2000;
const LOCATION_CHARS: usize = 300;
const EVIDENCE_CHARS: usize = 300;
const LANDING_TEXT_CHARS: usize = 300;
const MAX_REF_CHARS: usize = 200;
const MAX_URL_BYTES: usize = 300;

/// What a reviewer decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Approve,
    Changes,
    Escalate,
}

impl Decision {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Changes => "changes",
            Self::Escalate => "escalate",
        }
    }
}

/// How a verdict settles an open finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Settlement {
    Resolved,
    Open,
    /// The reviewer accepts the builder's reasons.
    Withdrawn,
}

impl Settlement {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Open => "open",
            Self::Withdrawn => "withdrawn",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Settled {
    pub(crate) id: String,
    pub(crate) status: Settlement,
    pub(crate) note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    /// A real defect, which keeps the review from approving.
    Blocking,
    /// Style or preference; rides along in a fix round that happens anyway.
    Minor,
}

impl Severity {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Blocking => "blocking",
            Self::Minor => "minor",
        }
    }
}

/// A finding as a reviewer raised it, before SCV numbers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewFinding {
    pub(crate) severity: Severity,
    pub(crate) title: String,
    pub(crate) detail: Option<String>,
    pub(crate) location: Option<String>,
}

/// The commits an approving reviewer approved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Approved {
    pub(crate) base: String,
    pub(crate) head: String,
}

/// A reviewer's verdict, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub(crate) decision: Decision,
    pub(crate) summary: String,
    pub(crate) prior: Vec<Settled>,
    pub(crate) findings: Vec<NewFinding>,
    pub(crate) evidence: Vec<String>,
    pub(crate) landing_check: Option<LandingCheck>,
    pub(crate) approved: Option<Approved>,
}

/// What a verdict must hold in its round.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Expect<'a> {
    /// The IDs of the blocking findings still open, which `prior` settles.
    pub(crate) open: &'a [String],
    /// The builder reported a landing this round: `landing_check` is required.
    pub(crate) landed: bool,
    /// The call lands after approval: an `approve` names `approved`.
    pub(crate) after_approval: bool,
}

/// A reviewer's check of landed commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckStatus {
    /// The commits are on the ref and carry the expected change.
    Confirmed,
    /// They are not on the ref.
    NotFound,
    /// They differ from the expected change.
    Mismatch,
    /// The reviewer could not check.
    Unverifiable,
}

impl CheckStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::NotFound => "not_found",
            Self::Mismatch => "mismatch",
            Self::Unverifiable => "unverifiable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LandingCheck {
    pub(crate) status: CheckStatus,
    pub(crate) reference: Option<String>,
    pub(crate) commits: Vec<String>,
    pub(crate) evidence: Vec<String>,
    pub(crate) note: Option<String>,
}

/// What a builder reported of its landing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportStatus {
    Landed,
    NotLanded,
    /// Attempted; partial outward state is possible.
    Failed,
}

impl ReportStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Landed => "landed",
            Self::NotLanded => "not_landed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LandingReport {
    pub(crate) status: ReportStatus,
    /// Set for `landed`.
    pub(crate) reference: Option<String>,
    /// Non-empty for `landed`.
    pub(crate) commits: Vec<String>,
    pub(crate) url: Option<String>,
    pub(crate) detail: Option<String>,
}

/// The content of the last fenced block in `reply` whose info string is
/// `info`. `truncated` says the reply was cut, so its last block may be
/// lost.
fn last_block(reply: &str, info: &str, truncated: bool) -> Result<String, String> {
    let mut found = None;
    let mut open: Option<(char, usize, bool, Vec<&str>)> = None;
    for line in reply.lines() {
        if let Some((fence, length, ours, lines)) = &mut open {
            if closes(line, *fence, *length) {
                if *ours {
                    found = Some(lines.join("\n"));
                }
                open = None;
            } else if *ours {
                lines.push(line);
            }
            continue;
        }
        if let Some((fence, length, rest)) = opens(line) {
            let ours = rest.split_whitespace().next() == Some(info);
            open = Some((fence, length, ours, Vec::new()));
        }
    }
    // A cut reply may have lost its last block, which is the one that counts.
    if truncated || open.is_some_and(|(_, _, ours, _)| ours) {
        return Err(format!(
            "the reply was cut off before its {info} block ended"
        ));
    }
    found.ok_or_else(|| format!("no {info} block"))
}

/// A fence that opens a block: up to three spaces, at least three backticks
/// or tildes, then the info string.
fn opens(line: &str) -> Option<(char, usize, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let fence = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let length = rest.chars().take_while(|c| *c == fence).count();
    if length < 3 {
        return None;
    }
    let info = &rest[length..];
    // A backtick fence's info string has no backticks.
    if fence == '`' && info.contains('`') {
        return None;
    }
    Some((fence, length, info.trim()))
}

/// Whether `line` closes a block opened with `length` of `fence`.
fn closes(line: &str, fence: char, length: usize) -> bool {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return false;
    }
    let rest = line[indent..].trim_end();
    rest.chars().count() >= length && rest.chars().all(|c| c == fence)
}

/// The block's JSON object, within `max` bytes.
fn object(content: &str, info: &str, max: usize) -> Result<Map<String, Value>, String> {
    if content.len() > max {
        return Err(format!("the {info} block is over {} KiB", max / 1024));
    }
    match serde_json::from_str::<Value>(content) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err(format!("the {info} block is not a JSON object")),
        Err(error) => Err(format!("the {info} block is not valid JSON: {error}")),
    }
}

/// Parse and check the reviewer's verdict in `reply`.
pub(crate) fn parse_verdict(
    reply: &str,
    truncated: bool,
    expect: &Expect<'_>,
) -> Result<Verdict, String> {
    let content = last_block(reply, VERDICT_BLOCK, truncated)?;
    let object = object(&content, VERDICT_BLOCK, MAX_VERDICT_BYTES)?;
    let decision = match required_text(&object, "verdict", 32)?.as_str() {
        "approve" => Decision::Approve,
        "changes" => Decision::Changes,
        "escalate" => Decision::Escalate,
        other => {
            return Err(format!(
                "verdict {other:?} is not approve, changes, or escalate"
            ));
        }
    };
    let summary = required_text(&object, "summary", SUMMARY_CHARS)?;
    let prior = prior(&object, expect.open)?;
    let findings = items(&object, "findings", MAX_FINDINGS)?
        .iter()
        .map(new_finding)
        .collect::<Result<Vec<_>, _>>()?;
    let evidence = texts(&object, "evidence", MAX_EVIDENCE, EVIDENCE_CHARS)?;
    let landing_check = if expect.landed {
        let value = object
            .get("landing_check")
            .filter(|value| !value.is_null())
            .ok_or("landing_check is required: the builder reported a landing this round")?;
        Some(landing_check(value)?)
    } else {
        None
    };
    let still_open = prior
        .iter()
        .any(|settled| settled.status == Settlement::Open);
    let blocking = findings
        .iter()
        .any(|finding| finding.severity == Severity::Blocking);
    let mut approved = None;
    match decision {
        Decision::Approve => {
            if still_open {
                return Err("approve leaves a prior finding open".into());
            }
            if blocking {
                return Err("approve raises a new blocking finding".into());
            }
            if evidence.is_empty() {
                return Err("approve needs evidence: what you ran or read".into());
            }
            if expect.after_approval {
                let value = object
                    .get("approved")
                    .filter(|value| !value.is_null())
                    .ok_or("approve must name the approved base and head commits")?;
                approved = Some(approved_range(value)?);
            }
        }
        Decision::Changes if !still_open && !blocking => {
            return Err("changes needs an open prior finding or a new blocking one".into());
        }
        Decision::Changes | Decision::Escalate => {}
    }
    Ok(Verdict {
        decision,
        summary,
        prior,
        findings,
        evidence,
        landing_check,
        approved,
    })
}

/// `prior`: exactly one entry per open finding, by its ID.
fn prior(object: &Map<String, Value>, open: &[String]) -> Result<Vec<Settled>, String> {
    let entries = items(object, "prior", usize::MAX)?;
    let mut settled: Vec<Settled> = Vec::new();
    for entry in entries {
        let entry = entry
            .as_object()
            .ok_or("each prior entry must be an object")?;
        let id = required_text(entry, "id", 16)?;
        if !open.contains(&id) {
            return Err(format!("prior names {id:?}, which is not an open finding"));
        }
        if settled.iter().any(|earlier| earlier.id == id) {
            return Err(format!("prior settles {id} twice"));
        }
        let status = match required_text(entry, "status", 16)?.as_str() {
            "resolved" => Settlement::Resolved,
            "open" => Settlement::Open,
            "withdrawn" => Settlement::Withdrawn,
            other => {
                return Err(format!(
                    "prior status {other:?} is not resolved, open, or withdrawn"
                ));
            }
        };
        let note = optional_text(entry, "note", NOTE_CHARS)?;
        if status == Settlement::Withdrawn && note.is_none() {
            return Err(format!("prior {id} is withdrawn without a note saying why"));
        }
        settled.push(Settled { id, status, note });
    }
    if let Some(missing) = open
        .iter()
        .find(|id| !settled.iter().any(|settled| &settled.id == *id))
    {
        return Err(format!("prior must settle open finding {missing}"));
    }
    Ok(settled)
}

fn new_finding(value: &Value) -> Result<NewFinding, String> {
    let object = value.as_object().ok_or("each finding must be an object")?;
    let severity = match required_text(object, "severity", 16)?.as_str() {
        "blocking" => Severity::Blocking,
        "minor" => Severity::Minor,
        other => {
            return Err(format!(
                "finding severity {other:?} is not blocking or minor"
            ));
        }
    };
    Ok(NewFinding {
        severity,
        title: required_text(object, "title", TITLE_CHARS)?,
        detail: optional_text(object, "detail", DETAIL_CHARS)?,
        location: optional_text(object, "location", LOCATION_CHARS)?,
    })
}

fn approved_range(value: &Value) -> Result<Approved, String> {
    let object = value
        .as_object()
        .ok_or("approved must be an object with base and head")?;
    let commit = |key: &str| -> Result<String, String> {
        let value = object
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("approved.{key} must be a commit"))?;
        if is_commit(value) {
            Ok(value.to_owned())
        } else {
            Err(format!(
                "approved.{key} {:?} is not 7 to 40 lowercase hex characters",
                cut(value, 64)
            ))
        }
    };
    Ok(Approved {
        base: commit("base")?,
        head: commit("head")?,
    })
}

/// A `landing_check` object, in a verdict or a confirmation block.
fn landing_check(value: &Value) -> Result<LandingCheck, String> {
    let object = value.as_object().ok_or("landing_check must be an object")?;
    let status = match required_text(object, "status", 16)?.as_str() {
        "confirmed" => CheckStatus::Confirmed,
        "not_found" => CheckStatus::NotFound,
        "mismatch" => CheckStatus::Mismatch,
        "unverifiable" => CheckStatus::Unverifiable,
        other => {
            return Err(format!(
                "landing_check status {other:?} is not confirmed, not_found, mismatch, or \
                 unverifiable"
            ));
        }
    };
    let reference = reference(object)?;
    let commits = commits(object)?;
    let evidence = texts(object, "evidence", MAX_CHECK_EVIDENCE, EVIDENCE_CHARS)?;
    let note = optional_text(object, "note", LANDING_TEXT_CHARS)?;
    match status {
        CheckStatus::Confirmed if evidence.is_empty() => {
            return Err("a confirmed landing_check needs evidence".into());
        }
        CheckStatus::Confirmed if reference.is_none() || commits.is_empty() => {
            return Err("a confirmed landing_check names its ref and commits".into());
        }
        CheckStatus::Unverifiable if note.is_none() => {
            return Err("an unverifiable landing_check needs a note saying why".into());
        }
        _ => {}
    }
    Ok(LandingCheck {
        status,
        reference,
        commits,
        evidence,
        note,
    })
}

/// Parse the approving reviewer's `scv-landing-check` block in `reply`.
pub(crate) fn parse_check(reply: &str, truncated: bool) -> Result<LandingCheck, String> {
    let content = last_block(reply, CHECK_BLOCK, truncated)?;
    let object = object(&content, CHECK_BLOCK, MAX_LANDING_BYTES)?;
    landing_check(&Value::Object(object))
}

/// Parse the builder's `scv-landing` block in `reply`: `None` when the
/// reply has none.
pub(crate) fn parse_landing(reply: &str, truncated: bool) -> Result<Option<LandingReport>, String> {
    let content = match last_block(reply, LANDING_BLOCK, truncated) {
        Ok(content) => content,
        Err(_) if !mentions_block(reply, LANDING_BLOCK) && !truncated => return Ok(None),
        Err(error) => return Err(error),
    };
    let object = object(&content, LANDING_BLOCK, MAX_LANDING_BYTES)?;
    let status = match required_text(&object, "status", 16)?.as_str() {
        "landed" => ReportStatus::Landed,
        "not_landed" => ReportStatus::NotLanded,
        "failed" => ReportStatus::Failed,
        other => {
            return Err(format!(
                "landing status {other:?} is not landed, not_landed, or failed"
            ));
        }
    };
    let (reference, commits) = if status == ReportStatus::Landed {
        let reference = reference(&object)?.ok_or("a landed report names its ref")?;
        let commits = commits(&object)?;
        if commits.is_empty() {
            return Err("a landed report names its commits".into());
        }
        (Some(reference), commits)
    } else {
        (None, Vec::new())
    };
    // The URL is shown nowhere, so one that does not fit is dropped.
    let url = object
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| {
            url.starts_with("https://")
                && url.len() <= MAX_URL_BYTES
                && !url.chars().any(|c| c.is_whitespace() || c.is_control())
        })
        .map(str::to_owned);
    Ok(Some(LandingReport {
        status,
        reference,
        commits,
        url,
        detail: optional_text(&object, "detail", LANDING_TEXT_CHARS)?,
    }))
}

/// Whether `reply` opens a block with `info` at all, complete or not.
fn mentions_block(reply: &str, info: &str) -> bool {
    reply
        .lines()
        .filter_map(opens)
        .any(|(_, _, rest)| rest.split_whitespace().next() == Some(info))
}

fn reference(object: &Map<String, Value>) -> Result<Option<String>, String> {
    let Some(value) = object.get("ref").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let value = value.as_str().ok_or("ref must be a string")?;
    let valid = !value.is_empty()
        && value.chars().count() <= MAX_REF_CHARS
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c));
    if valid {
        Ok(Some(value.to_owned()))
    } else {
        Err(format!(
            "ref {:?} is not 1 to {MAX_REF_CHARS} characters of letters, digits, '.', '_', '/', \
             or '-'",
            cut(value, 64)
        ))
    }
}

fn commits(object: &Map<String, Value>) -> Result<Vec<String>, String> {
    let values = items(object, "commits", MAX_COMMITS)?;
    values
        .iter()
        .map(|value| {
            let commit = value.as_str().ok_or("each commit must be a string")?;
            if is_commit(commit) {
                Ok(commit.to_owned())
            } else {
                Err(format!(
                    "commit {:?} is not 7 to 40 lowercase hex characters",
                    cut(commit, 64)
                ))
            }
        })
        .collect()
}

/// 7 to 40 lowercase hex characters.
pub(crate) fn is_commit(value: &str) -> bool {
    (7..=40).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// Whether two commit IDs name the same commit: one is a prefix of the other.
pub(crate) fn same_commit(a: &str, b: &str) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// An array under `key`, at most `max` long; absent or null is empty.
fn items<'a>(object: &'a Map<String, Value>, key: &str, max: usize) -> Result<&'a [Value], String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(values)) if values.len() > max => Err(format!(
            "{key} has {} entries, more than {max}",
            values.len()
        )),
        Some(Value::Array(values)) => Ok(values),
        Some(_) => Err(format!("{key} must be an array")),
    }
}

/// An array of strings under `key`, each cleaned and cut; empty ones dropped.
fn texts(
    object: &Map<String, Value>,
    key: &str,
    max: usize,
    chars: usize,
) -> Result<Vec<String>, String> {
    items(object, key, max)?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(|text| clean(text, chars))
                .ok_or_else(|| format!("each {key} entry must be a string"))
        })
        .filter(|text| !matches!(text, Ok(text) if text.is_empty()))
        .collect()
}

fn required_text(object: &Map<String, Value>, key: &str, chars: usize) -> Result<String, String> {
    optional_text(object, key, chars)?.ok_or_else(|| format!("{key} is required"))
}

/// A string under `key`, cleaned and cut; absent, null, or blank is `None`.
fn optional_text(
    object: &Map<String, Value>,
    key: &str,
    chars: usize,
) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(clean(text, chars)).filter(|text| !text.is_empty())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

/// `text` with line breaks and tabs as spaces, other control characters
/// removed, trimmed, and cut to `chars` characters with `…`.
pub(crate) fn clean(text: &str, chars: usize) -> String {
    let flat: String = text
        .chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    cut(flat.trim(), chars)
}

/// `text` cut to `chars` characters, with `…` when cut.
pub(crate) fn cut(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(chars.saturating_sub(1)).collect();
    short.push('…');
    short
}

#[cfg(test)]
mod tests;
