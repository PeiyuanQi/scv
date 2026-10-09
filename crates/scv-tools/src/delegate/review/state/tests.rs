//! Unit tests for `src/delegate/review/state.rs`.

use super::*;

fn report(status: ReportStatus, commits: &[&str]) -> LandingReport {
    LandingReport {
        status,
        reference: (status == ReportStatus::Landed).then(|| "origin/main".to_owned()),
        commits: commits.iter().map(|commit| (*commit).to_owned()).collect(),
        url: None,
        detail: Some("detail".into()),
    }
}

fn check(status: CheckStatus, commits: &[&str]) -> LandingCheck {
    LandingCheck {
        status,
        reference: Some("origin/main".into()),
        commits: commits.iter().map(|commit| (*commit).to_owned()).collect(),
        evidence: vec!["git branch -r --contains".into()],
        note: Some("no access to the remote".into()),
    }
}

#[test]
fn a_call_that_may_not_land_records_only_landed_or_failed_reports_as_unauthorized() {
    let mut landing = Landing::new(LandMode::NoLanding);
    landing.turn_started(TurnKind::Round);
    // No block, or one saying nothing landed: nothing happened.
    assert!(!landing.record(TurnKind::Round, 1, None, &Ok(None)));
    landing.record(
        TurnKind::Round,
        1,
        None,
        &Ok(Some(report(ReportStatus::NotLanded, &[]))),
    );
    landing.record(TurnKind::Round, 1, None, &Err("bad".into()));
    let summary = landing.summary(ReviewOutcome::Approved, 1, None, None);
    assert_eq!(summary.status, LandingStatus::NotRequested);
    assert!(!summary.unauthorized);
    landing.record(
        TurnKind::Round,
        1,
        None,
        &Ok(Some(report(ReportStatus::Landed, &["4f2a9c1"]))),
    );
    let summary = landing.summary(ReviewOutcome::Approved, 1, None, None);
    assert_eq!(summary.status, LandingStatus::Landed);
    assert!(summary.unauthorized && summary.landed_before_review);
    // Its turn completed, so the round's review owed it a check.
    assert_eq!(summary.evidence, Some(LandingEvidence::Unconfirmed));
    assert_eq!(
        summary.reason.as_deref(),
        Some("the reviewer's check of the landing did not run: the job ended first")
    );
    // A turn that failed after landing stops the job before any review:
    // the landing is unknown, its commits and the violation still shown.
    let mut failed = Landing::new(LandMode::NoLanding);
    failed.turn_started(TurnKind::Round);
    failed.record(
        TurnKind::Round,
        1,
        Some("failed"),
        &Ok(Some(report(ReportStatus::Landed, &["4f2a9c1"]))),
    );
    let summary = failed.summary(ReviewOutcome::Stopped, 1, None, None);
    assert_eq!(summary.status, LandingStatus::Unknown);
    assert_eq!(summary.reason.as_deref(), Some("the builder turn failed"));
    assert_eq!(summary.commits, vec!["4f2a9c1"]);
    assert!(summary.unauthorized);
}

#[test]
fn before_review_turns_must_report_and_one_that_lands_then_fails_is_unknown() {
    let mut landing = Landing::new(LandMode::BeforeReview);
    assert_eq!(
        landing
            .summary(ReviewOutcome::Stopped, 0, None, None)
            .status,
        LandingStatus::NotAttempted
    );
    landing.turn_started(TurnKind::Round);
    assert!(landing.record(TurnKind::Round, 1, None, &Ok(None)));
    let summary = landing.summary(ReviewOutcome::NoVerdict, 1, None, None);
    assert_eq!(summary.status, LandingStatus::Unknown);
    assert_eq!(summary.reason.as_deref(), Some("no valid landing report"));
    landing.turn_started(TurnKind::Round);
    landing.record(
        TurnKind::Round,
        2,
        Some("timed out"),
        &Ok(Some(report(ReportStatus::Landed, &["4f2a9c1"]))),
    );
    let summary = landing.summary(ReviewOutcome::Stopped, 2, None, None);
    assert_eq!(summary.status, LandingStatus::Unknown);
    assert_eq!(
        summary.reason.as_deref(),
        Some("the builder turn timed out")
    );
    // The landing it reported is still recorded.
    assert_eq!(summary.commits, vec!["4f2a9c1"]);
    assert!(summary.landed_before_review);
}

