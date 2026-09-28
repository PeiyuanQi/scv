//! The command guard: defence in depth under the typed client.
//!
//! The client offers no method that changes a mailbox, so the guard should
//! never fire. It exists so that a bug in how a command is built cannot put
//! a mailbox-changing command on the wire: every command is checked whole,
//! literals included as markers, before any byte of it is written, against
//! an allowlist of verbs and of the arguments each may take. Anything it
//! does not recognize is refused, so a new server extension or a malformed
//! argument fails closed.

use std::fmt;

/// What a connection may do. This release has only a read-only mode; a
/// write mode would be a new variant with its own allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    ReadOnly,
}

/// One piece of a logical command, in wire order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Part {
    /// Command text, written as is: printable ASCII only.
    Text(String),
    /// A synchronizing literal: `{n}` ends the line so far, and the bytes
    /// follow once the server answers `+`. Opaque to the guard.
    Literal(Vec<u8>),
    /// A line sent after the server's `+` without a literal announcing it:
    /// the SASL response of `AUTHENTICATE` without an initial response.
    Line(String),
}

/// A command the guard refused. `verb` is only ever the command's verb
/// (such as `STORE` or `UID FETCH`), never an argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuardViolation {
    pub(crate) verb: String,
}

impl fmt::Display for GuardViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "refused to send an IMAP {} command: this connection is read-only",
            self.verb
        )
    }
}

impl std::error::Error for GuardViolation {}

/// The verb label for a command the guard cannot read.
const MALFORMED: &str = "(malformed)";

/// `FETCH` items that read without changing anything. `BODY[...]`,
/// `RFC822`, and `RFC822.TEXT` set `\Seen`, so only `BODY.PEEK[...]` reads
/// content.
const FETCH_ITEMS: [&str; 7] = [
    "UID",
    "FLAGS",
    "INTERNALDATE",
    "RFC822.SIZE",
    "ENVELOPE",
    "BODYSTRUCTURE",
    "RFC822.HEADER",
];

/// Checks a whole logical command, tag first, and returns its verb.
pub(crate) fn check(mode: Mode, parts: &[Part]) -> Result<String, GuardViolation> {
    let Mode::ReadOnly = mode;
    let verb = verb_label(parts);
    let refuse = || GuardViolation { verb: verb.clone() };
    let tokens = tokenize(parts).map_err(|()| refuse())?;
    let [Token::Atom(tag), Token::Atom(word), rest @ ..] = tokens.as_slice() else {
        return Err(refuse());
    };
    if tag.is_empty() || tag.len() > 32 || !tag.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(refuse());
    }
    // The verb as the tokens read it must be the one the label names.
    let (command, args) = match (word.to_ascii_uppercase().as_str(), rest) {
        ("UID", [Token::Atom(sub), args @ ..]) => {
            (format!("UID {}", sub.to_ascii_uppercase()), args)
        }
        (word, args) => (word.to_owned(), args),
    };
    if command != verb {
        return Err(refuse());
    }
    let allowed = match verb.as_str() {
        "CAPABILITY" | "NOOP" | "LOGOUT" => args.is_empty(),
        "ID" => id_args(args),
        "LOGIN" => matches!(args, [user, password] if astring(user) && astring(password)),
        "AUTHENTICATE" => authenticate_args(args),
        "EXAMINE" => matches!(args, [mailbox] if astring(mailbox)),
        "LIST" => {
            matches!(args, [reference, pattern] if astring(reference) && list_pattern(pattern))
        }
        "STATUS" => status_args(args),
        "UID SEARCH" => search_args(args),
        "UID FETCH" => fetch_args(args),
        _ => false,
    };
    if allowed { Ok(verb) } else { Err(refuse()) }
}

