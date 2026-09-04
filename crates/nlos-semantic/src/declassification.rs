use nlos_capability::{
    AuthorizeSemanticRequest, CapabilityAuthority, CapabilityRights, CapabilityTarget,
};
use nlos_identity::{
    IdentityAuthority, VerifySemanticAuthoritySignatureRequest, VerifySemanticSignatureRequest,
};
use nlos_types::{ControlDomainId, KeyId, PrincipalId, ReceiptId, SemanticEventId};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};

use crate::{
    IssueDeclassificationDecision, IssueDeclassificationReceiptRequest, MAX_LINEAGE_ITEMS,
    MAX_NONCE_BYTES, MIN_NONCE_BYTES, SemanticAuthorityError, StoreSigner, TaintFlags, canonical,
    decode_u64, encode_scope, encode_u64, model::DeclassificationReceipt,
};

#[allow(clippy::too_many_arguments)] // Admission binding facts are explicit per `[SEM-DECLASS-001]`.
pub(crate) fn apply_declassification(
    transaction: &Transaction<'_>,
    effective_taint: TaintFlags,
    holder: PrincipalId,
    scope: CapabilityTarget,
    purpose_digest: Option<[u8; 32]>,
    declared: &[SemanticEventId],
    captured: &[SemanticEventId],
    declassification_receipt_id: Option<ReceiptId>,
    admitted_at_ms: u64,
) -> Result<TaintFlags, SemanticAuthorityError> {
    let Some(receipt_id) = declassification_receipt_id else {
        return Ok(effective_taint);
    };
    let receipt = load_declassification_receipt(transaction, receipt_id)?.ok_or(
        SemanticAuthorityError::DeclassificationReceiptNotFound(receipt_id),
    )?;
    if receipt.expires_at_ms < admitted_at_ms {
        return Err(SemanticAuthorityError::DeclassificationReceiptExpired);
    }
    if receipt.holder != holder {
        return Err(SemanticAuthorityError::DeclassificationReceiptHolderMismatch);
    }
    if receipt.scope != scope {
        return Err(SemanticAuthorityError::DeclassificationReceiptScopeMismatch);
    }
    if receipt.purpose_digest != purpose_digest {
        return Err(SemanticAuthorityError::DeclassificationReceiptPurposeMismatch);
    }
    let lineage: std::collections::BTreeSet<_> = declared.iter().chain(captured).copied().collect();
    for source in &receipt.source_events {
        if !lineage.contains(source) {
            return Err(SemanticAuthorityError::DeclassificationReceiptSourceMismatch(*source));
        }
    }
    if !effective_taint.contains(receipt.removed_labels) {
        return Err(SemanticAuthorityError::DeclassificationLabelNotPresent);
    }
    Ok(effective_taint.without(receipt.removed_labels))
}

