//! Unit tests for `src/delegate/review/blocks.rs`.

use super::*;

/// `body` as a reply ending with a fenced block with `info`.
fn reply(info: &str, body: &str) -> String {
    format!("I checked the work.\n\n```{info}\n{body}\n```\n")
}

fn verdict(body: &str) -> Result<Verdict, String> {
    parse_verdict(&reply(VERDICT_BLOCK, body), false, &Expect::default())
}

const APPROVE: &str =
    r#"{"verdict":"approve","summary":"Looks right.","evidence":["ran cargo test: 12 passed"]}"#;

#[test]
fn each_verdict_is_accepted() {
    let approved = verdict(APPROVE).unwrap();
    assert_eq!(approved.decision, Decision::Approve);
    assert_eq!(approved.summary, "Looks right.");
    assert_eq!(approved.evidence, vec!["ran cargo test: 12 passed"]);
    let changes = verdict(
        r#"{"verdict":"changes","summary":"A race.","findings":[
            {"severity":"blocking","title":"Sleep instead of a lock","location":"shop/src/cart.rs:88"},
            {"severity":"minor","title":"Test name typo"}]}"#,
    )
    .unwrap();
    assert_eq!(changes.decision, Decision::Changes);
    assert_eq!(changes.findings.len(), 2);
    assert_eq!(changes.findings[0].severity, Severity::Blocking);
    assert_eq!(
        changes.findings[0].location.as_deref(),
        Some("shop/src/cart.rs:88")
    );
    let escalated =
        verdict(r#"{"verdict":"escalate","summary":"The brief drops a table."}"#).unwrap();
    assert_eq!(escalated.decision, Decision::Escalate);
}

#[test]
fn only_the_last_block_counts_and_prose_never_does() {
    let text = format!(
        "LGTM, approve!\n```{VERDICT_BLOCK}\n{APPROVE}\n```\nOn second thought:\n```{VERDICT_BLOCK}\n\
         {{\"verdict\":\"changes\",\"summary\":\"No.\",\"findings\":[{{\"severity\":\"blocking\",\"title\":\"Broken\"}}]}}\n```"
    );
    let parsed = parse_verdict(&text, false, &Expect::default()).unwrap();
    assert_eq!(parsed.decision, Decision::Changes);
    assert_eq!(
        verdict_error("LGTM, approved."),
        format!("no {VERDICT_BLOCK} block")
    );
}

fn verdict_error(text: &str) -> String {
    parse_verdict(text, false, &Expect::default()).unwrap_err()
}

#[test]
fn a_block_cut_off_or_in_a_truncated_reply_is_malformed() {
    let open = format!("```{VERDICT_BLOCK}\n{APPROVE}\n");
    assert!(
        verdict_error(&open).contains("cut off"),
        "{}",
        verdict_error(&open)
    );
    let truncated = parse_verdict(&reply(VERDICT_BLOCK, APPROVE), true, &Expect::default());
    assert!(truncated.unwrap_err().contains("cut off"));
}

