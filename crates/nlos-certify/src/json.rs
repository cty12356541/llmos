//! Pure-Rust RFC 8259 JSON escaping, quoting, and recursive value parsing.
//!
//! `nlos-certify` avoids pulling external serialization dependencies into the
//! certification TCB. This module provides deterministic string escaping for
//! canonical JSON output and a total, fail-closed RFC 8259 parser for auditing
//! `package.json`, `package-lock.json`, JSON payload configurations, and
//! [`crate::StoreCertificationReceipt`] envelopes.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{self, Write as _};

/// Parsed RFC 8259 JSON value with deterministic [`BTreeMap`] object ordering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JsonValue {
    /// JSON `null`.
    Null,
    /// JSON `true` or `false`.
    Bool(bool),
    /// Exact lexical representation of a JSON number.
    Number(String),
    /// Unescaped UTF-8 JSON string.
    String(String),
    /// Ordered JSON array.
    Array(Vec<Self>),
    /// Key-sorted JSON object.
    Object(BTreeMap<String, Self>),
}

impl JsonValue {
    /// Returns the inner string slice if this value is [`Self::String`].
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Returns the inner boolean if this value is [`Self::Bool`].
    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Parses this value as an unsigned 64-bit integer if it is an integer [`Self::Number`].
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(raw) => raw.parse::<u64>().ok(),
            _ => None,
        }
    }

    /// Parses this value as an unsigned 32-bit integer if it is an integer [`Self::Number`].
    #[must_use]
    pub fn as_u32(&self) -> Option<u32> {
        match self {
            Self::Number(raw) => raw.parse::<u32>().ok(),
            _ => None,
        }
    }

    /// Parses this value as `usize` if it is a non-negative integer [`Self::Number`].
    #[must_use]
    pub fn as_usize(&self) -> Option<usize> {
        match self {
            Self::Number(raw) => raw.parse::<usize>().ok(),
            _ => None,
        }
    }

    /// Returns the inner slice if this value is [`Self::Array`].
    #[must_use]
    pub fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    /// Returns the inner map if this value is [`Self::Object`].
    #[must_use]
    pub const fn as_object(&self) -> Option<&BTreeMap<String, Self>> {
        match self {
            Self::Object(map) => Some(map),
            _ => None,
        }
    }
}

/// Fail-closed JSON syntax error with byte offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonParseError {
    /// Byte offset where parsing failed.
    pub offset: usize,
    /// Human-readable diagnostic reason.
    pub message: String,
}

impl fmt::Display for JsonParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "JSON parse error at byte {}: {}",
            self.offset, self.message
        )
    }
}

impl Error for JsonParseError {}

/// Escapes `input` for safe inclusion inside a JSON string literal (RFC 8259 §7).
#[must_use]
pub fn escape_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Wraps `input` in double quotes after RFC 8259 escaping.
#[must_use]
pub fn quote_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    out.push('"');
    out.push_str(&escape_json(input));
    out.push('"');
    out
}

/// Encodes arbitrary bytes as lowercase hexadecimal.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Decodes an exact-length hexadecimal string into a fixed-size byte array.
///
/// # Errors
///
/// Returns an error description if `hex_str` does not have length `N * 2` or
/// contains non-hexadecimal ASCII characters.
pub fn hex_decode_array<const N: usize>(hex_str: &str) -> Result<[u8; N], String> {
    if hex_str.len() != N * 2 {
        return Err(format!(
            "expected {} hex chars, got {}",
            N * 2,
            hex_str.len()
        ));
    }
    let mut out = [0_u8; N];
    let raw = hex_str.as_bytes();
    for (idx, slot) in out.iter_mut().enumerate() {
        let hi = decode_hex_nibble(raw[idx * 2])
            .ok_or_else(|| format!("invalid hex byte at offset {}", idx * 2))?;
        let lo = decode_hex_nibble(raw[idx * 2 + 1])
            .ok_or_else(|| format!("invalid hex byte at offset {}", idx * 2 + 1))?;
        *slot = (hi << 4) | lo;
    }
    Ok(out)
}

const fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Parses an RFC 8259 JSON document into a [`JsonValue`].
///
/// # Errors
///
/// Returns [`JsonParseError`] if the document is malformed, exceeds the maximum
/// nesting depth (`128`), or contains trailing non-whitespace tokens.
pub fn parse_json(input: &str) -> Result<JsonValue, JsonParseError> {
    let stripped = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut parser = Parser {
        src: stripped,
        bytes: stripped.as_bytes(),
        pos: 0,
    };
    parser.skip_ws();
    let value = parser.parse_value(0)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(parser.err("unexpected trailing token after top-level JSON value"));
    }
    Ok(value)
}

const MAX_JSON_DEPTH: usize = 128;

