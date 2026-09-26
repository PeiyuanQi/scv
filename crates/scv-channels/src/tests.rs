//! Unit tests for `src/lib.rs`.

use super::*;
use std::path::Path;

mod bridge;

/// Credentials that bind nothing, for state-level tests.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Test;

impl state::Credentials for Test {
    fn fingerprint(&self) -> Result<String> {
        Ok("test".into())
    }
}

fn test_store(directory: &Path) -> state::Store<Test> {
    state::Store::new(&scv_client::Layout::new(directory), "test")
}

#[test]
fn chunks_on_utf8_boundaries() {
    let chunks = split_utf8("a🙂b", 4);
    assert_eq!(chunks, vec!["a", "🙂", "b"]);
}

#[test]
fn chunks_make_progress_below_codepoint_size() {
    assert_eq!(split_utf8("🙂", 1), vec!["🙂"]);
    assert_eq!(split_utf8("🙂", 0), vec!["🙂"]);
}

#[test]
fn interrupted_recovery_is_durable_and_preserves_retry_identity() {
    let directory = tempfile::tempdir().unwrap();
    let store = test_store(directory.path());
    let original = state::BridgeState {
        in_flight: vec![state::InFlight {
            message_id: "incoming".into(),
            to_user_id: "sender".into(),
            context_token: "context".into(),
            key: String::new(),
        }],
        ..Default::default()
    };
    store.save_state("default", &original).unwrap();
    let mut recovered = store.load_state("default").unwrap();
    recover_interrupted(&store, "default", &mut recovered).unwrap();
    let mut restarted = store.load_state("default").unwrap();
    assert!(restarted.in_flight.is_empty());
    assert!(restarted.seen.is_empty());
    let pending = restarted.pending[0].clone();
    assert_eq!(pending.message_id, "incoming");
    assert_eq!(pending.to_user_id, "sender");
    assert_eq!(pending.context_token, "context");
    assert_eq!(pending.reply, FAILURE_REPLY);
    assert_eq!(pending.client_ids.len(), 1);
    recover_interrupted(&store, "default", &mut restarted).unwrap();
    assert_eq!(restarted.pending.len(), 1);
    assert_eq!(restarted.pending[0].client_ids, pending.client_ids);
}

#[test]
fn dedup_eviction_keeps_newest_entries() {
    let mut state = state::BridgeState {
        seen: (0..4096).map(|i| i.to_string()).collect(),
        ..Default::default()
    };
    mark_seen(&mut state, "newest");
    assert_eq!(state.seen.len(), 4096);
    assert_eq!(state.seen.first().unwrap(), "1");
    assert_eq!(state.seen.last().unwrap(), "newest");
    mark_seen(&mut state, "newest");
    assert_eq!(state.seen.len(), 4096);
}

#[test]
fn already_seen_batch_ids_survive_until_cursor_commit() {
    let mut state = state::BridgeState {
        seen: (0..4096).map(|i| i.to_string()).collect(),
        ..Default::default()
    };
    mark_seen(&mut state, "0");
    for index in 0..4095 {
        mark_seen(&mut state, &format!("batch-{index}"));
    }
    assert_eq!(state.seen.len(), 4096);
    assert_eq!(state.seen.first().unwrap(), "0");
}

