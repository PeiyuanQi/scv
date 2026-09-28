//! Unit tests for `src/email/parse.rs`.

use super::*;

#[test]
fn msg_id_keeps_brackets_and_case_and_drops_comments_and_space() {
    assert_eq!(
        msg_id(b" (a comment) <Abc.123@Example.COM> ").as_deref(),
        Some("<Abc.123@Example.COM>")
    );
    assert_eq!(
        msg_id(b"\r\n <x@[10.0.0.1]>").as_deref(),
        Some("<x@[10.0.0.1]>")
    );
    // Folded across lines and with a nested comment.
    assert_eq!(
        msg_id(b"<a@b> (one (two) \\) three)").as_deref(),
        Some("<a@b>")
    );
}

#[test]
fn msg_id_refuses_what_is_not_a_msg_id() {
    for value in [
        &b""[..],
        b"no brackets@example.com",
        b"<no-at-sign>",
        b"<@example.com>",
        b"<a@>",
        b"<a..b@example.com>",
        b"<\"quoted\"@example.com>",
        b"<a@b@c>",
        b"<.a@b>",
    ] {
        assert_eq!(msg_id(value), None, "{:?}", String::from_utf8_lossy(value));
    }
    let long = format!("<{}@example.com>", "a".repeat(240));
    assert_eq!(msg_id(long.as_bytes()), None);
    let fits = format!("<{}@example.com>", "a".repeat(236));
    assert_eq!(fits.len(), MAX_MSG_ID_BYTES);
    assert_eq!(msg_id(fits.as_bytes()), Some(fits));
}

const DATE: &str = "01-Jan-2026 00:00:00 +0000";

/// The identity of a message in INBOX from `a@example.com`, subject `Hi`.
fn inbox(message_id: Option<&[u8]>, received: &str, size: u64) -> String {
    identity("INBOX", message_id, received, size, "a@example.com", "Hi")
}

