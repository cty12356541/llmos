//! W82-B-CONTRACT: preserve recipient identity and enforce both Blob byte bounds.

use nlos_schema::{
    CompatibilityError, blob_reference_schema_identity, decode_mint_reference_request,
    decode_read_reference_result, encode_mint_reference_request, encode_read_reference_result,
    sabi::v1::{MintReferenceRequest, ReadReferenceResult},
};
use prost::Message;

fn read_result(chunk_len: usize, offset: u64) -> ReadReferenceResult {
    ReadReferenceResult {
        schema: Some(blob_reference_schema_identity()),
        chunk: vec![0x5a; chunk_len],
        offset,
        chunk_digest_sha256: vec![0x44; 32],
    }
}

#[test]
fn mint_recipient_field_survives_the_bounded_codec() {
    // Field 8, wire type 2, length 16. A codec predating the additive field
    // drops it on typed re-encode, which must not erase recipient binding.
    let mut wire = MintReferenceRequest {
        schema: Some(blob_reference_schema_identity()),
        ..Default::default()
    }
    .encode_to_vec();
    wire.extend_from_slice(&[0x42, 0x10]);
    wire.extend_from_slice(&[0x22; 16]);
    let decoded = decode_mint_reference_request(&wire).unwrap();
    assert_eq!(decoded.recipient_principal_id, vec![0x22; 16]);
    assert_eq!(encode_mint_reference_request(&decoded).unwrap(), wire);
}

#[test]
fn recipient_golden_round_trips_without_rewriting_the_legacy_mint_vector() {
    let raw = include_str!(
        "../../../schema/golden/epochs/v1.1-blob-20261116/nlos.sabi.BlobReference.MintReferenceRequest-recipient-v1.1.hex"
    )
    .trim_end();
    let wire: Vec<u8> = (0..raw.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&raw[i..i + 2], 16).unwrap())
        .collect();
    let mint = decode_mint_reference_request(&wire).unwrap();
    assert_eq!(mint.recipient_principal_id, vec![0x22; 16]);
    assert_eq!(mint.max_chunk_bytes, 65_408);
    assert_eq!(mint.size_bytes, 70_000);
    assert_eq!(encode_mint_reference_request(&mint).unwrap(), wire);
}

#[test]
fn chunk_over_the_effective_bound_is_rejected_even_when_the_frame_fits() {
    for offset in [0, 127, 128, 16_384, u64::MAX] {
        let accepted = read_result(65_408, offset);
        assert!(encode_read_reference_result(&accepted).unwrap().len() <= 65_486);
        let oversized = read_result(65_409, offset);
        let raw = oversized.encode_to_vec();
        assert!(
            raw.len() < 65_536,
            "this exercises the independent chunk bound"
        );
        assert!(encode_read_reference_result(&oversized).is_err());
        assert!(decode_read_reference_result(&raw).is_err());
    }
}

#[test]
fn full_encoded_limit_includes_identity_extensions_and_offset() {
    let mut accepted = read_result(65_408, u64::MAX);
    accepted.schema.as_mut().unwrap().non_critical_extension_ids = (128..=151).collect();
    assert_eq!(accepted.encoded_len(), 65_536);
    let raw = encode_read_reference_result(&accepted).unwrap();
    assert_eq!(decode_read_reference_result(&raw).unwrap(), accepted);
    accepted
        .schema
        .as_mut()
        .unwrap()
        .non_critical_extension_ids
        .push(49);
    assert_eq!(accepted.encoded_len(), 65_537);
    assert!(matches!(
        encode_read_reference_result(&accepted),
        Err(CompatibilityError::FrameTooLarge {
            actual: 65_537,
            maximum: 65_536
        })
    ));
    assert!(matches!(
        decode_read_reference_result(&accepted.encode_to_vec()),
        Err(CompatibilityError::FrameTooLarge {
            actual: 65_537,
            maximum: 65_536
        })
    ));
}
