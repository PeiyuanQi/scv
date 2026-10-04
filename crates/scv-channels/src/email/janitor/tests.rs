//! Unit tests for `src/email/janitor.rs`.

use super::*;
use std::os::unix::fs::symlink;
use std::time::Duration;

#[test]
fn stale_temporaries_go_and_everything_else_stays() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    std::fs::write(path.join(".tmpOLD"), "mail text").unwrap();
    std::fs::write(path.join(".state.json.a1B2c3.tmp"), "mail text").unwrap();
    std::fs::write(path.join("default.json"), "{}").unwrap();
    std::fs::write(path.join(".hidden"), "x").unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("keep"), "x").unwrap();
    symlink(outside.path().join("keep"), path.join(".tmpLINK")).unwrap();
    std::fs::create_dir(path.join(".tmpDIR")).unwrap();
    let old = SystemTime::now() + Duration::from_secs(2 * 3600);
    // Seen from two hours ahead, every file is old; only regular temporary
    // files go.
    assert_eq!(remove_stale_temporaries(path, None, old), 2);
    assert!(!path.join(".tmpOLD").exists());
    assert!(!path.join(".state.json.a1B2c3.tmp").exists());
    assert!(path.join("default.json").exists() && path.join(".hidden").exists());
    assert!(path.join(".tmpLINK").symlink_metadata().is_ok());
    assert!(outside.path().join("keep").exists());
    assert!(path.join(".tmpDIR").exists());
    std::fs::write(path.join(".tmpFRESH"), "x").unwrap();
    assert_eq!(remove_stale_temporaries(path, None, SystemTime::now()), 0);
    assert!(path.join(".tmpFRESH").exists());
}

#[test]
fn in_the_shared_directory_only_the_accounts_own_temporaries_go() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    // What an interrupted write of each account's state leaves.
    let leftover = |account: &str| {
        let target = path.join(format!("{account}.json"));
        let mut name = scv_client::fs::temporary_prefix(&target);
        name.push("a1B2c3.tmp");
        let leftover = path.join(name);
        std::fs::write(&leftover, "mail text").unwrap();
        leftover
    };
    let work = leftover("work");
    let other = leftover("work2");
    let home = leftover("home");
    std::fs::write(path.join(".tmpUNKNOWN"), "x").unwrap();
    let old = SystemTime::now() + Duration::from_secs(2 * 3600);
    let own = own_temporaries(path, "work");
    assert_eq!(remove_stale_temporaries(path, Some(&own), old), 1);
    assert!(!work.exists());
    assert!(other.exists() && home.exists(), "another account's stay");
    assert!(path.join(".tmpUNKNOWN").exists(), "nobody's is left alone");
}

#[test]
fn the_empty_directory_is_emptied_without_following_links() {
    let directory = tempfile::tempdir().unwrap();
    let empty = directory.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("precious"), "x").unwrap();
    std::fs::write(empty.join("dropped"), "x").unwrap();
    std::fs::create_dir_all(empty.join("dir/inner")).unwrap();
    symlink(outside.path(), empty.join("link")).unwrap();
    assert_eq!(empty_directory(&empty), 3);
    assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0);
    assert!(outside.path().join("precious").exists());
    assert_eq!(empty_directory(&directory.path().join("missing")), 0);
}

#[tokio::test]
async fn a_sweep_prunes_and_flags_a_full_disk_once() {
    use crate::email::test_support::{FixedClock, ledger};
    let home = tempfile::tempdir().unwrap();
    let ledger = ledger(home.path());
    let settings = crate::email::MailSettings::parse(Some(
        &"[notify]\nroute = [\"feishu:mail\"]\n".parse().unwrap(),
    ))
    .unwrap();
    let clock = FixedClock::new();
    let low = LowSpace::default();
    let private = home.path().join("state/mail/default");
    std::fs::create_dir_all(private.join("empty")).unwrap();
    std::fs::write(private.join("empty/stray"), "x").unwrap();
    let janitor = Janitor {
        ledger: &ledger,
        settings: &settings,
        clock: &clock,
        low_space: &low,
        private: &private,
        state_dir: &home.path().join("state/channels/email"),
        account: "default",
        free_space: free_bytes,
        registration: None,
    };
    janitor.sweep().await.unwrap();
    assert!(!low.is_low());
    assert!(!private.join("empty/stray").exists());
    // No disk has this much free: the floor is crossed.
    let mut full = settings.clone();
    full.retention.min_free_mib = u64::MAX / (1024 * 1024);
    let janitor = Janitor {
        settings: &full,
        ..janitor
    };
    janitor.sweep().await.unwrap();
    janitor.sweep().await.unwrap();
    assert!(low.is_low());
    let state = ledger.snapshot();
    let notes: Vec<_> = state
        .queue
        .iter()
        .filter(|item| item.key.starts_with("system:disk:"))
        .collect();
    assert_eq!(notes.len(), 1);
}

#[tokio::test]
async fn free_space_that_cannot_be_told_counts_as_too_little() {
    use crate::email::test_support::{FixedClock, ledger};
    let home = tempfile::tempdir().unwrap();
    let ledger = ledger(home.path());
    let settings = crate::email::MailSettings::parse(Some(
        &"[notify]\nroute = [\"feishu:mail\"]\n".parse().unwrap(),
    ))
    .unwrap();
    let clock = FixedClock::new();
    let low = LowSpace::default();
    let private = home.path().join("state/mail/default");
    let janitor = Janitor {
        ledger: &ledger,
        settings: &settings,
        clock: &clock,
        low_space: &low,
        private: &private,
        state_dir: &home.path().join("state/channels/email"),
        account: "default",
        free_space: |_| None,
        registration: None,
    };
    janitor.sweep().await.unwrap();
    assert!(low.is_low(), "new mail is counted, not reported");
    let state = ledger.snapshot();
    let note = state
        .queue
        .iter()
        .find(|item| item.key.starts_with("system:disk:"))
        .unwrap();
    assert!(note.text.contains("cannot tell"), "{}", note.text);
    // Once it can be told again, and there is room, mail is reported.
    let janitor = Janitor {
        free_space: |_| Some(u64::MAX),
        ..janitor
    };
    janitor.sweep().await.unwrap();
    assert!(!low.is_low());
}
