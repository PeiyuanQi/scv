//! A reviewed job's live state, which the loop writes as it goes and which
//! `agent_status`, `agent_cancel`, the final result, and the journal all
//! read, so they always agree.
//!
//! Once the job's cancellation token fires, the state is frozen: the loop's
//! later writes are refused, so the outcome `agent_cancel` reads right after
//! cancelling is the one the job ends with. An outcome the loop fixed before
//! the cancel (an approving verdict) stands; otherwise the review is
//! `stopped` (`cancelled`).

use std::sync::Mutex;

use scv_protocol::{
    LandMode, LandingEvidence, LandingStatus, LandingSummary, OpenFinding, Refusal, ReviewOutcome,
    ReviewSummary, ReviewerResult, TriedReviewer,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::blocks::{CheckStatus, LandingCheck, LandingReport, ReportStatus, same_commit};
use crate::sync::lock;

/// Where a reviewed job stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Before its first builder turn, such as while queued.
    Waiting,
    Builder,
    Reviewer,
    /// The reviewer's one repair turn for a malformed verdict.
    Repair,
    /// The builder's landing turn after an approving verdict.
    Landing,
    /// The approving reviewer's check of the landing.
    Confirmation,
    Finished,
}

impl Phase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Builder => "builder",
            Self::Reviewer => "reviewer",
            Self::Repair => "repair",
            Self::Landing => "landing",
            Self::Confirmation => "confirmation",
            Self::Finished => "finished",
        }
    }
}

/// A reviewed job's state, shared by its loop and its job.
#[derive(Debug)]
pub(crate) struct Live {
    state: Mutex<State>,
}

#[derive(Debug)]
pub(crate) struct State {
    rounds: u32,
    journal: String,
    pub(crate) round: u32,
    pub(crate) phase: Phase,
    /// The agent of the latest reviewer attempt.
    pub(crate) reviewer: String,
    pub(crate) fallback: Option<String>,
    /// Shown once the job holds the builder conversation's lane.
    pub(crate) builder_session: Option<String>,
    /// The outcome the loop fixed, with its reason.
    fixed: Option<(ReviewOutcome, Option<String>)>,
    /// A journal write failed.
    journal_failed: bool,
    pub(crate) tried: Vec<TriedReviewer>,
    pub(crate) refusals: Vec<Refusal>,
    pub(crate) summary: Option<String>,
    pub(crate) open: Vec<OpenFinding>,
    pub(crate) landing: Landing,
}

/// At most this many refusals are kept for the result.
const MAX_REFUSALS: usize = 2;
/// At most this many open findings are named in the outcome.
const MAX_OPEN_LISTED: usize = 5;

impl State {
    /// Record `agent`'s latest attempt result.
    pub(crate) fn tried(&mut self, agent: &str, result: ReviewerResult) {
        match self.tried.iter_mut().find(|tried| tried.agent == agent) {
            Some(tried) => tried.result = result,
            None => self.tried.push(TriedReviewer {
                agent: agent.to_owned(),
                result,
            }),
        }
    }

    /// Keep a reviewer's refusal for the result.
    pub(crate) fn refused(&mut self, agent: &str, reply: String) {
        if self.refusals.len() < MAX_REFUSALS {
            self.refusals.push(Refusal {
                agent: agent.to_owned(),
                reply,
            });
        }
    }
}

impl Live {
    pub(crate) fn new(rounds: u32, land: LandMode, journal: String) -> Self {
        Self {
            state: Mutex::new(State {
                rounds,
                journal,
                round: 0,
                phase: Phase::Waiting,
                reviewer: String::new(),
                fallback: None,
                builder_session: None,
                fixed: None,
                journal_failed: false,
                tried: Vec::new(),
                refusals: Vec::new(),
                summary: None,
                open: Vec::new(),
                landing: Landing::new(land),
            }),
        }
    }

    /// Apply `change` unless the job is cancelled; whether it applied.
    pub(crate) fn update(
        &self,
        token: &CancellationToken,
        change: impl FnOnce(&mut State),
    ) -> bool {
        let mut state = lock(&self.state);
        if token.is_cancelled() {
            return false;
        }
        change(&mut state);
        true
    }

