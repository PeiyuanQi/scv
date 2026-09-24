//! Unit tests for `src/attachments.rs`.

use super::*;

fn attachment(kind: &str, path: &Path, name: &str, mime: &str, size: u64) -> Attachment {
    Attachment {
        kind: kind.into(),
        path: path.display().to_string(),
        name: name.into(),
        mime: mime.into(),
        size,
        transcript: None,
    }
}

#[test]
fn owners_see_paths_and_images_go_to_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let photo = dir.path().join("photo.jpg");
    let report = dir.path().join("report.pdf");
    let files = [
        attachment("image", &photo, "photo.jpg", "image/jpeg", 2048),
        attachment(
            "file",
            &report,
            "report.pdf",
            "application/pdf",
            3 * 1024 * 1024,
        ),
    ];
    let input = turn_input("what are these?", &files, true);
    assert!(
        input
            .text
            .starts_with("what are these?\n\n[The user attached:]")
    );
    assert!(input.text.contains(&format!(
        "- image photo.jpg (image/jpeg, 2 KB) at {}",
        photo.display()
    )));
    assert!(
        input
            .text
            .contains("- file report.pdf (application/pdf, 3.0 MB) at")
    );
    assert!(input.text.contains("hand them to an agent"));
    assert_eq!(
        input.images,
        vec![ImageInput {
            path: photo,
            mime: "image/jpeg".into()
        }]
    );
}

#[test]
fn tool_free_sessions_see_no_paths_and_voice_transcripts_are_quoted() {
    let dir = tempfile::tempdir().unwrap();
    let mut voice = attachment("audio", &dir.path().join("v.silk"), "", "audio/silk", 900);
    voice.transcript = Some(" see you at five ".into());
    let input = turn_input("", &[voice], false);
    assert_eq!(
        input.text,
        "[The user attached:]\n- voice or audio (audio/silk, 900 bytes); it says: \"see you at five\""
    );
    assert!(!input.text.contains(&dir.path().display().to_string()));
    assert!(input.images.is_empty());
}

#[test]
fn unsupported_or_oversized_images_are_listed_but_not_shown() {
    let dir = tempfile::tempdir().unwrap();
    let files = [
        attachment(
            "image",
            &dir.path().join("a.heic"),
            "a.heic",
            "image/heic",
            10,
        ),
        attachment(
            "image",
            &dir.path().join("b.png"),
            "b.png",
            "image/png",
            scv_provider_openai::MAX_IMAGE_BYTES + 1,
        ),
    ];
    let input = turn_input("", &files, true);
    assert!(input.images.is_empty());
    assert!(input.text.contains("a.heic"));
    assert!(input.text.contains("b.png"));
}

#[test]
fn validation_requires_absolute_regular_files_of_known_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f.txt");
    std::fs::write(&file, "x").unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&file, &link).unwrap();
    assert!(validate(&[attachment("file", &file, "f.txt", "", 1)]).is_ok());
    assert!(validate(&[attachment("file", Path::new("f.txt"), "", "", 1)]).is_err());
    assert!(validate(&[attachment("file", &link, "", "", 1)]).is_err());
    assert!(validate(&[attachment("file", dir.path(), "", "", 1)]).is_err());
    assert!(validate(&[attachment("script", &file, "", "", 1)]).is_err());
    let many: Vec<_> = (0..=MAX_TURN_ATTACHMENTS)
        .map(|_| attachment("file", &file, "", "", 1))
        .collect();
    assert!(validate(&many).is_err());
}
