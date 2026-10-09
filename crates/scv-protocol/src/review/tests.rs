//! Unit tests for `src/review.rs`.

use super::*;
use crate::{JobChange, JobReport, JobStatus, OriginKind, TurnOrigin, describe_reports};

fn review(outcome: ReviewOutcome, reason: Option<&str>) -> ReviewSummary {
    ReviewSummary {
        outcome,
        reason: reason.map(str::to_owned),
        round: 2,
        rounds: 3,
        reviewer: "claude".into(),
        fallback: None,
        tried: vec![TriedReviewer {
            agent: "claude".into(),
            result: ReviewerResult::Verdict,
        }],
        refusals: Vec::new(),
        summary: None,
        open_count: 0,
        open: Vec::new(),
        journal: "rev-1759961234-3fa9c1".into(),
        journal_incomplete: false,
        journal_pending: false,
    }
}

fn landing(status: LandingStatus) -> LandingSummary {
    LandingSummary {
        mode: LandMode::NoLanding,
        status,
        reference: None,
        commits: Vec::new(),
        evidence: None,
        checked_by: None,
        landed_before_review: false,
        unauthorized: false,
        reason: None,
        detail: None,
    }
}

fn outcome(review: ReviewSummary, landing: LandingSummary) -> JobOutcome {
    JobOutcome {
        job: "job-4".into(),
        review,
        landing,
    }
}

#[test]
fn an_approved_review_that_landed_reads_as_two_separate_facts() {
    let landed = LandingSummary {
        mode: LandMode::AfterApproval,
        reference: Some("origin/main".into()),
        commits: vec!["4f2a9c1e0b7d".into()],
        evidence: Some(LandingEvidence::ReviewerConfirmed),
        checked_by: Some("claude".into()),
        ..landing(LandingStatus::Landed)
    };
    let approved = outcome(review(ReviewOutcome::Approved, None), landed);
    assert_eq!(
        outcome_notice(&approved),
        "job-4 · Review: approved · round 2 of 3 · reviewer claude\n\
         job-4 · Landing: landed · 4f2a9c1 → origin/main · confirmed by reviewer claude"
    );
    assert_eq!(
        describe_outcome(&approved),
        "Review: approved · round 2 of 3 · reviewer claude\n\
         Landing: landed · 4f2a9c1 → origin/main · confirmed by reviewer claude\n"
    );
}

#[test]
fn every_review_outcome_has_its_line() {
    let line = |summary: ReviewSummary| {
        outcome_notice(&outcome(summary, landing(LandingStatus::NotRequested)))
            .lines()
            .next()
            .unwrap()
            .to_owned()
    };
    let fallback = ReviewSummary {
        reviewer: "grok".into(),
        fallback: Some("codex unavailable".into()),
        ..review(ReviewOutcome::Approved, None)
    };
    assert_eq!(
        line(fallback.clone()),
        "job-4 · Review: approved · round 2 of 3 · reviewer grok (codex unavailable)"
    );
    let escalated = ReviewSummary {
        summary: Some("The brief asks to drop a table; the user must decide.".into()),
        ..review(ReviewOutcome::Escalated, None)
    };
    assert_eq!(
        line(escalated),
        "job-4 · Review: NOT approved · escalated in round 2 of 3 · reviewer claude: \"The brief \
         asks to drop a table; the user must decide.\""
    );
    for (reason, phrase) in [
        (
            "reviewer_timeout",
            "reviewer grok (codex unavailable) timed out",
        ),
        (
            "reviewer_failed",
            "reviewer grok (codex unavailable) failed",
        ),
        (
            "reviewer_cancelled",
            "reviewer grok (codex unavailable) was stopped",
        ),
        (
            "reviewer_declined",
            "reviewer grok (codex unavailable) declined",
        ),
        (
            "no_conversation_slot",
            "no room for a reviewer conversation",
        ),
        (
            "malformed_verdict",
            "the verdict of reviewer grok (codex unavailable) stayed malformed",
        ),
    ] {
        let summary = ReviewSummary {
            outcome: ReviewOutcome::NoVerdict,
            reason: Some(reason.into()),
            ..fallback.clone()
        };
        assert_eq!(
            line(summary),
            format!("job-4 · Review: NOT approved · no verdict in round 2 of 3 · {phrase}")
        );
    }
    let none = ReviewSummary {
        reviewer: "claude".into(),
        tried: ["codex", "grok", "claude"]
            .into_iter()
            .map(|agent| TriedReviewer {
                agent: agent.into(),
                result: ReviewerResult::Unavailable,
            })
            .collect(),
        ..review(ReviewOutcome::NoVerdict, Some("no_reviewer_available"))
    };
    assert_eq!(
        line(none),
        "job-4 · Review: NOT approved · no verdict in round 2 of 3 · no reviewer available \
         (codex, grok, claude)"
    );
    for (reason, phrase) in [
        ("builder_failed", "the builder turn failed"),
        ("builder_timeout", "the builder turn timed out"),
        ("builder_declined", "the builder declined"),
        ("builder_cancelled", "the builder turn was stopped"),
        (
            "builder_no_session",
            "the builder returned no conversation to continue",
        ),
        ("cancelled", "the job was cancelled"),
        ("journal_error", "the journal could not be written"),
    ] {
        assert_eq!(
            line(review(ReviewOutcome::Stopped, Some(reason))),
            format!("job-4 · Review: NOT approved · stopped in round 2 of 3 · {phrase}")
        );
    }
    let queued = ReviewSummary {
        round: 0,
        reviewer: String::new(),
        tried: Vec::new(),
        ..review(ReviewOutcome::Stopped, Some("cancelled"))
    };
    assert_eq!(
        line(queued),
        "job-4 · Review: NOT approved · stopped before round 1 · the job was cancelled while queued"
    );
    assert_eq!(
        line(review(ReviewOutcome::Unknown, None)),
        "job-4 · Review: unknown outcome · see journal rev-1759961234-3fa9c1"
    );
}

