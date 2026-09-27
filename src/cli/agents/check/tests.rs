//! Unit tests for `src/cli/agents/check.rs`.

use super::*;

#[test]
fn an_excerpt_is_one_bounded_line_without_control_characters() {
    assert_eq!(excerpt("  ok\n"), "ok");
    assert_eq!(excerpt("a\u{1b}[31mb\nc"), "a [31mb c");
    let long = "x".repeat(EXCERPT_CHARS + 5);
    let cut = excerpt(&long);
    assert_eq!(cut.chars().count(), EXCERPT_CHARS + 1);
    assert!(cut.ends_with('…'));
    assert_eq!(
        excerpt(&"y".repeat(EXCERPT_CHARS)),
        "y".repeat(EXCERPT_CHARS)
    );
}

#[test]
fn ages_read_in_the_largest_whole_unit() {
    assert_eq!(ago(600), "10 minutes");
    assert_eq!(ago(7200), "2 hours");
    assert_eq!(ago(3 * 86400), "3 days");
}
