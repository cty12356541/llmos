//! Zero-dependency bounded RFC 8259 `JSON` parser and serializer.
//!
//! Supports full `UTF-8`, standard escape sequences, `\uXXXX` escapes including
//! `UTF-16` surrogate pairs, exact `u64`/`i64` integer preservation alongside
//! finite `f64` floats, and bounded nesting depth (`MAX_JSON_DEPTH = 64`).

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{self, Write as _};

/// Maximum nesting depth permitted when parsing untrusted `JSON` payloads.
pub const MAX_JSON_DEPTH: usize = 64;

/// Numeric representation inside [`JsonValue::Number`], preserving exact
/// 64-bit integer precision for token accounting (`[BUD-ARITH-001]`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum JsonNumber {
    /// Unsigned 64-bit integer.
    UInt(u64),
    /// Signed 64-bit integer.
    Int(i64),
    /// Finite IEEE-754 double-precision float (`NaN`/`Infinity` forbidden).
    Float(f64),
}

impl JsonNumber {
    /// Constructs a finite floating-point `JSON` number, returning `None` for
    /// `NaN` or `+-Infinity` (which RFC 8259 forbids).
    #[must_use]
    pub const fn from_f64(value: f64) -> Option<Self> {
        if value.is_finite() {
            Some(Self::Float(value))
        } else {
            None
        }
    }

    /// Extracts an exact `u64` when the number is a non-negative integer.
    #[must_use]
    pub fn as_u64(self) -> Option<u64> {
        match self {
            Self::UInt(v) => Some(v),
            Self::Int(v) => u64::try_from(v).ok(),
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            Self::Float(v) => {
                if v.is_finite() && v >= 0.0 && v.fract() == 0.0 && v < (u64::MAX as f64) {
                    Some(v as u64)
                } else {
                    None
                }
            }
        }
    }

    /// Extracts an exact `i64` when the number fits a signed 64-bit integer.
    #[must_use]
    pub fn as_i64(self) -> Option<i64> {
        match self {
            Self::Int(v) => Some(v),
            Self::UInt(v) => i64::try_from(v).ok(),
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
            Self::Float(v) => {
                if v.is_finite()
                    && v.fract() == 0.0
                    && v >= (i64::MIN as f64)
                    && v < (i64::MAX as f64)
                {
                    Some(v as i64)
                } else {
                    None
                }
            }
        }
    }

    /// Converts the number to `f64`.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub const fn as_f64(self) -> f64 {
        match self {
            Self::UInt(v) => v as f64,
            Self::Int(v) => v as f64,
            Self::Float(v) => v,
        }
    }
}

/// RFC 8259 `JSON` value tree.
#[derive(Clone, Debug, PartialEq)]
pub enum JsonValue {
    /// `null`
    Null,
    /// `true` or `false`
    Bool(bool),
    /// Numeric literal (`u64`, `i64`, or finite `f64`).
    Number(JsonNumber),
    /// `UTF-8` string.
    String(String),
    /// Ordered array of values.
    Array(Vec<Self>),
    /// Key-sorted object map.
    Object(BTreeMap<String, Self>),
}

impl JsonValue {
    /// Parses a complete RFC 8259 `JSON` document from `input`.
    ///
    /// # Errors
    /// Fails closed on syntax errors, unescaped control bytes, invalid
    /// `\uXXXX` or lone `UTF-16` surrogates, depth overflow, or trailing bytes.
    pub fn parse(input: &str) -> Result<Self, JsonError> {
        let mut parser = Parser::new(input);
        parser.skip_ws();
        let value = parser.parse_value(0)?;
        parser.skip_ws();
        if !parser.is_eof() {
            return Err(parser.err("unexpected trailing characters after JSON value"));
        }
        Ok(value)
    }

    /// Builds a `JsonValue::Object` from an iterator of `(key, value)` pairs.
    #[must_use]
    pub fn object<I, K>(entries: I) -> Self
    where
        I: IntoIterator<Item = (K, Self)>,
        K: Into<String>,
    {
        let mut map = BTreeMap::new();
        for (k, v) in entries {
            map.insert(k.into(), v);
        }
        Self::Object(map)
    }

