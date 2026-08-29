#![allow(deprecated)] // Deprecated unsigned Capability entries stay pinned as replay-equivalent to the signed entries.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_capability::{
    AuthorizeSemanticRequest, CapabilityAuthority, CapabilityAuthorityError,
    CapabilityIssueDecision, CapabilityRevocationDecision, CapabilityRights, CapabilityTarget,
    DelegateCapabilityRequest, IssueRootCapabilityRequest, RevokeCapabilityRequest,
    SignedDelegateCapabilityRequest, SignedIssueRootCapabilityRequest,
    SignedRevokeCapabilityRequest, delegate_command_message, issue_root_command_message,
    revoke_command_message,
};
use nlos_identity::{
    BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose, VerifySemanticSignatureRequest,
    semantic_signature_message,
};
use nlos_types::{IdempotencyKey, NamespaceId, SemanticEventId};
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
            "nlos-capability-{label}-{}-{nonce}-{}",
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

fn bootstrap(
    identity: &IdentityAuthority,
    seed: u8,
) -> (SigningKey, nlos_identity::IdentityBinding) {
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
    issuer: nlos_identity::IdentityBinding,
    holder: nlos_identity::IdentityBinding,
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
    delegator: nlos_identity::IdentityBinding,
    recipient: nlos_identity::IdentityBinding,
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

fn verified_signer(
    identity: &IdentityAuthority,
    key: &SigningKey,
    binding: nlos_identity::IdentityBinding,
    event_id: SemanticEventId,
    at_ms: u64,
) -> nlos_identity::VerifiedSemanticSigner {
    let signature = key.sign(&semantic_signature_message(event_id)).to_bytes();
    identity
        .verify_semantic_signature(VerifySemanticSignatureRequest {
            event_id,
            issuer: binding.principal_id,
            control_domain_id: binding.control_domain_id,
            key_id: binding.key_id,
            signature,
            admitted_at_ms: at_ms,
        })
        .unwrap()
}

#[test]
fn root_issue_is_authority_bound_durable_and_exactly_replayable() {
    let root = Root::new("root");
    let (request, expected_record, expected_receipt) = {
        let identity = IdentityAuthority::open(root.path()).unwrap();
        let (_, principal) = bootstrap(&identity, 10);
        let capability = CapabilityAuthority::open(root.path()).unwrap();
        let request = root_request(principal, principal, 0x70);
        let first = capability.issue_root(&identity, request).unwrap();
        assert!(matches!(first, CapabilityIssueDecision::Issued(_, _)));
        let replay = capability.issue_root(&identity, request).unwrap();
        assert!(matches!(replay, CapabilityIssueDecision::Replayed(_, _)));
        assert_eq!(first.record(), replay.record());
        assert_eq!(first.receipt(), replay.receipt());
        (request, first.record(), first.receipt())
    };

    let identity = IdentityAuthority::open(root.path()).unwrap();
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let replay = capability.issue_root(&identity, request).unwrap();
    assert_eq!(replay.record(), expected_record);
    assert_eq!(replay.receipt(), expected_receipt);
    assert_eq!(
        capability
            .inspect_active(expected_record.handle, 2_000)
            .unwrap(),
        expected_record
    );
}

#[test]
fn delegation_attenuates_every_mechanical_dimension() {
    let root = Root::new("attenuate");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (_, delegator) = bootstrap(&identity, 20);
    let (_, recipient) = bootstrap(&identity, 30);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let parent = capability
        .issue_root(&identity, root_request(delegator, delegator, 0x71))
        .unwrap()
        .record();
    let valid = delegate_request(parent, delegator, recipient, 0x72);
    let child = capability.delegate(&identity, valid).unwrap().record();
    assert_eq!(child.parent, Some(parent.handle));
    assert_eq!(child.holder, recipient.principal_id);
    assert!(child.rights.is_subset_of(parent.rights));

    let mut amplified = valid;
    amplified.idempotency_key = IdempotencyKey::from_bytes([0x73; 16]);
    amplified.rights = all_rights().union(CapabilityRights::SEMANTIC_ADJUDICATE);
    assert!(matches!(
        capability.delegate(&identity, amplified),
        Err(CapabilityAuthorityError::RightsAmplification)
    ));
    let mut scope = valid;
    scope.idempotency_key = IdempotencyKey::from_bytes([0x74; 16]);
    scope.target = CapabilityTarget::Namespace(NamespaceId::from_bytes([0xee; 16]));
    assert!(matches!(
        capability.delegate(&identity, scope),
        Err(CapabilityAuthorityError::ScopeAmplification)
    ));
    let mut validity = valid;
    validity.idempotency_key = IdempotencyKey::from_bytes([0x75; 16]);
    validity.valid_until_ms = 9_001;
    assert!(matches!(
        capability.delegate(&identity, validity),
        Err(CapabilityAuthorityError::ValidityAmplification)
    ));
    let mut limit = valid;
    limit.idempotency_key = IdempotencyKey::from_bytes([0x76; 16]);
    limit.call_limit = None;
    assert!(matches!(
        capability.delegate(&identity, limit),
        Err(CapabilityAuthorityError::CallLimitAmplification)
    ));
    let mut depth = valid;
    depth.idempotency_key = IdempotencyKey::from_bytes([0x77; 16]);
    depth.delegation_depth_remaining = 3;
    assert!(matches!(
        capability.delegate(&identity, depth),
        Err(CapabilityAuthorityError::DelegationDepthAmplification)
    ));
}

#[test]
fn semantic_authorization_requires_verified_holder_scope_right_and_purpose() {
    let root = Root::new("authorize");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (_, issuer) = bootstrap(&identity, 40);
    let (holder_key, holder) = bootstrap(&identity, 50);
    let (other_key, other) = bootstrap(&identity, 60);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let parent = capability
        .issue_root(&identity, root_request(issuer, issuer, 0x78))
        .unwrap()
        .record();
    let child = capability
        .delegate(&identity, delegate_request(parent, issuer, holder, 0x79))
        .unwrap()
        .record();
    let event_id = SemanticEventId::from_bytes([0x88; 32]);
    let signer = verified_signer(&identity, &holder_key, holder, event_id, 2_000);
    let request = AuthorizeSemanticRequest {
        handle: child.handle,
        signer,
        target: child.target,
        required_right: CapabilityRights::SEMANTIC_APPEND,
        purpose_digest: child.purpose_digest,
        admitted_at_ms: 2_000,
    };
    let authorization = capability.authorize_semantic(request).unwrap();
    assert_eq!(authorization.holder, holder.principal_id);

    let other_signer = verified_signer(&identity, &other_key, other, event_id, 2_000);
    assert!(matches!(
        capability.authorize_semantic(AuthorizeSemanticRequest {
            signer: other_signer,
            ..request
        }),
        Err(CapabilityAuthorityError::HolderMismatch)
    ));
    assert!(matches!(
        capability.authorize_semantic(AuthorizeSemanticRequest {
            required_right: CapabilityRights::SEMANTIC_RETRACT,
            ..request
        }),
        Err(CapabilityAuthorityError::RequiredRightMissing)
    ));
    assert!(matches!(
        capability.authorize_semantic(AuthorizeSemanticRequest {
            purpose_digest: Some([0xaa; 32]),
            ..request
        }),
        Err(CapabilityAuthorityError::PurposeMismatch)
    ));
}

#[test]
fn direct_revocation_is_replayable_and_generation_fenced() {
    let root = Root::new("revoke");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (_, issuer) = bootstrap(&identity, 70);
    let (_, holder) = bootstrap(&identity, 80);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let record = capability
        .issue_root(&identity, root_request(issuer, holder, 0x80))
        .unwrap()
        .record();
    let request = RevokeCapabilityRequest {
        handle: record.handle,
        revoker_key_id: holder.key_id,
        idempotency_key: IdempotencyKey::from_bytes([0x81; 16]),
        revoked_at_ms: 3_000,
    };
    let first = capability.revoke(&identity, request).unwrap();
    let replay = capability.revoke(&identity, request).unwrap();
    assert_eq!(first.receipt(), replay.receipt());
    assert_eq!(first.receipt().resulting_generation.get(), 2);
    assert!(matches!(
        capability.inspect_active(record.handle, 3_001),
        Err(CapabilityAuthorityError::GenerationFenceConflict)
    ));
    assert!(matches!(
        capability.revoke(
            &identity,
            RevokeCapabilityRequest {
                idempotency_key: IdempotencyKey::from_bytes([0x82; 16]),
                ..request
            }
        ),
        Err(CapabilityAuthorityError::GenerationFenceConflict)
    ));
}

#[test]
fn ancestor_revocation_invalidates_descendants_after_restart() {
    let root = Root::new("ancestor");
    let (child, parent, issuer_key_id) = {
        let identity = IdentityAuthority::open(root.path()).unwrap();
        let (_, issuer) = bootstrap(&identity, 90);
        let (_, holder) = bootstrap(&identity, 100);
        let capability = CapabilityAuthority::open(root.path()).unwrap();
        let parent = capability
            .issue_root(&identity, root_request(issuer, issuer, 0x83))
            .unwrap()
            .record();
        let child = capability
            .delegate(&identity, delegate_request(parent, issuer, holder, 0x84))
            .unwrap()
            .record();
        capability
            .revoke(
                &identity,
                RevokeCapabilityRequest {
                    handle: parent.handle,
                    revoker_key_id: issuer.key_id,
                    idempotency_key: IdempotencyKey::from_bytes([0x85; 16]),
                    revoked_at_ms: 3_000,
                },
            )
            .unwrap();
        (child, parent, issuer.key_id)
    };

    let identity = IdentityAuthority::open(root.path()).unwrap();
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    assert!(matches!(
        capability.inspect_active(child.handle, 3_001),
        Err(CapabilityAuthorityError::AncestorRevokedOrFenced)
    ));
    assert!(matches!(
        capability.revoke(
            &identity,
            RevokeCapabilityRequest {
                handle: parent.handle,
                revoker_key_id: issuer_key_id,
                idempotency_key: IdempotencyKey::from_bytes([0x86; 16]),
                revoked_at_ms: 3_100,
            }
        ),
        Err(CapabilityAuthorityError::GenerationFenceConflict)
    ));
}

#[test]
fn capability_descriptors_versions_and_receipts_are_ddl_protected() {
    let root = Root::new("immutable");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (_, principal) = bootstrap(&identity, 110);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let record = capability
        .issue_root(&identity, root_request(principal, principal, 0x87))
        .unwrap()
        .record();
    drop(capability);

    let raw = Connection::open(root.path().join("capability-authority.db")).unwrap();
    assert!(
        raw.execute(
            "UPDATE capability_heads SET rights=1 WHERE capability_id=?1",
            [record.handle.capability_id.as_bytes().as_slice()]
        )
        .is_err()
    );
    assert!(raw.execute("DELETE FROM capability_versions", []).is_err());
    assert!(
        raw.execute("DELETE FROM capability_issue_receipts", [])
            .is_err()
    );
}

/// The deprecated unsigned issue entry and the signed entry are two fronts
/// of one durable authority: with the identical command, whichever entry
/// executes first, the other replays the exact same durable capability.
#[test]
fn deprecated_unsigned_issue_replays_through_signed_entry() {
    // Direction 1: the unsigned entry issues; the signed entry replays it.
    let root = Root::new("equivalence-issue");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (_, issuer) = bootstrap(&identity, 240);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let command = root_request(issuer, issuer, 0x88);
    let unsigned = capability.issue_root(&identity, command).unwrap();
    assert!(matches!(unsigned, CapabilityIssueDecision::Issued(_, _)));
    assert!(matches!(
        capability.issue_root_signed(
            &identity,
            SignedIssueRootCapabilityRequest {
                command,
                signer: issuer.principal_id,
                signature: signing_key(240)
                    .sign(&issue_root_command_message(command))
                    .to_bytes(),
            },
        ),
        Ok(CapabilityIssueDecision::Replayed(record, _)) if record == unsigned.record(),
    ));

    // Direction 2: the signed entry issues; the unsigned entry replays it.
    let root = Root::new("equivalence-issue-reverse");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (_, issuer) = bootstrap(&identity, 244);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let command = root_request(issuer, issuer, 0x90);
    let signed = capability
        .issue_root_signed(
            &identity,
            SignedIssueRootCapabilityRequest {
                command,
                signer: issuer.principal_id,
                signature: signing_key(244)
                    .sign(&issue_root_command_message(command))
                    .to_bytes(),
            },
        )
        .unwrap();
    assert!(matches!(signed, CapabilityIssueDecision::Issued(_, _)));
    assert!(matches!(
        capability.issue_root(&identity, command),
        Ok(CapabilityIssueDecision::Replayed(record, _)) if record == signed.record(),
    ));
}

/// The deprecated unsigned delegate/revoke entries stay replay-equivalent to
/// the signed entries for identical commands.
#[test]
fn deprecated_unsigned_delegate_and_revoke_replay_through_signed_entries() {
    // Delegate, direction 1: the unsigned entry issues; the signed entry
    // replays the exact same durable child capability.
    let root = Root::new("equivalence-delegate");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (_, delegator) = bootstrap(&identity, 245);
    let (_, recipient) = bootstrap(&identity, 246);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let parent = capability
        .issue_root(&identity, root_request(delegator, delegator, 0x91))
        .unwrap()
        .record();
    let command = delegate_request(parent, delegator, recipient, 0x92);
    let unsigned = capability.delegate(&identity, command).unwrap();
    assert!(matches!(unsigned, CapabilityIssueDecision::Issued(_, _)));
    assert!(matches!(
        capability.delegate_signed(
            &identity,
            SignedDelegateCapabilityRequest {
                command,
                signer: delegator.principal_id,
                signature: signing_key(245)
                    .sign(&delegate_command_message(command))
                    .to_bytes(),
            },
        ),
        Ok(CapabilityIssueDecision::Replayed(record, _)) if record == unsigned.record(),
    ));

    // Revoke, direction 2: the signed entry revokes; the unsigned entry
    // replays the exact same durable revocation receipt.
    let root = Root::new("equivalence-revoke");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (_, holder) = bootstrap(&identity, 247);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let record = capability
        .issue_root(&identity, root_request(holder, holder, 0x93))
        .unwrap()
        .record();
    let command = RevokeCapabilityRequest {
        handle: record.handle,
        revoker_key_id: holder.key_id,
        idempotency_key: IdempotencyKey::from_bytes([0x94; 16]),
        revoked_at_ms: 3_000,
    };
    let signed = capability
        .revoke_signed(
            &identity,
            SignedRevokeCapabilityRequest {
                command,
                signer: holder.principal_id,
                signature: signing_key(247)
                    .sign(&revoke_command_message(command))
                    .to_bytes(),
            },
        )
        .unwrap();
    assert!(matches!(signed, CapabilityRevocationDecision::Revoked(_)));
    assert!(matches!(
        capability.revoke(&identity, command),
        Ok(CapabilityRevocationDecision::Replayed(receipt)) if receipt == signed.receipt(),
    ));
}
