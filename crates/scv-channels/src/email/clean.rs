//! Making mail text safe to show and cheap to send to a model: sanitizing
//! untrusted text for the mail chat, and cleaning a body for triage.

use html2text::render::{TaggedLine, TextDecorator};

/// Untrusted text with every character that could disguise it removed:
/// CRLF and CR become LF, U+2028 and U+2029 become LF, and NUL and other C0
/// controls except LF and TAB, DEL and C1 controls, bidi controls,
/// zero-width and invisible-operator characters, and Unicode tag characters
/// are dropped.
pub(crate) fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                chars.next_if_eq(&'\n');
                out.push('\n');
            }
            '\u{2028}' | '\u{2029}' => out.push('\n'),
            '\n' | '\t' => out.push(c),
            c if hidden(c) => {}
            c => out.push(c),
        }
    }
    out
}

/// A character [`sanitize`] drops: it controls the terminal or the text
/// direction, or it takes no space and so can hide or split words.
fn hidden(c: char) -> bool {
    matches!(
        c,
        '\0'..='\u{1F}'
            | '\u{7F}'..='\u{9F}'
            | '\u{61C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

/// Each URL in `text` (`http://`, `https://`, and `ftp://` in any case, and
/// bare `www.` hosts) replaced with `[link: <host>]`, or `[link]` when it has
/// no valid host. The host is lowercased, without user info or port, so
/// `http://bank.com@evil.com` shows `evil.com`. Trailing punctuation stays
/// outside the link, and angle brackets around it go with it. `mailto:`
/// addresses stay as they are.
pub(crate) fn replace_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut previous = None;
    while let Some(c) = rest.chars().next() {
        let Some((len, scheme)) = url_at(rest, previous) else {
            out.push(c);
            previous = Some(c);
            rest = &rest[c.len_utf8()..];
            continue;
        };
        let url = &rest[..len];
        rest = &rest[len..];
        if previous == Some('<')
            && let Some(after) = rest.strip_prefix('>')
        {
            out.pop();
            rest = after;
        }
        let shown = host(&url[scheme..])
            .map_or_else(|| "[link]".to_owned(), |host| format!("[link: {host}]"));
        // A link written out as its own text shows once.
        if out.ends_with(&format!("{shown} ")) {
            out.pop();
        } else {
            out.push_str(&shown);
        }
        previous = Some(']');
    }
    out
}

/// The URL starting `text`, if one does: its length without trailing
/// punctuation, and the length of its scheme (0 for a bare `www.` host).
/// `previous` is the character before it.
fn url_at(text: &str, previous: Option<char>) -> Option<(usize, usize)> {
    let scheme = ["http://", "https://", "ftp://"]
        .iter()
        .find(|scheme| starts_with_ignore_case(text, scheme))
        .map(|scheme| scheme.len());
    let scheme = match scheme {
        Some(scheme) => scheme,
        // A bare host only where a word starts, so addresses and longer
        // names are not split.
        None if starts_with_ignore_case(text, "www.")
            && text[4..].chars().next().is_some_and(char::is_alphanumeric)
            && !previous.is_some_and(|c| {
                c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | '/' | ':')
            }) =>
        {
            0
        }
        None => return None,
    };
    let end = text.find(url_end).unwrap_or(text.len());
    let url = &text[..end];
    let mut open = [0usize; 3];
    let mut close = [0usize; 3];
    for c in url.chars() {
        if let Some(kind) = "([{".find(c) {
            open[kind] += 1;
        } else if let Some(kind) = ")]}".find(c) {
            close[kind] += 1;
        }
    }
    let mut len = end;
    while len > scheme {
        let Some(last) = url[..len].chars().next_back() else {
            break;
        };
        let trailing = match ")]}".find(last) {
            // A closing bracket is the URL's own when it balances one inside.
            Some(kind) if close[kind] > open[kind] => {
                close[kind] -= 1;
                true
            }
            Some(_) => false,
            None => ".,;:!?'".contains(last) || TRAILING_CJK.contains(last),
        };
        if !trailing {
            break;
        }
        len -= last.len_utf8();
    }
    Some((len, scheme))
}

/// CJK punctuation that ends a sentence around a URL rather than belonging
/// to it.
const TRAILING_CJK: &str = "。，；：！？）」』】";

/// Whether `c` ends a URL wherever it appears: whitespace, a control, a
/// delimiter, or CJK punctuation, none of which a URL holds unencoded.
fn url_end(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || matches!(c, '<' | '>' | '"' | '“' | '”' | '‘' | '’')
        || "。，、；：！？（）「」『』【】《》〈〉".contains(c)
}

