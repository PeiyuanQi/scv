//! Unit tests for `src/encode.rs`.

use scv_core::ToolCall;

use super::*;

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

fn tool(id: &str, name: &str, content: &str) -> Message {
    Message::Tool {
        call_id: id.into(),
        name: name.into(),
        content: content.into(),
        is_error: false,
    }
}

fn user(content: &str) -> Message {
    Message::user(content)
}

fn assistant(content: &str, tool_calls: Vec<ToolCall>) -> Message {
    Message::Assistant {
        content: content.into(),
        tool_calls,
    }
}

fn fc(id: &str, name: &str, arguments: &str) -> Value {
    json!({"type":"function_call","call_id":id,"name":name,"arguments":arguments})
}

fn out(id: &str, output: &str) -> Value {
    json!({"type":"function_call_output","call_id":id,"output":output})
}

#[test]
fn multi_step_turns_replay_each_call_before_its_output() {
    let messages = vec![
        user("summarize README"),
        assistant("", vec![call("c1", "read", json!({"path":"README.md"}))]),
        tool("c1", "read", "# SCV"),
        assistant("", vec![call("c2", "bash", json!({"command":"ls"}))]),
        tool("c2", "bash", "src"),
        assistant("A Rust agent.", Vec::new()),
        user("thanks"),
    ];
    assert_eq!(
        response_input(&messages, true).0,
        vec![
            json!({"role":"user","content":"summarize README"}),
            fc("c1", "read", r#"{"path":"README.md"}"#),
            out("c1", "# SCV"),
            fc("c2", "bash", r#"{"command":"ls"}"#),
            out("c2", "src"),
            json!({"role":"assistant","content":"A Rust agent."}),
            json!({"role":"user","content":"thanks"}),
        ]
    );
}

#[test]
fn parallel_calls_keep_the_assistant_text_and_call_order() {
    let messages = vec![
        user("compare"),
        assistant(
            "Reading both.",
            vec![
                call("a", "read", json!({"path":"a"})),
                call("b", "read", json!({"path":"b"})),
            ],
        ),
        tool("a", "read", "A"),
        tool("b", "read", "B"),
    ];
    assert_eq!(
        response_input(&messages, true).0,
        vec![
            json!({"role":"user","content":"compare"}),
            json!({"role":"assistant","content":"Reading both."}),
            fc("a", "read", r#"{"path":"a"}"#),
            fc("b", "read", r#"{"path":"b"}"#),
            out("a", "A"),
            out("b", "B"),
        ]
    );
}

#[test]
fn an_interrupted_call_is_closed_before_the_next_message() {
    let messages = vec![
        user("run both"),
        assistant(
            "",
            vec![
                call("a", "bash", json!({"command":"true"})),
                call("b", "bash", json!({"command":"sleep 99"})),
            ],
        ),
        tool("a", "bash", "ok"),
        user("never mind"),
    ];
    let input = response_input(&messages, true).0;
    assert_eq!(input[3], out("a", "ok"));
    assert_eq!(input[4]["type"], "function_call_output");
    assert_eq!(input[4]["call_id"], "b");
    assert!(
        input[4]["output"]
            .as_str()
            .unwrap()
            .contains("did not complete")
    );
    assert_eq!(input[5], json!({"role":"user","content":"never mind"}));
    assert_eq!(input.len(), 6);
}

#[test]
fn an_output_without_its_call_gets_one_and_plain_messages_are_unchanged() {
    let messages = vec![
        Message::HistoryNote {
            content: "[earlier]".into(),
        },
        tool("x", "read", "orphaned"),
        assistant("", Vec::new()),
    ];
    assert_eq!(
        response_input(&messages, true).0,
        vec![
            json!({"role":"user","content":"[earlier]"}),
            fc("x", "read", "{}"),
            out("x", "orphaned"),
            json!({"role":"assistant","content":""}),
        ]
    );
}

fn image_message(dir: &std::path::Path, name: &str, bytes: &[u8]) -> Message {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    Message::User {
        content: format!("look at {name}"),
        images: vec![ImageInput {
            path,
            mime: "image/png".into(),
        }],
    }
}

#[test]
fn user_images_become_input_images_read_at_request_time() {
    let dir = tempfile::tempdir().unwrap();
    let messages = vec![image_message(dir.path(), "a.png", b"png-bytes")];
    let (input, shown) = response_input(&messages, true);
    assert_eq!(shown, 1);
    assert_eq!(
        input[0],
        json!({"role":"user","content":[
            {"type":"input_text","text":"look at a.png"},
            {"type":"input_image","image_url":"data:image/png;base64,cG5nLWJ5dGVz"},
        ]})
    );
    // Without image input, or once the file is gone, the model gets a note.
    let (input, shown) = response_input(&messages, false);
    assert_eq!(shown, 0);
    assert_eq!(
        input[0]["content"][0]["text"],
        "look at a.png\n[image a.png: not shown to you here]"
    );
    std::fs::remove_file(dir.path().join("a.png")).unwrap();
    let (input, shown) = response_input(&messages, true);
    assert_eq!(shown, 0);
    assert_eq!(
        input[0]["content"][0]["text"],
        "look at a.png\n[image a.png: could not be read]"
    );
}

#[test]
fn only_the_newest_images_are_shown() {
    let dir = tempfile::tempdir().unwrap();
    let messages: Vec<_> = (0..MAX_REQUEST_IMAGES + 2)
        .map(|index| image_message(dir.path(), &format!("{index}.png"), b"x"))
        .collect();
    let (input, shown) = response_input(&messages, true);
    assert_eq!(shown, MAX_REQUEST_IMAGES);
    assert_eq!(input[0]["content"].as_array().unwrap().len(), 1);
    assert!(
        input[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with("not shown to you here]")
    );
    assert_eq!(input.last().unwrap()["content"][1]["type"], "input_image");
}