#[allow(clippy::too_many_lines)] // Issuance gates mirror assertion admission ordering.
pub(crate) fn issue_declassification_receipt(
    connection: &mut Connection,
    identity: &IdentityAuthority,
    capability: &CapabilityAuthority,
    store_signer: &impl StoreSigner,
    request: &IssueDeclassificationReceiptRequest,
) -> Result<IssueDeclassificationDecision, SemanticAuthorityError> {
    validate_declassification_issue_request(request)?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(existing) = load_declassification_by_nonce(&transaction, &request.nonce)? {
        let replay = declassification_receipt_from_row(&transaction, existing)?;
        if declassification_request_matches_receipt(request, &replay) {
            transaction.commit()?;
            return Ok(IssueDeclassificationDecision::Replayed(replay));
        }
        return Err(SemanticAuthorityError::DeclassificationNonceReplayConflict);
    }

    let adjudicator_binding = identity.inspect_current_binding(request.adjudicator_key_id)?;
    let issue_event_id = declassification_issue_event_id(request);
    let signer = identity.verify_semantic_signature(VerifySemanticSignatureRequest {
        event_id: issue_event_id,
        issuer: adjudicator_binding.principal_id,
        control_domain_id: adjudicator_binding.control_domain_id,
        key_id: request.adjudicator_key_id,
        signature: request.adjudicator_signature,
        admitted_at_ms: request.issued_at_ms,
    })?;
    capability.authorize_semantic(AuthorizeSemanticRequest {
        handle: request.capability,
        signer,
        target: request.scope,
        required_right: CapabilityRights::SEMANTIC_ADJUDICATE,
        purpose_digest: request.purpose_digest,
        admitted_at_ms: request.issued_at_ms,
    })?;
    let capability_record = capability.inspect_active(request.capability, request.issued_at_ms)?;
    if capability_record.valid_until_ms < request.issued_at_ms {
        return Err(SemanticAuthorityError::Capability(
            nlos_capability::CapabilityAuthorityError::CapabilityExpired,
        ));
    }
    for source in &request.source_events {
        let exists = transaction
            .query_row(
                "SELECT 1 FROM admission_receipts WHERE event_id=?1",
                [source.as_bytes().as_slice()],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            return Err(SemanticAuthorityError::DanglingLineage(*source));
        }
    }

    let receipt_core_digest = build_declassification_receipt_core_digest(
        request.holder,
        request.scope,
        &request.source_events,
        request.removed_labels,
        request.purpose_digest,
        request.expires_at_ms,
        &request.nonce,
        request.issued_at_ms,
        store_signer.principal_id(),
        store_signer.control_domain_id(),
        store_signer.key_id(),
    );
    let mut receipt_id_bytes = [0_u8; 16];
    receipt_id_bytes.copy_from_slice(&receipt_core_digest[..16]);
    let receipt_id = ReceiptId::from_bytes(receipt_id_bytes);
    let receipt_message =
        declassification_receipt_signature_message(receipt_id, receipt_core_digest);
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
            verified_at_ms: request.issued_at_ms,
        })?;
    if verified_store.principal_id() != store_signer.principal_id()
        || verified_store.control_domain_id() != store_signer.control_domain_id()
        || verified_store.key_id() != store_signer.key_id()
    {
        return Err(SemanticAuthorityError::StoreSignerBindingMismatch);
    }
    let receipt = DeclassificationReceipt {
        receipt_id,
        holder: request.holder,
        scope: request.scope,
        source_events: request.source_events.clone(),
        removed_labels: request.removed_labels,
        purpose_digest: request.purpose_digest,
        expires_at_ms: request.expires_at_ms,
        nonce: request.nonce.clone(),
        issued_at_ms: request.issued_at_ms,
        store_principal: store_signer.principal_id(),
        store_control_domain: store_signer.control_domain_id(),
        store_key_id: store_signer.key_id(),
        store_signature,
    };
    insert_declassification_receipt(&transaction, &receipt)?;
    transaction.commit()?;
    Ok(IssueDeclassificationDecision::Issued(receipt))
}

pub(crate) fn inspect_declassification_receipt(
    connection: &Connection,
    receipt_id: ReceiptId,
) -> Result<DeclassificationReceipt, SemanticAuthorityError> {
    load_declassification_receipt(connection, receipt_id)?.ok_or(
        SemanticAuthorityError::DeclassificationReceiptNotFound(receipt_id),
    )
}

/// Domain-separated authorization identity for declassification issuance.
///
/// Adjudicators sign `semantic_signature_message` over this value.
#[must_use]
pub fn declassification_issue_authorization_id(
    request: &IssueDeclassificationReceiptRequest,
) -> SemanticEventId {
    declassification_issue_event_id(request)
}

fn validate_declassification_issue_request(
    request: &IssueDeclassificationReceiptRequest,
) -> Result<(), SemanticAuthorityError> {
    if !(MIN_NONCE_BYTES..=MAX_NONCE_BYTES).contains(&request.nonce.len()) {
        return Err(SemanticAuthorityError::InvalidNonce);
    }
    canonical::validate_sorted_unique(&request.source_events)?;
    if request.source_events.is_empty() || request.source_events.len() > MAX_LINEAGE_ITEMS {
        return Err(SemanticAuthorityError::InvalidLineage);
    }
    if request.removed_labels.bits() == 0 {
        return Err(SemanticAuthorityError::DeclassificationRemovedLabelsEmpty);
    }
    Ok(())
}