/// The host of a URL without its scheme, lowercased, without user info,
/// port, or a final dot; `None` when it is not a plausible host.
fn host(url: &str) -> Option<String> {
    let authority = &url[..url.find(['/', '?', '#', '\\']).unwrap_or(url.len())];
    // The last `@` ends the user info, as browsers read it.
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let digits = |port: &str| port.bytes().all(|byte| byte.is_ascii_digit());
    let host = if let Some(literal) = host_port.strip_prefix('[') {
        let (inner, port) = literal.split_once(']')?;
        let port_ok = port.is_empty() || port.strip_prefix(':').is_some_and(digits);
        let inner_ok = !inner.is_empty()
            && inner
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || matches!(byte, b':' | b'.'));
        if !(port_ok && inner_ok) {
            return None;
        }
        &host_port[..inner.len() + 2]
    } else {
        let host = match host_port.rsplit_once(':') {
            Some((host, port)) if digits(port) => host,
            Some(_) => return None,
            None => host_port,
        };
        let host = host.trim_end_matches('.');
        let valid = host.chars().any(char::is_alphanumeric)
            && host
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '.' | '_'));
        if !valid {
            return None;
        }
        host
    };
    Some(host.to_lowercase())
}

/// Whether `text` starts with the ASCII `prefix`, ignoring ASCII case.
fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// The most HTML [`html_to_text`] reads; the rest is dropped.
const MAX_HTML_BYTES: usize = 1 << 20;

/// The deepest element nesting [`html_to_text`] hands to the HTML renderer;
/// deeper markup is read by the tag stripper, whose cost does not grow with
/// depth.
const MAX_HTML_DEPTH: usize = 256;

/// The line width HTML is laid out at: wider than any input read, so
/// paragraphs never wrap.
const HTML_WIDTH: usize = MAX_HTML_BYTES;

/// HTML as the plain text a reader sees: no link targets or footnotes, no
/// `head`, `style`, or `script`, images as their alt text, quoted blocks
/// prefixed `> `, and tables read cell by cell. At most the first MiB is
/// read.
pub(crate) fn html_to_text(html: &str) -> String {
    let html = &html[..floor_char_boundary(html, MAX_HTML_BYTES)];
    if nesting_depth(html) <= MAX_HTML_DEPTH {
        // The renderer is third-party code on untrusted input; a panic in it
        // falls back to the tag stripper like an error does.
        let rendered = std::panic::catch_unwind(|| {
            html2text::config::with_decorator(PlainText::default())
                .raw_mode(true)
                .unicode_strikeout(false)
                .allow_width_overflow()
                .string_from_read(html.as_bytes(), HTML_WIDTH)
        });
        if let Ok(Ok(text)) = rendered {
            return text;
        }
    }
    strip_tags(html)
}

/// The largest char boundary in `text` at or before `index`.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    (0..=index)
        .rev()
        .find(|&at| text.is_char_boundary(at))
        .unwrap_or(0)
}

/// An upper estimate of how deeply the elements of `html` nest: every start
/// tag opens a level and every end tag closes one, except void elements and
/// those whose end tag is optional (they cannot nest on their own).
fn nesting_depth(html: &str) -> usize {
    const FLAT: &[&str] = &[
        "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
        "source", "track", "wbr", "keygen", "p", "li", "dt", "dd", "option", "optgroup", "tr",
        "td", "th", "tbody", "thead", "tfoot", "colgroup", "rb", "rt", "rtc", "rp",
    ];
    let bytes = html.as_bytes();
    let (mut depth, mut deepest) = (0usize, 0usize);
    let mut at = 0;
    while let Some(offset) = bytes[at..].iter().position(|&byte| byte == b'<') {
        at += offset + 1;
        if bytes[at..].starts_with(b"!--") {
            at = html[at + 3..]
                .find("-->")
                .map_or(bytes.len(), |end| at + 3 + end + 3);
            continue;
        }
        let closing = bytes.get(at) == Some(&b'/');
        let name_start = at + usize::from(closing);
        let name_len = bytes[name_start..]
            .iter()
            .take_while(|byte| byte.is_ascii_alphanumeric())
            .count();
        if name_len == 0 || !bytes[name_start].is_ascii_alphabetic() {
            continue;
        }
        let name = &bytes[name_start..name_start + name_len];
        if FLAT
            .iter()
            .any(|flat| name.eq_ignore_ascii_case(flat.as_bytes()))
        {
            continue;
        }
        if closing {
            depth = depth.saturating_sub(1);
        } else {
            depth += 1;
            deepest = deepest.max(depth);
        }
    }
    deepest
}

