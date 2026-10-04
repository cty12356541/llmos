//! Owner-issued durability receipt boundary.
//!
//! Production issuance is the only in-crate writer of `durability_receipts`:
//! every minted row records the store identity triple that signed it, derives
//! its `receipt_id` deterministically from the signed core digest, and is
//! verified against the `IdentityAuthority` before the transaction commits.
//! Rows without a store identity triple (pre-v7 legacy or direct-injected
//! writes) carry no verifiable provenance and fail verification closed.
//!
//! Since schema v9, production issuance signs the `nlos-canonical`
//! `DigestEnvelope` signing preimage (domain
//! [`DURABILITY_RECEIPT_PREIMAGE_DOMAIN`], `object_id` = receipt id,
//! `payload_digest` = signed core digest): the preimage is encoded, gated
//! through the strict canonical decoder, and only its SHA-256 is handed to
//! the [`StoreSigner`]. Pre-v9 rows keep the legacy domain-message digest
//! ([`durability_receipt_signature_message`]) verifiable bit-for-bit; the
//! per-row shape marker selects the verification path.

use nlos_canonical::{
    CanonicalDigestEnvelope, CanonicalError, CanonicalObjectId, Sha256Digest, SignatureDomain,
    decode_signing_preimage_for_domain, encode_signing_preimage,
};
use nlos_identity::{IdentityAuthority, VerifySemanticAuthoritySignatureRequest};
use nlos_types::{ControlDomainId, KeyId, PrincipalId, ReceiptId, SemanticEventId};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::{
    DurabilityDecision, DurabilityReceipt, IssueDurabilityReceiptRequest, SemanticAuthorityError,
    StoreSigner, decode_u64, encode_u64,
};

/// Signature domain of the canonical durability-receipt preimage. Charset is
/// pinned by `nlos-canonical`'s [`SignatureDomain`] rules.
pub const DURABILITY_RECEIPT_PREIMAGE_DOMAIN: &str = "nlos.durability-receipt/v1";

/// Pre-v9 signed-message shape: the legacy domain-message digest.
pub const SIGNATURE_PREIMAGE_LEGACY: u8 = 0;

/// v9 signed-message shape: SHA-256 over the canonical `DigestEnvelope`
/// signing preimage.
pub const SIGNATURE_PREIMAGE_CANONICAL: u8 = 1;

