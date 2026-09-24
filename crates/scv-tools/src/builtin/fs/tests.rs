//! Unit tests for `src/builtin/fs.rs`.

use std::os::unix::fs::symlink;

use super::*;

#[test]
fn rejects_parent_traversal() {
    assert!(validate_relative(Path::new("../secret")).is_err());
    assert!(validate_relative(Path::new("/etc/passwd")).is_err());
}

#[test]
fn detects_secret_like_paths() {
    assert!(is_secret_like(Path::new(".env")));
    assert!(is_secret_like(Path::new("keys/id.pem")));
    assert!(!is_secret_like(Path::new("src/main.rs")));
}

#[tokio::test]
async fn read_is_contained_and_bounded() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("hello.txt"), "abcdef").unwrap();
    let tool = ReadTool { max_bytes: 3 };
    let output = tool
        .execute(
            json!({"path":"hello.txt"}),
            ToolContext::new(
                directory.path().canonicalize().unwrap(),
                tokio_util::sync::CancellationToken::new(),
            ),
        )
        .await
        .unwrap();
    assert!(output.truncated);
    assert!(output.content.contains("abc"));
}

#[tokio::test]
async fn read_rejects_symlink_escape() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "nope").unwrap();
    symlink(outside.path(), workspace.path().join("escape")).unwrap();
    let tool = ReadTool { max_bytes: 100 };
    let result = tool
        .execute(
            json!({"path":"escape/secret"}),
            ToolContext::new(
                workspace.path().canonicalize().unwrap(),
                tokio_util::sync::CancellationToken::new(),
            ),
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("workspace"));
}

#[tokio::test]
async fn write_is_atomic_and_checks_hash() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let tool = WriteTool { max_bytes: 100 };
    tool.execute(
        json!({"path":"file.txt","content":"first","mode":"create"}),
        ToolContext::new(root.clone(), tokio_util::sync::CancellationToken::new()),
    )
    .await
    .unwrap();
    let hash = format!("{:x}", Sha256::digest(b"first"));
    tool.execute(
        json!({"path":"file.txt","content":"second","mode":"replace","expected_sha256":hash}),
        ToolContext::new(root.clone(), tokio_util::sync::CancellationToken::new()),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("file.txt")).unwrap(),
        "second"
    );
    let result = tool
            .execute(
                json!({"path":"file.txt","content":"third","mode":"replace","expected_sha256":"deadbeef"}),
                ToolContext::new(root, tokio_util::sync::CancellationToken::new()),
            )
            .await;
    assert!(result.unwrap_err().to_string().contains("changed"));
}

#[tokio::test]
async fn write_rejects_symlink_escape() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), workspace.path().join("escape")).unwrap();
    let tool = WriteTool { max_bytes: 100 };
    let result = tool
        .execute(
            json!({"path":"escape/file.txt","content":"nope","mode":"create"}),
            ToolContext::new(
                workspace.path().canonicalize().unwrap(),
                tokio_util::sync::CancellationToken::new(),
            ),
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("workspace"));
    assert!(!outside.path().join("file.txt").exists());
}
