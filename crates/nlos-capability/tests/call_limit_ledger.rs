#![allow(deprecated)] // Deprecated unsigned Capability entries stay the test front for issuing capabilities.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_capability::{
    CapabilityAuthority, CapabilityAuthorityError, CapabilityConsumptionDecision, CapabilityRecord,
    CapabilityRights, CapabilityTarget, ConsumeCapabilityRequest, DelegateCapabilityRequest,
    IssueRootCapabilityRequest,
};
use nlos_identity::{
    BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose,
    VerifySemanticSignatureRequest, semantic_signature_message,
};
use nlos_types::{CapabilityId, IdempotencyKey, NamespaceId, SemanticEventId};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-capability-ledger-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn bootstrap(identity: &IdentityAuthority, seed: u8) -> (SigningKey, IdentityBinding) {
    let key = signing_key(seed);
    let binding = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: 10_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .unwrap()
        .binding();
    (key, binding)
}

fn root_request(
    issuer: IdentityBinding,
    holder: IdentityBinding,
    seed: u8,
    call_limit: Option<u64>,
) -> IssueRootCapabilityRequest {
    IssueRootCapabilityRequest {
        issuer_key_id: issuer.key_id,
        holder_key_id: holder.key_id,
        target: CapabilityTarget::Namespace(NamespaceId::from_bytes([0x44; 16])),
        rights: CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::DELEGATE),
        purpose_digest: None,
        valid_from_ms: 1_000,
        valid_until_ms: 9_000,
        delegation_depth_remaining: 3,
        call_limit,
        idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
        issued_at_ms: 500,
    }
}

fn consume_request(
    identity: &IdentityAuthority,
    key: &SigningKey,
    binding: IdentityBinding,
    record: CapabilityRecord,
    idempotency_key: IdempotencyKey,
    at_ms: u64,
) -> ConsumeCapabilityRequest {
    let event_id = SemanticEventId::from_bytes([0x66; 32]);
    let signature = key.sign(&semantic_signature_message(event_id)).to_bytes();
    let signer = identity
        .verify_semantic_signature(VerifySemanticSignatureRequest {
            event_id,
            issuer: binding.principal_id,
            control_domain_id: binding.control_domain_id,
            key_id: binding.key_id,
            signature,
            admitted_at_ms: at_ms,
        })
        .unwrap();
    ConsumeCapabilityRequest {
        handle: record.handle,
        signer,
        target: record.target,
        required_right: CapabilityRights::SEMANTIC_APPEND,
        purpose_digest: record.purpose_digest,
        idempotency_key,
        consumed_at_ms: at_ms,
    }
}

