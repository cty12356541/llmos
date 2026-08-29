//! Signature-gated Capability commands (ADR-0010).
//!
//! Every `issue_root_signed` / `delegate_signed` / `revoke_signed` command
//! must carry the acting principal's Ed25519 signature over the
//! domain-separated command message, verified against the principal's
//! current Identity key binding before any durable write. The durable
//! decision digest covers only the semantic command, so replays never
//! re-verify signatures and signed/deprecated entries stay inter-replayable.
//! Stale key generations cannot be pinned by callers because verification
//! resolves the binding by principal; the expressible rotation failure is
//! key revocation, which fails closed everywhere except on replays of
//! already-durable decisions.

#![allow(deprecated)] // Bidirectional equivalence tests exercise the deprecated unsigned entries.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_capability::{
    CapabilityAuthority, CapabilityAuthorityError, CapabilityIssueDecision,
    CapabilityRevocationDecision, CapabilityRights, CapabilityTarget, DelegateCapabilityRequest,
    IssueRootCapabilityRequest, RevokeCapabilityRequest, SignedDelegateCapabilityRequest,
    SignedIssueRootCapabilityRequest, SignedRevokeCapabilityRequest, delegate_command_message,
    issue_root_command_message, revoke_command_message,
};
use nlos_identity::{
    BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose, RevokeKeyRequest,
};
use nlos_types::{Generation, IdempotencyKey, NamespaceId, PrincipalId};
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
            "nlos-capability-signed-{label}-{}-{nonce}-{}",
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

fn all_rights() -> CapabilityRights {
    CapabilityRights::SEMANTIC_APPEND
        .union(CapabilityRights::SEMANTIC_RETRACT)
        .union(CapabilityRights::DELEGATE)
}

fn root_request(
    issuer: &IdentityBinding,
    holder: &IdentityBinding,
    seed: u8,
) -> IssueRootCapabilityRequest {
    IssueRootCapabilityRequest {
        issuer_key_id: issuer.key_id,
        holder_key_id: holder.key_id,
        target: CapabilityTarget::Namespace(NamespaceId::from_bytes([0x44; 16])),
        rights: all_rights(),
        purpose_digest: None,
        valid_from_ms: 1_000,
        valid_until_ms: 9_000,
        delegation_depth_remaining: 3,
        call_limit: Some(100),
        idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
        issued_at_ms: 500,
    }
}

fn delegate_request(
    parent: nlos_capability::CapabilityRecord,
    delegator: &IdentityBinding,
    recipient: &IdentityBinding,
    seed: u8,
) -> DelegateCapabilityRequest {
    DelegateCapabilityRequest {
        parent: parent.handle,
        delegator_key_id: delegator.key_id,
        recipient_key_id: recipient.key_id,
        target: parent.target,
        rights: CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::DELEGATE),
        purpose_digest: Some([0x55; 32]),
        valid_from_ms: 1_200,
        valid_until_ms: 8_000,
        delegation_depth_remaining: 2,
        call_limit: Some(50),
        idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
        delegated_at_ms: 1_100,
    }
}

fn signed_issue_root(
    capability: &CapabilityAuthority,
    identity: &IdentityAuthority,
    key: &SigningKey,
    signer: &IdentityBinding,
    command: IssueRootCapabilityRequest,
) -> Result<CapabilityIssueDecision, CapabilityAuthorityError> {
    capability.issue_root_signed(
        identity,
        SignedIssueRootCapabilityRequest {
            command,
            signer: signer.principal_id,
            signature: key.sign(&issue_root_command_message(command)).to_bytes(),
        },
    )
}

fn signed_delegate(
    capability: &CapabilityAuthority,
    identity: &IdentityAuthority,
    key: &SigningKey,
    signer: &IdentityBinding,
    command: DelegateCapabilityRequest,
) -> Result<CapabilityIssueDecision, CapabilityAuthorityError> {
    capability.delegate_signed(
        identity,
        SignedDelegateCapabilityRequest {
            command,
            signer: signer.principal_id,
            signature: key.sign(&delegate_command_message(command)).to_bytes(),
        },
    )
}

fn signed_revoke(
    capability: &CapabilityAuthority,
    identity: &IdentityAuthority,
    key: &SigningKey,
    signer: &IdentityBinding,
    command: RevokeCapabilityRequest,
) -> Result<CapabilityRevocationDecision, CapabilityAuthorityError> {
    capability.revoke_signed(
        identity,
        SignedRevokeCapabilityRequest {
            command,
            signer: signer.principal_id,
            signature: key.sign(&revoke_command_message(command)).to_bytes(),
        },
    )
}