    /// Returns `true` if this value is [`JsonValue::Null`].
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Looks up a field by `key` when `self` is a [`JsonValue::Object`].
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(map) => map.get(key),
            _ => None,
        }
    }

    /// Looks up an element by `index` when `self` is a [`JsonValue::Array`].
    #[must_use]
    pub fn get_index(&self, index: usize) -> Option<&Self> {
        match self {
            Self::Array(items) => items.get(index),
            _ => None,
        }
    }

    /// Extracts `&str` when `self` is a [`JsonValue::String`].
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Extracts `u64` when `self` is a non-negative integer [`JsonValue::Number`].
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(n) => n.as_u64(),
            _ => None,
        }
    }

    /// Extracts `i64` when `self` is an integer [`JsonValue::Number`].
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Number(n) => n.as_i64(),
            _ => None,
        }
    }

    /// Extracts `f64` when `self` is a [`JsonValue::Number`].
    #[must_use]
    pub const fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(n.as_f64()),
            _ => None,
        }
    }

    /// Extracts `bool` when `self` is a [`JsonValue::Bool`].
    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Extracts `&Vec<JsonValue>` when `self` is a [`JsonValue::Array`].
    #[must_use]
    pub const fn as_array(&self) -> Option<&Vec<Self>> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Extracts `&BTreeMap<String, JsonValue>` when `self` is a [`JsonValue::Object`].
    #[must_use]
    pub const fn as_object(&self) -> Option<&BTreeMap<String, Self>> {
        match self {
            Self::Object(map) => Some(map),
            _ => None,
        }
    }

    /// Serializes `self` into compact RFC 8259 `JSON`.
    #[must_use]
    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        write_json_compact(self, &mut out);
        out
    }

    /// Serializes `self` into 2-space indented human-readable RFC 8259 `JSON`.
    #[must_use]
    pub fn to_json_pretty(&self) -> String {
        let mut out = String::new();
        write_json_pretty(self, 0, &mut out);
        out
    }
}

impl fmt::Display for JsonValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_json_string())
    }
}

impl From<bool> for JsonValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<u64> for JsonValue {
    fn from(v: u64) -> Self {
        Self::Number(JsonNumber::UInt(v))
    }
}

impl From<u32> for JsonValue {
    fn from(v: u32) -> Self {
        Self::Number(JsonNumber::UInt(u64::from(v)))
    }
}

impl From<usize> for JsonValue {
    fn from(v: usize) -> Self {
        Self::Number(JsonNumber::UInt(v as u64))
    }
}

impl From<i64> for JsonValue {
    fn from(v: i64) -> Self {
        Self::Number(JsonNumber::Int(v))
    }
}

impl From<String> for JsonValue {
    fn from(v: String) -> Self {
        Self::String(v)
    }
}

impl From<&str> for JsonValue {
    fn from(v: &str) -> Self {
        Self::String(v.to_owned())
    }
}

/// Syntax or structural error encountered while parsing RFC 8259 `JSON`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonError {
    pub offset: usize,
    pub message: &'static str,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte offset {}", self.message, self.offset)
    }
}

impl Error for JsonError {}

