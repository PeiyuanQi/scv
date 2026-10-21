//! Unit tests for `src/session/persist.rs`.

use super::*;

fn queue(session: &str, owner: ProcessIdentity) -> SavedQueue {
    SavedQueue {
        v: 1,
        session: session.into(),
        label: "TUI".into(),
        workspace: PathBuf::from("/w"),
        owner,
        saved_unix: unix_now(),
        paused: false,
        entries: vec![QueueEntry {
            queue_id: "q1".into(),
            revision: 1,
            prompt: "run the tests".into(),
            submitter: "r1".into(),
            attachments: Vec::new(),
        }],
        active: Some("the long build".into()),
    }
}

fn dead() -> ProcessIdentity {
    // The PID of a process that has exited (this one's child).
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let identity = ProcessIdentity::of(child.id()).unwrap();
    child.wait().unwrap();
    identity
}

#[test]
fn a_saved_queue_reads_back_and_an_empty_one_is_removed() {
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::new(home.path());
    let me = ProcessIdentity::current().unwrap();
    let saved = queue("3f2a-session", me);
    save(&layout, &saved);
    assert_eq!(load(&layout, "3f2a-session"), Some(saved.clone()));
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(layout.session_queues().join("3f2a-session.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    // A session still running here is not restorable.
    assert!(!stopped(&saved));
    assert_eq!(restorable(&layout), []);
    let empty = SavedQueue {
        entries: Vec::new(),
        active: None,
        ..saved
    };
    save(&layout, &empty);
    assert_eq!(load(&layout, "3f2a-session"), None);
}

#[test]
fn only_stopped_sessions_are_restorable_and_old_ones_go() {
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::new(home.path());
    let gone = dead();
    save(&layout, &queue("stopped-1", gone));
    let mut old = queue("stopped-2", gone);
    old.saved_unix = 1;
    save(&layout, &old);
    save(
        &layout,
        &queue("running", ProcessIdentity::current().unwrap()),
    );
    let found = restorable(&layout);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, "stopped-1");
    assert_eq!(found[0].queued, 1);
    assert_eq!(found[0].interrupted.as_deref(), Some("the long build"));
    assert!(load(&layout, "stopped-2").is_none());
    remove(&layout, "stopped-1");
    assert_eq!(restorable(&layout), []);
}

#[test]
fn session_ids_cannot_name_other_files() {
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::new(home.path());
    for bad in ["", "../x", "a/b", "a.json"] {
        assert!(!valid_session(bad), "{bad}");
        assert!(load(&layout, bad).is_none());
    }
    save(
        &layout,
        &queue("../escape", ProcessIdentity::current().unwrap()),
    );
    assert!(!home.path().join("state/escape.json").exists());
    assert!(valid_session("3f2a9c1e-0b7d-4e5f-8a6b-1234567890ab"));
    assert_eq!(active_line(&"x".repeat(400)).chars().count(), 301);
}