#[test]
fn identity_is_one_stored_message_not_one_message_id() {
    let with_id = inbox(Some(b" <a@b> "), DATE, 10);
    assert_eq!(with_id.len(), 64);
    assert!(
        with_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
    // The same message, however its Message-ID was written, is one.
    assert_eq!(with_id, inbox(Some(b"(c)<a@b>"), DATE, 10));
    // A Message-ID reused by another delivery, or another sender, is not.
    for other in [
        inbox(Some(b"<a@b>"), "01-Jan-2026 00:00:01 +0000", 10),
        inbox(Some(b"<a@b>"), DATE, 11),
        identity("INBOX", Some(b"<a@b>"), DATE, 10, "b@example.com", "Hi"),
        identity("INBOX", Some(b"<a@b>"), DATE, 10, "a@example.com", "Re: Hi"),
        identity("Archive", Some(b"<a@b>"), DATE, 10, "a@example.com", "Hi"),
    ] {
        assert_ne!(with_id, other);
    }
    // Case is part of a Message-ID.
    assert_ne!(
        inbox(Some(b"<A@b>"), DATE, 10),
        inbox(Some(b"<a@b>"), DATE, 10)
    );
    // An invalid Message-ID counts as none.
    let without = inbox(None, DATE, 10);
    assert_ne!(without, with_id);
    assert_eq!(without, inbox(Some(b"not an id"), DATE, 10));
}

#[test]
fn messages_without_an_id_in_the_same_second_with_the_same_size_stay_apart() {
    let one = identity("INBOX", None, DATE, 10, "a@example.com", "Alert");
    assert_ne!(
        one,
        identity("INBOX", None, DATE, 10, "b@example.com", "Alert")
    );
    assert_ne!(
        one,
        identity("INBOX", None, DATE, 10, "a@example.com", "Other")
    );
    assert_eq!(
        one,
        identity("INBOX", None, DATE, 10, "a@example.com", "Alert")
    );
    // Each field is framed by its length, so moving bytes between two
    // fields makes another identity.
    assert_ne!(
        identity("INBOX", None, DATE, 10, "ab", "c"),
        identity("INBOX", None, DATE, 10, "a", "bc")
    );
}

#[test]
fn digest_is_hex_sha256() {
    let expected: String = {
        use sha2::Digest as _;
        sha2::Sha256::digest(b"<a@b>")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    };
    assert_eq!(digest(b"<a@b>"), expected);
}

#[test]
fn a_huge_header_value_is_decoded_only_so_far() {
    let raw = "=?utf-8?Q?x?= ".repeat(100_000);
    let decoded = decode_words(raw.as_bytes());
    assert!(decoded.len() <= MAX_HEADER_VALUE_BYTES, "{}", decoded.len());
    assert!(decoded.starts_with("xx"));
}

#[test]
fn header_fields_unfold_lowercase_and_stop_at_the_blank_line() {
    let block = b"Message-ID: <a@b>\r\nList-Id: Team\r\n <team.example.com>\r\n\
X-Bad Name: skipped\r\nnot a field\r\nPrecedence:  bulk \r\n\r\nBody: not a header\r\n";
    let fields = header_fields(block);
    assert_eq!(
        fields,
        vec![
            ("message-id".to_owned(), b"<a@b>".to_vec()),
            ("list-id".to_owned(), b"Team <team.example.com>".to_vec()),
            ("precedence".to_owned(), b"bulk".to_vec()),
        ]
    );
    assert!(header_fields(b"").is_empty());
}

#[test]
fn decode_words_unfolds_and_trims_plain_text() {
    assert_eq!(decode_words(b""), "");
    assert_eq!(decode_words(b"  Hello  "), "Hello");
    assert_eq!(decode_words(b"Hello\r\n World"), "Hello World");
    assert_eq!(decode_words(b"Hello\n\tWorld"), "Hello\tWorld");
    // Bare line breaks are dropped, not turned into spaces.
    assert_eq!(decode_words(b"a\rb\nc\r\n"), "abc");
}

#[test]
fn decode_words_decodes_b_and_q_words_in_any_case() {
    assert_eq!(decode_words(b"=?UTF-8?B?5Lit5paH?="), "中文");
    assert_eq!(decode_words(b"=?utf-8?b?5Lit5paH?="), "中文");
    assert_eq!(
        decode_words(b"=?iso-8859-1?q?caf=E9_au_lait?="),
        "café au lait"
    );
    assert_eq!(decode_words(b"=?UTF-8?Q?caf=c3=a9?="), "café");
    // A stray `=` in Q text stays as it is.
    assert_eq!(decode_words(b"=?utf-8?q?1=2?="), "1=2");
    // Unpadded base64 and an RFC 2231 language.
    assert_eq!(decode_words(b"=?utf-8?B?YQ?="), "a");
    assert_eq!(decode_words(b"=?utf-8*en?Q?hi?="), "hi");
    assert_eq!(decode_words(b"=?utf-8?q??="), "");
}

#[test]
fn decode_words_drops_space_only_between_adjacent_encoded_words() {
    assert_eq!(decode_words(b"=?utf-8?q?a?= =?utf-8?q?b?="), "ab");
    assert_eq!(
        decode_words(b"=?utf-8?q?a?=\r\n =?UTF-8?Q?b?=\r\n\t=?utf-8?q?c?="),
        "abc"
    );
    assert_eq!(
        decode_words(b"Re: =?utf-8?q?caf=C3=A9?= now"),
        "Re: café now"
    );
    assert_eq!(decode_words(b"=?utf-8?q?a?= and =?utf-8?q?b?="), "a and b");
    // Encoded spaces are kept.
    assert_eq!(decode_words(b"=?utf-8?q?a_?= =?utf-8?q?_b?="), "a  b");
    // Adjacent words in different charsets are each decoded.
    assert_eq!(
        decode_words(b"=?iso-8859-1?q?caf=E9?= =?utf-8?q?=E4=B8=AD?="),
        "café中"
    );
}

#[test]
fn decode_words_joins_a_character_split_across_encoded_words() {
    // U+4E2D is E4 B8 AD in UTF-8.
    assert_eq!(decode_words(b"=?UTF-8?B?5Lg=?= =?UTF-8?B?rQ==?="), "中");
    assert_eq!(
        decode_words(b"=?utf-8?q?=E4=B8?=\r\n =?utf-8?q?=AD=E6?= =?utf-8?b?lof=?="),
        "中文"
    );
    // Charset labels that name the same encoding still join.
    assert_eq!(decode_words(b"=?utf-8?q?=E4=B8?= =?UTF8?q?=AD?="), "中");
    // A different charset in between breaks the character.
    assert_eq!(
        decode_words(b"=?utf-8?q?=E4=B8?= =?iso-8859-1?q?x?= =?utf-8?q?=AD?="),
        "\u{FFFD}x\u{FFFD}"
    );
}

#[test]
fn decode_words_reads_chinese_japanese_and_korean_charsets() {
    // `gb2312` is GBK in encoding_rs.
    assert_eq!(decode_words(b"=?gb2312?B?1tDOxA==?="), "中文");
    assert_eq!(decode_words(b"=?GBK?Q?=D6=D0=CE=C4?="), "中文");
    assert_eq!(decode_words(b"=?gb18030?B?1tDOxA==?="), "中文");
    assert_eq!(decode_words(b"=?big5?B?pKSk5Q==?="), "中文");
    assert_eq!(decode_words(b"=?shift_jis?B?k/qWe4zq?="), "日本語");
    assert_eq!(
        decode_words(b"=?ISO-2022-JP?B?GyRCRnxLXDhsGyhC?="),
        "日本語"
    );
    assert_eq!(decode_words(b"=?euc-kr?B?x9Gx2w==?="), "한글");
}

#[test]
fn decode_words_reads_raw_bytes_as_utf8_or_else_gb18030() {
    assert_eq!(decode_words("中文 subject".as_bytes()), "中文 subject");
    assert_eq!(decode_words(b"\xd6\xd0\xce\xc4 subject"), "中文 subject");
    // Raw text next to an encoded word.
    assert_eq!(
        decode_words(b"\xd6\xd0\xce\xc4 =?utf-8?q?caf=C3=A9?="),
        "中文 café"
    );
    // Four-byte GB18030 sequences too.
    let (bytes, _, _) = encoding_rs::GB18030.encode("𠀀 ok");
    assert_eq!(decode_words(&bytes), "𠀀 ok");
}

#[test]
fn decode_words_leaves_malformed_or_unknown_words_as_written() {
    for raw in [
        "=?x-unknown?q?abc?=",
        "=?utf-7?q?abc?=",
        "=?utf-8?x?abc?=",
        "=?utf-8?b?@@@@?=",
        "=?utf-8?b?YQ===?=",
        "=?utf-8?b?Y?=",
        "=?utf-8?q?abc",
        "=?utf-8?q?abc?",
        "=??q?abc?=",
        "=?utf 8?q?abc?=",
        "=?utf-8?q",
        "=?",
        "=",
    ] {
        assert_eq!(decode_words(raw.as_bytes()), raw, "{raw}");
    }
    // A malformed word does not stop the next one decoding.
    assert_eq!(
        decode_words(b"=?bad?q?x?= =?utf-8?q?ok?="),
        "=?bad?q?x?= ok"
    );
    // The replacement encoding decodes nothing useful, so it counts as unknown.
    assert_eq!(
        decode_words(b"=?iso-2022-kr?q?abc?="),
        "=?iso-2022-kr?q?abc?="
    );
}

#[test]
fn decode_words_keeps_a_decoded_value_on_one_line() {
    assert_eq!(
        decode_words(b"=?utf-8?q?a=0D=0Aapprove_ABC123?="),
        "a  approve ABC123"
    );
    assert!(!decode_words(b"=?utf-8?b?YQpi?=").contains('\n'));
}

#[test]
fn decode_words_stays_linear_on_hostile_input() {
    let cases = [
        b"=?".repeat(200_000),
        b"=?utf-8?q?".repeat(40_000),
        [&b"=?utf-8?q?"[..], &b"x".repeat(400_000)].concat(),
        b"=?utf-8?q?a?= ".repeat(30_000),
        b"=?a=?b?=?".repeat(50_000),
        b"\xff\xfe".repeat(200_000),
    ];
    for raw in cases {
        let start = std::time::Instant::now();
        let text = decode_words(&raw);
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        assert!(text.len() <= raw.len() * 3);
    }
    assert_eq!(
        decode_words(&b"=?utf-8?q?a?= ".repeat(1_000)),
        "a".repeat(1_000)
    );
}

#[test]
fn decode_body_undoes_base64_leniently() {
    let b64 = TransferEncoding::Base64;
    assert_eq!(decode_body(b"SGVs\r\nbG8=\r\n", b64, None), "Hello");
    // Characters outside the alphabet are skipped.
    assert_eq!(decode_body(b"SG!Vs*bG8\t=", b64, Some("utf-8")), "Hello");
    // Concatenated padded streams.
    assert_eq!(decode_body(b"SGk=SGk=", b64, None), "HiHi");
    assert_eq!(decode_body(b"SGk", b64, None), "");
    assert_eq!(decode_body(b"", b64, None), "");
    assert_eq!(decode_body(b"====", b64, None), "");
}

#[test]
fn decode_body_drops_an_incomplete_base64_quantum() {
    let b64 = TransferEncoding::Base64;
    // "Hello World" cut at each point after "Hello Wor".
    assert_eq!(decode_body(b"SGVsbG8gV29y", b64, None), "Hello Wor");
    assert_eq!(decode_body(b"SGVsbG8gV29ybA", b64, None), "Hello Wor");
    assert_eq!(decode_body(b"SGVsbG8gV29ybG", b64, None), "Hello Wor");
    assert_eq!(decode_body(b"SGVsbG8gV29ybGQ", b64, None), "Hello Wor");
    assert_eq!(decode_body(b"SGVsbG8gV29ybGQ=", b64, None), "Hello World");
    // A cut inside a UTF-8 character drops the character.
    assert_eq!(decode_body(b"5Lit5pa", b64, None), "中");
    assert_eq!(decode_body(b"5Lit5paH", b64, None), "中文");
}

#[test]
fn decode_body_undoes_quoted_printable_leniently() {
    let qp = TransferEncoding::QuotedPrintable;
    assert_eq!(decode_body(b"a=\r\nb=\nc", qp, None), "abc");
    // Blanks before a soft break are the transport's.
    assert_eq!(decode_body(b"a= \t\r\nb", qp, None), "ab");
    assert_eq!(decode_body(b"=3D=3d=41", qp, None), "==A");
    assert_eq!(
        decode_body(b"caf=C3=A9 caf=c3=a9", qp, Some("utf-8")),
        "café café"
    );
    // Invalid sequences stay as they are.
    assert_eq!(decode_body(b"a=ZZb =4g =\tx", qp, None), "a=ZZb =4g =\tx");
    assert_eq!(decode_body(b"line\r\nnext", qp, None), "line\r\nnext");
}

#[test]
fn decode_body_drops_a_quoted_printable_sequence_cut_at_the_end() {
    let qp = TransferEncoding::QuotedPrintable;
    assert_eq!(decode_body(b"abc=", qp, None), "abc");
    assert_eq!(decode_body(b"abc=4", qp, None), "abc");
    assert_eq!(decode_body(b"abc=\r", qp, None), "abc");
    assert_eq!(decode_body(b"abc=  ", qp, None), "abc");
    assert_eq!(decode_body(b"abc=Z", qp, None), "abc=Z");
    // A cut inside a character: its first byte arrived, the rest did not.
    assert_eq!(decode_body(b"caf=C3", qp, Some("utf-8")), "caf");
    assert_eq!(decode_body(b"caf=C3=A", qp, Some("utf-8")), "caf");
}

#[test]
fn decode_body_passes_other_transfer_encodings_through() {
    for encoding in [
        TransferEncoding::SevenBit,
        TransferEncoding::EightBit,
        TransferEncoding::Binary,
        TransferEncoding::Other,
    ] {
        assert_eq!(decode_body(b"a=3Db SGk=", encoding, None), "a=3Db SGk=");
        assert_eq!(decode_body("中文".as_bytes(), encoding, None), "中文");
    }
}

#[test]
fn decode_body_reads_each_charset() {
    let text = "中文 日本語 test";
    for (label, sample) in [
        ("gb2312", "中文 简体"),
        ("GBK", "中文 简体"),
        ("gb18030", "中文 𠀀 €"),
        ("big5", "中文 繁體"),
        ("iso-2022-jp", "日本語 テキスト"),
        ("shift_jis", "日本語 テキスト"),
        ("euc-jp", "日本語"),
        ("euc-kr", "한국어"),
        ("windows-1252", "café € naïve"),
        ("iso-8859-1", "café naïve"),
        ("koi8-r", "Привет"),
        ("utf-8", text),
        ("UTF-8", text),
    ] {
        let encoding = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap();
        let (bytes, _, unmappable) = encoding.encode(sample);
        assert!(!unmappable, "{label}");
        assert_eq!(
            decode_body(&bytes, TransferEncoding::EightBit, Some(label)),
            sample,
            "{label}"
        );
    }
    // Hand-written bytes, independent of the encoder.
    assert_eq!(
        decode_body(
            b"\xd6\xd0\xce\xc4",
            TransferEncoding::EightBit,
            Some("gb2312")
        ),
        "中文"
    );
    assert_eq!(
        decode_body(
            b"\xa4\xa4\xa4\xe5",
            TransferEncoding::EightBit,
            Some("Big5")
        ),
        "中文"
    );
    assert_eq!(
        decode_body(
            b"\x93\xfa\x96\x7b\x8c\xea",
            TransferEncoding::EightBit,
            Some("Shift_JIS")
        ),
        "日本語"
    );
    assert_eq!(
        decode_body(
            b"\x1b$BF|K\\8l\x1b(B",
            TransferEncoding::SevenBit,
            Some("ISO-2022-JP")
        ),
        "日本語"
    );
    assert_eq!(
        decode_body(
            b"caf\xe9 \x80",
            TransferEncoding::EightBit,
            Some("windows-1252")
        ),
        "café €"
    );
    // A quoted label, and base64 GBK.
    assert_eq!(
        decode_body(b"1tDOxA==", TransferEncoding::Base64, Some(" \"GB2312\" ")),
        "中文"
    );
}

#[test]
fn decode_body_drops_a_character_cut_at_the_end() {
    for (label, sample) in [
        ("gbk", "中文字"),
        ("gb18030", "中𠀀"),
        ("big5", "中文字"),
        ("shift_jis", "日本語"),
        ("euc-jp", "日本語"),
        ("iso-2022-jp", "日本語"),
        ("utf-8", "中文字"),
        ("utf-16le", "中文字"),
    ] {
        let encoding = encoding_rs::Encoding::for_label(label.as_bytes()).unwrap();
        let bytes: Vec<u8> = if label == "utf-16le" {
            sample.encode_utf16().flat_map(u16::to_le_bytes).collect()
        } else {
            encoding.encode(sample).0.into_owned()
        };
        for cut in 0..=bytes.len() {
            let text = decode_body(&bytes[..cut], TransferEncoding::EightBit, Some(label));
            assert!(
                text.chars().filter(|&c| c == '\u{FFFD}').count() == 0,
                "{label} cut at {cut}: {text:?}"
            );
            assert!(sample.starts_with(&text), "{label} cut at {cut}: {text:?}");
        }
    }
}

#[test]
fn decode_body_falls_back_to_utf8() {
    let raw = "naïve".as_bytes();
    for charset in [
        None,
        Some("x-unknown"),
        Some("utf-7"),
        Some(""),
        Some("replacement"),
    ] {
        assert_eq!(
            decode_body(raw, TransferEncoding::EightBit, charset),
            "naïve"
        );
    }
    // Invalid bytes become replacement characters.
    assert_eq!(
        decode_body(b"a\xffb\xc3", TransferEncoding::EightBit, None),
        "a\u{FFFD}b"
    );
    assert_eq!(
        decode_body(b"\xed\xa0\x80x", TransferEncoding::EightBit, Some("utf-8")),
        "\u{FFFD}\u{FFFD}\u{FFFD}x"
    );
}

#[test]
fn decode_body_reads_mislabelled_us_ascii_as_utf8_when_it_is() {
    assert_eq!(
        decode_body(
            "café".as_bytes(),
            TransferEncoding::EightBit,
            Some("us-ascii")
        ),
        "café"
    );
    assert_eq!(
        decode_body(
            &"café 中".as_bytes()[..8],
            TransferEncoding::EightBit,
            Some("ascii")
        ),
        "café "
    );
    // Without a complete UTF-8 character, 8-bit bytes read as windows-1252.
    assert_eq!(
        decode_body(b"caf\xe9", TransferEncoding::EightBit, Some("US-ASCII")),
        "café"
    );
    // Other labels are trusted.
    assert_eq!(
        decode_body(
            "é".as_bytes(),
            TransferEncoding::EightBit,
            Some("iso-8859-1")
        ),
        "Ã©"
    );
}

#[test]
fn decode_body_strips_a_leading_byte_order_mark() {
    assert_eq!(
        decode_body(b"\xef\xbb\xbfhi", TransferEncoding::EightBit, Some("utf-8")),
        "hi"
    );
    assert_eq!(
        decode_body(b"\xef\xbb\xbfhi", TransferEncoding::EightBit, None),
        "hi"
    );
    assert_eq!(
        decode_body(b"77u/aGk=", TransferEncoding::Base64, None),
        "hi"
    );
    // Only a leading one.
    assert_eq!(
        decode_body("a\u{FEFF}b".as_bytes(), TransferEncoding::EightBit, None),
        "a\u{FEFF}b"
    );
}

#[test]
fn decode_body_handles_large_input() {
    let raw = "SGVsbG8g".repeat(100_000) + "\r\n";
    let text = decode_body(raw.as_bytes(), TransferEncoding::Base64, Some("utf-8"));
    assert_eq!(text.len(), 600_000);
    let raw = "=".repeat(300_000) + &"= ".repeat(100_000);
    let text = decode_body(raw.as_bytes(), TransferEncoding::QuotedPrintable, None);
    assert!(text.len() <= raw.len());
}
