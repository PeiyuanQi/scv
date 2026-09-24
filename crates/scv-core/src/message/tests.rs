//! Unit tests for `src/message.rs`.

use super::*;
use crate::IMAGE_TOKENS;

#[test]
fn images_cost_a_fixed_amount_of_context_whatever_their_size() {
    let plain = Message::user("look");
    let with_image = Message::User {
        content: "look".into(),
        images: vec![ImageInput {
            path: "/media/huge.png".into(),
            mime: "image/png".into(),
        }],
    };
    let extra = with_image.estimated_tokens(4) - plain.estimated_tokens(4);
    assert!(
        (IMAGE_TOKENS..IMAGE_TOKENS + 20).contains(&extra),
        "{extra}"
    );
    // Older history without images reads back unchanged.
    let json = serde_json::to_string(&plain).unwrap();
    assert_eq!(json, r#"{"role":"user","content":"look"}"#);
    assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), plain);
}
