#![allow(deprecated)] // Deprecated unsigned Capability entries stay the test front for issuing capabilities.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_capability::{
    AuthorizeSemanticRequest, CapabilityAuthority, CapabilityAuthorityError, CapabilityRights,
    CapabilityTarget, ConsumeCapabilityRequest, DelegateCapabilityRequest,
    IssueRootCapabilityRequest,
};
use nlos_identity::{
    BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose,
    VerifySemanticSignatureRequest, semantic_signature_message,
};
use nlos_types::{IdempotencyKey, NamespaceId, SemanticEventId, TaskId};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-capability-ns-{label}-{}-{nonce}-{}",
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

fn namespace_id(first: u8, second: u8) -> NamespaceId {
    NamespaceId::from_bytes([first, second, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
}

fn all_rights() -> CapabilityRights {
    CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::DELEGATE)
}

fn root_namespace_request(
    issuer: IdentityBinding,
    holder: IdentityBinding,
    namespace: NamespaceId,
    seed: u8,
) -> IssueRootCapabilityRequest {
    IssueRootCapabilityRequest {
        issuer_key_id: issuer.key_id,
        holder_key_id: holder.key_id,
        target: CapabilityTarget::Namespace(namespace),
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

fn root_task_request(
    issuer: IdentityBinding,
    holder: IdentityBinding,
    task: TaskId,
    seed: u8,
) -> IssueRootCapabilityRequest {
    IssueRootCapabilityRequest {
        issuer_key_id: issuer.key_id,
        holder_key_id: holder.key_id,
        target: CapabilityTarget::Task(task),
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

fn delegate_to(
    parent: nlos_capability::CapabilityRecord,
    delegator: IdentityBinding,
    recipient: IdentityBinding,
    target: CapabilityTarget,
    seed: u8,
) -> DelegateCapabilityRequest {
    DelegateCapabilityRequest {
        parent: parent.handle,
        delegator_key_id: delegator.key_id,
        recipient_key_id: recipient.key_id,
        target,
        rights: CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::DELEGATE),
        purpose_digest: None,
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
    binding: IdentityBinding,
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
fn delegate_narrows_namespace_target_within_parent_prefix() {
    let root = Root::new("narrow-delegate");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (_, delegator) = bootstrap(&identity, 10);
    let (_, recipient) = bootstrap(&identity, 20);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let parent_ns = namespace_id(0x44, 0x00);
    let child_ns = namespace_id(0x44, 0x55);
    let parent = capability
        .issue_root(
            &identity,
            root_namespace_request(delegator, delegator, parent_ns, 0x30),
        )
        .unwrap()
        .record();
    let child = capability
        .delegate(
            &identity,
            delegate_to(
                parent,
                delegator,
                recipient,
                CapabilityTarget::Namespace(child_ns),
                0x31,
            ),
        )
        .unwrap()
        .record();
    assert_eq!(child.target, CapabilityTarget::Namespace(child_ns));
    assert_eq!(child.parent, Some(parent.handle));
}

#[test]
fn delegate_rejects_namespace_scope_amplification() {
    let root = Root::new("amplify-delegate");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (_, delegator) = bootstrap(&identity, 11);
    let (_, recipient) = bootstrap(&identity, 21);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let parent_ns = namespace_id(0x44, 0x00);
    let wider_ns = namespace_id(0x45, 0x00);
    let parent = capability
        .issue_root(
            &identity,
            root_namespace_request(delegator, delegator, parent_ns, 0x32),
        )
        .unwrap()
        .record();
    assert!(matches!(
        capability.delegate(
            &identity,
            delegate_to(
                parent,
                delegator,
                recipient,
                CapabilityTarget::Namespace(wider_ns),
                0x33,
            ),
        ),
        Err(CapabilityAuthorityError::ScopeAmplification)
    ));
}

#[test]
fn authorize_and_consume_accept_requested_target_within_capability_subtree() {
    let root = Root::new("subtree-admit");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (holder_key, holder) = bootstrap(&identity, 12);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let parent_ns = namespace_id(0x44, 0x00);
    let child_ns = namespace_id(0x44, 0x66);
    let record = capability
        .issue_root(
            &identity,
            root_namespace_request(holder, holder, parent_ns, 0x34),
        )
        .unwrap()
        .record();
    let event_id = SemanticEventId::from_bytes([0x99; 32]);
    let signer = verified_signer(&identity, &holder_key, holder, event_id, 2_000);
    let authorize = AuthorizeSemanticRequest {
        handle: record.handle,
        signer,
        target: CapabilityTarget::Namespace(child_ns),
        required_right: CapabilityRights::SEMANTIC_APPEND,
        purpose_digest: None,
        admitted_at_ms: 2_000,
    };
    capability.authorize_semantic(authorize).unwrap();

    let consume = ConsumeCapabilityRequest {
        handle: record.handle,
        signer,
        target: CapabilityTarget::Namespace(child_ns),
        required_right: CapabilityRights::SEMANTIC_APPEND,
        purpose_digest: None,
        idempotency_key: IdempotencyKey::from_bytes([0x35; 16]),
        consumed_at_ms: 2_000,
    };
    capability.consume(consume).unwrap();

    assert!(matches!(
        capability.authorize_semantic(AuthorizeSemanticRequest {
            target: CapabilityTarget::Namespace(namespace_id(0x45, 0x00)),
            ..authorize
        }),
        Err(CapabilityAuthorityError::TargetMismatch)
    ));
}

#[test]
fn narrowed_delegate_replays_across_restart() {
    let root = Root::new("narrow-replay");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (_, delegator) = bootstrap(&identity, 13);
    let (_, recipient) = bootstrap(&identity, 23);
    let parent_ns = namespace_id(0x44, 0x00);
    let child_ns = namespace_id(0x44, 0x77);
    let parent = {
        let capability = CapabilityAuthority::open(root.path()).unwrap();
        capability
            .issue_root(
                &identity,
                root_namespace_request(delegator, delegator, parent_ns, 0x36),
            )
            .unwrap()
            .record()
    };
    let request = delegate_to(
        parent,
        delegator,
        recipient,
        CapabilityTarget::Namespace(child_ns),
        0x37,
    );
    let first = {
        let capability = CapabilityAuthority::open(root.path()).unwrap();
        capability.delegate(&identity, request).unwrap()
    };
    drop(identity);
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let replay = capability.delegate(&identity, request).unwrap();
    assert_eq!(first.record(), replay.record());
    assert_eq!(first.receipt(), replay.receipt());
}

#[test]
fn task_target_still_requires_exact_match() {
    let root = Root::new("task-exact");
    let identity = IdentityAuthority::open(root.path()).unwrap();
    let (_, delegator) = bootstrap(&identity, 14);
    let (recipient_key, recipient) = bootstrap(&identity, 24);
    let capability = CapabilityAuthority::open(root.path()).unwrap();
    let task_a = TaskId::from_bytes([0xaa; 16]);
    let task_b = TaskId::from_bytes([0xbb; 16]);
    let parent = capability
        .issue_root(
            &identity,
            root_task_request(delegator, delegator, task_a, 0x38),
        )
        .unwrap()
        .record();
    assert!(matches!(
        capability.delegate(
            &identity,
            delegate_to(
                parent,
                delegator,
                recipient,
                CapabilityTarget::Task(task_b),
                0x39,
            ),
        ),
        Err(CapabilityAuthorityError::ScopeAmplification)
    ));
    let exact = capability
        .delegate(
            &identity,
            delegate_to(
                parent,
                delegator,
                recipient,
                CapabilityTarget::Task(task_a),
                0x3a,
            ),
        )
        .unwrap()
        .record();
    let event_id = SemanticEventId::from_bytes([0xab; 32]);
    let signer = verified_signer(&identity, &recipient_key, recipient, event_id, 2_000);
    assert!(matches!(
        capability.authorize_semantic(AuthorizeSemanticRequest {
            handle: exact.handle,
            signer,
            target: CapabilityTarget::Task(task_b),
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: None,
            admitted_at_ms: 2_000,
        }),
        Err(CapabilityAuthorityError::TargetMismatch)
    ));
    capability
        .authorize_semantic(AuthorizeSemanticRequest {
            handle: exact.handle,
            signer,
            target: CapabilityTarget::Task(task_a),
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: None,
            admitted_at_ms: 2_000,
        })
        .unwrap();
}