#[test]
fn held_replies_are_bounded_per_conversation_and_expire() {
    let day = 24 * 60 * 60;
    let now = 30 * day;
    let held = |key: &str, reply: String, held_at: u64| state::HeldReply {
        key: key.into(),
        to_user_id: "u".into(),
        reply,
        held_at,
    };
    let mut state = state::BridgeState::default();
    state.held.push(held("a", "expired".into(), now - 7 * day));
    for index in 0..6 {
        state
            .held
            .push(held("a", format!("a{index}"), now - 6 * day + index));
    }
    state.held.push(held("b", "b0".into(), now));
    prune_held(&mut state, now);
    let kept: Vec<_> = state.held.iter().map(|h| h.reply.as_str()).collect();
    assert_eq!(kept, ["a2", "a3", "a4", "a5", "b0"]);

    // Bytes per conversation: the newest replies that fit are kept.
    let mut state = state::BridgeState::default();
    // Three of these exceed the per-conversation byte budget.
    let big = "x".repeat(HELD_MAX_BYTES_PER_CONVERSATION * 3 / 8);
    for index in 0..3 {
        state.held.push(held("a", big.clone(), now + index));
    }
    state.held.push(held("a", "small".into(), now + 3));
    prune_held(&mut state, now);
    assert_eq!(state.held.len(), 3);
    assert_eq!(state.held.last().unwrap().reply, "small");

    // The overall count keeps the newest across conversations.
    let mut state = state::BridgeState::default();
    for index in 0..(HELD_MAX_TOTAL as u64 + 5) {
        state
            .held
            .push(held(&format!("k{index}"), "r".into(), now + index));
    }
    prune_held(&mut state, now);
    assert_eq!(state.held.len(), HELD_MAX_TOTAL);
    assert_eq!(state.held[0].key, "k5");

    // Long refused replies are shortened so they fit beside a new reply.
    let long = "🙂".repeat(MAX_REPLY_BYTES);
    let truncated = truncate_held(long);
    assert!(truncated.len() <= MAX_HELD_REPLY_BYTES);
    assert!(truncated.ends_with("[truncated]"));
}

#[test]
fn only_held_replies_that_fit_ride_along_and_refusal_restores_them() {
    let claim = state::InFlight {
        message_id: "m".into(),
        to_user_id: "u".into(),
        context_token: "c".into(),
        key: String::new(),
    };
    let held = |key: &str, reply: &str, held_at: u64| state::HeldReply {
        key: key.into(),
        to_user_id: "u".into(),
        reply: reply.into(),
        held_at,
    };
    let now = unix_now();
    let mut state = state::BridgeState {
        held: vec![
            held("u", "one", now - 2),
            held("other", "x", now - 1),
            held("u", "two", now),
        ],
        ..Default::default()
    };
    let pending = compose_pending(&mut state, &claim, "new", now);
    assert_eq!(
        pending.reply,
        format!("{HELD_HEADER}one\n\n{HELD_HEADER}two\n\n{LATEST_HEADER}new")
    );
    assert_eq!(pending.own.as_deref(), Some("new"));
    assert_eq!(state.held, [held("other", "x", now - 1)]);

    // Refused before any chunk arrived: carried replies return ahead of it.
    let chunks = split_utf8(&pending.reply, MAX_REPLY_BYTES);
    hold_refused(&mut state, &pending, &chunks, now + 1);
    let order: Vec<_> = state.held.iter().map(|h| h.reply.as_str()).collect();
    assert_eq!(order, ["one", "x", "two", "new"]);

    // A new reply too large to share a message leaves held replies waiting.
    let own = "y".repeat(MAX_REPLY_BYTES - LATEST_HEADER.len());
    let pending = compose_pending(&mut state, &claim, &own, now + 1);
    assert_eq!(pending.reply, own);
    assert!(pending.carried.is_empty());
    assert_eq!(state.held.len(), 4);
}

#[test]
fn owner_turns_outlast_the_longest_tool_call() {
    assert_eq!(
        owner_turn_timeout(Duration::from_secs(1800)),
        Duration::from_secs(2100)
    );
    assert_eq!(
        owner_turn_timeout(Duration::from_secs(3600)),
        Duration::from_secs(3900)
    );
    // Short tool ceilings keep the 30-minute floor for multi-step turns.
    assert_eq!(
        owner_turn_timeout(Duration::from_secs(60)),
        OWNER_TURN_TIMEOUT
    );
    assert_eq!(owner_turn_timeout(Duration::MAX), Duration::MAX);
}

