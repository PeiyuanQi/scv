//! Unit tests for `src/delegate/background/ledger.rs`.

use serde_json::json;

use super::*;

fn ledger(root: &Path) -> JobLedger {
    JobLedger::new(root, "session-1", "TUI", Path::new("/w")).unwrap()
}

fn new_job<'a>(job: &'a str, arguments: &'a Value) -> NewJob<'a> {
    NewJob {
        job,
        agent: "codex",
        task: "Fix the flaky test",
        arguments,
        started_by: JobOrigin::Model,
        reviewed: false,
        queued: false,
    }
}

#[test]
fn a_record_keeps_the_prompt_apart_from_the_other_arguments() {
    let root = tempfile::tempdir().unwrap();
    let ledger = ledger(root.path());
    let arguments =
        json!({"agent":"codex","prompt":"Fix the flaky test","cwd":"shop","effort":"high"});
    let record = ledger.record(&new_job("job-1", &arguments)).unwrap();
    assert_eq!(record.prompt, "Fix the flaky test");
    assert_eq!(
        record.arguments,
        json!({"agent":"codex","cwd":"shop","effort":"high"})
    );
    assert_eq!(record.state, AgentState::Working);
    assert_eq!(record.label, "TUI");
    ledger.save(&record);
    assert_eq!(load(root.path(), "session-1", "job-1"), Some(record));
    let queued = ledger
        .record(&NewJob {
            queued: true,
            ..new_job("job-2", &arguments)
        })
        .unwrap();
    assert_eq!(queued.state, AgentState::Queued);
    // Files are private.
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(root.path().join("session-1/job-1.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn finishing_records_the_reply_conversation_and_worktree() {
    let root = tempfile::tempdir().unwrap();
    let ledger = ledger(root.path());
    let arguments = json!({"prompt":"go"});
    let mut record = ledger.record(&new_job("job-1", &arguments)).unwrap();
    let result = json!({
        "agent":"codex","status":"failed","reply":"I could not","error":"tests fail",
        "session":"codex-2",
        "worktree":{"path":"/h/worktrees/shop-1/codex-abc123","branch":"scv/codex-abc123","repo":"/w/shop"}
    });
    finish(&mut record, AgentState::Failed, &result, false);
    assert_eq!(record.state, AgentState::Failed);
    assert_eq!(record.reply.as_deref(), Some("I could not"));
    assert_eq!(record.error.as_deref(), Some("tests fail"));
    assert_eq!(record.conversation.as_deref(), Some("codex-2"));
    assert_eq!(record.worktree.unwrap().branch, "scv/codex-abc123");
    assert!(record.finished_unix.is_some());
}

#[test]
fn unfinished_jobs_of_dead_processes_become_interrupted_and_resumable() {
    let root = tempfile::tempdir().unwrap();
    let arguments = json!({"prompt":"go"});
    let mut record = ledger(root.path())
        .record(&new_job("job-1", &arguments))
        .unwrap();
    record.owner = ProcessIdentity {
        pid: u32::MAX - 2,
        start_time: 1,
    };
    save(root.path(), &record).unwrap();
    let mut alive = record.clone();
    alive.job = "job-2".into();
    alive.owner = ProcessIdentity::current().unwrap();
    save(root.path(), &alive).unwrap();
    let settled = settle_orphans(root.path());
    assert_eq!(settled.len(), 1);
    let interrupted = load(root.path(), "session-1", "job-1").unwrap();
    assert_eq!(interrupted.state, AgentState::Interrupted);
    assert!(interrupted.resumable());
    assert_eq!(
        load(root.path(), "session-1", "job-2").unwrap().state,
        AgentState::Working
    );
    let resumed = update(root.path(), "session-1", "job-1", |record| {
        record.resumed_as = Some("workbench-1/job-1".into());
    })
    .unwrap();
    assert!(!resumed.resumable());
    let mut reviewed = interrupted;
    reviewed.reviewed = true;
    assert!(!reviewed.resumable());
}

#[test]
fn finished_jobs_are_pruned_once_old_and_names_never_escape_the_ledger() {
    let root = tempfile::tempdir().unwrap();
    let arguments = json!({"prompt":"go"});
    let ledger = ledger(root.path());
    let mut done = ledger.record(&new_job("job-1", &arguments)).unwrap();
    finish(&mut done, AgentState::Completed, &json!({}), true);
    done.updated_unix = 1;
    save(root.path(), &done).unwrap();
    let running = ledger.record(&new_job("job-2", &arguments)).unwrap();
    save(root.path(), &running).unwrap();
    assert_eq!(prune(root.path(), Duration::from_secs(3600)), 1);
    assert_eq!(list(root.path()).len(), 1);
    remove(root.path(), "session-1", "job-2").unwrap();
    assert_eq!(list(root.path()), []);
    assert!(!root.path().join("session-1").exists());
    assert!(JobLedger::new(root.path(), "../x", "TUI", Path::new("/w")).is_none());
    assert!(load(root.path(), "session-1", "../job-1").is_none());
    let mut escaping = running;
    escaping.job = "../../etc".into();
    assert!(save(root.path(), &escaping).is_err());
}

#[test]
fn a_record_named_for_another_job_is_not_trusted() {
    let root = tempfile::tempdir().unwrap();
    let arguments = json!({"prompt":"go"});
    let record = ledger(root.path())
        .record(&new_job("job-1", &arguments))
        .unwrap();
    save(root.path(), &record).unwrap();
    std::fs::rename(
        root.path().join("session-1/job-1.json"),
        root.path().join("session-1/job-9.json"),
    )
    .unwrap();
    assert!(load(root.path(), "session-1", "job-9").is_none());
    assert_eq!(list(root.path()), []);
}

#[test]
fn a_session_back_under_its_id_numbers_its_jobs_after_the_recorded_ones() {
    let root = tempfile::tempdir().unwrap();
    let ledger = ledger(root.path());
    assert_eq!(ledger.last_number(), 0);
    let arguments = json!({"agent":"codex","prompt":"p"});
    for job in ["job-2", "job-10"] {
        ledger.save(&ledger.record(&new_job(job, &arguments)).unwrap());
    }
    std::fs::write(root.path().join("session-1/notes.json"), "{}").unwrap();
    assert_eq!(ledger.last_number(), 10);
    let jobs = crate::delegate::background::BackgroundJobs::new(2, None).with_ledger(ledger);
    assert_eq!(jobs.state().next, 10);
}