struct Parser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn err(&self, msg: impl Into<String>) -> JsonParseError {
        JsonParseError {
            offset: self.pos,
            message: msg.into(),
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b' ' | b'\t' | b'\r' | b'\n' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn parse_value(&mut self, depth: usize) -> Result<JsonValue, JsonParseError> {
        if depth > MAX_JSON_DEPTH {
            return Err(self.err("JSON nesting depth exceeds 128"));
        }
        self.skip_ws();
        let Some(ch) = self.peek() else {
            return Err(self.err("unexpected end of JSON input"));
        };
        match ch {
            b'n' => self.parse_literal(b"null", JsonValue::Null),
            b't' => self.parse_literal(b"true", JsonValue::Bool(true)),
            b'f' => self.parse_literal(b"false", JsonValue::Bool(false)),
            b'"' => self.parse_string().map(JsonValue::String),
            b'[' => self.parse_array(depth + 1),
            b'{' => self.parse_object(depth + 1),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => Err(self.err(format!("unexpected byte 0x{ch:02x}"))),
        }
    }

    fn parse_literal(
        &mut self,
        expected: &[u8],
        output: JsonValue,
    ) -> Result<JsonValue, JsonParseError> {
        if self.bytes[self.pos..].starts_with(expected) {
            self.pos += expected.len();
            Ok(output)
        } else {
            Err(self.err("invalid JSON literal"))
        }
    }

    fn parse_string(&mut self) -> Result<String, JsonParseError> {
        if self.peek() != Some(b'"') {
            return Err(self.err("expected '\"' to start JSON string"));
        }
        self.pos += 1;
        let mut out = String::new();
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'"' {
                self.pos += 1;
                return Ok(out);
            }
            if b == b'\\' {
                self.pos += 1;
                if self.pos >= self.bytes.len() {
                    return Err(self.err("unterminated escape sequence in JSON string"));
                }
                let esc = self.bytes[self.pos];
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\x08'),
                    b'f' => out.push('\x0c'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let code_unit = self.parse_hex_u16()?;
                        if (0xD800..=0xDBFF).contains(&code_unit) {
                            if !self.bytes[self.pos..].starts_with(b"\\u") {
                                return Err(self.err("unpaired high UTF-16 surrogate"));
                            }
                            self.pos += 2;
                            let low = self.parse_hex_u16()?;
                            if !(0xDC00..=0xDFFF).contains(&low) {
                                return Err(self.err("invalid low UTF-16 surrogate"));
                            }
                            let combined = 0x10000_u32
                                + ((u32::from(code_unit) - 0xD800) << 10)
                                + (u32::from(low) - 0xDC00);
                            let ch = char::from_u32(combined)
                                .ok_or_else(|| self.err("invalid decoded scalar value"))?;
                            out.push(ch);
                        } else if (0xDC00..=0xDFFF).contains(&code_unit) {
                            return Err(self.err("unpaired low UTF-16 surrogate"));
                        } else {
                            let ch = char::from_u32(u32::from(code_unit))
                                .ok_or_else(|| self.err("invalid BMP scalar"))?;
                            out.push(ch);
                        }
                    }
                    _ => return Err(self.err("unknown JSON escape sequence")),
                }
            } else if b < 0x20 {
                return Err(self.err("unescaped ASCII control character in JSON string"));
            } else {
                let tail = &self.src[self.pos..];
                let Some(ch) = tail.chars().next() else {
                    return Err(self.err("unexpected end of UTF-8 stream"));
                };
                out.push(ch);
                self.pos += ch.len_utf8();
            }
        }
        Err(self.err("unterminated JSON string literal"))
    }

    fn parse_hex_u16(&mut self) -> Result<u16, JsonParseError> {
        if self.pos + 4 > self.bytes.len() {
            return Err(self.err("truncated \\uXXXX escape"));
        }
        let mut val = 0_u16;
        for _ in 0..4 {
            let b = self.bytes[self.pos];
            self.pos += 1;
            let nibble =
                decode_hex_nibble(b).ok_or_else(|| self.err("invalid hex digit in \\uXXXX"))?;
            val = (val << 4) | u16::from(nibble);
        }
        Ok(val)
    }

    fn parse_number(&mut self) -> Result<JsonValue, JsonParseError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
            }
            Some(b'1'..=b'9') => {
                self.pos += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.err("invalid JSON number")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected digit after decimal point"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected digit in JSON exponent"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        Ok(JsonValue::Number(self.src[start..self.pos].to_owned()))
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonValue, JsonParseError> {
        self.pos += 1;
        self.skip_ws();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            items.push(self.parse_value(depth)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(JsonValue::Array(items));
                }
                _ => return Err(self.err("expected ',' or ']' in JSON array")),
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsonValue, JsonParseError> {
        self.pos += 1;
        self.skip_ws();
        let mut map = BTreeMap::new();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(map));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected ':' after JSON object key"));
            }
            self.pos += 1;
            let value = self.parse_value(depth)?;
            map.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(JsonValue::Object(map));
                }
                _ => return Err(self.err("expected ',' or '}' in JSON object")),
            }
        }
    }
}
