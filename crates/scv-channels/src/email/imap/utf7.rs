//! Modified UTF-7 mailbox names (RFC 3501 §5.1.3).
//!
//! Without `UTF8=ACCEPT` enabled, IMAP names mailboxes in this encoding:
//! printable ASCII stands for itself except `&`, written `&-`, and every
//! other run of characters is UTF-16 in a variant of base64 (`,` for `/`,
//! no padding) between `&` and `-`. The output is printable ASCII, so a
//! mailbox name can never carry a line break onto the wire.

use anyhow::{Result, bail};
use base64::Engine as _;
use base64::alphabet::Alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

const ALPHABET: Alphabet =
    match Alphabet::new("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,") {
        Ok(alphabet) => alphabet,
        Err(_) => panic!("the modified base64 alphabet is valid"),
    };

const ENGINE: GeneralPurpose = GeneralPurpose::new(
    &ALPHABET,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::RequireNone),
);

/// A mailbox name as the wire writes it.
pub(crate) fn encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending: Vec<u16> = Vec::new();
    for c in name.chars() {
        if (' '..='~').contains(&c) {
            flush(&mut pending, &mut out);
            if c == '&' {
                out.push_str("&-");
            } else {
                out.push(c);
            }
        } else {
            let mut units = [0u16; 2];
            pending.extend_from_slice(c.encode_utf16(&mut units));
        }
    }
    flush(&mut pending, &mut out);
    out
}

fn flush(pending: &mut Vec<u16>, out: &mut String) {
    if pending.is_empty() {
        return;
    }
    let bytes: Vec<u8> = pending.iter().flat_map(|unit| unit.to_be_bytes()).collect();
    out.push('&');
    out.push_str(&ENGINE.encode(bytes));
    out.push('-');
    pending.clear();
}

/// A mailbox name from the wire, as Unicode.
pub(crate) fn decode(name: &str) -> Result<String> {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('-') else {
            bail!("a mailbox name has an unterminated modified UTF-7 run");
        };
        let run = &after[..end];
        if run.is_empty() {
            out.push('&');
        } else {
            let bytes = ENGINE.decode(run)?;
            if bytes.len() % 2 != 0 {
                bail!("a mailbox name has a malformed modified UTF-7 run");
            }
            let units = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .copied()
                .map(u16::from_be_bytes);
            for c in char::decode_utf16(units) {
                out.push(c?);
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests;
