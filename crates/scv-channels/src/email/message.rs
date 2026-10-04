//! Building an outgoing message's bytes from its bound content.
//!
//! Everything here is deterministic: the bytes follow from the action's
//! content (sender, recipients, subject, body, threading headers, and
//! `Message-ID`) and the `Date` the executor sets when it runs. The body is
//! one `text/plain; charset=utf-8` part in quoted-printable; there are no
//! attachments and no HTML. Header text that is not plain ASCII is encoded
//! as RFC 2047 words, and every header line is folded to stay under 78
//! columns where it can and never passes 998 bytes.

use super::content::Outgoing;

/// Where header lines are folded, and the hard limit of one line.
const FOLD_AT: usize = 78;
/// The longest encoded word, `=?UTF-8?B?...?=`.
const MAX_WORD: usize = 75;
/// Quoted-printable lines stay within this many characters.
const QP_LINE: usize = 76;

/// The message `message` as sent at `unix` seconds in the time zone
/// `offset` seconds east of UTC, with CRLF line ends.
pub(crate) fn build(message: &Outgoing, unix: u64, offset: i32) -> Vec<u8> {
    let mut headers = Vec::new();
    headers.push(("Date", date(unix, offset)));
    headers.push(("From", mailbox(&message.from.name, &message.from.address)));
    headers.push(("To", message.to.join(", ")));
    if !message.cc.is_empty() {
        headers.push(("Cc", message.cc.join(", ")));
    }
    headers.push(("Subject", unstructured(&message.subject)));
    headers.push(("Message-ID", message.message_id.clone()));
    if let Some(parent) = &message.in_reply_to {
        headers.push(("In-Reply-To", parent.clone()));
    }
    if !message.references.is_empty() {
        headers.push(("References", message.references.join(" ")));
    }
    headers.push(("MIME-Version", "1.0".to_owned()));
    headers.push(("Content-Type", "text/plain; charset=utf-8".to_owned()));
    headers.push(("Content-Transfer-Encoding", "quoted-printable".to_owned()));
    let mut out = String::new();
    for (name, value) in headers {
        out.push_str(&fold(&format!("{name}: {value}")));
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    out.push_str(&quoted_printable(&message.body));
    out.into_bytes()
}

/// `name <address>`, the name quoted or encoded as it needs; the address
/// alone without a name.
fn mailbox(name: &str, address: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return address.to_owned();
    }
    let plain = name
        .bytes()
        .all(|byte| (0x20..0x7f).contains(&byte) && byte != b'"' && byte != b'\\');
    if plain {
        format!("\"{name}\" <{address}>")
    } else {
        format!("{} <{address}>", encoded_words(name))
    }
}

/// Unstructured header text: as it is when plain ASCII, else encoded words.
fn unstructured(text: &str) -> String {
    let plain = text.bytes().all(|byte| (0x20..0x7f).contains(&byte)) && !text.contains("=?");
    if plain {
        text.to_owned()
    } else {
        encoded_words(text)
    }
}

/// `text` as RFC 2047 `B` words in UTF-8, each at most [`MAX_WORD`]
/// characters, split between characters, joined by spaces for folding.
fn encoded_words(text: &str) -> String {
    use base64::Engine as _;
    const FRAME: usize = "=?UTF-8?B??=".len();
    // Base64 turns 3 bytes into 4 characters.
    let room = (MAX_WORD - FRAME) / 4 * 3;
    let mut words = Vec::new();
    let mut chunk = String::new();
    for c in text.chars() {
        if chunk.len() + c.len_utf8() > room {
            words.push(std::mem::take(&mut chunk));
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(chunk);
    }
    words
        .iter()
        .map(|word| {
            format!(
                "=?UTF-8?B?{}?=",
                base64::engine::general_purpose::STANDARD.encode(word.as_bytes())
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A header line folded at spaces so each piece stays within [`FOLD_AT`]
/// where it can; a piece with no space to break at stays whole.
fn fold(line: &str) -> String {
    if line.len() <= FOLD_AT {
        return line.to_owned();
    }
    let mut out = String::new();
    let mut current = String::new();
    for (index, word) in line.split(' ').enumerate() {
        if index == 0 {
            current.push_str(word);
            continue;
        }
        if current.len() + 1 + word.len() > FOLD_AT && !current.trim().is_empty() {
            out.push_str(&current);
            out.push_str("\r\n");
            current = format!(" {word}");
        } else {
            current.push(' ');
            current.push_str(word);
        }
    }
    out.push_str(&current);
    out
}

/// `body` in quoted-printable with CRLF line ends: printable ASCII but `=`
/// as it is, everything else as `=XX`, a space or tab before a line end
/// encoded, and soft breaks keeping every line within [`QP_LINE`].
pub(crate) fn quoted_printable(body: &str) -> String {
    let mut out = String::with_capacity(body.len() * 2);
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    for (index, line) in lines.iter().enumerate() {
        let bytes = line.as_bytes();
        let mut current = String::new();
        for (at, &byte) in bytes.iter().enumerate() {
            let last = at + 1 == bytes.len();
            let piece = match byte {
                b'=' => "=3D".to_owned(),
                b' ' | b'\t' if last => format!("={byte:02X}"),
                0x21..=0x7e | b' ' | b'\t' => char::from(byte).to_string(),
                _ => format!("={byte:02X}"),
            };
            // Leave room for the soft break's `=`.
            if current.len() + piece.len() > QP_LINE - 1 {
                out.push_str(&current);
                out.push_str("=\r\n");
                current.clear();
            }
            current.push_str(&piece);
        }
        out.push_str(&current);
        if index + 1 < lines.len() {
            out.push_str("\r\n");
        }
    }
    if !out.ends_with("\r\n") {
        out.push_str("\r\n");
    }
    out
}

/// An RFC 5322 date: `Fri, 04 Oct 2024 17:00:00 +0800`.
pub(crate) fn date(unix: u64, offset: i32) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let local = i64::try_from(unix).unwrap_or(i64::MAX) + i64::from(offset);
    let days = local.div_euclid(86_400);
    let seconds = local.rem_euclid(86_400);
    let (year, month, day) = civil(days);
    let sign = if offset < 0 { '-' } else { '+' };
    let minutes = offset.unsigned_abs() / 60;
    format!(
        "{}, {day:02} {} {year} {:02}:{:02}:{:02} {sign}{:02}{:02}",
        DAYS[usize::try_from(days.rem_euclid(7)).unwrap_or(0)],
        MONTHS[usize::try_from(month - 1).unwrap_or(0)],
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
        minutes / 60,
        minutes % 60,
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01.
pub(crate) fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests;
