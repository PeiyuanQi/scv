//! Unit tests for `src/email/content.rs`.

use super::*;

/// A fixed reply, the same as the vector computed apart from this code.
pub(crate) fn sample() -> ActionContent {
    ActionContent {
        v: 1,
        id: format!("a{}1", "0".repeat(31)),
        account: "default".into(),
        fingerprint: "f".repeat(64),
        kind: ActionKind::Send,
        group: Some("g0123456789abcdef".into()),
        origin: Origin::Owner {
            route: "feishu:mail".into(),
            message_id: "om_1".into(),
        },
        source: Some(Source {
            reference: SourceRef::Imap {
                mailbox: "INBOX".into(),
                uidvalidity: 7,
                uid: 42,
            },
            identity: "i".repeat(64),
            locator: "l".repeat(64),
            message_id: Some("<m@example.com>".into()),
        }),
        display: Some(Display {
            handle: "4K7P".into(),
            from_address: "alice@example.com".into(),
            from_name: "Alice".into(),
            subject: "Hello".into(),
        }),
        folder: Some(Folder {
            role: FolderRole::Sent,
            name: "Sent".into(),
        }),
        message: Some(Outgoing {
            form: Form::Reply,
            from: Mailbox {
                name: "Me".into(),
                address: "me@example.org".into(),
            },
            to: vec!["alice@example.com".into()],
            cc: Vec::new(),
            subject: "Re: Hello".into(),
            body: "Hi Alice,\nThanks.".into(),
            in_reply_to: Some("<m@example.com>".into()),
            references: vec!["<m@example.com>".into()],
            message_id: "<x@example.org>".into(),
            sent_copy: SentCopy::Provider,
            notes: Vec::new(),
        }),
        created_at: 1_728_032_400,
        hard_expiry: 1_728_291_600,
        digest: String::new(),
    }
    .seal_digest()
}

#[test]
fn the_encoding_matches_its_specification_byte_for_byte() {
    let mut bytes = Vec::new();
    for (name, value) in [
        ("x", Canon::Null),
        ("n", Canon::Int(5)),
        ("s", Canon::Str("é".into())),
        ("l", Canon::List(vec![Canon::Str("a".into())])),
    ] {
        encode_field(name, &value, &mut bytes);
    }
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(
        hex,
        "7800006e000200000000000000057300030000000000000002c3a96c0004000000000000000103000000000000000161"
    );
}

#[test]
fn the_digest_is_pinned_to_an_independent_computation() {
    // Computed apart from this code, from the encoding's specification.
    assert_eq!(
        sample().digest,
        "478d7a63ea74eac211e71d4ba5bc5dd51702796125c1baa14adf38b28331a470"
    );
    assert_eq!(sample().short_digest(), "478d7a63");
}

#[test]
fn the_digest_covers_every_field_and_tells_empty_from_absent() {
    let base = sample();
    let changed: Vec<ActionContent> = vec![
        ActionContent {
            kind: ActionKind::Draft,
            ..base.clone()
        },
        ActionContent {
            group: None,
            ..base.clone()
        },
        ActionContent {
            hard_expiry: base.hard_expiry + 1,
            ..base.clone()
        },
        {
            let mut other = base.clone();
            other.message.as_mut().unwrap().to[0] = "mallory@example.com".into();
            other
        },
        {
            let mut other = base.clone();
            other.message.as_mut().unwrap().body.push(' ');
            other
        },
        {
            let mut other = base.clone();
            other.message.as_mut().unwrap().cc.push(String::new());
            other
        },
        {
            let mut other = base.clone();
            other.message.as_mut().unwrap().in_reply_to = None;
            other
        },
        {
            let mut other = base.clone();
            other.message.as_mut().unwrap().in_reply_to = Some(String::new());
            other
        },
        {
            let mut other = base.clone();
            other.folder.as_mut().unwrap().name = "Sent Items".into();
            other
        },
        {
            let mut other = base.clone();
            other.message.as_mut().unwrap().notes.push("n".into());
            other
        },
    ];
    let mut seen = vec![base.compute_digest()];
    for content in changed {
        let digest = content.compute_digest();
        assert!(!seen.contains(&digest), "{content:?}");
        seen.push(digest);
    }
}

#[test]
fn the_digest_does_not_depend_on_json() {
    let content = sample();
    let pretty = serde_json::to_string_pretty(&content).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&pretty).unwrap();
    // Reordered keys read back to the same content, and the same digest.
    let object = value.as_object_mut().unwrap();
    let digest = object.remove("digest").unwrap();
    object.insert("digest".into(), digest);
    let reread: ActionContent = serde_json::from_value(value).unwrap();
    assert_eq!(reread.compute_digest(), content.digest);
}

#[test]
fn content_files_are_written_once_and_read_back_whole() {
    let home = tempfile::tempdir().unwrap();
    let store = ContentStore::new(home.path().to_owned());
    let content = sample();
    store.write_new(&content).unwrap();
    let path = home.path().join(format!("{}.json", content.id));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert_eq!(store.read(&content.id).unwrap(), Some(content.clone()));
    // Never rewritten.
    assert!(store.write_new(&content).is_err());
    // A digest that does not match refuses the write.
    let mut forged = sample();
    forged.id = format!("a{}2", "0".repeat(31));
    forged.message.as_mut().unwrap().body = "changed".into();
    assert!(store.write_new(&forged).is_err());
    assert_eq!(store.list().len(), 1);
    assert_eq!(store.list()[0].id, content.id);
    store.remove(&content.id).unwrap();
    store.remove(&content.id).unwrap();
    assert_eq!(store.read(&content.id).unwrap(), None);
}

#[test]
fn a_file_is_refused_when_it_names_another_action_or_is_not_an_id() {
    let home = tempfile::tempdir().unwrap();
    let store = ContentStore::new(home.path().to_owned());
    let content = sample();
    let other = format!("a{}9", "0".repeat(31));
    std::fs::write(
        home.path().join(format!("{other}.json")),
        serde_json::to_string(&content).unwrap(),
    )
    .unwrap();
    assert!(store.read(&other).is_err());
    assert!(store.read("../../etc/passwd").is_err());
    assert!(store.read("a123").is_err());
    assert!(!valid_id(&format!("a{}", "G".repeat(32))));
    assert!(valid_id(&new_id()));
}
