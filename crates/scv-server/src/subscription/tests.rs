//! Unit tests for `src/subscription.rs`.

use std::path::{Path, PathBuf};

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

use scv_channels::{ChannelKind, FollowUp, TaskFollowUps};
use scv_client::Layout;
use scv_protocol::{AgentState, JobOrigin, RemoteTools};
use scv_tools::background::ledger::{self, JobRecord};
use scv_tools::background::subscription::{Subscriptions, acceptance_token};
use scv_tools::delegation::{DelegationRegistry, ProcessIdentity};
use serde_json::json;

use super::FollowUps;

fn record(home: &Path) {
    let owner = ProcessIdentity::current().expect("this process has a start time");
    let job = JobRecord {
        v: 1,
        session: "session-1".into(),
        job: "job-1".into(),
        agent: "codex".into(),
        task: "Fix the flaky test".into(),
        prompt: "Fix the flaky test".into(),
        arguments: json!({"session": "codex-1"}),
        workspace: PathBuf::from("/work"),
        label: "channel".into(),
        started_by: JobOrigin::Model,
        reviewed: false,
        owner,
        state: AgentState::Working,
        created_unix: 1,
        updated_unix: 1,
        finished_unix: None,
        conversation: Some("codex-1".into()),
        worktree: None,
        reply: None,
        error: None,
        mode: None,
        reported: false,
        interrupted: None,
        resumed_as: None,
        resumed_from: None,
        accepted_follow_up: None,
        dismissed: false,
    };
    ledger::save(&Layout::new(home).jobs(), &job).expect("record the job");
}

#[tokio::test]
async fn a_reload_does_not_redeliver_or_resume() {
    let home = tempfile::tempdir().expect("temp dir");
    record(home.path());
    let layout = Layout::new(home.path());
    Subscriptions::open(&layout.subscriptions(), &layout.jobs())
        .bind(
            "owner\0thread\0omt_1",
            "session-1",
            "job-1",
            "owner",
            "because",
        )
        .expect("bind the job");
    let follow = FollowUps::open(&layout);
    // No workbench is attached, so the delivery cannot be accepted. The
    // message stays pending: a reload does not settle it and does not resume
    // the job.
    assert_eq!(
        follow
            .follow_up(
                "feishu:default",
                "owner\0thread\0omt_1",
                "m1",
                "hello",
                "",
                ""
            )
            .await,
        FollowUp::Held
    );
    ledger::update(&layout.jobs(), "session-1", "job-1", |job| {
        job.state = AgentState::Interrupted;
    });
    follow.reload();
    assert!(
        ledger::load(&layout.jobs(), "session-1", "job-1")
            .expect("job")
            .resumed_as
            .is_none()
    );
    assert_eq!(
        follow
            .follow_up(
                "feishu:default",
                "owner\0thread\0omt_1",
                "m1",
                "hello",
                "",
                ""
            )
            .await,
        FollowUp::Held
    );
    assert!(
        ledger::load(&layout.jobs(), "session-1", "job-1")
            .expect("job")
            .resumed_as
            .is_none()
    );
}

#[test]
fn the_daemon_binds_an_existing_job_as_the_session_owner() {
    let home = tempfile::tempdir().expect("temp dir");
    record(home.path());
    let layout = Layout::new(home.path());
    let follow = FollowUps::open(&layout);
    let error = follow
        .bind_owner("owner\0thread\0omt_1", "missing", "job-9", "because")
        .expect_err("a missing job is not bound");
    assert!(error.contains("no job"), "{error}");
    let bound = follow
        .bind_owner(
            "owner\0thread\0omt_1",
            "session-1",
            "job-1",
            "follow this job",
        )
        .expect("bind");
    assert_ne!(bound.authorization.who, "");
    assert_eq!(
        bound.authorization.scope,
        scv_tools::background::subscription::DELIVER_FOLLOW_UPS
    );
    assert!(
        follow
            .bind_owner("owner\0thread\0omt_1", "session-1", "job-1", "again")
            .is_err()
    );
}

