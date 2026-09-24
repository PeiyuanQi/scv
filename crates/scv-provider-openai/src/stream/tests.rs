//! Unit tests for `src/stream.rs`.

use serde_json::json;

use super::*;

#[test]
fn sources_are_appended_only_when_the_text_does_not_link_them() {
    let citations = vec![
        Citation {
            title: "tokio 1.53.1 - Docs.rs".into(),
            url: "https://docs.rs/tokio".into(),
        },
        Citation {
            title: String::new(),
            url: "https://crates.io/crates/tokio".into(),
        },
    ];
    assert_eq!(
        sources_appendix("See [docs.rs](https://docs.rs/tokio).", &citations).unwrap(),
        "\n\nSources:\n- <https://crates.io/crates/tokio>"
    );
    assert_eq!(
        sources_appendix("Latest is 1.53.1.", &citations[..1]).unwrap(),
        "\n\nSources:\n- [tokio 1.53.1 - Docs.rs](https://docs.rs/tokio)"
    );
    assert!(
        sources_appendix(
            "https://docs.rs/tokio https://crates.io/crates/tokio",
            &citations
        )
        .is_none()
    );
    let mut collected = Vec::new();
    for annotation in [
        json!({"type":"url_citation","url":"https://a.test","title":"A"}),
        json!({"type":"url_citation","url":"https://a.test","title":"again"}),
        json!({"type":"file_citation","file_id":"f"}),
    ] {
        add_citation(&mut collected, &annotation);
    }
    assert_eq!(collected.len(), 1);
}
