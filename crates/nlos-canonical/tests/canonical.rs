use nlos_canonical::{
    CanonicalDigestEnvelope, CanonicalError, CanonicalObjectId, Extension, MAX_CANONICAL_BYTES,
    Sha256Digest, SignatureDomain, decode, decode_signing_preimage_for_domain, encode,
    encode_signing_preimage,
};
use std::fmt::Write;

fn domain() -> SignatureDomain {
    SignatureDomain::new("nlos.conformance.golden/v1").unwrap()
}

fn envelope() -> CanonicalDigestEnvelope {
    CanonicalDigestEnvelope::new(
        CanonicalObjectId::from_bytes(core::array::from_fn(|index| u8::try_from(index).unwrap())),
        Sha256Digest::from_bytes(core::array::from_fn(|index| {
            0xa0 + u8::try_from(index).unwrap()
        })),
        vec![Extension::new(7, b"critical".to_vec()).unwrap()],
        vec![
            Extension::new(42, b"opaque".to_vec()).unwrap(),
            Extension::new(7_001, vec![0x00, 0xff]).unwrap(),
        ],
    )
    .unwrap()
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn replace_once(bytes: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    let offset = bytes
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("test pattern must be unique in canonical fixture");
    assert_eq!(
        bytes
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count(),
        1
    );
    let mut result = Vec::with_capacity(bytes.len() - needle.len() + replacement.len());
    result.extend_from_slice(&bytes[..offset]);
    result.extend_from_slice(replacement);
    result.extend_from_slice(&bytes[offset + needle.len()..]);
    result
}

#[test]
fn encoding_matches_checked_in_golden_and_round_trips() {
    let encoded = encode(&envelope()).unwrap();
    let expected =
        include_str!("../../../schema/golden/nlos.canonical.DigestEnvelope-v1.hex").trim();

    assert_eq!(hex(&encoded), expected);
    assert_eq!(decode(&encoded, &[7]).unwrap(), envelope());

    let preimage = encode_signing_preimage(&domain(), &envelope()).unwrap();
    let expected_preimage =
        include_str!("../../../schema/golden/nlos.canonical.DigestEnvelope-preimage-v1.hex").trim();
    assert_eq!(hex(&preimage), expected_preimage);
    assert_eq!(
        decode_signing_preimage_for_domain(&preimage, &domain(), &[7]).unwrap(),
        envelope()
    );
}

#[test]
fn signing_domain_is_required_and_ascii_restricted() {
    let preimage = encode_signing_preimage(&domain(), &envelope()).unwrap();
    let other = SignatureDomain::new("nlos.other/v1").unwrap();
    assert!(matches!(
        decode_signing_preimage_for_domain(&preimage, &other, &[7]),
        Err(CanonicalError::DomainMismatch { .. })
    ));
    assert!(matches!(
        SignatureDomain::new("nlos.收据/v1"),
        Err(CanonicalError::InvalidDomain)
    ));
}

#[test]
fn critical_extensions_require_support_but_noncritical_round_trip() {
    let encoded = encode(&envelope()).unwrap();
    assert_eq!(
        decode(&encoded, &[]),
        Err(CanonicalError::UnsupportedCriticalExtension(7))
    );

    let decoded = decode(&encoded, &[7]).unwrap();
    assert_eq!(decoded.noncritical_extensions()[0].id(), 42);
    assert_eq!(decoded.noncritical_extensions()[0].value(), b"opaque");
    assert_eq!(encode(&decoded).unwrap(), encoded);
}

#[test]
fn producers_cannot_emit_duplicate_or_unsorted_extension_maps() {
    let duplicate = vec![
        Extension::new(7, Vec::new()).unwrap(),
        Extension::new(7, Vec::new()).unwrap(),
    ];
    assert!(matches!(
        CanonicalDigestEnvelope::new(
            CanonicalObjectId::from_bytes([0; 16]),
            Sha256Digest::from_bytes([0; 32]),
            duplicate,
            Vec::new()
        ),
        Err(CanonicalError::DuplicateExtension { id: 7, .. })
    ));

    let unsorted = vec![
        Extension::new(9, Vec::new()).unwrap(),
        Extension::new(8, Vec::new()).unwrap(),
    ];
    assert!(matches!(
        CanonicalDigestEnvelope::new(
            CanonicalObjectId::from_bytes([0; 16]),
            Sha256Digest::from_bytes([0; 32]),
            unsorted,
            Vec::new()
        ),
        Err(CanonicalError::ExtensionOrder {
            previous: 9,
            current: 8,
            ..
        })
    ));
}

#[test]
fn duplicate_and_out_of_order_top_level_keys_are_rejected() {
    let canonical = encode(&envelope()).unwrap();
    let duplicate = replace_once(
        &canonical,
        &[0x01, 0x01, 0x02, 0x00],
        &[0x01, 0x01, 0x01, 0x00],
    );
    assert_eq!(
        decode(&duplicate, &[7]),
        Err(CanonicalError::DuplicateField(1))
    );

    let out_of_order = replace_once(
        &canonical,
        &[0x01, 0x01, 0x02, 0x00],
        &[0x02, 0x00, 0x01, 0x01],
    );
    assert!(matches!(
        decode(&out_of_order, &[7]),
        Err(CanonicalError::FieldOrder {
            previous: 2,
            current: 1
        })
    ));
}

#[test]
fn duplicate_and_out_of_order_extension_keys_are_rejected() {
    let canonical = encode(&envelope()).unwrap();
    let duplicate = replace_once(&canonical, &[0x19, 0x1b, 0x59], &[0x18, 0x2a]);
    assert_eq!(
        decode(&duplicate, &[7]),
        Err(CanonicalError::DuplicateExtension {
            class: "noncritical",
            id: 42
        })
    );

    let out_of_order = replace_once(&canonical, &[0x19, 0x1b, 0x59], &[0x18, 0x29]);
    assert_eq!(
        decode(&out_of_order, &[7]),
        Err(CanonicalError::ExtensionOrder {
            class: "noncritical",
            previous: 42,
            current: 41
        })
    );
}

#[test]
fn non_shortest_integer_representation_is_rejected() {
    let canonical = encode(&envelope()).unwrap();
    let non_shortest = replace_once(
        &canonical,
        &[0x01, 0x01, 0x02, 0x00],
        &[0x01, 0x18, 0x01, 0x02, 0x00],
    );
    assert_eq!(
        decode(&non_shortest, &[7]),
        Err(CanonicalError::NonCanonicalEncoding)
    );
}

#[test]
fn unknown_major_rejects_but_higher_minor_round_trips() {
    let canonical = encode(&envelope()).unwrap();
    let unknown_major = replace_once(
        &canonical,
        &[0x01, 0x01, 0x02, 0x00],
        &[0x01, 0x02, 0x02, 0x00],
    );
    assert_eq!(
        decode(&unknown_major, &[7]),
        Err(CanonicalError::UnsupportedMajor(2))
    );

    let higher_minor = replace_once(
        &canonical,
        &[0x02, 0x00, 0x03, 0x67],
        &[0x02, 0x18, 0x63, 0x03, 0x67],
    );
    let decoded = decode(&higher_minor, &[7]).unwrap();
    assert_eq!(decoded.schema_minor(), 99);
    assert_eq!(encode(&decoded).unwrap(), higher_minor);
}

#[test]
fn digest_algorithm_cannot_be_substituted() {
    let canonical = encode(&envelope()).unwrap();
    let substituted = replace_once(&canonical, b"sha-256", b"sha-512");
    assert_eq!(
        decode(&substituted, &[7]),
        Err(CanonicalError::UnsupportedDigestAlgorithm(
            "sha-512".to_owned()
        ))
    );
}

#[test]
fn floats_tags_and_indefinite_maps_are_rejected() {
    let canonical = encode(&envelope()).unwrap();
    let float_major = replace_once(
        &canonical,
        &[0x01, 0x01, 0x02, 0x00],
        &[0x01, 0xf9, 0x3c, 0x00, 0x02, 0x00],
    );
    assert!(matches!(
        decode(&float_major, &[7]),
        Err(CanonicalError::Malformed(_))
    ));

    let tagged_major = replace_once(
        &canonical,
        &[0x01, 0x01, 0x02, 0x00],
        &[0x01, 0xc0, 0x01, 0x02, 0x00],
    );
    assert!(matches!(
        decode(&tagged_major, &[7]),
        Err(CanonicalError::Malformed(_))
    ));

    let mut indefinite = canonical;
    indefinite[0] = 0xbf;
    indefinite.push(0xff);
    assert_eq!(
        decode(&indefinite, &[7]),
        Err(CanonicalError::IndefiniteLength)
    );
}

#[test]
fn trailing_and_oversized_input_fail_closed() {
    let mut trailing = encode(&envelope()).unwrap();
    trailing.push(0x00);
    assert_eq!(decode(&trailing, &[7]), Err(CanonicalError::TrailingData));

    let oversized = vec![0_u8; MAX_CANONICAL_BYTES + 1];
    assert!(matches!(
        decode(&oversized, &[7]),
        Err(CanonicalError::FrameTooLarge { .. })
    ));
}

#[test]
fn signing_preimage_lengths_are_authoritative() {
    let mut preimage = encode_signing_preimage(&domain(), &envelope()).unwrap();
    let body_length_offset = 4 + domain().as_str().len();
    let last_length_byte = body_length_offset + 3;
    preimage[last_length_byte] = preimage[last_length_byte].checked_add(1).unwrap();
    assert_eq!(
        decode_signing_preimage_for_domain(&preimage, &domain(), &[7]),
        Err(CanonicalError::TrailingData)
    );
}

#[test]
fn excessive_nesting_is_not_part_of_the_profile() {
    // Definite map containing a map where the first unsigned key is required.
    let nested = [0xa8, 0xa1, 0x00, 0x00];
    assert!(matches!(
        decode(&nested, &[]),
        Err(CanonicalError::Malformed(_))
    ));
}