    /// Read the state, cancelled or not.
    pub(crate) fn read<T>(&self, read: impl FnOnce(&State) -> T) -> T {
        read(&lock(&self.state))
    }

    /// A journal write failed. Recorded even once the job is cancelled: an
    /// incomplete journal is never hidden.
    pub(crate) fn journal_failed(&self) {
        lock(&self.state).journal_failed = true;
    }

    /// Fix the review's outcome, unless the job is cancelled. An outcome
    /// once fixed is never replaced.
    pub(crate) fn fix(
        &self,
        token: &CancellationToken,
        outcome: ReviewOutcome,
        reason: Option<&str>,
    ) -> bool {
        self.update(token, |state| {
            state
                .fixed
                .get_or_insert_with(|| (outcome, reason.map(str::to_owned)));
        })
    }

    /// The job's review while it runs, for `agent_status` and `agent_wait`.
    pub(crate) fn status(&self) -> Value {
        let state = lock(&self.state);
        let mut status = json!({
            "round": state.round,
            "rounds": state.rounds,
            "phase": state.phase.as_str(),
            "land": land_name(state.landing.mode),
            "journal": state.journal,
        });
        if !state.reviewer.is_empty() {
            status["reviewer"] = state.reviewer.clone().into();
        }
        if let Some(session) = &state.builder_session {
            status["builder_session"] = session.clone().into();
        }
        status
    }

    /// The review and landing as they stand: the fixed outcome, or
    /// `stopped` (`cancelled`) when the loop fixed none, which only a
    /// cancelled or abandoned job leaves. With `cancelled`, a turn that may
    /// land or a check that was running counts as cut short.
    pub(crate) fn snapshot(&self, cancelled: bool) -> (ReviewSummary, LandingSummary) {
        let state = lock(&self.state);
        let (outcome, reason) = state
            .fixed
            .clone()
            .unwrap_or((ReviewOutcome::Stopped, Some("cancelled".to_owned())));
        let cut = cancelled.then_some(state.phase);
        let halted = state
            .journal_failed
            .then_some("the journal could not be written");
        let landing = state.landing.summary(outcome, state.round, cut, halted);
        let open_count = u32::try_from(state.open.len()).unwrap_or(u32::MAX);
        let review = ReviewSummary {
            outcome,
            reason,
            round: state.round,
            rounds: state.rounds,
            reviewer: state.reviewer.clone(),
            fallback: state.fallback.clone(),
            tried: state.tried.clone(),
            refusals: state.refusals.clone(),
            summary: state.summary.clone(),
            open_count: if outcome == ReviewOutcome::Approved {
                0
            } else {
                open_count
            },
            open: if outcome == ReviewOutcome::Approved {
                Vec::new()
            } else {
                state.open.iter().take(MAX_OPEN_LISTED).cloned().collect()
            },
            journal: state.journal.clone(),
            journal_incomplete: state.journal_failed,
            journal_pending: false,
        };
        (review, landing)
    }
}

/// The `land` value a call gives for `mode`, or `none`.
pub(crate) fn land_name(mode: LandMode) -> &'static str {
    match mode {
        LandMode::AfterApproval => "after_approval",
        LandMode::BeforeReview => "before_review",
        _ => "none",
    }
}

/// What kind of builder turn reported a landing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnKind {
    /// A round's builder turn.
    Round,
    /// The landing turn after an approving verdict.
    Landing,
}

impl TurnKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Round => "round",
            Self::Landing => "landing",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Self::Round => "builder turn",
            Self::Landing => "landing turn",
        }
    }

    /// The check a landing in a turn of this kind is due.
    fn check(self) -> &'static str {
        match self {
            Self::Round => "the reviewer's check of the landing",
            Self::Landing => "the confirmation",
        }
    }
}

/// What the builder reported landed in the job, and who checked it.
#[derive(Debug)]
pub(crate) struct Landing {
    mode: LandMode,
    /// The last report's status; `None` until a report counts.
    status: Option<LandingStatus>,
    /// SCV's words for an `unknown` status.
    reason: Option<String>,
    reference: Option<String>,
    commits: Vec<String>,
    landed_before_review: bool,
    unauthorized: bool,
    detail: Option<String>,
    url: Option<String>,
    /// Each `landed` report, with its check.
    reports: Vec<LandedReport>,
    /// Round builder turns started.
    builder_turns: u32,
}

