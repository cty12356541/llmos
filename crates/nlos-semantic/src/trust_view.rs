//! Read-only owner-local Trust View prefix for one committed event.
//!
//! Full TrustPolicy/SemanticCheckpoint resolution and Gate aggregation are
//! intentionally out of scope; this module derives taint/labels and
//! verification facts from durable admission rows only.

use rusqlite::{Connection, OptionalExtension};

use nlos_types::{ReceiptId, SemanticEventId};

use crate::model::{
    TrustViewJudgmentFact, TrustViewJudgmentRole, TrustViewSnapshot, TrustViewVerificationFact,
    TrustViewVerificationStatus, VerificationOutcome, VerificationTarget,
};
use crate::{
    EDGE_DECLARED, SemanticAuthorityError, decode_unsigned_assertion_event,
    decode_unsigned_judgment_event, decode_unsigned_verification_event, load_edge_ids,
    load_event_retraction, load_receipt,
};

pub(crate) fn inspect_trust_view(
    connection: &Connection,
    event_id: SemanticEventId,
) -> Result<TrustViewSnapshot, SemanticAuthorityError> {
    let admitted = connection
        .query_row(
            "SELECT 1 FROM admission_receipts WHERE event_id=?1",
            [event_id.as_bytes().as_slice()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !admitted {
        return Err(SemanticAuthorityError::EventNotFound(event_id));
    }
    let admission = load_receipt(connection, event_id)?;
    let declared_parents = load_edge_ids(connection, event_id, EDGE_DECLARED)?;
    validate_committed_lineage(
        connection,
        event_id,
        &declared_parents,
        &admission.captured_inputs,
    )?;
    let declassification_receipt_id = load_declassification_receipt_id(connection, event_id)?;
    let verification_facts = load_verification_facts(connection, event_id)?;
    let judgment_facts = load_judgment_facts(connection, event_id)?;
    let retraction = load_event_retraction(connection, event_id)?;
    let verification_status = derive_verification_status(&verification_facts);
    Ok(TrustViewSnapshot {
        event_id,
        effective_taint: admission.effective_taint,
        declassification_receipt_id,
        verification_status,
        verification_facts,
        judgment_facts,
        retraction,
    })
}

fn validate_committed_lineage(
    connection: &Connection,
    event_id: SemanticEventId,
    declared: &[SemanticEventId],
    captured: &[SemanticEventId],
) -> Result<(), SemanticAuthorityError> {
    for parent in declared.iter().chain(captured) {
        if *parent == event_id {
            return Err(SemanticAuthorityError::InvalidLineage);
        }
        let exists = connection
            .query_row(
                "SELECT 1 FROM admission_receipts WHERE event_id=?1",
                [parent.as_bytes().as_slice()],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            return Err(SemanticAuthorityError::DanglingLineage(*parent));
        }
    }
    Ok(())
}

fn load_declassification_receipt_id(
    connection: &Connection,
    event_id: SemanticEventId,
) -> Result<Option<ReceiptId>, SemanticAuthorityError> {
    let row = connection
        .query_row(
            "SELECT event_type, canonical_unsigned_event FROM semantic_events WHERE event_id=?1",
            [event_id.as_bytes().as_slice()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    let Some((event_type, canonical)) = row else {
        return Err(SemanticAuthorityError::EventNotFound(event_id));
    };
    if event_type != 1 {
        return Ok(None);
    }
    Ok(decode_unsigned_assertion_event(&canonical)?.declassification_receipt_id)
}

fn load_verification_facts(
    connection: &Connection,
    subject_event_id: SemanticEventId,
) -> Result<Vec<TrustViewVerificationFact>, SemanticAuthorityError> {
    let mut statement = connection.prepare(
        "SELECT e.canonical_unsigned_event, l.log_seq, a.admitted_at_ms
         FROM semantic_events e
         JOIN event_log l ON l.event_id = e.event_id
         JOIN admission_receipts a ON a.event_id = e.event_id
         WHERE e.event_type = 3
         ORDER BY l.log_seq",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut facts = Vec::new();
    for row in rows {
        let (canonical, log_seq, admitted_at_ms) = row?;
        let event = decode_unsigned_verification_event(&canonical)?;
        if matches!(
            event.target,
            VerificationTarget::Event(target) if target.event_id == subject_event_id
        ) {
            facts.push(TrustViewVerificationFact {
                verification_event_id: crate::semantic_event_id(&canonical),
                log_seq: crate::decode_u64(log_seq)?,
                outcome: event.outcome,
                admitted_at_ms: crate::decode_u64(admitted_at_ms)?,
            });
        }
    }
    Ok(facts)
}

fn load_judgment_facts(
    connection: &Connection,
    subject_event_id: SemanticEventId,
) -> Result<Vec<TrustViewJudgmentFact>, SemanticAuthorityError> {
    let mut statement = connection.prepare(
        "SELECT e.canonical_unsigned_event, l.log_seq
         FROM semantic_events e
         JOIN event_log l ON l.event_id = e.event_id
         JOIN admission_receipts a ON a.event_id = e.event_id
         WHERE e.event_type = 2
         ORDER BY l.log_seq",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut facts = Vec::new();
    for row in rows {
        let (canonical, log_seq) = row?;
        let event = decode_unsigned_judgment_event(&canonical)?;
        if event.source == subject_event_id {
            facts.push(TrustViewJudgmentFact {
                judgment_event_id: crate::semantic_event_id(&canonical),
                log_seq: crate::decode_u64(log_seq)?,
                relation: event.relation,
                counterpart_event_id: event.target,
                role: TrustViewJudgmentRole::Source,
            });
        } else if event.target == subject_event_id {
            facts.push(TrustViewJudgmentFact {
                judgment_event_id: crate::semantic_event_id(&canonical),
                log_seq: crate::decode_u64(log_seq)?,
                relation: event.relation,
                counterpart_event_id: event.source,
                role: TrustViewJudgmentRole::Target,
            });
        }
    }
    Ok(facts)
}

fn derive_verification_status(facts: &[TrustViewVerificationFact]) -> TrustViewVerificationStatus {
    let Some(latest) = facts.iter().max_by_key(|fact| fact.log_seq) else {
        return TrustViewVerificationStatus::Unverified;
    };
    match latest.outcome {
        VerificationOutcome::Pass => TrustViewVerificationStatus::Pass,
        VerificationOutcome::Fail => TrustViewVerificationStatus::Fail,
        VerificationOutcome::Inconclusive => TrustViewVerificationStatus::Inconclusive,
        VerificationOutcome::Error => TrustViewVerificationStatus::Error,
    }
}
