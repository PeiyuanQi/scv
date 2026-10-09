//! Unit tests for `src/delegate/background/owed.rs`.

use super::*;

fn outcome(pending: bool, incomplete: bool) -> JobOutcome {
    let mut outcome: JobOutcome = serde_json::from_value(serde_json::json!({
        "job":"job-1",
        "review":{"outcome":"stopped","reason":"cancelled","round":1,"rounds":3,
                  "journal":"rev-1-abcdef"},
        "landing":{"mode":"none","status":"not_requested"}
    }))
    .unwrap();
    outcome.review.journal_pending = pending;
    outcome.review.journal_incomplete = incomplete;
    outcome
}

fn change() -> JobChange {
    serde_json::from_value(serde_json::json!({
        "job":"job-1","tool":"agent","agent":"claude","status":"cancelled"
    }))
    .unwrap()
}

#[test]
fn a_change_sent_after_the_job_stopped_says_the_final_outcome_and_ends_the_debt() {
    let mut owed = Owed::default();
    owed.cancelling("job-1", "cancel");
    assert_eq!(
        owed.waited("job-1", "cancel", Some(outcome(true, false))),
        Some(outcome(true, false))
    );
    // It stops before the change goes out: the change carries the end.
    assert!(!owed.stopped("job-1", &outcome(false, true)));
    let mut sent = change();
    owed.take("cancel", &mut sent);
    assert_eq!(sent.outcome, Some(outcome(false, true)));
    assert!(owed.take_updates().is_empty());
    assert!(!owed.sent("cancel", true));
    assert!(owed.take_updates().is_empty());
    assert!(owed.owed.is_empty());
}

#[test]
fn a_pending_change_is_followed_by_one_update_once_the_job_stops() {
    let mut owed = Owed::default();
    owed.cancelling("job-1", "cancel");
    owed.waited("job-1", "cancel", Some(outcome(true, false)));
    let mut sent = change();
    owed.take("cancel", &mut sent);
    assert_eq!(sent.outcome, Some(outcome(true, false)));
    // Stopping while the change is being sent queues nothing yet.
    assert!(!owed.stopped("job-1", &outcome(false, false)));
    assert!(owed.take_updates().is_empty());
    assert!(owed.sent("cancel", true), "now due");
    assert_eq!(owed.take_updates(), [outcome(false, false)]);
    // Taken once: being sent, not taken again.
    assert!(owed.take_updates().is_empty());
    owed.updated(&["job-1".to_owned()], true);
    assert!(owed.owed.is_empty());
}

#[test]
fn whatever_did_not_reach_the_client_is_owed_by_an_update() {
    // A change that failed to go out.
    let mut owed = Owed::default();
    owed.cancelling("job-1", "cancel");
    owed.waited("job-1", "cancel", None);
    owed.stopped("job-1", &outcome(false, true));
    let mut sent = change();
    owed.take("cancel", &mut sent);
    assert!(owed.sent("cancel", false));
    assert_eq!(owed.take_updates(), [outcome(false, true)]);
    // An update that failed to go out stays owed.
    owed.updated(&["job-1".to_owned()], false);
    assert_eq!(owed.take_updates(), [outcome(false, true)]);
    // A cancel whose call was dropped mid-wait: due once the job stops.
    let mut owed = Owed::default();
    owed.cancelling("job-1", "cancel");
    assert!(!owed.abandoned("job-1", "cancel"));
    assert!(owed.take_updates().is_empty());
    assert!(owed.stopped("job-1", &outcome(false, false)));
    assert_eq!(owed.take_updates(), [outcome(false, false)]);
    // A change pushed out unsent, or left by a turn that ended.
    for leave in [true, false] {
        let mut owed = Owed::default();
        owed.cancelling("job-1", "cancel");
        owed.waited("job-1", "cancel", Some(outcome(true, false)));
        owed.stopped("job-1", &outcome(false, false));
        if leave {
            owed.evicted("cancel", "job-1");
            assert!(owed.turn_ended());
        } else {
            let mut sent = change();
            owed.take("cancel", &mut sent);
            assert!(owed.turn_ended(), "never sent, the turn being over");
        }
        assert_eq!(owed.take_updates(), [outcome(false, false)]);
    }
}

#[test]
fn other_calls_and_jobs_are_left_alone() {
    let mut owed = Owed::default();
    owed.cancelling("job-1", "cancel");
    owed.waited("job-1", "cancel", Some(outcome(true, false)));
    let mut other = change();
    other.job = "job-2".into();
    owed.take("cancel", &mut other);
    assert_eq!(other.outcome, None);
    let mut sent = change();
    owed.take("another-call", &mut sent);
    assert_eq!(sent.outcome, None);
    assert!(!owed.sent("another-call", true));
    owed.updated(&["job-1".to_owned()], true);
    assert_eq!(owed.owed.len(), 1, "it was never being updated");
}