fn declassification_issue_event_id(
    request: &IssueDeclassificationReceiptRequest,
) -> SemanticEventId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/declassification-issue/v1");
    hasher.update(request.holder.as_bytes());
    let (scope_kind, scope_id) = encode_scope(request.scope);
    hasher.update([u8::try_from(scope_kind).unwrap_or(0)]);
    hasher.update(scope_id);
    hasher.update((request.source_events.len() as u64).to_be_bytes());
    for source in &request.source_events {
        hasher.update(source.as_bytes());
    }
    hasher.update(request.removed_labels.bits().to_be_bytes());
    match request.purpose_digest {
        Some(digest) => {
            hasher.update([1]);
            hasher.update(digest);
        }
        None => hasher.update([0]),
    }
    hasher.update(request.expires_at_ms.to_be_bytes());
    hasher.update((request.nonce.len() as u64).to_be_bytes());
    hasher.update(&request.nonce);
    SemanticEventId::from_bytes(hasher.finalize().into())
}

fn declassification_request_matches_receipt(
    request: &IssueDeclassificationReceiptRequest,
    receipt: &DeclassificationReceipt,
) -> bool {
    request.holder == receipt.holder
        && request.scope == receipt.scope
        && request.source_events == receipt.source_events
        && request.removed_labels == receipt.removed_labels
        && request.purpose_digest == receipt.purpose_digest
        && request.expires_at_ms == receipt.expires_at_ms
        && request.nonce == receipt.nonce
}

#[allow(clippy::too_many_arguments)] // Receipt core digest binds every authorization fact.
fn build_declassification_receipt_core_digest(
    holder: PrincipalId,
    scope: CapabilityTarget,
    source_events: &[SemanticEventId],
    removed_labels: TaintFlags,
    purpose_digest: Option<[u8; 32]>,
    expires_at_ms: u64,
    nonce: &[u8],
    issued_at_ms: u64,
    store_principal: PrincipalId,
    store_control_domain: ControlDomainId,
    store_key_id: KeyId,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/declassification-receipt/v1");
    hasher.update(holder.as_bytes());
    let (scope_kind, scope_id) = encode_scope(scope);
    hasher.update([u8::try_from(scope_kind).unwrap_or(0)]);
    hasher.update(scope_id);
    hasher.update((source_events.len() as u64).to_be_bytes());
    for source in source_events {
        hasher.update(source.as_bytes());
    }
    hasher.update(removed_labels.bits().to_be_bytes());
    match purpose_digest {
        Some(digest) => {
            hasher.update([1]);
            hasher.update(digest);
        }
        None => hasher.update([0]),
    }
    hasher.update(expires_at_ms.to_be_bytes());
    hasher.update((nonce.len() as u64).to_be_bytes());
    hasher.update(nonce);
    hasher.update(issued_at_ms.to_be_bytes());
    hasher.update(store_principal.as_bytes());
    hasher.update(store_control_domain.as_bytes());
    hasher.update(store_key_id.as_bytes());
    hasher.finalize().into()
}

