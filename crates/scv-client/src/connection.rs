//! Framed JSON over a byte stream: the reading loop around
//! [`FrameDecoder`], and a [`Connection`] that pairs it with a writer.

use scv_protocol::{ClientMessage, Frame, FrameDecoder, encode_frame};
use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Read the next frame from `reader`.
///
/// Cancel-safe: bytes are consumed from `reader` only as `decoder` takes
/// them, and a partial line stays in `decoder`, so a read abandoned in a
/// `select!` resumes where it stopped.
pub async fn read_frame<R>(reader: &mut R, decoder: &mut FrameDecoder) -> io::Result<Frame>
where
    R: AsyncBufRead + Unpin,
{
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(decoder.finish());
        }
        let step = decoder.feed(available);
        reader.consume(step.consumed);
        if let Some(frame) = step.frame {
            return Ok(frame);
        }
    }
}

/// Write `message` as one frame and flush it.
pub async fn write_message<W>(writer: &mut W, message: &ClientMessage) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let bytes = encode_frame(message).map_err(io::Error::other)?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

/// A client's side of one protocol connection: a buffered reader, a writer,
/// and the decoder that bounds what the server may send.
#[derive(Debug)]
pub struct Connection<R, W> {
    reader: R,
    writer: W,
    decoder: FrameDecoder,
}

impl<R, W> Connection<R, W>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Wrap a stream's halves; `decoder` sets the frame limit.
    pub fn new(reader: R, writer: W, decoder: FrameDecoder) -> Self {
        Self {
            reader,
            writer,
            decoder,
        }
    }

    /// Send one message.
    pub async fn send(&mut self, message: &ClientMessage) -> io::Result<()> {
        write_message(&mut self.writer, message).await
    }

    /// The next frame. Cancel-safe, like [`read_frame`].
    pub async fn read(&mut self) -> io::Result<Frame> {
        read_frame(&mut self.reader, &mut self.decoder).await
    }

    /// The decoder, to change its limit once the peer declares one.
    #[cfg(test)]
    pub(crate) fn decoder_mut(&mut self) -> &mut FrameDecoder {
        &mut self.decoder
    }
}

#[cfg(test)]
mod tests;
