//! Newline-delimited JSON frame codec for the MCP stdio transport
//! (`mcp-bridge-driver`): one JSON-RPC message per `\n`-terminated UTF-8
//! frame, with hard bounds on frame size and buffered backlog.

use std::error::Error;
use std::fmt;

/// Maximum size of a single frame (bytes between newlines).
pub const MAX_FRAME_BYTES: usize = 1 << 20;
/// Maximum bytes buffered before the peer is declared misbehaving.
pub const MAX_BUFFERED_BYTES: usize = 8 << 20;

/// Incremental frame decoder fed with arbitrary byte chunks.
#[derive(Debug)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    /// Creates an empty decoder.
    #[must_use]
    pub const fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Feeds one chunk and drains every complete frame (`\n`-terminated,
    /// trailing `\r` tolerated for CRLF peers).
    ///
    /// # Errors
    /// Returns [`FrameError::FrameTooLarge`] or [`FrameError::BacklogTooLarge`]
    /// when the peer exceeds the hard bounds; the decoder is left in a
    /// fail-closed state (callers must tear the transport down).
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, FrameError> {
        if self.buffer.len() + chunk.len() > MAX_BUFFERED_BYTES {
            return Err(FrameError::BacklogTooLarge {
                buffered: self.buffer.len() + chunk.len(),
            });
        }
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        let mut start = 0_usize;
        while let Some(offset) = self.buffer[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + offset;
            let frame_len = end - start;
            if frame_len > MAX_FRAME_BYTES {
                return Err(FrameError::FrameTooLarge {
                    frame_bytes: frame_len,
                });
            }
            let mut line = self.buffer[start..end].to_vec();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let frame = String::from_utf8(line).map_err(|_| FrameError::NotUtf8)?;
            frames.push(frame);
            start = end + 1;
        }
        if start > 0 {
            self.buffer.drain(..start);
        }
        Ok(frames)
    }

    /// Bytes currently buffered awaiting a newline.
    #[must_use]
    pub fn pending_bytes(&self) -> usize {
        self.buffer.len()
    }
}

/// Encodes one message as a single `\n`-terminated frame.
///
/// # Errors
/// Returns [`FrameError::FrameTooLarge`] when the encoded message exceeds
/// [`MAX_FRAME_BYTES`].
pub fn encode_frame(message: &str) -> Result<Vec<u8>, FrameError> {
    if message.bytes().any(|byte| byte == b'\n') {
        return Err(FrameError::EmbeddedNewline);
    }
    let mut frame = Vec::with_capacity(message.len() + 1);
    frame.extend_from_slice(message.as_bytes());
    frame.push(b'\n');
    if frame.len() > MAX_FRAME_BYTES {
        return Err(FrameError::FrameTooLarge {
            frame_bytes: frame.len(),
        });
    }
    Ok(frame)
}

/// Fail-closed framing violations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrameError {
    /// A single frame exceeded [`MAX_FRAME_BYTES`].
    FrameTooLarge {
        /// Observed frame size in bytes.
        frame_bytes: usize,
    },
    /// Buffered backlog exceeded [`MAX_BUFFERED_BYTES`].
    BacklogTooLarge {
        /// Observed backlog size in bytes.
        buffered: usize,
    },
    /// A frame was not valid UTF-8.
    NotUtf8,
    /// Encode input contained an embedded newline.
    EmbeddedNewline,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooLarge { frame_bytes } => {
                write!(f, "frame exceeds {MAX_FRAME_BYTES} bytes ({frame_bytes})")
            }
            Self::BacklogTooLarge { buffered } => {
                write!(
                    f,
                    "framing backlog exceeds {MAX_BUFFERED_BYTES} bytes ({buffered})"
                )
            }
            Self::NotUtf8 => f.write_str("frame payload is not valid UTF-8"),
            Self::EmbeddedNewline => f.write_str("message contains an embedded newline"),
        }
    }
}

impl Error for FrameError {}

#[cfg(test)]
mod tests {
    use super::{FrameDecoder, FrameError, MAX_FRAME_BYTES, encode_frame};

    #[test]
    fn decodes_frames_across_chunk_boundaries() {
        let mut decoder = FrameDecoder::new();
        let first = decoder.push(b"{\"a\":1}\n{\"b\"").expect("first chunk");
        assert_eq!(first, vec!["{\"a\":1}".to_owned()]);
        assert_eq!(decoder.pending_bytes(), 4);
        let second = decoder.push(b":2}\n").expect("second chunk");
        assert_eq!(second, vec!["{\"b\":2}".to_owned()]);
        assert_eq!(decoder.pending_bytes(), 0);
    }

    #[test]
    fn tolerates_crlf_and_rejects_oversized_frames() {
        let mut decoder = FrameDecoder::new();
        let frames = decoder.push(b"one\r\ntwo\n").expect("crlf frames");
        assert_eq!(frames, vec!["one".to_owned(), "two".to_owned()]);

        let mut greedy = FrameDecoder::new();
        let mut huge = vec![b'x'; MAX_FRAME_BYTES + 2];
        huge.push(b'\n');
        assert!(matches!(
            greedy.push(&huge),
            Err(FrameError::FrameTooLarge { .. })
        ));

        assert!(encode_frame("no newline").is_ok());
        assert!(matches!(
            encode_frame("bad\nnewline"),
            Err(FrameError::EmbeddedNewline)
        ));
    }
}
