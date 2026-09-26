//! Unit tests for `src/delegate/conversation.rs`.

use super::*;

fn store(max: usize, idle: Duration, markers: Option<&Path>) -> Arc<ConversationStore> {
    Arc::new(ConversationStore::new(
        ConversationLimits { max, idle },
        markers.map(Path::to_path_buf),
    ))
}

const DAY: Duration = Duration::from_secs(86400);

#[test]
fn handles_are_issued_per_agent_and_vendor_ids_are_not_handles() {
    assert!(is_handle("codex-2"));
    assert!(is_handle("pi-10"));
    for not_handle in [
        "01a0cd5a-7195-7b31-a503-e235d5da7b45",
        "b514bbf5-a5b7-4bbe-83f9-5824ab41c35c",
        "codex",
        "codex-",
        "-1",
        "Codex-1",
        "codex-1a",
        "../codex-1",
    ] {
        assert!(!is_handle(not_handle), "{not_handle}");
        assert_eq!(handle_agent(not_handle), None, "{not_handle}");
    }
    // A handle names its agent.
    assert_eq!(handle_agent("codex-2"), Some("codex"));
    assert_eq!(handle_agent("pi-10"), Some("pi"));
    let store = store(8, DAY, None);
    let cwd = Path::new("/w");
    let first = store.begin("codex", None, cwd, false).unwrap();
    assert_eq!((first.handle.as_str(), first.turn), ("codex-1", 1));
    assert_eq!(first.vendor, None);
    assert_eq!(
        first.finish(Some("t-1".into()), true).as_deref(),
        Some("codex-1")
    );
    let claude = store.begin("claude", None, cwd, true).unwrap();
    assert_eq!(claude.handle, "claude-1");
    assert!(claude.vendor.is_some());
    claude.finish(None, true);
    let second = store.begin("codex", None, cwd, false).unwrap();
    assert_eq!(second.handle, "codex-2");
}

#[test]
fn continuing_pins_agent_and_cwd_and_counts_turns() {
    let store = store(8, DAY, None);
    let cwd = Path::new("/w/scv");
    let turn = store.begin("codex", None, cwd, false).unwrap();
    turn.finish(Some("thread-a".into()), true);
    let next = store.begin("codex", Some("codex-1"), cwd, false).unwrap();
    assert_eq!(next.turn, 2);
    assert_eq!(next.vendor.as_deref(), Some("thread-a"));
    // One turn at a time.
    let busy = store
        .begin("codex", Some("codex-1"), cwd, false)
        .unwrap_err();
    assert!(busy.message.starts_with("session busy"), "{}", busy.message);
    next.finish(Some("thread-a".into()), true);
    let moved = store
        .begin("codex", Some("codex-1"), Path::new("/w/other"), false)
        .unwrap_err();
    assert!(moved.message.contains("runs in"), "{}", moved.message);
    let other_agent = store
        .begin("claude", Some("codex-1"), cwd, false)
        .unwrap_err();
    assert!(
        other_agent.message.contains("belongs to codex, not claude"),
        "{}",
        other_agent.message
    );
    let third = store.begin("codex", Some("codex-1"), cwd, false).unwrap();
    assert_eq!(third.turn, 3);
}

#[test]
fn vendor_ids_and_unknown_handles_are_rejected() {
    let store = store(8, DAY, None);
    let cwd = Path::new("/w");
    store
        .begin("codex", None, cwd, false)
        .unwrap()
        .finish(Some("01a0cd5a-7195-7b31".into()), true);
    let vendor = store
        .begin("codex", Some("01a0cd5a-7195-7b31"), cwd, false)
        .unwrap_err();
    assert!(
        vendor.message.contains("not a conversation handle"),
        "{}",
        vendor.message
    );
    let unknown = store
        .begin("codex", Some("codex-9"), cwd, false)
        .unwrap_err();
    assert!(
        unknown.message.contains("unknown in this session"),
        "{}",
        unknown.message
    );
    // Another session's store knows nothing of this one's handles.
    let other = super::tests::store(8, DAY, None);
    assert!(other.begin("codex", Some("codex-1"), cwd, false).is_err());
}

#[test]
fn timed_out_turns_stay_resumable_but_failed_first_turns_are_forgotten() {
    let store = store(8, DAY, None);
    let cwd = Path::new("/w");
    // A first turn that timed out after the CLI reported its session.
    let turn = store.begin("codex", None, cwd, false).unwrap();
    assert_eq!(
        turn.finish(Some("t".into()), false).as_deref(),
        Some("codex-1")
    );
    assert_eq!(
        store
            .begin("codex", Some("codex-1"), cwd, false)
            .unwrap()
            .turn,
        2
    );
    // A first turn that failed before the CLI reported anything.
    let failed = store.begin("codex", None, cwd, false).unwrap();
    assert_eq!(failed.finish(None, false), None);
    assert!(!store.handles().contains(&"codex-2".to_owned()));
    // An abandoned first turn is forgotten; an abandoned later turn frees the conversation.
    drop(store.begin("codex", None, cwd, false).unwrap());
    assert_eq!(store.handles(), vec!["codex-1".to_owned()]);
}

