//! A scripted IMAP server for tests, over an in-memory stream.
//!
//! It answers each command from a script and records every command it
//! receives. It fails the test, by panicking its task (which
//! [`Fake::finish`] re-raises), when a command is not the one scripted,
//! when one arrives after the script ends, and when one could change a
//! mailbox: that last check is its own, independent of the client's guard.

use tokio::io::{
    AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream, ReadHalf,
    WriteHalf,
};
use tokio::task::JoinHandle;

/// One exchange of the script.
pub(crate) enum Step {
    /// A command whose text after the tag is exactly `expect` (literals
    /// inline, as `{n}\r\n` and their bytes), answered with `reply`, in
    /// which `{tag}` stands for the command's tag.
    Command { expect: String, reply: String },
    /// A line sent after a `+` (a SASL response), answered with `reply`,
    /// `{tag}` standing for the last command's tag.
    Line { expect: String, reply: String },
    /// Reads the next command and never answers it.
    Hang,
}

pub(crate) fn command(expect: &str, reply: &str) -> Step {
    Step::Command {
        expect: expect.to_owned(),
        reply: reply.to_owned(),
    }
}

pub(crate) fn line(expect: &str, reply: &str) -> Step {
    Step::Line {
        expect: expect.to_owned(),
        reply: reply.to_owned(),
    }
}

/// The running server.
pub(crate) struct Fake {
    task: JoinHandle<Vec<String>>,
}

/// Starts a server that sends `greeting` and then follows `script`; the
/// returned stream is the client's end.
pub(crate) fn serve(greeting: &str, script: Vec<Step>) -> (DuplexStream, Fake) {
    let (client, server) = tokio::io::duplex(1 << 16);
    let task = tokio::spawn(run(server, greeting.to_owned(), script));
    (client, Fake { task })
}

impl Fake {
    /// Every command line received, tags and literals included, once the
    /// client has closed its end. Re-raises the server's panic.
    pub(crate) async fn finish(self) -> Vec<String> {
        match self.task.await {
            Ok(received) => received,
            Err(error) => std::panic::resume_unwind(error.into_panic()),
        }
    }
}

type Reader = BufReader<ReadHalf<DuplexStream>>;
type Writer = WriteHalf<DuplexStream>;

async fn run(stream: DuplexStream, greeting: String, script: Vec<Step>) -> Vec<String> {
    let (read, mut write) = tokio::io::split(stream);
    let mut read = BufReader::new(read);
    let mut received = Vec::new();
    let _ = write.write_all(greeting.as_bytes()).await;
    let mut last_tag = String::new();
    for step in script {
        match step {
            Step::Command { expect, reply } => {
                let Some(command) = read_command(&mut read, &mut write).await else {
                    panic!("the client closed the connection before sending `{expect}`");
                };
                received.push(command.clone());
                let (tag, text) = command.split_once(' ').unwrap_or((&command, ""));
                assert_read_only(text);
                assert_eq!(text, expect, "the client sent an unexpected command");
                last_tag = tag.to_owned();
                let _ = write
                    .write_all(reply.replace("{tag}", tag).as_bytes())
                    .await;
            }
            Step::Line { expect, reply } => {
                let mut line = Vec::new();
                let _ = read.read_until(b'\n', &mut line).await;
                let line = String::from_utf8_lossy(&line).trim_end().to_owned();
                received.push(line.clone());
                assert_eq!(line, expect, "the client sent an unexpected line");
                let _ = write
                    .write_all(reply.replace("{tag}", &last_tag).as_bytes())
                    .await;
            }
            Step::Hang => {
                if let Some(command) = read_command(&mut read, &mut write).await {
                    let text = command.split_once(' ').map_or("", |(_, text)| text);
                    assert_read_only(text);
                    received.push(command);
                }
                let mut rest = Vec::new();
                let _ = read.read_to_end(&mut rest).await;
                assert!(
                    rest.is_empty(),
                    "the client sent more to a server that hung"
                );
                return received;
            }
        }
    }
    if let Some(command) = read_command(&mut read, &mut write).await {
        let text = command.split_once(' ').map_or("", |(_, text)| text);
        assert_read_only(text);
        panic!("the client sent an unscripted command: {command}");
    }
    received
}

/// One command, literals inline: for each line that ends by announcing a
/// literal, the server answers `+` and reads its bytes. `None` at the end
/// of the stream.
async fn read_command(read: &mut Reader, write: &mut Writer) -> Option<String> {
    let mut command = Vec::new();
    loop {
        let mut line = Vec::new();
        if read.read_until(b'\n', &mut line).await.ok()? == 0 {
            return None;
        }
        let Some(length) = literal(&line) else {
            let line = line.strip_suffix(b"\n").unwrap_or(&line);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            command.extend_from_slice(line);
            return Some(String::from_utf8_lossy(&command).into_owned());
        };
        command.extend_from_slice(&line);
        write.write_all(b"+ go ahead\r\n").await.ok()?;
        let mut bytes = vec![0; length];
        read.read_exact(&mut bytes).await.ok()?;
        command.extend_from_slice(&bytes);
    }
}

fn literal(line: &[u8]) -> Option<usize> {
    let line = std::str::from_utf8(line)
        .ok()?
        .trim_end_matches(['\r', '\n']);
    let inner = line.strip_suffix('}')?;
    let digits = &inner[inner.rfind('{')? + 1..];
    digits.parse().ok()
}

/// Fails the test unless `text` (a command after its tag) is one a
/// read-only reader may send.
fn assert_read_only(text: &str) {
    let upper = text.to_ascii_uppercase();
    let mut words = upper.split(' ');
    let verb = words.next().unwrap_or_default();
    let allowed = match verb {
        "CAPABILITY" | "NOOP" | "LOGOUT" | "ID" | "LOGIN" | "EXAMINE" | "LIST" | "STATUS" => true,
        "AUTHENTICATE" => words.next() == Some("PLAIN"),
        "UID" => match words.next() {
            Some("SEARCH") => true,
            Some("FETCH") => {
                !upper.contains("BODY[")
                    && !upper.contains("BINARY")
                    && !upper.contains("RFC822.TEXT")
                    && !upper.split([' ', '(', ')']).any(|word| word == "RFC822")
            }
            _ => false,
        },
        _ => false,
    };
    assert!(
        allowed,
        "the fake server received a command that is not read-only: {text}"
    );
}
