//! Unit tests for `src/delegate/live.rs`.

use super::*;
use crate::delegate::records::DelegationRegistry;

/// A live `sh -c script` recorded under a fresh registry in `home`.
fn spawn(home: &std::path::Path, script: &str) -> (Arc<DelegationRegistry>, Arc<LiveChild>) {
    let registry = Arc::new(DelegationRegistry::new(&scv_client::Layout::new(home)));
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
    )
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
    let (registry, child) = spawn(home.path(), "sleep 0.2");
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
    let (registry, child) = spawn(home.path(), "exec sleep 30");
    let handle = registry.list(true)[0].record.handle.clone();
    registry.kill(&handle).await.unwrap();
    settles(&registry, &child).await;
}

#[tokio::test]
async fn close_stops_a_running_child_and_its_record() {
    let home = tempfile::tempdir().unwrap();
    // Ignores the closed input, so the group kill ends it.
    let (registry, child) = spawn(home.path(), "trap '' TERM; sleep 30");
    child.close().await;
    assert!(!child.is_running());
    assert!(registry.list(true).is_empty());
    assert!(!zombie(child.pid));
}
