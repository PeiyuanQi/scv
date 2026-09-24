//! Replaying the conversation as Responses input items.

use base64::Engine as _;
use scv_core::{ImageInput, Message};
use serde_json::{Value, json};

/// The largest image file sent inline; a bigger one is described instead.
pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
/// Images one request shows, newest first; older ones are described.
const MAX_REQUEST_IMAGES: usize = 8;
/// Image types the Responses API reads.
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Replays the conversation as Responses input items, and counts the images
/// it shows.
///
/// The Responses API answers a `function_call_output` only when the input also
/// carries its `function_call`, and rejects a `function_call` that has no
/// output, so every call and output are emitted as a pair. A turn cancelled
/// between a call and its result leaves an unanswered call in history, which
/// is closed with a synthetic failure before the next message.
///
/// With `images`, the newest [`MAX_REQUEST_IMAGES`] images of user messages
/// go as `input_image` items; the rest, and every image without `images`,
/// become a note in the message text.
pub(crate) fn response_input(messages: &[Message], images: bool) -> (Vec<Value>, usize) {
    let mut input = Vec::with_capacity(messages.len());
    let mut unanswered: Vec<&str> = Vec::new();
    let mut budget = if images { MAX_REQUEST_IMAGES } else { 0 };
    // Newest images first: count back from the end to know which to show.
    let mut show = vec![false; messages.len()];
    for (index, message) in messages.iter().enumerate().rev() {
        if let Message::User { images, .. } = message
            && !images.is_empty()
            && budget >= images.len()
        {
            budget -= images.len();
            show[index] = true;
        }
    }
    let mut shown = 0;
    for (index, message) in messages.iter().enumerate() {
        match message {
            Message::Tool {
                call_id,
                name,
                content,
                ..
            } => {
                if let Some(position) = unanswered.iter().position(|id| *id == call_id) {
                    unanswered.remove(position);
                } else {
                    input.push(function_call(call_id, name, "{}"));
                }
                input.push(function_call_output(call_id, content));
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                close_unanswered(&mut input, &mut unanswered);
                if !content.is_empty() || tool_calls.is_empty() {
                    input.push(json!({"role":"assistant","content":content}));
                }
                for call in tool_calls {
                    input.push(function_call(
                        &call.id,
                        &call.name,
                        &call.arguments.to_string(),
                    ));
                    unanswered.push(&call.id);
                }
            }
            Message::User { content, images } if !images.is_empty() => {
                close_unanswered(&mut input, &mut unanswered);
                let (item, count) = user_with_images(content, images, show[index]);
                shown += count;
                input.push(item);
            }
            Message::User { content, .. } | Message::HistoryNote { content } => {
                close_unanswered(&mut input, &mut unanswered);
                input.push(json!({"role":"user","content":content}));
            }
        }
    }
    close_unanswered(&mut input, &mut unanswered);
    (input, shown)
}

/// A user message with images: `input_text` plus one `input_image` per image
/// when `show`, each read from disk now. An image that cannot be shown is
/// named in the text instead, and the count says how many were shown.
pub(crate) fn user_with_images(content: &str, images: &[ImageInput], show: bool) -> (Value, usize) {
    let mut notes = String::new();
    let mut parts = Vec::new();
    for image in images {
        let name = image
            .path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let data = show.then(|| image_data_url(image)).flatten();
        match data {
            Some(url) => parts.push(json!({"type":"input_image","image_url":url})),
            None if !show => notes.push_str(&format!("\n[image {name}: not shown to you here]")),
            None => notes.push_str(&format!("\n[image {name}: could not be read]")),
        }
    }
    let shown = parts.len();
    let mut content_parts = vec![json!({"type":"input_text","text":format!("{content}{notes}")})];
    content_parts.extend(parts);
    (json!({"role":"user","content":content_parts}), shown)
}

/// `data:` URL of an image the Responses API reads, or `None` when the file
/// is gone, too large, or of another type.
pub(crate) fn image_data_url(image: &ImageInput) -> Option<String> {
    if !IMAGE_TYPES.contains(&image.mime.as_str()) {
        return None;
    }
    let metadata = std::fs::metadata(&image.path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let bytes = std::fs::read(&image.path).ok()?;
    Some(format!(
        "data:{};base64,{}",
        image.mime,
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

pub(crate) fn close_unanswered(input: &mut Vec<Value>, unanswered: &mut Vec<&str>) {
    for call_id in unanswered.drain(..) {
        input.push(function_call_output(
            call_id,
            "Tool call did not complete: the turn ended before it returned a result.",
        ));
    }
}

pub(crate) fn function_call(call_id: &str, name: &str, arguments: &str) -> Value {
    json!({"type":"function_call","call_id":call_id,"name":name,"arguments":arguments})
}

pub(crate) fn function_call_output(call_id: &str, output: &str) -> Value {
    json!({"type":"function_call_output","call_id":call_id,"output":output})
}

#[cfg(test)]
mod tests;