pub(crate) fn issue_durability_receipt(
    connection: &mut Connection,
    identity: &IdentityAuthority,
    store_signer: &impl StoreSigner,
    request: &IssueDurabilityReceiptRequest,
) -> Result<DurabilityDecision, SemanticAuthorityError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let event = crate::load_event_record(&transaction, request.event_id)?
        .ok_or(SemanticAuthorityError::EventNotFound(request.event_id))?;
    let admission = crate::load_receipt(&transaction, request.event_id)?;
    if admission.receipt_id != request.admission_receipt_id
        || admission.event_id != request.event_id
        || admission.log_seq != event.log_seq
    {
        return Err(SemanticAuthorityError::DurabilityAdmissionBindingMismatch);
    }
    if request.durable_at_ms < admission.admitted_at_ms {
        return Err(SemanticAuthorityError::DurabilityBeforeAdmission);
    }

    let receipt_core_digest = build_durability_receipt_core_digest(
        request.event_id,
        admission.log_seq,
        request.durable_checkpoint_id,
        request.durable_at_ms,
        store_signer.principal_id(),
        store_signer.control_domain_id(),
        store_signer.key_id(),
    );
    let mut receipt_id_bytes = [0_u8; 16];
    receipt_id_bytes.copy_from_slice(&receipt_core_digest[..16]);
    let receipt_id = ReceiptId::from_bytes(receipt_id_bytes);
    if let Some(existing) =
        load_durability_receipt_optional(&transaction, request.event_id, receipt_id)?
    {
        // The derived id covers every signed field, so a stored row under the
        // same id that disagrees with the request is an injected corruption.
        if existing.durable_checkpoint_id == request.durable_checkpoint_id
            && existing.durable_at_ms == request.durable_at_ms
            && existing.store_principal == Some(store_signer.principal_id())
            && existing.store_control_domain == Some(store_signer.control_domain_id())
            && existing.store_key_id == Some(store_signer.key_id())
        {
            transaction.commit()?;
            return Ok(DurabilityDecision::Replayed(existing));
        }
        return Err(SemanticAuthorityError::CorruptRecord(
            "durability receipt replay binding",
        ));
    }

    let receipt_message = durability_receipt_signature_digest(
        SIGNATURE_PREIMAGE_CANONICAL,
        receipt_id,
        receipt_core_digest,
    )?;
    let store_signature = store_signer
        .sign(&receipt_message)
        .map_err(|error| SemanticAuthorityError::StoreSigningFailed(error.message().to_owned()))?;
    let verified_store =
        identity.verify_semantic_authority_signature(VerifySemanticAuthoritySignatureRequest {
            message_digest: receipt_message,
            issuer: store_signer.principal_id(),
            control_domain_id: store_signer.control_domain_id(),
            key_id: store_signer.key_id(),
            signature: store_signature,
            verified_at_ms: request.durable_at_ms,
        })?;
    if verified_store.principal_id() != store_signer.principal_id()
        || verified_store.control_domain_id() != store_signer.control_domain_id()
        || verified_store.key_id() != store_signer.key_id()
    {
        return Err(SemanticAuthorityError::StoreSignerBindingMismatch);
    }
    let receipt = DurabilityReceipt {
        receipt_id,
        event_id: request.event_id,
        durable_checkpoint_id: request.durable_checkpoint_id,
        durable_at_ms: request.durable_at_ms,
        store_principal: Some(store_signer.principal_id()),
        store_control_domain: Some(store_signer.control_domain_id()),
        store_key_id: Some(store_signer.key_id()),
        store_signature,
        signature_preimage_version: SIGNATURE_PREIMAGE_CANONICAL,
    };
    insert_durability_receipt(&transaction, &receipt)?;
    transaction.commit()?;
    Ok(DurabilityDecision::Issued(receipt))
}

pub(crate) fn verify_durability_receipt(
    connection: &Connection,
    identity: &IdentityAuthority,
    event_id: SemanticEventId,
    receipt_id: ReceiptId,
) -> Result<DurabilityReceipt, SemanticAuthorityError> {
    let receipt = load_durability_receipt_optional(connection, event_id, receipt_id)?.ok_or(
        SemanticAuthorityError::DurabilityReceiptNotFound(receipt_id),
    )?;
    let admission = crate::load_receipt(connection, event_id)?;
    // A row without a complete store identity triple — a pre-v7 legacy row or
    // a direct injection — has no verifiable provenance.
    let (Some(store_principal), Some(store_control_domain), Some(store_key_id)) = (
        receipt.store_principal,
        receipt.store_control_domain,
        receipt.store_key_id,
    ) else {
        return Err(SemanticAuthorityError::DurabilityReceiptUnverifiable);
    };
    let receipt_core_digest = build_durability_receipt_core_digest(
        receipt.event_id,
        admission.log_seq,
        receipt.durable_checkpoint_id,
        receipt.durable_at_ms,
        store_principal,
        store_control_domain,
        store_key_id,
    );
    let mut expected_id_bytes = [0_u8; 16];
    expected_id_bytes.copy_from_slice(&receipt_core_digest[..16]);
    if receipt.receipt_id.as_bytes() != &expected_id_bytes {
        return Err(SemanticAuthorityError::DurabilityReceiptUnverifiable);
    }
    let receipt_message = durability_receipt_signature_digest(
        receipt.signature_preimage_version,
        receipt.receipt_id,
        receipt_core_digest,
    )?;
    let verified_store =
        identity.verify_semantic_authority_signature(VerifySemanticAuthoritySignatureRequest {
            message_digest: receipt_message,
            issuer: store_principal,
            control_domain_id: store_control_domain,
            key_id: store_key_id,
            signature: receipt.store_signature,
            verified_at_ms: receipt.durable_at_ms,
        })?;
    if verified_store.principal_id() != store_principal
        || verified_store.control_domain_id() != store_control_domain
        || verified_store.key_id() != store_key_id
    {
        return Err(SemanticAuthorityError::DurabilityReceiptUnverifiable);
    }
    Ok(receipt)
}

