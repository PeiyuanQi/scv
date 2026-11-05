//! Unit tests for `src/delegate/background/subscription.rs`.

use std::path::Path;

use scv_protocol::{AgentState, JobOrigin};
use serde_json::json;

use super::{DELIVER_FOLLOW_UPS, Decision, Error, Fanout, Incoming, Plan, Subscriptions, Worker};
use crate::delegate::background::ledger::{self, JobLedger, NewJob};

const THREAD: &str = "owner\0thread\0omt_1";
const EXTRA: &str = "owner\0thread\0omt_2";
const WHO: &str = "owner";
const WHY: &str = "follow this job from the thread";

fn jobs(home: &Path) -> std::path::PathBuf {
    home.join("jobs")
}

fn open(home: &Path) -> Subscriptions {
    Subscriptions::open(&home.join("subscriptions"), &jobs(home))
}

fn job(home: &Path) -> ledger::JobRecord {
    let ledger = JobLedger::new(&jobs(home), "session-1", "channel", Path::new("/work")).unwrap();
    let arguments = json!({"prompt": "Fix the flaky test", "session": "codex-1"});
    let mut record = ledger
        .record(&NewJob {
            job: "job-1",
            agent: "codex",
            task: "Fix the flaky test",
            arguments: &arguments,
            started_by: JobOrigin::Model,
            reviewed: false,
            queued: false,
        })
        .unwrap();
    record.conversation = Some("codex-1".into());
    ledger.save(&record);
    record
}

fn bind(book: &mut Subscriptions) {
    book.bind(THREAD, "session-1", "job-1", WHO, WHY).unwrap();
}

fn incoming<'a>(
    account: Option<&'a str>,
    thread: &'a str,
    message_id: &'a str,
    text: &'a str,
) -> Incoming<'a> {
    Incoming {
        account,
        thread,
        message_id,
        text,
        reply_to: "",
        thread_reply: "",
    }
}

#[test]
fn bind_records_an_existing_job_and_does_not_start_one() {
    let home = tempfile::tempdir().unwrap();
    let record = job(home.path());
    let before = ledger::list(&jobs(home.path()));
    let mut book = open(home.path());
    let subscription = book.bind(THREAD, "session-1", "job-1", WHO, WHY).unwrap();
    assert_eq!(subscription.thread, THREAD);
    assert_eq!(subscription.session, "session-1");
    assert_eq!(subscription.job, "job-1");
    assert_eq!(subscription.authorization.who, WHO);
    assert_eq!(subscription.authorization.why, WHY);
    assert_eq!(subscription.authorization.scope, DELIVER_FOLLOW_UPS);
    assert!(!subscription.authorization.stopped);
    assert!(subscription.cursor.is_none());
    assert_eq!(ledger::list(&jobs(home.path())), before);
    assert_eq!(
        ledger::load(&jobs(home.path()), "session-1", "job-1"),
        Some(record)
    );
    let saved = open(home.path());
    let again = saved.subscription(THREAD).unwrap();
    assert_eq!(again.authorization, subscription.authorization);
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(
        home.path()
            .join("subscriptions")
            .join(format!("{}.json", subscription.id)),
    )
    .unwrap()
    .permissions()
    .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn bind_rejects_a_missing_job() {
    let home = tempfile::tempdir().unwrap();
    let mut book = open(home.path());
    let error = book
        .bind(THREAD, "session-1", "job-9", WHO, WHY)
        .unwrap_err();
    assert!(matches!(
        error,
        Error::MissingJob { ref session, ref job } if session == "session-1" && job == "job-9"
    ));
    assert_eq!(book.len(), 0);
    job(home.path());
    ledger::dismiss(&jobs(home.path()), "session-1", "job-1").unwrap();
    let error = book
        .bind(THREAD, "session-1", "job-1", WHO, WHY)
        .unwrap_err();
    assert!(matches!(error, Error::MissingJob { .. }));
    assert_eq!(book.len(), 0);
}

#[test]
fn a_thread_has_one_live_subscription() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    assert!(matches!(
        book.bind(THREAD, "session-1", "job-1", WHO, WHY),
        Err(Error::ThreadTaken)
    ));
    book.attach(THREAD, EXTRA).unwrap();
    assert!(matches!(
        book.bind(EXTRA, "session-1", "job-1", WHO, WHY),
        Err(Error::ThreadTaken)
    ));
    let other = "owner\0thread\0omt_3";
    book.bind(other, "session-1", "job-1", WHO, WHY).unwrap();
    assert!(book.subscription(THREAD).is_some());
    assert_eq!(book.subscription(EXTRA).unwrap().thread, THREAD);
    assert_eq!(book.subscription(other).unwrap().thread, other);
    assert_ne!(
        book.subscription(THREAD).unwrap().id,
        book.subscription(other).unwrap().id
    );
}

#[test]
fn cursor_skips_already_delivered_ids_across_a_reload() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    assert!(matches!(
        book.decide(THREAD, "m1", "first", Worker::Accept).unwrap(),
        Plan::Deliver(_)
    ));
    book.commit_delivery(THREAD, "m1").unwrap();
    assert!(matches!(
        book.decide(THREAD, "m2", "second", Worker::Accept).unwrap(),
        Plan::Deliver(_)
    ));
    book.commit_delivery(THREAD, "m2").unwrap();
    let mut book = open(home.path());
    assert_eq!(
        book.subscription(THREAD).unwrap().cursor.as_deref(),
        Some("m2")
    );
    let plans = book
        .accept_replay(
            THREAD,
            &[("m1", "first"), ("m2", "second"), ("m3", "third")],
            Worker::Accept,
        )
        .unwrap();
    assert!(matches!(plans[0], Plan::Settled));
    assert!(matches!(plans[1], Plan::Settled));
    assert!(matches!(plans[2], Plan::Deliver(_)));
    assert!(matches!(
        book.decide(THREAD, "m1", "first", Worker::Accept).unwrap(),
        Plan::Settled
    ));
    assert!(matches!(
        book.decide(THREAD, "m2", "second", Worker::Accept).unwrap(),
        Plan::Settled
    ));
}

