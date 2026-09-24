//! Files a client attaches to its turn, such as media a chat user sent:
//! checked, listed for the model, and shown as images when it accepts them.

use std::path::Path;

use scv_core::{ImageInput, TurnInput};
use scv_protocol::{Attachment, MAX_TURN_ATTACHMENTS};

/// Kinds a client may attach.
const KINDS: [&str; 5] = ["image", "audio", "video", "file", "sticker"];
/// Image types the provider shows the model.
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
/// Longest attachment name or transcript kept for the model, in characters.
const MAX_NAME_CHARS: usize = 200;
const MAX_TRANSCRIPT_CHARS: usize = 4000;

/// Refuse attachments that are too many, of an unknown kind, or not an
/// absolute path to a regular file.
pub fn validate(attachments: &[Attachment]) -> Result<(), String> {
    if attachments.len() > MAX_TURN_ATTACHMENTS {
        return Err(format!(
            "a turn may attach at most {MAX_TURN_ATTACHMENTS} files"
        ));
    }
    for attachment in attachments {
        if !KINDS.contains(&attachment.kind.as_str()) {
            return Err(format!("unknown attachment kind {:?}", attachment.kind));
        }
        let path = Path::new(&attachment.path);
        if !path.is_absolute() {
            return Err("attachment paths must be absolute".into());
        }
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => {}
            _ => return Err("an attachment is not a regular file".into()),
        }
    }
    Ok(())
}

/// The model's input for `prompt` with `attachments`: the text, a list of
/// the files, and the images. A session with tools sees each file's path so
/// it can open it or hand it to an agent; one without sees names only.
pub fn turn_input(prompt: &str, attachments: &[Attachment], tools: bool) -> TurnInput {
    if attachments.is_empty() {
        return prompt.into();
    }
    let mut text = prompt.trim_end().to_owned();
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str("[The user attached:]");
    let mut images = Vec::new();
    for attachment in attachments {
        let image = attachment.kind == "image"
            && IMAGE_TYPES.contains(&attachment.mime.as_str())
            && attachment.size <= scv_provider_openai::MAX_IMAGE_BYTES;
        text.push_str("\n- ");
        text.push_str(&describe(attachment));
        if tools {
            text.push_str(&format!(" at {}", attachment.path));
        }
        if let Some(transcript) = attachment
            .transcript
            .as_deref()
            .filter(|transcript| !transcript.trim().is_empty())
        {
            text.push_str(&format!(
                "; it says: \"{}\"",
                bounded(transcript.trim(), MAX_TRANSCRIPT_CHARS)
            ));
        }
        if image {
            images.push(ImageInput {
                path: attachment.path.clone().into(),
                mime: attachment.mime.clone(),
            });
        }
    }
    if tools
        && attachments.iter().any(|attachment| {
            attachment.kind != "image" || !IMAGE_TYPES.contains(&attachment.mime.as_str())
        })
    {
        text.push_str(
            "\nOpen files you cannot read directly with a tool, or hand them to an agent that can.",
        );
    }
    TurnInput { text, images }
}

/// `image photo.jpg (image/jpeg, 120 KB)`.
fn describe(attachment: &Attachment) -> String {
    let kind = if attachment.kind == "audio" {
        "voice or audio"
    } else {
        attachment.kind.as_str()
    };
    let name = bounded(attachment.name.trim(), MAX_NAME_CHARS);
    let mut details = Vec::new();
    if !attachment.mime.is_empty() {
        details.push(attachment.mime.clone());
    }
    details.push(size(attachment.size));
    if name.is_empty() {
        format!("{kind} ({})", details.join(", "))
    } else {
        format!("{kind} {name} ({})", details.join(", "))
    }
}

fn size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    if bytes >= MIB {
        format!("{:.1} MB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{} KB", bytes.div_ceil(KIB))
    } else {
        format!("{bytes} bytes")
    }
}

fn bounded(value: &str, max_chars: usize) -> String {
    let mut output: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max_chars)
        .collect();
    if value.chars().count() > max_chars {
        output.push('…');
    }
    output
}

#[cfg(test)]
mod tests {
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
}