#[derive(Debug)]
struct LandedReport {
    round: u32,
    kind: TurnKind,
    reference: String,
    commits: Vec<String>,
    check: Check,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Check {
    /// None is due: the turn that reported it did not complete, so the job
    /// stopped before any review of it.
    NotStarted,
    /// The turn that reported it completed, so a check is due: the round's
    /// review, or the approving reviewer's confirmation. None has started.
    Due,
    /// A reviewer is checking it now.
    Running(Option<String>),
    /// A reviewer found every commit on the ref.
    Covered(String),
    /// A reviewer found them missing or different, and why.
    Disputed(String, String),
    /// A check was due but produced nothing usable, and why.
    NoResult(Option<String>, String),
}

impl Landing {
    fn new(mode: LandMode) -> Self {
        Self {
            mode,
            status: None,
            reason: None,
            reference: None,
            commits: Vec::new(),
            landed_before_review: false,
            unauthorized: false,
            detail: None,
            url: None,
            reports: Vec::new(),
            builder_turns: 0,
        }
    }

    /// Whether a turn of `kind` may land under the call's mode.
    pub(crate) fn may_land(&self, kind: TurnKind) -> bool {
        matches!(
            (self.mode, kind),
            (LandMode::BeforeReview, TurnKind::Round)
                | (LandMode::AfterApproval, TurnKind::Landing)
        )
    }

    pub(crate) fn turn_started(&mut self, kind: TurnKind) {
        if kind == TurnKind::Round {
            self.builder_turns += 1;
        }
    }

    /// Record what a builder turn of `kind` in `round` reported: `report`
    /// is its parsed block, `None` without one, or why it was invalid;
    /// `ended` is `None` for a completed turn, otherwise how it ended, such
    /// as `timed out`. Returns whether the turn may land, for the journal.
    pub(crate) fn record(
        &mut self,
        kind: TurnKind,
        round: u32,
        ended: Option<&str>,
        report: &Result<Option<LandingReport>, String>,
    ) -> bool {
        let may_land = self.may_land(kind);
        match report {
            Ok(Some(report)) if may_land || report.status != ReportStatus::NotLanded => {
                self.unauthorized |= !may_land;
                self.apply(kind, round, report, ended.is_none());
                if let Some(ended) = ended {
                    self.status = Some(LandingStatus::Unknown);
                    self.reason = Some(format!("the {} {ended}", kind.noun()));
                }
            }
            // A turn that may not land and reports nothing landed.
            Ok(_) | Err(_) if !may_land => {}
            _ => {
                self.status = Some(LandingStatus::Unknown);
                self.reason = Some(match ended {
                    Some(ended) => format!("the {} {ended}", kind.noun()),
                    None => "no valid landing report".to_owned(),
                });
                self.detail = None;
            }
        }
        may_land
    }

    fn apply(&mut self, kind: TurnKind, round: u32, report: &LandingReport, completed: bool) {
        self.status = Some(match report.status {
            ReportStatus::Landed => LandingStatus::Landed,
            ReportStatus::NotLanded => LandingStatus::NotLanded,
            ReportStatus::Failed => LandingStatus::Failed,
        });
        self.reason = None;
        self.detail.clone_from(&report.detail);
        if report.url.is_some() {
            self.url.clone_from(&report.url);
        }
        if report.status != ReportStatus::Landed {
            return;
        }
        let reference = report.reference.clone().unwrap_or_default();
        for commit in &report.commits {
            if !self.commits.iter().any(|known| same_commit(known, commit)) {
                self.commits.push(commit.clone());
            }
        }
        self.reference = Some(reference.clone());
        self.landed_before_review |= kind == TurnKind::Round;
        self.reports.push(LandedReport {
            round,
            kind,
            reference,
            commits: report.commits.clone(),
            check: if completed {
                Check::Due
            } else {
                Check::NotStarted
            },
        });
    }