#[test]
fn hand_back_drops_only_the_extra_thread_and_returns_the_message() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    let authorization = book.subscription(THREAD).unwrap().authorization.clone();
    book.attach(THREAD, EXTRA).unwrap();
    assert_eq!(
        book.subscription(THREAD).unwrap().authorization,
        authorization
    );
    book.decide(EXTRA, "m1", "not this job", Worker::Accept)
        .unwrap();
    let plan = book.hand_back(EXTRA, "m1", "not this job").unwrap();
    let Plan::HandedBack(handed) = plan else {
        panic!("hand-back returns the message to intake, got {plan:?}");
    };
    assert_eq!(handed.text, "not this job");
    assert_eq!(handed.message_id, "m1");
    assert_eq!(handed.dropped.as_deref(), Some(EXTRA));
    assert!(book.subscription(EXTRA).is_none());
    assert_eq!(book.subscription(THREAD).unwrap().thread, THREAD);
    assert_eq!(
        book.subscription(THREAD).unwrap().extra_threads,
        Vec::<String>::new()
    );
    assert!(matches!(
        book.decide(EXTRA, "m2", "ordinary", Worker::Accept)
            .unwrap(),
        Plan::Intake
    ));
    let decision = book
        .subscription(THREAD)
        .unwrap()
        .decisions
        .iter()
        .find(|decision| decision.message_id == "m1")
        .unwrap();
    assert_eq!(decision.fanout, Fanout::HandedBack);
    assert_eq!(decision.thread, EXTRA);
}

#[test]
fn the_bound_thread_cannot_be_dropped() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    book.attach(THREAD, EXTRA).unwrap();
    book.decide(THREAD, "m1", "still mine", Worker::Accept)
        .unwrap();
    let plan = book.hand_back(THREAD, "m1", "still mine").unwrap();
    let Plan::HandedBack(handed) = plan else {
        panic!("the message goes to intake, got {plan:?}");
    };
    assert_eq!(handed.text, "still mine");
    assert!(handed.dropped.is_none());
    assert_eq!(book.subscription(THREAD).unwrap().thread, THREAD);
    assert_eq!(
        book.subscription(THREAD).unwrap().extra_threads,
        vec![EXTRA.to_owned()]
    );
    let next = book
        .decide(THREAD, "m2", "continue", Worker::Accept)
        .unwrap();
    assert!(matches!(next, Plan::Deliver(_)));
}

#[test]
fn a_dismissed_or_stopped_job_receives_nothing() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    ledger::dismiss(&jobs(home.path()), "session-1", "job-1").unwrap();
    assert!(matches!(
        book.decide(THREAD, "m1", "hello", Worker::Accept).unwrap(),
        Plan::Withheld
    ));
    let record =
        ledger::load_including_dismissed(&jobs(home.path()), "session-1", "job-1").unwrap();
    assert!(record.dismissed);
    assert!(record.resumed_as.is_none());
    assert_eq!(
        book.subscription(THREAD).unwrap().decisions[0].fanout,
        Fanout::Intake,
        "the id is remembered so a later presentation is not delivered"
    );
    assert!(matches!(
        book.decide(THREAD, "m1", "hello", Worker::Accept).unwrap(),
        Plan::Intake
    ));

    let home = tempfile::tempdir().unwrap();
    job(home.path());
    ledger::update(&jobs(home.path()), "session-1", "job-1", |record| {
        record.state = AgentState::Cancelled;
    });
    let mut book = open(home.path());
    bind(&mut book);
    assert!(matches!(
        book.decide(THREAD, "m1", "hello", Worker::Accept).unwrap(),
        Plan::Withheld
    ));
    assert!(
        ledger::load(&jobs(home.path()), "session-1", "job-1")
            .unwrap()
            .resumed_as
            .is_none()
    );

    let home = tempfile::tempdir().unwrap();
    job(home.path());
    ledger::update(&jobs(home.path()), "session-1", "job-1", |record| {
        record.state = AgentState::Completed;
    });
    let mut book = open(home.path());
    bind(&mut book);
    book.stop(THREAD).unwrap();
    assert!(matches!(
        book.decide(THREAD, "m1", "hello", Worker::Accept).unwrap(),
        Plan::Withheld
    ));
    assert!(
        ledger::load(&jobs(home.path()), "session-1", "job-1")
            .unwrap()
            .resumed_as
            .is_none()
    );
    assert!(book.subscription(THREAD).unwrap().authorization.stopped);
    let mut reloaded = open(home.path());
    assert!(matches!(
        reloaded
            .decide(THREAD, "m2", "again", Worker::Accept)
            .unwrap(),
        Plan::Withheld
    ));
}

#[test]
fn a_running_reviewed_job_does_not_take_a_follow_up() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    ledger::update(&jobs(home.path()), "session-1", "job-1", |record| {
        record.reviewed = true;
        record.state = AgentState::Working;
    });
    let mut book = open(home.path());
    bind(&mut book);
    let plan = book
        .decide(THREAD, "m1", "look at this", Worker::Accept)
        .unwrap();
    assert!(
        matches!(plan, Plan::Withheld),
        "the review's conversation is not given the message, got {plan:?}"
    );
    assert_eq!(book.pending_messages(), []);
    assert!(
        ledger::load(&jobs(home.path()), "session-1", "job-1")
            .unwrap()
            .resumed_as
            .is_none(),
        "no unreviewed job is queued onto the review"
    );
    named_job(home.path(), "job-2");
    assert!(
        matches!(
            book.bind(THREAD, "session-1", "job-2", WHO, "another"),
            Err(Error::ThreadTaken)
        ),
        "the thread stays bound while the review runs"
    );
}

#[test]
fn a_finished_job_is_resumed_under_the_same_authorization() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    ledger::update(&jobs(home.path()), "session-1", "job-1", |record| {
        record.state = AgentState::Completed;
        record.finished_unix = Some(1);
    });
    assert!(
        book.subscription(THREAD).is_some(),
        "ending the job keeps the subscription"
    );
    let authorization = book.subscription(THREAD).unwrap().authorization.clone();
    let plan = book
        .decide(THREAD, "m9", "please continue", Worker::Accept)
        .unwrap();
    let Plan::Resume(resume) = plan else {
        panic!("a later message resumes the finished job, got {plan:?}");
    };
    assert_eq!(resume.authorization, authorization);
    assert_eq!(resume.session, "session-1");
    assert_eq!(resume.job, "job-1");
    assert_eq!(resume.prompt, "please continue");
    assert_eq!(resume.conversation.as_deref(), Some("codex-1"));
    let successor = successor(home.path());
    book.commit_resume(&resume.session, &resume.job, &successor)
        .unwrap();
    let old = ledger::load(&jobs(home.path()), "session-1", "job-1").unwrap();
    assert_eq!(old.resumed_as.as_deref(), Some(successor.as_str()));
    let new = ledger::load(&jobs(home.path()), "session-1", "job-2").unwrap();
    assert_eq!(new.resumed_from.as_deref(), Some("session-1/job-1"));
    let reloaded = open(home.path());
    assert_eq!(
        reloaded.subscription(THREAD).unwrap().authorization,
        authorization
    );
    assert!(!reloaded.subscription(THREAD).unwrap().authorization.stopped);
}

