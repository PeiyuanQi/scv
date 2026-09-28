//! Unit tests for `src/email/imap/structure.rs`.

use super::*;
use crate::email::imap::wire::{self, Response};

/// A BODYSTRUCTURE value, parsed as a FETCH response carries it.
fn structure(raw: &str) -> Value {
    match wire::parse(format!("* 1 FETCH (BODYSTRUCTURE {raw})\r\n").as_bytes()).unwrap() {
        Response::Message { values, .. } => values[0].list().unwrap()[1].clone(),
        other => panic!("not a FETCH: {other:?}"),
    }
}

fn part(id: &str, mime: &str, charset: Option<&str>, encoding: &str, size: u64) -> PartRef {
    PartRef {
        id: id.to_owned(),
        mime: mime.to_owned(),
        charset: charset.map(str::to_owned),
        encoding: TransferEncoding::parse(encoding),
        size,
    }
}

fn attachment(name: &str, mime: &str, size: u64) -> AttachmentInfo {
    AttachmentInfo {
        name: name.to_owned(),
        mime: mime.to_owned(),
        size,
    }
}

const PLAIN: &str = "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"QUOTED-PRINTABLE\" 120 4 NIL NIL NIL NIL)";
const HTML: &str =
    "(\"TEXT\" \"HTML\" (\"CHARSET\" \"UTF-8\") NIL NIL \"BASE64\" 800 11 NIL NIL NIL NIL)";

#[test]
fn a_single_text_part_is_part_one() {
    let layout = layout(&structure(
        "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"US-ASCII\") NIL NIL \"7BIT\" 3028 92)",
    ));
    assert_eq!(
        layout,
        Layout {
            text: Some(part("1", "text/plain", Some("us-ascii"), "7bit", 3028)),
            attachments: vec![],
        }
    );
}

#[test]
fn alternative_prefers_plain_over_html() {
    let layout = layout(&structure(&format!(
        "({PLAIN}{HTML} \"ALTERNATIVE\" (\"BOUNDARY\" \"b1\") NIL NIL)"
    )));
    assert_eq!(
        layout.text,
        Some(part(
            "1",
            "text/plain",
            Some("utf-8"),
            "quoted-printable",
            120
        ))
    );
    assert!(layout.attachments.is_empty());
    // Order does not matter: plain wins wherever it is.
    let layout = layout_of(&format!("({HTML}{PLAIN} \"ALTERNATIVE\")"));
    assert_eq!(layout.text.unwrap().id, "2");
}

fn layout_of(raw: &str) -> Layout {
    layout(&structure(raw))
}

#[test]
fn mixed_lists_a_pdf_and_an_inline_image_as_attachments() {
    let layout = layout_of(&format!(
        "({HTML}\
         (\"IMAGE\" \"PNG\" (\"NAME\" \"logo.png\") \"<logo@x>\" NIL \"BASE64\" 2000 NIL (\"INLINE\" NIL) NIL)\
         (\"IMAGE\" \"GIF\" NIL \"<spacer@x>\" NIL \"BASE64\" 43 NIL NIL NIL)\
         (\"APPLICATION\" \"PDF\" (\"NAME\" \"invoice.pdf\") NIL NIL \"BASE64\" 90000 NIL \
          (\"ATTACHMENT\" (\"FILENAME\" \"invoice.pdf\")) NIL) \
         \"MIXED\" (\"BOUNDARY\" \"b2\") NIL NIL)"
    ));
    assert_eq!(
        layout,
        Layout {
            text: Some(part("1", "text/html", Some("utf-8"), "base64", 800)),
            attachments: vec![
                attachment("logo.png", "image/png", 2000),
                attachment("", "image/gif", 43),
                attachment("invoice.pdf", "application/pdf", 90000),
            ],
        }
    );
}

#[test]
fn nested_multiparts_number_their_parts() {
    let layout = layout_of(&format!(
        "(({PLAIN}{HTML} \"ALTERNATIVE\" (\"BOUNDARY\" \"i\") NIL NIL)\
         (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\" \"NAME\" \"notes.txt\") NIL NIL \"7BIT\" 30 2 NIL \
          (\"ATTACHMENT\" (\"FILENAME\" \"notes.txt\")) NIL NIL) \
         \"MIXED\" (\"BOUNDARY\" \"o\") NIL NIL)"
    ));
    assert_eq!(
        layout.text,
        Some(part(
            "1.1",
            "text/plain",
            Some("utf-8"),
            "quoted-printable",
            120
        ))
    );
    assert_eq!(
        layout.attachments,
        vec![attachment("notes.txt", "text/plain", 30)]
    );

    let deeper = layout_of(&format!(
        "((\"APPLICATION\" \"PDF\" NIL NIL NIL \"BASE64\" 5 NIL NIL NIL)\
         (({HTML} \"RELATED\") \"ALTERNATIVE\") \"MIXED\")"
    ));
    assert_eq!(deeper.text.unwrap().id, "2.1.1");
}

#[test]
fn an_attached_text_part_is_never_the_text() {
    let layout = layout_of(&format!(
        "((\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 30 2 NIL (\"attachment\" NIL) NIL NIL){HTML} \"MIXED\")"
    ));
    assert_eq!(layout.text.unwrap().id, "2");
    assert_eq!(layout.attachments, vec![attachment("", "text/plain", 30)]);
}

#[test]
fn html_only() {
    assert_eq!(
        layout_of("(\"TEXT\" \"HTML\" (\"CHARSET\" \"GB2312\") NIL NIL \"BASE64\" 1000 20)"),
        Layout {
            text: Some(part("1", "text/html", Some("gb2312"), "base64", 1000)),
            attachments: vec![],
        }
    );
}

