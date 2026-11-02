//! Unit tests for `src/publish.rs`.

use serde_json::json;

use super::*;

fn pending(name: &str, recent: Result<u64, &str>) -> Pending {
    Pending {
        name: name.into(),
        recent: recent.map_err(str::to_owned),
    }
}

#[test]
fn a_release_within_the_limit_goes_without_a_question() {
    let crates = [pending("scv-core", Ok(0)), pending("scv-cli", Ok(19))];
    assert_eq!(decide(20, "0.4.8", &crates), Verdict::Go);
    assert_eq!(decide(20, "0.4.8", &[]), Verdict::Go);
}

#[test]
fn one_crate_over_the_line_asks_for_the_whole_release() {
    let crates = [pending("scv-core", Ok(3)), pending("scv-cli", Ok(5))];
    let Verdict::Ask(question) = decide(5, "0.4.8", &crates) else {
        panic!("over the line must ask");
    };
    assert!(
        question.contains("0.4.8 of scv-core, scv-cli"),
        "{question}"
    );
    assert!(
        question.contains("- scv-cli: 5 in the last 24 hours"),
        "{question}"
    );
    assert!(!question.contains("- scv-core"), "{question}");
    assert!(question.ends_with("Publish anyway?"), "{question}");
    // A limit of 0 asks before every release.
    assert!(matches!(
        decide(0, "0.4.8", &[pending("scv-core", Ok(0))]),
        Verdict::Ask(_)
    ));
}

#[test]
fn a_crate_at_crates_ios_own_limit_blocks_the_release_unasked() {
    // crates.io would refuse scv-cli after scv-core was published, whatever
    // the owner answered, so no limit asks or goes.
    let crates = [pending("scv-core", Ok(0)), pending("scv-cli", Ok(20))];
    for limit in [0, 5, 20, 21, u64::MAX] {
        let Verdict::Blocked(reason) = decide(limit, "0.4.8", &crates) else {
            panic!("a crate at crates.io's limit must block under {limit}");
        };
        assert!(
            reason.contains("0.4.8 of scv-core, scv-cli is blocked"),
            "{reason}"
        );
        assert!(
            reason.contains("- scv-cli: 20 in the last 24 hours"),
            "{reason}"
        );
        assert!(!reason.contains("- scv-core"), "{reason}");
    }
    // Also beside a count that cannot be fetched, which alone would ask.
    let crates = [
        pending("scv-core", Err("HTTP 503")),
        pending("scv-cli", Ok(31)),
    ];
    assert!(matches!(decide(20, "0.4.8", &crates), Verdict::Blocked(_)));
    // Above crates.io's limit, the configured line acts as 20.
    assert_eq!(
        decide(21, "0.4.8", &[pending("scv-cli", Ok(19))]),
        Verdict::Go
    );
}

#[test]
fn a_count_that_cannot_be_fetched_counts_as_over() {
    let crates = [pending("scv-core", Err("HTTP 503"))];
    let Verdict::Ask(question) = decide(20, "0.4.8", &crates) else {
        panic!("an unknown count must ask");
    };
    assert!(
        question.contains("could not be read from crates.io (HTTP 503)"),
        "{question}"
    );
}

#[test]
fn versions_count_by_their_creation_in_the_last_day_yanked_included() {
    // 2026-10-30T12:00:00Z.
    let now: u64 = 1_793_361_600;
    let since = now - WINDOW_SECONDS;
    let body = json!({
        "versions": [
            {"num": "0.4.9", "created_at": "2026-10-30T11:59:59.123456+00:00", "yanked": false},
            {"num": "0.4.8", "created_at": "2026-10-29T12:00:00Z", "yanked": true},
            {"num": "0.4.7", "created_at": "2026-10-30T02:00:00+08:00", "yanked": false},
            {"num": "0.4.6", "created_at": "2026-10-29T11:59:59Z", "yanked": false}
        ],
        "meta": {"total": 40, "next_page": "?page=2&per_page=100"}
    });
    let page = read_versions_page(&body, since).unwrap();
    // 0.4.7 is 2026-10-29T18:00:00Z, inside the day; 0.4.6 is one second out.
    assert_eq!(page.recent, 3);
    assert!(page.older);
    assert_eq!(page.next_page.as_deref(), Some("?page=2&per_page=100"));
    // Anything unreadable leaves the count unknown.
    assert!(read_versions_page(&json!({"errors": []}), since).is_err());
    let bad = json!({"versions": [{"num": "1.0.0", "created_at": "yesterday"}]});
    assert!(read_versions_page(&bad, since).is_err());
}

#[test]
fn rfc3339_times_read_as_unix_seconds() {
    assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(unix_seconds("2000-03-01T00:00:00Z"), Some(951_868_800));
    assert_eq!(unix_seconds("2026-10-30T12:00:00.5Z"), Some(1_793_361_600));
    assert_eq!(
        unix_seconds("2026-10-30T13:30:00+01:30"),
        Some(1_793_361_600)
    );
    assert_eq!(unix_seconds("2026-10-30T12:00:00"), None);
    assert_eq!(unix_seconds("2026-13-30T12:00:00Z"), None);
}