#[test]
fn restart_does_not_redeliver_or_resume_an_interrupted_job() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    book.decide(THREAD, "m1", "first", Worker::Accept).unwrap();
    book.commit_delivery(THREAD, "m1").unwrap();
    book.decide(THREAD, "m2", "second", Worker::Accept).unwrap();
    book.commit_delivery(THREAD, "m2").unwrap();
    ledger::update(&jobs(home.path()), "session-1", "job-1", |record| {
        record.state = AgentState::Interrupted;
        record.interrupted = Some("the SCV process that ran it stopped".into());
    });
    let mut book = open(home.path());
    book.reload();
    let record = ledger::load(&jobs(home.path()), "session-1", "job-1").unwrap();
    assert_eq!(record.state, AgentState::Interrupted);
    assert!(record.resumed_as.is_none());
    let plans = book
        .accept_replay(
            THREAD,
            &[
                ("earlier", "old"),
                ("m1", "first"),
                ("m2", "second"),
                ("m3", "later"),
            ],
            Worker::Accept,
        )
        .unwrap();
    assert!(
        matches!(plans[0], Plan::Settled),
        "before the cursor: {plans:?}"
    );
    assert!(matches!(plans[1], Plan::Settled));
    assert!(matches!(plans[2], Plan::Settled));
    let Plan::Resume(resume) = &plans[3] else {
        panic!("only a later message resumes it, got {plans:?}");
    };
    assert_eq!(resume.authorization.scope, DELIVER_FOLLOW_UPS);
    assert_eq!(resume.prompt, "later");
    assert!(
        ledger::load(&jobs(home.path()), "session-1", "job-1")
            .unwrap()
            .resumed_as
            .is_none(),
        "planning the resume does not resume it until the link is committed"
    );
    let successor = successor(home.path());
    book.commit_resume("session-1", "job-1", &successor)
        .unwrap();
    assert_eq!(
        ledger::load(&jobs(home.path()), "session-1", "job-1")
            .unwrap()
            .resumed_as
            .as_deref(),
        Some(successor.as_str())
    );
}

#[test]
fn a_delivery_is_not_settled_until_the_job_takes_it() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    assert!(matches!(
        book.decide(THREAD, "m1", "first", Worker::Accept).unwrap(),
        Plan::Deliver(_)
    ));
    assert!(book.subscription(THREAD).unwrap().cursor.is_none());
    assert_eq!(
        book.subscription(THREAD).unwrap().pending[0].message_id,
        "m1"
    );
    let mut book = open(home.path());
    assert!(matches!(
        book.decide(THREAD, "m1", "first", Worker::Accept).unwrap(),
        Plan::Deliver(_)
    ));
    book.commit_delivery(THREAD, "m1").unwrap();
    let mut book = open(home.path());
    assert_eq!(book.subscription(THREAD).unwrap().pending, Vec::new());
    assert!(matches!(
        book.decide(THREAD, "m1", "first", Worker::Accept).unwrap(),
        Plan::Settled
    ));
}

#[test]
fn an_old_message_stays_settled_after_the_decision_window() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    for index in 0..65 {
        let id = format!("m{index}");
        book.decide(THREAD, &id, "again", Worker::Accept).unwrap();
        book.commit_delivery(THREAD, &id).unwrap();
    }
    let mut book = open(home.path());
    assert!(
        book.subscription(THREAD).unwrap().decisions.len() <= 64,
        "the record keeps a window"
    );
    assert!(matches!(
        book.decide(THREAD, "m0", "again", Worker::Accept).unwrap(),
        Plan::Settled
    ));
    assert!(matches!(
        book.decide(THREAD, "m64", "again", Worker::Accept).unwrap(),
        Plan::Settled
    ));
}

#[test]
fn hand_back_still_applies_after_the_job_took_the_message() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    book.attach(THREAD, EXTRA).unwrap();
    book.decide(EXTRA, "m1", "not this job", Worker::Accept)
        .unwrap();
    book.commit_delivery(EXTRA, "m1").unwrap();
    let plan = book.hand_back(EXTRA, "m1", "not this job").unwrap();
    let Plan::HandedBack(handed) = plan else {
        panic!("hand-back still returns the message, got {plan:?}");
    };
    assert_eq!(handed.dropped.as_deref(), Some(EXTRA));
    assert_eq!(book.subscription(THREAD).unwrap().thread, THREAD);
    assert!(book.subscription(EXTRA).is_none());
    assert!(matches!(
        book.decide(THREAD, "m2", "stay", Worker::Accept).unwrap(),
        Plan::Deliver(_)
    ));
}

#[test]
fn a_running_job_without_a_conversation_is_still_offered() {
    let home = tempfile::tempdir().unwrap();
    let ledger = JobLedger::new(
        &jobs(home.path()),
        "session-1",
        "channel",
        Path::new("/work"),
    )
    .unwrap();
    let arguments = json!({"prompt": "Fix the flaky test"});
    let record = ledger
        .record(&NewJob {
            job: "job-1",
            agent: "codex",
            task: "Fix the flaky test",
            arguments: &arguments,
            started_by: JobOrigin::Model,
            reviewed: false,
            queued: false,
        })
        .unwrap();
    assert!(record.conversation.is_none());
    ledger.save(&record);
    let mut book = open(home.path());
    bind(&mut book);
    let plan = book.decide(THREAD, "m1", "hello", Worker::Accept).unwrap();
    let Plan::Deliver(delivery) = plan else {
        panic!("a running job is offered even before its conversation is known, got {plan:?}");
    };
    assert!(delivery.conversation.is_none());
    assert_eq!(delivery.job, "job-1");
}

#[test]
fn a_cancelled_continuation_receives_nothing() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    let successor = successor(home.path());
    ledger::update(&jobs(home.path()), "session-1", "job-2", |record| {
        record.state = AgentState::Cancelled;
    });
    book.note_current(THREAD, &successor).unwrap();
    ledger::update(&jobs(home.path()), "session-1", "job-1", |record| {
        record.resumed_as = Some(successor.clone());
    });
    assert!(matches!(
        book.decide(THREAD, "m1", "hello", Worker::Accept).unwrap(),
        Plan::Withheld
    ));
}