fn consumption_row_count(root: &Path, capability_id: CapabilityId) -> i64 {
    let raw = Connection::open(root.join("capability-authority.db")).unwrap();
    raw.query_row(
        "SELECT COUNT(*) FROM capability_consumption_rows WHERE capability_id=?1",
        [capability_id.as_bytes().as_slice()],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn consume_decrements_and_exhaustion_rejects_typed_with_zero_partial_state() {
    let root = Root::new("deplete");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (holder_key, holder) = bootstrap(&identity, 10);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let record = capability
        .issue_root(&identity, root_request(holder, holder, 0x20, Some(2)))
        .unwrap()
        .record();
    assert_eq!(
        capability.call_limit_remaining(record.handle).unwrap(),
        Some(2)
    );

    let first = capability
        .consume(consume_request(
            &identity,
            &holder_key,
            holder,
            record,
            IdempotencyKey::from_bytes([0x21; 16]),
            2_000,
        ))
        .unwrap();
    assert!(matches!(
        first,
        CapabilityConsumptionDecision::Consumed(receipt) if receipt.remaining == Some(1),
    ));
    assert_eq!(
        capability.call_limit_remaining(record.handle).unwrap(),
        Some(1)
    );

    capability
        .consume(consume_request(
            &identity,
            &holder_key,
            holder,
            record,
            IdempotencyKey::from_bytes([0x22; 16]),
            2_000,
        ))
        .unwrap();
    assert_eq!(
        capability.call_limit_remaining(record.handle).unwrap(),
        Some(0)
    );

    let exhausted = capability.consume(consume_request(
        &identity,
        &holder_key,
        holder,
        record,
        IdempotencyKey::from_bytes([0x23; 16]),
        2_000,
    ));
    assert!(matches!(
        exhausted,
        Err(CapabilityAuthorityError::CallLimitExhausted)
    ));
    assert_eq!(
        capability.call_limit_remaining(record.handle).unwrap(),
        Some(0)
    );
    assert_eq!(
        consumption_row_count(root.path(), record.handle.capability_id),
        2
    );
}

#[test]
fn consume_replay_is_free_conflicts_fail_closed_and_restart_persists() {
    let root = Root::new("replay");
    let record = {
        let identity = IdentityAuthority::open(root.path()).unwrap();
        let (holder_key, holder) = bootstrap(&identity, 20);
        let capability = CapabilityAuthority::open(root.path()).unwrap();
        let record = capability
            .issue_root(&identity, root_request(holder, holder, 0x30, Some(3)))
            .unwrap()
            .record();
        let first = capability
            .consume(consume_request(
                &identity,
                &holder_key,
                holder,
                record,
                IdempotencyKey::from_bytes([0x31; 16]),
                2_000,
            ))
            .unwrap();
        assert!(matches!(
            first,
            CapabilityConsumptionDecision::Consumed(receipt) if receipt.remaining == Some(2),
        ));

        let replay = capability.consume(consume_request(
            &identity,
            &holder_key,
            holder,
            record,
            IdempotencyKey::from_bytes([0x31; 16]),
            2_000,
        ));
        assert!(matches!(
            replay,
            Ok(CapabilityConsumptionDecision::Replayed(receipt)) if receipt == first.receipt(),
        ));
        assert_eq!(
            capability.call_limit_remaining(record.handle).unwrap(),
            Some(2)
        );

        let mut rebound = consume_request(
            &identity,
            &holder_key,
            holder,
            record,
            IdempotencyKey::from_bytes([0x31; 16]),
            2_000,
        );
        rebound.consumed_at_ms = 2_001;
        assert!(matches!(
            capability.consume(rebound),
            Err(CapabilityAuthorityError::IdempotencyConflict)
        ));
        assert_eq!(
            capability.call_limit_remaining(record.handle).unwrap(),
            Some(2)
        );
        record
    };

    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (holder_key, holder) = bootstrap(&identity, 20);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    assert_eq!(
        capability.call_limit_remaining(record.handle).unwrap(),
        Some(2)
    );
    let replay = capability.consume(consume_request(
        &identity,
        &holder_key,
        holder,
        record,
        IdempotencyKey::from_bytes([0x31; 16]),
        2_000,
    ));
    assert!(matches!(
        replay,
        Ok(CapabilityConsumptionDecision::Replayed(receipt))
            if receipt.remaining == Some(2),
    ));
    let _ = holder_key;
}

#[test]
fn concurrent_consumption_linearizes_under_quota() {
    let root = Root::new("linearize");
    let identity = Arc::new(IdentityAuthority::open(root.path()).unwrap());
    let (holder_key, holder) = bootstrap(&identity, 30);
    let capability = Arc::new(CapabilityAuthority::open(root.path()).unwrap());
    let record = Arc::new(
        capability
            .issue_root(&identity, root_request(holder, holder, 0x40, Some(3)))
            .unwrap()
            .record(),
    );
    let holder = Arc::new(holder);
    let holder_key = Arc::new(holder_key);

    let receivers: Vec<_> = (0..8u8)
        .map(|seed| {
            let identity = Arc::clone(&identity);
            let capability = Arc::clone(&capability);
            let holder = Arc::clone(&holder);
            let holder_key = Arc::clone(&holder_key);
            let record = Arc::clone(&record);
            let (sender, receiver) = std::sync::mpsc::channel();
            thread::spawn(move || {
                let _ = sender.send(capability.consume(consume_request(
                    &identity,
                    &holder_key,
                    *holder,
                    *record,
                    IdempotencyKey::from_bytes([0x50 + seed; 16]),
                    2_000,
                )));
            });
            receiver
        })
        .collect();

    let mut consumed_remaining = Vec::new();
    let mut exhausted = 0;
    for receiver in receivers {
        match receiver.recv().unwrap() {
            Ok(CapabilityConsumptionDecision::Consumed(receipt)) => {
                consumed_remaining.push(receipt.remaining);
            }
            Ok(CapabilityConsumptionDecision::Replayed(_)) => {
                panic!("distinct idempotency keys must never replay each other");
            }
            Err(CapabilityAuthorityError::CallLimitExhausted) => exhausted += 1,
            Err(error) => panic!("unexpected concurrent failure: {error}"),
        }
    }
    assert_eq!(consumed_remaining.len(), 3);
    assert_eq!(exhausted, 5);
    consumed_remaining.sort_unstable();
    assert_eq!(consumed_remaining, vec![Some(0), Some(1), Some(2)]);
    assert_eq!(
        capability.call_limit_remaining(record.handle).unwrap(),
        Some(0)
    );
    assert_eq!(
        consumption_row_count(root.path(), record.handle.capability_id),
        3
    );
}

#[test]
fn unlimited_capability_never_exhausts_and_readback_is_none() {
    let root = Root::new("unlimited");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (holder_key, holder) = bootstrap(&identity, 40);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let record = capability
        .issue_root(&identity, root_request(holder, holder, 0x60, None))
        .unwrap()
        .record();
    assert_eq!(
        capability.call_limit_remaining(record.handle).unwrap(),
        None
    );
    for seed in 0..5u8 {
        let decision = capability
            .consume(consume_request(
                &identity,
                &holder_key,
                holder,
                record,
                IdempotencyKey::from_bytes([0x61 + seed; 16]),
                2_000,
            ))
            .unwrap();
        assert!(matches!(
            decision,
            CapabilityConsumptionDecision::Consumed(receipt) if receipt.remaining.is_none(),
        ));
    }
    assert_eq!(
        capability.call_limit_remaining(record.handle).unwrap(),
        None
    );
    assert_eq!(
        consumption_row_count(root.path(), record.handle.capability_id),
        5
    );
}

#[test]
fn delegated_budget_is_independent_not_pooled_with_parent() {
    let root = Root::new("delegate-budget");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (delegator_key, delegator) = bootstrap(&identity, 50);
    let (recipient_key, recipient) = bootstrap(&identity, 60);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let parent = capability
        .issue_root(
            &identity,
            root_request(delegator, delegator, 0x70, Some(10)),
        )
        .unwrap()
        .record();
    let child = capability
        .delegate(
            &identity,
            DelegateCapabilityRequest {
                parent: parent.handle,
                delegator_key_id: delegator.key_id,
                recipient_key_id: recipient.key_id,
                target: parent.target,
                rights: CapabilityRights::SEMANTIC_APPEND,
                purpose_digest: parent.purpose_digest,
                valid_from_ms: parent.valid_from_ms,
                valid_until_ms: parent.valid_until_ms,
                delegation_depth_remaining: 2,
                call_limit: Some(5),
                idempotency_key: IdempotencyKey::from_bytes([0x71; 16]),
                delegated_at_ms: 1_100,
            },
        )
        .unwrap()
        .record();

    for seed in 0..5u8 {
        capability
            .consume(consume_request(
                &identity,
                &recipient_key,
                recipient,
                child,
                IdempotencyKey::from_bytes([0x72 + seed; 16]),
                2_000,
            ))
            .unwrap();
    }
    assert_eq!(
        capability.call_limit_remaining(child.handle).unwrap(),
        Some(0)
    );
    assert!(matches!(
        capability.consume(consume_request(
            &identity,
            &recipient_key,
            recipient,
            child,
            IdempotencyKey::from_bytes([0x7d; 16]),
            2_000,
        )),
        Err(CapabilityAuthorityError::CallLimitExhausted)
    ));
    assert_eq!(
        capability.call_limit_remaining(parent.handle).unwrap(),
        Some(10)
    );
    assert_eq!(
        consumption_row_count(root.path(), child.handle.capability_id),
        5
    );
    assert_eq!(
        consumption_row_count(root.path(), parent.handle.capability_id),
        0
    );

    capability
        .consume(consume_request(
            &identity,
            &delegator_key,
            delegator,
            parent,
            IdempotencyKey::from_bytes([0x7e; 16]),
            2_000,
        ))
        .unwrap();
    assert_eq!(
        capability.call_limit_remaining(parent.handle).unwrap(),
        Some(9)
    );
    assert_eq!(
        capability.call_limit_remaining(child.handle).unwrap(),
        Some(0)
    );
}

#[test]
fn consume_enforces_semantic_admission_gates_without_spending() {
    let root = Root::new("gates");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (holder_key, holder) = bootstrap(&identity, 70);
    let (_, outsider) = bootstrap(&identity, 80);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let record = capability
        .issue_root(&identity, root_request(holder, holder, 0x80, Some(1)))
        .unwrap()
        .record();

    assert!(matches!(
        capability.consume(consume_request(
            &identity,
            &signing_key(80),
            outsider,
            record,
            IdempotencyKey::from_bytes([0x81; 16]),
            2_000,
        )),
        Err(CapabilityAuthorityError::HolderMismatch)
    ));
    assert_eq!(
        capability.call_limit_remaining(record.handle).unwrap(),
        Some(1)
    );

    let mut wrong_purpose = consume_request(
        &identity,
        &holder_key,
        holder,
        record,
        IdempotencyKey::from_bytes([0x82; 16]),
        2_000,
    );
    wrong_purpose.purpose_digest = Some([0x99; 32]);
    assert!(matches!(
        capability.consume(wrong_purpose),
        Err(CapabilityAuthorityError::PurposeMismatch)
    ));

    let expired = consume_request(
        &identity,
        &holder_key,
        holder,
        record,
        IdempotencyKey::from_bytes([0x83; 16]),
        9_001,
    );
    assert!(matches!(
        capability.consume(expired),
        Err(CapabilityAuthorityError::CapabilityExpired)
    ));

    assert_eq!(
        consumption_row_count(root.path(), record.handle.capability_id),
        0
    );
}

#[test]
fn consumption_rows_are_ddl_protected() {
    let root = Root::new("immutable-ledger");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (holder_key, holder) = bootstrap(&identity, 90);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let record = capability
        .issue_root(&identity, root_request(holder, holder, 0x90, Some(2)))
        .unwrap()
        .record();
    capability
        .consume(consume_request(
            &identity,
            &holder_key,
            holder,
            record,
            IdempotencyKey::from_bytes([0x91; 16]),
            2_000,
        ))
        .unwrap();
    drop(capability);

    let raw = Connection::open(root.path().join("capability-authority.db")).unwrap();
    assert!(
        raw.execute(
            "UPDATE capability_consumption_rows SET remaining=99 WHERE idempotency_key=?1",
            [IdempotencyKey::from_bytes([0x91; 16]).as_bytes().as_slice()]
        )
        .is_err()
    );
    assert!(
        raw.execute("DELETE FROM capability_consumption_rows", [])
            .is_err()
    );
}
