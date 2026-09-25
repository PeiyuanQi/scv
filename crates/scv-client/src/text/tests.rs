//! Unit tests for src/text.rs.

use super::*;

#[test]
fn utf8_prefix_never_splits_a_character() {
    assert_eq!(utf8_prefix("hello", 10), "hello");
    assert_eq!(utf8_prefix("hello", 3), "hel");
    assert_eq!(utf8_prefix("héllo", 2), "h");
    assert_eq!(utf8_prefix("héllo", 3), "hé");
    assert_eq!(utf8_prefix("日本", 5), "日");
    assert_eq!(utf8_prefix("日本", 0), "");
}