#[test]
fn the_resume_chain_follows_every_link() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    let mut previous = "job-1".to_owned();
    for index in 2..=10 {
        let name = format!("job-{index}");
        let ledger = JobLedger::new(
            &jobs(home.path()),
            "session-1",
            "channel",
            Path::new("/work"),
        )
        .unwrap();
        let arguments = json!({"prompt": "continue", "session": "codex-1"});
        let mut record = ledger
            .record(&NewJob {
                job: &name,
                agent: "codex",
                task: "Fix the flaky test",
                arguments: &arguments,
                started_by: JobOrigin::Model,
                reviewed: false,
                queued: false,
            })
            .unwrap();
        record.resumed_from = Some(format!("session-1/{previous}"));
        record.conversation = Some("codex-1".into());
        if index < 10 {
            record.state = AgentState::Completed;
            record.finished_unix = Some(1);
        }
        ledger.save(&record);
        ledger::update(&jobs(home.path()), "session-1", &previous, |old| {
            old.resumed_as = Some(format!("session-1/{name}"));
            old.state = AgentState::Completed;
            old.finished_unix = Some(1);
        });
        previous = name;
    }
    let plan = book
        .decide(THREAD, "m1", "still going", Worker::Accept)
        .unwrap();
    let Plan::Deliver(delivery) = plan else {
        panic!("the tip of a long chain still takes the message, got {plan:?}");
    };
    assert_eq!(delivery.job, "job-10");
}

#[test]
fn prune_keeps_the_ledger_records_a_live_subscription_needs() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    ledger::update(&jobs(home.path()), "session-1", "job-1", |record| {
        record.state = AgentState::Completed;
        record.updated_unix = 0;
        record.finished_unix = Some(0);
    });
    let mut book = open(home.path());
    bind(&mut book);
    let successor = successor(home.path());
    ledger::update(&jobs(home.path()), "session-1", "job-2", |record| {
        record.state = AgentState::Completed;
        record.updated_unix = 0;
        record.finished_unix = Some(0);
    });
    book.commit_resume("session-1", "job-1", &successor)
        .unwrap();
    for name in ["job-1", "job-2"] {
        let mut record = ledger::load(&jobs(home.path()), "session-1", name).unwrap();
        record.state = AgentState::Completed;
        record.updated_unix = 0;
        record.finished_unix = Some(0);
        ledger::save(&jobs(home.path()), &record).unwrap();
    }
    let pinned = book.pinned_jobs();
    assert!(pinned.contains("session-1/job-1"));
    assert!(pinned.contains("session-1/job-2"));
    assert_eq!(
        ledger::prune_keeping(
            &jobs(home.path()),
            std::time::Duration::from_secs(1),
            &pinned
        ),
        0
    );
    assert!(ledger::load(&jobs(home.path()), "session-1", "job-1").is_some());
    assert!(ledger::load(&jobs(home.path()), "session-1", "job-2").is_some());
    book.stop(THREAD).unwrap();
    assert_eq!(
        ledger::prune_keeping(
            &jobs(home.path()),
            std::time::Duration::from_secs(1),
            &book.pinned_jobs()
        ),
        2
    );
}

#[test]
fn hand_back_does_not_rewind_the_cursor() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    book.decide(THREAD, "m1", "first", Worker::Accept).unwrap();
    book.commit_delivery(THREAD, "m1").unwrap();
    book.decide(THREAD, "m2", "second", Worker::Accept).unwrap();
    book.commit_delivery(THREAD, "m2").unwrap();
    book.hand_back(THREAD, "m1", "not this").unwrap();
    assert_eq!(
        book.subscription(THREAD).unwrap().cursor.as_deref(),
        Some("m2")
    );
    let plans = book
        .accept_replay(
            THREAD,
            &[("m1", "first"), ("m2", "second"), ("m3", "third")],
            Worker::Accept,
        )
        .unwrap();
    assert!(matches!(plans[0], Plan::HandedBack(_)));
    assert!(matches!(plans[1], Plan::Settled));
    assert!(matches!(plans[2], Plan::Deliver(_)));
}

#[test]
fn an_unreadable_history_does_not_deliver_an_old_id() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    for index in 0..65 {
        let id = format!("m{index}");
        book.decide(THREAD, &id, "again", Worker::Accept).unwrap();
        book.commit_delivery(THREAD, &id).unwrap();
    }
    let id = book.subscription(THREAD).unwrap().id.clone();
    std::fs::write(
        home.path()
            .join("subscriptions")
            .join(format!("{id}.history.json")),
        b"not json",
    )
    .unwrap();
    let mut book = open(home.path());
    let old = book.decide(THREAD, "m0", "again", Worker::Accept);
    assert!(
        matches!(old, Err(Error::Unreadable)),
        "an id outside the window is not delivered, got {old:?}"
    );
    assert!(matches!(
        book.decide(THREAD, "m64", "again", Worker::Accept).unwrap(),
        Plan::Settled
    ));
    let fresh = book.decide(THREAD, "m65", "new", Worker::Accept);
    assert!(
        matches!(fresh, Err(Error::Unreadable)),
        "a new id is not delivered while history cannot be read, got {fresh:?}"
    );
}

#[test]
fn a_job_record_settles_a_pending_message_and_a_plan_does_not() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    assert!(matches!(
        book.decide(THREAD, "m1", "hello", Worker::Accept).unwrap(),
        Plan::Deliver(_)
    ));
    assert!(book.subscription(THREAD).unwrap().cursor.is_none());
    // The plan is recorded and the job has not taken it.
    let mut book = open(home.path());
    assert!(matches!(
        book.decide(THREAD, "m1", "hello", Worker::Accept).unwrap(),
        Plan::Deliver(_)
    ));
    let token = super::acceptance_token(THREAD, "m1");
    let mut accepted = ledger::load(&jobs(home.path()), "session-1", "job-1").unwrap();
    accepted.job = "job-2".into();
    accepted.accepted_follow_up = Some(token);
    accepted.resumed_from = Some("session-1/job-1".into());
    accepted.state = AgentState::Working;
    ledger::save(&jobs(home.path()), &accepted).unwrap();
    assert!(matches!(
        book.decide(THREAD, "m1", "hello", Worker::Accept).unwrap(),
        Plan::Settled
    ));
    assert_eq!(
        book.subscription(THREAD).unwrap().cursor.as_deref(),
        Some("m1")
    );
    assert_eq!(
        book.subscription(THREAD).unwrap().current.as_deref(),
        Some("session-1/job-2")
    );
    assert_eq!(
        ledger::load(&jobs(home.path()), "session-1", "job-1")
            .unwrap()
            .resumed_as
            .as_deref(),
        Some("session-1/job-2")
    );
    assert!(matches!(
        book.decide(THREAD, "m1", "hello", Worker::Accept).unwrap(),
        Plan::Settled
    ));
}

