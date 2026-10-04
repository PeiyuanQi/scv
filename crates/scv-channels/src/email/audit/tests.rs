//! Unit tests for `src/email/audit.rs`.

use super::*;

fn line(at: u64, id: &str) -> Line {
    Line {
        at,
        id: id.into(),
        kind: "draft".into(),
        event: "proposed".into(),
        generation: 1,
        digest: "01234567".into(),
        detail: None,
    }
}

#[test]
fn lines_are_appended_privately_and_pruned_by_age_then_size() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("audit.jsonl");
    append(&path, &[line(10, "a1"), line(20, "a2")]).unwrap();
    append(&path, &[line(30, "a3")]).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 3);
    assert_eq!(prune(&path, 35, 20, usize::MAX).unwrap(), 1);
    let kept = std::fs::read_to_string(&path).unwrap();
    assert!(!kept.contains("\"a1\"") && kept.contains("\"a3\""));
    let one = kept.lines().last().unwrap().len() + 1;
    assert_eq!(prune(&path, 35, 20, one).unwrap(), 1);
    assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
    assert_eq!(prune(&home.path().join("missing"), 0, 1, 1).unwrap(), 0);
}

#[test]
fn a_pending_journal_is_appended_only_for_the_state_it_names() {
    let home = tempfile::tempdir().unwrap();
    let audit = home.path().join("audit.jsonl");
    let state = home.path().join("state.json");
    let text = "{\"v\":1}";
    std::fs::write(&state, text).unwrap();
    let lines = vec![line(10, "a1")];
    stage(&audit, text, &lines).unwrap();
    assert!(pending_path(&audit).is_file());
    reconcile(&audit, &state).unwrap();
    assert_eq!(std::fs::read_to_string(&audit).unwrap().lines().count(), 1);
    assert!(!pending_path(&audit).exists());
    // The same lines are already the tail, so folding again does not repeat them.
    stage(&audit, text, &lines).unwrap();
    reconcile(&audit, &state).unwrap();
    assert_eq!(std::fs::read_to_string(&audit).unwrap().lines().count(), 1);
    assert!(!pending_path(&audit).exists());

    stage(&audit, "other-state", &[line(11, "a2")]).unwrap();
    reconcile(&audit, &state).unwrap();
    let got = std::fs::read_to_string(&audit).unwrap();
    assert!(!got.contains("\"a2\""), "{got}");
    assert!(!pending_path(&audit).exists());

    std::fs::write(pending_path(&audit), b"{not json").unwrap();
    assert!(reconcile(&audit, &state).is_err());
    assert!(
        pending_path(&audit).is_file(),
        "an unreadable journal is kept"
    );
}

#[test]
fn a_multi_line_commit_replaces_the_log_whole_and_replays_once() {
    let home = tempfile::tempdir().unwrap();
    let audit = home.path().join("audit.jsonl");
    let state = home.path().join("state.json");
    append(&audit, &[line(1, "old")]).unwrap();
    let mut old = std::fs::File::open(&audit).unwrap();
    let lines = [line(2, "first"), line(2, "second")];
    stage(&audit, "committed", &lines).unwrap();
    std::fs::write(&state, "committed").unwrap();
    // Simulate stopping after the audit replacement but before journal removal.
    append(&audit, &lines).unwrap();
    let mut old_text = String::new();
    old.read_to_string(&mut old_text).unwrap();
    assert_eq!(
        old_text.lines().count(),
        1,
        "an existing reader sees the old complete file"
    );
    reconcile(&audit, &state).unwrap();
    assert_eq!(std::fs::read_to_string(&audit).unwrap().lines().count(), 3);
    assert!(!pending_path(&audit).exists());
}