#[test]
fn an_unresolved_review_lists_its_open_findings_unprefixed() {
    let open = (1..=6)
        .map(|n| OpenFinding {
            id: format!("3.{n}"),
            title: format!("Finding {n}"),
            location: (n == 1).then(|| "shop/src/cart.rs:88".to_owned()),
        })
        .collect();
    let unresolved = ReviewSummary {
        round: 3,
        open_count: 7,
        open,
        ..review(ReviewOutcome::Unresolved, Some("round_limit"))
    };
    let landing = LandingSummary {
        mode: LandMode::AfterApproval,
        reason: Some("review not approved".into()),
        ..landing(LandingStatus::NotAttempted)
    };
    assert_eq!(
        outcome_notice(&outcome(unresolved.clone(), landing.clone())),
        "job-4 · Review: NOT approved · unresolved after 3 of 3 rounds · 7 blocking findings open:\n  \
         - \"Finding 1\" (shop/src/cart.rs:88)\n  - \"Finding 2\"\n  - \"Finding 3\"\n  - \"Finding 4\"\n  \
         - \"Finding 5\"\n  - and 2 more\n\
         job-4 · Landing: not attempted · review not approved"
    );
    let one = ReviewSummary {
        open_count: 1,
        open: unresolved.open[..1].to_vec(),
        ..unresolved
    };
    assert!(
        describe_outcome(&outcome(one, landing))
            .starts_with("Review: NOT approved · unresolved after 3 of 3 rounds · 1 blocking finding open:\n  - \"Finding 1\"")
    );
}

#[test]
fn every_landing_status_has_its_line() {
    let line = |landing: LandingSummary| {
        outcome_notice(&outcome(review(ReviewOutcome::Approved, None), landing))
            .lines()
            .last()
            .unwrap()
            .to_owned()
    };
    let landed = LandingSummary {
        mode: LandMode::BeforeReview,
        reference: Some("origin/main".into()),
        commits: ["4f2a9c1e0b7d", "9e8d7c6", "1111111", "2222222", "3333333"]
            .map(str::to_owned)
            .to_vec(),
        landed_before_review: true,
        ..landing(LandingStatus::Landed)
    };
    assert_eq!(
        line(landed.clone()),
        "job-4 · Landing: landed before review · 4f2a9c1, 9e8d7c6, 1111111 +2 more → origin/main · \
         builder-reported, not verified"
    );
    let disputed = LandingSummary {
        evidence: Some(LandingEvidence::ReviewerDisputed),
        checked_by: Some("codex".into()),
        reason: Some("the landed change does not match the approved one".into()),
        landed_before_review: false,
        commits: vec!["4f2a9c1".into()],
        ..landed.clone()
    };
    assert_eq!(
        line(disputed.clone()),
        "job-4 · Landing: landed · 4f2a9c1 → origin/main · reviewer codex could NOT confirm: the \
         landed change does not match the approved one"
    );
    let unconfirmed = LandingSummary {
        evidence: Some(LandingEvidence::Unconfirmed),
        reason: Some("the confirmation timed out".into()),
        ..disputed.clone()
    };
    assert_eq!(
        line(unconfirmed),
        "job-4 · Landing: landed · 4f2a9c1 → origin/main · NOT confirmed: the confirmation timed out"
    );
    let unauthorized = LandingSummary {
        evidence: Some(LandingEvidence::BuilderReported),
        unauthorized: true,
        reason: None,
        ..disputed.clone()
    };
    assert_eq!(
        line(unauthorized),
        "job-4 · Landing: landed · 4f2a9c1 → origin/main · builder-reported, not verified · NOT \
         authorized by this call"
    );
    let not_landed = LandingSummary {
        detail: Some("a gate failed: cargo deny".into()),
        ..landing(LandingStatus::NotLanded)
    };
    assert_eq!(
        line(not_landed),
        "job-4 · Landing: not landed · builder-reported: \"a gate failed: cargo deny\""
    );
    let failed = LandingSummary {
        detail: Some("push rejected".into()),
        reference: Some("origin/main".into()),
        commits: vec!["4f2a9c1".into()],
        ..landing(LandingStatus::Failed)
    };
    assert_eq!(
        line(failed),
        "job-4 · Landing: failed · builder-reported · partial state possible: \"push rejected\"; \
         earlier: 4f2a9c1 → origin/main"
    );
    let unknown = LandingSummary {
        reason: Some("the landing turn timed out".into()),
        ..landing(LandingStatus::Unknown)
    };
    assert_eq!(
        line(unknown),
        "job-4 · Landing: unknown · the landing turn timed out · check before relying on it"
    );
    assert_eq!(
        line(landing(LandingStatus::NotRequested)),
        "job-4 · Landing: not requested"
    );
}

