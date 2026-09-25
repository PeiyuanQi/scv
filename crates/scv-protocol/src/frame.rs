//! Newline-delimited framing, without I/O.
//!
//! Every SCV connection carries one JSON object per line. Frame limits are a
//! trust boundary: a peer must not make the other side buffer without bound.
//! [`FrameDecoder`] is the one implementation of that limit; the reading
//! loop that feeds it lives with the I/O (`scv_client::read_frame`).

use serde::Serialize;

/// What a decoder does with a line longer than its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    /// Report [`Frame::TooLarge`] at once, consuming nothing more. The caller
    /// is expected to drop the connection.
    Stop,
    /// Discard the rest of the line, then report [`Frame::TooLarge`], so the
    /// connection can carry on with the next line.
    Skip,
}

/// One result of decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A complete line, without its `\n`.
    Line(Vec<u8>),
    /// A line over the limit.
    TooLarge,
    /// End of input on a line boundary.
    End,
    /// End of input in the middle of a line; the bytes read so far.
    Truncated(Vec<u8>),
}

/// How much of a buffer [`FrameDecoder::feed`] used, and what it completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Bytes of the buffer that belong to the decoder now; consume them.
    pub consumed: usize,
    /// A completed line or an overflow, if any.
    pub frame: Option<Frame>,
}

/// Splits a byte stream into bounded lines.
///
/// The decoder keeps a partial line between calls, so a read loop that is
/// cancelled between buffers loses nothing: feed what the reader has, consume
/// exactly [`Step::consumed`], and call again.
#[derive(Debug, Clone)]
pub struct FrameDecoder {
    /// Most bytes one line may take, counting its `\n`.
    limit: usize,
    overflow: Overflow,
    partial: Vec<u8>,
    discarding: bool,
}

impl FrameDecoder {
    /// A decoder whose lines, counting the `\n`, take at most `limit` bytes.
    pub fn new(limit: usize, overflow: Overflow) -> Self {
        Self {
            limit,
            overflow,
            partial: Vec::new(),
            discarding: false,
        }
    }

    /// Change the limit for the lines still to come.
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
    }

    /// Whether no partial line is buffered or being discarded.
    pub fn is_empty(&self) -> bool {
        self.partial.is_empty() && !self.discarding
    }

    /// Take bytes from `available` up to and including the next `\n`.
    pub fn feed(&mut self, available: &[u8]) -> Step {
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        if !self.discarding {
            if self.partial.len().saturating_add(take) > self.limit {
                match self.overflow {
                    Overflow::Stop => {
                        return Step {
                            consumed: 0,
                            frame: Some(Frame::TooLarge),
                        };
                    }
                    Overflow::Skip => {
                        self.discarding = true;
                        self.partial.clear();
                    }
                }
            } else {
                self.partial.extend_from_slice(&available[..take]);
            }
        }
        let frame = newline.map(|_| {
            if std::mem::take(&mut self.discarding) {
                Frame::TooLarge
            } else {
                let mut line = std::mem::take(&mut self.partial);
                line.pop();
                Frame::Line(line)
            }
        });
        Step {
            consumed: take,
            frame,
        }
    }

    /// The input ended: what was left, if anything.
    pub fn finish(&mut self) -> Frame {
        if std::mem::take(&mut self.discarding) {
            Frame::TooLarge
        } else if self.partial.is_empty() {
            Frame::End
        } else {
            Frame::Truncated(std::mem::take(&mut self.partial))
        }
    }
}

/// Strip a line's trailing `\r` (and `\n`) and check it against `max_bytes`:
/// the rule for peers that may send CRLF and whose limit excludes the line
/// ending. `None` means too large.
pub fn trim_line(mut line: Vec<u8>, max_bytes: usize) -> Option<Vec<u8>> {
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    (line.len() <= max_bytes).then_some(line)
}

/// Encode `message` as one frame: its JSON and a `\n`.
pub fn encode_frame(message: &impl Serialize) -> serde_json::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(message)?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
mod tests;
