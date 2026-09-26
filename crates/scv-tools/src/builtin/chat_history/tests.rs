//! Unit tests for `src/builtin/chat_history.rs`.

use std::time::Duration;

use scv_client::history::{LocalTime, Log, Role};

use super::*;

const AT: i64 = 1_790_456_645;

struct Chat {
    _home: tempfile::TempDir,
    config: ChatHistoryConfig,
}

fn chat() -> Chat {
    let home = tempfile::tempdir().unwrap();
    let conversation = history::conversation_path("wechat", "default", "0a1b").unwrap();
    let config = ChatHistoryConfig::new(
        &home.path().join("history"),
        &home.path().join("state/media"),
        &home.path().join("history"),
        &conversation,
        64 * 1024,
    );
    std::fs::create_dir_all(&config.media).unwrap();
    Chat {
        _home: home,
        config,
    }
}

fn entry(role: Role, text: &str, files: Vec<FileRef>) -> Entry {
    Entry {
        at: AT as u64 * 1000,
        role,
        text: text.into(),
        files,
        ..Entry::default()
    }
}

fn call(config: &ChatHistoryConfig, arguments: Value) -> Value {
    let args: HistoryArgs = parse_args(&arguments).unwrap();
    args.check().unwrap();
    serde_json::from_str(&run(config, &args).unwrap().content).unwrap()
}

#[test]
fn searches_lists_and_reads_the_log_with_file_places() {
    let chat = chat();
    let config = &chat.config;
    let photo = config.media.join("abc-cat.jpg");
    std::fs::write(&photo, b"jpg").unwrap();
    let file = |name: &str, path: &Path| FileRef {
        kind: "image".into(),
        name: name.into(),
        path: path.display().to_string(),
        ..FileRef::default()
    };
    let mut log = Log::new(config.log.clone(), Duration::from_secs(7200));
    let local = LocalTime::at(AT, 0);
    log.append(
        entry(
            Role::Owner,
            "look at my cat",
            vec![
                file("cat.jpg", &photo),
                file("old.jpg", &config.media.join("gone-old.jpg")),
            ],
        ),
        &local,
    )
    .unwrap();
    log.append(
        entry(
            Role::Scv,
            "Lovely cat.",
            vec![FileRef {
                kind: "image".into(),
                name: "reply.png".into(),
                ..FileRef::default()
            }],
        ),
        &local,
    )
    .unwrap();
    let mut voice = entry(
        Role::Owner,
        "",
        vec![FileRef {
            kind: "audio".into(),
            name: "voice.silk".into(),
            transcript: "feed the cat".into(),
            ..FileRef::default()
        }],
    );
    voice.at += 1;
    log.append(voice, &local).unwrap();
    let found = call(config, json!({"action":"search","query":"CAT"}));
    assert_eq!(found["hits"].as_array().unwrap().len(), 3);
    assert_eq!(found["hits"][1]["role"], "scv");
    assert_eq!(found["more"], false);
    let id = found["hits"][0]["episode"].as_str().unwrap().to_owned();
    let listed = call(config, json!({"action":"episodes"}));
    assert_eq!(listed["episodes"][0]["id"], id);
    assert_eq!(listed["episodes"][0]["opening"], "look at my cat");
    let read = call(config, json!({"action":"read","episode":id}));
    assert_eq!(read["total"], 3);
    let files = &read["messages"][0]["files"];
    assert_eq!(files[0]["path"], photo.display().to_string());
    assert_eq!(files[1]["gone"], true);
    // What SCV sent has no copy; a voice message keeps what it said, and
    // an owner's file with no path was never saved.
    assert_eq!(read["messages"][1]["files"][0]["sent"], true);
    let voice = &read["messages"][2]["files"][0];
    assert_eq!(voice["transcript"], "feed the cat");
    assert_eq!(voice["not_saved"], true);
    // Kept, the file shows at its new place.
    let kept = keep(config, &photo.display().to_string()).unwrap();
    assert!(kept.is_file() && !photo.exists());
    let read = call(
        config,
        json!({"action":"read","episode":id,"offset":0,"limit":1}),
    );
    assert_eq!(read["messages"].as_array().unwrap().len(), 1);
    assert_eq!(read["messages"][0]["files"][0]["kept"], true);
    assert_eq!(
        read["messages"][0]["files"][0]["path"],
        kept.display().to_string()
    );
    // Keeping it again reports where it is.
    assert_eq!(keep(config, &kept.display().to_string()).unwrap(), kept);
}

#[test]
fn bad_arguments_are_refused() {
    let chat = chat();
    let refused = |arguments: Value| {
        let args: HistoryArgs = parse_args(&arguments).unwrap();
        args.check().is_err()
    };
    assert!(refused(json!({"action":"search"})));
    assert!(refused(json!({"action":"search","query":"  "})));
    assert!(refused(json!({"action":"read"})));
    assert!(refused(json!({"action":"delete"})));
    assert!(refused(json!({"action":"episodes","before":"yesterday"})));
    assert!(!refused(json!({"action":"episodes","after":"2026-09-01"})));
    let args: HistoryArgs =
        parse_args(&json!({"action":"read","episode":"../../etc/passwd"})).unwrap();
    assert!(run(&chat.config, &args).is_err());
    // An empty log has nothing, rather than failing.
    let listed = call(&chat.config, json!({"action":"episodes"}));
    assert_eq!(listed["episodes"], json!([]));
}

#[test]
fn only_this_chats_received_files_can_be_kept() {
    let chat = chat();
    let config = &chat.config;
    let elsewhere = config.media.parent().unwrap().join("other.txt");
    std::fs::write(&elsewhere, b"x").unwrap();
    std::fs::create_dir_all(config.media.join("sub")).unwrap();
    std::fs::write(config.media.join("sub/deep.txt"), b"x").unwrap();
    for path in [
        elsewhere.display().to_string(),
        config.media.join("sub/deep.txt").display().to_string(),
        config.media.join("missing.txt").display().to_string(),
        config.media.join("../other.txt").display().to_string(),
        "relative.txt".into(),
        "/".into(),
    ] {
        assert!(keep(config, &path).is_err(), "{path}");
    }
    #[cfg(unix)]
    {
        let link = config.media.join("link.txt");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        assert!(keep(config, &link.display().to_string()).is_err());
    }
    let taken = config.media.join("a.txt");
    std::fs::write(&taken, b"new").unwrap();
    std::fs::create_dir_all(&config.kept).unwrap();
    std::fs::write(config.kept.join("a.txt"), b"old").unwrap();
    assert!(keep(config, &taken.display().to_string()).is_err());
    assert!(taken.exists());
}

#[test]
fn large_results_are_cut_to_the_output_limit() {
    let value = json!({"hits":(0..200).map(|index| json!({"excerpt":"x".repeat(100),"index":index})).collect::<Vec<_>>(),"more":false});
    let output = bounded(value, 2000);
    assert!(output.truncated);
    assert!(output.content.len() <= 2000);
    let parsed: Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(parsed["more"], true);
    assert!(!parsed["hits"].as_array().unwrap().is_empty());
}
