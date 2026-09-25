//! Unit tests for src/frame.rs.

use super::*;

/// Feed `input` in `chunk`-sized buffers, as a reader would hand it over,
/// and collect every frame through the end of input.
fn decode(decoder: &mut FrameDecoder, input: &[u8], chunk: usize) -> Vec<Frame> {
    let mut frames = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        let available = &rest[..rest.len().min(chunk)];
        let step = decoder.feed(available);
        if step.consumed == 0 {
            frames.extend(step.frame);
            return frames;
        }
        rest = &rest[step.consumed..];
        frames.extend(step.frame);
    }
    frames.push(decoder.finish());
    frames
}

#[test]
fn lines_split_on_newlines_across_any_buffer_size() {
    for chunk in [1, 2, 3, 7, 64] {
        let mut decoder = FrameDecoder::new(64, Overflow::Stop);
        assert_eq!(
            decode(&mut decoder, b"{\"a\":1}\n{\"b\":2}\n", chunk),
            vec![
                Frame::Line(b"{\"a\":1}".to_vec()),
                Frame::Line(b"{\"b\":2}".to_vec()),
                Frame::End
            ],
            "chunk {chunk}"
        );
    }
}

#[test]
fn the_limit_counts_the_newline() {
    let mut decoder = FrameDecoder::new(4, Overflow::Stop);
    assert_eq!(
        decode(&mut decoder, b"abc\n", 64),
        vec![Frame::Line(b"abc".to_vec()), Frame::End]
    );
    let mut decoder = FrameDecoder::new(4, Overflow::Stop);
    assert_eq!(decode(&mut decoder, b"abcd\n", 64), vec![Frame::TooLarge]);
}

#[test]
fn stop_reports_an_overflow_before_consuming_it() {
    let mut decoder = FrameDecoder::new(4, Overflow::Stop);
    let step = decoder.feed(b"ab");
    assert_eq!(step.consumed, 2);
    assert_eq!(step.frame, None);
    let step = decoder.feed(b"cde\n");
    assert_eq!(
        step,
        Step {
            consumed: 0,
            frame: Some(Frame::TooLarge)
        }
    );
}

#[test]
fn skip_discards_the_long_line_and_keeps_going() {
    for chunk in [1, 3, 64] {
        let mut decoder = FrameDecoder::new(4, Overflow::Skip);
        assert_eq!(
            decode(&mut decoder, b"far too long\nok\n", chunk),
            vec![Frame::TooLarge, Frame::Line(b"ok".to_vec()), Frame::End],
            "chunk {chunk}"
        );
    }
}

#[test]
fn input_ending_mid_line_is_truncated_or_too_large() {
    let mut decoder = FrameDecoder::new(64, Overflow::Stop);
    assert_eq!(
        decode(&mut decoder, b"{\"a\":1}\npart", 64),
        vec![
            Frame::Line(b"{\"a\":1}".to_vec()),
            Frame::Truncated(b"part".to_vec())
        ]
    );
    let mut decoder = FrameDecoder::new(4, Overflow::Skip);
    assert_eq!(decode(&mut decoder, b"endless", 64), vec![Frame::TooLarge]);
}

#[test]
fn a_raised_limit_applies_to_the_next_bytes() {
    let mut decoder = FrameDecoder::new(4, Overflow::Stop);
    assert_eq!(decoder.feed(b"ab").consumed, 2);
    assert!(!decoder.is_empty());
    decoder.set_limit(16);
    assert_eq!(
        decoder.feed(b"cdefg\n").frame,
        Some(Frame::Line(b"abcdefg".to_vec()))
    );
    assert!(decoder.is_empty());
}

#[test]
fn trim_line_strips_carriage_returns_then_checks_the_limit() {
    assert_eq!(trim_line(b"abc\r".to_vec(), 3), Some(b"abc".to_vec()));
    assert_eq!(trim_line(b"abc\r\r".to_vec(), 3), Some(b"abc".to_vec()));
    assert_eq!(trim_line(b"abcd\r".to_vec(), 3), None);
    assert_eq!(trim_line(Vec::new(), 0), Some(Vec::new()));
}

#[test]
fn encode_frame_appends_one_newline() {
    assert_eq!(
        encode_frame(&serde_json::json!({"a": 1})).unwrap(),
        b"{\"a\":1}\n"
    );
}