pub(crate) fn load_durability_receipt_optional(
    connection: &Connection,
    event_id: SemanticEventId,
    receipt_id: ReceiptId,
) -> Result<Option<DurabilityReceipt>, SemanticAuthorityError> {
    connection
        .query_row(
            "SELECT event_id, durable_checkpoint_id, durable_at_ms,
                    store_principal_id, store_control_domain_id, store_key_id, store_signature,
                    signature_preimage_version
             FROM durability_receipts WHERE receipt_id=?1 AND event_id=?2",
            params![
                receipt_id.as_bytes().as_slice(),
                event_id.as_bytes().as_slice()
            ],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()?
        .map(|row| {
            Ok(DurabilityReceipt {
                receipt_id,
                event_id: SemanticEventId::from_bytes(
                    row.0.try_into().map_err(|_| {
                        SemanticAuthorityError::CorruptRecord("durability event id")
                    })?,
                ),
                durable_checkpoint_id: row
                    .1
                    .try_into()
                    .map_err(|_| SemanticAuthorityError::CorruptRecord("durable checkpoint id"))?,
                durable_at_ms: decode_u64(row.2)?,
                store_principal: row
                    .3
                    .map(|bytes| {
                        Ok::<PrincipalId, SemanticAuthorityError>(PrincipalId::from_bytes(
                            bytes.try_into().map_err(|_| {
                                SemanticAuthorityError::CorruptRecord("durability store principal")
                            })?,
                        ))
                    })
                    .transpose()?,
                store_control_domain: row
                    .4
                    .map(|bytes| {
                        Ok::<ControlDomainId, SemanticAuthorityError>(ControlDomainId::from_bytes(
                            bytes.try_into().map_err(|_| {
                                SemanticAuthorityError::CorruptRecord("durability store domain")
                            })?,
                        ))
                    })
                    .transpose()?,
                store_key_id: row
                    .5
                    .map(|bytes| {
                        Ok::<KeyId, SemanticAuthorityError>(KeyId::from_bytes(
                            bytes.try_into().map_err(|_| {
                                SemanticAuthorityError::CorruptRecord("durability store key")
                            })?,
                        ))
                    })
                    .transpose()?,
                store_signature: row.6.try_into().map_err(|_| {
                    SemanticAuthorityError::CorruptRecord("durability store signature")
                })?,
                signature_preimage_version: u8::try_from(row.7).map_err(|_| {
                    SemanticAuthorityError::CorruptRecord(
                        "durability receipt signature preimage version",
                    )
                })?,
            })
        })
        .transpose()
}

fn insert_durability_receipt(
    transaction: &Transaction<'_>,
    receipt: &DurabilityReceipt,
) -> Result<(), SemanticAuthorityError> {
    transaction.execute(
        "INSERT INTO durability_receipts (
            receipt_id, event_id, durable_checkpoint_id, durable_at_ms,
            store_principal_id, store_control_domain_id, store_key_id, store_signature,
            signature_preimage_version
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            receipt.receipt_id.as_bytes().as_slice(),
            receipt.event_id.as_bytes().as_slice(),
            receipt.durable_checkpoint_id.as_slice(),
            encode_u64(receipt.durable_at_ms)?,
            receipt.store_principal.map(|id| id.as_bytes().to_vec()),
            receipt
                .store_control_domain
                .map(|id| id.as_bytes().to_vec()),
            receipt.store_key_id.map(|id| id.as_bytes().to_vec()),
            receipt.store_signature.as_slice(),
            i64::from(receipt.signature_preimage_version),
        ],
    )?;
    Ok(())
}

/// Recomputes the signed `DurabilityReceipt` core digest: the admission
/// binding facts (event, log sequence), the durability observation
/// (checkpoint, timestamp), and the store identity triple that minted the
/// receipt.
#[allow(clippy::too_many_arguments)] // Fixed signed Receipt core field order.
#[must_use]
pub fn build_durability_receipt_core_digest(
    event_id: SemanticEventId,
    log_seq: u64,
    durable_checkpoint_id: [u8; 32],
    durable_at_ms: u64,
    store_principal: PrincipalId,
    store_control_domain: ControlDomainId,
    store_key_id: KeyId,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/durability-receipt/v1");
    hasher.update(event_id.as_bytes());
    hasher.update(log_seq.to_be_bytes());
    hasher.update(durable_checkpoint_id);
    hasher.update(durable_at_ms.to_be_bytes());
    hasher.update([2]);
    hasher.update(store_principal.as_bytes());
    hasher.update(store_control_domain.as_bytes());
    hasher.update(store_key_id.as_bytes());
    hasher.finalize().into()
}