#[test]
fn a_full_history_keeps_old_ids_and_refuses_a_new_one() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    book.decide(THREAD, "m0", "first", Worker::Accept).unwrap();
    book.commit_delivery(THREAD, "m0").unwrap();
    let id = book.subscription(THREAD).unwrap().id.clone();
    let decisions: Vec<Decision> = (0..super::MAX_HISTORY)
        .map(|index| Decision {
            message_id: if index == 0 {
                "m0".into()
            } else {
                format!("id{index}")
            },
            thread: THREAD.into(),
            fanout: Fanout::Delivered,
            text: None,
            reply_to: None,
            thread_reply: None,
            account: None,
        })
        .collect();
    super::write_history(&home.path().join("subscriptions"), &id, &decisions).unwrap();
    let mut book = open(home.path());
    assert!(matches!(
        book.decide(THREAD, "m0", "first", Worker::Accept).unwrap(),
        Plan::Settled
    ));
    let fresh = book.decide(THREAD, "brand-new", "later", Worker::Accept);
    assert!(
        matches!(fresh, Err(Error::Invalid(_))),
        "a full history does not forget m0 to take a new id, got {fresh:?}"
    );
    assert!(matches!(
        book.decide(THREAD, "m0", "first", Worker::Accept).unwrap(),
        Plan::Settled
    ));
}

#[test]
fn a_stopped_or_withheld_subscription_can_be_bound_again() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    book.decide(THREAD, "m1", "first", Worker::Accept).unwrap();
    book.commit_delivery(THREAD, "m1").unwrap();
    book.decide(THREAD, "m-pending", "waiting", Worker::Accept)
        .unwrap();
    book.stop(THREAD).unwrap();
    named_job(home.path(), "job-2");
    let replaced = book
        .bind(THREAD, "session-1", "job-2", WHO, "the next job")
        .unwrap();
    assert_eq!(replaced.job, "job-2");
    assert!(!replaced.authorization.stopped);
    assert_eq!(replaced.cursor.as_deref(), Some("m1"));
    assert!(matches!(
        book.decide(THREAD, "m1", "first", Worker::Accept).unwrap(),
        Plan::Settled
    ));
    assert!(matches!(
        book.decide(THREAD, "m3", "new", Worker::Accept).unwrap(),
        Plan::Deliver(_)
    ));
    let returns = book.take_returns("feishu:default", WHO);
    assert_eq!(returns.len(), 1);
    assert_eq!(returns[0].message_id, "m-pending");
    assert_eq!(returns[0].text, "waiting");
    // A finished job is still resumed, so its thread stays taken.
    ledger::update(&jobs(home.path()), "session-1", "job-2", |record| {
        record.state = AgentState::Completed;
        record.finished_unix = Some(1);
    });
    named_job(home.path(), "job-3");
    assert!(matches!(
        book.bind(THREAD, "session-1", "job-3", WHO, WHY),
        Err(Error::ThreadTaken)
    ));
    ledger::dismiss(&jobs(home.path()), "session-1", "job-2").unwrap();
    let again = book
        .bind(THREAD, "session-1", "job-3", WHO, "after dismiss")
        .unwrap();
    assert_eq!(again.job, "job-3");
    assert!(matches!(
        book.decide(THREAD, "m1", "first", Worker::Accept).unwrap(),
        Plan::Settled
    ));
}

#[test]
fn hand_back_uses_the_job_that_accepted_the_message() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    book.decide_account(
        incoming(Some("feishu:default"), THREAD, "m5", "for job-2"),
        Worker::Accept,
    )
    .unwrap();
    book.commit_delivery(THREAD, "m5").unwrap();
    let token = super::acceptance_token(THREAD, "m5");
    named_job(home.path(), "job-2");
    ledger::update(&jobs(home.path()), "session-1", "job-2", |record| {
        record.accepted_follow_up = Some(token);
        record.resumed_from = Some("session-1/job-1".into());
    });
    ledger::update(&jobs(home.path()), "session-1", "job-1", |record| {
        record.resumed_as = Some("session-1/job-2".into());
        record.state = AgentState::Completed;
        record.finished_unix = Some(1);
    });
    book.note_current(THREAD, "session-1/job-2").unwrap();
    book.decide_account(
        incoming(Some("feishu:default"), THREAD, "m6", "for job-3"),
        Worker::Accept,
    )
    .unwrap();
    book.commit_delivery(THREAD, "m6").unwrap();
    named_job(home.path(), "job-3");
    ledger::update(&jobs(home.path()), "session-1", "job-3", |record| {
        record.accepted_follow_up = Some(super::acceptance_token(THREAD, "m6"));
        record.resumed_from = Some("session-1/job-2".into());
    });
    ledger::update(&jobs(home.path()), "session-1", "job-2", |record| {
        record.resumed_as = Some("session-1/job-3".into());
        record.state = AgentState::Completed;
        record.finished_unix = Some(1);
    });
    book.note_current(THREAD, "session-1/job-3").unwrap();
    assert_eq!(
        book.latest_offer("session-1", "job-2"),
        Some((THREAD.to_owned(), "m5".to_owned()))
    );
    assert_eq!(
        book.latest_offer("session-1", "job-3"),
        Some((THREAD.to_owned(), "m6".to_owned()))
    );
    assert!(book.latest_offer("session-1", "job-1").is_none());
}

#[test]
fn a_return_is_taken_only_by_the_account_that_received_it() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    book.decide_account(
        incoming(Some("feishu:default"), THREAD, "m1", "not this"),
        Worker::Accept,
    )
    .unwrap();
    book.hand_back(THREAD, "m1", "not this").unwrap();
    assert_eq!(book.take_returns("slack:default", WHO), []);
    assert_eq!(book.take_returns("feishu:mail", WHO), []);
    let taken = book.take_returns("feishu:default", "someone-else");
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].text, "not this");
    // Still queued until the bridge has saved the claim.
    assert_eq!(book.take_returns("feishu:default", WHO).len(), 1);
    book.ack_returns(
        "feishu:default",
        WHO,
        &[(&taken[0].thread, taken[0].message_id.as_str())],
    )
    .unwrap();
    assert_eq!(book.take_returns("feishu:default", WHO), []);
}

#[test]
fn a_return_stays_on_disk_until_its_claim_is_saved() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    book.decide_account(
        incoming(Some("feishu:default"), THREAD, "m1", "not this"),
        Worker::Accept,
    )
    .unwrap();
    let plan = book.hand_back(THREAD, "m1", "not this").unwrap();
    assert!(matches!(plan, Plan::HandedBack(_)), "{plan:?}");
    drop(book);
    let book = open(home.path());
    let taken = book.take_returns("feishu:default", WHO);
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].text, "not this");
    // Looking does not drop it. A crash here offers it again.
    drop(book);
    let mut book = open(home.path());
    assert_eq!(book.take_returns("feishu:default", WHO).len(), 1);
    book.ack_returns("feishu:default", WHO, &[(THREAD, "m1")])
        .unwrap();
    drop(book);
    let mut book = open(home.path());
    assert_eq!(book.take_returns("feishu:default", WHO), []);
    assert!(matches!(
        book.decide(THREAD, "m1", "not this", Worker::Accept)
            .unwrap(),
        Plan::HandedBack(_)
    ));
}