/// HTML reduced to its text without a parser: tags dropped (block tags
/// leave a line break), comments and `head`, `script`, and `style` contents
/// dropped, whitespace collapsed, and common entities decoded.
fn strip_tags(html: &str) -> String {
    const BLOCK: &[&str] = &[
        "address",
        "article",
        "aside",
        "blockquote",
        "br",
        "center",
        "dd",
        "div",
        "dl",
        "dt",
        "fieldset",
        "figure",
        "footer",
        "form",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "header",
        "hr",
        "li",
        "main",
        "nav",
        "ol",
        "p",
        "pre",
        "section",
        "table",
        "tr",
        "ul",
    ];
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        push_html_text(&mut out, &rest[..open]);
        rest = &rest[open..];
        if let Some(comment) = rest.strip_prefix("<!--") {
            rest = comment.find("-->").map_or("", |end| &comment[end + 3..]);
            continue;
        }
        let tag = rest[1..].strip_prefix('/').unwrap_or(&rest[1..]);
        let name_len = tag.bytes().take_while(u8::is_ascii_alphanumeric).count();
        let declaration = rest[1..].starts_with(['!', '?']);
        if !declaration && (name_len == 0 || !tag.as_bytes()[0].is_ascii_alphabetic()) {
            push_html_text(&mut out, "<");
            rest = &rest[1..];
            continue;
        }
        let name = tag[..name_len].to_ascii_lowercase();
        let closing = rest[1..].starts_with('/');
        rest = rest.find('>').map_or("", |end| &rest[end + 1..]);
        if declaration {
            continue;
        }
        if !closing && matches!(name.as_str(), "head" | "script" | "style") {
            rest = after_end_tag(rest, &name);
        } else if BLOCK.contains(&name.as_str()) {
            out.push('\n');
        } else if matches!(name.as_str(), "td" | "th") {
            out.push(' ');
        }
    }
    push_html_text(&mut out, rest);
    out
}

/// `html` after the end tag `</name>`, or empty when it has none.
fn after_end_tag<'a>(html: &'a str, name: &str) -> &'a str {
    for (at, _) in html.match_indices("</") {
        let tail = &html[at + 2..];
        let whole_name = !tail
            .as_bytes()
            .get(name.len())
            .is_some_and(u8::is_ascii_alphanumeric);
        if starts_with_ignore_case(tail, name) && whole_name {
            return tail.find('>').map_or("", |end| &tail[end + 1..]);
        }
    }
    ""
}

