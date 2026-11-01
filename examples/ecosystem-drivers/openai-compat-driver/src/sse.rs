//! Server-Sent-Events stream parsing for streamed OpenAI-compatible chat
//! completions (`chat.completion.chunk` deltas terminated by `data: [DONE]`).

use std::error::Error;
use std::fmt;

use ecosystem_driver_kit::JsonValue;

/// Incremental SSE event parser fed with arbitrary byte chunks.
#[derive(Debug)]
pub struct SseParser {
    buffer: Vec<u8>,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    /// Creates an empty parser.
    #[must_use]
    pub const fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Feeds one chunk and drains every complete SSE event (blank-line
    /// terminated block of `field: value` lines).
    ///
    /// # Errors
    /// Returns [`SseError::EventTooLarge`] when a single event exceeds
    /// [`MAX_SSE_EVENT_BYTES`].
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, SseError> {
        const MAX_SSE_EVENT_BYTES: usize = 1 << 20;
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(separator) = find_blank_line(&self.buffer) {
            if separator > MAX_SSE_EVENT_BYTES {
                return Err(SseError::EventTooLarge(separator));
            }
            let block = self.buffer[..separator].to_vec();
            self.buffer
                .drain(..separator + blank_line_len(&self.buffer, separator));
            if let Some(event) = SseEvent::parse(&block) {
                events.push(event);
            }
        }
        Ok(events)
    }

    /// Bytes buffered awaiting a blank-line terminator.
    #[must_use]
    pub fn pending_bytes(&self) -> usize {
        self.buffer.len()
    }
}

/// Maximum size of a single SSE event.
pub const MAX_SSE_EVENT_BYTES: usize = 1 << 20;

fn find_blank_line(buffer: &[u8]) -> Option<usize> {
    let mut index = 0;
    while index < buffer.len() {
        match buffer[index..].iter().position(|byte| *byte == b'\n') {
            Some(offset) => {
                let line_end = index + offset;
                let next = line_end + 1;
                let line = &buffer[index..line_end];
                let line_trimmed = strip_cr(line);
                if line_trimmed.is_empty() {
                    return Some(index);
                }
                index = next;
            }
            None => break,
        }
    }
    None
}

fn blank_line_len(buffer: &[u8], at: usize) -> usize {
    // `at` points at the empty line's newline (or its `\r`); consume it plus
    // the terminator.
    let mut length = 1;
    if buffer[at] == b'\r' {
        length += 1;
    }
    length
}

fn strip_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// One parsed SSE event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SseEvent {
    /// Joined `data:` payload lines.
    pub data: String,
    /// `event:` field when present.
    pub event: Option<String>,
    /// `id:` field when present.
    pub id: Option<String>,
}

impl SseEvent {
    fn parse(block: &[u8]) -> Option<Self> {
        let text = String::from_utf8_lossy(block);
        let mut data_lines: Vec<&str> = Vec::new();
        let mut event = None;
        let mut id = None;
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            if let Some(value) = line.strip_prefix("data:") {
                data_lines.push(value.trim_start_matches(' '));
            } else if let Some(value) = line.strip_prefix("event:") {
                event = Some(value.trim_start_matches(' ').to_owned());
            } else if let Some(value) = line.strip_prefix("id:") {
                id = Some(value.trim_start_matches(' ').to_owned());
            }
        }
        if data_lines.is_empty() {
            return None;
        }
        Some(Self {
            data: data_lines.join("\n"),
            event,
            id,
        })
    }
}

/// The stream sentinel terminating a chat completion stream.
pub const DONE_SENTINEL: &str = "[DONE]";

/// Aggregate state of one streamed chat completion.
#[derive(Clone, Debug, Default)]
pub struct StreamAggregator {
    /// Concatenated assistant deltas.
    pub text: String,
    /// Last observed `finish_reason`.
    pub finish_reason: Option<String>,
    /// Usage from the final `include_usage` chunk (when requested).
    pub usage: Option<crate::chat::ChatUsage>,
    /// Whether the `[DONE]` sentinel has been observed.
    pub done: bool,
    /// Number of chunk events consumed.
    pub chunks: u64,
}