/// The verb of a command, read from its first words for the violation's
/// label (`UID` with the word after it): letters, digits, `.`, and `-`,
/// at most 32 of them, uppercased.
fn verb_label(parts: &[Part]) -> String {
    let Some(Part::Text(text)) = parts.first() else {
        return MALFORMED.to_owned();
    };
    let label = |word: &str| -> String {
        word.chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
            .take(32)
            .collect::<String>()
            .to_ascii_uppercase()
    };
    let mut words = text
        .split(|c: char| c == ' ' || c == '(' || c == '"' || c.is_ascii_control())
        .filter(|word| !word.is_empty())
        .skip(1);
    let verb = words.next().map(label).unwrap_or_default();
    let verb = if verb == "UID" {
        match words.next().map(label) {
            Some(sub) if !sub.is_empty() => format!("UID {sub}"),
            _ => verb,
        }
    } else {
        verb
    };
    if verb.is_empty() {
        MALFORMED.to_owned()
    } else {
        verb
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// An atom, including any `[...]` section and `<...>` partial in it.
    Atom(String),
    Quoted(String),
    Literal,
    Open,
    Close,
    Line(String),
}

/// Splits a command into tokens the way a server reads it. Any control or
/// 8-bit byte in command text is refused: line breaks would end the
/// command early and smuggle in another.
fn tokenize(parts: &[Part]) -> Result<Vec<Token>, ()> {
    let mut tokens = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        match part {
            Part::Text(text) => {
                if !text.bytes().all(|byte| (0x20..0x7f).contains(&byte)) {
                    return Err(());
                }
                tokenize_text(text.as_bytes(), &mut tokens)?;
            }
            Part::Literal(_) => tokens.push(Token::Literal),
            Part::Line(line) => {
                if index + 1 != parts.len()
                    || !line.bytes().all(|byte| (0x20..0x7f).contains(&byte))
                {
                    return Err(());
                }
                tokens.push(Token::Line(line.clone()));
            }
        }
    }
    Ok(tokens)
}

fn tokenize_text(text: &[u8], tokens: &mut Vec<Token>) -> Result<(), ()> {
    let mut at = 0;
    while at < text.len() {
        match text[at] {
            b' ' => at += 1,
            b'(' => {
                tokens.push(Token::Open);
                at += 1;
            }
            b')' => {
                tokens.push(Token::Close);
                at += 1;
            }
            b'"' => {
                let mut value = Vec::new();
                at += 1;
                loop {
                    match text.get(at) {
                        None => return Err(()),
                        Some(b'"') => break,
                        Some(b'\\') => match text.get(at + 1) {
                            Some(&byte @ (b'"' | b'\\')) => {
                                value.push(byte);
                                at += 2;
                            }
                            _ => return Err(()),
                        },
                        Some(&byte) => {
                            value.push(byte);
                            at += 1;
                        }
                    }
                }
                at += 1;
                tokens.push(Token::Quoted(String::from_utf8_lossy(&value).into_owned()));
            }
            b'{' | b'[' | b']' | b'\\' | b'<' => return Err(()),
            _ => {
                let start = at;
                while at < text.len() && !matches!(text[at], b' ' | b'(' | b')' | b'"') {
                    if text[at] == b'[' {
                        // A section runs to its `]`, spaces and
                        // parentheses included.
                        let close = text[at..].iter().position(|&byte| byte == b']');
                        at += close.ok_or(())?;
                    }
                    if text[at] == b'{' {
                        return Err(());
                    }
                    at += 1;
                }
                tokens.push(Token::Atom(
                    String::from_utf8_lossy(&text[start..at]).into_owned(),
                ));
            }
        }
    }
    Ok(())
}

/// An `astring`: an atom, a quoted string, or a literal.
fn astring(token: &Token) -> bool {
    match token {
        Token::Atom(atom) => atom
            .bytes()
            .all(|byte| !matches!(byte, b'[' | b'%' | b'*' | b'{')),
        Token::Quoted(_) | Token::Literal => true,
        _ => false,
    }
}

/// A `LIST` pattern: an astring, or an atom with the `%` and `*`
/// wildcards.
fn list_pattern(token: &Token) -> bool {
    match token {
        Token::Atom(atom) => atom.bytes().all(|byte| !matches!(byte, b'[' | b'{')),
        other => astring(other),
    }
}

/// `ID NIL` or `ID ("key" "value" ...)` with strings or NIL.
fn id_args(args: &[Token]) -> bool {
    match args {
        [Token::Atom(nil)] => nil.eq_ignore_ascii_case("NIL"),
        [Token::Open, fields @ .., Token::Close] => fields.iter().all(|field| match field {
            Token::Quoted(_) => true,
            Token::Atom(nil) => nil.eq_ignore_ascii_case("NIL"),
            _ => false,
        }),
        _ => false,
    }
}

/// `AUTHENTICATE PLAIN`, with either a base64 initial response (SASL-IR)
/// or a single base64 line after the server's `+`.
fn authenticate_args(args: &[Token]) -> bool {
    let base64 = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
    };
    match args {
        [Token::Atom(mechanism), rest @ ..] if mechanism.eq_ignore_ascii_case("PLAIN") => {
            match rest {
                [] => true,
                [Token::Atom(initial) | Token::Line(initial)] => base64(initial),
                _ => false,
            }
        }
        _ => false,
    }
}