/// Appends HTML character data: runs of whitespace as one space and the
/// common character references decoded.
fn push_html_text(out: &mut String, text: &str) {
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if c.is_ascii_whitespace() {
            if !out.ends_with([' ', '\n']) {
                out.push(' ');
            }
            rest = &rest[1..];
            continue;
        }
        if c == '&'
            && let Some((decoded, len)) = char_reference(rest)
        {
            out.push(decoded);
            rest = &rest[len..];
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
}

/// The character a reference such as `&amp;` or `&#x4E2D;` at the start of
/// `text` stands for, and its length.
fn char_reference(text: &str) -> Option<(char, usize)> {
    let end = text.bytes().take(12).position(|byte| byte == b';')?;
    let body = &text[1..end];
    let decoded = match body {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{A0}',
        _ => {
            let number = body.strip_prefix('#')?;
            let value = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(value)
                .filter(|&c| c != '\0')
                .unwrap_or(char::REPLACEMENT_CHARACTER)
        }
    };
    Some((decoded, end + 1))
}

/// Renders HTML as the text a reader sees: no link targets, emphasis, or
/// heading marks. Quoted blocks keep `> ` so quoted history can be found.
#[derive(Clone, Default)]
struct PlainText {
    /// The web targets of the links being rendered, innermost last; `None`
    /// for a link to anything else.
    links: Vec<Option<String>>,
}

impl TextDecorator for PlainText {
    type Annotation = ();

    fn decorate_link_start(&mut self, url: &str) -> (String, ()) {
        let target = url.split_whitespace().next().unwrap_or_default();
        let web = ["http://", "https://", "ftp://"]
            .iter()
            .any(|scheme| starts_with_ignore_case(target, scheme));
        self.links.push(web.then(|| target.to_owned()));
        (String::new(), ())
    }

    /// A link's real target follows its text, which [`replace_links`] then
    /// shows as its host: text that looks like one address cannot hide that
    /// the link goes to another.
    fn decorate_link_end(&mut self) -> String {
        match self.links.pop().flatten() {
            Some(target) => format!(" {target}"),
            None => String::new(),
        }
    }

    fn decorate_em_start(&self) -> (String, ()) {
        (String::new(), ())
    }

    fn decorate_em_end(&self) -> String {
        String::new()
    }

    fn decorate_strong_start(&self) -> (String, ()) {
        (String::new(), ())
    }

    fn decorate_strong_end(&self) -> String {
        String::new()
    }

    fn decorate_strikeout_start(&self) -> (String, ()) {
        (String::new(), ())
    }

    fn decorate_strikeout_end(&self) -> String {
        String::new()
    }

    fn decorate_code_start(&self) -> (String, ()) {
        (String::new(), ())
    }

    fn decorate_code_end(&self) -> String {
        String::new()
    }

    fn decorate_preformat_first(&self) {}

    fn decorate_preformat_cont(&self) {}

    fn decorate_image(&mut self, _src: &str, title: &str) -> (String, ()) {
        (title.to_owned(), ())
    }

    fn header_prefix(&self, _level: usize) -> String {
        String::new()
    }

    fn quote_prefix(&self) -> String {
        "> ".to_owned()
    }

    fn unordered_item_prefix(&self) -> String {
        "- ".to_owned()
    }

    fn ordered_item_prefix(&self, i: i64) -> String {
        format!("{i}. ")
    }

    fn make_subblock_decorator(&self) -> Self {
        Self::default()
    }

    fn decorate_superscript_start(&self) -> (String, ()) {
        (String::new(), ())
    }

    fn decorate_superscript_end(&self) -> String {
        String::new()
    }

    fn finalise(&mut self, _urls: Vec<String>) -> Vec<TaggedLine<()>> {
        Vec::new()
    }
}

/// A body cleaned for a model: HTML turned into text, links replaced,
/// quoted history and signatures removed, whitespace collapsed, and cut at
/// a line boundary to at most `max_bytes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cleaned {
    pub(crate) text: String,
    /// Text was cut to fit.
    pub(crate) truncated: bool,
}

/// The line that ends a cut body.
const TRUNCATED: &str = "[truncated]";

/// `text` (HTML when `html`) cleaned for a model: sanitized, links replaced,
/// quoted lines, quoted history from its reply header on, and the signature
/// removed, whitespace collapsed, and cut to at most `max_bytes` with a
/// final `[truncated]` line. A body that is nothing but quoted history keeps
/// it, so the model still sees what arrived.
pub(crate) fn clean_body(text: &str, html: bool, max_bytes: usize) -> Cleaned {
    let converted;
    let text = if html {
        converted = html_to_text(text);
        converted.as_str()
    } else {
        text
    };
    let text = replace_links(&sanitize(text));
    let lines: Vec<&str> = text.split('\n').collect();
    let mut cleaned = tidy(&without_signature(&without_history(&lines)));
    if cleaned.is_empty() {
        cleaned = tidy(&lines);
    }
    truncate(cleaned, max_bytes)
}

/// `lines` without quoted lines and without everything from the first
/// reply header on.
fn without_history<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    let rule = |line: &str| {
        let line = line.trim();
        line.len() >= 5 && line.bytes().all(|byte| byte == b'_')
    };
    let mut end = (0..lines.len())
        .find(|&at| !quoted(lines[at]) && reply_header(&lines[at..]))
        .unwrap_or(lines.len());
    // Outlook rules the header off with underscores.
    while end > 0 && end < lines.len() && rule(lines[end - 1]) {
        end -= 1;
    }
    lines[..end]
        .iter()
        .copied()
        .filter(|line| !quoted(line))
        .collect()
}

/// Whether `line` is quoted: it starts with `>` after optional blanks.
fn quoted(line: &str) -> bool {
    line.trim_start().starts_with('>')
}

/// The longest attribution line (`On … wrote:`) recognized, in bytes.
const MAX_ATTRIBUTION_BYTES: usize = 400;

/// Whether `lines` start with a reply header that introduces quoted
/// history.
fn reply_header(lines: &[&str]) -> bool {
    let line = lines[0].trim();
    let next = lines.get(1).map_or("", |next| next.trim());
    attribution(line, next, "On ", &["wrote:"])
        || attribution(line, next, "在", &["写道：", "写道:"])
        || marker(line)
        || header_block(lines, &["From:"], &["Sent:", "Date:"], &["To:", "Subject:"])
        || header_block(
            lines,
            &["发件人:", "发件人："],
            &[
                "发送时间:",
                "发送时间：",
                "发送日期:",
                "发送日期：",
                "日期:",
                "日期：",
            ],
            &[],
        )
}

