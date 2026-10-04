//! Unit tests for `src/mail_chat/codes.rs`.

use super::*;

#[cfg(feature = "email")]
#[test]
fn codes_and_handles_use_the_unambiguous_alphabet() {
    for _ in 0..200 {
        let code = new_code();
        assert_eq!(code.len(), CODE_LEN);
        assert!(code.bytes().all(|byte| ALPHABET.contains(&byte)), "{code}");
        let handle = new_handle();
        assert_eq!(handle.len(), HANDLE_LEN);
        assert!(
            handle.bytes().all(|byte| ALPHABET.contains(&byte)),
            "{handle}"
        );
    }
    for confusable in b"ILOU01" {
        assert!(!ALPHABET.contains(confusable));
    }
}

#[cfg(feature = "email")]
#[test]
fn codes_are_drawn_evenly_enough_to_differ() {
    let drawn: std::collections::HashSet<String> = (0..500).map(|_| new_code()).collect();
    assert!(drawn.len() > 490, "{} distinct of 500", drawn.len());
}

#[test]
fn reading_matches_case_insensitively_and_strips_a_handle_mark() {
    assert_eq!(code("q7m2kd").as_deref(), Some("Q7M2KD"));
    assert_eq!(code("Q7M2K"), None);
    assert_eq!(code("Q7M2KDX"), None);
    assert_eq!(code("Q7M2K0"), None);
    assert_eq!(code("Q7M2K!"), None);
    assert_eq!(handle("#4k7p").as_deref(), Some("4K7P"));
    assert_eq!(handle("＃4K7P").as_deref(), Some("4K7P"));
    assert_eq!(handle("4K7P").as_deref(), Some("4K7P"));
    assert_eq!(handle("#4K7"), None);
    assert_eq!(handle("##4K7P"), None);
}
