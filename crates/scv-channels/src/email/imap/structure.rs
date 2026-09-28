//! Reading a BODYSTRUCTURE: which part holds the text to read, and which
//! parts are attachments to list by name, type, and size.
//!
//! Only the structure is read here; no part's content is fetched to
//! decide. An attached message (`message/rfc822`) is listed as one
//! attachment and never entered, so its text is never taken for the
//! message's own.

use super::wire::Value;
use crate::email::parse::{self, TransferEncoding};
use crate::email::source::{AttachmentInfo, MAX_ATTACHMENTS, PartRef};

/// What a message's structure offers the pipeline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Layout {
    /// The first `text/plain` part that is not an attachment, else the
    /// first such `text/html`.
    pub(crate) text: Option<PartRef>,
    /// At most [`MAX_ATTACHMENTS`], in structure order.
    pub(crate) attachments: Vec<AttachmentInfo>,
}

/// The layout of a BODYSTRUCTURE (or BODY) value.
pub(crate) fn layout(structure: &Value) -> Layout {
    let mut found = Found::default();
    visit(structure, "", &mut found);
    Layout {
        text: found.plain.or(found.html),
        attachments: found.attachments,
    }
}

#[derive(Default)]
struct Found {
    plain: Option<PartRef>,
    html: Option<PartRef>,
    attachments: Vec<AttachmentInfo>,
}

/// Visits a body depth-first. `section` is its part number, empty for the
/// message itself: a multipart's children are numbered from 1 below it,
/// and a message that is not multipart has the single part `1`. The
/// parser's depth bound also bounds this recursion.
fn visit(body: &Value, section: &str, found: &mut Found) {
    let Some(items) = body.list().filter(|items| !items.is_empty()) else {
        return;
    };
    if matches!(items.first(), Some(Value::List(_))) {
        let children = items.iter().take_while(|item| item.list().is_some());
        for (index, child) in children.enumerate() {
            let number = index + 1;
            let child_section = if section.is_empty() {
                number.to_string()
            } else {
                format!("{section}.{number}")
            };
            visit(child, &child_section, found);
        }
        return;
    }
    let section = if section.is_empty() { "1" } else { section };
    let part = Part::read(items);
    if part.is_attachment() {
        if found.attachments.len() < MAX_ATTACHMENTS {
            found.attachments.push(AttachmentInfo {
                name: part.file_name().unwrap_or_default(),
                mime: part.mime(),
                size: part.size,
            });
        }
        return;
    }
    let slot = match (part.kind.as_str(), part.subtype.as_str()) {
        ("text", "plain") => &mut found.plain,
        ("text", "html") => &mut found.html,
        _ => return,
    };
    if slot.is_none() {
        *slot = Some(PartRef {
            id: section.to_owned(),
            mime: part.mime(),
            charset: part
                .param("charset")
                .map(|charset| String::from_utf8_lossy(charset).trim().to_ascii_lowercase()),
            encoding: part.encoding,
            size: part.size,
        });
    }
}

/// Parameters as `(name, value)`, names lowercased.
type Params<'a> = Vec<(String, &'a [u8])>;

/// A disposition, lowercased, and its parameters.
type Disposition<'a> = (String, Params<'a>);

/// A leaf part's fields, from `(type subtype params id description
/// encoding size ...)` and its extension data.
struct Part<'a> {
    /// Lowercased.
    kind: String,
    /// Lowercased.
    subtype: String,
    params: Params<'a>,
    encoding: TransferEncoding,
    size: u64,
    disposition: Option<Disposition<'a>>,
}

impl<'a> Part<'a> {
    fn read(items: &'a [Value]) -> Self {
        let text = |index: usize| {
            items
                .get(index)
                .and_then(Value::bytes)
                .map(|bytes| String::from_utf8_lossy(bytes).trim().to_ascii_lowercase())
        };
        let kind = text(0).unwrap_or_else(|| "text".to_owned());
        let subtype = text(1).unwrap_or_else(|| "plain".to_owned());
        let encoding = TransferEncoding::parse(text(5).as_deref().unwrap_or("7bit"));
        let size = items.get(6).and_then(Value::number).unwrap_or(0);
        // Extension data starts after the fields each kind carries: a text
        // part adds its line count, an attached message its envelope, body,
        // and line count. The first extension is the MD5, then the
        // disposition.
        let fixed = if kind == "text" {
            8
        } else if is_message(&kind, &subtype) && items.get(8).is_some_and(|b| b.list().is_some()) {
            10
        } else {
            7
        };
        let disposition = items
            .get(fixed + 1)
            .and_then(disposition)
            .or_else(|| items.iter().skip(7).find_map(disposition));
        Self {
            kind,
            subtype,
            params: items.get(2).map(params).unwrap_or_default(),
            encoding,
            size,
            disposition,
        }
    }

    fn mime(&self) -> String {
        format!("{}/{}", self.kind, self.subtype)
    }

