use std::borrow::Cow;

/// Checked-in seeds use `hex:` so they remain reviewable text. Mutated inputs
/// that lose the marker are exercised as raw wire bytes.
pub fn seed_or_raw(input: &[u8], maximum_raw_bytes: usize) -> Option<Cow<'_, [u8]>> {
    if let Some(encoded) = input.strip_prefix(b"hex:") {
        let encoded = trim_ascii(encoded);
        if encoded.len() > maximum_raw_bytes.saturating_mul(2)
            || !encoded.len().is_multiple_of(2)
            || !encoded.iter().all(u8::is_ascii_hexdigit)
        {
            return None;
        }

        let decoded = encoded
            .chunks_exact(2)
            .map(|pair| (nibble(pair[0]) << 4) | nibble(pair[1]))
            .collect();
        return Some(Cow::Owned(decoded));
    }

    (input.len() <= maximum_raw_bytes).then_some(Cow::Borrowed(input))
}

fn trim_ascii(mut input: &[u8]) -> &[u8] {
    while input.first().is_some_and(u8::is_ascii_whitespace) {
        input = &input[1..];
    }
    while input.last().is_some_and(u8::is_ascii_whitespace) {
        input = &input[..input.len() - 1];
    }
    input
}

fn nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => unreachable!("caller validates hexadecimal input"),
    }
}
