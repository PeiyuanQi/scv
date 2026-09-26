//! Unit tests for `src/builtin/chat_attach.rs`.

use super::*;
use std::os::unix::fs::{PermissionsExt as _, symlink};

fn config(root: &Path) -> ChatAttachConfig {
    ChatAttachConfig::standard(
        Some(&root.join("home")),
        &root.join("home/.scv"),
        root.join("home/.scv/state/media/outbox"),
        vec![root.join("home/.scv/state/media")],
        1024,
    )
}

fn file(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn workspace_files_are_attached_with_their_resolved_path() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("home/work");
    file(&workspace.join("out/chart.png"), b"png");
    let attached = config(root.path())
        .check(&workspace, "out/chart.png")
        .unwrap();
    assert_eq!(attached.name, "chart.png");
    assert_eq!(attached.size, 3);
    assert_eq!(
        Path::new(&attached.path),
        std::fs::canonicalize(workspace.join("out/chart.png")).unwrap()
    );
}

#[test]
fn secrets_are_refused_even_through_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let workspace = home.join("work");
    file(&home.join(".ssh/config"), b"Host x");
    file(&home.join(".scv/config.toml"), b"[provider]");
    file(&home.join(".scv/credentials/wechat/default.json"), b"{}");
    file(&home.join(".cargo/credentials.toml"), b"token");
    file(&home.join(".config/gh/hosts.yml"), b"token");
    file(&workspace.join(".env"), b"KEY=1");
    file(&workspace.join("server.pem"), b"-----");
    std::fs::create_dir_all(&workspace).unwrap();
    symlink(home.join(".ssh/config"), workspace.join("innocent.txt")).unwrap();
    let config = config(root.path());
    for path in [
        home.join(".ssh/config").display().to_string(),
        home.join(".scv/config.toml").display().to_string(),
        home.join(".scv/credentials/wechat/default.json")
            .display()
            .to_string(),
        home.join(".cargo/credentials.toml").display().to_string(),
        home.join(".config/gh/hosts.yml").display().to_string(),
        ".env".into(),
        "server.pem".into(),
        "innocent.txt".into(),
    ] {
        let error = config.check(&workspace, &path).unwrap_err();
        assert!(
            error.message.contains("credentials or keys"),
            "{path}: {}",
            error.message
        );
    }
}

#[test]
fn media_the_user_sent_stays_attachable_inside_the_instance() {
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("home/.scv/state/media/wechat/photo.jpg");
    file(&media, b"jpg");
    let attached = config(root.path())
        .check(root.path(), &media.display().to_string())
        .unwrap();
    assert_eq!(attached.name, "photo.jpg");
}

#[test]
fn directories_missing_empty_and_oversized_files_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("home/work");
    std::fs::create_dir_all(workspace.join("dir")).unwrap();
    file(&workspace.join("empty"), b"");
    file(&workspace.join("big"), &[0; 2048]);
    let config = config(root.path());
    assert!(
        config
            .check(&workspace, "dir")
            .unwrap_err()
            .message
            .contains("regular file")
    );
    assert!(
        config
            .check(&workspace, "empty")
            .unwrap_err()
            .message
            .contains("empty")
    );
    assert!(
        config
            .check(&workspace, "big")
            .unwrap_err()
            .message
            .contains("limit")
    );
    assert!(config.check(&workspace, "missing").is_err());
}

#[tokio::test]
async fn the_tool_reports_the_attachment_the_client_reads() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("home/work");
    file(&workspace.join("notes.txt"), b"hello");
    let tool = ChatAttachTool {
        config: config(root.path()),
    };
    let arguments = json!({"path":"notes.txt","caption":" today's notes "});
    assert_eq!(tool.risk(&arguments).unwrap(), ToolRisk::Network);
    let output = tool
        .execute(
            arguments,
            ToolContext::new(workspace, tokio_util::sync::CancellationToken::new()),
        )
        .await
        .unwrap();
    let attached =
        scv_protocol::reply_attachment(CHAT_ATTACH_TOOL, !output.is_error(), &output.content)
            .unwrap();
    assert_eq!(attached.name, "notes.txt");
    assert_eq!(attached.caption, "today's notes");
    // The client sends a private copy in the outbox, not the original.
    let copy = Path::new(&attached.path);
    let outbox = std::fs::canonicalize(root.path().join("home/.scv/state/media/outbox")).unwrap();
    assert!(copy.starts_with(&outbox), "{}", copy.display());
    assert_eq!(std::fs::read(copy).unwrap(), b"hello");
    assert_eq!(
        std::fs::metadata(copy).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(tool.risk(&json!({"path":"x","extra":1})).is_err());
}