#[test]
fn hand_back_keeps_text_after_the_offer_list_rolls_off() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    for index in 0..70 {
        let id = format!("m{index}");
        let text = format!("body {index}");
        book.decide(THREAD, &id, &text, Worker::Accept).unwrap();
        book.commit_delivery(THREAD, &id).unwrap();
    }
    assert!(book.subscription(THREAD).unwrap().offers.len() <= 64);
    assert!(
        book.subscription(THREAD)
            .unwrap()
            .offers
            .iter()
            .all(|held| held.message_id != "m0")
    );
    let plan = book.hand_back(THREAD, "m0", "").unwrap();
    let Plan::HandedBack(handed) = plan else {
        panic!("the first message can still be handed back, got {plan:?}");
    };
    assert_eq!(handed.text, "body 0");
}

#[test]
fn hand_back_after_unsubscribe_returns_a_delivered_message() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    book.decide_account(
        Incoming {
            account: Some("feishu:default"),
            thread: THREAD,
            message_id: "m1",
            text: "hello",
            reply_to: "thread:om_1",
            thread_reply: "thread:root",
        },
        Worker::Accept,
    )
    .unwrap();
    book.commit_delivery(THREAD, "m1").unwrap();
    book.stop(THREAD).unwrap();
    let plan = book.hand_back(THREAD, "m1", "").unwrap();
    assert!(
        matches!(plan, Plan::Intake),
        "a stopped subscription still returns what it delivered, got {plan:?}"
    );
    let taken = book.take_returns("feishu:default", WHO);
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].text, "hello");
    assert_eq!(taken[0].reply_to, "thread:om_1");
    assert_eq!(taken[0].thread_reply, "thread:root");
    assert!(matches!(
        book.decide(THREAD, "m2", "next", Worker::Accept).unwrap(),
        Plan::Withheld
    ));
}

#[test]
fn an_unknown_hand_back_does_not_settle_the_id() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    let error = book.hand_back(THREAD, "nope", "").unwrap_err();
    assert!(matches!(error, Error::Invalid(_)), "{error}");
    assert!(
        book.subscription(THREAD).unwrap().returns.is_empty(),
        "an unknown id queues no return"
    );
    assert!(matches!(
        book.decide(THREAD, "nope", "real", Worker::Accept).unwrap(),
        Plan::Deliver(_)
    ));
}

#[test]
fn hand_back_keeps_the_reply_handle_after_the_offer_list_rolls_off() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    book.decide_account(
        Incoming {
            account: Some("feishu:default"),
            thread: THREAD,
            message_id: "m0",
            text: "body 0",
            reply_to: "thread:om_0",
            thread_reply: "thread:root",
        },
        Worker::Accept,
    )
    .unwrap();
    book.commit_delivery(THREAD, "m0").unwrap();
    for index in 1..70 {
        let id = format!("m{index}");
        book.decide(THREAD, &id, "later", Worker::Accept).unwrap();
        book.commit_delivery(THREAD, &id).unwrap();
    }
    book.hand_back(THREAD, "m0", "").unwrap();
    let taken = book.take_returns("feishu:default", WHO);
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].text, "body 0");
    assert_eq!(taken[0].reply_to, "thread:om_0");
    assert_eq!(taken[0].thread_reply, "thread:root");
}

fn named_job(home: &Path, name: &str) {
    let ledger = JobLedger::new(&jobs(home), "session-1", "channel", Path::new("/work")).unwrap();
    let arguments = json!({"prompt": "continue", "session": "codex-1"});
    let mut record = ledger
        .record(&NewJob {
            job: name,
            agent: "codex",
            task: "Fix the flaky test",
            arguments: &arguments,
            started_by: JobOrigin::Model,
            reviewed: false,
            queued: false,
        })
        .unwrap();
    record.conversation = Some("codex-1".into());
    ledger.save(&record);
}

fn successor(home: &Path) -> String {
    let ledger = JobLedger::new(&jobs(home), "session-1", "workbench", Path::new("/work")).unwrap();
    let arguments = json!({"prompt": "please continue", "session": "codex-1"});
    let mut record = ledger
        .record(&NewJob {
            job: "job-2",
            agent: "codex",
            task: "Fix the flaky test",
            arguments: &arguments,
            started_by: JobOrigin::Owner,
            reviewed: false,
            queued: false,
        })
        .unwrap();
    record.resumed_from = Some("session-1/job-1".into());
    record.conversation = Some("codex-1".into());
    ledger.save(&record);
    "session-1/job-2".into()
}

#[test]
fn a_large_subscription_stays_readable_without_losing_pending_or_returns() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    book.decide_account(
        incoming(Some("feishu:default"), THREAD, "held", "please return this"),
        Worker::Accept,
    )
    .unwrap();
    book.hand_back(THREAD, "held", "please return this")
        .unwrap();
    let body = "x".repeat(16 * 1024);
    for index in 0..16 {
        let id = format!("m{index}");
        book.decide(THREAD, &id, &body, Worker::Accept).unwrap();
        book.commit_delivery(THREAD, &id).unwrap();
    }
    book.decide(THREAD, "pending", &body, Worker::Accept)
        .unwrap();
    let id = book.subscription(THREAD).unwrap().id.clone();
    let path = home.path().join("subscriptions").join(format!("{id}.json"));
    let bytes = std::fs::metadata(&path).unwrap().len();
    assert!(
        bytes <= super::MAX_RECORD_BYTES,
        "the record must stay within what a reload reads, got {bytes}"
    );
    let mut book = open(home.path());
    let saved = book
        .subscription(THREAD)
        .expect("reload keeps a record that fits");
    assert!(
        saved
            .decisions
            .iter()
            .any(|decision| decision.message_id == "m0" && decision.fanout == Fanout::Delivered),
        "replay protection for the oldest delivery stays on the record"
    );
    assert!(
        saved
            .pending
            .iter()
            .any(|held| held.message_id == "pending" && held.text == body),
        "a pending message is not shed to fit the record"
    );
    assert!(
        saved
            .returns
            .iter()
            .any(|held| held.message_id == "held" && held.text == "please return this"),
        "a return is not shed to fit the record"
    );
    assert!(matches!(
        book.decide(THREAD, "m0", &body, Worker::Accept).unwrap(),
        Plan::Settled
    ));
    assert!(matches!(
        book.decide(THREAD, "m15", &body, Worker::Accept).unwrap(),
        Plan::Settled
    ));
}

