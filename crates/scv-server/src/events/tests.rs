//! Unit tests for `src/events.rs`.

use super::*;

#[test]
fn progress_text_is_bounded_on_a_character_boundary() {
    let text = "é".repeat(400);
    let bounded = bounded_progress(text);
    assert!(bounded.len() <= scv_core::MAX_PROGRESS_EVENT_BYTES);
    assert!(bounded.chars().all(|character| character == 'é'));
    assert_eq!(bounded_progress("short".into()), "short");
}
