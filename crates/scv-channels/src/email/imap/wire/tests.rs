//! Unit tests for `src/email/imap/wire.rs`.

use super::*;
use tokio::io::BufReader;

fn parsed(raw: &[u8]) -> Response {
    parse(raw).unwrap()
}

fn atom(text: &str) -> Value {
    Value::Atom(text.to_owned())
}

fn quoted(text: &str) -> Value {
    Value::Quoted(text.as_bytes().to_vec())
}

fn code(name: &str, args: Vec<Value>) -> Code {
    Code {
        name: name.to_owned(),
        args,
    }
}

/// The FETCH list of `* 1 FETCH (...)`.
fn fetch_list(raw: &[u8]) -> Vec<Value> {
    match parsed(raw) {
        Response::Message { kind, values, .. } if kind == "FETCH" => {
            values[0].list().unwrap().to_vec()
        }
        other => panic!("not a FETCH: {other:?}"),
    }
}

#[test]
fn parses_tagged_completions_with_their_codes() {
    assert_eq!(
        parsed(b"A0001 OK [READ-ONLY] EXAMINE completed\r\n"),
        Response::Tagged {
            tag: "A0001".to_owned(),
            status: Status::Ok,
            code: Some(code("READ-ONLY", vec![])),
            text: b"EXAMINE completed".to_vec(),
        }
    );
    assert_eq!(
        parsed(b"a7 no [unavailable] try later\n"),
        Response::Tagged {
            tag: "a7".to_owned(),
            status: Status::No,
            code: Some(code("UNAVAILABLE", vec![])),
            text: b"try later".to_vec(),
        }
    );
    assert_eq!(
        parsed(b"A2 BAD\r\n"),
        Response::Tagged {
            tag: "A2".to_owned(),
            status: Status::Bad,
            code: None,
            text: Vec::new(),
        }
    );
    // A bracket that never closes is text, not a code.
    assert_eq!(
        parsed(b"A3 NO [ALERT oops\r\n"),
        Response::Tagged {
            tag: "A3".to_owned(),
            status: Status::No,
            code: None,
            text: b"[ALERT oops".to_vec(),
        }
    );
}

#[test]
fn parses_untagged_status_data_and_continuations() {
    assert_eq!(
        parsed(b"* OK [UIDVALIDITY 3857529045] UIDs valid\r\n"),
        Response::Status {
            status: Status::Ok,
            code: Some(code("UIDVALIDITY", vec![Value::Number(3857529045)])),
            text: b"UIDs valid".to_vec(),
        }
    );
    assert_eq!(
        parsed(b"* OK [PERMANENTFLAGS ()] Read-only mailbox\r\n"),
        Response::Status {
            status: Status::Ok,
            code: Some(code("PERMANENTFLAGS", vec![Value::List(vec![])])),
            text: b"Read-only mailbox".to_vec(),
        }
    );
    assert_eq!(
        parsed(b"* PREAUTH [CAPABILITY IMAP4rev1 IDLE] welcome\r\n"),
        Response::Status {
            status: Status::PreAuth,
            code: Some(code("CAPABILITY", vec![atom("IMAP4rev1"), atom("IDLE")])),
            text: b"welcome".to_vec(),
        }
    );
    assert_eq!(
        parsed(b"* BYE Autologout\r\n"),
        Response::Status {
            status: Status::Bye,
            code: None,
            text: b"Autologout".to_vec(),
        }
    );
    assert_eq!(
        parsed(b"* capability IMAP4rev1 AUTH=PLAIN ID\r\n"),
        Response::Data {
            kind: "CAPABILITY".to_owned(),
            values: vec![atom("IMAP4rev1"), atom("AUTH=PLAIN"), atom("ID")],
        }
    );
    assert_eq!(
        parsed(b"* SEARCH 2 84 882 \r\n"),
        Response::Data {
            kind: "SEARCH".to_owned(),
            values: vec![Value::Number(2), Value::Number(84), Value::Number(882)],
        }
    );
    assert_eq!(
        parsed(b"* SEARCH\r\n"),
        Response::Data {
            kind: "SEARCH".to_owned(),
            values: vec![],
        }
    );
    assert_eq!(
        parsed(b"* 23 exists\r\n"),
        Response::Message {
            number: 23,
            kind: "EXISTS".to_owned(),
            values: vec![],
        }
    );
    assert_eq!(
        parsed(b"+ go ahead\r\n"),
        Response::Continuation {
            text: b"go ahead".to_vec(),
        }
    );
    assert_eq!(
        parsed(b"+\r\n"),
        Response::Continuation { text: Vec::new() }
    );
}