#[test]
fn a_full_return_queue_keeps_unclaimed_messages() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    for index in 0..super::MAX_RETURNS {
        let id = format!("m{index}");
        book.decide_account(
            incoming(Some("feishu:default"), THREAD, &id, "body"),
            Worker::Accept,
        )
        .unwrap();
        book.hand_back(THREAD, &id, "body").unwrap();
    }
    assert_eq!(
        book.subscription(THREAD).unwrap().returns.len(),
        super::MAX_RETURNS
    );
    book.decide_account(
        incoming(Some("feishu:default"), THREAD, "overflow", "one more"),
        Worker::Accept,
    )
    .unwrap();
    let error = book.hand_back(THREAD, "overflow", "one more").unwrap_err();
    assert!(
        matches!(error, Error::Invalid(_)),
        "a full queue refuses the new hand-back, got {error}"
    );
    let record = book.subscription(THREAD).unwrap();
    assert_eq!(record.returns.len(), super::MAX_RETURNS);
    assert_eq!(record.returns[0].message_id, "m0");
    assert_eq!(record.returns[0].account.as_deref(), Some("feishu:default"));
    assert!(
        record
            .returns
            .iter()
            .all(|held| held.message_id != "overflow"),
        "the refused message is not queued by discarding an older one"
    );
    assert!(
        record
            .pending
            .iter()
            .any(|held| held.message_id == "overflow")
    );
    assert_eq!(
        record
            .decisions
            .iter()
            .find(|decision| decision.message_id == "overflow")
            .map(|decision| decision.fanout),
        Some(Fanout::Pending)
    );
    let book = open(home.path());
    let saved = book.subscription(THREAD).unwrap();
    assert_eq!(saved.returns.len(), super::MAX_RETURNS);
    assert_eq!(saved.returns[0].message_id, "m0");
    assert_eq!(
        book.take_returns("feishu:default", WHO).len(),
        super::MAX_RETURNS
    );
    assert_eq!(book.take_returns("slack:other", WHO), []);
}

#[test]
fn dropping_an_extra_thread_keeps_a_settled_id() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    book.attach(THREAD, EXTRA).unwrap();
    book.decide(EXTRA, "m1", "not this job", Worker::Accept)
        .unwrap();
    book.commit_delivery(EXTRA, "m1").unwrap();
    let plan = book.hand_back(EXTRA, "m1", "not this job").unwrap();
    assert!(matches!(plan, Plan::HandedBack(_)), "{plan:?}");
    assert!(book.subscription(EXTRA).is_none());

    let mut book = open(home.path());
    let replayed = book
        .decide(EXTRA, "m1", "not this job", Worker::Accept)
        .unwrap();
    assert!(
        matches!(replayed, Plan::HandedBack(_)),
        "replaying the settled id stays handed back, got {replayed:?}"
    );
    assert!(matches!(
        book.decide(EXTRA, "m2", "a new message", Worker::Accept)
            .unwrap(),
        Plan::Intake
    ));

    named_job(home.path(), "job-2");
    book.bind(EXTRA, "session-1", "job-2", WHO, WHY).unwrap();
    let again = book
        .decide(EXTRA, "m1", "not this job", Worker::Accept)
        .unwrap();
    assert!(
        matches!(again, Plan::HandedBack(_)),
        "binding the thread again does not deliver the settled id, got {again:?}"
    );
    assert!(matches!(
        book.decide(EXTRA, "m3", "fresh work", Worker::Accept)
            .unwrap(),
        Plan::Deliver(_)
    ));
}

#[test]
fn a_hand_back_after_the_offer_list_rolls_off_keeps_the_account() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    book.decide_account(
        Incoming {
            account: Some("feishu:default"),
            thread: THREAD,
            message_id: "m0",
            text: "body 0",
            reply_to: "thread:om_0",
            thread_reply: "thread:root",
        },
        Worker::Accept,
    )
    .unwrap();
    book.commit_delivery(THREAD, "m0").unwrap();
    for index in 1..70 {
        let id = format!("m{index}");
        book.decide(THREAD, &id, "later", Worker::Accept).unwrap();
        book.commit_delivery(THREAD, &id).unwrap();
    }
    let mut book = open(home.path());
    assert!(
        book.subscription(THREAD)
            .unwrap()
            .offers
            .iter()
            .all(|held| held.message_id != "m0")
    );
    book.hand_back(THREAD, "m0", "").unwrap();
    assert_eq!(book.take_returns("slack:other", WHO), []);
    let taken = book.take_returns("feishu:default", "someone-else");
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].account.as_deref(), Some("feishu:default"));
    assert_eq!(taken[0].text, "body 0");
    assert_eq!(taken[0].reply_to, "thread:om_0");
    assert_eq!(taken[0].thread_reply, "thread:root");
}

fn deliver_with_handles(book: &mut Subscriptions, message_id: &str, text: &str) {
    book.decide_account(
        Incoming {
            account: Some("feishu:default"),
            thread: THREAD,
            message_id,
            text,
            reply_to: "thread:om_0",
            thread_reply: "thread:root",
        },
        Worker::Accept,
    )
    .unwrap();
    book.commit_delivery(THREAD, message_id).unwrap();
}

#[test]
fn a_missing_sidecar_keeps_hand_back_provenance_after_offers_roll_off() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    deliver_with_handles(&mut book, "m0", "body 0");
    let id = book.subscription(THREAD).unwrap().id.clone();
    drop(book);
    let history = home
        .path()
        .join("subscriptions")
        .join(format!("{id}.history.json"));
    std::fs::remove_file(&history).unwrap();
    let mut book = open(home.path());
    for index in 1..71 {
        let message_id = format!("m{index}");
        book.decide(THREAD, &message_id, "later", Worker::Accept)
            .unwrap();
        book.commit_delivery(THREAD, &message_id).unwrap();
    }
    drop(book);
    let mut book = open(home.path());
    assert!(
        book.subscription(THREAD)
            .unwrap()
            .offers
            .iter()
            .all(|held| held.message_id != "m0")
    );
    book.hand_back(THREAD, "m0", "").unwrap();
    assert_eq!(
        book.take_returns("slack:other", WHO),
        [],
        "another account of the same owner must not run the return"
    );
    let taken = book.take_returns("feishu:default", "someone-else");
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].account.as_deref(), Some("feishu:default"));
    assert_eq!(taken[0].text, "body 0");
    assert_eq!(taken[0].reply_to, "thread:om_0");
    assert_eq!(taken[0].thread_reply, "thread:root");
}