#[test]
fn every_landed_commit_must_be_covered_for_the_reviewer_to_confirm() {
    let mut landing = Landing::new(LandMode::BeforeReview);
    landing.turn_started(TurnKind::Round);
    landing.record(
        TurnKind::Round,
        1,
        None,
        &Ok(Some(report(ReportStatus::Landed, &["4f2a9c1e0b7d"]))),
    );
    landing.checking(TurnKind::Round, 1, Some("claude"));
    landing.checked(
        TurnKind::Round,
        1,
        "claude",
        &check(CheckStatus::Confirmed, &["4f2a9c1"]),
    );
    let summary = landing.summary(ReviewOutcome::Approved, 1, None, None);
    assert_eq!(summary.evidence, Some(LandingEvidence::ReviewerConfirmed));
    assert_eq!(summary.checked_by.as_deref(), Some("claude"));
    // A second landing, checked with other commits, is disputed.
    landing.turn_started(TurnKind::Round);
    landing.record(
        TurnKind::Round,
        2,
        None,
        &Ok(Some(report(ReportStatus::Landed, &["9e8d7c6"]))),
    );
    let pending = landing.summary(
        ReviewOutcome::Approved,
        2,
        None,
        Some("the journal could not be written"),
    );
    assert_eq!(pending.evidence, Some(LandingEvidence::Unconfirmed));
    assert_eq!(
        pending.reason.as_deref(),
        Some("the reviewer's check of the landing did not run: the journal could not be written")
    );
    landing.checked(
        TurnKind::Round,
        2,
        "claude",
        &check(CheckStatus::Confirmed, &["1111111"]),
    );
    let summary = landing.summary(ReviewOutcome::Approved, 2, None, None);
    assert_eq!(summary.evidence, Some(LandingEvidence::ReviewerDisputed));
    assert_eq!(
        summary.reason.as_deref(),
        Some("the reviewer's check names other commits")
    );
    assert_eq!(summary.commits, vec!["4f2a9c1e0b7d", "9e8d7c6"]);
}

#[test]
fn a_confirmation_maps_to_evidence() {
    let landed = || {
        let mut landing = Landing::new(LandMode::AfterApproval);
        landing.turn_started(TurnKind::Round);
        landing.record(
            TurnKind::Landing,
            1,
            None,
            &Ok(Some(report(ReportStatus::Landed, &["4f2a9c1"]))),
        );
        landing.checking(TurnKind::Landing, 1, Some("codex"));
        landing
    };
    for (status, evidence, reason) in [
        (
            CheckStatus::Confirmed,
            LandingEvidence::ReviewerConfirmed,
            None,
        ),
        (
            CheckStatus::NotFound,
            LandingEvidence::ReviewerDisputed,
            Some("the commits are not on origin/main"),
        ),
        (
            CheckStatus::Mismatch,
            LandingEvidence::ReviewerDisputed,
            Some("the landed change does not match the approved one"),
        ),
        (
            CheckStatus::Unverifiable,
            LandingEvidence::Unconfirmed,
            Some("the reviewer could not verify: \"no access to the remote\""),
        ),
    ] {
        let mut landing = landed();
        landing.checked(TurnKind::Landing, 1, "codex", &check(status, &["4f2a9c1"]));
        let summary = landing.summary(ReviewOutcome::Approved, 1, None, None);
        assert_eq!(summary.evidence, Some(evidence), "{status:?}");
        assert_eq!(summary.reason.as_deref(), reason, "{status:?}");
        assert_eq!(summary.checked_by.as_deref(), Some("codex"));
    }
    let mut landing = landed();
    landing.unchecked(
        TurnKind::Landing,
        1,
        Some("codex"),
        "the confirmation timed out",
    );
    let summary = landing.summary(ReviewOutcome::Approved, 1, None, None);
    assert_eq!(summary.evidence, Some(LandingEvidence::Unconfirmed));
    assert_eq!(
        summary.reason.as_deref(),
        Some("the confirmation timed out")
    );
    // Cancelled while it ran.
    let summary = landed().summary(ReviewOutcome::Approved, 1, Some(Phase::Confirmation), None);
    assert_eq!(summary.status, LandingStatus::Landed);
    assert_eq!(summary.evidence, Some(LandingEvidence::Unconfirmed));
    assert_eq!(
        summary.reason.as_deref(),
        Some("the job was cancelled during the confirmation")
    );
}

