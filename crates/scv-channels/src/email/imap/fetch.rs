//! Interpreting FETCH data: the items one message returned, its ENVELOPE,
//! and the dates IMAP writes (`INTERNALDATE`, and `SEARCH SINCE`'s).

use super::wire::Value;
use crate::email::parse;
use crate::email::source::Address;

/// What one message returned across the FETCH responses of a command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Fetched {
    pub(crate) uid: Option<u32>,
    pub(crate) flags: Option<Vec<String>>,
    /// `INTERNALDATE` exactly as sent, padding kept.
    pub(crate) internal_date: Option<String>,
    /// `RFC822.SIZE`.
    pub(crate) size: Option<u64>,
    pub(crate) envelope: Option<Value>,
    /// `BODYSTRUCTURE`, or the non-extensible `BODY` form.
    pub(crate) structure: Option<Value>,
    /// Each `BODY[...]` item: its normalized section spec (see
    /// [`normalize_spec`]) and its data, `None` when the server sent NIL.
    pub(crate) sections: Vec<(String, Option<Vec<u8>>)>,
}

impl Fetched {
    /// Adds the items of one FETCH response (its parenthesized list).
    /// Unknown items are skipped; a later value replaces an earlier one.
    pub(crate) fn merge(&mut self, items: &[Value]) {
        for [key, value] in items.as_chunks::<2>().0 {
            match key {
                Value::Atom(name) => match name.to_ascii_uppercase().as_str() {
                    "UID" => {
                        if let Some(uid) = value.number().and_then(|uid| u32::try_from(uid).ok()) {
                            self.uid = Some(uid);
                        }
                    }
                    "FLAGS" => {
                        self.flags = value.list().map(|flags| {
                            flags
                                .iter()
                                .filter_map(|flag| match flag {
                                    Value::Atom(flag) => Some(flag.clone()),
                                    _ => None,
                                })
                                .collect()
                        });
                    }
                    "INTERNALDATE" => {
                        self.internal_date = value
                            .bytes()
                            .map(|date| String::from_utf8_lossy(date).into_owned());
                    }
                    "RFC822.SIZE" => self.size = value.number(),
                    "ENVELOPE" => self.envelope = Some(value.clone()),
                    "BODYSTRUCTURE" => self.structure = Some(value.clone()),
                    "BODY" if self.structure.is_none() => self.structure = Some(value.clone()),
                    _ => {}
                },
                // Some servers echo the `BODY.PEEK` the client asked for.
                Value::Section { name, spec, .. }
                    if name.eq_ignore_ascii_case("BODY")
                        || name.eq_ignore_ascii_case("BODY.PEEK") =>
                {
                    let spec = normalize_spec(spec);
                    let data = value.bytes().map(<[u8]>::to_vec);
                    self.sections.retain(|(known, _)| *known != spec);
                    self.sections.push((spec, data));
                }
                _ => {}
            }
        }
    }

    /// The data of the section whose normalized spec is `spec`.
    pub(crate) fn section(&self, spec: &str) -> Option<&[u8]> {
        let spec = normalize_spec(spec);
        self.sections
            .iter()
            .find(|(known, _)| *known == spec)
            .and_then(|(_, data)| data.as_deref())
    }

    /// The data of the first `HEADER.FIELDS (...)` section, whatever
    /// fields the server echoed back in its key.
    pub(crate) fn header_fields(&self) -> Option<&[u8]> {
        self.sections
            .iter()
            .find(|(spec, _)| {
                spec.starts_with("HEADER.FIELDS") && !spec.starts_with("HEADER.FIELDS.NOT")
            })
            .and_then(|(_, data)| data.as_deref())
    }
}

/// A section spec in one spelling: uppercased, quotes removed, and runs of
/// whitespace made one space, with none just inside parentheses.
pub(crate) fn normalize_spec(spec: &str) -> String {
    let words: Vec<String> = spec
        .replace('"', "")
        .replace('(', " ( ")
        .replace(')', " ) ")
        .split_whitespace()
        .map(str::to_ascii_uppercase)
        .collect();
    words.join(" ").replace("( ", "(").replace(" )", ")")
}