/// Whether `line`, or `line` wrapped onto `next`, is an attribution such as
/// `On Mon, 1 Jan 2026 at 10:00, Ann <ann@example.com> wrote:`: it starts
/// with `start`, ends with one of `ends`, and names a date or an address.
fn attribution(line: &str, next: &str, start: &str, ends: &[&str]) -> bool {
    if !line.starts_with(start) {
        return false;
    }
    let dated = |text: &str| {
        text.bytes()
            .any(|byte| byte.is_ascii_digit() || byte == b'@')
    };
    if ends.iter().any(|end| line.ends_with(end)) {
        return line.len() <= MAX_ATTRIBUTION_BYTES && dated(line);
    }
    ends.iter().any(|end| next.ends_with(end))
        && line.len() + next.len() <= MAX_ATTRIBUTION_BYTES
        && (dated(line) || dated(next))
}

/// Whether `line` is a dashed marker such as `-----Original Message-----`
/// or `---- 回复的原邮件 ----`.
fn marker(line: &str) -> bool {
    let inner = line.trim_start_matches('-');
    let lead = line.len() - inner.len();
    let label = inner.trim_end_matches('-');
    let trail = inner.len() - label.len();
    let label = label.trim();
    lead >= 2
        && trail >= 2
        && [
            "original message",
            "original",
            "原始邮件",
            "邮件原件",
            "回复的原邮件",
        ]
        .iter()
        .any(|name| label.eq_ignore_ascii_case(name))
}

/// Whether `lines` start with a header block: a line starting with one of
/// `from`, followed within four lines by one starting with one of `date`
/// and, unless `to` is empty, one starting with one of `to`.
fn header_block(lines: &[&str], from: &[&str], date: &[&str], to: &[&str]) -> bool {
    let starts = |line: &str, names: &[&str]| {
        let line = line.trim_start();
        names.iter().any(|name| starts_with_ignore_case(line, name))
    };
    let window = || lines.iter().skip(1).take(4);
    starts(lines[0], from)
        && window().any(|line| starts(line, date))
        && (to.is_empty() || window().any(|line| starts(line, to)))
}

/// `lines` without a signature: everything from a `-- ` or `--` line on,
/// when it follows the first non-empty line.
fn without_signature<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    let end = lines
        .iter()
        .position(|line| !line.trim().is_empty())
        .and_then(|first| {
            lines[first + 1..]
                .iter()
                .position(|line| matches!(*line, "-- " | "--"))
                .map(|at| first + 1 + at)
        })
        .unwrap_or(lines.len());
    lines[..end].to_vec()
}

/// `lines` joined with each line's whitespace runs collapsed to one space
/// and its ends trimmed, at most one blank line in a row, and no blank
/// lines around.
fn tidy(lines: &[&str]) -> String {
    let mut out = String::new();
    let mut blank = false;
    for line in lines {
        let mut words = line.split_whitespace();
        let Some(first) = words.next() else {
            blank = !out.is_empty();
            continue;
        };
        if !out.is_empty() {
            out.push_str(if blank { "\n\n" } else { "\n" });
        }
        blank = false;
        out.push_str(first);
        for word in words {
            out.push(' ');
            out.push_str(word);
        }
    }
    out
}

/// `text` cut to at most `max_bytes` including a final `[truncated]` line:
/// at the last line boundary that fits, else at a char boundary.
fn truncate(text: String, max_bytes: usize) -> Cleaned {
    if text.len() <= max_bytes {
        return Cleaned {
            text,
            truncated: false,
        };
    }
    let Some(budget) = max_bytes.checked_sub(TRUNCATED.len()) else {
        return Cleaned {
            text: text[..floor_char_boundary(&text, max_bytes)]
                .trim_end()
                .to_owned(),
            truncated: true,
        };
    };
    // The kept text and the line break before the marker.
    let budget = budget.saturating_sub(1);
    let by_line = text.as_bytes()[..=budget]
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map(|end| text[..end].trim_end())
        .filter(|kept| !kept.is_empty());
    let kept = by_line.unwrap_or_else(|| text[..floor_char_boundary(&text, budget)].trim_end());
    let text = if kept.is_empty() {
        TRUNCATED.to_owned()
    } else {
        format!("{kept}\n{TRUNCATED}")
    };
    Cleaned {
        text,
        truncated: true,
    }
}

#[cfg(test)]
mod tests;