#[test]
fn an_attached_message_is_listed_and_never_entered() {
    let inner = "(\"MESSAGE\" \"RFC822\" NIL NIL NIL \"7BIT\" 400 \
                 (\"date\" \"inner subject\" NIL NIL NIL NIL NIL NIL NIL NIL) \
                 (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 50 3 NIL NIL NIL NIL) \
                 12 NIL (\"ATTACHMENT\" (\"FILENAME\" \"fwd.eml\")) NIL NIL)";
    let layout = layout_of(&format!(
        "({PLAIN}{inner} \"MIXED\" (\"BOUNDARY\" \"m\") NIL NIL)"
    ));
    assert_eq!(layout.text.unwrap().id, "1");
    assert_eq!(
        layout.attachments,
        vec![attachment("fwd.eml", "message/rfc822", 400)]
    );

    // Its text is not the message's own, even when there is no other.
    let bare = "(\"MESSAGE\" \"RFC822\" NIL NIL NIL \"7BIT\" 400 \
                (NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL) \
                (\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 50 3) 12)";
    assert_eq!(
        layout_of(&format!("({bare} \"MIXED\")")),
        Layout {
            text: None,
            attachments: vec![attachment("", "message/rfc822", 400)],
        }
    );
    assert_eq!(layout_of(bare).attachments.len(), 1);
}

#[test]
fn decodes_rfc_2231_file_names() {
    let named = |disposition: &str, params: &str| {
        layout_of(&format!(
            "(\"APPLICATION\" \"PDF\" {params} NIL NIL \"BASE64\" 100 NIL {disposition} NIL)"
        ))
        .attachments[0]
            .name
            .clone()
    };
    assert_eq!(
        named(
            "(\"ATTACHMENT\" (\"FILENAME*\" \"utf-8''%E6%8A%A5%E5%91%8A.pdf\"))",
            "NIL"
        ),
        "报告.pdf"
    );
    assert_eq!(
        named(
            "(\"ATTACHMENT\" (\"filename*0*\" \"utf-8''%E6%8A%A5\" \"filename*1*\" \"%E5%91%8A\" \"filename*2\" \".pdf\"))",
            "NIL"
        ),
        "报告.pdf"
    );
    assert_eq!(
        named("NIL", "(\"NAME*0\" \"long\" \"NAME*1\" \"name.txt\")"),
        "longname.txt"
    );
    assert_eq!(
        named("NIL", "(\"NAME*\" \"utf-8'en'%41%42.txt\")"),
        "AB.txt"
    );
    assert_eq!(
        named(
            "(\"ATTACHMENT\" (\"FILENAME*\" \"utf-8''100%.txt\"))",
            "NIL"
        ),
        "100%.txt"
    );
    // The disposition's name wins over the type's.
    assert_eq!(
        named(
            "(\"ATTACHMENT\" (\"FILENAME\" \"a.pdf\"))",
            "(\"NAME\" \"b.pdf\")"
        ),
        "a.pdf"
    );
    // A gap ends the continuation.
    assert_eq!(named("NIL", "(\"NAME*0\" \"a\" \"NAME*2\" \"c\")"), "a");
}

#[test]
fn missing_extension_data_is_fine() {
    let layout = layout_of(
        "((\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 10 1)\
         (\"APPLICATION\" \"OCTET-STREAM\" (\"NAME\" \"a.bin\") NIL NIL \"BASE64\" 30) \"MIXED\")",
    );
    assert_eq!(
        layout,
        Layout {
            text: Some(part("1", "text/plain", None, "7bit", 10)),
            attachments: vec![attachment("a.bin", "application/octet-stream", 30)],
        }
    );
    // Some servers leave out the fields a well-formed part must have.
    let short = layout_of("(\"TEXT\" \"PLAIN\")");
    assert_eq!(short.text, Some(part("1", "text/plain", None, "7bit", 0)));
}

#[test]
fn keywords_may_be_lowercase() {
    assert_eq!(
        layout_of(
            "(\"text\" \"plain\" (\"charset\" \"UTF-8\") nil nil \"base64\" 10 1 nil (\"inline\" nil) nil nil)"
        )
        .text,
        Some(part("1", "text/plain", Some("utf-8"), "base64", 10))
    );
}

#[test]
fn a_name_parameter_makes_a_part_an_attachment() {
    let layout = layout_of(
        "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\" \"NAME\" \"readme.txt\") NIL NIL \"7BIT\" 10 1)",
    );
    assert_eq!(layout.text, None);
    assert_eq!(
        layout.attachments,
        vec![attachment("readme.txt", "text/plain", 10)]
    );
}

#[test]
fn other_text_types_are_neither_text_nor_attachments() {
    let layout = layout_of(&format!(
        "((\"TEXT\" \"CALENDAR\" NIL NIL NIL \"7BIT\" 10 1){HTML} \"MIXED\")"
    ));
    assert_eq!(layout.text.unwrap().id, "2");
    assert!(layout.attachments.is_empty());
}

#[test]
fn lists_at_most_sixteen_attachments() {
    let pdf = "(\"APPLICATION\" \"PDF\" NIL NIL NIL \"BASE64\" 1 NIL NIL NIL)";
    let layout = layout_of(&format!("({}{PLAIN} \"MIXED\")", pdf.repeat(20)));
    assert_eq!(layout.attachments.len(), MAX_ATTACHMENTS);
    assert_eq!(layout.text.unwrap().id, "21");
}

#[test]
fn structures_of_the_wrong_shape_have_no_layout() {
    for value in [
        Value::Nil,
        Value::Number(3),
        Value::List(vec![]),
        Value::Atom("TEXT".to_owned()),
    ] {
        assert_eq!(layout(&value), Layout::default());
    }
}
