//! Unit tests for `src/session.rs`.

use super::*;

#[test]
fn accumulated_reply_is_cut_with_a_note_at_the_byte_limit() {
    let limit = 32;
    let mut answer = String::new();
    append_capped(&mut answer, "hello ", limit);
    assert_eq!(answer, "hello ");
    append_capped(&mut answer, &"é".repeat(40), limit);
    assert!(answer.len() <= limit, "{} bytes", answer.len());
    assert!(answer.starts_with("hello é"));
    assert!(answer.ends_with(TRUNCATED_NOTE));
    let cut = answer.clone();
    append_capped(&mut answer, "more", limit);
    assert_eq!(answer, cut, "nothing follows the truncation note");
}
