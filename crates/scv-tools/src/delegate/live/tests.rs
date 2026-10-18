//! Unit tests for `src/delegate/live.rs`.

use super::*;
use crate::delegate::records::DelegationRegistry;

/// A live `sh -c script` recorded under a fresh registry in `home`.
async fn spawn(home: &std::path::Path, script: &str) -> (Arc<DelegationRegistry>, Arc<LiveChild>) {
    let registry = Arc::new(DelegationRegistry::for_test(&scv_client::Layout::new(home)));
    let pending = registry.begin("fake", "session", home, Some(("fake-1", 1)));
    let child = LiveChild::spawn(
        LiveSpec {
            executable: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: home.to_owned(),
            environment: pending.environment.clone(),
            max_line_bytes: 1024,
        },
        Some((Arc::clone(&registry), pending)),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    (registry, child)
}

/// Whether `pid` is an uncollected zombie.
fn zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let state = stat.rsplit_once(") ")?.1.chars().next()?;
            Some(state == 'Z')
        })
        .unwrap_or(false)
}

/// Wait until the child is collected and its record is gone.
async fn settles(registry: &DelegationRegistry, child: &LiveChild) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while child.is_running() || !registry.list(true).is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the exited child was collected and forgotten");
    assert!(!zombie(child.pid), "the exited child was left a zombie");
}

#[tokio::test]
async fn a_child_exiting_between_turns_is_collected_and_forgotten() {
    let home = tempfile::tempdir().unwrap();
    // Idle between turns: nothing reads its output when it exits.
    let (registry, child) = spawn(home.path(), "sleep 0.2").await;
    assert!(child.is_running());
    assert_eq!(registry.list(true).len(), 1);
    settles(&registry, &child).await;
    // Closing afterwards is a quick no-op.
    tokio::time::timeout(Duration::from_secs(5), child.close())
        .await
        .unwrap();
}

#[tokio::test]
async fn killing_an_idle_child_frees_it_at_once() {
    let home = tempfile::tempdir().unwrap();
    let (registry, child) = spawn(home.path(), "exec sleep 30").await;
    let handle = registry.list(true)[0].record.handle.clone();
    registry.kill(&handle).await.unwrap();
    settles(&registry, &child).await;
}

#[tokio::test]
async fn close_stops_a_running_child_and_its_record() {
    let home = tempfile::tempdir().unwrap();
    // Ignores the closed input, so the group kill ends it.
    let (registry, child) = spawn(home.path(), "trap '' TERM; sleep 30").await;
    child.close().await;
    assert!(!child.is_running());
    assert_eq!(registry.list(true), []);
    assert!(!zombie(child.pid));
}

#[tokio::test]
async fn a_child_is_recorded_idle_between_turns_and_at_work_during_one() {
    let home = tempfile::tempdir().unwrap();
    let (registry, child) = spawn(home.path(), "exec sleep 30").await;
    let entry = || {
        let mut entries = registry.list(true);
        assert_eq!(entries.len(), 1);
        entries.remove(0)
    };
    // A child starts inside its first turn.
    assert!(entry().working());
    let turn = child.begin_turn(2);
    assert_eq!(entry().record.turn, Some(2));
    assert!(entry().working());
    drop(turn);
    // Between turns its process lives on, but it is not at work.
    let idle = entry();
    assert!(idle.record.idle_since_unix.is_some());
    assert!(idle.processes > 0);
    assert!(!idle.working());
    // Its own background jobs keep it at work between turns, until they
    // settle.
    child.set_background_jobs(2);
    assert_eq!(entry().record.background_jobs, Some(2));
    assert!(entry().working());
    child.set_background_jobs(0);
    assert_eq!(entry().record.background_jobs, None);
    assert!(!entry().working());
    let turn = child.begin_turn(3);
    assert_eq!(entry().record.idle_since_unix, None);
    assert!(entry().working());
    // A turn that outlives its child leaves no record behind.
    child.close().await;
    drop(turn);
    assert_eq!(registry.list(true), []);
}

#[tokio::test]
async fn a_write_the_child_can_take_beats_a_passed_deadline_or_a_fired_token() {
    let home = tempfile::tempdir().unwrap();
    let input = home.path().join("input");
    let (_registry, child) = spawn(home.path(), &format!("exec cat > '{}'", input.display())).await;
    child.send(&serde_json::json!({"n": 1})).await.unwrap();
    // A turn whose deadline has just passed and whose token has fired still
    // gets out a request the child can take now, such as its prompt.
    // Repeated, since an unordered race would let some through anyway.
    let fired = CancellationToken::new();
    fired.cancel();
    for n in 2..=20 {
        child
            .send_until(&serde_json::json!({"n": n}), Some(Instant::now()), &fired)
            .await
            .unwrap();
    }
    child.close().await;
    let expected: String = (1..=20).map(|n| format!("{{\"n\":{n}}}\n")).collect();
    assert_eq!(std::fs::read_to_string(&input).unwrap(), expected);
}

#[tokio::test]
async fn an_owed_message_a_child_never_reads_is_bounded_and_ends_its_input() {
    let home = tempfile::tempdir().unwrap();
    // Never reads its input, so a message larger than the pipe stalls.
    let (_registry, child) = spawn(home.path(), "exec sleep 30").await;
    let large = serde_json::json!({"text": "x".repeat(1 << 20)});
    let started = Instant::now();
    let error = child.send(&large).await.unwrap_err();
    assert_eq!(
        error.kind,
        scv_core::ToolFailure::Limit,
        "{}",
        error.message
    );
    assert!(started.elapsed() < SEND_GRACE + Duration::from_secs(3));
    // Part of that line reached the child, so nothing may follow it.
    let error = child.send(&serde_json::json!({"n": 1})).await.unwrap_err();
    assert!(
        error.message.contains("input is closed"),
        "{}",
        error.message
    );
    child.close().await;
}
