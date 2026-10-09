//! Unit tests for `src/delegate/review/journal.rs`.

use super::*;
use std::os::unix::fs::MetadataExt as _;

#[test]
fn events_are_numbered_lines_in_a_private_file() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("state/reviews");
    let mut journal = Journal::create(&dir).unwrap();
    assert!(journal.id().starts_with("rev-"), "{}", journal.id());
    assert!(!journal.is_started());
    journal
        .append(
            "review.started",
            serde_json::json!({"job":"job-1","seq":99}),
        )
        .unwrap();
    journal
        .append("review.finished", serde_json::json!({"outcome":"approved"}))
        .unwrap();
    assert!(journal.is_finished());
    let path = dir.join(format!("{}.jsonl", journal.id()));
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["v"], 1);
    // The event's own fields never replace the envelope's.
    assert_eq!(lines[0]["seq"], 1);
    assert_eq!(lines[0]["event"], "review.started");
    assert_eq!(lines[0]["job"], "job-1");
    assert_eq!(lines[1]["seq"], 2);
    assert!(lines[1]["ts"].as_u64().unwrap() > 0);
    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
}

#[test]
fn a_symlinked_directory_is_refused_and_an_empty_journal_can_go() {
    let home = tempfile::tempdir().unwrap();
    let elsewhere = home.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let dir = home.path().join("reviews");
    std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();
    assert!(Journal::create(&dir).is_err());
    let dir = home.path().join("real");
    let journal = Journal::create(&dir).unwrap();
    let path = dir.join(format!("{}.jsonl", journal.id()));
    assert!(path.exists());
    journal.remove_empty();
    assert!(!path.exists());
}

#[test]
fn old_journals_are_pruned_but_nothing_else() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path();
    for name in ["rev-1-aaaaaa.jsonl", "rev-2-bbbbbb.jsonl", "notes.txt"] {
        std::fs::write(dir.join(name), "{}\n").unwrap();
    }
    let target = home.path().join("target.jsonl");
    std::fs::write(&target, "{}\n").unwrap();
    std::os::unix::fs::symlink(&target, dir.join("rev-3-cccccc.jsonl")).unwrap();
    // A month from now, everything written now is old.
    let later = SystemTime::now() + RETENTION + Duration::from_secs(60);
    prune(dir, RETENTION, later);
    let mut left: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left, ["notes.txt", "rev-3-cccccc.jsonl", "target.jsonl"]);
    // Recent ones stay.
    std::fs::write(dir.join("rev-4-dddddd.jsonl"), "{}\n").unwrap();
    prune(dir, RETENTION, SystemTime::now());
    assert!(dir.join("rev-4-dddddd.jsonl").exists());
}
