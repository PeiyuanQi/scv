//! Reviewed background jobs: what SCV itself decided about a job that ran
//! with an independent reviewer, and what its builder landed. Every value
//! comes from SCV's own state; reviewer and builder text appears only as
//! bounded, cleaned fields that [`describe_outcome`] quotes.

use serde::{Deserialize, Serialize};

/// A reviewed job's review outcome and landing status, which clients show
/// as SCV's own lines apart from the model's summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobOutcome {
    /// The job's handle, such as `job-4`.
    pub job: String,
    /// How the review ended.
    pub review: ReviewSummary,
    /// What the builder landed, apart from the review.
    pub landing: LandingSummary,
}

/// How a reviewed job's review ended.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewSummary {
    /// The outcome; only [`ReviewOutcome::Approved`] means approved.
    pub outcome: ReviewOutcome,
    /// Why it ended so, such as `round_limit` or `reviewer_timeout`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The round it ended in; 0 when no builder turn ran.
    pub round: u32,
    /// The call's round limit.
    pub rounds: u32,
    /// The agent of the last reviewer attempt; empty when none ran.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reviewer: String,
    /// Why the reviewer is not the first in the order, such as
    /// `codex unavailable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    /// Each reviewer agent tried in the job, with its latest result.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tried: Vec<TriedReviewer>,
    /// What reviewers that declined said, so the user can be told.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refusals: Vec<Refusal>,
    /// The last verdict's summary, as the reviewer wrote it, bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Blocking findings still open.
    #[serde(default)]
    pub open_count: u32,
    /// The first of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open: Vec<OpenFinding>,
    /// The review journal's ID, such as `rev-1759961234-3fa9c1`.
    pub journal: String,
    /// A journal write failed after the outcome was decided, so the journal
    /// lacks later steps. The outcome stands; a failure before it was
    /// decided is the outcome `stopped` (`journal_error`) instead.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub journal_incomplete: bool,
    /// The job had not stopped when this outcome was stated, so its journal
    /// was still being written: the outcome is decided, and a
    /// `background.updated` event follows once the job stops, saying whether
    /// the journal ended complete.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub journal_pending: bool,
}

/// How a reviewed job's review ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReviewOutcome {
    /// A reviewer approved the work. The only approval.
    Approved,
    /// The last round still had open blocking findings.
    Unresolved,
    /// The reviewer said the user must decide.
    Escalated,
    /// No usable verdict: no reviewer, or it declined, failed, timed out,
    /// or stayed malformed.
    NoVerdict,
    /// A builder turn did not complete, the job was cancelled, or the
    /// journal could not be written.
    Stopped,
    /// An outcome this client does not know, from a newer server.
    #[serde(other)]
    Unknown,
}

/// One reviewer agent a reviewed job tried.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TriedReviewer {
    /// The agent, such as `claude`.
    pub agent: String,
    /// How its latest attempt ended.
    pub result: ReviewerResult,
}

/// How one reviewer attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReviewerResult {
    /// It gave a verdict.
    Verdict,
    /// It was unavailable: missing, signed out, or its provider failed.
    Unavailable,
    /// It declined the request.
    Declined,
    /// It failed, timed out, was stopped, or its verdict stayed malformed.
    Failed,
    /// No conversation slot was free to start it.
    NoSlot,
    /// A result this client does not know, from a newer server.
    #[serde(other)]
    Unknown,
}

/// What a reviewer that declined said.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Refusal {
    /// The agent that declined.
    pub agent: String,
    /// Its reply, bounded: untrusted delegated-agent output.
    pub reply: String,
}

/// A blocking finding still open when the review ended.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenFinding {
    /// SCV's number for it, `<round>.<n>`.
    pub id: String,
    /// Its title, as the reviewer wrote it, bounded.
    pub title: String,
    /// Where, as the reviewer wrote it, bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