#[test]
fn structural_violations_are_malformed() {
    for (body, error) in [
        ("{not json", "not valid JSON"),
        ("[1,2]", "not a JSON object"),
        (r#"{"verdict":"lgtm","summary":"x"}"#, "is not approve"),
        (
            r#"{"verdict":"approve","summary":"  "}"#,
            "summary is required",
        ),
        (
            r#"{"verdict":"approve","summary":"x"}"#,
            "approve needs evidence",
        ),
        (
            r#"{"verdict":"approve","summary":"x","evidence":["e"],"findings":[{"severity":"blocking","title":"t"}]}"#,
            "raises a new blocking finding",
        ),
        (r#"{"verdict":"changes","summary":"x"}"#, "changes needs"),
        (
            r#"{"verdict":"changes","summary":"x","findings":[{"severity":"minor","title":"t"}]}"#,
            "changes needs",
        ),
        (
            r#"{"verdict":"changes","summary":"x","findings":[{"severity":"urgent","title":"t"}]}"#,
            "not blocking or minor",
        ),
        (
            r#"{"verdict":"changes","summary":"x","findings":[{"severity":"blocking"}]}"#,
            "title is required",
        ),
        (
            r#"{"verdict":"approve","summary":"x","evidence":"ran it"}"#,
            "must be an array",
        ),
    ] {
        let found = verdict(body).unwrap_err();
        assert!(found.contains(error), "{body}: {found}");
    }
    let many: Vec<String> = (0..21)
        .map(|n| format!(r#"{{"severity":"minor","title":"t{n}"}}"#))
        .collect();
    let body = format!(
        r#"{{"verdict":"escalate","summary":"x","findings":[{}]}}"#,
        many.join(",")
    );
    assert!(verdict(&body).unwrap_err().contains("more than 20"));
    let big = format!(
        r#"{{"verdict":"escalate","summary":"{}"}}"#,
        "x".repeat(17 * 1024)
    );
    assert!(verdict(&big).unwrap_err().contains("over 16 KiB"));
}

#[test]
fn prior_settles_each_open_finding_exactly_once() {
    let open = ["1.1".to_owned(), "1.2".to_owned()];
    let expect = Expect {
        open: &open,
        ..Expect::default()
    };
    let parse = |body: &str| parse_verdict(&reply(VERDICT_BLOCK, body), false, &expect);
    let settled = parse(
        r#"{"verdict":"approve","summary":"Fixed.","evidence":["ran it 20x"],
            "prior":[{"id":"1.1","status":"resolved"},
                     {"id":"1.2","status":"withdrawn","note":"the builder showed the retry is bounded"}]}"#,
    )
    .unwrap();
    assert_eq!(settled.prior[1].status, Settlement::Withdrawn);
    for (body, error) in [
        (
            r#"{"verdict":"changes","summary":"x","prior":[{"id":"1.1","status":"open"}]}"#,
            "prior must settle open finding 1.2",
        ),
        (
            r#"{"verdict":"changes","summary":"x","prior":[{"id":"1.1","status":"open"},{"id":"1.1","status":"open"},{"id":"1.2","status":"open"}]}"#,
            "prior settles 1.1 twice",
        ),
        (
            r#"{"verdict":"changes","summary":"x","prior":[{"id":"1.1","status":"open"},{"id":"1.2","status":"open"},{"id":"2.9","status":"open"}]}"#,
            "not an open finding",
        ),
        (
            r#"{"verdict":"changes","summary":"x","prior":[{"id":"1.1","status":"open"},{"id":"1.2","status":"withdrawn"}]}"#,
            "withdrawn without a note",
        ),
        (
            r#"{"verdict":"approve","summary":"x","evidence":["e"],"prior":[{"id":"1.1","status":"open"},{"id":"1.2","status":"resolved"}]}"#,
            "approve leaves a prior finding open",
        ),
    ] {
        let found = parse(body).unwrap_err();
        assert!(found.contains(error), "{body}: {found}");
    }
    // A still-open prior finding makes changes valid without new findings.
    let changes = parse(
        r#"{"verdict":"changes","summary":"x","prior":[{"id":"1.1","status":"open"},{"id":"1.2","status":"resolved"}]}"#,
    )
    .unwrap();
    assert_eq!(changes.decision, Decision::Changes);
    // Without open findings, any prior entry names an unknown one.
    assert!(
        verdict(r#"{"verdict":"approve","summary":"x","evidence":["e"],"prior":[{"id":"1.1","status":"resolved"}]}"#)
            .unwrap_err()
            .contains("not an open finding")
    );
}

#[test]
fn landing_check_and_approved_range_are_required_when_due() {
    let landed = Expect {
        landed: true,
        ..Expect::default()
    };
    let error = parse_verdict(&reply(VERDICT_BLOCK, APPROVE), false, &landed).unwrap_err();
    assert!(error.contains("landing_check is required"), "{error}");
    let checked = parse_verdict(
        &reply(
            VERDICT_BLOCK,
            r#"{"verdict":"approve","summary":"x","evidence":["e"],
                "landing_check":{"status":"confirmed","ref":"origin/main","commits":["4f2a9c1"],
                                 "evidence":["git branch -r --contains 4f2a9c1"]}}"#,
        ),
        false,
        &landed,
    )
    .unwrap();
    let check = checked.landing_check.unwrap();
    assert_eq!(check.status, CheckStatus::Confirmed);
    assert_eq!(check.reference.as_deref(), Some("origin/main"));
    let after = Expect {
        after_approval: true,
        ..Expect::default()
    };
    let error = parse_verdict(&reply(VERDICT_BLOCK, APPROVE), false, &after).unwrap_err();
    assert!(error.contains("approved base and head"), "{error}");
    let approved = parse_verdict(
        &reply(
            VERDICT_BLOCK,
            r#"{"verdict":"approve","summary":"x","evidence":["e"],"approved":{"base":"1b2c3d4","head":"7e8f9a0"}}"#,
        ),
        false,
        &after,
    )
    .unwrap();
    assert_eq!(
        approved.approved,
        Some(Approved {
            base: "1b2c3d4".into(),
            head: "7e8f9a0".into()
        })
    );
    let bad = parse_verdict(
        &reply(
            VERDICT_BLOCK,
            r#"{"verdict":"approve","summary":"x","evidence":["e"],"approved":{"base":"HEAD~1","head":"7e8f9a0"}}"#,
        ),
        false,
        &after,
    );
    assert!(bad.unwrap_err().contains("approved.base"));
    // Otherwise both are ignored.
    let ignored = verdict(
        r#"{"verdict":"approve","summary":"x","evidence":["e"],"landing_check":"whatever","approved":7}"#,
    )
    .unwrap();
    assert_eq!((ignored.landing_check, ignored.approved), (None, None));
}

#[test]
fn text_is_cleaned_cut_and_unknown_fields_are_ignored() {
    let long = "y".repeat(1200);
    let parsed = verdict(&format!(
        r#"{{"verdict":"escalate","summary":"{long}","mood":"great",
            "findings":[{{"severity":"minor","title":"a\tb\u0007c\nd","detail":"use `Mutex` here, not ```sleep```"}}]}}"#
    ))
    .unwrap();
    assert_eq!(parsed.summary.chars().count(), 1000);
    assert!(parsed.summary.ends_with('…'));
    assert_eq!(parsed.findings[0].title, "a bc d");
    assert_eq!(
        parsed.findings[0].detail.as_deref(),
        Some("use `Mutex` here, not ```sleep```")
    );
}

#[test]
fn backticks_inside_the_json_do_not_end_the_block() {
    let text = format!(
        "```{VERDICT_BLOCK}\n{{\"verdict\":\"changes\",\"summary\":\"see ``` below\",\
         \"findings\":[{{\"severity\":\"blocking\",\"title\":\"```rust fence```\"}}]}}\n```"
    );
    let parsed = parse_verdict(&text, false, &Expect::default()).unwrap();
    assert_eq!(parsed.findings[0].title, "```rust fence```");
    // Other fenced blocks before it are skipped, and a longer fence works.
    let text = format!("```rust\nfn main() {{}}\n```\n````{VERDICT_BLOCK}\n{APPROVE}\n````\n");
    assert_eq!(
        parse_verdict(&text, false, &Expect::default())
            .unwrap()
            .decision,
        Decision::Approve
    );
}

#[test]
fn landing_reports_are_checked_per_status() {
    let landing = |body: &str| parse_landing(&reply(LANDING_BLOCK, body), false);
    let landed = landing(
        r#"{"status":"landed","ref":"origin/main","commits":["4f2a9c1e0b7d"],
            "url":"https://github.com/o/shop/pull/77","detail":"squash-merged PR #77"}"#,
    )
    .unwrap()
    .unwrap();
    assert_eq!(landed.status, ReportStatus::Landed);
    assert_eq!(landed.reference.as_deref(), Some("origin/main"));
    assert_eq!(
        landed.url.as_deref(),
        Some("https://github.com/o/shop/pull/77")
    );
    let not = landing(r#"{"status":"not_landed","detail":"a gate failed"}"#)
        .unwrap()
        .unwrap();
    assert_eq!((not.status, not.reference), (ReportStatus::NotLanded, None));
    let failed = landing(r#"{"status":"failed","detail":"push rejected"}"#)
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, ReportStatus::Failed);
    for (body, error) in [
        (
            r#"{"status":"landed","commits":["4f2a9c1"]}"#,
            "names its ref",
        ),
        (
            r#"{"status":"landed","ref":"origin/main"}"#,
            "names its commits",
        ),
        (
            r#"{"status":"landed","ref":"origin/main","commits":[]}"#,
            "names its commits",
        ),
        (
            r#"{"status":"landed","ref":"origin/main","commits":["4F2A9C1"]}"#,
            "lowercase hex",
        ),
        (
            r#"{"status":"landed","ref":"origin/main","commits":["abc"]}"#,
            "lowercase hex",
        ),
        (
            r#"{"status":"landed","ref":"origin main","commits":["4f2a9c1"]}"#,
            "is not 1 to 200",
        ),
        (r#"{"status":"merged"}"#, "is not landed"),
    ] {
        assert!(
            landing(body).unwrap_err().contains(error),
            "{body}: {:?}",
            landing(body)
        );
    }
    let commits: Vec<String> = (0..21).map(|n| format!("\"{n:07x}\"")).collect();
    let many = format!(
        r#"{{"status":"landed","ref":"origin/main","commits":[{}]}}"#,
        commits.join(",")
    );
    assert!(landing(&many).unwrap_err().contains("more than 20"));
    // A URL that does not fit is dropped, never shown.
    let dropped = landing(
        r#"{"status":"landed","ref":"origin/main","commits":["4f2a9c1"],"url":"http://x y"}"#,
    )
    .unwrap()
    .unwrap();
    assert_eq!(dropped.url, None);
    // No block at all means nothing was reported; a cut one is malformed.
    assert_eq!(parse_landing("Done, nothing landed.", false), Ok(None));
    assert!(parse_landing("Done.", true).is_err());
    assert!(
        parse_landing(&format!("```{LANDING_BLOCK}\n{{\"status\""), false)
            .unwrap_err()
            .contains("cut off")
    );
    // The last block wins.
    let two = format!(
        "{}{}",
        reply(LANDING_BLOCK, r#"{"status":"failed"}"#),
        reply(LANDING_BLOCK, r#"{"status":"not_landed"}"#)
    );
    assert_eq!(
        parse_landing(&two, false).unwrap().unwrap().status,
        ReportStatus::NotLanded
    );
}

#[test]
fn landing_checks_need_evidence_to_confirm_and_a_note_to_give_up() {
    let check = |body: &str| parse_check(&reply(CHECK_BLOCK, body), false);
    let confirmed = check(
        r#"{"status":"confirmed","ref":"origin/main","commits":["4f2a9c1"],"evidence":["diff equals 1b2c3d4..7e8f9a0"]}"#,
    )
    .unwrap();
    assert_eq!(confirmed.status, CheckStatus::Confirmed);
    assert!(
        check(r#"{"status":"confirmed","ref":"origin/main","commits":["4f2a9c1"]}"#)
            .unwrap_err()
            .contains("needs evidence")
    );
    assert!(
        check(r#"{"status":"confirmed","evidence":["e"]}"#)
            .unwrap_err()
            .contains("names its ref and commits")
    );
    assert!(
        check(r#"{"status":"unverifiable"}"#)
            .unwrap_err()
            .contains("needs a note")
    );
    assert_eq!(
        check(r#"{"status":"mismatch","ref":"origin/main"}"#)
            .unwrap()
            .status,
        CheckStatus::Mismatch
    );
    assert!(check(r#"{"status":"maybe"}"#).is_err());
    assert!(parse_check("no block here", false).is_err());
}

#[test]
fn commits_match_by_prefix() {
    assert!(is_commit("4f2a9c1"));
    assert!(!is_commit("4f2a9c"));
    assert!(!is_commit(&"a".repeat(41)));
    assert!(same_commit("4f2a9c1e0b7d", "4f2a9c1"));
    assert!(same_commit("4f2a9c1", "4f2a9c1e0b7d"));
    assert!(!same_commit("4f2a9c1", "4f2a9c2"));
}