struct Parser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    const fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
        }
    }

    const fn is_eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    const fn err(&self, message: &'static str) -> JsonError {
        JsonError {
            offset: self.pos,
            message,
        }
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        if depth > MAX_JSON_DEPTH {
            return Err(self.err("JSON nesting depth exceeds MAX_JSON_DEPTH"));
        }
        self.skip_ws();
        let Some(b) = self.peek() else {
            return Err(self.err("unexpected end of JSON input"));
        };
        match b {
            b'n' => self.parse_literal(b"null", JsonValue::Null),
            b't' => self.parse_literal(b"true", JsonValue::Bool(true)),
            b'f' => self.parse_literal(b"false", JsonValue::Bool(false)),
            b'"' => self.parse_string().map(JsonValue::String),
            b'[' => self.parse_array(depth + 1),
            b'{' => self.parse_object(depth + 1),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => Err(self.err("unexpected character in JSON value")),
        }
    }

    fn parse_literal(&mut self, expected: &[u8], out: JsonValue) -> Result<JsonValue, JsonError> {
        let end = self.pos.saturating_add(expected.len());
        if self.bytes.get(self.pos..end) == Some(expected) {
            self.pos = end;
            Ok(out)
        } else {
            Err(self.err("invalid JSON literal"))
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        self.pos += 1; // '['
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

    fn parse_object(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        self.pos += 1; // '{'
        self.skip_ws();
        let mut map = BTreeMap::new();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(map));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected string key in JSON object"));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected ':' after key in JSON object"));
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

    fn parse_string(&mut self) -> Result<String, JsonError> {
        self.pos += 1; // opening '"'
        let mut out = String::new();
        while let Some(b) = self.peek() {
            match b {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let ch = self.parse_escape()?;
                    out.push(ch);
                }
                0x00..=0x1F => {
                    return Err(self.err("unescaped control character in JSON string"));
                }
                _ => {
                    let rest = &self.src[self.pos..];
                    let Some(ch) = rest.chars().next() else {
                        return Err(self.err("unexpected end of UTF-8 JSON string"));
                    };
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        Err(self.err("unterminated JSON string"))
    }

    fn parse_escape(&mut self) -> Result<char, JsonError> {
        let Some(esc) = self.peek() else {
            return Err(self.err("unterminated escape sequence in JSON string"));
        };
        self.pos += 1;
        match esc {
            b'"' => Ok('"'),
            b'\\' => Ok('\\'),
            b'/' => Ok('/'),
            b'b' => Ok('\u{0008}'),
            b'f' => Ok('\u{000C}'),
            b'n' => Ok('\n'),
            b'r' => Ok('\r'),
            b't' => Ok('\t'),
            b'u' => self.parse_unicode_escape(),
            _ => Err(self.err("invalid escape character in JSON string")),
        }
    }

    fn parse_unicode_escape(&mut self) -> Result<char, JsonError> {
        let code_unit = self.parse_hex4()?;
        if (0xD800..=0xDBFF).contains(&code_unit) {
            if self.bytes.get(self.pos..self.pos.saturating_add(2)) != Some(b"\\u") {
                return Err(self.err("unpaired high UTF-16 surrogate in JSON string"));
            }
            self.pos += 2;
            let low = self.parse_hex4()?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(self.err("invalid low UTF-16 surrogate following high surrogate"));
            }
            let scalar =
                0x1_0000_u32 + ((u32::from(code_unit - 0xD800)) << 10) + u32::from(low - 0xDC00);
            char::from_u32(scalar).ok_or_else(|| self.err("invalid Unicode scalar value"))
        } else if (0xDC00..=0xDFFF).contains(&code_unit) {
            Err(self.err("unpaired low UTF-16 surrogate in JSON string"))
        } else {
            char::from_u32(u32::from(code_unit))
                .ok_or_else(|| self.err("invalid Unicode code point"))
        }
    }

    fn parse_hex4(&mut self) -> Result<u16, JsonError> {
        let end = self.pos.saturating_add(4);
        let Some(slice) = self.bytes.get(self.pos..end) else {
            return Err(self.err("incomplete \\uXXXX escape sequence"));
        };
        let mut acc = 0_u16;
        for &byte in slice {
            let digit = match byte {
                b'0'..=b'9' => u16::from(byte - b'0'),
                b'a'..=b'f' => u16::from(byte - b'a' + 10),
                b'A'..=b'F' => u16::from(byte - b'A' + 10),
                _ => return Err(self.err("non-hex digit in \\uXXXX escape sequence")),
            };
            acc = (acc << 4) | digit;
        }
        self.pos = end;
        Ok(acc)
    }

    fn parse_number(&mut self) -> Result<JsonValue, JsonError> {
        let start = self.pos;
        let is_negative = if self.peek() == Some(b'-') {
            self.pos += 1;
            true
        } else {
            false
        };

        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.err("leading zeros are forbidden in JSON numbers"));
                }
            }
            Some(b'1'..=b'9') => {
                self.pos += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.err("expected digit in JSON number")),
        }

        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected digit after decimal point in JSON number"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }

        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected digit in JSON number exponent"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }

        let token = &self.src[start..self.pos];
        if !is_float {
            if !is_negative && let Ok(u) = token.parse::<u64>() {
                return Ok(JsonValue::Number(JsonNumber::UInt(u)));
            }
            if let Ok(i) = token.parse::<i64>() {
                return Ok(JsonValue::Number(JsonNumber::Int(i)));
            }
        }
        let parsed = token
            .parse::<f64>()
            .map_err(|_| self.err("invalid floating-point JSON number"))?;
        let num = JsonNumber::from_f64(parsed)
            .ok_or_else(|| self.err("JSON number overflowed to non-finite float"))?;
        Ok(JsonValue::Number(num))
    }
}