/// The ENVELOPE fields the pipeline reads, decoded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Envelope {
    /// The `Date` header, undecoded.
    pub(crate) date: Option<String>,
    pub(crate) subject: String,
    pub(crate) from: Vec<Address>,
    pub(crate) reply_to: Vec<Address>,
    pub(crate) to: Vec<Address>,
    pub(crate) cc: Vec<Address>,
    /// The `Message-ID` header's raw value.
    pub(crate) message_id: Option<Vec<u8>>,
}

impl Envelope {
    /// Reads `(date subject from sender reply-to to cc bcc in-reply-to
    /// message-id)`; a field that is missing or of the wrong shape is
    /// empty.
    pub(crate) fn read(value: &Value) -> Self {
        let Some(fields) = value.list() else {
            return Self::default();
        };
        let field = |index: usize| fields.get(index);
        let addresses = |index: usize| field(index).map(addresses).unwrap_or_default();
        Self {
            date: field(0)
                .and_then(Value::bytes)
                .map(|date| String::from_utf8_lossy(date).into_owned()),
            subject: field(1)
                .and_then(Value::bytes)
                .map(parse::decode_words)
                .unwrap_or_default(),
            from: addresses(2),
            reply_to: addresses(4),
            to: addresses(5),
            cc: addresses(6),
            message_id: field(9).and_then(Value::bytes).map(<[u8]>::to_vec),
        }
    }
}

/// An address list: each `(name adl mailbox host)`, skipping the group
/// markers RFC 3501 writes with a NIL host.
fn addresses(value: &Value) -> Vec<Address> {
    let Some(entries) = value.list() else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let parts = entry.list()?;
            let host = parts.get(3)?.bytes()?;
            let name = parts
                .first()
                .and_then(Value::bytes)
                .map(parse::decode_words)
                .unwrap_or_default();
            let mailbox = parts.get(2).and_then(Value::bytes).unwrap_or_default();
            let address = if mailbox.is_empty() {
                String::new()
            } else {
                format!(
                    "{}@{}",
                    String::from_utf8_lossy(mailbox),
                    String::from_utf8_lossy(host)
                )
            };
            Some(Address { name, address })
        })
        .collect()
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// An `INTERNALDATE` (`dd-Mon-yyyy hh:mm:ss +zzzz`, the day possibly
/// space-padded) in Unix seconds; `None` when it does not parse or falls
/// before 1970.
pub(crate) fn internal_date_seconds(text: &str) -> Option<u64> {
    let (date, rest) = text.trim().split_once(' ')?;
    let (time, zone) = rest.trim().split_once(' ')?;
    let mut date = date.split('-');
    let day: u32 = number(date.next()?, 1, 2)?;
    let month = date.next()?;
    let month = MONTHS
        .iter()
        .position(|name| name.eq_ignore_ascii_case(month))?;
    let year: i64 = number(date.next()?, 4, 4)?;
    let mut time = time.split(':');
    let hour: i64 = number(time.next()?, 2, 2)?;
    let minute: i64 = number(time.next()?, 2, 2)?;
    let second: i64 = number(time.next()?, 2, 2)?;
    let zone = zone.trim();
    let sign = match zone.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let zone: i64 = number(&zone[1..], 4, 4)?;
    if date.next().is_some()
        || time.next().is_some()
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
        || zone % 100 > 59
    {
        return None;
    }
    let offset = sign * ((zone / 100) * 3600 + (zone % 100) * 60);
    let days = days_from_civil(year, month as u32 + 1, day);
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(seconds).ok()
}

/// A decimal field of `min..=max` digits.
fn number<T: std::str::FromStr>(text: &str, min: usize, max: usize) -> Option<T> {
    (text.len() >= min && text.len() <= max && text.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

/// A `SEARCH` date (`d-Mon-yyyy`, RFC 3501's `date`) for the UTC day that
/// holds `unix_seconds`.
pub(crate) fn search_date(unix_seconds: u64) -> String {
    let days = i64::try_from(unix_seconds / 86_400).unwrap_or(i64::MAX / 2);
    let (year, month, day) = civil_from_days(days);
    format!("{day}-{}-{year}", MONTHS[(month - 1) as usize])
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = i64::from((month + 9) % 12);
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date `days` after 1970-01-01, as (year, month 1..=12, day 1..=31).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests;