#[test]
fn outcomes_round_trip_and_newer_values_read_as_unknown() {
    let landed = LandingSummary {
        mode: LandMode::AfterApproval,
        reference: Some("origin/main".into()),
        commits: vec!["4f2a9c1".into()],
        evidence: Some(LandingEvidence::Unconfirmed),
        checked_by: Some("codex".into()),
        reason: Some("the confirmation timed out".into()),
        ..landing(LandingStatus::Landed)
    };
    let value = outcome(review(ReviewOutcome::Approved, None), landed);
    let wire = serde_json::to_value(&value).unwrap();
    assert_eq!(wire["landing"]["ref"], "origin/main");
    assert_eq!(wire["landing"]["mode"], "after_approval");
    assert_eq!(wire["landing"]["evidence"], "unconfirmed");
    assert!(wire["landing"].get("unauthorized").is_none(), "{wire}");
    assert_eq!(serde_json::from_value::<JobOutcome>(wire).unwrap(), value);
    let newer: JobOutcome = serde_json::from_value(serde_json::json!({
        "job":"job-1",
        "review":{"outcome":"vetoed","round":1,"rounds":3,"journal":"rev-1-abcdef",
                  "tried":[{"agent":"claude","result":"abstained"}]},
        "landing":{"mode":"later","status":"queued","evidence":"notarized"}
    }))
    .unwrap();
    assert_eq!(newer.review.outcome, ReviewOutcome::Unknown);
    assert_eq!(newer.review.tried[0].result, ReviewerResult::Unknown);
    assert_eq!(newer.landing.mode, LandMode::Unknown);
    assert_eq!(newer.landing.status, LandingStatus::Unknown);
    assert_eq!(newer.landing.evidence, Some(LandingEvidence::Unknown));
    assert_eq!(
        outcome_notice(&newer),
        "job-1 · Review: unknown outcome · see journal rev-1-abcdef\n\
         job-1 · Landing: unknown · check before relying on it"
    );
}