#[test]
fn parses_quoted_strings_literals_nil_and_nested_lists() {
    let raw = b"* 12 FETCH (UID 7 FLAGS (\\Seen $Junk) ENVELOPE (\"a \\\"b\\\" \\\\ c\" nil \
((\"x\" NIL \"u\" \"h\"))) RFC822.HEADER {11}\r\nHi: there\r\n)\r\n";
    assert_eq!(
        fetch_list(raw),
        vec![
            atom("UID"),
            Value::Number(7),
            atom("FLAGS"),
            Value::List(vec![atom("\\Seen"), atom("$Junk")]),
            atom("ENVELOPE"),
            Value::List(vec![
                quoted("a \"b\" \\ c"),
                Value::Nil,
                Value::List(vec![Value::List(vec![
                    quoted("x"),
                    Value::Nil,
                    quoted("u"),
                    quoted("h"),
                ])]),
            ]),
            atom("RFC822.HEADER"),
            Value::Literal(b"Hi: there\r\n".to_vec()),
        ]
    );
}

#[test]
fn parses_section_keys_with_field_lists_and_origins() {
    let raw = b"* 1 FETCH (UID 5 BODY[HEADER.FIELDS (MESSAGE-ID LIST-ID)] {4}\r\nab\r\n \
BODY[1.2]<0> \"hello\" body[TEXT] NIL BODY[] \"\")\r\n";
    assert_eq!(
        fetch_list(raw),
        vec![
            atom("UID"),
            Value::Number(5),
            Value::Section {
                name: "BODY".to_owned(),
                spec: "HEADER.FIELDS (MESSAGE-ID LIST-ID)".to_owned(),
                origin: None,
            },
            Value::Literal(b"ab\r\n".to_vec()),
            Value::Section {
                name: "BODY".to_owned(),
                spec: "1.2".to_owned(),
                origin: Some(0),
            },
            quoted("hello"),
            Value::Section {
                name: "body".to_owned(),
                spec: "TEXT".to_owned(),
                origin: None,
            },
            Value::Nil,
            Value::Section {
                name: "BODY".to_owned(),
                spec: String::new(),
                origin: None,
            },
            quoted(""),
        ]
    );
}

#[test]
fn tolerates_bare_line_feeds_binary_literals_and_tight_lists() {
    assert_eq!(
        fetch_list(b"* 1 FETCH (BODY[] ~{3}\n\x00\xff\n)\n"),
        vec![
            Value::Section {
                name: "BODY".to_owned(),
                spec: String::new(),
                origin: None,
            },
            Value::Literal(vec![0, 0xff, b'\n']),
        ]
    );
    // Multipart bodies follow each other with no space between them.
    assert_eq!(
        fetch_list(b"* 1 FETCH (BODYSTRUCTURE ((\"A\")(\"B\") \"MIXED\"))\r\n")[1],
        Value::List(vec![
            Value::List(vec![quoted("A")]),
            Value::List(vec![quoted("B")]),
            quoted("MIXED"),
        ])
    );
}

#[test]
fn malformed_responses_are_errors() {
    let samples: [&[u8]; 17] = [
        b"",
        b"*",
        b"* ",
        b"*OK\r\n",
        b"A1\r\n",
        b"A1 MAYBE done\r\n",
        b"A1 BYE done\r\n",
        b"* 1 FETCH (UID 1\r\n",
        b"* 1 FETCH (UID \"open\r\n",
        b"* 1 FETCH (BODY[1 \"x\")\r\n",
        b"* 1 FETCH ({5}\r\nab)\r\n",
        b"* 1 FETCH ({x}\r\nab)\r\n",
        b"* 1 FETCH ({2} ab)\r\n",
        b"* SEARCH )\r\n",
        b"* 1 FETCH (BODY[1]<x> NIL)\r\n",
        b"* 99999999999999999999999 EXISTS\r\n",
        b"* 1 FETCH ([1] NIL)\r\n",
    ];
    for sample in samples {
        assert!(
            parse(sample).is_err(),
            "{:?}",
            String::from_utf8_lossy(sample)
        );
    }
}

#[test]
fn nesting_is_bounded() {
    let nested = |depth: usize| {
        format!(
            "* 1 FETCH (X {}{})\r\n",
            "(".repeat(depth),
            ")".repeat(depth)
        )
    };
    assert!(parse(nested(MAX_DEPTH - 1).as_bytes()).is_ok());
    assert!(parse(nested(MAX_DEPTH).as_bytes()).is_err());
    assert!(parse(nested(50_000).as_bytes()).is_err());
}