#[test]
fn limits_forget_the_least_recently_used_and_idle_conversations() {
    let store = store(2, DAY, None);
    let cwd = Path::new("/w");
    for id in ["a", "b"] {
        store
            .begin("codex", None, cwd, false)
            .unwrap()
            .finish(Some(id.into()), true);
    }
    // codex-1 was used more recently than codex-2.
    store
        .begin("codex", Some("codex-1"), cwd, false)
        .unwrap()
        .finish(Some("a".into()), true);
    store
        .begin("codex", None, cwd, false)
        .unwrap()
        .finish(Some("c".into()), true);
    assert_eq!(
        store.handles(),
        vec!["codex-1".to_owned(), "codex-3".to_owned()]
    );
    // Every remembered conversation busy: no room for another.
    let busy_a = store.begin("codex", Some("codex-1"), cwd, false).unwrap();
    let busy_b = store.begin("codex", Some("codex-3"), cwd, false).unwrap();
    assert!(store.begin("codex", None, cwd, false).is_err());
    drop((busy_a, busy_b));

    let idle = super::tests::store(8, Duration::ZERO, None);
    idle.begin("pi", None, cwd, true)
        .unwrap()
        .finish(None, true);
    let expired = idle.begin("pi", Some("pi-1"), cwd, true).unwrap_err();
    assert!(
        expired.message.contains("forgotten after 0 seconds idle"),
        "{}",
        expired.message
    );
}

#[test]
fn markers_follow_the_conversation_and_gc_keeps_live_transcripts() {
    let home = tempfile::tempdir().unwrap();
    let markers = home.path().join("state/conversations");
    let adapter = home.path().join("agents/codex");
    let day = adapter.join("sessions/2026/01/02");
    std::fs::create_dir_all(&day).unwrap();
    let old = SystemTime::now() - Duration::from_secs(10 * 86400);
    let transcript = |id: &str| {
        let path = day.join(format!("rollout-2026-01-02T00-00-00-{id}.jsonl"));
        std::fs::write(&path, "{}\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        path
    };
    let live = transcript("live-id");
    let stale = transcript("stale-id");
    let recent = day.join("rollout-recent-id.jsonl");
    std::fs::write(&recent, "{}\n").unwrap();
    // A link out of the tree is never followed or removed.
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::os::unix::fs::symlink(outside.path(), day.join("link.jsonl")).unwrap();

    let store = store(8, DAY, Some(&markers));
    store
        .begin("codex", None, Path::new("/w"), false)
        .unwrap()
        .finish(Some("live-id".into()), true);
    assert!(markers.join("live-id.json").is_file());
    // A marker left by a process that no longer runs does not protect anything.
    write_private_json(
        &markers,
        "stale-id.json",
        &Marker {
            owner: ProcessIdentity {
                pid: u32::MAX - 1,
                start_time: 1,
            },
            agent: "codex".into(),
            handle: "codex-9".into(),
        },
    )
    .unwrap();
    let files = ConversationFiles {
        dir: "sessions",
        extension: "jsonl",
    };
    let dry = collect_garbage(&adapter, files, &markers, DAY, true).unwrap();
    assert_eq!(dry.removed, vec![stale.clone()]);
    assert_eq!(dry.kept_live, 1);
    assert!(stale.exists() && markers.join("stale-id.json").exists());
    let report = collect_garbage(&adapter, files, &markers, DAY, false).unwrap();
    assert_eq!(report.removed, vec![stale.clone()]);
    assert!(!stale.exists() && live.exists() && recent.exists());
    assert!(outside.path().exists());
    assert!(!markers.join("stale-id.json").exists());
    // Ending the session releases its transcripts.
    drop(store);
    assert!(!markers.join("live-id.json").exists());
    // Never younger than the minimum age, whatever was asked.
    let report = collect_garbage(&adapter, files, &markers, Duration::ZERO, false).unwrap();
    assert_eq!(report.removed, vec![live]);
    assert!(recent.exists());
}

#[test]
fn ages_parse_with_units() {
    assert_eq!(parse_age("30d"), Ok(Duration::from_secs(30 * 86400)));
    assert_eq!(parse_age("12h"), Ok(Duration::from_secs(12 * 3600)));
    assert_eq!(parse_age("90m"), Ok(Duration::from_secs(5400)));
    assert_eq!(parse_age("45"), Ok(Duration::from_secs(45)));
    assert!(parse_age("3w").is_err());
    assert!(parse_age("d").is_err());
    assert!(parse_age("-1d").is_err());
}