#[tokio::test]
async fn a_handed_back_message_stays_settled() {
    let home = tempfile::tempdir().expect("temp dir");
    record(home.path());
    let layout = Layout::new(home.path());
    let follow = FollowUps::open(&layout);
    let thread = "owner\0thread\0omt_1";
    follow
        .bind_owner(thread, "session-1", "job-1", "because")
        .expect("bind");
    assert_eq!(
        follow
            .follow_up("feishu:default", thread, "m1", "not this job", "", "")
            .await,
        FollowUp::Held
    );
    let registry = DelegationRegistry::new(&layout);
    follow
        .hand_back_message(&registry, Some(thread), Some("m1"), None)
        .expect("hand back");
    assert_eq!(
        follow
            .follow_up("feishu:default", thread, "m1", "not this job", "", "")
            .await,
        FollowUp::Settled
    );
    let returns = follow.take_returns("feishu:default", "owner").await;
    assert_eq!(returns.len(), 1);
    assert_eq!(returns[0].message_id, "m1");
    assert_eq!(returns[0].text, "not this job");
}

#[tokio::test]
async fn an_accepted_job_is_not_sent_again_and_a_plan_is_not_acceptance() {
    let home = tempfile::tempdir().expect("temp dir");
    record(home.path());
    let layout = Layout::new(home.path());
    let follow = FollowUps::open(&layout);
    let thread = "owner\0thread\0omt_1";
    follow
        .bind_owner(thread, "session-1", "job-1", "because")
        .expect("bind");
    // No workbench, so nothing accepts the text. The plan stays pending.
    assert_eq!(
        follow
            .follow_up("feishu:default", thread, "m1", "hello", "", "")
            .await,
        FollowUp::Held
    );
    assert!(
        ledger::load(&layout.jobs(), "session-1", "job-1")
            .expect("job")
            .resumed_as
            .is_none()
    );
    let mut accepted = ledger::load(&layout.jobs(), "session-1", "job-1").expect("job");
    accepted.job = "job-2".into();
    accepted.accepted_follow_up = Some(acceptance_token(thread, "m1"));
    accepted.resumed_from = Some("session-1/job-1".into());
    ledger::save(&layout.jobs(), &accepted).expect("record the acceptance");
    assert_eq!(
        follow
            .follow_up("feishu:default", thread, "m1", "hello", "", "")
            .await,
        FollowUp::Settled
    );
    assert_eq!(
        ledger::load(&layout.jobs(), "session-1", "job-1")
            .expect("job")
            .resumed_as
            .as_deref(),
        Some("session-1/job-2")
    );
    assert_eq!(
        follow
            .follow_up("feishu:default", thread, "m1", "hello", "", "")
            .await,
        FollowUp::Settled
    );
}

#[tokio::test]
async fn an_unbound_thread_is_an_ordinary_turn() {
    let home = tempfile::tempdir().expect("temp dir");
    let follow = FollowUps::open(&Layout::new(home.path()));
    assert_eq!(
        follow
            .follow_up("feishu:default", "owner", "m1", "hello", "", "")
            .await,
        FollowUp::Turn
    );
    assert_eq!(
        follow
            .follow_up(
                "feishu:default",
                "owner\0thread\0omt_1",
                "m2",
                "there",
                "",
                ""
            )
            .await,
        FollowUp::Turn
    );
}

#[tokio::test]
async fn an_unreadable_history_keeps_the_message_for_retry() {
    let home = tempfile::tempdir().expect("temp dir");
    record(home.path());
    let layout = Layout::new(home.path());
    let thread = "owner\0thread\0omt_1";
    let mut book = Subscriptions::open(&layout.subscriptions(), &layout.jobs());
    book.bind(thread, "session-1", "job-1", "owner", "because")
        .expect("bind");
    let id = book.subscription(thread).expect("subscription").id.clone();
    std::fs::write(
        layout.subscriptions().join(format!("{id}.history.json")),
        b"not json",
    )
    .expect("break the history");
    let follow = FollowUps::open(&layout);
    assert_eq!(
        follow
            .follow_up("feishu:default", thread, "m1", "hello", "", "")
            .await,
        FollowUp::Held
    );
    let waiting = follow.unrecorded();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].1, "m1");
    assert_eq!(waiting[0].2, "hello");
    assert_eq!(
        Subscriptions::open(&layout.subscriptions(), &layout.jobs()).pending_messages(),
        []
    );
    assert_eq!(
        follow
            .follow_up("feishu:default", thread, "m1", "hello", "", "")
            .await,
        FollowUp::Held
    );
    assert_eq!(follow.unrecorded().len(), 1);
}