#[test]
fn recovery_tells_each_chat_which_background_jobs_stopped() {
    let directory = tempfile::tempdir().unwrap();
    let store = test_store(directory.path());
    let job = |to: &str, job: &str, task: &str| state::RunningJob {
        to_user_id: to.into(),
        job: job.into(),
        tool: "agent_codex".into(),
        task: task.into(),
        started_at: 1,
    };
    let mut saved = state::BridgeState {
        in_flight: vec![state::InFlight {
            message_id: "incoming".into(),
            to_user_id: "alice".into(),
            context_token: "context".into(),
            key: "alice".into(),
        }],
        jobs: vec![
            job("alice", "job-1", "Fix the build"),
            job("bob", "job-1", ""),
            job("alice", "job-2", "Publish"),
        ],
        ..Default::default()
    };
    // Without a planned restart: the generic failure and an unexpected stop.
    recover_interrupted(&store, "default", &mut saved).unwrap();
    let restarted = store.load_state("default").unwrap();
    assert!(restarted.jobs.is_empty() && restarted.in_flight.is_empty());
    let replies: Vec<_> = restarted
        .pending
        .iter()
        .map(|pending| (pending.to_user_id.as_str(), pending.reply.as_str()))
        .collect();
    assert_eq!(
        replies,
        [
            ("alice", FAILURE_REPLY),
            (
                "alice",
                "An unexpected interruption stopped background work that was still running:\n\
                 - job-1 (codex): Fix the build\n- job-2 (codex): Publish\n\
                 Ask again if you still need it."
            ),
            (
                "bob",
                "An unexpected interruption stopped background work that was still running:\n\
                 - job-1 (codex)\nAsk again if you still need it."
            ),
        ]
    );
    // Notices answer no message: no reply handle, the chat's own key.
    assert!(restarted.pending[1].context_token.is_empty());
    assert_eq!(restarted.pending[2].key, "bob");
}

#[test]
fn after_a_planned_restart_claims_are_told_why() {
    let directory = tempfile::tempdir().unwrap();
    let store = test_store(directory.path());
    let mut saved = state::BridgeState {
        in_flight: vec![state::InFlight {
            message_id: "incoming".into(),
            to_user_id: "alice".into(),
            context_token: "context".into(),
            key: String::new(),
        }],
        ..Default::default()
    };
    let restart = hub::Restart {
        to_version: "0.1.37".into(),
    };
    recover_interrupted_after(&store, "default", &mut saved, Some(&restart), "").unwrap();
    assert_eq!(
        store.load_state("default").unwrap().pending[0].reply,
        "SCV restarted to update to v0.1.37 before finishing this; ask again if you still need it."
    );
}

#[test]
fn recovered_replies_and_notices_are_scvs_own_words_and_carry_the_system_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let store = test_store(directory.path());
    let now = unix_now();
    let mut saved = state::BridgeState {
        in_flight: vec![state::InFlight {
            message_id: "incoming".into(),
            to_user_id: "alice".into(),
            context_token: "context".into(),
            key: "alice".into(),
        }],
        // The model's answer that was refused earlier, still waiting.
        held: vec![state::HeldReply {
            key: "alice".into(),
            to_user_id: "alice".into(),
            reply: "the model's earlier answer".into(),
            held_at: now,
        }],
        jobs: vec![state::RunningJob {
            to_user_id: "alice".into(),
            job: "job-1".into(),
            tool: "agent_codex".into(),
            task: String::new(),
            started_at: 1,
        }],
        ..Default::default()
    };
    recover_interrupted_after(&store, "default", &mut saved, None, "PREFIX: ").unwrap();
    let restarted = store.load_state("default").unwrap();
    let replies: Vec<_> = restarted
        .pending
        .iter()
        .map(|pending| pending.reply.as_str())
        .collect();
    // The failure reply carries the held answer: the prefix marks SCV's own
    // part, never the model's answer ahead of it.
    assert_eq!(
        replies,
        [
            format!(
                "{HELD_HEADER}the model's earlier answer\n\n{LATEST_HEADER}PREFIX: {FAILURE_REPLY}"
            ),
            "PREFIX: An unexpected interruption stopped background work that was still \
             running:\n- job-1 (codex)\nAsk again if you still need it."
                .to_owned(),
        ]
    );
    assert_eq!(
        restarted.pending[0].own.as_deref(),
        Some(format!("PREFIX: {FAILURE_REPLY}").as_str())
    );
}