#[test]
fn truncated_and_random_input_never_panics() {
    let samples: [&[u8]; 4] = [
        b"* 1 FETCH (UID 5 BODY[HEADER.FIELDS (MESSAGE-ID)]<0> {4}\r\nab\r\n FLAGS (\\Seen))\r\n",
        b"A0001 OK [CAPABILITY IMAP4rev1 AUTH=PLAIN] done\r\n",
        b"* 3 FETCH (ENVELOPE (\"d\" \"s\" ((\"n\" NIL \"m\" \"h\")) NIL NIL NIL NIL NIL NIL \"<i@d>\"))\r\n",
        b"* OK [UIDNEXT 4] x\r\n",
    ];
    for sample in samples {
        for end in 0..=sample.len() {
            let _ = parse(&sample[..end]);
        }
    }
    let alphabet = b"* ()[]{}<>\"\\~+.0123456789 ANILOKFETCH\r\n";
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..5_000 {
        let length = (next() % 80) as usize;
        let bytes: Vec<u8> = (0..length)
            .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
            .collect();
        let _ = parse(&bytes);
        let mut raw = b"* 1 FETCH (".to_vec();
        raw.extend_from_slice(&bytes);
        let _ = parse(&raw);
    }
}

#[tokio::test]
async fn reads_a_response_through_its_literals() {
    let input: &[u8] =
        b"* 1 FETCH (BODY[] {5}\r\nA\r\nB\r BODY[1] {0}\r\n)\r\n* 2 EXISTS\nA1 OK done\r\n";
    let mut reader = BufReader::new(input);
    let mut budget = MAX_RESPONSE;
    assert_eq!(
        read_response(&mut reader, &mut budget).await.unwrap(),
        b"* 1 FETCH (BODY[] {5}\r\nA\r\nB\r BODY[1] {0}\r\n)\r\n"
    );
    assert_eq!(
        read_response(&mut reader, &mut budget).await.unwrap(),
        b"* 2 EXISTS\n"
    );
    assert_eq!(
        read_response(&mut reader, &mut budget).await.unwrap(),
        b"A1 OK done\r\n"
    );
    assert_eq!(budget, MAX_RESPONSE - input.len());
    assert!(read_response(&mut reader, &mut budget).await.is_err());
}

#[tokio::test]
async fn reads_non_synchronizing_and_binary_literal_announcements() {
    let input: &[u8] = b"* 1 FETCH (BINARY[1] ~{2}\r\n\x00\x01 X {1+}\r\nZ)\r\n";
    let mut reader = BufReader::new(input);
    let mut budget = MAX_RESPONSE;
    let raw = read_response(&mut reader, &mut budget).await.unwrap();
    assert_eq!(raw, input);
}

#[tokio::test]
async fn a_cut_off_literal_is_an_error() {
    let input: &[u8] = b"* 1 FETCH (BODY[] {10}\r\nshort";
    let mut budget = MAX_RESPONSE;
    assert!(
        read_response(&mut BufReader::new(input), &mut budget)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn bounds_lines_literals_and_the_whole_response() {
    let mut budget = MAX_RESPONSE;
    let mut line = vec![b'a'; MAX_LINE - 2];
    line.extend_from_slice(b"\r\n");
    assert!(
        read_response(&mut BufReader::new(line.as_slice()), &mut budget)
            .await
            .is_ok()
    );
    line.insert(0, b'a');
    assert!(
        read_response(&mut BufReader::new(line.as_slice()), &mut budget)
            .await
            .is_err()
    );

    let announced = format!("* 1 FETCH (BODY[] {{{}}}\r\n", MAX_LITERAL + 1);
    let mut budget = MAX_RESPONSE;
    assert!(
        read_response(&mut BufReader::new(announced.as_bytes()), &mut budget)
            .await
            .is_err()
    );

    let mut budget = 10;
    assert!(
        read_response(
            &mut BufReader::new(&b"* OK hello world\r\n"[..]),
            &mut budget
        )
        .await
        .is_err()
    );
    let mut budget = 30;
    let literal: &[u8] = b"* 1 FETCH (BODY[] {40}\r\n";
    assert!(
        read_response(&mut BufReader::new(literal), &mut budget)
            .await
            .is_err()
    );
}
