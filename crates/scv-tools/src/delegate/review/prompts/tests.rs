//! Unit tests for `src/delegate/review/prompts.rs`.

use super::*;

fn finding(id: &str, severity: Severity) -> Finding {
    Finding {
        id: id.into(),
        severity,
        title: "Sleep instead of a lock".into(),
        detail: Some("cart.rs:88 sleeps before commit".into()),
        location: Some("shop/src/cart.rs:88".into()),
    }
}

#[test]
fn the_builder_notice_names_the_rounds_and_the_landing_mode() {
    let none = builder_notice(LandMode::NoLanding, 3);
    assert!(none.contains("up to 3 rounds"), "{none}");
    assert!(none.contains("```scv-landing\n"), "{none}");
    assert!(none.ends_with("message anyone in this job."), "{none}");
    let after = builder_notice(LandMode::AfterApproval, 5);
    assert!(after.contains("name its base and head commits"), "{after}");
    let before = builder_notice(LandMode::BeforeReview, 3);
    assert!(
        before.contains("authorizes landing before review"),
        "{before}"
    );
}

#[test]
fn the_reviewer_is_never_told_the_round_limit() {
    let open = [finding("1.1", Severity::Blocking)];
    for land in [
        LandMode::NoLanding,
        LandMode::AfterApproval,
        LandMode::BeforeReview,
    ] {
        let prompt = reviewer_prompt(&ReviewerBrief {
            land,
            prompt: "Fix the flaky checkout test.",
            focus: Some("no sleeps"),
            reply: "Replaced the sleep with a lock.",
            open: &open,
            landed: None,
        });
        // It may hear that rounds exist, never how many or which this is.
        for told in ["3 rounds", " of 3", "round 1", "round 2", "rounds left"] {
            assert!(!prompt.contains(told), "{told}: {prompt}");
        }
        assert!(prompt.contains("1.1 [blocking] \"Sleep instead of a lock\""));
        assert!(prompt.contains("Focus from the user: no sleeps"));
        assert!(prompt.contains("```scv-verdict\n"));
        assert!(
            prompt
                .contains("Commits the brief names as landed before this job are not a violation.")
        );
        assert!(prompt.contains("untrusted"));
    }
}

#[test]
fn the_reviewer_checks_a_reported_landing_and_names_what_it_approves() {
    let commits = ["4f2a9c1".to_owned()];
    let prompt = reviewer_prompt(&ReviewerBrief {
        land: LandMode::BeforeReview,
        prompt: "Hotfix it.",
        focus: None,
        reply: "Landed.",
        open: &[],
        landed: Some(("origin/main", &commits)),
    });
    assert!(prompt.contains("landed 4f2a9c1 on origin/main"), "{prompt}");
    assert!(!prompt.contains("settle every open finding"));
    let after = reviewer_prompt(&ReviewerBrief {
        land: LandMode::AfterApproval,
        prompt: "Fix it.",
        focus: None,
        reply: "Done.",
        open: &[],
        landed: None,
    });
    assert!(after.contains("\"approved\""), "{after}");
}

#[test]
fn quoted_text_is_bounded() {
    let long = "x".repeat(20 * 1024);
    let prompt = reviewer_prompt(&ReviewerBrief {
        land: LandMode::NoLanding,
        prompt: &long,
        focus: None,
        reply: &long,
        open: &[],
        landed: None,
    });
    assert!(prompt.len() < 20 * 1024, "{}", prompt.len());
    assert_eq!(prompt.matches("[cut at 8 KiB]").count(), 2);
}

