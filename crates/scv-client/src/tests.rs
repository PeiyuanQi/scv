//! Unit tests for `src/lib.rs`.

use super::*;

#[test]
fn only_a_positive_inherited_depth_is_declared() {
    assert_eq!(parse_delegation_depth(Some("2")), Some(2));
    assert_eq!(parse_delegation_depth(Some(" 1\n")), Some(1));
    assert_eq!(parse_delegation_depth(Some("0")), None);
    assert_eq!(parse_delegation_depth(Some("deep")), None);
    assert_eq!(parse_delegation_depth(None), None);
}