    /// The commits the turn of `kind` in `round` reported landed, and the ref.
    pub(crate) fn reported(&self, kind: TurnKind, round: u32) -> Option<(String, Vec<String>)> {
        let mut found = None;
        for report in self
            .reports
            .iter()
            .filter(|report| report.kind == kind && report.round == round)
        {
            let (_, commits) = found.get_or_insert_with(|| (report.reference.clone(), Vec::new()));
            commits.extend(report.commits.iter().cloned());
        }
        found
    }

    /// Every commit reported landed so far, and the last ref.
    pub(crate) fn landed_so_far(&self) -> Option<(String, Vec<String>)> {
        self.reference
            .clone()
            .map(|reference| (reference, self.commits.clone()))
    }

    /// `agent` starts checking the reports of `kind` in `round`.
    pub(crate) fn checking(&mut self, kind: TurnKind, round: u32, agent: Option<&str>) {
        for report in self.reports_of(kind, round) {
            if matches!(
                report.check,
                Check::NotStarted | Check::Due | Check::Running(_)
            ) {
                report.check = Check::Running(agent.map(str::to_owned));
            }
        }
    }

    /// `agent`'s check of the reports of `kind` in `round`.
    pub(crate) fn checked(
        &mut self,
        kind: TurnKind,
        round: u32,
        agent: &str,
        check: &LandingCheck,
    ) {
        for report in self.reports_of(kind, round) {
            report.check = judge(report, agent, check);
        }
    }

    /// The check of the reports of `kind` in `round` produced nothing
    /// usable, for `reason`.
    pub(crate) fn unchecked(
        &mut self,
        kind: TurnKind,
        round: u32,
        agent: Option<&str>,
        reason: &str,
    ) {
        for report in self.reports_of(kind, round) {
            if matches!(
                report.check,
                Check::NotStarted | Check::Due | Check::Running(_)
            ) {
                report.check = Check::NoResult(agent.map(str::to_owned), reason.to_owned());
            }
        }
    }

    fn reports_of(
        &mut self,
        kind: TurnKind,
        round: u32,
    ) -> impl Iterator<Item = &mut LandedReport> {
        self.reports
            .iter_mut()
            .filter(move |report| report.kind == kind && report.round == round)
    }

    /// The landing as the result states it, given the review's `outcome`,
    /// the `round` it reached, for a cancelled job the phase the cancel cut
    /// short, and why the loop halted early, if it did.
    pub(crate) fn summary(
        &self,
        outcome: ReviewOutcome,
        round: u32,
        cut: Option<Phase>,
        halted: Option<&str>,
    ) -> LandingSummary {
        let cut_landing = match cut {
            Some(Phase::Landing) => true,
            Some(Phase::Builder) => self.mode == LandMode::BeforeReview,
            _ => false,
        };
        let (status, status_reason) = if cut_landing {
            (
                LandingStatus::Unknown,
                Some("the job was cancelled during a turn that may land".to_owned()),
            )
        } else if let Some(status) = self.status {
            (status, self.reason.clone())
        } else {
            match self.mode {
                LandMode::AfterApproval | LandMode::BeforeReview
                    if round == 0 || self.builder_turns == 0 =>
                {
                    (
                        LandingStatus::NotAttempted,
                        Some("the job ended before any builder turn".to_owned()),
                    )
                }
                LandMode::AfterApproval if outcome == ReviewOutcome::Approved => (
                    LandingStatus::NotAttempted,
                    Some("the job ended before the landing turn".to_owned()),
                ),
                LandMode::AfterApproval => (
                    LandingStatus::NotAttempted,
                    Some("review not approved".to_owned()),
                ),
                LandMode::BeforeReview => (
                    LandingStatus::Unknown,
                    Some("no valid landing report".to_owned()),
                ),
                _ => (LandingStatus::NotRequested, None),
            }
        };
        let (evidence, checked_by, evidence_reason) = self.evidence(cut, halted);
        let landed = status == LandingStatus::Landed;
        LandingSummary {
            mode: self.mode,
            status,
            reference: self.reference.clone(),
            commits: self.commits.clone(),
            evidence: if landed { evidence } else { None },
            checked_by: if landed { checked_by } else { None },
            landed_before_review: self.landed_before_review,
            unauthorized: self.unauthorized,
            reason: if landed {
                evidence_reason
            } else {
                status_reason
            },
            detail: self.detail.clone(),
        }
    }

