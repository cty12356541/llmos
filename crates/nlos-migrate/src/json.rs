//! Deterministic `RFC 8259` JSON formatting helpers with zero external crate
//! dependencies and zero wall-clock inputs.

use std::fmt::Write as _;

/// Escapes an arbitrary UTF-8 string as a quoted `RFC 8259` JSON string literal.
#[must_use]
pub fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ctrl if (ctrl as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", ctrl as u32);
            }
            normal => out.push(normal),
        }
    }
    out.push('"');
    out
}

/// Formats an `Option<&str>` as either a quoted JSON string or `null`.
#[must_use]
pub fn json_opt_string(value: Option<&str>) -> String {
    match value {
        Some(text) => json_string(text),
        None => "null".to_owned(),
    }
}

/// Formats an `Option<u32>` as either a base-10 integer or `null`.
#[must_use]
pub fn json_opt_u32(value: Option<u32>) -> String {
    match value {
        Some(num) => num.to_string(),
        None => "null".to_owned(),
    }
}

/// Formats a slice of `u32` integers as a compact JSON array `[1, 2, 3]`.
#[must_use]
pub fn json_u32_array(values: &[u32]) -> String {
    let items: Vec<String> = values.iter().map(u32::to_string).collect();
    format!("[{}]", items.join(", "))
}

/// Formats a byte slice as lowercase hexadecimal.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Decodes an exact-length hexadecimal string into a fixed byte array.
///
/// # Errors
///
/// Returns a descriptive error string when `raw` does not have length `2 * N`
/// or contains non-hexadecimal characters.
pub fn hex_decode_fixed<const N: usize>(raw: &str, field_name: &str) -> Result<[u8; N], String> {
    let trimmed = raw.trim();
    if trimmed.len() != N * 2 {
        return Err(format!(
            "{field_name} must be {} hex characters, got {}",
            N * 2,
            trimmed.len()
        ));
    }
    let mut out = [0u8; N];
    let bytes = trimmed.as_bytes();
    for (idx, slot) in out.iter_mut().enumerate() {
        let hi = decode_nibble(bytes[idx * 2]).ok_or_else(|| {
            format!(
                "{field_name} contains non-hex character at offset {}",
                idx * 2
            )
        })?;
        let lo = decode_nibble(bytes[idx * 2 + 1]).ok_or_else(|| {
            format!(
                "{field_name} contains non-hex character at offset {}",
                idx * 2 + 1
            )
        })?;
        *slot = (hi << 4) | lo;
    }
    Ok(out)
}

const fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