impl StreamAggregator {
    /// Creates an empty aggregator.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            text: String::new(),
            finish_reason: None,
            usage: None,
            done: false,
            chunks: 0,
        }
    }

    /// Folds one SSE event into the aggregate.
    ///
    /// # Errors
    /// Returns [`SseError::MalformedChunk`] when a non-sentinel data payload
    /// is not a JSON object with a `choices` array.
    pub fn fold_event(&mut self, event: &SseEvent) -> Result<(), SseError> {
        if event.data.trim() == DONE_SENTINEL {
            self.done = true;
            return Ok(());
        }
        let value = JsonValue::parse(&event.data).map_err(|error| {
            SseError::MalformedChunk(format!("chunk is not valid JSON: {error}"))
        })?;
        if let Some(usage) = value.get("usage")
            && !usage.is_null()
        {
            self.usage = Some(crate::chat::ChatUsage::from_json(usage));
        }
        let Some(choices) = value.get("choices").and_then(JsonValue::as_array) else {
            return Err(SseError::MalformedChunk(
                "chunk lacks a choices array".to_owned(),
            ));
        };
        for choice in choices {
            if let Some(reason) = choice.get("finish_reason").and_then(JsonValue::as_str) {
                self.finish_reason = Some(reason.to_owned());
            }
            if let Some(delta) = choice.get("delta")
                && let Some(fragment) = delta.get("content").and_then(JsonValue::as_str)
            {
                self.text.push_str(fragment);
            }
        }
        self.chunks = self.chunks.saturating_add(1);
        Ok(())
    }
}

/// Fail-closed SSE violations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SseError {
    /// A single event exceeded [`MAX_SSE_EVENT_BYTES`].
    EventTooLarge(usize),
    /// A data payload was neither the sentinel nor a valid chunk object.
    MalformedChunk(String),
}

impl fmt::Display for SseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventTooLarge(size) => {
                write!(f, "SSE event exceeds {MAX_SSE_EVENT_BYTES} bytes ({size})")
            }
            Self::MalformedChunk(reason) => write!(f, "malformed stream chunk: {reason}"),
        }
    }
}

impl Error for SseError {}

#[cfg(test)]
mod tests {
    use super::{DONE_SENTINEL, SseEvent, SseParser, StreamAggregator};

    #[test]
    fn parses_events_across_chunk_boundaries_with_crlf() {
        let mut parser = SseParser::new();
        let events = parser
            .push(b"data: {\"a\":1}\r\n\r\ndata: [DONE]\n\n")
            .expect("parse stream");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(events[1].data, DONE_SENTINEL);
        assert_eq!(parser.pending_bytes(), 0);

        let mut split_parser = SseParser::new();
        let first_half = split_parser.push(b"data: {\"b\"").expect("first half");
        assert_eq!(first_half.len(), 0);
        let second = split_parser
            .push(b":2}\n\ndata: x\n\n")
            .expect("second half");
        assert_eq!(second.len(), 2);
        assert_eq!(second[0].data, "{\"b\":2}");

        let mut no_data = SseParser::new();
        let comment_only = no_data
            .push(b": keepalive comment\n\n")
            .expect("comment only");
        assert_eq!(comment_only.len(), 0);
    }

    #[test]
    fn aggregator_folds_deltas_usage_and_done() {
        let mut parser = SseParser::new();
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":80,\"completion_tokens\":12,\"total_tokens\":92}}\n\n",
            "data: [DONE]\n\n",
        );
        let events = parser.push(stream.as_bytes()).expect("parse");
        let mut aggregate = StreamAggregator::new();
        for event in &events {
            aggregate.fold_event(event).expect("fold");
        }
        assert_eq!(aggregate.text, "Hello");
        assert_eq!(aggregate.finish_reason.as_deref(), Some("stop"));
        assert_eq!(aggregate.usage.map(|usage| usage.total_tokens), Some(92));
        assert!(aggregate.done);
        assert_eq!(aggregate.chunks, 3);

        let mut strict = StreamAggregator::new();
        let bad = SseEvent {
            data: "not json".to_owned(),
            event: None,
            id: None,
        };
        assert!(strict.fold_event(&bad).is_err());
    }
}