#[test]
fn the_fix_prompt_lists_findings_and_what_already_landed() {
    let findings = [
        finding("1.1", Severity::Blocking),
        finding("1.2", Severity::Minor),
    ];
    let commits = ["4f2a9c1".to_owned()];
    let prompt = fix_prompt(
        2,
        3,
        LandMode::BeforeReview,
        &findings,
        Some(("origin/main", &commits)),
    );
    assert!(prompt.starts_with("[SCV review, round 2 of 3]"), "{prompt}");
    assert!(prompt.contains("already landed on origin/main (4f2a9c1)"));
    assert!(prompt.contains("Land the fix as before"));
    assert!(prompt.contains("- 1.1 [blocking]"));
    assert!(prompt.contains("- 1.2 [minor]"));
    let held = fix_prompt(
        2,
        3,
        LandMode::NoLanding,
        &findings,
        Some(("origin/main", &commits)),
    );
    assert!(held.contains("Do not land the fix now."), "{held}");
}

#[test]
fn the_confirmation_names_the_approved_range_and_the_ref() {
    let prompt = confirmation_prompt(
        ("1b2c3d4", "7e8f9a0"),
        1,
        r#"{"status":"landed","ref":"origin/main","commits":["4f2a9c1"]}"#,
        "origin/main",
    );
    assert!(
        prompt.contains("You approved 1b2c3d4..7e8f9a0 in round 1"),
        "{prompt}"
    );
    assert!(prompt.contains("```scv-landing-check\n"));
    assert!(prompt.contains("Do not edit, commit, land, revert, push"));
    assert!(repair_prompt("no scv-verdict block").contains("(no scv-verdict block)"));
}

/// `count` findings from round `round`, each with a long detail.
fn many(round: u32, count: usize, title: &str, detail: usize) -> Vec<Finding> {
    (1..=count)
        .map(|n| Finding {
            id: format!("{round}.{n}"),
            severity: Severity::Blocking,
            title: format!("{title} {n}"),
            detail: Some("d".repeat(detail)),
            location: Some(format!("src/lib.rs:{n}")),
        })
        .collect()
}

fn listed(text: &str, findings: &[Finding]) -> Vec<String> {
    findings
        .iter()
        .filter(|finding| !text.contains(&format!("- {} [blocking] \"", finding.id)))
        .map(|finding| finding.id.clone())
        .collect()
}

#[test]
fn every_open_finding_reaches_the_next_reviewer_and_the_builder() {
    // Two verdicts of ten blocking findings with long details: the case an
    // earlier whole-list cut lost 2.4 to 2.10 in.
    let mut open = many(1, 10, "Defect", 1300);
    open.extend(many(2, 10, "Defect", 1300));
    let prompt = reviewer_prompt(&ReviewerBrief {
        land: LandMode::NoLanding,
        prompt: "Fix the project.",
        focus: None,
        reply: "I fixed the findings.",
        open: &open,
        landed: None,
    });
    assert!(
        listed(&prompt, &open).is_empty(),
        "{:?}",
        listed(&prompt, &open)
    );
    let fix = fix_prompt(3, 5, LandMode::NoLanding, &open, None);
    assert!(listed(&fix, &open).is_empty());
    // Far more than fits: details shortened, every line kept.
    let open: Vec<Finding> = (1..=6)
        .flat_map(|round| many(round, 20, "Defect", 2000))
        .collect();
    let list = finding_list(&open, FINDINGS_BYTES);
    assert!(list.len() <= FINDINGS_BYTES, "{}", list.len());
    assert!(list.starts_with("(Details are shortened to fit"));
    assert!(listed(&list, &open).is_empty());
    assert!(list.contains("(src/lib.rs:20)"));
    // So many that only shortened lines fit.
    let title = "é".repeat(200);
    let open: Vec<Finding> = (1..=20)
        .flat_map(|round| many(round, 20, &title, 2000))
        .collect();
    let list = finding_list(&open, FINDINGS_BYTES);
    assert!(list.len() <= FINDINGS_BYTES, "{}", list.len());
    assert!(list.starts_with("(Too many findings to quote in full"));
    assert!(listed(&list, &open).is_empty());
    assert!(!list.contains("dddd"));
}