fn raw_connection(root: &Root) -> Connection {
    Connection::open(root.path().join("capability-authority.db")).unwrap()
}

fn raw_count(root: &Root, table: &str) -> i64 {
    raw_connection(root)
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn raw_blob(root: &Root, sql: &str) -> Vec<u8> {
    raw_connection(root)
        .query_row(sql, [], |row| row.get(0))
        .unwrap()
}

fn raw_i64(root: &Root, sql: &str) -> i64 {
    raw_connection(root)
        .query_row(sql, [], |row| row.get(0))
        .unwrap()
}

/// Signed issuance roundtrips, records the verified signer principal in the
/// durable head row, and survives an authority restart (replay + inspect).
#[test]
fn signed_issue_root_roundtrip_records_signer_principal() {
    let root = Root::new("issue-roundtrip");
    let (issuer_key, issuer, request, first) = {
        let identity = IdentityAuthority::open(root.path()).unwrap();
        let (issuer_key, issuer) = bootstrap(&identity, 10);
        let (_, holder) = bootstrap(&identity, 20);
        let capability = CapabilityAuthority::open(root.path()).unwrap();
        let command = root_request(&issuer, &holder, 0xa0);
        let first =
            signed_issue_root(&capability, &identity, &issuer_key, &issuer, command).unwrap();
        assert!(matches!(first, CapabilityIssueDecision::Issued(_, _)));
        assert_eq!(first.record().issuer, issuer.principal_id);
        assert_eq!(first.record().holder, holder.principal_id);
        (issuer_key, issuer, command, first)
    };

    let identity = IdentityAuthority::open(root.path()).unwrap();
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let replay = signed_issue_root(&capability, &identity, &issuer_key, &issuer, request).unwrap();
    assert!(matches!(replay, CapabilityIssueDecision::Replayed(_, _)));
    assert_eq!(replay.record(), first.record());
    assert_eq!(replay.receipt(), first.receipt());
    assert_eq!(
        capability
            .inspect_active(first.record().handle, 2_000)
            .unwrap(),
        first.record()
    );

    let stored = raw_blob(&root, "SELECT issuer_principal_id FROM capability_heads");
    assert_eq!(stored, first.record().issuer.as_bytes().to_vec());
}

/// Signed delegation roundtrips and durably records the delegator (the
/// verified signer) as the child capability's issuer.
#[test]
fn signed_delegate_roundtrip_records_delegator_as_issuer() {
    let root = Root::new("delegate-roundtrip");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (delegator_key, delegator) = bootstrap(&identity, 30);
    let (_, recipient) = bootstrap(&identity, 40);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let parent = signed_issue_root(
        &capability,
        &identity,
        &delegator_key,
        &delegator,
        root_request(&delegator, &delegator, 0xa1),
    )
    .unwrap()
    .record();
    let command = delegate_request(parent, &delegator, &recipient, 0xa2);
    let child = signed_delegate(&capability, &identity, &delegator_key, &delegator, command)
        .unwrap()
        .record();
    assert_eq!(child.parent, Some(parent.handle));
    assert_eq!(child.issuer, delegator.principal_id);
    assert_eq!(child.holder, recipient.principal_id);

    let replay =
        signed_delegate(&capability, &identity, &delegator_key, &delegator, command).unwrap();
    assert!(matches!(replay, CapabilityIssueDecision::Replayed(_, _)));
    assert_eq!(replay.record(), child);

    let stored = raw_blob(
        &root,
        "SELECT issuer_principal_id FROM capability_heads
         WHERE parent_capability_id IS NOT NULL",
    );
    assert_eq!(stored, delegator.principal_id.as_bytes().to_vec());
}

/// Signed revocation roundtrips and durably records the verified signer as
/// the receipt's revoker principal.
#[test]
fn signed_revocation_roundtrip_records_signer() {
    let root = Root::new("revoke-roundtrip");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (issuer_key, issuer) = bootstrap(&identity, 50);
    let (holder_key, holder) = bootstrap(&identity, 60);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let record = signed_issue_root(
        &capability,
        &identity,
        &issuer_key,
        &issuer,
        root_request(&issuer, &holder, 0xa3),
    )
    .unwrap()
    .record();
    let command = RevokeCapabilityRequest {
        handle: record.handle,
        revoker_key_id: holder.key_id,
        idempotency_key: IdempotencyKey::from_bytes([0xa4; 16]),
        revoked_at_ms: 3_000,
    };
    let first = signed_revoke(&capability, &identity, &holder_key, &holder, command).unwrap();
    assert!(matches!(first, CapabilityRevocationDecision::Revoked(_)));
    assert_eq!(first.receipt().revoker, holder.principal_id);
    assert_eq!(first.receipt().resulting_generation.get(), 2);

    let replay = signed_revoke(&capability, &identity, &holder_key, &holder, command).unwrap();
    assert!(matches!(replay, CapabilityRevocationDecision::Replayed(_)));
    assert_eq!(replay.receipt(), first.receipt());

    let stored = raw_blob(
        &root,
        "SELECT revoker_principal_id FROM capability_revocation_receipts",
    );
    assert_eq!(stored, holder.principal_id.as_bytes().to_vec());
}

/// Tampered signatures fail closed with `SignatureInvalid` and leave zero
/// durable rows behind for all three commands.
#[test]
fn tampered_signature_rejects_with_zero_durable_writes() {
    let issue_dir = Root::new("tamper-issue");
    let identity = IdentityAuthority::open(issue_dir.path()).unwrap();
    let (issuer_key, issuer) = bootstrap(&identity, 70);
    let (_, holder) = bootstrap(&identity, 80);
    let capability = CapabilityAuthority::open(issue_dir.path()).unwrap();
    let command = root_request(&issuer, &holder, 0xa5);
    let mut signature = issuer_key
        .sign(&issue_root_command_message(command))
        .to_bytes();
    signature[0] ^= 1;
    assert!(matches!(
        capability.issue_root_signed(
            &identity,
            SignedIssueRootCapabilityRequest {
                command,
                signer: issuer.principal_id,
                signature,
            },
        ),
        Err(CapabilityAuthorityError::SignatureInvalid)
    ));
    assert_eq!(raw_count(&issue_dir, "capability_heads"), 0);
    assert_eq!(raw_count(&issue_dir, "capability_versions"), 0);
    assert_eq!(raw_count(&issue_dir, "capability_issue_receipts"), 0);

    let delegate_dir = Root::new("tamper-delegate");
    let identity = IdentityAuthority::open(delegate_dir.path()).unwrap();
    let (delegator_key, delegator) = bootstrap(&identity, 90);
    let (_, recipient) = bootstrap(&identity, 100);
    let capability = CapabilityAuthority::open(delegate_dir.path()).unwrap();
    let parent = signed_issue_root(
        &capability,
        &identity,
        &delegator_key,
        &delegator,
        root_request(&delegator, &delegator, 0xa6),
    )
    .unwrap()
    .record();
    let command = delegate_request(parent, &delegator, &recipient, 0xa7);
    let mut signature = delegator_key
        .sign(&delegate_command_message(command))
        .to_bytes();
    signature[63] ^= 1;
    assert!(matches!(
        capability.delegate_signed(
            &identity,
            SignedDelegateCapabilityRequest {
                command,
                signer: delegator.principal_id,
                signature,
            },
        ),
        Err(CapabilityAuthorityError::SignatureInvalid)
    ));
    assert_eq!(raw_count(&delegate_dir, "capability_heads"), 1);
    assert_eq!(raw_count(&delegate_dir, "capability_issue_receipts"), 1);

    let revoke_dir = Root::new("tamper-revoke");
    let identity = IdentityAuthority::open(revoke_dir.path()).unwrap();
    let (issuer_key, issuer) = bootstrap(&identity, 110);
    let (holder_key, holder) = bootstrap(&identity, 120);
    let capability = CapabilityAuthority::open(revoke_dir.path()).unwrap();
    let record = signed_issue_root(
        &capability,
        &identity,
        &issuer_key,
        &issuer,
        root_request(&issuer, &holder, 0xa8),
    )
    .unwrap()
    .record();
    let command = RevokeCapabilityRequest {
        handle: record.handle,
        revoker_key_id: holder.key_id,
        idempotency_key: IdempotencyKey::from_bytes([0xa9; 16]),
        revoked_at_ms: 3_000,
    };
    let mut signature = holder_key.sign(&revoke_command_message(command)).to_bytes();
    signature[0] ^= 0x80;
    assert!(matches!(
        capability.revoke_signed(
            &identity,
            SignedRevokeCapabilityRequest {
                command,
                signer: holder.principal_id,
                signature,
            },
        ),
        Err(CapabilityAuthorityError::SignatureInvalid)
    ));
    assert_eq!(raw_count(&revoke_dir, "capability_revocation_receipts"), 0);
    assert_eq!(
        raw_i64(
            &revoke_dir,
            "SELECT current_generation FROM capability_heads"
        ),
        1
    );
}

/// A signature computed over a different command message is rejected for the
/// submitted command, with zero durable writes.
#[test]
fn signature_over_wrong_message_rejects_with_zero_writes() {
    let root = Root::new("wrong-message");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (issuer_key, issuer) = bootstrap(&identity, 130);
    let (_, holder) = bootstrap(&identity, 140);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let command = root_request(&issuer, &holder, 0xaa);
    let mut other = command;
    other.rights = CapabilityRights::SEMANTIC_APPEND;
    let signature = issuer_key
        .sign(&issue_root_command_message(other))
        .to_bytes();
    assert!(matches!(
        capability.issue_root_signed(
            &identity,
            SignedIssueRootCapabilityRequest {
                command,
                signer: issuer.principal_id,
                signature,
            },
        ),
        Err(CapabilityAuthorityError::SignatureInvalid)
    ));
    assert_eq!(raw_count(&root, "capability_heads"), 0);
}

/// Delegate and revoke signatures bind the acting principal: a valid
/// signature by any other principal cannot act as delegator or revoker.
#[test]
fn delegate_and_revoke_signatures_bind_their_own_principals() {
    let root = Root::new("bind-principals");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (delegator_key, delegator) = bootstrap(&identity, 150);
    let (recipient_key, recipient) = bootstrap(&identity, 160);
    let (attacker_key, attacker) = bootstrap(&identity, 170);
    let (_, holder) = bootstrap(&identity, 180);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let parent = signed_issue_root(
        &capability,
        &identity,
        &delegator_key,
        &delegator,
        root_request(&delegator, &delegator, 0xab),
    )
    .unwrap()
    .record();

    // The recipient's valid signature cannot execute the delegator's command.
    let command = delegate_request(parent, &delegator, &recipient, 0xac);
    assert!(matches!(
        capability.delegate_signed(
            &identity,
            SignedDelegateCapabilityRequest {
                command,
                signer: recipient.principal_id,
                signature: recipient_key
                    .sign(&delegate_command_message(command))
                    .to_bytes(),
            },
        ),
        Err(CapabilityAuthorityError::SignatureInvalid)
    ));
    assert_eq!(raw_count(&root, "capability_heads"), 1);

    // A foreign signature cannot borrow the holder's revoker key binding.
    let borrow = RevokeCapabilityRequest {
        handle: parent.handle,
        revoker_key_id: holder.key_id,
        idempotency_key: IdempotencyKey::from_bytes([0xad; 16]),
        revoked_at_ms: 3_000,
    };
    assert!(matches!(
        capability.revoke_signed(
            &identity,
            SignedRevokeCapabilityRequest {
                command: borrow,
                signer: attacker.principal_id,
                signature: attacker_key
                    .sign(&revoke_command_message(borrow))
                    .to_bytes(),
            },
        ),
        Err(CapabilityAuthorityError::SignatureInvalid)
    ));

    // An honestly signed command by an unauthorized principal is still
    // rejected by the authorization gate behind the signature.
    let hostile = RevokeCapabilityRequest {
        handle: parent.handle,
        revoker_key_id: attacker.key_id,
        idempotency_key: IdempotencyKey::from_bytes([0xae; 16]),
        revoked_at_ms: 3_000,
    };
    assert!(matches!(
        signed_revoke(&capability, &identity, &attacker_key, &attacker, hostile),
        Err(CapabilityAuthorityError::RevokerUnauthorized)
    ));
}

/// A signer principal that was never registered fails closed as
/// `PrincipalUnknown` with zero durable writes.
#[test]
fn unknown_signer_principal_fails_closed() {
    let root = Root::new("unknown-principal");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (_, issuer) = bootstrap(&identity, 190);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let command = root_request(&issuer, &issuer, 0xaf);
    assert!(matches!(
        capability.issue_root_signed(
            &identity,
            SignedIssueRootCapabilityRequest {
                command,
                signer: PrincipalId::from_bytes([0x99; 16]),
                signature: [0x77; 64],
            },
        ),
        Err(CapabilityAuthorityError::PrincipalUnknown(_))
    ));
    assert_eq!(raw_count(&root, "capability_heads"), 0);
}

/// A revoked key fails closed for every new signed command.
#[test]
fn revoked_key_fails_closed_after_registration() {
    let root = Root::new("revoked-key");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (issuer_key, issuer) = bootstrap(&identity, 200);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    signed_issue_root(
        &capability,
        &identity,
        &issuer_key,
        &issuer,
        root_request(&issuer, &issuer, 0xb0),
    )
    .unwrap();
    identity
        .revoke_key(RevokeKeyRequest {
            key_id: issuer.key_id,
            expected_key_generation: Generation::INITIAL,
            expected_identity_snapshot_id: issuer.identity_snapshot_id,
            idempotency_key: IdempotencyKey::from_bytes([0xb1; 16]),
            revoked_at_ms: 2_000,
        })
        .unwrap();
    assert!(matches!(
        signed_issue_root(
            &capability,
            &identity,
            &issuer_key,
            &issuer,
            root_request(&issuer, &issuer, 0xb2),
        ),
        Err(CapabilityAuthorityError::KeyRevoked)
    ));
    assert_eq!(raw_count(&root, "capability_heads"), 1);
}

/// Replay is the durable authority: a decided command replays its original
/// decision without re-verifying the signature, even after the signer key
/// was revoked in the Identity authority.
#[test]
fn replay_does_not_reverify_signature_after_key_revocation() {
    let root = Root::new("replay-after-revocation");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (issuer_key, issuer) = bootstrap(&identity, 210);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let command = root_request(&issuer, &issuer, 0xb3);
    let first = signed_issue_root(&capability, &identity, &issuer_key, &issuer, command).unwrap();
    assert!(matches!(first, CapabilityIssueDecision::Issued(_, _)));
    identity
        .revoke_key(RevokeKeyRequest {
            key_id: issuer.key_id,
            expected_key_generation: Generation::INITIAL,
            expected_identity_snapshot_id: issuer.identity_snapshot_id,
            idempotency_key: IdempotencyKey::from_bytes([0xb4; 16]),
            revoked_at_ms: 2_000,
        })
        .unwrap();
    let replay = signed_issue_root(&capability, &identity, &issuer_key, &issuer, command).unwrap();
    assert!(matches!(
        replay,
        CapabilityIssueDecision::Replayed(record, _) if record == first.record(),
    ));
    assert_eq!(replay.receipt(), first.receipt());
}

/// The deprecated unsigned entries and the signed entries are two fronts of
/// one durable authority: with identical commands, whichever entry executes
/// first, the other replays the exact same durable decision.
#[test]
fn signed_and_unsigned_entries_are_inter_replayable() {
    // Direction 1: the unsigned entry issues; the signed entry replays it.
    let issue_dir = Root::new("equivalence-issue");
    let identity = IdentityAuthority::open(issue_dir.path()).unwrap();
    let (issuer_key, issuer) = bootstrap(&identity, 220);
    let capability = CapabilityAuthority::open(issue_dir.path()).unwrap();
    let command = root_request(&issuer, &issuer, 0xb5);
    let unsigned = capability.issue_root(&identity, command).unwrap();
    assert!(matches!(unsigned, CapabilityIssueDecision::Issued(_, _)));
    assert!(matches!(
        signed_issue_root(&capability, &identity, &issuer_key, &issuer, command),
        Ok(CapabilityIssueDecision::Replayed(record, _)) if record == unsigned.record(),
    ));

    // Direction 2: the signed entry revokes; the unsigned entry replays it.
    let revoke_dir = Root::new("equivalence-revoke");
    let identity = IdentityAuthority::open(revoke_dir.path()).unwrap();
    let (holder_key, holder) = bootstrap(&identity, 230);
    let capability = CapabilityAuthority::open(revoke_dir.path()).unwrap();
    let record = capability
        .issue_root(&identity, root_request(&holder, &holder, 0xb6))
        .unwrap()
        .record();
    let command = RevokeCapabilityRequest {
        handle: record.handle,
        revoker_key_id: holder.key_id,
        idempotency_key: IdempotencyKey::from_bytes([0xb7; 16]),
        revoked_at_ms: 3_000,
    };
    let signed = signed_revoke(&capability, &identity, &holder_key, &holder, command).unwrap();
    assert!(matches!(signed, CapabilityRevocationDecision::Revoked(_)));
    assert!(matches!(
        capability.revoke(&identity, command),
        Ok(CapabilityRevocationDecision::Replayed(receipt)) if receipt == signed.receipt(),
    ));
}
