//! Zero-dependency deterministic RFC 8259 JSON and hex encoding utilities.

use std::fmt::Write as _;

/// Escapes an arbitrary UTF-8 string into an RFC 8259 JSON string literal
/// (including the surrounding double quotes).
#[must_use]
pub fn json_quote(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    out.push('"');
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ctrl if (ctrl as u32) <= 0x1F => {
                let _ = write!(out, "\\u{:04x}", ctrl as u32);
            }
            normal => out.push(normal),
        }
    }
    out.push('"');
    out
}

/// Formats an optional string as either an RFC 8259 JSON string literal or `null`.
#[must_use]
pub fn json_opt_str(value: Option<&str>) -> String {
    match value {
        Some(text) => json_quote(text),
        None => "null".to_owned(),
    }
}

/// Formats an optional `u32` as either a JSON number or `null`.
#[must_use]
pub fn json_opt_u32(value: Option<u32>) -> String {
    match value {
        Some(number) => number.to_string(),
        None => "null".to_owned(),
    }
}

/// Formats an optional `u64` as either a JSON number or `null`.
#[must_use]
pub fn json_opt_u64(value: Option<u64>) -> String {
    match value {
        Some(number) => number.to_string(),
        None => "null".to_owned(),
    }
}

/// Encodes a byte slice as lowercase hexadecimal.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Decodes an exact-length hexadecimal string into `[u8; N]`.
#[must_use]
pub fn hex_decode_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut array = [0_u8; N];
    for (index, slot) in array.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(array)
}

#[cfg(test)]
mod tests {
    use super::{hex_decode_array, hex_encode, json_opt_str, json_opt_u32, json_quote};

    #[test]
    fn json_quote_escapes_control_and_special_characters() {
        let raw = "quote\"slash\\tab\tnewline\ncr\rctrl\u{0001}end";
        assert_eq!(
            json_quote(raw),
            "\"quote\\\"slash\\\\tab\\tnewline\\ncr\\rctrl\\u0001end\""
        );
        assert_eq!(json_opt_str(None), "null");
        assert_eq!(json_opt_str(Some("ok")), "\"ok\"");
        assert_eq!(json_opt_u32(None), "null");
        assert_eq!(json_opt_u32(Some(7)), "7");
    }

    #[test]
    fn hex_roundtrip_preserves_exact_bytes() {
        let sample = [0x4e, 0x10, 0xab, 0xff];
        let encoded = hex_encode(&sample);
        assert_eq!(encoded, "4e10abff");
        assert_eq!(hex_decode_array::<4>(&encoded), Some(sample));
        assert_eq!(hex_decode_array::<4>("4e10ab"), None);
        assert_eq!(hex_decode_array::<4>("4e10abzz"), None);
    }
}