fn declassification_receipt_signature_message(
    receipt_id: ReceiptId,
    core_digest: [u8; 32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/declassification-receipt-signature/v1");
    hasher.update(receipt_id.as_bytes());
    hasher.update(core_digest);
    hasher.finalize().into()
}

fn load_declassification_by_nonce(
    transaction: &Transaction<'_>,
    nonce: &[u8],
) -> Result<Option<ReceiptId>, SemanticAuthorityError> {
    let row = transaction
        .query_row(
            "SELECT receipt_id FROM declassification_receipts WHERE nonce=?1",
            [nonce],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    row.map(|bytes| crate::decode_id(bytes, ReceiptId::from_bytes, "declassification receipt id"))
        .transpose()
}

fn declassification_receipt_from_row(
    transaction: &Transaction<'_>,
    receipt_id: ReceiptId,
) -> Result<DeclassificationReceipt, SemanticAuthorityError> {
    load_declassification_receipt(transaction, receipt_id)?.ok_or(
        SemanticAuthorityError::DeclassificationReceiptNotFound(receipt_id),
    )
}

fn load_declassification_receipt(
    connection: &Connection,
    receipt_id: ReceiptId,
) -> Result<Option<DeclassificationReceipt>, SemanticAuthorityError> {
    let row = connection
        .query_row(
            "SELECT holder_principal_id, scope_kind, scope_id, removed_labels, purpose_digest,
                    expires_at_ms, nonce, issued_at_ms, store_principal_id,
                    store_control_domain_id, store_key_id, store_signature
             FROM declassification_receipts WHERE receipt_id=?1",
            [receipt_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, Vec<u8>>(8)?,
                    row.get::<_, Vec<u8>>(9)?,
                    row.get::<_, Vec<u8>>(10)?,
                    row.get::<_, Vec<u8>>(11)?,
                ))
            },
        )
        .optional()?;
    let Some((
        holder,
        scope_kind,
        scope_id,
        removed_labels,
        purpose_digest,
        expires_at_ms,
        nonce,
        issued_at_ms,
        store_principal,
        store_control_domain,
        store_key_id,
        store_signature,
    )) = row
    else {
        return Ok(None);
    };
    let scope = crate::decode_scope(
        scope_kind,
        crate::decode_array(scope_id, "declassification scope id")?,
    )?;
    let removed_labels = u64::try_from(removed_labels)
        .map_err(|_| SemanticAuthorityError::CorruptRecord("negative removed labels"))?;
    let removed_labels = TaintFlags::from_bits(removed_labels).ok_or(
        SemanticAuthorityError::CorruptRecord("unknown removed labels"),
    )?;
    let purpose_digest = purpose_digest
        .map(|bytes| crate::decode_array(bytes, "declassification purpose digest"))
        .transpose()?;
    let mut statement = connection.prepare(
        "SELECT source_event_id FROM declassification_source_events
         WHERE receipt_id=?1 ORDER BY source_event_id",
    )?;
    let mut source_events = Vec::new();
    let mut rows = statement.query([receipt_id.as_bytes().as_slice()])?;
    while let Some(row) = rows.next()? {
        source_events.push(crate::decode_id(
            row.get(0)?,
            SemanticEventId::from_bytes,
            "declassification source",
        )?);
    }
    Ok(Some(DeclassificationReceipt {
        receipt_id,
        holder: crate::decode_id(holder, PrincipalId::from_bytes, "declassification holder")?,
        scope,
        source_events,
        removed_labels,
        purpose_digest,
        expires_at_ms: decode_u64(expires_at_ms)?,
        nonce,
        issued_at_ms: decode_u64(issued_at_ms)?,
        store_principal: crate::decode_id(
            store_principal,
            PrincipalId::from_bytes,
            "declassification store principal",
        )?,
        store_control_domain: crate::decode_id(
            store_control_domain,
            ControlDomainId::from_bytes,
            "declassification store domain",
        )?,
        store_key_id: crate::decode_id(
            store_key_id,
            KeyId::from_bytes,
            "declassification store key",
        )?,
        store_signature: crate::decode_array(store_signature, "declassification store signature")?,
    }))
}

fn insert_declassification_receipt(
    transaction: &Transaction<'_>,
    receipt: &DeclassificationReceipt,
) -> Result<(), SemanticAuthorityError> {
    let (scope_kind, scope_id) = encode_scope(receipt.scope);
    transaction.execute(
        "INSERT INTO declassification_receipts (
            receipt_id, holder_principal_id, scope_kind, scope_id, removed_labels,
            purpose_digest, expires_at_ms, nonce, issued_at_ms,
            store_principal_id, store_control_domain_id, store_key_id, store_signature
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            receipt.receipt_id.as_bytes().as_slice(),
            receipt.holder.as_bytes().as_slice(),
            scope_kind,
            scope_id.as_slice(),
            i64::try_from(receipt.removed_labels.bits())
                .map_err(|_| SemanticAuthorityError::CorruptRecord("removed labels"))?,
            receipt.purpose_digest,
            encode_u64(receipt.expires_at_ms)?,
            receipt.nonce.as_slice(),
            encode_u64(receipt.issued_at_ms)?,
            receipt.store_principal.as_bytes().as_slice(),
            receipt.store_control_domain.as_bytes().as_slice(),
            receipt.store_key_id.as_bytes().as_slice(),
            receipt.store_signature.as_slice(),
        ],
    )?;
    for source in &receipt.source_events {
        transaction.execute(
            "INSERT INTO declassification_source_events (receipt_id, source_event_id)
             VALUES (?1, ?2)",
            params![
                receipt.receipt_id.as_bytes().as_slice(),
                source.as_bytes().as_slice(),
            ],
        )?;
    }
    Ok(())
}
