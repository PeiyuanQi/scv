//! Reading and parsing IMAP server responses (RFC 3501, RFC 9051).
//!
//! Framing and parsing are separate: [`read_response`] delimits one response
//! by its lines and announced literals without interpreting it, and
//! [`parse`] turns those bytes into a [`Response`]. A response that does not
//! parse therefore never desynchronizes the connection, so the client can
//! skip a malformed untagged response (one hostile message's structure)
//! instead of failing every later fetch. Parsing is lenient about case and
//! spacing, bounded in depth, and never panics.

use anyhow::{Result, bail};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncReadExt as _};

/// One line outside literals, terminator included.
pub(crate) const MAX_LINE: usize = 64 * 1024;
/// One literal's announced length.
pub(crate) const MAX_LITERAL: usize = 4 * 1024 * 1024;
/// Everything the server sends in answer to one command.
pub(crate) const MAX_RESPONSE: usize = 16 * 1024 * 1024;
/// Nested lists; real BODYSTRUCTUREs stay far below this, and the bound
/// keeps parsing and dropping a hostile tree off the end of the stack.
const MAX_DEPTH: usize = 100;

/// A value in a response, as the server sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Nil,
    Number(u64),
    /// An atom, such as `UID`, `\Seen`, or `IMAP4rev1`, case kept.
    Atom(String),
    /// A quoted string, escapes undone.
    Quoted(Vec<u8>),
    /// A literal's raw bytes.
    Literal(Vec<u8>),
    List(Vec<Value>),
    /// A FETCH key with a section, such as
    /// `BODY[HEADER.FIELDS (MESSAGE-ID)]<0>`: `name` is `BODY`, `spec` is
    /// what the brackets hold, and `origin` the partial's start.
    Section {
        name: String,
        spec: String,
        origin: Option<u64>,
    },
}

impl Value {
    /// The bytes of a string (quoted, literal, or a bare atom where a
    /// string belongs); `None` for NIL and everything else.
    pub(crate) fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Quoted(bytes) | Self::Literal(bytes) => Some(bytes),
            Self::Atom(atom) => Some(atom.as_bytes()),
            _ => None,
        }
    }

    /// A number, also when a lenient server quoted it.
    pub(crate) fn number(&self) -> Option<u64> {
        match self {
            Self::Number(number) => Some(*number),
            Self::Quoted(bytes) => std::str::from_utf8(bytes).ok()?.trim().parse().ok(),
            _ => None,
        }
    }

    pub(crate) fn list(&self) -> Option<&[Value]> {
        match self {
            Self::List(items) => Some(items),
            _ => None,
        }
    }
}

/// A status response's condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Ok,
    No,
    Bad,
    PreAuth,
    Bye,
}

impl Status {
    fn parse(word: &str) -> Option<Self> {
        Some(match word.to_ascii_uppercase().as_str() {
            "OK" => Self::Ok,
            "NO" => Self::No,
            "BAD" => Self::Bad,
            "PREAUTH" => Self::PreAuth,
            "BYE" => Self::Bye,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::No => "NO",
            Self::Bad => "BAD",
            Self::PreAuth => "PREAUTH",
            Self::Bye => "BYE",
        }
    }
}

/// A bracketed response code, such as `[UIDVALIDITY 3857529045]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Code {
    /// Uppercased.
    pub(crate) name: String,
    /// What follows the name, parsed; empty when it does not parse.
    pub(crate) args: Vec<Value>,
}

impl Code {
    /// The code's only argument as a number, such as `UIDNEXT`'s.
    pub(crate) fn number(&self) -> Option<u64> {
        self.args.first().and_then(Value::number)
    }
}

