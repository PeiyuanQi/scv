//! Unit tests for `src/delegate/records.rs`.

use super::*;
use std::os::unix::{fs::PermissionsExt as _, process::CommandExt as _};

fn registry(home: &Path) -> Arc<DelegationRegistry> {
    Arc::new(DelegationRegistry::new(home))
}

/// Spawn `sh -c script` in its own process group with `environment`.
fn spawn_tagged(script: &str, environment: &[(OsString, OsString)]) -> std::process::Child {
    std::process::Command::new("sh")
        .args(["-c", script])
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap()
}

async fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

#[test]
fn chains_match_only_their_own_handle() {
    assert!(chain_names("abcd/s1/codex-1a2b3c", "codex-1a2b3c"));
    assert!(chain_names(
        "x/s/claude-000000;abcd/s1/codex-1a2b3c",
        "codex-1a2b3c"
    ));
    assert!(!chain_names("abcd/s1/codex-1a2b3c", "codex-1a2b3"));
    assert!(!chain_names("abcd/s1/codex-1a2b3c", "1a2b3c"));
}

#[test]
fn a_declared_client_depth_raises_the_recorded_depth() {
    let home = tempfile::tempdir().unwrap();
    let registry = DelegationRegistry::new(home.path());
    let pending = registry.begin_at(2, "codex", "session", home.path(), None);
    assert_eq!(pending.depth, 3);
    assert!(
        pending
            .environment
            .iter()
            .any(|(name, value)| { name == DEPTH_VARIABLE && value == "3" })
    );
}

#[test]
fn nested_tags_extend_the_chain_and_depth() {
    let home = tempfile::tempdir().unwrap();
    let mut registry = DelegationRegistry::new(home.path());
    registry.chain = Some("aaaa/s0/codex-111111".into());
    registry.depth = 1;
    let pending = registry.begin("claude", "s1", home.path(), Some(("claude-1", 2)));
    let value = |name: &str| {
        pending
            .environment
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.to_str().unwrap().to_owned())
            .unwrap()
    };
    assert_eq!(
        value(PARENT_VARIABLE),
        format!(
            "aaaa/s0/codex-111111;{}/s1/{}",
            registry.instance, pending.handle
        )
    );
    assert_eq!(value(DEPTH_VARIABLE), "2");
    assert!(pending.handle.starts_with("claude-"));
}

#[tokio::test]
async fn records_are_private_and_removed_when_the_run_finishes() {
    let home = tempfile::tempdir().unwrap();
    let registry = registry(home.path());
    let pending = registry.begin("codex", "session", home.path(), None);
    let environment = pending.environment.clone();
    let mut child = spawn_tagged("sleep 30", &environment);
    let guard = registry.register(pending, child.id()).unwrap();
    let path = registry
        .record_dir()
        .join(format!("{}.json", guard.handle()));
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(registry.record_dir()), 0o700);
    assert_eq!(mode(registry.record_dir().parent().unwrap()), 0o700);

    let listed = registry.list(false);
    assert_eq!(listed.len(), 1);
    assert!(!listed[0].orphaned);
    assert_eq!(listed[0].record.process.pid, child.id());
    assert!(listed[0].processes >= 1);

    crate::process::ProcessGroup::new(child.id())
        .expect("child process group")
        .signal(libc::SIGKILL);
    child.wait().unwrap();
    guard.finish().await;
    assert!(!path.exists());
    assert!(registry.list(true).is_empty());
}

