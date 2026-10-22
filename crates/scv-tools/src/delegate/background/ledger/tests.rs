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

#[test]
fn a_late_older_revision_never_replaces_a_newer_one() {
    // Records are written after the job table is let go, so two writes of
    // one job can land in either order; the newest stays.
    let root = tempfile::tempdir().unwrap();
    let ledger = ledger(root.path());
    let arguments = json!({"prompt":"p"});
    let mut record = ledger.record(&new_job("job-1", &arguments)).unwrap();
    let older = record.clone();
    record.state = AgentState::Completed;
    record.reported = true;
    ledger.save_revision(&record, 3);
    ledger.save_revision(&older, 2);
    assert_eq!(
        load(root.path(), "session-1", "job-1"),
        Some(record.clone())
    );
    record.reply = Some("done".into());
    ledger.save_revision(&record, 4);
    assert_eq!(
        load(root.path(), "session-1", "job-1")
            .unwrap()
            .reply
            .as_deref(),
        Some("done")
    );
}

#[test]
fn a_snapshot_taken_before_a_dismissal_or_a_resume_does_not_undo_either() {
    // The runtime copies a record, lets the job table go, then writes. The
    // owner can dismiss or resume in between; that copy must not land over
    // what they did.
    let root = tempfile::tempdir().unwrap();
    let ledger = ledger(root.path());
    let arguments = json!({"prompt":"secret plan","cwd":"shop"});
    let mut running = ledger.record(&new_job("job-1", &arguments)).unwrap();
    ledger.save(&running);
    let copied = running.clone();
    finish(
        &mut running,
        AgentState::Completed,
        &json!({"reply":"done"}),
        true,
    );
    ledger.save(&running);
    dismiss(root.path(), "session-1", "job-1").unwrap();
    // Both copies were taken before the dismissal and are written now.
    ledger.save_revision(&copied, 2);
    ledger.save_revision(&running, 4);
    assert_eq!(load(root.path(), "session-1", "job-1"), None);
    let kept = std::fs::read_to_string(root.path().join("session-1/job-1.json")).unwrap();
    assert!(
        !kept.contains("secret plan") && !kept.contains("done"),
        "{kept}"
    );

    let mut interrupted = ledger.record(&new_job("job-2", &arguments)).unwrap();
    interrupted.state = AgentState::Interrupted;
    ledger.save(&interrupted);
    let mut before_resume = interrupted.clone();
    update(root.path(), "session-1", "job-2", |record| {
        record.resumed_as = Some("workbench-1/job-9".into());
    });
    // Reported after the resume, from the copy that does not know it.
    before_resume.reported = true;
    ledger.save_revision(&before_resume, 3);
    let kept = load(root.path(), "session-1", "job-2").unwrap();
    assert_eq!(kept.resumed_as.as_deref(), Some("workbench-1/job-9"));
    assert!(kept.reported);

    let mut successor = ledger.record(&new_job("job-3", &arguments)).unwrap();
    successor.resumed_from = Some("session-1/job-2".into());
    ledger.save(&successor);
    let mut stale = successor.clone();
    stale.resumed_from = None;
    stale.reply = Some("continued".into());
    ledger.save_revision(&stale, 2);
    let kept = load(root.path(), "session-1", "job-3").unwrap();
    assert_eq!(kept.resumed_from.as_deref(), Some("session-1/job-2"));
    assert_eq!(kept.reply.as_deref(), Some("continued"));
}

#[test]
fn a_control_heavy_prompt_still_loads() {
    let root = tempfile::tempdir().unwrap();
    let ledger = ledger(root.path());
    let arguments = json!({"prompt":"p"});
    let mut record = ledger.record(&new_job("job-1", &arguments)).unwrap();
    record.prompt = "\u{1}".repeat(16 * 1024);
    record.reply = Some("\u{1}".repeat(8 * 1024));
    record.error = Some("\u{1}".repeat(8 * 1024));
    ledger.save(&record);
    let loaded = load(root.path(), "session-1", "job-1").expect("record over the old cap vanished");
    assert!(!loaded.prompt.is_empty());
}

#[test]
fn a_dismissed_record_keeps_only_its_number() {
    let root = tempfile::tempdir().unwrap();
    let ledger = ledger(root.path());
    let arguments = json!({"prompt":"secret plan","cwd":"shop"});
    let mut record = ledger.record(&new_job("job-2", &arguments)).unwrap();
    record.state = AgentState::Failed;
    record.error = Some("boom".into());
    ledger.save(&record);
    dismiss(root.path(), "session-1", "job-2").unwrap();
    assert_eq!(load(root.path(), "session-1", "job-2"), None);
    assert_eq!(list(root.path()), []);
    assert_eq!(ledger.last_number(), 2);
    let kept = std::fs::read_to_string(root.path().join("session-1/job-2.json")).unwrap();
    assert!(
        !kept.contains("secret plan") && !kept.contains("boom"),
        "{kept}"
    );
    assert!(dismiss(root.path(), "session-1", "job-2").is_err());
    // It goes with the rest once old enough.
    assert_eq!(prune(root.path(), Duration::ZERO), 0);
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(prune(root.path(), Duration::ZERO), 1);
}
