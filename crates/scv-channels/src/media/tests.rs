//! Unit tests for `src/media.rs`.

use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn declared_types_win_then_magic_bytes_then_extensions() {
    assert_eq!(mime_type(Some("image/png"), "x.jpg", b""), "image/png");
    assert_eq!(
        mime_type(
            Some("application/octet-stream"),
            "x",
            b"\x89PNG\r\n\x1a\nrest"
        ),
        "image/png"
    );
    assert_eq!(
        mime_type(Some("image/*"), "x", b"\xff\xd8\xff\xe0"),
        "image/jpeg"
    );
    assert_eq!(mime_type(None, "clip", b"\0\0\0\x18ftypmp42"), "video/mp4");
    assert_eq!(mime_type(None, "voice", b"\x02#!SILK_V3"), "audio/silk");
    assert_eq!(mime_type(None, "report.PDF", b"plain"), "application/pdf");
    assert_eq!(mime_type(None, "blob", b"????"), "application/octet-stream");
    assert_eq!(extension("image/jpeg"), "jpg");
    assert_eq!(extension("application/x-unknown"), "bin");
    assert_eq!(MediaKind::for_mime("image/png"), MediaKind::Image);
    assert_eq!(MediaKind::for_mime("image/heic"), MediaKind::File);
    assert_eq!(MediaKind::for_mime("video/mp4"), MediaKind::Video);
}

#[test]
fn names_lose_directories_control_characters_and_leading_dots() {
    assert_eq!(safe_name("../../etc/passwd"), "passwd");
    assert_eq!(safe_name("C:\\Users\\a\\photo.jpg"), "photo.jpg");
    assert_eq!(safe_name(".bashrc"), "bashrc");
    assert_eq!(safe_name("a\u{0}b\nc?.txt"), "a_b_c_.txt");
    assert!(safe_name(&"長".repeat(100)).len() <= MAX_NAME_BYTES);
}

#[test]
fn saved_files_are_private_and_never_overwrite() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("wechat/default/c1");
    let first = save(&dir, "../photo.jpg", b"one").unwrap();
    let second = save(&dir, "photo.jpg", b"two").unwrap();
    assert_ne!(first, second);
    assert!(first.starts_with(&dir));
    assert!(
        first
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with("-photo.jpg")
    );
    assert_eq!(std::fs::read(&first).unwrap(), b"one");
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&first), 0o600);
    assert_eq!(mode(&dir), 0o700);
    assert!(
        save(&dir, "", b"x")
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with("-file")
    );
}

#[test]
fn prune_removes_old_files_and_their_empty_directories() {
    let root = tempfile::tempdir().unwrap();
    let old = save(&root.path().join("a/b"), "old.txt", b"x").unwrap();
    let fresh = save(&root.path().join("c"), "new.txt", b"x").unwrap();
    let past = SystemTime::now() - Duration::from_secs(10 * 24 * 3600);
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(past)
        .unwrap();
    assert_eq!(prune(root.path(), Duration::from_secs(7 * 24 * 3600)), 1);
    assert!(!old.exists());
    assert!(!root.path().join("a").exists());
    assert!(fresh.exists());
    assert!(root.path().exists());
}

#[test]
fn only_regular_files_inside_the_outbox_count() {
    let root = tempfile::tempdir().unwrap();
    let outbox = root.path().join("outbox");
    let inside = save(&outbox, "a.txt", b"x").unwrap();
    let outside = save(&root.path().join("other"), "b.txt", b"x").unwrap();
    let link = outbox.join("link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let linked_dir = outbox.join("dir");
    std::os::unix::fs::symlink(root.path().join("other"), &linked_dir).unwrap();
    assert!(!is_inside(
        &outbox,
        &linked_dir.join(outside.file_name().unwrap())
    ));
    assert!(is_inside(&outbox, &inside));
    assert!(!is_inside(&outbox, &outside));
    assert!(!is_inside(&outbox, &link));
    assert!(!is_inside(&outbox, &outbox.join("missing")));
    assert!(!is_inside(&outbox, &outbox.join("../other/b.txt")));
}

#[test]
fn limits_follow_the_sender_and_kind() {
    let settings = MediaSettings::default();
    assert_eq!(
        settings.limit(MediaKind::File, true),
        Some(50 * 1024 * 1024)
    );
    assert_eq!(
        settings.limit(MediaKind::Image, false),
        Some(5 * 1024 * 1024)
    );
    assert_eq!(settings.limit(MediaKind::File, false), None);
    let off = MediaSettings {
        owner_max_mib: 0,
        ..MediaSettings::default()
    };
    assert_eq!(off.limit(MediaKind::Image, true), None);
    assert_eq!(off.limit(MediaKind::Image, false), None);
}