#[tokio::test]
async fn kill_stops_a_local_run_and_marks_it_killed() {
    let home = tempfile::tempdir().unwrap();
    let registry = registry(home.path());
    let pending = registry.begin("claude", "session", home.path(), None);
    let environment = pending.environment.clone();
    let mut child = spawn_tagged("trap '' TERM; sleep 30", &environment);
    let guard = registry.register(pending, child.id()).unwrap();
    registry.kill(guard.handle()).await.unwrap();
    assert!(guard.was_killed());
    assert!(child.wait().unwrap().code().is_none(), "killed by a signal");
    assert!(registry.kill("claude-nosuch").await.is_err());
    guard.finish().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn reconcile_removes_conversation_markers_of_exited_processes() {
    let home = tempfile::tempdir().unwrap();
    let daemon = registry(home.path());
    let markers = daemon.conversation_dir();
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    let gone_pid = gone.id();
    gone.wait().unwrap();
    let dead = ProcessIdentity {
        pid: gone_pid,
        start_time: 1,
    };
    let live = ProcessIdentity::current().unwrap();
    for (id, owner) in [("dead-id", dead), ("live-id", live)] {
        let marker = serde_json::json!({"owner": owner, "agent": "codex", "handle": "codex-1"});
        write_private_json(&markers, &format!("{id}.json"), &marker).unwrap();
    }
    let report = daemon.reconcile().await;
    assert_eq!(report.stale_markers, 1);
    assert!(!markers.join("dead-id.json").exists());
    assert!(markers.join("live-id.json").is_file());
    assert_eq!(daemon.reconcile().await, ReconcileReport::default());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn reconcile_reaps_an_orphan_and_its_detached_descendants() {
    let home = tempfile::tempdir().unwrap();
    let owner = registry(home.path());
    let pending = owner.begin("codex", "session", home.path(), None);
    let environment = pending.environment.clone();
    // The agent starts a detached descendant in a new session, outside its group.
    let mut child = spawn_tagged("setsid sleep 60 & exec sleep 60", &environment);
    let guard = owner.register(pending, child.id()).unwrap();
    let handle = guard.handle().to_owned();
    assert!(wait_for(|| tagged_processes(&handle).len() >= 2).await);
    let path = owner.record_dir().join(format!("{handle}.json"));
    // Rewrite the record as if a process that has since died owned it.
    let mut record = read_record(&path).unwrap();
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    let gone_pid = gone.id();
    gone.wait().unwrap();
    record.owner = ProcessIdentity {
        pid: gone_pid,
        start_time: 1,
    };
    write_record(owner.record_dir(), &record).unwrap();
    std::mem::forget(guard);

    // Another SCV process of the same instance reconciles.
    let daemon = registry(home.path());
    assert!(daemon.list(false).is_empty());
    let orphans = daemon.list(true);
    assert_eq!(orphans.len(), 1);
    assert!(orphans[0].orphaned);
    assert!(orphans[0].processes >= 2);
    let report = daemon.reconcile().await;
    assert_eq!(report.reaped, vec![handle.clone()]);
    assert_eq!(daemon.reaped_total(), 1);
    assert!(child.wait().unwrap().code().is_none());
    assert!(wait_for(|| tagged_processes(&handle).is_empty()).await);
    assert!(!path.exists());
    assert_eq!(daemon.reconcile().await, ReconcileReport::default());
}

#[tokio::test]
async fn an_abandoned_run_is_cleaned_up_when_its_guard_drops() {
    let home = tempfile::tempdir().unwrap();
    let registry = registry(home.path());
    let pending = registry.begin("pi", "session", home.path(), None);
    let environment = pending.environment.clone();
    let mut child = spawn_tagged("sleep 30", &environment);
    let guard = registry.register(pending, child.id()).unwrap();
    let path = registry
        .record_dir()
        .join(format!("{}.json", guard.handle()));
    drop(guard);
    assert!(child.wait().unwrap().code().is_none());
    assert!(!path.exists());
}

#[test]
fn records_for_another_instance_or_under_the_wrong_name_are_ignored() {
    let home = tempfile::tempdir().unwrap();
    let registry = DelegationRegistry::new(home.path());
    let dir = registry.record_dir().to_owned();
    let record = DelegationRecord {
        handle: "codex-abcdef".into(),
        agent: "codex".into(),
        instance: "other".into(),
        session: "s".into(),
        owner: ProcessIdentity {
            pid: 1,
            start_time: 1,
        },
        process: ProcessIdentity {
            pid: 1,
            start_time: 1,
        },
        pgid: 1,
        cwd: "/".into(),
        started_unix: 0,
        depth: 1,
        conversation: None,
        turn: None,
    };
    write_record(&dir, &record).unwrap();
    std::fs::copy(
        dir.join("codex-abcdef.json"),
        dir.join("codex-renamed.json"),
    )
    .unwrap();
    assert!(registry.list(true).is_empty());
}
