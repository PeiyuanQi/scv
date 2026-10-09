//! Reviewed jobs end to end: a stdio server runs an `agent` call with
//! `review` through fake agent scripts, reports SCV's outcome with the
//! report turn, and keeps the review's journal under `state/reviews`.

use super::background::{Server, serve_provider};
use crate::support::{call, text, write_private};
use scv_protocol::{ReviewOutcome, ServerEvent, TurnOrigin};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt as _, path::Path, time::Duration};

/// A fake agent at `home/fake-<name>` that prints its next canned reply
/// (`home/<name>-<n>.out`) on each call, after hanging when
/// `home/<name>-<n>.sleep` exists, and records its arguments in
/// `home/<name>-<n>.args`.
fn fake_agent(home: &Path, name: &str, replies: &[String]) {
    for (index, reply) in replies.iter().enumerate() {
        std::fs::write(home.join(format!("{name}-{}.out", index + 1)), reply).unwrap();
    }
    let script = home.join(format!("fake-{name}"));
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             dir={dir:?}\n\
             n=$(cat \"$dir/{name}-count\" 2>/dev/null || echo 0)\n\
             n=$((n + 1))\n\
             echo \"$n\" > \"$dir/{name}-count\"\n\
             printf '%s\\n' \"$@\" > \"$dir/{name}-$n.args\"\n\
             if [ -f \"$dir/{name}-$n.sleep\" ]; then sleep 30; fi\n\
             cat \"$dir/{name}-$n.out\"\n",
            dir = home.display().to_string()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Codex's `--json` events for a reply.
fn codex_says(reply: &str) -> String {
    [
        json!({"type":"thread.started","thread_id":"0199a213-81c0-7800-8aa1-bbab2a035a53"}),
        json!({"type":"item.completed","item":{"id":"i1","type":"agent_message","text":reply}}),
        json!({"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}),
    ]
    .iter()
    .map(|event| format!("{event}\n"))
    .collect()
}

/// Codex failing because it is signed out.
fn codex_signed_out() -> String {
    [
        json!({"type":"thread.started","thread_id":"0199a213-81c0-7800-8aa1-bbab2a035a54"}),
        json!({"type":"turn.failed","error":{"message":"Error: not logged in"}}),
    ]
    .iter()
    .map(|event| format!("{event}\n"))
    .collect()
}

/// Claude Code's `stream-json` result for a reply.
fn claude_says(reply: &str) -> String {
    format!(
        "{}\n",
        json!({"type":"result","subtype":"success","is_error":false,"result":reply,
               "usage":{"input_tokens":1,"output_tokens":1}})
    )
}

fn verdict(body: &Value) -> String {
    format!("I reviewed the change.\n\n```scv-verdict\n{body}\n```")
}

/// An SCV home whose preferred agent is a fake Claude, beside a fake Codex.
fn home_with_agents(
    claude: &[String],
    codex: &[String],
) -> (tempfile::TempDir, std::path::PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(home.path()).unwrap();
    fake_agent(&path, "claude", claude);
    fake_agent(&path, "codex", codex);
    write_private(
        &path.join("config.toml"),
        &format!(
            "[agent]\nprefer = [\"claude\"]\n\n\
             [agents.claude]\ncommand = {:?}\ntransport = \"resume\"\n\n\
             [agents.codex]\ncommand = {:?}\ntransport = \"resume\"\n",
            path.join("fake-claude").display().to_string(),
            path.join("fake-codex").display().to_string()
        ),
    );
    (home, path)
}

/// The review journals in `home`, each as its events.
fn journals(home: &Path) -> Vec<Vec<Value>> {
    let Ok(entries) = std::fs::read_dir(home.join("state/reviews")) else {
        return Vec::new();
    };
    entries
        .map(|entry| {
            std::fs::read_to_string(entry.unwrap().path())
                .unwrap()
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        })
        .collect()
}

/// Run the server until the report turn about the reviewed job ends, and
/// return that turn's origin.
async fn report_origin(server: &mut Server) -> TurnOrigin {
    loop {
        match server.next().await.expect("server went quiet") {
            ServerEvent::TurnCompleted {
                origin: Some(origin),
                ..
            } => return origin,
            ServerEvent::TurnFailed { message, .. } => panic!("turn failed: {message}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn a_reviewed_job_loops_until_approval_and_reports_scvs_outcome() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = serve_provider(
        listener,
        vec![
            call(
                "call_1",
                "agent",
                json!({"prompt":"Fix the flaky checkout test.","review":{}}),
            ),
            text("Started job-1, with a review."),
            text("job-1 was approved in round 2."),
        ],
    );
    let (_home, home) = home_with_agents(
        &[
            claude_says("Replaced the sleep with a lock on branch fix/race."),
            claude_says("Also removed the retry."),
        ],
        &[
            codex_says(&verdict(&json!({
                "verdict":"changes","summary":"A retry still masks the race.",
                "findings":[{"severity":"blocking","title":"Retry masks the race",
                             "location":"shop/src/cart.rs:90"}]
            }))),
            codex_says(&verdict(&json!({
                "verdict":"approve","summary":"The race is gone.",
                "prior":[{"id":"1.1","status":"resolved"}],
                "evidence":["ran cargo test -p checkout 20x: all passed"]
            }))),
        ],
    );
    let workspace = tempfile::tempdir().unwrap();
    let mut server = Server::start(&home, address, workspace.path()).await;
    server.turn("fix the flaky test and have it reviewed").await;
    let origin = tokio::time::timeout(Duration::from_secs(60), report_origin(&mut server))
        .await
        .expect("the review never reported");
    assert_eq!(origin.jobs, ["job-1"]);
    let outcome = &origin.outcomes[0];
    assert_eq!(outcome.review.outcome, ReviewOutcome::Approved);
    assert_eq!((outcome.review.round, outcome.review.rounds), (2, 3));
    assert_eq!(outcome.review.reviewer, "codex");
    assert_eq!(
        scv_protocol::outcome_notice(outcome),
        "job-1 · Review: approved · round 2 of 3 · reviewer codex\njob-1 · Landing: not requested"
    );
    // The builder's second turn continued its conversation.
    let second = std::fs::read_to_string(home.join("claude-2.args")).unwrap();
    assert!(second.contains("--resume"), "{second}");
    assert!(second.contains("[SCV review, round 2 of 3]"), "{second}");
    assert!(second.contains("Retry masks the race"), "{second}");
    // The reviewer never learned the round limit.
    let review = std::fs::read_to_string(home.join("codex-1.args")).unwrap();
    assert!(review.contains("independent reviewer"), "{review}");
    assert!(!review.contains("3 rounds"), "{review}");
    // The model read SCV's lines in the report and was told how to use them.
    let bodies: Vec<String> = requests.try_iter().collect();
    let report = bodies.last().unwrap();
    assert!(
        report.contains("Review: approved · round 2 of 3 · reviewer codex"),
        "{report}"
    );
    assert!(
        report.contains("SCV shows the user its own Review and Landing lines"),
        "{report}"
    );
    // Every step is in the journal.
    let journals = journals(&home);
    assert_eq!(journals.len(), 1);
    let events: Vec<&str> = journals[0]
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect();
    assert_eq!(events.first(), Some(&"review.started"));
    assert_eq!(events.last(), Some(&"review.finished"));
    assert_eq!(
        events.iter().filter(|event| **event == "verdict").count(),
        2
    );
    assert_eq!(journals[0].last().unwrap()["outcome"], "approved");
    assert_eq!(journals[0][0]["job"], "job-1");
}

#[tokio::test]
async fn a_signed_out_reviewer_hands_over_and_a_crash_leaves_an_interrupted_journal() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let _requests = serve_provider(
        listener,
        vec![
            call("call_1", "agent", json!({"prompt":"Fix it.","review":{}})),
            text("Started job-1."),
        ],
    );
    // Codex is signed out, so the next reviewer is a fresh Claude, which
    // hangs until the server dies.
    let (_home, home) = home_with_agents(
        &[claude_says("Done."), claude_says("unused")],
        &[codex_signed_out()],
    );
    std::fs::write(home.join("claude-2.sleep"), "").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut server = Server::start(&home, address, workspace.path()).await;
    server.turn("fix it, reviewed").await;
    // The call is approved and its turn ends; the job runs on.
    loop {
        match server.next().await.expect("server went quiet") {
            ServerEvent::TurnCompleted { origin: None, .. } => break,
            ServerEvent::TurnFailed { message, .. } => panic!("turn failed: {message}"),
            _ => {}
        }
    }
    tokio::time::timeout(Duration::from_secs(60), async {
        while !home.join("claude-2.args").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the fallback reviewer never started");
    let reviewer = std::fs::read_to_string(home.join("claude-2.args")).unwrap();
    assert!(reviewer.contains("independent reviewer"), "{reviewer}");
    // A fresh conversation, never the builder's.
    assert!(!reviewer.contains("--resume"), "{reviewer}");
    server.kill().await;
    let journal = journals(&home).pop().unwrap();
    let events: Vec<&str> = journal
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect();
    assert!(events.contains(&"reviewer.finished"), "{events:?}");
    assert_eq!(events.last(), Some(&"reviewer.started"), "{events:?}");
    let unavailable = journal
        .iter()
        .find(|event| event["event"] == "reviewer.finished")
        .unwrap();
    assert_eq!(unavailable["agent"], "codex");
    assert_eq!(unavailable["unavailable"], true);
    // Nothing ends it: the review was interrupted.
    assert!(!events.contains(&"review.finished"));
}
