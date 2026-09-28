//! Decoding what a mail provider returns: header values (RFC 2047 encoded
//! words and raw 8-bit text), transfer encodings, and charsets, plus the
//! message identity every record uses.

use std::borrow::Cow;

use encoding_rs::{CoderResult, Encoding};

/// A MIME part's `Content-Transfer-Encoding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransferEncoding {
    SevenBit,
    EightBit,
    Binary,
    Base64,
    QuotedPrintable,
    /// Anything else, treated as 8-bit.
    Other,
}

impl TransferEncoding {
    /// The encoding a header or BODYSTRUCTURE names, case-insensitively.
    pub(crate) fn parse(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "7bit" => Self::SevenBit,
            "8bit" => Self::EightBit,
            "binary" => Self::Binary,
            "base64" => Self::Base64,
            "quoted-printable" => Self::QuotedPrintable,
            _ => Self::Other,
        }
    }
}

/// The most bytes of one header value decoded: well over what any field
/// keeps once decoded ([`super::source::Meta::bounded`]), and a bound on
/// the work one hostile header costs.
pub(crate) const MAX_HEADER_VALUE_BYTES: usize = 16 * 1024;

/// A header value as text: RFC 2047 encoded words decoded in any charset
/// `encoding_rs` knows, raw 8-bit bytes read as UTF-8 or else GB18030, and
/// folding removed. Adjacent encoded words in one charset are decoded
/// together, so a character split between them survives; an encoded word
/// that is malformed or names an unknown charset stays as written. A line
/// break an encoded word decodes to becomes a space: a header value is one
/// line. Only the first [`MAX_HEADER_VALUE_BYTES`] of `raw` are read.
pub(crate) fn decode_words(raw: &[u8]) -> String {
    let raw: Vec<u8> = raw
        .iter()
        .take(MAX_HEADER_VALUE_BYTES)
        .copied()
        .filter(|&byte| byte != b'\r' && byte != b'\n')
        .collect();
    let mut out = String::with_capacity(raw.len());
    // The decoded bytes of the current run of adjacent encoded words.
    let mut pending: Option<(&'static Encoding, Vec<u8>)> = None;
    let mut plain_start = 0;
    let mut at = 0;
    while at < raw.len() {
        let word = if raw[at] == b'=' {
            encoded_word(&raw[at..])
        } else {
            None
        };
        let Some((encoding, bytes, len)) = word else {
            at += 1;
            continue;
        };
        let between = &raw[plain_start..at];
        let adjacent =
            pending.is_some() && between.iter().all(|&byte| matches!(byte, b' ' | b'\t'));
        if !adjacent {
            flush_words(&mut out, pending.take());
            push_raw(&mut out, between);
        }
        match &mut pending {
            Some((current, buffer)) if *current == encoding => buffer.extend_from_slice(&bytes),
            _ => {
                flush_words(&mut out, pending.take());
                pending = Some((encoding, bytes));
            }
        }
        at += len;
        plain_start = at;
    }
    flush_words(&mut out, pending);
    push_raw(&mut out, &raw[plain_start..]);
    out.trim().to_owned()
}

/// The longest charset name (with its RFC 2231 language) an encoded word
/// may carry; this also bounds the work each `=?` costs.
const MAX_CHARSET_BYTES: usize = 64;

/// The encoded word `=?charset?B|Q?text?=` at the start of `input`: its
/// charset, its decoded bytes, and its length. `None` when it is malformed or
/// its charset is unknown.
fn encoded_word(input: &[u8]) -> Option<(&'static Encoding, Vec<u8>, usize)> {
    let rest = input.strip_prefix(b"=?")?;
    let label_len = rest
        .iter()
        .take(MAX_CHARSET_BYTES + 1)
        .position(|&byte| byte == b'?')?;
    let label = &rest[..label_len];
    if label.is_empty() || !label.iter().all(|&byte| charset_byte(byte)) {
        return None;
    }
    // RFC 2231 allows `charset*language`.
    let charset = label.split(|&byte| byte == b'*').next().unwrap_or(label);
    let encoding = known_encoding(charset)?;
    let method = rest.get(label_len + 1)?.to_ascii_uppercase();
    if rest.get(label_len + 2) != Some(&b'?') {
        return None;
    }
    let text_start = label_len + 3;
    let text_len = rest
        .get(text_start..)?
        .iter()
        .position(|&byte| byte == b'?')?;
    if rest.get(text_start + text_len + 1) != Some(&b'=') {
        return None;
    }
    let text = &rest[text_start..text_start + text_len];
    let bytes = match method {
        b'B' => word_base64(text)?,
        b'Q' => word_q(text),
        _ => return None,
    };
    Some((encoding, bytes, 2 + text_start + text_len + 2))
}

/// RFC 2047 `token` characters that can appear in a charset name.
fn charset_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-^_`{|}~".contains(&byte)
}

/// The `encoding_rs` encoding a charset label names, if it can decode text
/// (the WHATWG replacement encoding cannot).
fn known_encoding(label: &[u8]) -> Option<&'static Encoding> {
    Encoding::for_label(label).filter(|&encoding| encoding != encoding_rs::REPLACEMENT)
}

/// A `B` encoded word's text, strictly: only the base64 alphabet with
/// optional final padding.
fn word_base64(text: &[u8]) -> Option<Vec<u8>> {
    let end = text
        .iter()
        .rposition(|&byte| byte != b'=')
        .map_or(0, |last| last + 1);
    let body = &text[..end];
    if text.len() - end > 2
        || body.len() % 4 == 1
        || !body.iter().all(|&byte| sextet(byte).is_some())
    {
        return None;
    }
    let mut padded = body.to_vec();
    padded.resize(body.len().div_ceil(4) * 4, b'=');
    Some(base64(&padded))
}

/// A `Q` encoded word's text: `_` is a space and `=XX` a byte; a stray `=`
/// stays as it is.
fn word_q(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut at = 0;
    while at < text.len() {
        match text[at] {
            b'_' => out.push(b' '),
            b'=' => {
                if let Some(byte) = hex_pair(&text[at + 1..]) {
                    out.push(byte);
                    at += 3;
                    continue;
                }
                out.push(b'=');
            }
            byte => out.push(byte),
        }
        at += 1;
    }
    out
}

/// Appends the text a run of adjacent encoded words decodes to.
fn flush_words(out: &mut String, pending: Option<(&'static Encoding, Vec<u8>)>) {
    if let Some((encoding, bytes)) = pending {
        let (text, _) = encoding.decode_without_bom_handling(&bytes);
        out.extend(
            text.chars()
                .map(|c| if matches!(c, '\r' | '\n') { ' ' } else { c }),
        );
    }
}

/// Appends raw header bytes: UTF-8 when they are, else GB18030, which
/// non-compliant Chinese mailers send unencoded.
fn push_raw(out: &mut String, bytes: &[u8]) {
    match std::str::from_utf8(bytes) {
        Ok(text) => out.push_str(text),
        Err(_) => out.push_str(&encoding_rs::GB18030.decode_without_bom_handling(bytes).0),
    }
}

/// A part's content as text: the transfer encoding undone, tolerating input
/// cut anywhere (a partial fetch), then decoded from `charset` (UTF-8 when
/// absent or unknown). A leading byte order mark is dropped, and so is a
/// character the cut left incomplete.
pub(crate) fn decode_body(raw: &[u8], encoding: TransferEncoding, charset: Option<&str>) -> String {
    let bytes = match encoding {
        TransferEncoding::Base64 => Cow::Owned(base64(raw)),
        TransferEncoding::QuotedPrintable => Cow::Owned(quoted_printable(raw)),
        TransferEncoding::SevenBit
        | TransferEncoding::EightBit
        | TransferEncoding::Binary
        | TransferEncoding::Other => Cow::Borrowed(raw),
    };
    let text = decode_prefix(&bytes, body_encoding(charset, &bytes));
    match text.strip_prefix('\u{FEFF}') {
        Some(rest) => rest.to_owned(),
        None => text,
    }
}

/// The encoding a body's charset label names, UTF-8 when it names none
/// `encoding_rs` can decode. A body labelled US-ASCII that holds non-ASCII
/// UTF-8 (possibly cut at the end) is read as UTF-8: its 8-bit bytes
/// already prove the label wrong.
fn body_encoding(charset: Option<&str>, bytes: &[u8]) -> &'static Encoding {
    let Some(label) = charset.map(|label| label.trim().trim_matches('"')) else {
        return encoding_rs::UTF_8;
    };
    let Some(encoding) = known_encoding(label.as_bytes()) else {
        return encoding_rs::UTF_8;
    };
    let ascii = ["us-ascii", "ascii", "ansi_x3.4-1968"]
        .iter()
        .any(|name| label.eq_ignore_ascii_case(name));
    let valid = match std::str::from_utf8(bytes) {
        Ok(text) => Some(text.len()),
        Err(error) => error.error_len().is_none().then(|| error.valid_up_to()),
    };
    let utf8 = valid.is_some_and(|valid| !bytes[..valid].is_ascii());
    if ascii && utf8 {
        encoding_rs::UTF_8
    } else {
        encoding
    }
}

/// `bytes` decoded as a prefix of a longer stream: a byte order mark is
/// honoured, malformed sequences become U+FFFD, and a sequence cut at the
/// end is dropped rather than replaced.
fn decode_prefix(bytes: &[u8], encoding: &'static Encoding) -> String {
    let mut decoder = encoding.new_decoder();
    let mut out = String::with_capacity(
        decoder
            .max_utf8_buffer_length(bytes.len())
            .unwrap_or(bytes.len()),
    );
    let mut rest = bytes;
    loop {
        let (result, read, _) = decoder.decode_to_string(rest, &mut out, false);
        rest = &rest[read..];
        match result {
            CoderResult::InputEmpty => return out,
            CoderResult::OutputFull => out.reserve(
                decoder
                    .max_utf8_buffer_length(rest.len())
                    .unwrap_or(rest.len())
                    .max(16),
            ),
        }
    }
}

/// Base64 as mail carries it: characters outside the alphabet (line breaks
/// included) are skipped, padding ends a quantum early, and an incomplete
/// final quantum is dropped.
fn base64(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    let mut quantum = 0u32;
    let mut count = 0;
    for &byte in input {
        if let Some(value) = sextet(byte) {
            quantum = quantum << 6 | u32::from(value);
            count += 1;
            if count == 4 {
                out.extend_from_slice(&quantum.to_be_bytes()[1..]);
                quantum = 0;
                count = 0;
            }
        } else if byte == b'=' {
            // Two characters hold one byte and three hold two.
            match count {
                2 => out.push((quantum >> 4) as u8),
                3 => out.extend_from_slice(&((quantum >> 2) as u16).to_be_bytes()),
                _ => {}
            }
            quantum = 0;
            count = 0;
        }
    }
    out
}

/// The value of a base64 alphabet character.
fn sextet(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Quoted-printable: soft line breaks removed and `=XX` decoded in either
/// case. An `=` that starts no valid sequence stays as it is, and one the
/// cut left incomplete at the end is dropped.
fn quoted_printable(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut at = 0;
    while at < input.len() {
        if input[at] != b'=' {
            out.push(input[at]);
            at += 1;
            continue;
        }
        let rest = &input[at + 1..];
        if let Some(byte) = hex_pair(rest) {
            out.push(byte);
            at += 3;
            continue;
        }
        // A soft line break, possibly after transport-added blanks.
        let blanks = rest
            .iter()
            .take_while(|&&byte| matches!(byte, b' ' | b'\t'))
            .count();
        let after = &rest[blanks..];
        if after.starts_with(b"\r\n") {
            at += 1 + blanks + 2;
        } else if after.starts_with(b"\n") {
            at += 1 + blanks + 1;
        } else if after.is_empty() || after == b"\r" || (rest.len() == 1 && hex(rest[0]).is_some())
        {
            break;
        } else {
            out.push(b'=');
            at += 1;
        }
    }
    out
}

/// The byte two hex digits at the start of `input` spell, in either case.
fn hex_pair(input: &[u8]) -> Option<u8> {
    match input {
        [high, low, ..] => Some(hex(*high)? << 4 | hex(*low)?),
        _ => None,
    }
}

/// The value of one hex digit, in either case.
fn hex(byte: u8) -> Option<u8> {
    char::from(byte).to_digit(16).map(|digit| digit as u8)
}

/// The longest `msg-id` kept, in bytes; a longer one is treated as absent.
pub(crate) const MAX_MSG_ID_BYTES: usize = 250;

/// The first valid RFC 5322 `msg-id` in a `Message-ID` value, with its angle
/// brackets, and with comments and folding whitespace around it removed; at
/// most [`MAX_MSG_ID_BYTES`]. Case is kept. The obsolete forms (quoted
/// local parts, spaces inside) are not valid here.
pub(crate) fn msg_id(value: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(value).ok()?;
    let text = strip_comments(text);
    let start = text.find('<')?;
    let end = start + text[start..].find('>')?;
    let id = &text[start..=end];
    let inner = &id[1..id.len() - 1];
    let (left, right) = inner.split_once('@')?;
    let atom = |part: &str| {
        !part.is_empty()
            && !part.starts_with('.')
            && !part.ends_with('.')
            && !part.contains("..")
            && part.bytes().all(|byte| atext(byte) || byte == b'.')
    };
    // id-right may also be a no-fold literal: `[...]` without brackets,
    // backslashes, or whitespace inside.
    let literal = |part: &str| {
        part.len() >= 2
            && part.starts_with('[')
            && part.ends_with(']')
            && part[1..part.len() - 1]
                .bytes()
                .all(|byte| (33..=126).contains(&byte) && !matches!(byte, b'[' | b']' | b'\\'))
    };
    (id.len() <= MAX_MSG_ID_BYTES && atom(left) && (atom(right) || literal(right)))
        .then(|| id.to_owned())
}

/// RFC 5322 `atext`: printable ASCII other than specials.
fn atext(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~".contains(&byte)
}

/// `text` without RFC 5322 comments (`(...)`, nested, with `\` escapes) and
/// with its whitespace removed.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    let mut escaped = false;
    for c in text.chars() {
        if depth > 0 {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '(' {
                depth += 1;
            } else if c == ')' {
                depth -= 1;
            }
            continue;
        }
        match c {
            '(' => depth = 1,
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

/// The identity of a stored message, as lowercase hex SHA-256 over these,
/// each prefixed by its length: the mailbox; the valid `msg-id` in its
/// `Message-ID` value, or nothing; the provider's received time exactly as
/// it sent it; its size in decimal; its sender's address; and its subject.
/// None of them change when the provider renumbers the mailbox, so a
/// message listed again is known. A `Message-ID` alone is the sender's to
/// choose, so it never makes two messages one: another delivery differs in
/// its received time or size, and two messages without one that arrive in
/// the same second with the same size are still told apart by sender and
/// subject. One function computes it for every record that names a message
/// by content.
pub(crate) fn identity(
    mailbox: &str,
    message_id: Option<&[u8]>,
    received: &str,
    size: u64,
    from: &str,
    subject: &str,
) -> String {
    use sha2::Digest as _;
    let id = message_id.and_then(msg_id).unwrap_or_default();
    let size = size.to_string();
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"scv-mail-identity-1");
    for field in [mailbox, &id, received, &size, from, subject] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field.as_bytes());
    }
    hex_string(&hasher.finalize())
}

/// `bytes` as lowercase hex SHA-256, so that a record can tell what it saw
/// before without keeping it: a `Message-ID`, for spotting one reused.
pub(crate) fn digest(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex_string(&sha2::Sha256::digest(bytes))
}

fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The fields of a header block, in order, each name lowercased and each
/// value unfolded (a line break followed by whitespace becomes that
/// whitespace) and trimmed, but not decoded. Lines that are not fields are
/// skipped, and the block ends at its first empty line.
pub(crate) fn header_fields(block: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut fields: Vec<(String, Vec<u8>)> = Vec::new();
    for line in block.split(|&byte| byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break;
        }
        if matches!(line[0], b' ' | b'\t') {
            if let Some((_, value)) = fields.last_mut() {
                value.extend_from_slice(line);
            }
            continue;
        }
        let Some(colon) = line.iter().position(|&byte| byte == b':') else {
            continue;
        };
        let name = &line[..colon];
        if name.is_empty() || !name.iter().all(|&byte| (33..=126).contains(&byte)) {
            continue;
        }
        fields.push((
            String::from_utf8_lossy(name).to_ascii_lowercase(),
            line[colon + 1..].to_vec(),
        ));
    }
    for (_, value) in &mut fields {
        let start = value
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .unwrap_or(value.len());
        let end = value
            .iter()
            .rposition(|byte| !byte.is_ascii_whitespace())
            .map_or(start, |end| end + 1);
        *value = value[start..end.max(start)].to_vec();
    }
    fields
}

#[cfg(test)]
mod tests;