/// Escapes and writes a `UTF-8` string as an RFC 8259 quoted `JSON` string.
pub fn write_escaped_json_str(value: &str, out: &mut String) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_json_number(num: JsonNumber, out: &mut String) {
    match num {
        JsonNumber::UInt(v) => {
            let _ = write!(out, "{v}");
        }
        JsonNumber::Int(v) => {
            let _ = write!(out, "{v}");
        }
        JsonNumber::Float(v) => {
            if v.is_finite() {
                if v == 0.0 {
                    out.push_str("0.0");
                } else {
                    let _ = write!(out, "{v}");
                }
            } else {
                out.push_str("null");
            }
        }
    }
}

fn write_json_compact(value: &JsonValue, out: &mut String) {
    match value {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        JsonValue::Number(n) => write_json_number(*n, out),
        JsonValue::String(s) => write_escaped_json_str(s, out),
        JsonValue::Array(items) => {
            out.push('[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                write_json_compact(item, out);
            }
            out.push(']');
        }
        JsonValue::Object(map) => {
            out.push('{');
            for (idx, (k, v)) in map.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                write_escaped_json_str(k, out);
                out.push(':');
                write_json_compact(v, out);
            }
            out.push('}');
        }
    }
}

fn write_json_pretty(value: &JsonValue, indent: usize, out: &mut String) {
    let pad = "  ".repeat(indent);
    let inner_pad = "  ".repeat(indent + 1);
    match value {
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {
            write_json_compact(value, out);
        }
        JsonValue::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&inner_pad);
                write_json_pretty(item, indent + 1, out);
            }
            out.push('\n');
            out.push_str(&pad);
            out.push(']');
        }
        JsonValue::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (idx, (k, v)) in map.iter().enumerate() {
                if idx > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&inner_pad);
                write_escaped_json_str(k, out);
                out.push_str(": ");
                write_json_pretty(v, indent + 1, out);
            }
            out.push('\n');
            out.push_str(&pad);
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{JsonNumber, JsonValue};

    #[test]
    fn round_trips_utf8_escapes_surrogate_pairs_and_numbers() {
        let raw = r#"{
            "null_v": null,
            "bool_v": true,
            "u64_v": 9007199254740993,
            "neg_v": -42,
            "float_v": 0.25,
            "cjk": "自然语言操作系统\n第二行\t\"引号\"",
            "surrogate_g_clef": "\uD834\uDD1E"
        }"#;
        let parsed = JsonValue::parse(raw).expect("valid RFC 8259 JSON");
        assert!(parsed.get("null_v").expect("null_v").is_null());
        assert_eq!(
            parsed.get("bool_v").and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            parsed.get("u64_v").and_then(JsonValue::as_u64),
            Some(9_007_199_254_740_993)
        );
        assert_eq!(parsed.get("neg_v").and_then(JsonValue::as_i64), Some(-42));
        assert_eq!(
            parsed.get("float_v").and_then(JsonValue::as_f64),
            Some(0.25)
        );
        assert_eq!(
            parsed.get("surrogate_g_clef").and_then(JsonValue::as_str),
            Some("\u{1D11E}")
        );

        let serialized = parsed.to_json_string();
        let reparsed = JsonValue::parse(&serialized).expect("reparse compact JSON");
        assert_eq!(parsed, reparsed);
        assert!(JsonNumber::from_f64(f64::NAN).is_none());
    }

    #[test]
    fn rejects_lone_surrogates_control_chars_and_leading_zeros() {
        assert!(JsonValue::parse(r#""\uD800""#).is_err());
        assert!(JsonValue::parse(r#""\uDC00""#).is_err());
        assert!(JsonValue::parse("\"raw\nnewline\"").is_err());
        assert!(JsonValue::parse("01").is_err());
        assert!(JsonValue::parse(r#"{"a":1} trailing"#).is_err());
    }
}