/// What a reviewed job's builder landed, and who backs that.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LandingSummary {
    /// When the call allowed the builder to land.
    pub mode: LandMode,
    /// Where the landing stands.
    pub status: LandingStatus,
    /// The last ref commits were reported landed on, such as `origin/main`.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// Every commit reported landed in the job, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<String>,
    /// Who backs a `landed` status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<LandingEvidence>,
    /// The reviewer agent behind the evidence, when one checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_by: Option<String>,
    /// Commits landed in a round turn, before that round's verdict.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub landed_before_review: bool,
    /// The builder reported landing that the call did not allow.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unauthorized: bool,
    /// SCV's own words on the status or evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The builder's own one-line detail, bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// When a reviewed call allows its builder to land.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LandMode {
    /// Not in this job.
    #[serde(rename = "none")]
    NoLanding,
    /// In one extra builder turn after an approving verdict.
    AfterApproval,
    /// In each round's builder turn, before that round's review.
    BeforeReview,
    /// A mode this client does not know, from a newer server.
    #[serde(other)]
    Unknown,
}

/// Where a reviewed job's landing stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LandingStatus {
    /// The call did not allow landing, and none was reported.
    NotRequested,
    /// Landing was allowed, but no turn that may land ran.
    NotAttempted,
    /// The builder reported commits on a ref.
    Landed,
    /// The builder reported it did not land.
    NotLanded,
    /// The builder reported a failed attempt; partial state is possible.
    Failed,
    /// A turn that may land did not complete, or its report is missing or
    /// invalid; also any status this client does not know.
    #[serde(other)]
    Unknown,
}

/// Who backs a `landed` status. SCV runs no git, so a landing is only ever
/// the builder's report or a reviewer's check of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LandingEvidence {
    /// No check was due.
    BuilderReported,
    /// A reviewer found every landed commit on the ref.
    ReviewerConfirmed,
    /// A reviewer found them missing or different.
    ReviewerDisputed,
    /// A check was due but produced no usable result.
    Unconfirmed,
    /// Evidence this client does not know, from a newer server.
    #[serde(other)]
    Unknown,
}

/// The longest reviewer-written summary quoted in an escalation line.
const QUOTED_SUMMARY_CHARS: usize = 200;
/// Open findings an unresolved line lists.
const LISTED_FINDINGS: usize = 5;
/// Landed commits a landing line names.
const LISTED_COMMITS: usize = 3;
/// The longest builder detail quoted in a landing line.
const QUOTED_DETAIL_CHARS: usize = 120;
/// The longest refusal quoted in a report.
const QUOTED_REFUSAL_CHARS: usize = 300;

/// `outcome` as a report states it, without the job: always a Review line,
/// then any open findings indented, then what each reviewer that declined
/// said, quoted and attributed, then a Landing line.
pub fn describe_outcome(outcome: &JobOutcome) -> String {
    let (review, findings) = review_lines(&outcome.review);
    let mut text = review;
    text.push('\n');
    for finding in findings {
        text.push_str(&finding);
        text.push('\n');
    }
    for refusal in &outcome.review.refusals {
        text.push_str(&format!(
            "  - reviewer {} declined, saying (untrusted): \"{}\"\n",
            refusal.agent,
            shorten(&refusal.reply, QUOTED_REFUSAL_CHARS)
        ));
    }
    text.push_str(&landing_line(&outcome.landing));
    text.push('\n');
    text
}

/// `outcome` as a client shows it, in SCV's words alone: the Review line,
/// any open findings, and the Landing line, the two prefixed with the job,
/// such as `job-4 · Review: approved · round 2 of 3 · reviewer claude`.
/// Reviewers' refusals are left to the report, which quotes them.
pub fn outcome_notice(outcome: &JobOutcome) -> String {
    let (review, findings) = review_lines(&outcome.review);
    let mut text = format!("{} · {review}", outcome.job);
    for finding in findings {
        text.push('\n');
        text.push_str(&finding);
    }
    text.push_str(&format!(
        "\n{} · {}",
        outcome.job,
        landing_line(&outcome.landing)
    ));
    text
}

/// The Review line and the open-finding lines under it.
fn review_lines(review: &ReviewSummary) -> (String, Vec<String>) {
    let (mut line, findings) = review_line(review);
    if review.journal_incomplete && review.reason.as_deref() != Some("journal_error") {
        line.push_str(&format!(
            " · journal {} INCOMPLETE: a write failed",
            review.journal
        ));
    } else if review.journal_pending {
        line.push_str(&format!(
            " · journal {} still open: the job is still stopping, an update follows",
            review.journal
        ));
    }
    (line, findings)
}

