//! Unit tests for `src/delegate/records.rs`.

use super::*;
use std::os::unix::{fs::PermissionsExt as _, process::CommandExt as _};

fn registry(home: &Path) -> Arc<DelegationRegistry> {
    Arc::new(DelegationRegistry::new(&Layout::new(home)))
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

/// The instance ID is part of every `SCV_PARENT` tag, so orphan detection
/// and planned restarts recognize records of the same instance only while it
/// stays exactly this hash of the instance home.
#[test]
fn the_instance_id_is_a_stable_hash_of_the_home() {
    let registry = DelegationRegistry::new(&Layout::new(Path::new("/srv/scv")));
    assert_eq!(registry.instance(), "8973cbc5");
    assert_eq!(
        registry.record_dir(),
        Path::new("/srv/scv/state/delegations")
    );
    assert_eq!(
        registry.conversation_dir(),
        Path::new("/srv/scv/state/conversations")
    );
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
fn a_chain_names_the_run_this_process_started() {
    let home = tempfile::tempdir().unwrap();
    let registry = DelegationRegistry::new(&Layout::new(home.path()));
    let mut agent = std::process::Command::new("sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let record = |handle: &str, owner: ProcessIdentity| DelegationRecord {
        handle: handle.into(),
        agent: "codex".into(),
        instance: registry.instance().into(),
        session: "s".into(),
        owner,
        process: ProcessIdentity::of(agent.id()).unwrap(),
        pgid: agent.id(),
        cwd: "/work".into(),
        started_unix: 1,
        depth: 1,
        conversation: None,
        turn: None,
        idle_since_unix: None,
        background_jobs: None,
    };
    write_record(
        registry.record_dir(),
        &record("codex-a1b2c3", ProcessIdentity::current().unwrap()),
    )
    .unwrap();
    // A run another live process owns is not this process's.
    write_record(
        registry.record_dir(),
        &record("codex-d4e5f6", ProcessIdentity::of(agent.id()).unwrap()),
    )
    .unwrap();
    let own = format!("{}/s/codex-a1b2c3", registry.instance());
    let expected = Some(ChainRun {
        handle: "codex-a1b2c3".into(),
        session: "s".into(),
    });
    assert_eq!(registry.own_run(&own), expected);
    // Nested SCVs add entries of their own instances around it.
    assert_eq!(
        registry.own_run(&format!(
            "elsewhere/x/codex-000000;{own};nested/y/claude-111111"
        )),
        expected
    );
    assert_eq!(registry.own_run("elsewhere/s/codex-a1b2c3"), None);
    assert_eq!(
        registry.own_run(&format!("{}/s/codex-d4e5f6", registry.instance())),
        None
    );
    assert_eq!(registry.own_run("codex-a1b2c3"), None);
    assert_eq!(registry.own_run(""), None);
    agent.kill().unwrap();
    agent.wait().unwrap();
}

#[test]
fn a_declared_client_depth_raises_the_recorded_depth() {
    let home = tempfile::tempdir().unwrap();
    let registry = DelegationRegistry::new(&Layout::new(home.path()));
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
    let mut registry = DelegationRegistry::new(&Layout::new(home.path()));
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
        write_private_json(markers, &format!("{id}.json"), &marker).unwrap();
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
    let registry = DelegationRegistry::new(&Layout::new(home.path()));
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
        idle_since_unix: None,
        background_jobs: None,
    };
    write_record(&dir, &record).unwrap();
    std::fs::copy(
        dir.join("codex-abcdef.json"),
        dir.join("codex-renamed.json"),
    )
    .unwrap();
    assert!(registry.list(true).is_empty());
}

/// Records are read across SCV processes of different releases: the daemon,
/// `scv exec` servers, and after a planned restart the previous release's
/// watchdog or a rolled-back daemon.
#[test]
fn records_stay_readable_across_releases() {
    /// 0.3.0's record: the same fields without `idle_since_unix` and
    /// `background_jobs`.
    #[derive(Deserialize)]
    #[allow(dead_code, reason = "only parsing matters")]
    struct Release030 {
        handle: String,
        agent: String,
        instance: String,
        session: String,
        owner: ProcessIdentity,
        process: ProcessIdentity,
        pgid: u32,
        cwd: PathBuf,
        started_unix: u64,
        depth: u32,
        #[serde(default)]
        conversation: Option<String>,
        #[serde(default)]
        turn: Option<u32>,
    }
    // A record as 0.3.0 wrote it parses unchanged and says nothing of idling
    // or background jobs.
    let old = r#"{"handle":"scv-92f0c3","agent":"scv","instance":"8973cbc5","session":"s","owner":{"pid":10,"start_time":5},"process":{"pid":11,"start_time":6},"pgid":11,"cwd":"/w","started_unix":1,"depth":1,"conversation":"scv-1","turn":1}"#;
    let record: DelegationRecord = serde_json::from_str(old).unwrap();
    assert_eq!(record.idle_since_unix, None);
    assert_eq!(record.background_jobs, None);
    assert_eq!(serde_json::to_string(&record).unwrap(), old);
    // A live child between turns with a background job of its own adds two
    // fields, which 0.3.0 skips.
    let idle = DelegationRecord {
        idle_since_unix: Some(7),
        background_jobs: Some(2),
        ..record
    };
    let encoded = serde_json::to_string(&idle).unwrap();
    assert!(
        encoded.ends_with(r#""turn":1,"idle_since_unix":7,"background_jobs":2}"#),
        "{encoded}"
    );
    let read: Release030 = serde_json::from_str(&encoded).unwrap();
    assert_eq!(read.handle, "scv-92f0c3");
    assert_eq!(read.turn, Some(1));
    assert_eq!(
        serde_json::from_str::<DelegationRecord>(&encoded).unwrap(),
        idle
    );
}

#[test]
fn a_live_child_between_turns_works_only_while_its_own_jobs_do() {
    let record: DelegationRecord = serde_json::from_str(
        r#"{"handle":"scv-92f0c3","agent":"scv","instance":"8973cbc5","session":"s","owner":{"pid":10,"start_time":5},"process":{"pid":11,"start_time":6},"pgid":11,"cwd":"/w","started_unix":1,"depth":1,"conversation":"scv-1","turn":1}"#,
    )
    .unwrap();
    let entry = |idle_since_unix, background_jobs, processes| DelegationEntry {
        record: DelegationRecord {
            idle_since_unix,
            background_jobs,
            ..record.clone()
        },
        orphaned: false,
        processes,
    };
    // Mid-turn, or a per-turn run: at work while its processes live.
    assert!(entry(None, None, 1).working());
    assert!(!entry(None, None, 0).working());
    // Between turns: at work only while its own background jobs are.
    assert!(!entry(Some(7), None, 1).working());
    assert!(!entry(Some(7), Some(0), 1).working());
    assert!(entry(Some(7), Some(1), 1).working());
    // A child that is gone does no work, whatever its record says.
    assert!(!entry(Some(7), Some(1), 0).working());
}