/// The owner's Feishu account, so a retry can read its current tool grant.
fn sign_in_owner(layout: &Layout) {
    let dir = layout.channel_credentials("feishu");
    std::fs::create_dir_all(&dir).expect("credentials dir");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dir.join("default.json"))
        .expect("credentials");
    file.write_all(
        br#"{"app_id":"cli_a1b2c3d4e5f60718","app_secret":"app-secret","brand":"feishu","owner_open_id":"ou_owner"}"#,
    )
    .expect("write credentials");
}

fn set_tools(layout: &Layout, tools: RemoteTools) {
    let settings = scv_channels::state::AccountSettings {
        remote_tools: tools,
        ..scv_channels::state::AccountSettings::default()
    };
    ChannelKind::Feishu
        .accounts(layout)
        .save_settings("default", &settings)
        .expect("save the tool grant");
}

/// A successor that already accepted `thread`'s `m1`, so a retry that is
/// allowed to run settles the pending message without a workbench.
fn record_acceptance(layout: &Layout, thread: &str) {
    let mut accepted = ledger::load(&layout.jobs(), "session-1", "job-1").expect("job");
    accepted.job = "job-2".into();
    accepted.accepted_follow_up = Some(acceptance_token(thread, "m1"));
    accepted.resumed_from = Some("session-1/job-1".into());
    ledger::save(&layout.jobs(), &accepted).expect("record the acceptance");
}

fn pending_ids(layout: &Layout) -> Vec<String> {
    Subscriptions::open(&layout.subscriptions(), &layout.jobs())
        .pending_messages()
        .into_iter()
        .map(|held| held.message_id)
        .collect()
}

#[tokio::test]
async fn a_retried_follow_up_waits_until_the_account_grants_tools() {
    let home = tempfile::tempdir().expect("temp dir");
    record(home.path());
    let layout = Layout::new(home.path());
    sign_in_owner(&layout);
    set_tools(&layout, RemoteTools::Owner);
    let thread = "ou_owner\0thread\0omt_1";
    let follow = FollowUps::open(&layout);
    follow
        .bind_owner(thread, "session-1", "job-1", "because")
        .expect("bind");
    assert_eq!(
        follow
            .follow_up("feishu:default", thread, "m1", "hello", "", "")
            .await,
        FollowUp::Held
    );
    record_acceptance(&layout, thread);

    set_tools(&layout, RemoteTools::None);
    follow.retry_pending().await;
    assert_eq!(pending_ids(&layout), ["m1"]);
    assert!(
        ledger::load(&layout.jobs(), "session-1", "job-1")
            .expect("job")
            .resumed_as
            .is_none(),
        "remote_tools = none does not give the held message to the job"
    );

    set_tools(&layout, RemoteTools::Owner);
    follow.retry_pending().await;
    assert_eq!(pending_ids(&layout), Vec::<String>::new());
    assert_eq!(
        ledger::load(&layout.jobs(), "session-1", "job-1")
            .expect("job")
            .resumed_as
            .as_deref(),
        Some("session-1/job-2")
    );
}

#[tokio::test]
async fn a_retried_follow_up_needs_the_owners_chat() {
    let home = tempfile::tempdir().expect("temp dir");
    record(home.path());
    let layout = Layout::new(home.path());
    sign_in_owner(&layout);
    set_tools(&layout, RemoteTools::Owner);
    // A group key is not the owner's direct chat, even when it names them.
    let thread = "oc_group\0ou_owner";
    let follow = FollowUps::open(&layout);
    follow
        .bind_owner(thread, "session-1", "job-1", "because")
        .expect("bind");
    assert_eq!(
        follow
            .follow_up("feishu:default", thread, "m1", "hello", "", "")
            .await,
        FollowUp::Held
    );
    record_acceptance(&layout, thread);
    follow.retry_pending().await;
    assert_eq!(pending_ids(&layout), ["m1"]);
    assert!(
        ledger::load(&layout.jobs(), "session-1", "job-1")
            .expect("job")
            .resumed_as
            .is_none(),
        "a chat that is not the granted owner is not given to the job"
    );
}
