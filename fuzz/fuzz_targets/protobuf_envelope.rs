#![no_main]

mod support;

use libfuzzer_sys::fuzz_target;
use nlos_schema::{MAX_ENVELOPE_BYTES, decode_sabi_envelope};
use support::seed_or_raw;

fuzz_target!(|input: &[u8]| {
    let Some(wire) = seed_or_raw(input, MAX_ENVELOPE_BYTES + 1) else {
        return;
    };

    if let Ok(validated) = decode_sabi_envelope(&wire) {
        assert_eq!(validated.wire_bytes(), wire.as_ref());
        assert_eq!(validated.into_wire_bytes(), wire.as_ref());
    }
});