    /// Who backs the landed commits: a dispute outweighs a missing check,
    /// which outweighs a report no check was due for; only checks covering
    /// every report confirm. A due check that never ran is a missing one.
    fn evidence(
        &self,
        cut: Option<Phase>,
        halted: Option<&str>,
    ) -> (Option<LandingEvidence>, Option<String>, Option<String>) {
        if self.reports.is_empty() {
            return (None, None, None);
        }
        let why = if cut.is_some() {
            "the job was cancelled"
        } else {
            halted.unwrap_or("the job ended first")
        };
        let checks: Vec<Check> = self
            .reports
            .iter()
            .map(|report| match (&report.check, cut) {
                (Check::Due, _) => {
                    Check::NoResult(None, format!("{} did not run: {why}", report.kind.check()))
                }
                (Check::Running(agent), Some(Phase::Confirmation)) => Check::NoResult(
                    agent.clone(),
                    "the job was cancelled during the confirmation".into(),
                ),
                (Check::Running(agent), Some(_)) => Check::NoResult(
                    agent.clone(),
                    "the job was cancelled during the review".into(),
                ),
                (Check::Running(agent), None) => Check::NoResult(
                    agent.clone(),
                    format!("{} produced no result: {why}", report.kind.check()),
                ),
                (check, _) => check.clone(),
            })
            .collect();
        if let Some((agent, reason)) = checks.iter().rev().find_map(|check| match check {
            Check::Disputed(agent, reason) => Some((agent, reason)),
            _ => None,
        }) {
            return (
                Some(LandingEvidence::ReviewerDisputed),
                Some(agent.clone()),
                Some(reason.clone()),
            );
        }
        if let Some((agent, reason)) = checks.iter().rev().find_map(|check| match check {
            Check::NoResult(agent, reason) => Some((agent, reason)),
            _ => None,
        }) {
            return (
                Some(LandingEvidence::Unconfirmed),
                agent.clone(),
                Some(reason.clone()),
            );
        }
        if checks.contains(&Check::NotStarted) {
            return (Some(LandingEvidence::BuilderReported), None, None);
        }
        let agent = checks.iter().rev().find_map(|check| match check {
            Check::Covered(agent) => Some(agent.clone()),
            _ => None,
        });
        (Some(LandingEvidence::ReviewerConfirmed), agent, None)
    }

    /// The URL of the last report that gave one, for the tool result.
    pub(crate) fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    /// The last report's status, once one counted.
    pub(crate) fn status(&self) -> Option<LandingStatus> {
        self.status
    }
}

/// What `agent`'s `check` says of `report`.
fn judge(report: &LandedReport, agent: &str, check: &LandingCheck) -> Check {
    let agent = agent.to_owned();
    match check.status {
        CheckStatus::Confirmed => {
            let same_ref = check.reference.as_deref() == Some(report.reference.as_str());
            let covered = report.commits.iter().all(|commit| {
                check
                    .commits
                    .iter()
                    .any(|checked| same_commit(commit, checked))
            });
            if same_ref && covered && !check.evidence.is_empty() {
                Check::Covered(agent)
            } else {
                Check::Disputed(agent, "the reviewer's check names other commits".into())
            }
        }
        CheckStatus::NotFound => Check::Disputed(
            agent,
            format!("the commits are not on {}", report.reference),
        ),
        CheckStatus::Mismatch => Check::Disputed(
            agent,
            match report.kind {
                TurnKind::Landing => "the landed change does not match the approved one".into(),
                TurnKind::Round => "the landed change does not match the builder's report".into(),
            },
        ),
        CheckStatus::Unverifiable => Check::NoResult(
            Some(agent),
            format!(
                "the reviewer could not verify: \"{}\"",
                check.note.as_deref().unwrap_or_default()
            ),
        ),
    }
}

#[cfg(test)]
mod tests;
