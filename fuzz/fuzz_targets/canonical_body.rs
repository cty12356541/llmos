#![no_main]

mod support;

use libfuzzer_sys::fuzz_target;
use nlos_canonical::{MAX_CANONICAL_BYTES, decode, encode};
use support::seed_or_raw;

const GOLDEN_CRITICAL_EXTENSION: u32 = 7;

fuzz_target!(|input: &[u8]| {
    let Some(body) = seed_or_raw(input, MAX_CANONICAL_BYTES + 1) else {
        return;
    };

    for supported in [&[][..], &[GOLDEN_CRITICAL_EXTENSION][..]] {
        if let Ok(decoded) = decode(&body, supported) {
            assert_eq!(
                encode(&decoded).expect("decoded value must encode"),
                body.as_ref()
            );
            assert!(
                decoded
                    .critical_extensions()
                    .iter()
                    .all(|extension| supported.contains(&extension.id()))
            );
        }
    }
});