fn review_line(review: &ReviewSummary) -> (String, Vec<String>) {
    let of = format!("round {} of {}", review.round, review.rounds);
    let reviewer = reviewer_name(review);
    let line = match review.outcome {
        ReviewOutcome::Approved => format!("Review: approved · {of} · reviewer {reviewer}"),
        ReviewOutcome::Unresolved => {
            let noun = if review.open_count == 1 {
                "finding"
            } else {
                "findings"
            };
            let mut findings: Vec<String> = review
                .open
                .iter()
                .take(LISTED_FINDINGS)
                .map(|finding| match &finding.location {
                    Some(location) => format!("  - \"{}\" ({location})", finding.title),
                    None => format!("  - \"{}\"", finding.title),
                })
                .collect();
            let listed = u32::try_from(findings.len()).unwrap_or(u32::MAX);
            if review.open_count > listed {
                findings.push(format!("  - and {} more", review.open_count - listed));
            }
            return (
                format!(
                    "Review: NOT approved · unresolved after {} of {} rounds · {} blocking {noun} \
                     open:",
                    review.round, review.rounds, review.open_count
                ),
                findings,
            );
        }
        ReviewOutcome::Escalated => {
            let summary = review.summary.as_deref().unwrap_or_default();
            format!(
                "Review: NOT approved · escalated in {of} · reviewer {reviewer}: \"{}\"",
                shorten(summary, QUOTED_SUMMARY_CHARS)
            )
        }
        ReviewOutcome::NoVerdict => format!(
            "Review: NOT approved · no verdict in {of} · {}",
            no_verdict_phrase(review, &reviewer)
        ),
        ReviewOutcome::Stopped if review.round == 0 => format!(
            "Review: NOT approved · stopped before round 1 · {}",
            stopped_phrase(review)
        ),
        ReviewOutcome::Stopped => format!(
            "Review: NOT approved · stopped in {of} · {}",
            stopped_phrase(review)
        ),
        ReviewOutcome::Unknown => {
            format!("Review: unknown outcome · see journal {}", review.journal)
        }
    };
    (line, Vec::new())
}

/// `claude`, or `grok (codex unavailable)` when it was not the first choice.
fn reviewer_name(review: &ReviewSummary) -> String {
    match &review.fallback {
        Some(fallback) => format!("{} ({fallback})", review.reviewer),
        None => review.reviewer.clone(),
    }
}

fn no_verdict_phrase(review: &ReviewSummary, reviewer: &str) -> String {
    match review.reason.as_deref().unwrap_or_default() {
        "reviewer_timeout" => format!("reviewer {reviewer} timed out"),
        "reviewer_failed" => format!("reviewer {reviewer} failed"),
        "reviewer_cancelled" => format!("reviewer {reviewer} was stopped"),
        "reviewer_declined" => format!("reviewer {reviewer} declined"),
        "reviewer_unavailable" => format!("reviewer {reviewer} is unavailable"),
        "no_reviewer_available" => {
            let tried: Vec<&str> = review
                .tried
                .iter()
                .map(|tried| tried.agent.as_str())
                .collect();
            format!("no reviewer available ({})", tried.join(", "))
        }
        "no_conversation_slot" => "no room for a reviewer conversation".to_owned(),
        "malformed_verdict" => format!("the verdict of reviewer {reviewer} stayed malformed"),
        other => format!("reason {other}"),
    }
}

fn stopped_phrase(review: &ReviewSummary) -> String {
    match review.reason.as_deref().unwrap_or_default() {
        "builder_failed" => "the builder turn failed".to_owned(),
        "builder_timeout" => "the builder turn timed out".to_owned(),
        "builder_declined" => "the builder declined".to_owned(),
        "builder_cancelled" => "the builder turn was stopped".to_owned(),
        "builder_no_session" => "the builder returned no conversation to continue".to_owned(),
        "cancelled" if review.round == 0 => "the job was cancelled while queued".to_owned(),
        "cancelled" => "the job was cancelled".to_owned(),
        "journal_error" => "the journal could not be written".to_owned(),
        other => format!("reason {other}"),
    }
}

