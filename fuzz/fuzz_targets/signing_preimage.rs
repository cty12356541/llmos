#![no_main]

mod support;

use libfuzzer_sys::fuzz_target;
use nlos_canonical::{
    MAX_CANONICAL_BYTES, MAX_DOMAIN_BYTES, SignatureDomain, decode_signing_preimage_for_domain,
    encode_signing_preimage,
};
use support::seed_or_raw;

const GOLDEN_CRITICAL_EXTENSION: u32 = 7;

fuzz_target!(|input: &[u8]| {
    let maximum = 8 + MAX_DOMAIN_BYTES + MAX_CANONICAL_BYTES + 1;
    let Some(preimage) = seed_or_raw(input, maximum) else {
        return;
    };
    let expected_domain =
        SignatureDomain::new("nlos.conformance.golden/v1").expect("static domain is valid");

    if let Ok(decoded) = decode_signing_preimage_for_domain(
        &preimage,
        &expected_domain,
        &[GOLDEN_CRITICAL_EXTENSION],
    ) {
        assert_eq!(
            encode_signing_preimage(&expected_domain, &decoded).expect("decoded value must encode"),
            preimage.as_ref()
        );
    }
});