/// `STATUS mailbox (ITEM ...)`.
fn status_args(args: &[Token]) -> bool {
    match args {
        [mailbox, Token::Open, items @ .., Token::Close] if astring(mailbox) => {
            !items.is_empty()
                && items.iter().all(|item| {
                    matches!(item, Token::Atom(item)
                        if item.bytes().all(|byte| byte.is_ascii_alphabetic() || byte == b'-'))
                })
        }
        _ => false,
    }
}

/// The search keys the client uses: `ALL`, `UID <set>`, and the date keys.
fn search_args(args: &[Token]) -> bool {
    if args.is_empty() {
        return false;
    }
    let mut index = 0;
    while index < args.len() {
        let Token::Atom(key) = &args[index] else {
            return false;
        };
        let key = key.to_ascii_uppercase();
        let operand = args.get(index + 1);
        let ok = match key.as_str() {
            "ALL" => {
                index += 1;
                true
            }
            "UID" => {
                index += 2;
                matches!(operand, Some(Token::Atom(set)) if sequence_set(set))
            }
            "SINCE" | "BEFORE" | "ON" | "SENTSINCE" | "SENTBEFORE" | "SENTON" => {
                index += 2;
                matches!(operand, Some(Token::Atom(date) | Token::Quoted(date)) if search_date(date))
            }
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    true
}

/// `UID FETCH <set> <item>` or `UID FETCH <set> (<item> ...)`.
fn fetch_args(args: &[Token]) -> bool {
    let (set, items) = match args {
        [Token::Atom(set), Token::Open, items @ .., Token::Close] => (set, items),
        [Token::Atom(set), item @ Token::Atom(_)] => (set, std::slice::from_ref(item)),
        _ => return false,
    };
    sequence_set(set)
        && !items.is_empty()
        && items
            .iter()
            .all(|item| matches!(item, Token::Atom(item) if fetch_item(item)))
}

fn fetch_item(item: &str) -> bool {
    let upper = item.to_ascii_uppercase();
    if FETCH_ITEMS.contains(&upper.as_str()) {
        return true;
    }
    let Some(rest) = upper.strip_prefix("BODY.PEEK[") else {
        return false;
    };
    let Some((section, partial)) = rest.split_once(']') else {
        return false;
    };
    body_section(section) && (partial.is_empty() || fetch_partial(partial))
}

/// A section spec: empty, `HEADER`, `TEXT`, `HEADER.FIELDS[.NOT] (...)`,
/// or a part number optionally followed by one of those or `MIME`.
fn body_section(section: &str) -> bool {
    if section.is_empty() {
        return true;
    }
    let mut rest = section;
    let mut numbered = false;
    loop {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || rest.starts_with('0') {
            break;
        }
        numbered = true;
        rest = &rest[digits..];
        match rest.strip_prefix('.') {
            Some(after) => rest = after,
            None => return rest.is_empty(),
        }
    }
    if numbered && rest == "MIME" {
        return true;
    }
    match rest {
        "HEADER" | "TEXT" => true,
        _ => {
            let fields = rest
                .strip_prefix("HEADER.FIELDS.NOT (")
                .or_else(|| rest.strip_prefix("HEADER.FIELDS ("));
            fields
                .and_then(|fields| fields.strip_suffix(')'))
                .is_some_and(|fields| {
                    !fields.is_empty()
                        && fields.split(' ').all(|name| {
                            !name.is_empty()
                                && name.bytes().all(|byte| {
                                    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
                                })
                        })
                })
        }
    }
}

/// `<origin.count>`, the count non-zero.
fn fetch_partial(partial: &str) -> bool {
    let Some((origin, count)) = partial
        .strip_prefix('<')
        .and_then(|inner| inner.strip_suffix('>'))
        .and_then(|inner| inner.split_once('.'))
    else {
        return false;
    };
    decimal(origin) && decimal(count) && count.bytes().any(|byte| byte != b'0')
}

fn decimal(text: &str) -> bool {
    !text.is_empty() && text.len() <= 10 && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// A sequence set: numbers, `*`, and ranges of them, joined by commas.
fn sequence_set(set: &str) -> bool {
    let number = |text: &str| text == "*" || (decimal(text) && !text.starts_with('0'));
    !set.is_empty()
        && set.split(',').all(|range| match range.split_once(':') {
            Some((low, high)) => number(low) && number(high),
            None => number(range),
        })
}

/// A search date, `d-Mon-yyyy`.
fn search_date(date: &str) -> bool {
    let mut parts = date.split('-');
    let (Some(day), Some(month), Some(year), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    (1..=2).contains(&day.len())
        && decimal(day)
        && month.len() == 3
        && month.bytes().all(|byte| byte.is_ascii_alphabetic())
        && year.len() == 4
        && decimal(year)
}

#[cfg(test)]
mod tests;