/// One server response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Response {
    /// `A0001 OK [code] text`: a command's completion.
    Tagged {
        tag: String,
        status: Status,
        code: Option<Code>,
        text: Vec<u8>,
    },
    /// `* OK [code] text`, `* BYE`, `* PREAUTH`, and the like.
    Status {
        status: Status,
        code: Option<Code>,
        text: Vec<u8>,
    },
    /// `* KIND values`, such as `* CAPABILITY ...` or `* SEARCH 1 2`;
    /// `kind` is uppercased.
    Data { kind: String, values: Vec<Value> },
    /// `* n KIND values`, such as `* 3 EXISTS` or `* 3 FETCH (...)`;
    /// `kind` is uppercased.
    Message {
        number: u64,
        kind: String,
        values: Vec<Value>,
    },
    /// `+ text`: the server waits for more of the command.
    Continuation { text: Vec<u8> },
}

/// Reads one whole response: its line, and for each literal a line ends by
/// announcing, the literal's bytes and the line after it. Each byte read is
/// charged to `budget`, the bytes left for the current command.
pub(crate) async fn read_response<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    budget: &mut usize,
) -> Result<Vec<u8>> {
    let mut raw = Vec::new();
    loop {
        let start = raw.len();
        read_line(reader, &mut raw, budget).await?;
        let Some(length) = announced_literal(&raw[start..])? else {
            return Ok(raw);
        };
        charge(budget, length)?;
        let at = raw.len();
        raw.resize(at + length, 0);
        reader.read_exact(&mut raw[at..]).await?;
    }
}

fn charge(budget: &mut usize, bytes: usize) -> Result<()> {
    match budget.checked_sub(bytes) {
        Some(left) => {
            *budget = left;
            Ok(())
        }
        None => bail!("the IMAP server sent more than {MAX_RESPONSE} bytes for one command"),
    }
}

/// Appends one line, through its `\n`, to `out`.
async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    out: &mut Vec<u8>,
    budget: &mut usize,
) -> Result<()> {
    let mut length = 0;
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            bail!("the IMAP server closed the connection");
        }
        let (take, done) = match buffer.iter().position(|&byte| byte == b'\n') {
            Some(end) => (end + 1, true),
            None => (buffer.len(), false),
        };
        length += take;
        if length > MAX_LINE {
            bail!("the IMAP server sent a line longer than {MAX_LINE} bytes");
        }
        charge(budget, take)?;
        out.extend_from_slice(&buffer[..take]);
        reader.consume(take);
        if done {
            return Ok(());
        }
    }
}

/// The length of the literal a line announces at its end (`{n}`, `{n+}`,
/// or `~{n}` before the line break), if it does.
fn announced_literal(line: &[u8]) -> Result<Option<usize>> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let Some(inner) = line.strip_suffix(b"}") else {
        return Ok(None);
    };
    let Some(open) = inner.iter().rposition(|&byte| byte == b'{') else {
        return Ok(None);
    };
    let digits = &inner[open + 1..];
    let digits = digits.strip_suffix(b"+").unwrap_or(digits);
    if digits.is_empty() || digits.len() > 10 || !digits.iter().all(u8::is_ascii_digit) {
        return Ok(None);
    }
    let length: usize = std::str::from_utf8(digits)?.parse()?;
    if length > MAX_LITERAL {
        bail!("the IMAP server announced a literal over {MAX_LITERAL} bytes");
    }
    Ok(Some(length))
}

/// Parses one response as [`read_response`] framed it.
pub(crate) fn parse(raw: &[u8]) -> Result<Response> {
    let mut parser = Parser { input: raw, at: 0 };
    parser.response()
}

/// A malformed response. The message never quotes the input: it may be
/// mail content.
fn malformed<T>() -> Result<T> {
    bail!("the IMAP server sent a malformed response")
}

