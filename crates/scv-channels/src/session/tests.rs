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

#[test]
fn a_task_is_the_first_line_of_the_prompt_shortened() {
    assert_eq!(task_line("\n  Fix the build\nthen test"), "Fix the build");
    let long = "x".repeat(200);
    let task = task_line(&long);
    assert_eq!(task.chars().count(), TASK_CHARS + 1);
    assert!(task.ends_with('…'));
    assert_eq!(task_line(""), "");
}