#[test]
fn landing_after_approval_is_not_attempted_unless_the_review_approves() {
    let mut landing = Landing::new(LandMode::AfterApproval);
    landing.turn_started(TurnKind::Round);
    let summary = landing.summary(ReviewOutcome::Unresolved, 3, None, None);
    assert_eq!(summary.status, LandingStatus::NotAttempted);
    assert_eq!(summary.reason.as_deref(), Some("review not approved"));
    let summary = landing.summary(ReviewOutcome::Approved, 1, None, None);
    assert_eq!(
        summary.reason.as_deref(),
        Some("the job ended before the landing turn")
    );
    // A cancel during the landing turn leaves it unknown.
    let summary = landing.summary(ReviewOutcome::Approved, 1, Some(Phase::Landing), None);
    assert_eq!(summary.status, LandingStatus::Unknown);
    assert_eq!(
        summary.reason.as_deref(),
        Some("the job was cancelled during a turn that may land")
    );
    // A landing turn that reports it did not land.
    landing.record(
        TurnKind::Landing,
        1,
        None,
        &Ok(Some(report(ReportStatus::NotLanded, &[]))),
    );
    let summary = landing.summary(ReviewOutcome::Approved, 1, None, None);
    assert_eq!(summary.status, LandingStatus::NotLanded);
    assert_eq!(summary.detail.as_deref(), Some("detail"));
}

#[test]
fn a_cancel_freezes_the_state_and_keeps_a_fixed_approval() {
    let live = Live::new(3, LandMode::AfterApproval, "rev-1-abcdef".into());
    let token = CancellationToken::new();
    assert!(live.update(&token, |state| state.round = 1));
    let (review, landing) = live.snapshot(true);
    assert_eq!(review.outcome, ReviewOutcome::Stopped);
    assert_eq!(review.reason.as_deref(), Some("cancelled"));
    assert_eq!(landing.status, LandingStatus::NotAttempted);
    // Fixed first: the cancel cannot undo it.
    assert!(live.fix(&token, ReviewOutcome::Approved, None));
    token.cancel();
    assert!(!live.update(&token, |state| state.round = 2));
    assert!(!live.fix(&token, ReviewOutcome::Unresolved, Some("round_limit")));
    let (review, _) = live.snapshot(true);
    assert_eq!((review.outcome, review.round), (ReviewOutcome::Approved, 1));
    // Cancelled first: an approval that comes later is refused.
    let live = Live::new(3, LandMode::NoLanding, "rev-1-abcdef".into());
    let token = CancellationToken::new();
    token.cancel();
    assert!(!live.fix(&token, ReviewOutcome::Approved, None));
    assert_eq!(live.snapshot(true).0.outcome, ReviewOutcome::Stopped);
}

#[test]
fn the_running_status_shows_round_phase_and_mode() {
    let live = Live::new(3, LandMode::AfterApproval, "rev-1-abcdef".into());
    let token = CancellationToken::new();
    live.update(&token, |state| {
        state.round = 2;
        state.phase = Phase::Reviewer;
        state.reviewer = "claude".into();
    });
    assert_eq!(
        live.status(),
        json!({"round":2,"rounds":3,"phase":"reviewer","land":"after_approval",
               "journal":"rev-1-abcdef","reviewer":"claude"})
    );
    live.update(&token, |state| {
        state.builder_session = Some("codex-3".into());
    });
    assert_eq!(live.status()["builder_session"], "codex-3");
}

#[test]
fn a_due_confirmation_that_never_ran_is_unconfirmed_never_builder_reported() {
    let mut landing = Landing::new(LandMode::AfterApproval);
    landing.turn_started(TurnKind::Round);
    landing.record(
        TurnKind::Landing,
        1,
        None,
        &Ok(Some(report(ReportStatus::Landed, &["4f2a9c1"]))),
    );
    for (cut, halted, reason) in [
        (
            None,
            Some("the journal could not be written"),
            "the confirmation did not run: the journal could not be written",
        ),
        (
            Some(Phase::Landing),
            None,
            "the job was cancelled during a turn that may land",
        ),
        (
            Some(Phase::Reviewer),
            None,
            "the confirmation did not run: the job was cancelled",
        ),
    ] {
        let summary = landing.summary(ReviewOutcome::Approved, 1, cut, halted);
        assert_eq!(summary.reason.as_deref(), Some(reason), "{cut:?}");
        if summary.status == LandingStatus::Landed {
            assert_eq!(summary.evidence, Some(LandingEvidence::Unconfirmed));
        }
    }
}

#[test]
fn a_journal_failure_is_recorded_even_after_a_cancel() {
    let live = Live::new(3, LandMode::NoLanding, "rev-1-abcdef".into());
    let token = CancellationToken::new();
    live.fix(&token, ReviewOutcome::Approved, None);
    token.cancel();
    live.journal_failed();
    let (review, _) = live.snapshot(true);
    assert_eq!(review.outcome, ReviewOutcome::Approved);
    assert!(review.journal_incomplete);
}