/// The Landing line.
fn landing_line(landing: &LandingSummary) -> String {
    let reason = landing.reason.as_deref().unwrap_or_default();
    let unauthorized = if landing.unauthorized {
        " · NOT authorized by this call"
    } else {
        ""
    };
    match landing.status {
        LandingStatus::NotRequested => "Landing: not requested".to_owned(),
        LandingStatus::NotAttempted if reason.is_empty() => "Landing: not attempted".to_owned(),
        LandingStatus::NotAttempted => format!("Landing: not attempted · {reason}"),
        LandingStatus::Landed => {
            let before = if landing.landed_before_review {
                " before review"
            } else {
                ""
            };
            format!(
                "Landing: landed{before} · {} · {}{unauthorized}",
                commits_on_ref(landing),
                evidence_phrase(landing)
            )
        }
        LandingStatus::NotLanded => format!(
            "Landing: not landed · builder-reported{}{}{unauthorized}",
            quoted_detail(landing),
            earlier(landing)
        ),
        LandingStatus::Failed => format!(
            "Landing: failed · builder-reported · partial state possible{}{}{unauthorized}",
            quoted_detail(landing),
            earlier(landing)
        ),
        LandingStatus::Unknown => {
            let reason = if reason.is_empty() {
                String::new()
            } else {
                format!(" · {reason}")
            };
            format!(
                "Landing: unknown{reason}{} · check before relying on it{unauthorized}",
                earlier(landing)
            )
        }
    }
}

/// `4f2a9c1, 9e8d7c6 → origin/main`, at most three commits.
fn commits_on_ref(landing: &LandingSummary) -> String {
    let mut commits: Vec<&str> = landing
        .commits
        .iter()
        .take(LISTED_COMMITS)
        .map(|commit| commit.get(..7).unwrap_or(commit))
        .collect();
    let more = landing.commits.len().saturating_sub(LISTED_COMMITS);
    let more = if more > 0 {
        format!(" +{more} more")
    } else {
        String::new()
    };
    if commits.is_empty() {
        commits.push("no commits named");
    }
    format!(
        "{}{more} → {}",
        commits.join(", "),
        landing.reference.as_deref().unwrap_or("an unnamed ref")
    )
}

fn evidence_phrase(landing: &LandingSummary) -> String {
    let checker = landing.checked_by.as_deref().unwrap_or("reviewer");
    let reason = landing.reason.as_deref().unwrap_or("no reason given");
    match landing.evidence {
        Some(LandingEvidence::ReviewerConfirmed) => format!("confirmed by reviewer {checker}"),
        Some(LandingEvidence::ReviewerDisputed) => {
            format!("reviewer {checker} could NOT confirm: {reason}")
        }
        Some(LandingEvidence::Unconfirmed) => format!("NOT confirmed: {reason}"),
        Some(LandingEvidence::Unknown) => "evidence unknown".to_owned(),
        Some(LandingEvidence::BuilderReported) | None => {
            "builder-reported, not verified".to_owned()
        }
    }
}

/// `: "<detail>"`, the builder's own words, or nothing.
fn quoted_detail(landing: &LandingSummary) -> String {
    landing
        .detail
        .as_deref()
        .filter(|detail| !detail.is_empty())
        .map_or_else(String::new, |detail| {
            format!(": \"{}\"", shorten(detail, QUOTED_DETAIL_CHARS))
        })
}

/// `; earlier: <commits> → <ref>` when commits landed before a later
/// report that did not land.
fn earlier(landing: &LandingSummary) -> String {
    if landing.commits.is_empty() {
        String::new()
    } else {
        format!("; earlier: {}", commits_on_ref(landing))
    }
}

/// `text` cut to `limit` characters, with `…` when cut.
fn shorten(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(limit.saturating_sub(1)).collect();
    short.push('…');
    short
}

#[cfg(test)]
mod tests;
