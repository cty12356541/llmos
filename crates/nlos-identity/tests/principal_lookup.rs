use ed25519_dalek::SigningKey;
use nlos_identity::{
    BootstrapPrincipalRequest, IdentityAuthority, IdentityAuthorityError, KeyPurpose,
    RevokeKeyRequest, RotateKeyRequest,
};
use nlos_types::{IdempotencyKey, PrincipalId};

#[test]
fn principal_lookup_returns_latest_durable_binding_even_when_revoked() {
    let root = std::env::temp_dir().join(format!("nlos-id-principal-{}", std::process::id()));
    let authority = IdentityAuthority::open(&root).unwrap();
    let unknown = PrincipalId::from_bytes([0; 16]);
    assert!(matches!(
        authority.inspect_current_principal_binding(unknown),
        Err(IdentityAuthorityError::PrincipalNotFound(id)) if id == unknown
    ));
    let first = authority
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [1; 32],
            control_domain_policy_digest: [2; 32],
            public_key: SigningKey::from_bytes(&[3; 32]).verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 1,
            key_valid_until_ms: 50,
            idempotency_key: IdempotencyKey::from_bytes([4; 16]),
            created_at_ms: 1,
        })
        .unwrap()
        .binding();
    assert_eq!(
        authority
            .inspect_current_principal_binding(first.principal_id)
            .unwrap(),
        first
    );
    authority
        .rotate_key(RotateKeyRequest {
            key_id: first.key_id,
            expected_key_generation: first.key_generation,
            expected_identity_snapshot_id: first.identity_snapshot_id,
            new_public_key: SigningKey::from_bytes(&[5; 32]).verifying_key().to_bytes(),
            new_valid_from_ms: 2,
            new_valid_until_ms: 80,
            idempotency_key: IdempotencyKey::from_bytes([6; 16]),
            rotated_at_ms: 2,
        })
        .unwrap();
    let rotated = authority
        .inspect_current_principal_binding(first.principal_id)
        .unwrap();
    assert_eq!(rotated.key_generation.get(), first.key_generation.get() + 1);
    assert_eq!(
        authority.inspect_current_binding(first.key_id).unwrap(),
        rotated
    );
    authority
        .revoke_key(RevokeKeyRequest {
            key_id: rotated.key_id,
            expected_key_generation: rotated.key_generation,
            expected_identity_snapshot_id: rotated.identity_snapshot_id,
            idempotency_key: IdempotencyKey::from_bytes([7; 16]),
            revoked_at_ms: 3,
        })
        .unwrap();
    let revoked = authority
        .inspect_current_principal_binding(first.principal_id)
        .unwrap();
    assert_eq!(revoked.key_revoked_at_ms, Some(3));
    assert_eq!(
        authority.inspect_current_binding(first.key_id).unwrap(),
        revoked
    );
    drop(authority);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn duplicate_principal_heads_fail_closed_for_lookup_and_signature() {
    use ed25519_dalek::Signer;
    use nlos_identity::VerifyCapabilityCommandSignatureRequest;

    let root = std::env::temp_dir().join(format!("nlos-id-duplicate-{}", std::process::id()));
    let authority = IdentityAuthority::open(&root).unwrap();
    let signer = SigningKey::from_bytes(&[31; 32]);
    let binding = authority
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [32; 32],
            control_domain_policy_digest: [33; 32],
            public_key: signer.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 1,
            key_valid_until_ms: 50,
            idempotency_key: IdempotencyKey::from_bytes([34; 16]),
            created_at_ms: 1,
        })
        .unwrap()
        .binding();
    let connection = rusqlite::Connection::open(root.join("identity-authority.db")).unwrap();
    connection
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    connection.execute(
        "INSERT INTO key_heads (key_id, principal_id, control_domain_id, current_generation, created_at_ms)
         VALUES (?1,?2,?3,1,1)",
        rusqlite::params![[35_u8; 16], binding.principal_id.as_bytes(), binding.control_domain_id.as_bytes()],
    ).unwrap();
    let message_digest = [36; 32];
    let lookup = authority.inspect_current_principal_binding(binding.principal_id);
    let verification =
        authority.verify_capability_command_signature(VerifyCapabilityCommandSignatureRequest {
            message_digest,
            principal: binding.principal_id,
            signature: signer.sign(&message_digest).to_bytes(),
            verified_at_ms: 2,
        });
    drop(connection);
    drop(authority);
    std::fs::remove_dir_all(root).unwrap();
    assert!(
        matches!(
            lookup,
            Err(IdentityAuthorityError::CorruptRecord(
                "principal key binding"
            ))
        ),
        "ambiguous principal was not rejected as corrupt during lookup"
    );
    assert!(
        matches!(
            verification,
            Err(IdentityAuthorityError::CorruptRecord(
                "principal key binding"
            ))
        ),
        "ambiguous principal was not rejected as corrupt during verification"
    );
}
