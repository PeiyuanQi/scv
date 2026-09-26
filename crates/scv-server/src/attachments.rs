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
pub(crate) fn validate(attachments: &[Attachment]) -> Result<(), String> {
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
pub(crate) fn turn_input(prompt: &str, attachments: &[Attachment], tools: bool) -> TurnInput {
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
mod tests;