struct Parser<'a> {
    input: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.input.get(self.at).copied()
    }

    fn skip_spaces(&mut self) {
        while self.peek() == Some(b' ') {
            self.at += 1;
        }
    }

    /// Whether only the line break (or nothing) is left.
    fn at_end(&self) -> bool {
        matches!(
            &self.input[self.at.min(self.input.len())..],
            b"" | b"\n" | b"\r\n"
        )
    }

    /// The rest of the current line, without its break.
    fn rest_of_line(&mut self) -> Vec<u8> {
        let rest = &self.input[self.at..];
        let end = rest
            .iter()
            .position(|&byte| matches!(byte, b'\r' | b'\n'))
            .unwrap_or(rest.len());
        self.at += end;
        rest[..end].to_vec()
    }

    /// A run of bytes up to a space or line break.
    fn word(&mut self) -> Result<String> {
        let rest = &self.input[self.at..];
        let end = rest
            .iter()
            .position(|&byte| matches!(byte, b' ' | b'\r' | b'\n'))
            .unwrap_or(rest.len());
        if end == 0 {
            return malformed();
        }
        self.at += end;
        Ok(String::from_utf8_lossy(&rest[..end]).into_owned())
    }

    fn response(&mut self) -> Result<Response> {
        match self.peek() {
            Some(b'+') => {
                self.at += 1;
                self.skip_spaces();
                Ok(Response::Continuation {
                    text: self.rest_of_line(),
                })
            }
            Some(b'*') => {
                self.at += 1;
                if self.peek() != Some(b' ') {
                    return malformed();
                }
                self.skip_spaces();
                self.untagged()
            }
            Some(_) => {
                let tag = self.word()?;
                self.skip_spaces();
                let status = Status::parse(&self.word()?)
                    .filter(|status| matches!(status, Status::Ok | Status::No | Status::Bad));
                let Some(status) = status else {
                    return malformed();
                };
                let (code, text) = self.status_rest();
                Ok(Response::Tagged {
                    tag,
                    status,
                    code,
                    text,
                })
            }
            None => malformed(),
        }
    }

    fn untagged(&mut self) -> Result<Response> {
        let word = self.word()?;
        if word.bytes().all(|byte| byte.is_ascii_digit()) {
            let Ok(number) = word.parse() else {
                return malformed();
            };
            self.skip_spaces();
            let kind = self.word()?.to_ascii_uppercase();
            let values = self.values()?;
            return Ok(Response::Message {
                number,
                kind,
                values,
            });
        }
        if let Some(status) = Status::parse(&word) {
            let (code, text) = self.status_rest();
            return Ok(Response::Status { status, code, text });
        }
        let kind = word.to_ascii_uppercase();
        let values = self.values()?;
        Ok(Response::Data { kind, values })
    }

    /// A status response's optional `[code]` and its free text.
    fn status_rest(&mut self) -> (Option<Code>, Vec<u8>) {
        self.skip_spaces();
        let mut code = None;
        if self.peek() == Some(b'[') {
            let rest = &self.input[self.at + 1..];
            let line = rest
                .iter()
                .position(|&byte| matches!(byte, b'\r' | b'\n'))
                .unwrap_or(rest.len());
            if let Some(close) = rest[..line].iter().position(|&byte| byte == b']') {
                code = parse_code(&rest[..close]);
                self.at += close + 2;
                self.skip_spaces();
            }
        }
        (code, self.rest_of_line())
    }

    /// Values up to the end of the response.
    fn values(&mut self) -> Result<Vec<Value>> {
        let mut values = Vec::new();
        loop {
            self.skip_spaces();
            if self.at_end() {
                return Ok(values);
            }
            values.push(self.value(0)?);
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        match self.peek() {
            Some(b'(') => {
                if depth >= MAX_DEPTH {
                    return malformed();
                }
                self.at += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_spaces();
                    match self.peek() {
                        Some(b')') => {
                            self.at += 1;
                            return Ok(Value::List(items));
                        }
                        None | Some(b'\r' | b'\n') => return malformed(),
                        Some(_) => items.push(self.value(depth + 1)?),
                    }
                }
            }
            Some(b'"') => self.quoted(),
            Some(b'{') => self.literal(),
            Some(b'~') if self.input.get(self.at + 1) == Some(&b'{') => {
                self.at += 1;
                self.literal()
            }
            Some(b')' | b'[' | b'\r' | b'\n') | None => malformed(),
            Some(_) => self.atom(),
        }
    }

    fn quoted(&mut self) -> Result<Value> {
        self.at += 1;
        let mut bytes = Vec::new();
        loop {
            match self.peek() {
                Some(b'"') => {
                    self.at += 1;
                    return Ok(Value::Quoted(bytes));
                }
                Some(b'\\') => {
                    match self.input.get(self.at + 1) {
                        Some(&byte) if !matches!(byte, b'\r' | b'\n') => bytes.push(byte),
                        _ => return malformed(),
                    }
                    self.at += 2;
                }
                Some(b'\r' | b'\n') | None => return malformed(),
                Some(byte) => {
                    bytes.push(byte);
                    self.at += 1;
                }
            }
        }
    }

    fn literal(&mut self) -> Result<Value> {
        self.at += 1;
        let start = self.at;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.at += 1;
        }
        let digits = &self.input[start..self.at];
        if digits.is_empty() || digits.len() > 10 {
            return malformed();
        }
        let Ok(length) = String::from_utf8_lossy(digits).parse::<usize>() else {
            return malformed();
        };
        if self.peek() == Some(b'+') {
            self.at += 1;
        }
        if self.peek() != Some(b'}') {
            return malformed();
        }
        self.at += 1;
        if self.peek() == Some(b'\r') {
            self.at += 1;
        }
        if self.peek() != Some(b'\n') {
            return malformed();
        }
        self.at += 1;
        if length > MAX_LITERAL || self.input.len() - self.at < length {
            return malformed();
        }
        let bytes = self.input[self.at..self.at + length].to_vec();
        self.at += length;
        Ok(Value::Literal(bytes))
    }

    /// An atom, a number, NIL, or a section key.
    fn atom(&mut self) -> Result<Value> {
        let rest = &self.input[self.at..];
        let end = rest
            .iter()
            .position(|&byte| matches!(byte, b' ' | b'(' | b')' | b'"' | b'[' | b'\r' | b'\n'))
            .unwrap_or(rest.len());
        if end == 0 {
            return malformed();
        }
        let word = String::from_utf8_lossy(&rest[..end]).into_owned();
        self.at += end;
        if self.peek() == Some(b'[') {
            return self.section(word);
        }
        if word.eq_ignore_ascii_case("NIL") {
            return Ok(Value::Nil);
        }
        if word.len() <= 20
            && word.bytes().all(|byte| byte.is_ascii_digit())
            && let Ok(number) = word.parse()
        {
            return Ok(Value::Number(number));
        }
        Ok(Value::Atom(word))
    }

    /// `name[spec]<origin>`, the bracket already next.
    fn section(&mut self, name: String) -> Result<Value> {
        self.at += 1;
        let start = self.at;
        let mut quoted = false;
        loop {
            match self.peek() {
                None | Some(b'\r' | b'\n') => return malformed(),
                Some(b'"') => quoted = !quoted,
                Some(b'\\') if quoted => self.at += 1,
                Some(b']') if !quoted => break,
                Some(_) => {}
            }
            self.at += 1;
        }
        let spec = String::from_utf8_lossy(&self.input[start..self.at]).into_owned();
        self.at += 1;
        let mut origin = None;
        if self.peek() == Some(b'<') {
            let rest = &self.input[self.at + 1..];
            let Some(close) = rest.iter().position(|&byte| byte == b'>') else {
                return malformed();
            };
            let Ok(number) = String::from_utf8_lossy(&rest[..close]).parse() else {
                return malformed();
            };
            origin = Some(number);
            self.at += close + 2;
        }
        Ok(Value::Section { name, spec, origin })
    }
}

/// A response code from what its brackets hold; its arguments are best
/// effort, since some codes carry free-form text.
fn parse_code(inner: &[u8]) -> Option<Code> {
    let (name, rest) = match inner.iter().position(|&byte| byte == b' ') {
        Some(space) => (&inner[..space], &inner[space + 1..]),
        None => (inner, &b""[..]),
    };
    if name.is_empty() {
        return None;
    }
    let mut parser = Parser { input: rest, at: 0 };
    Some(Code {
        name: String::from_utf8_lossy(name).to_ascii_uppercase(),
        args: parser.values().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests;