#[test]
fn reports_changes_and_origins_carry_outcomes_and_older_frames_parse() {
    let value = outcome(
        review(ReviewOutcome::Approved, None),
        landing(LandingStatus::NotRequested),
    );
    let report = JobReport {
        job: "job-4".into(),
        agent: "codex".into(),
        task: "Fix the flaky checkout test".into(),
        status: JobStatus::Completed,
        session: Some("codex-3".into()),
        reply: "Fixed the race.".into(),
        outcome: Some(value.clone()),
    };
    assert_eq!(
        describe_reports(std::slice::from_ref(&report)),
        "job-4 (codex, conversation codex-3): completed\nTask: Fix the flaky checkout test\n\
         Review: approved · round 2 of 3 · reviewer claude\nLanding: not requested\n\
         Fixed the race.\n"
    );
    let wire = serde_json::to_string(&report).unwrap();
    assert_eq!(serde_json::from_str::<JobReport>(&wire).unwrap(), report);
    let older: JobReport = serde_json::from_str(
        r#"{"job":"job-1","agent":"codex","status":"completed","reply":"done"}"#,
    )
    .unwrap();
    assert_eq!(older.outcome, None);
    assert!(!serde_json::to_string(&older).unwrap().contains("outcome"));

    let started = JobChange {
        job: "job-4".into(),
        tool: "agent".into(),
        agent: "codex".into(),
        status: JobStatus::Running,
        task: String::new(),
        journal: Some("rev-1759961234-3fa9c1".into()),
        outcome: None,
    };
    let wire = serde_json::to_value(&started).unwrap();
    assert_eq!(wire["journal"], "rev-1759961234-3fa9c1");
    assert_eq!(serde_json::from_value::<JobChange>(wire).unwrap(), started);
    let settled = JobChange {
        status: JobStatus::Cancelled,
        journal: None,
        outcome: Some(value.clone()),
        ..started
    };
    let wire = serde_json::to_string(&settled).unwrap();
    assert_eq!(serde_json::from_str::<JobChange>(&wire).unwrap(), settled);

    let origin = TurnOrigin {
        kind: OriginKind::Background,
        jobs: vec!["job-4".into()],
        retry_seconds: None,
        outcomes: vec![value],
    };
    let wire = serde_json::to_string(&origin).unwrap();
    assert_eq!(serde_json::from_str::<TurnOrigin>(&wire).unwrap(), origin);
    let older: TurnOrigin =
        serde_json::from_str(r#"{"kind":"background","jobs":["job-1"]}"#).unwrap();
    assert!(older.outcomes.is_empty());
}

#[test]
fn a_journal_that_failed_after_the_outcome_is_named_beside_it() {
    let incomplete = ReviewSummary {
        journal_incomplete: true,
        ..review(ReviewOutcome::Approved, None)
    };
    let text = outcome_notice(&outcome(incomplete, landing(LandingStatus::NotRequested)));
    assert_eq!(
        text.lines().next().unwrap(),
        "job-4 · Review: approved · round 2 of 3 · reviewer claude · journal \
         rev-1759961234-3fa9c1 INCOMPLETE: a write failed"
    );
    let wire = serde_json::to_value(outcome(
        review(ReviewOutcome::Approved, None),
        landing(LandingStatus::NotRequested),
    ))
    .unwrap();
    assert!(wire["review"].get("journal_incomplete").is_none(), "{wire}");
    // A failure before the outcome is the outcome itself, said once.
    let stopped = ReviewSummary {
        journal_incomplete: true,
        ..review(ReviewOutcome::Stopped, Some("journal_error"))
    };
    assert_eq!(
        outcome_notice(&outcome(stopped, landing(LandingStatus::NotRequested)))
            .lines()
            .next()
            .unwrap(),
        "job-4 · Review: NOT approved · stopped in round 2 of 3 · the journal could not be written"
    );
}

#[test]
fn a_report_quotes_what_each_declining_reviewer_said_and_the_notice_does_not() {
    let declined = ReviewSummary {
        reviewer: "grok".into(),
        fallback: Some("codex declined".into()),
        refusals: vec![Refusal {
            agent: "codex".into(),
            reply: format!("I decline: {}", "x".repeat(400)),
        }],
        ..review(ReviewOutcome::Approved, None)
    };
    let value = outcome(declined, landing(LandingStatus::NotRequested));
    let described = describe_outcome(&value);
    let quoted = described.lines().nth(1).unwrap();
    assert!(
        quoted.starts_with("  - reviewer codex declined, saying (untrusted): \"I decline: xxx"),
        "{described}"
    );
    assert!(quoted.ends_with("…\""), "{quoted}");
    assert!(quoted.chars().count() < 360, "{quoted}");
    assert!(!outcome_notice(&value).contains("I decline"));
}

#[test]
fn an_outcome_stated_before_the_job_stopped_says_its_journal_is_still_open() {
    let pending = ReviewSummary {
        journal_pending: true,
        ..review(ReviewOutcome::Stopped, Some("cancelled"))
    };
    assert_eq!(
        outcome_notice(&outcome(pending, landing(LandingStatus::NotRequested)))
            .lines()
            .next()
            .unwrap(),
        "job-4 · Review: NOT approved · stopped in round 2 of 3 · the job was cancelled · \
         journal rev-1759961234-3fa9c1 still open: the job is still stopping, an update follows"
    );
    let update = crate::ServerEvent::BackgroundUpdated {
        session_id: "s".into(),
        seq: 9,
        outcomes: vec![outcome(
            review(ReviewOutcome::Approved, None),
            landing(LandingStatus::NotRequested),
        )],
    };
    let wire = serde_json::to_value(&update).unwrap();
    assert_eq!(wire["type"], "background.updated");
    assert_eq!(
        serde_json::from_value::<crate::ServerEvent>(wire).unwrap(),
        update
    );
    assert_eq!(update.turn_request_id(), None);
}