#[test]
fn a_sidecar_ahead_of_the_record_is_not_delivered_again() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    book.decide_account(
        Incoming {
            account: Some("feishu:default"),
            thread: THREAD,
            message_id: "m0",
            text: "body 0",
            reply_to: "thread:om_0",
            thread_reply: "thread:root",
        },
        Worker::Accept,
    )
    .unwrap();
    let id = book.subscription(THREAD).unwrap().id.clone();
    let record_path = home.path().join("subscriptions").join(format!("{id}.json"));
    let before_delivery = std::fs::read(&record_path).unwrap();
    book.commit_delivery(THREAD, "m0").unwrap();
    std::fs::write(&record_path, before_delivery).unwrap();
    drop(book);
    let mut book = open(home.path());
    assert!(
        matches!(
            book.decide(THREAD, "m0", "body 0", Worker::Accept).unwrap(),
            Plan::Settled
        ),
        "a delivery that reached the sidecar is not given again"
    );
    assert!(
        book.pending_messages()
            .iter()
            .all(|held| held.message_id != "m0")
    );
    book.hand_back(THREAD, "m0", "").unwrap();
    assert_eq!(book.take_returns("slack:other", WHO), []);
    let taken = book.take_returns("feishu:default", "someone-else");
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].account.as_deref(), Some("feishu:default"));
    assert_eq!(taken[0].text, "body 0");
    assert_eq!(taken[0].reply_to, "thread:om_0");
    assert_eq!(taken[0].thread_reply, "thread:root");
}

#[test]
fn a_hand_back_older_than_the_window_survives_a_crash_before_the_sidecar() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    for index in 0..70 {
        deliver_with_handles(&mut book, &format!("m{index}"), &format!("body {index}"));
    }
    assert!(
        book.subscription(THREAD)
            .unwrap()
            .decisions
            .iter()
            .all(|decision| decision.message_id != "m0"),
        "m0 has rolled out of the record's window"
    );
    let id = book.subscription(THREAD).unwrap().id.clone();
    let history = home
        .path()
        .join("subscriptions")
        .join(format!("{id}.history.json"));
    let delivered = std::fs::read(&history).unwrap();
    let plan = book.hand_back(THREAD, "m0", "").unwrap();
    assert!(matches!(plan, Plan::HandedBack(_)), "{plan:?}");
    // The process stops after the record write and before the sidecar's,
    // which still says m0 was delivered.
    std::fs::write(&history, delivered).unwrap();
    drop(book);
    let mut book = open(home.path());
    let taken = book.take_returns("feishu:default", WHO);
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].message_id, "m0");
    assert_eq!(taken[0].text, "body 0");
    book.ack_returns("feishu:default", WHO, &[(THREAD, "m0")])
        .unwrap();
    assert!(matches!(
        book.hand_back(THREAD, "m0", "").unwrap(),
        Plan::HandedBack(_)
    ));
    assert!(matches!(
        book.decide(THREAD, "m0", "body 0", Worker::Accept).unwrap(),
        Plan::HandedBack(_)
    ));
    assert_eq!(
        book.take_returns("feishu:default", WHO),
        [],
        "a return already run is not queued again"
    );
    drop(book);
    let book = open(home.path());
    assert_eq!(book.take_returns("feishu:default", WHO), []);
}

#[test]
fn a_return_to_intake_older_than_the_window_survives_a_crash_before_the_sidecar() {
    let home = tempfile::tempdir().unwrap();
    job(home.path());
    let mut book = open(home.path());
    bind(&mut book);
    book.decide_account(
        incoming(Some("feishu:default"), THREAD, "m0", "body 0"),
        Worker::Accept,
    )
    .unwrap();
    for index in 1..71 {
        deliver_with_handles(&mut book, &format!("m{index}"), "later");
    }
    book.stop(THREAD).unwrap();
    let id = book.subscription(THREAD).unwrap().id.clone();
    let history = home
        .path()
        .join("subscriptions")
        .join(format!("{id}.history.json"));
    let pending = std::fs::read(&history).unwrap();
    let plan = book
        .decide_account(
            incoming(Some("feishu:default"), THREAD, "m0", "body 0"),
            Worker::Accept,
        )
        .unwrap();
    assert_eq!(plan, Plan::Intake);
    std::fs::write(&history, pending).unwrap();
    drop(book);
    let mut book = open(home.path());
    let taken = book.take_returns("feishu:default", WHO);
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].message_id, "m0");
    book.ack_returns("feishu:default", WHO, &[(THREAD, "m0")])
        .unwrap();
    assert_eq!(
        book.decide_account(
            incoming(Some("feishu:default"), THREAD, "m0", "body 0"),
            Worker::Accept,
        )
        .unwrap(),
        Plan::Intake
    );
    assert_eq!(
        book.take_returns("feishu:default", WHO),
        [],
        "a message already returned to intake is not returned again"
    );
}

/// Let `writes` more record and history writes land on this thread, then
/// fail every later one, as if the process stopped there. `None` lifts it.
fn stop_after(writes: Option<usize>) {
    super::WRITES_LEFT.set(writes);
}

#[test]
fn a_recovered_hand_back_survives_a_failed_repair_and_a_later_crash() {
    // The process can stop after any write of the decision that follows.
    for writes in 0..4 {
        let home = tempfile::tempdir().unwrap();
        job(home.path());
        let mut book = open(home.path());
        bind(&mut book);
        for index in 0..70 {
            deliver_with_handles(&mut book, &format!("m{index}"), &format!("body {index}"));
        }
        let id = book.subscription(THREAD).unwrap().id.clone();
        let history = home
            .path()
            .join("subscriptions")
            .join(format!("{id}.history.json"));
        let delivered = std::fs::read(&history).unwrap();
        book.hand_back(THREAD, "m0", "").unwrap();
        // Stopped after the record write, before the sidecar's.
        std::fs::write(&history, &delivered).unwrap();
        drop(book);
        // The reload recovers m0 from the record and cannot write it out.
        stop_after(Some(0));
        let mut book = open(home.path());
        stop_after(None);
        assert_eq!(
            std::fs::read(&history).unwrap(),
            delivered,
            "the sidecar still says m0 was delivered"
        );
        let taken = book.take_returns("feishu:default", WHO);
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].message_id, "m0");
        book.ack_returns("feishu:default", WHO, &[(THREAD, "m0")])
            .unwrap();
        // A new message moves the record's window past m0.
        stop_after(Some(writes));
        let _ = book.decide(THREAD, "m70", "later", Worker::Accept);
        stop_after(None);
        drop(book);
        let mut book = open(home.path());
        assert!(
            matches!(
                book.hand_back(THREAD, "m0", "").unwrap(),
                Plan::HandedBack(_)
            ),
            "stopped after {writes} writes"
        );
        assert_eq!(
            book.take_returns("feishu:default", WHO),
            [],
            "stopped after {writes} writes: a return already run is not queued again"
        );
        assert!(matches!(
            book.decide(THREAD, "m0", "body 0", Worker::Accept).unwrap(),
            Plan::HandedBack(_)
        ));
    }
}