/// Computes the digest actually signed by the Semantic store authority over
/// a durability receipt.
#[must_use]
pub fn durability_receipt_signature_message(
    receipt_id: ReceiptId,
    receipt_core_digest: [u8; 32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/durability-receipt-signature/v1");
    hasher.update(receipt_id.as_bytes());
    hasher.update(receipt_core_digest);
    hasher.finalize().into()
}

/// Encodes the `nlos-canonical` signing preimage for one durability receipt:
/// a `CanonicalDigestEnvelope` binding the receipt id (`object_id`) to the
/// signed core digest (`payload_digest`) under
/// [`DURABILITY_RECEIPT_PREIMAGE_DOMAIN`]. No extensions are carried: the
/// core digest already covers every signed receipt field.
///
/// # Errors
///
/// Returns [`SemanticAuthorityError::CanonicalEncoding`] if the domain or
/// envelope is rejected, or the preimage exceeds the canonical byte bound.
pub fn durability_receipt_canonical_preimage(
    receipt_id: ReceiptId,
    receipt_core_digest: [u8; 32],
) -> Result<Vec<u8>, SemanticAuthorityError> {
    let domain = signature_domain()?;
    let envelope = CanonicalDigestEnvelope::new(
        CanonicalObjectId::from_bytes(receipt_id.into_bytes()),
        Sha256Digest::from_bytes(receipt_core_digest),
        Vec::new(),
        Vec::new(),
    )
    .map_err(canonical_encoding_error)?;
    encode_signing_preimage(&domain, &envelope).map_err(canonical_encoding_error)
}

fn signature_domain() -> Result<SignatureDomain, SemanticAuthorityError> {
    SignatureDomain::new(DURABILITY_RECEIPT_PREIMAGE_DOMAIN).map_err(canonical_encoding_error)
}

#[allow(clippy::needless_pass_by_value)] // `map_err` supplies the owned canonical error.
fn canonical_encoding_error(error: CanonicalError) -> SemanticAuthorityError {
    SemanticAuthorityError::CanonicalEncoding(error.to_string())
}

#[allow(clippy::needless_pass_by_value)] // `map_err` supplies the owned canonical error.
fn canonical_decoding_error(error: CanonicalError) -> SemanticAuthorityError {
    SemanticAuthorityError::CanonicalDecoding(error.to_string())
}

/// Computes the digest actually signed over one receipt for its recorded
/// shape, and — for the canonical shape — proves the produced preimage is
/// strictly canonical by decoding it back through `nlos-canonical` (whose
/// decoder rejects any non-deterministic re-encoding) before it is signed or
/// authenticated.
fn durability_receipt_signature_digest(
    signature_preimage_version: u8,
    receipt_id: ReceiptId,
    receipt_core_digest: [u8; 32],
) -> Result<[u8; 32], SemanticAuthorityError> {
    match signature_preimage_version {
        SIGNATURE_PREIMAGE_LEGACY => Ok(durability_receipt_signature_message(
            receipt_id,
            receipt_core_digest,
        )),
        SIGNATURE_PREIMAGE_CANONICAL => {
            let preimage = durability_receipt_canonical_preimage(receipt_id, receipt_core_digest)?;
            let decoded = decode_signing_preimage_for_domain(&preimage, &signature_domain()?, &[])
                .map_err(canonical_decoding_error)?;
            if decoded.object_id().into_bytes() != receipt_id.into_bytes()
                || decoded.payload_digest().into_bytes() != receipt_core_digest
            {
                return Err(SemanticAuthorityError::CorruptRecord(
                    "durability receipt canonical preimage binding",
                ));
            }
            Ok(Sha256::digest(&preimage).into())
        }
        _ => Err(SemanticAuthorityError::CorruptRecord(
            "durability receipt signature preimage version",
        )),
    }
}