    fn param(&self, name: &str) -> Option<&'a [u8]> {
        lookup(&self.params, name)
    }

    /// An attachment: disposed as one, named, not text, or an attached
    /// message.
    fn is_attachment(&self) -> bool {
        let disposition_params: &[(String, &[u8])] = self
            .disposition
            .as_ref()
            .map_or(&[], |(_, params)| params.as_slice());
        self.disposition
            .as_ref()
            .is_some_and(|(kind, _)| kind == "attachment")
            || has_param(disposition_params, "filename")
            || has_param(&self.params, "name")
            || self.kind != "text"
            || is_message(&self.kind, &self.subtype)
    }

    /// The disposition's `filename`, else the type's `name`, decoded.
    fn file_name(&self) -> Option<String> {
        self.disposition
            .as_ref()
            .and_then(|(_, params)| param_text(params, "filename"))
            .or_else(|| param_text(&self.params, "name"))
    }
}

fn is_message(kind: &str, subtype: &str) -> bool {
    kind == "message" && matches!(subtype, "rfc822" | "global")
}

/// `("NAME" "value" ...)` as pairs, names lowercased; NIL is none.
fn params(value: &Value) -> Params<'_> {
    value
        .list()
        .unwrap_or_default()
        .as_chunks::<2>()
        .0
        .iter()
        .filter_map(|[name, value]| {
            let name = name.bytes()?;
            let value = value.bytes().unwrap_or_default();
            Some((String::from_utf8_lossy(name).to_ascii_lowercase(), value))
        })
        .collect()
}

/// `("attachment" (params))`, the disposition's shape; anything else is
/// not one.
fn disposition(value: &Value) -> Option<Disposition<'_>> {
    let items = value.list()?;
    let kind = items.first()?.bytes()?;
    if items.len() != 2 || !matches!(items[1], Value::List(_) | Value::Nil) {
        return None;
    }
    Some((
        String::from_utf8_lossy(kind).trim().to_ascii_lowercase(),
        params(&items[1]),
    ))
}

fn lookup<'a>(params: &[(String, &'a [u8])], name: &str) -> Option<&'a [u8]> {
    params
        .iter()
        .find(|(known, _)| known == name)
        .map(|(_, value)| *value)
}

/// Whether a parameter is present in any RFC 2231 spelling: `name`,
/// `name*`, or continued as `name*0` / `name*0*`.
fn has_param(params: &[(String, &[u8])], name: &str) -> bool {
    params.iter().any(|(known, _)| {
        known == name
            || known
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with('*'))
    })
}

/// A parameter's value as text: RFC 2231's extended (`name*`) or continued
/// (`name*0*`, `name*1`, ...) forms first, else the plain value with RFC
/// 2047 encoded words decoded.
fn param_text(params: &[(String, &[u8])], name: &str) -> Option<String> {
    if let Some(value) = lookup(params, &format!("{name}*")) {
        return Some(extended(value));
    }
    if let Some(value) = continued(params, name) {
        return Some(value);
    }
    lookup(params, name).map(parse::decode_words)
}

/// An RFC 2231 extended value, `charset'language'percent-encoded`.
fn extended(value: &[u8]) -> String {
    let data = strip_charset(value);
    text_of(&percent_decode(data))
}

/// The value after `charset'language'`, or all of it without that prefix.
fn strip_charset(value: &[u8]) -> &[u8] {
    let mut quotes = value
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\'')
        .map(|(index, _)| index);
    match (quotes.next(), quotes.next()) {
        (Some(_), Some(second)) => &value[second + 1..],
        _ => value,
    }
}

/// Continued RFC 2231 segments joined in order, stopping at the first gap:
/// `name*N*` segments percent-decoded, `name*N` ones taken as they are.
fn continued(params: &[(String, &[u8])], name: &str) -> Option<String> {
    let mut segments: Vec<(u32, bool, &[u8])> = params
        .iter()
        .filter_map(|(known, value)| {
            let rest = known.strip_prefix(name)?.strip_prefix('*')?;
            let (index, encoded) = match rest.strip_suffix('*') {
                Some(index) => (index, true),
                None => (rest, false),
            };
            let index = index.parse().ok()?;
            Some((index, encoded, *value))
        })
        .collect();
    if segments.is_empty() {
        return None;
    }
    segments.sort_by_key(|(index, _, _)| *index);
    let any_encoded = segments.iter().any(|(_, encoded, _)| *encoded);
    let mut bytes = Vec::new();
    for (expected, (index, encoded, value)) in segments.into_iter().enumerate() {
        if index as usize != expected {
            break;
        }
        if encoded {
            let value = if index == 0 {
                strip_charset(value)
            } else {
                value
            };
            bytes.extend(percent_decode(value));
        } else {
            bytes.extend_from_slice(value);
        }
    }
    Some(if any_encoded {
        text_of(&bytes)
    } else {
        parse::decode_words(&bytes)
    })
}

/// Decoded bytes as text: UTF-8 when they are, else whatever
/// [`parse::decode_words`] makes of raw 8-bit text.
fn text_of(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => parse::decode_words(bytes),
    }
}

/// `%XX` escapes undone; a `%` not followed by two hex digits stays.
fn percent_decode(value: &[u8]) -> Vec<u8> {
    let hex = |byte: u8| char::from(byte).to_digit(16);
    let mut out = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'%'
            && let (Some(high), Some(low)) = (
                value.get(index + 1).and_then(|&byte| hex(byte)),
                value.get(index + 2).and_then(|&byte| hex(byte)),
            )
        {
            out.push((high * 16 + low) as u8);
            index += 3;
        } else {
            out.push(value[index]);
            index += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests;
