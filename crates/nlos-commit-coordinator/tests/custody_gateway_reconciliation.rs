#![allow(clippy::too_many_lines)] // End-to-end cross-authority integration scenarios stay linear per test.
//! End-to-end integration tests for `#17 C-CUSTODY-GATEWAY`:
//! `SigningKeyCustodyVault` + `EnforcementGatewayReconciliationAuthority` +
//! bounded `R-B` settlement across `ResourceAuthority` and `SqliteTaskAuthority`
//! (ADR-0017 Constraint 4 & Review Trigger 2, ADR-0022, `v0.5 §10.2`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_artifact::ArtifactStore;
use nlos_capability::custody::{
    BootstrapCustodyPrincipalRequest, CustodyKeyState, CustodySignRequest, CustodyTier,
    CustodyVaultError, IdentityAuthority, IdentityBinding, KeyPurpose, RevokeCompromisedKeyRequest,
    RotateCustodyKeyEpochRequest, SigningKeyCustodyVault,
};
use nlos_capability::{
    CapabilityAuthority, CapabilityRights, CapabilityTarget, IssueRootCapabilityRequest,
};
use nlos_commit_coordinator::{
    ApplyGatewayReconciliationContext, EnforcementGatewayReconciliationAuthority,
    GatewayOutcomeDisposition, GatewayReconcileError, GatewayReconciliationDecision,
    GatewayReconciliationLedger, IssueGatewayAttestationRequest, ReceiptFinality,
    verify_and_apply_gateway_reconciliation,
};
use nlos_resource::{
    AccountRecord, ActivateReservationRequest, ConsumeReservationRequest, CreateAccountRequest,
    CreateQuoteRequest, DriverRecord, QuarantineReservationRequest, QuoteRecord,
    RegisterDriverRequest, ReservationRecord, ReservationState, ReserveRequest, ResourceAuthority,
    ResourceDemand,
};
use nlos_task::{
    AttemptSpec, Authorities, ParticipantRegistryBinding, PermitDecision, PermitRecord,
    PermitRequest, PrepareResourceFinalizeRequest, ResourceCommitPlanId, ResourceCommitPlanState,
    ResourceConvergeDecision, SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority,
    TaskSnapshotReceiptSpec, TaskSpec, TaskWriteSetRequest, TaskWriteSetResourceReservationRequest,
    empty_effect_history_root,
};
use nlos_types::{
    CallId, CancellationScopeId, Generation, IdempotencyKey, KeyId, NamespaceId, OperationId,
    ReceiptId, TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nlos-c-custody-gw-{label}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp root");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct GatewayRig {
    identity: IdentityAuthority,
    custody: SigningKeyCustodyVault,
    gateway_id: [u8; 16],
    gateway: EnforcementGatewayReconciliationAuthority,
    binding: IdentityBinding,
}

impl GatewayRig {
    fn bootstrap(
        root: &Path,
        kek: [u8; 32],
        gateway_id: [u8; 16],
        seed: [u8; 32],
        tier: CustodyTier,
        valid_from_ms: u64,
        valid_until_ms: u64,
    ) -> Self {
        let identity = IdentityAuthority::open(root).expect("open identity authority");
        let custody =
            SigningKeyCustodyVault::open_durable(root, kek).expect("open durable custody vault");
        let (slot, binding) = custody
            .bootstrap_principal_under_custody(
                &identity,
                BootstrapCustodyPrincipalRequest {
                    authority_id: gateway_id,
                    principal_profile_digest: [seed[0] ^ 0x11; 32],
                    control_domain_policy_digest: [seed[0] ^ 0x22; 32],
                    key_purpose: KeyPurpose::SemanticSigning,
                    custody_tier: tier,
                    valid_from_ms,
                    valid_until_ms,
                    seed,
                    bootstrap_idempotency_key: IdempotencyKey::from_bytes([seed[0] ^ 0x33; 16]),
                    custody_binding_idempotency_key: IdempotencyKey::from_bytes(
                        [seed[0] ^ 0x44; 16],
                    ),
                    provisioned_at_ms: valid_from_ms,
                },
            )
            .expect("bootstrap gateway principal under custody");
        let gateway = EnforcementGatewayReconciliationAuthority::new(
            gateway_id,
            binding.principal_id,
            binding.control_domain_id,
            slot.key_id,
        );
        Self {
            identity,
            custody,
            gateway_id,
            gateway,
            binding,
        }
    }
}

struct ResourceTaskRig {
    resource: ResourceAuthority,
    task: SqliteTaskAuthority,
    driver: DriverRecord,
    account: AccountRecord,
}

impl ResourceTaskRig {
    fn new(root: &Path, seed: u8, initial_credit: u64) -> Self {
        let resource = ResourceAuthority::open(root).expect("open resource authority");
        let driver = resource
            .register_driver(RegisterDriverRequest {
                profile_digest: [seed; 32],
                idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x01; 16]),
                created_at_ms: 1_000,
            })
            .expect("register driver")
            .record();
        let account = resource
            .create_account(CreateAccountRequest {
                initial_credit,
                idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x02; 16]),
                created_at_ms: 1_000,
            })
            .expect("create account");
        let task =
            SqliteTaskAuthority::open(root.join("task.sqlite3")).expect("open task authority");
        Self {
            resource,
            task,
            driver,
            account,
        }
    }

    fn reserve_and_prepare_task_plan(
        &self,
        root: &Path,
        seed: u8,
        upper_bound: u64,
    ) -> (ReservationRecord, PermitRecord, ResourceCommitPlanId) {
        let quote: QuoteRecord = self
            .resource
            .create_quote(CreateQuoteRequest {
                driver_id: self.driver.driver_id,
                driver_generation: self.driver.generation,
                driver_fencing_token: self.driver.fencing_token,
                operation_proposal_digest: [seed ^ 0x10; 32],
                pricing_version: [seed ^ 0x11; 32],
                upper_bound,
                demand_capacity: ResourceDemand::default(),
                valid_until_ms: 20_000,
                idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x12; 16]),
                created_at_ms: 1_000,
            })
            .expect("create quote")
            .record();
        let reservation = self
            .resource
            .reserve(ReserveRequest {
                account_id: self.account.account_id,
                quote_id: quote.quote_id,
                call_id: CallId::from_bytes([seed ^ 0x13; 16]),
                operation_id: OperationId::from_bytes([seed ^ 0x14; 16]),
                idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x15; 16]),
                demand: ResourceDemand::default(),
                reserved_at_ms: 1_050,
            })
            .expect("reserve")
            .record();

        let task_id = TaskId::from_bytes([seed ^ 0x20; 16]);
        let attempt_id = TaskAttemptId::from_bytes([seed ^ 0x21; 16]);
        self.task
            .register_task(TaskSpec {
                application_id: None,
                plan_revision: None,
                task_id,
                task_generation: Generation::INITIAL,
                registered_at_ms: 1_000,
            })
            .expect("register task");
        let spec = AttemptSpec {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            snapshot: SnapshotBundle {
                snapshot_id: TaskSnapshotId::from_bytes([seed ^ 0x22; 16]),
                snapshot_digest: [seed ^ 0x23; 32],
                expected_head_commit_seq: 0,
                effect_history_root: empty_effect_history_root(),
                retry_fence_epoch: 0,
            },
            cancellation_scope_id: CancellationScopeId::from_bytes([seed ^ 0x24; 16]),
            cancellation_generation: Generation::INITIAL,
            idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x25; 16]),
            registered_at_ms: 1_010,
        };
        let snap_receipt_id = ReceiptId::from_bytes([seed ^ 0x26; 16]);
        self.task
            .register_snapshot_receipt(TaskSnapshotReceiptSpec {
                task_id,
                snapshot: spec.snapshot,
                receipt_id: snap_receipt_id,
                builder_id: [seed ^ 0x27; 16],
                builder_version_digest: [seed ^ 0x28; 32],
                per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes([seed ^ 0x29; 16])],
                dependency_closure_root: [seed ^ 0x2a; 32],
                semantic_resolver_digest: [seed ^ 0x2b; 32],
                canonical_iteration_digest: [seed ^ 0x2c; 32],
                achieved_consistency: SnapshotConsistency::Causal,
                built_at_ms: 1_005,
                authority_id: [seed ^ 0x2d; 16],
                key_id: [seed ^ 0x2e; 16],
                signature: [seed ^ 0x2f; 64],
            })
            .expect("register snapshot receipt");
        self.task
            .register_attempt_with_snapshot_receipt(spec, snap_receipt_id)
            .expect("register attempt");

        let reg0 = self
            .task
            .inspect_participant_registry(task_id)
            .expect("inspect participant registry");
        let reg1 = self
            .task
            .register_driver_gateway_participant(
                &self.resource,
                task_id,
                ParticipantRegistryBinding {
                    generation: reg0.generation,
                    root: reg0.root,
                },
                self.driver.driver_id,
                self.driver.generation,
                1_100,
            )
            .expect("register driver participant");
        self.task
            .register_resource_ledger_participant(
                &self.resource,
                task_id,
                ParticipantRegistryBinding {
                    generation: reg1.registry().generation,
                    root: reg1.registry().root,
                },
                self.account.account_id,
                Generation::INITIAL,
                1_110,
            )
            .expect("register ledger participant");

        let artifact =
            ArtifactStore::open(root.join(format!("artifact-{seed}"))).expect("artifact store");
        let authorities = Authorities {
            artifact: Some(&artifact),
            resource: Some(&self.resource),
            ..Authorities::default()
        };
        let write_set = self
            .task
            .seal_task_write_set_with_authorities_struct(
                authorities,
                TaskWriteSetRequest {
                    task_id,
                    attempt_id,
                    attempt_generation: Generation::INITIAL,
                    artifact_reads: Vec::new(),
                    artifact_writes: Vec::new(),
                    process_binding: None,
                    semantic_reads: Vec::new(),
                    semantic_appends: Vec::new(),
                    resource_reservations: vec![TaskWriteSetResourceReservationRequest {
                        reservation_id: reservation.reservation_id,
                        expected_call_id: reservation.call_id,
                        expected_operation_id: reservation.operation_id,
                        expected_quote_id: reservation.quote_id,
                    }],
                    planned_effects: Vec::new(),
                    effect_endpoints: Vec::new(),
                    idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x30; 16]),
                    sealed_at_ms: 1_200,
                },
            )
            .expect("seal write set")
            .record()
            .clone();
        let PermitDecision::Issued(permit) = self
            .task
            .request_commit_permit_with_authorities_struct(
                authorities,
                PermitRequest {
                    task_id,
                    attempt_id,
                    attempt_generation: Generation::INITIAL,
                    write_set_root: write_set.write_set_root,
                    planned_effects: Vec::new(),
                    idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x31; 16]),
                    valid_until_ms: 20_000,
                    requested_at_ms: 1_250,
                },
            )
            .expect("issue permit")
        else {
            panic!("expected fresh permit issuance");
        };
        let envelope = self
            .task
            .prepare_resource_finalize(PrepareResourceFinalizeRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                permit_id: permit.permit_id,
                idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x32; 16]),
                required_satisfaction: Vec::new(),
                fenced_participant_digest: [0u8; 32],
                prepared_at_ms: 1_300,
            })
            .expect("prepare resource finalize");
        (reservation, *permit, envelope.record().plan_id)
    }

    fn activate_consume_and_quarantine(
        &self,
        reservation: &ReservationRecord,
        consumptions: &[(u64, u64)],
        quarantine: bool,
    ) {
        let activation = self
            .resource
            .activate(ActivateReservationRequest {
                reservation_id: reservation.reservation_id,
                call_id: reservation.call_id,
                operation_id: reservation.operation_id,
                driver_id: reservation.driver_id,
                driver_generation: reservation.driver_generation,
                driver_fencing_token: reservation.driver_fencing_token,
                activation_token: reservation.activation_token,
                activated_at_ms: 1_400,
            })
            .expect("activate reservation")
            .receipt();
        for (idx, (seq, usage)) in consumptions.iter().enumerate() {
            let step_ms = 1_450 + u64::try_from(idx).expect("idx fits u64") * 10;
            self.resource
                .consume(ConsumeReservationRequest {
                    reservation_id: reservation.reservation_id,
                    operation_id: reservation.operation_id,
                    activation_receipt_id: activation.receipt_id,
                    sequence: *seq,
                    cumulative_usage: *usage,
                    consumed_at_ms: step_ms,
                })
                .expect("consume reservation");
        }
        if quarantine {
            self.resource
                .quarantine(QuarantineReservationRequest {
                    reservation_id: reservation.reservation_id,
                    operation_id: reservation.operation_id,
                    activation_receipt_id: activation.receipt_id,
                    reason_digest: [0xee; 32],
                    quarantined_at_ms: 1_550,
                })
                .expect("quarantine reservation");
        }
    }
}

#[test]
fn custody_vault_durable_sqlite_wal_full_reopen_kek_commitment_and_schema_isolation() {
    let root = TempRoot::new("durable-wal-kek");
    let kek = [0xa5; 32];
    let raw_seed = [0x4c; 32];

    let key_id = {
        let _cap = CapabilityAuthority::open(root.path()).expect("open capability authority");
        let rig = GatewayRig::bootstrap(
            root.path(),
            kek,
            [0x71; 16],
            raw_seed,
            CustodyTier::HardwareHsmAttested,
            1_000,
            10_000,
        );
        let audit = rig
            .custody
            .sign_digest_under_custody(CustodySignRequest {
                key_id: rig.gateway.key_id(),
                expected_rotation_epoch: Some(1),
                domain_tag: b"test/durable/v1",
                message_digest: [0x11; 32],
                signed_at_ms: 1_200,
            })
            .expect("sign under custody");
        assert_eq!(audit.sign_seq, 1);
        rig.gateway.key_id()
    };

    // CapabilityAuthority SQLite schema version MUST remain untouched at 2, while
    // signing-key-custody.db uses WAL + FULL and never stores plaintext seed bytes.
    let cap_conn =
        Connection::open(root.path().join("capability-authority.db")).expect("open cap db");
    let cap_ver: i64 = cap_conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .expect("cap user_version");
    assert_eq!(cap_ver, 2);

    let custody_conn =
        Connection::open(root.path().join("signing-key-custody.db")).expect("open custody db");
    let journal_mode: String = custody_conn
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .expect("journal_mode");
    assert!(journal_mode.eq_ignore_ascii_case("wal"));
    let wrapped_seed: Vec<u8> = custody_conn
        .query_row(
            "SELECT wrapped_seed FROM custody_key_slots WHERE key_id = ?1",
            [key_id.as_bytes().as_slice()],
            |r| r.get(0),
        )
        .expect("read wrapped_seed");
    assert_ne!(wrapped_seed.as_slice(), raw_seed.as_slice());
    drop(custody_conn);
    drop(cap_conn);

    // Reopen with wrong KEK must fail closed.
    let wrong_kek = SigningKeyCustodyVault::open_durable(root.path(), [0xb6; 32]);
    assert!(matches!(
        wrong_kek,
        Err(CustodyVaultError::WrappingKeyMismatch)
    ));

    // Reopen with matching KEK recovers durable slots and monotonic sign_seq.
    let reopened =
        SigningKeyCustodyVault::open_durable(root.path(), kek).expect("reopen with matching KEK");
    assert_eq!(reopened.current_sign_seq().expect("current_sign_seq"), 1);
    let slot = reopened
        .inspect_key_slot_epoch(key_id, 1)
        .expect("inspect slot after reopen");
    assert_eq!(slot.custody_tier, CustodyTier::HardwareHsmAttested);
    assert_eq!(slot.state, CustodyKeyState::Active);
}

#[test]
fn custody_signing_emits_strictly_monotonic_audit_seq_and_adr0022_root_capability() {
    let root = TempRoot::new("monotonic-audit-adr0022");
    let kek = [0x33; 32];
    let identity = IdentityAuthority::open(root.path()).expect("identity");
    let capability = CapabilityAuthority::open(root.path()).expect("capability");

    let (operator_key_id, gateway_key_id) = {
        let vault = SigningKeyCustodyVault::open_durable(root.path(), kek).expect("open vault");
        let (op_slot, _) = vault
            .bootstrap_principal_under_custody(
                &identity,
                BootstrapCustodyPrincipalRequest {
                    authority_id: [0x81; 16],
                    principal_profile_digest: [0x11; 32],
                    control_domain_policy_digest: [0x12; 32],
                    key_purpose: KeyPurpose::SemanticSigning,
                    custody_tier: CustodyTier::OperatorFileSealed,
                    valid_from_ms: 1_000,
                    valid_until_ms: 15_000,
                    seed: [0x51; 32],
                    bootstrap_idempotency_key: IdempotencyKey::from_bytes([0x61; 16]),
                    custody_binding_idempotency_key: IdempotencyKey::from_bytes([0x62; 16]),
                    provisioned_at_ms: 1_000,
                },
            )
            .expect("bootstrap operator");
        let (gw_slot, _) = vault
            .bootstrap_principal_under_custody(
                &identity,
                BootstrapCustodyPrincipalRequest {
                    authority_id: [0x82; 16],
                    principal_profile_digest: [0x21; 32],
                    control_domain_policy_digest: [0x22; 32],
                    key_purpose: KeyPurpose::SemanticSigning,
                    custody_tier: CustodyTier::PlatformKeychainSealed,
                    valid_from_ms: 1_000,
                    valid_until_ms: 15_000,
                    seed: [0x52; 32],
                    bootstrap_idempotency_key: IdempotencyKey::from_bytes([0x63; 16]),
                    custody_binding_idempotency_key: IdempotencyKey::from_bytes([0x64; 16]),
                    provisioned_at_ms: 1_000,
                },
            )
            .expect("bootstrap gateway");

        let (_, root_audit) = vault
            .issue_root_capability_under_custody(
                &capability,
                &identity,
                IssueRootCapabilityRequest {
                    issuer_key_id: op_slot.key_id,
                    holder_key_id: gw_slot.key_id,
                    target: CapabilityTarget::Namespace(NamespaceId::from_bytes([0x99; 16])),
                    rights: CapabilityRights::SEMANTIC_APPEND,
                    purpose_digest: None,
                    valid_from_ms: 1_100,
                    valid_until_ms: 12_000,
                    delegation_depth_remaining: 0,
                    call_limit: Some(8),
                    idempotency_key: IdempotencyKey::from_bytes([0x65; 16]),
                    issued_at_ms: 1_150,
                },
            )
            .expect("issue ADR-0022 root capability");
        assert_eq!(root_audit.sign_seq, 1);

        let second = vault
            .sign_digest_under_custody(CustodySignRequest {
                key_id: gw_slot.key_id,
                expected_rotation_epoch: None,
                domain_tag: b"test/seq/v1",
                message_digest: [0xa1; 32],
                signed_at_ms: 1_200,
            })
            .expect("second sign");
        assert_eq!(second.sign_seq, 2);
        (op_slot.key_id, gw_slot.key_id)
    };

    let reopened = SigningKeyCustodyVault::open_durable(root.path(), kek).expect("reopen vault");
    let third = reopened
        .sign_digest_under_custody(CustodySignRequest {
            key_id: gateway_key_id,
            expected_rotation_epoch: Some(1),
            domain_tag: b"test/seq/v1",
            message_digest: [0xa2; 32],
            signed_at_ms: 1_300,
        })
        .expect("third sign after reopen");
    let fourth = reopened
        .sign_digest_under_custody(CustodySignRequest {
            key_id: operator_key_id,
            expected_rotation_epoch: Some(1),
            domain_tag: b"test/seq/v1",
            message_digest: [0xa3; 32],
            signed_at_ms: 1_350,
        })
        .expect("fourth sign after reopen");
    assert_eq!(third.sign_seq, 3);
    assert_eq!(fourth.sign_seq, 4);

    let all = reopened.list_audit_receipts().expect("list audit receipts");
    let seqs: Vec<u64> = all.iter().map(|r| r.sign_seq).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4]);
    drop(reopened);

    // Audit log rows are DDL-protected against UPDATE and DELETE.
    let raw_conn =
        Connection::open(root.path().join("signing-key-custody.db")).expect("open custody sqlite");
    assert!(
        raw_conn
            .execute("DELETE FROM custody_sign_audit_log WHERE sign_seq = 1", [])
            .is_err()
    );
}

#[test]
fn custody_key_epoch_rotation_bars_old_signing_and_preserves_historical_verification() {
    let root = TempRoot::new("rotation-epoch");
    let rig = GatewayRig::bootstrap(
        root.path(),
        [0x44; 32],
        [0x91; 16],
        [0x61; 32],
        CustodyTier::PlatformKeychainSealed,
        1_000,
        10_000,
    );
    let key_id = rig.gateway.key_id();

    let historical_digest = [0xd1; 32];
    let epoch1_audit = rig
        .custody
        .sign_digest_under_custody(CustodySignRequest {
            key_id,
            expected_rotation_epoch: Some(1),
            domain_tag: b"test/historical/v1",
            message_digest: historical_digest,
            signed_at_ms: 1_500,
        })
        .expect("sign at epoch 1");

    let (rotation, id_rot) = rig
        .custody
        .rotate_key_epoch_with_identity(
            &rig.identity,
            &rig.binding,
            RotateCustodyKeyEpochRequest {
                current_key_id: key_id,
                expected_rotation_epoch: 1,
                new_key_id: key_id,
                new_custody_tier: CustodyTier::HardwareHsmAttested,
                new_valid_from_ms: 2_000,
                new_valid_until_ms: 12_000,
                new_seed: [0x62; 32],
                rotated_at_ms: 2_000,
            },
            IdempotencyKey::from_bytes([0x71; 16]),
            IdempotencyKey::from_bytes([0x72; 16]),
        )
        .expect("rotate key epoch with identity");
    assert_eq!(
        rotation.previous_slot.state,
        CustodyKeyState::RotatedVerifyOnly
    );
    assert_eq!(rotation.previous_slot.rotation_epoch, 1);
    assert_eq!(rotation.active_slot.state, CustodyKeyState::Active);
    assert_eq!(rotation.active_slot.rotation_epoch, 2);
    assert_eq!(id_rot.resulting_key_generation.get(), 2);

    // Old epoch 1 refuses new signing and refuses live active-epoch verification.
    assert!(matches!(
        rig.custody.sign_digest_under_custody(CustodySignRequest {
            key_id,
            expected_rotation_epoch: Some(1),
            domain_tag: b"test/historical/v1",
            message_digest: [0xd2; 32],
            signed_at_ms: 2_100,
        }),
        Err(CustodyVaultError::KeyStateNotSignable {
            rotation_epoch: 1,
            state: CustodyKeyState::RotatedVerifyOnly,
            ..
        })
    ));
    assert!(matches!(
        rig.custody.verify_active_epoch_signature(
            key_id,
            1,
            &historical_digest,
            &epoch1_audit.signature,
            2_100,
        ),
        Err(CustodyVaultError::KeyRotatedVerifyOnly {
            rotation_epoch: 1,
            ..
        })
    ));

    // Historical verification of pre-rotation signature succeeds, while backdated post-rotation timestamp fails.
    let hist_verified = rig
        .custody
        .verify_historical_signature(
            key_id,
            1,
            &historical_digest,
            &epoch1_audit.signature,
            1_500,
        )
        .expect("historical verify epoch 1");
    assert_eq!(
        hist_verified.key_state_at_check,
        CustodyKeyState::RotatedVerifyOnly
    );
    assert!(matches!(
        rig.custody.verify_historical_signature(
            key_id,
            1,
            &historical_digest,
            &epoch1_audit.signature,
            2_500,
        ),
        Err(CustodyVaultError::HistoricalTimestampAfterRotation { .. })
    ));

    // New epoch 2 signs and passes live active-epoch verification.
    let epoch2_digest = [0xd3; 32];
    let epoch2_audit = rig
        .custody
        .sign_digest_under_custody(CustodySignRequest {
            key_id,
            expected_rotation_epoch: None,
            domain_tag: b"test/live/v1",
            message_digest: epoch2_digest,
            signed_at_ms: 2_200,
        })
        .expect("sign at epoch 2");
    assert_eq!(epoch2_audit.rotation_epoch, 2);
    assert_eq!(epoch2_audit.sign_seq, 2);
    rig.custody
        .verify_active_epoch_signature(key_id, 2, &epoch2_digest, &epoch2_audit.signature, 2_200)
        .expect("live verify epoch 2");
}

#[test]
fn custody_compromised_revocation_tombstone_refuses_signing_and_flags_post_compromise_proofs() {
    let root = TempRoot::new("compromised-tombstone");
    let rig = GatewayRig::bootstrap(
        root.path(),
        [0x55; 32],
        [0x92; 16],
        [0x71; 32],
        CustodyTier::HardwareHsmAttested,
        1_000,
        10_000,
    );
    let key_id = rig.gateway.key_id();

    let pre_digest = [0xe1; 32];
    let pre_audit = rig
        .custody
        .sign_digest_under_custody(CustodySignRequest {
            key_id,
            expected_rotation_epoch: Some(1),
            domain_tag: b"test/compromise/v1",
            message_digest: pre_digest,
            signed_at_ms: 1_200,
        })
        .expect("pre-compromise sign");

    let post_digest = [0xe2; 32];
    let post_audit = rig
        .custody
        .sign_digest_under_custody(CustodySignRequest {
            key_id,
            expected_rotation_epoch: Some(1),
            domain_tag: b"test/compromise/v1",
            message_digest: post_digest,
            signed_at_ms: 1_800,
        })
        .expect("post-compromise sign before discovery");

    // Compromise discovered at 2_000 ms, retroactive to revoked_from_ms = 1_500 ms.
    let (tombstone, _) = rig
        .custody
        .revoke_compromised_key_with_identity(
            &rig.identity,
            &rig.binding,
            RevokeCompromisedKeyRequest {
                key_id,
                rotation_epoch: 1,
                revoked_from_ms: 1_500,
                reason_digest: [0xfe; 32],
                recorded_at_ms: 2_000,
            },
            IdempotencyKey::from_bytes([0x88; 16]),
        )
        .expect("record compromise tombstone");
    assert_eq!(tombstone.revoked_from_ms, 1_500);

    // Refuses all new signing and live active verification.
    assert!(matches!(
        rig.custody.sign_digest_under_custody(CustodySignRequest {
            key_id,
            expected_rotation_epoch: None,
            domain_tag: b"test/compromise/v1",
            message_digest: [0xe3; 32],
            signed_at_ms: 2_100,
        }),
        Err(CustodyVaultError::KeyCompromised {
            revoked_from_ms: 1_500,
            ..
        })
    ));
    assert!(matches!(
        rig.custody.verify_active_epoch_signature(
            key_id,
            1,
            &pre_digest,
            &pre_audit.signature,
            2_100,
        ),
        Err(CustodyVaultError::KeyCompromised {
            revoked_from_ms: 1_500,
            ..
        })
    ));

    // Historical check flags signature at 1_800 >= 1_500 as KeyCompromised, while 1_200 < 1_500 remains distinguishable.
    assert!(matches!(
        rig.custody.verify_historical_signature(
            key_id,
            1,
            &post_digest,
            &post_audit.signature,
            1_800,
        ),
        Err(CustodyVaultError::KeyCompromised {
            revoked_from_ms: 1_500,
            ..
        })
    ));
    let pre_verified = rig
        .custody
        .verify_historical_signature(key_id, 1, &pre_digest, &pre_audit.signature, 1_200)
        .expect("pre-compromise historical audit check");
    assert_eq!(
        pre_verified.key_state_at_check,
        CustodyKeyState::CompromisedRevoked
    );
}

#[test]
fn provisional_gateway_attestation_projects_rebate_without_moving_funds_or_unfreezing() {
    let root = TempRoot::new("provisional-audit-only");
    let gw_rig = GatewayRig::bootstrap(
        root.path(),
        [0x61; 32],
        [0xa1; 16],
        [0x72; 32],
        CustodyTier::HardwareHsmAttested,
        1_000,
        20_000,
    );
    let rt_rig = ResourceTaskRig::new(root.path(), 0x11, 1_000);
    let (reservation, _, plan_id) = rt_rig.reserve_and_prepare_task_plan(root.path(), 0x12, 200);
    rt_rig.activate_consume_and_quarantine(&reservation, &[(1, 50)], true);

    assert_eq!(
        rt_rig
            .resource
            .inspect_account(rt_rig.account.account_id)
            .expect("account")
            .available_credit,
        800
    );

    let mut ledger = GatewayReconciliationLedger::new();
    let (prov_v1, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x01; 16],
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                fencing_token: reservation.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::EffectClosed {
                    final_seq: 2,
                    final_usage: 90,
                },
                receipt_version: 1,
                finality: ReceiptFinality::Provisional,
                issued_at_ms: 1_600,
            },
        )
        .expect("issue provisional v1");

    let decision_v1 = verify_and_apply_gateway_reconciliation(
        ApplyGatewayReconciliationContext {
            ledger: &mut ledger,
            expected_gateway_id: gw_rig.gateway_id,
            custody_vault: &gw_rig.custody,
            identity_authority: &gw_rig.identity,
            resource_authority: &rt_rig.resource,
            task_commit_plan: Some((&rt_rig.task, plan_id)),
            verified_at_ms: 1_610,
        },
        &prov_v1,
    )
    .expect("apply provisional v1");
    let GatewayReconciliationDecision::ProjectedProvisional(audit_v1) = decision_v1 else {
        panic!("expected ProjectedProvisional");
    };
    assert_eq!(audit_v1.projected_usage, 90);
    assert_eq!(audit_v1.projected_rebate, 110);

    // Supersede with provisional v2 (higher usage estimate = 120, rebate = 80).
    let (prov_v2, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x02; 16],
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                fencing_token: reservation.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::EffectClosed {
                    final_seq: 3,
                    final_usage: 120,
                },
                receipt_version: 2,
                finality: ReceiptFinality::Provisional,
                issued_at_ms: 1_650,
            },
        )
        .expect("issue provisional v2");
    let decision_v2 = verify_and_apply_gateway_reconciliation(
        ApplyGatewayReconciliationContext {
            ledger: &mut ledger,
            expected_gateway_id: gw_rig.gateway_id,
            custody_vault: &gw_rig.custody,
            identity_authority: &gw_rig.identity,
            resource_authority: &rt_rig.resource,
            task_commit_plan: Some((&rt_rig.task, plan_id)),
            verified_at_ms: 1_660,
        },
        &prov_v2,
    )
    .expect("apply provisional v2");
    assert!(matches!(
        decision_v2,
        GatewayReconciliationDecision::ProjectedProvisional(_)
    ));

    // Exact replay of v2 returns ReplayedProvisional.
    let replay_v2 = verify_and_apply_gateway_reconciliation(
        ApplyGatewayReconciliationContext {
            ledger: &mut ledger,
            expected_gateway_id: gw_rig.gateway_id,
            custody_vault: &gw_rig.custody,
            identity_authority: &gw_rig.identity,
            resource_authority: &rt_rig.resource,
            task_commit_plan: Some((&rt_rig.task, plan_id)),
            verified_at_ms: 1_670,
        },
        &prov_v2,
    )
    .expect("replay provisional v2");
    assert!(matches!(
        replay_v2,
        GatewayReconciliationDecision::ReplayedProvisional(_)
    ));

    // [BUD-REBATE-001]: Credit is still 800, reservation is still Quarantined, and Task plan is still NotDue!
    assert_eq!(
        rt_rig
            .resource
            .inspect_account(rt_rig.account.account_id)
            .expect("account")
            .available_credit,
        800
    );
    assert_eq!(
        rt_rig
            .resource
            .inspect_reservation(reservation.reservation_id)
            .expect("reservation")
            .state,
        ReservationState::Quarantined
    );
    assert!(matches!(
        rt_rig
            .task
            .converge_resource_commit_plan(&rt_rig.resource, plan_id, 1_700)
            .expect("converge check"),
        ResourceConvergeDecision::NotDue(_)
    ));
}

#[test]
fn final_effect_closed_attestation_settles_quarantined_reservation_converges_task_and_replays_idempotently()
 {
    let root = TempRoot::new("final-effect-closed");
    let gw_rig = GatewayRig::bootstrap(
        root.path(),
        [0x62; 32],
        [0xa2; 16],
        [0x73; 32],
        CustodyTier::HardwareHsmAttested,
        1_000,
        20_000,
    );
    let rt_rig = ResourceTaskRig::new(root.path(), 0x21, 1_000);
    let (reservation, _, plan_id) = rt_rig.reserve_and_prepare_task_plan(root.path(), 0x22, 200);
    rt_rig.activate_consume_and_quarantine(&reservation, &[(1, 80)], true);

    let mut ledger = GatewayReconciliationLedger::new();
    let (prov_v1, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x11; 16],
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                fencing_token: reservation.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::EffectClosed {
                    final_seq: 2,
                    final_usage: 110,
                },
                receipt_version: 1,
                finality: ReceiptFinality::Provisional,
                issued_at_ms: 1_600,
            },
        )
        .expect("provisional v1");
    verify_and_apply_gateway_reconciliation(
        ApplyGatewayReconciliationContext {
            ledger: &mut ledger,
            expected_gateway_id: gw_rig.gateway_id,
            custody_vault: &gw_rig.custody,
            identity_authority: &gw_rig.identity,
            resource_authority: &rt_rig.resource,
            task_commit_plan: Some((&rt_rig.task, plan_id)),
            verified_at_ms: 1_605,
        },
        &prov_v1,
    )
    .expect("apply provisional v1");

    let (final_v2, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x12; 16],
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                fencing_token: reservation.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::EffectClosed {
                    final_seq: 2,
                    final_usage: 125,
                },
                receipt_version: 2,
                finality: ReceiptFinality::FinalNonSupersedeable,
                issued_at_ms: 1_700,
            },
        )
        .expect("issue final v2");

    let settled = verify_and_apply_gateway_reconciliation(
        ApplyGatewayReconciliationContext {
            ledger: &mut ledger,
            expected_gateway_id: gw_rig.gateway_id,
            custody_vault: &gw_rig.custody,
            identity_authority: &gw_rig.identity,
            resource_authority: &rt_rig.resource,
            task_commit_plan: Some((&rt_rig.task, plan_id)),
            verified_at_ms: 1_710,
        },
        &final_v2,
    )
    .expect("apply final v2");

    let GatewayReconciliationDecision::SettledFinal(outcome) = settled else {
        panic!("expected SettledFinal");
    };
    assert_eq!(outcome.finalization_receipt.final_usage, 125);
    assert_eq!(outcome.finalization_receipt.refund_credit, 75);
    assert!(matches!(
        outcome.task_convergence,
        Some(ResourceConvergeDecision::Finalized(_))
    ));
    assert_eq!(
        rt_rig
            .resource
            .inspect_account(rt_rig.account.account_id)
            .expect("account")
            .available_credit,
        875
    );
    assert_eq!(
        rt_rig
            .task
            .inspect_resource_commit_plan(plan_id)
            .expect("task plan")
            .state,
        ResourceCommitPlanState::Finalized
    );
    assert!(
        ledger
            .inspect_projected_audit(reservation.reservation_id)
            .is_none()
    );

    // Exact replay of final v2 is idempotent and never double-refunds.
    let replayed = verify_and_apply_gateway_reconciliation(
        ApplyGatewayReconciliationContext {
            ledger: &mut ledger,
            expected_gateway_id: gw_rig.gateway_id,
            custody_vault: &gw_rig.custody,
            identity_authority: &gw_rig.identity,
            resource_authority: &rt_rig.resource,
            task_commit_plan: Some((&rt_rig.task, plan_id)),
            verified_at_ms: 1_750,
        },
        &final_v2,
    )
    .expect("replay final v2");
    assert!(matches!(
        replayed,
        GatewayReconciliationDecision::ReplayedFinal(_)
    ));
    assert_eq!(
        rt_rig
            .resource
            .inspect_account(rt_rig.account.account_id)
            .expect("account")
            .available_credit,
        875
    );
}

#[test]
fn final_confirmed_no_effect_settles_zero_usage_and_rejects_nonzero_high_water() {
    let root = TempRoot::new("confirmed-no-effect");
    let gw_rig = GatewayRig::bootstrap(
        root.path(),
        [0x63; 32],
        [0xa3; 16],
        [0x74; 32],
        CustodyTier::PlatformKeychainSealed,
        1_000,
        20_000,
    );
    let rt_rig = ResourceTaskRig::new(root.path(), 0x31, 1_000);

    // Reservation A: zero consumption -> ConfirmedNoEffect settles with full refund = 150.
    let (res_zero, _, plan_zero) = rt_rig.reserve_and_prepare_task_plan(root.path(), 0x32, 150);
    rt_rig.activate_consume_and_quarantine(&res_zero, &[], true);
    assert_eq!(
        rt_rig
            .resource
            .inspect_account(rt_rig.account.account_id)
            .expect("account after reserve A")
            .available_credit,
        850
    );

    let mut ledger = GatewayReconciliationLedger::new();
    let (no_effect_ok, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x21; 16],
                reservation_id: res_zero.reservation_id,
                operation_id: res_zero.operation_id,
                fencing_token: res_zero.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::ConfirmedNoEffect,
                receipt_version: 1,
                finality: ReceiptFinality::FinalNonSupersedeable,
                issued_at_ms: 1_600,
            },
        )
        .expect("issue ConfirmedNoEffect for zero-usage reservation");

    let settled = verify_and_apply_gateway_reconciliation(
        ApplyGatewayReconciliationContext {
            ledger: &mut ledger,
            expected_gateway_id: gw_rig.gateway_id,
            custody_vault: &gw_rig.custody,
            identity_authority: &gw_rig.identity,
            resource_authority: &rt_rig.resource,
            task_commit_plan: Some((&rt_rig.task, plan_zero)),
            verified_at_ms: 1_610,
        },
        &no_effect_ok,
    )
    .expect("settle ConfirmedNoEffect");
    let GatewayReconciliationDecision::SettledFinal(outcome) = settled else {
        panic!("expected SettledFinal");
    };
    assert_eq!(outcome.finalization_receipt.final_usage, 0);
    assert_eq!(outcome.finalization_receipt.refund_credit, 150);
    assert_eq!(
        rt_rig
            .resource
            .inspect_account(rt_rig.account.account_id)
            .expect("account fully refunded")
            .available_credit,
        1_000
    );

    // Reservation B: usage_high_water = 30 -> ConfirmedNoEffect MUST fail closed ([BUD-CLOSE-001]).
    let (res_nonzero, _, plan_nonzero) =
        rt_rig.reserve_and_prepare_task_plan(root.path(), 0x35, 150);
    rt_rig.activate_consume_and_quarantine(&res_nonzero, &[(1, 30)], true);
    let (no_effect_bad, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x22; 16],
                reservation_id: res_nonzero.reservation_id,
                operation_id: res_nonzero.operation_id,
                fencing_token: res_nonzero.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::ConfirmedNoEffect,
                receipt_version: 1,
                finality: ReceiptFinality::FinalNonSupersedeable,
                issued_at_ms: 1_650,
            },
        )
        .expect("issue ConfirmedNoEffect on nonzero reservation");

    assert!(matches!(
        verify_and_apply_gateway_reconciliation(
            ApplyGatewayReconciliationContext {
                ledger: &mut ledger,
                expected_gateway_id: gw_rig.gateway_id,
                custody_vault: &gw_rig.custody,
                identity_authority: &gw_rig.identity,
                resource_authority: &rt_rig.resource,
                task_commit_plan: Some((&rt_rig.task, plan_nonzero)),
                verified_at_ms: 1_660,
            },
            &no_effect_bad,
        ),
        Err(GatewayReconcileError::BelowHighWaterMark {
            high_water_seq: 1,
            high_water_usage: 30,
            reported_seq: 0,
            reported_usage: 0,
        })
    ));
    assert_eq!(
        rt_rig
            .resource
            .inspect_account(rt_rig.account.account_id)
            .expect("account still frozen at 850")
            .available_credit,
        850
    );
    assert_eq!(
        rt_rig
            .task
            .inspect_resource_commit_plan(plan_nonzero)
            .expect("plan B")
            .state,
        ResourceCommitPlanState::Planned
    );
}

#[test]
fn supersession_conflicts_stale_versions_post_final_and_below_high_water_fail_closed() {
    let root = TempRoot::new("supersession-guards");
    let gw_rig = GatewayRig::bootstrap(
        root.path(),
        [0x64; 32],
        [0xa4; 16],
        [0x75; 32],
        CustodyTier::HardwareHsmAttested,
        1_000,
        20_000,
    );
    let rt_rig = ResourceTaskRig::new(root.path(), 0x41, 1_000);
    let (reservation, _, plan_id) = rt_rig.reserve_and_prepare_task_plan(root.path(), 0x42, 200);
    rt_rig.activate_consume_and_quarantine(&reservation, &[(2, 60)], true);

    let mut ledger = GatewayReconciliationLedger::new();

    // 1. Below high-water usage (50 < 60) fails closed.
    let (below_hw, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x31; 16],
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                fencing_token: reservation.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::EffectClosed {
                    final_seq: 2,
                    final_usage: 50,
                },
                receipt_version: 1,
                finality: ReceiptFinality::FinalNonSupersedeable,
                issued_at_ms: 1_600,
            },
        )
        .expect("issue below hw");
    assert!(matches!(
        verify_and_apply_gateway_reconciliation(
            ApplyGatewayReconciliationContext {
                ledger: &mut ledger,
                expected_gateway_id: gw_rig.gateway_id,
                custody_vault: &gw_rig.custody,
                identity_authority: &gw_rig.identity,
                resource_authority: &rt_rig.resource,
                task_commit_plan: Some((&rt_rig.task, plan_id)),
                verified_at_ms: 1_610,
            },
            &below_hw,
        ),
        Err(GatewayReconcileError::BelowHighWaterMark { .. })
    ));

    // 2. Exceeding upper_bound (250 > 200) fails closed.
    let (above_ub, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x32; 16],
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                fencing_token: reservation.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::EffectClosed {
                    final_seq: 3,
                    final_usage: 250,
                },
                receipt_version: 1,
                finality: ReceiptFinality::FinalNonSupersedeable,
                issued_at_ms: 1_615,
            },
        )
        .expect("issue above ub");
    assert!(matches!(
        verify_and_apply_gateway_reconciliation(
            ApplyGatewayReconciliationContext {
                ledger: &mut ledger,
                expected_gateway_id: gw_rig.gateway_id,
                custody_vault: &gw_rig.custody,
                identity_authority: &gw_rig.identity,
                resource_authority: &rt_rig.resource,
                task_commit_plan: Some((&rt_rig.task, plan_id)),
                verified_at_ms: 1_620,
            },
            &above_ub,
        ),
        Err(GatewayReconcileError::ExceedsUpperBound {
            upper_bound: 200,
            reported_usage: 250,
        })
    ));

    // 3. Apply valid provisional v2, then verify conflicting same-version v2 and stale v1 fail closed.
    let (prov_v2, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x33; 16],
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                fencing_token: reservation.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::EffectClosed {
                    final_seq: 2,
                    final_usage: 90,
                },
                receipt_version: 2,
                finality: ReceiptFinality::Provisional,
                issued_at_ms: 1_630,
            },
        )
        .expect("issue prov v2");
    verify_and_apply_gateway_reconciliation(
        ApplyGatewayReconciliationContext {
            ledger: &mut ledger,
            expected_gateway_id: gw_rig.gateway_id,
            custody_vault: &gw_rig.custody,
            identity_authority: &gw_rig.identity,
            resource_authority: &rt_rig.resource,
            task_commit_plan: None,
            verified_at_ms: 1_635,
        },
        &prov_v2,
    )
    .expect("apply prov v2");

    let (conflict_v2, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x34; 16],
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                fencing_token: reservation.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::EffectClosed {
                    final_seq: 2,
                    final_usage: 95,
                },
                receipt_version: 2,
                finality: ReceiptFinality::Provisional,
                issued_at_ms: 1_640,
            },
        )
        .expect("issue conflicting v2");
    assert!(matches!(
        verify_and_apply_gateway_reconciliation(
            ApplyGatewayReconciliationContext {
                ledger: &mut ledger,
                expected_gateway_id: gw_rig.gateway_id,
                custody_vault: &gw_rig.custody,
                identity_authority: &gw_rig.identity,
                resource_authority: &rt_rig.resource,
                task_commit_plan: None,
                verified_at_ms: 1_645,
            },
            &conflict_v2,
        ),
        Err(GatewayReconcileError::ConflictingSameVersionAttestation {
            receipt_version: 2,
            ..
        })
    ));

    let (stale_v1, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x35; 16],
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                fencing_token: reservation.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::EffectClosed {
                    final_seq: 2,
                    final_usage: 90,
                },
                receipt_version: 1,
                finality: ReceiptFinality::Provisional,
                issued_at_ms: 1_650,
            },
        )
        .expect("issue stale v1");
    assert!(matches!(
        verify_and_apply_gateway_reconciliation(
            ApplyGatewayReconciliationContext {
                ledger: &mut ledger,
                expected_gateway_id: gw_rig.gateway_id,
                custody_vault: &gw_rig.custody,
                identity_authority: &gw_rig.identity,
                resource_authority: &rt_rig.resource,
                task_commit_plan: None,
                verified_at_ms: 1_655,
            },
            &stale_v1,
        ),
        Err(GatewayReconcileError::StaleReceiptVersion {
            current_version: 2,
            attempted_version: 1,
            ..
        })
    ));

    // 4. Settle at FinalNonSupersedeable v3, then verify v4 supersession attempt fails closed!
    let (final_v3, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x36; 16],
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                fencing_token: reservation.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::EffectClosed {
                    final_seq: 3,
                    final_usage: 100,
                },
                receipt_version: 3,
                finality: ReceiptFinality::FinalNonSupersedeable,
                issued_at_ms: 1_700,
            },
        )
        .expect("issue final v3");
    verify_and_apply_gateway_reconciliation(
        ApplyGatewayReconciliationContext {
            ledger: &mut ledger,
            expected_gateway_id: gw_rig.gateway_id,
            custody_vault: &gw_rig.custody,
            identity_authority: &gw_rig.identity,
            resource_authority: &rt_rig.resource,
            task_commit_plan: Some((&rt_rig.task, plan_id)),
            verified_at_ms: 1_705,
        },
        &final_v3,
    )
    .expect("settle final v3");

    let (post_final_v4, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x37; 16],
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                fencing_token: reservation.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::EffectClosed {
                    final_seq: 4,
                    final_usage: 110,
                },
                receipt_version: 4,
                finality: ReceiptFinality::FinalNonSupersedeable,
                issued_at_ms: 1_750,
            },
        )
        .expect("issue post-final v4");
    assert!(matches!(
        verify_and_apply_gateway_reconciliation(
            ApplyGatewayReconciliationContext {
                ledger: &mut ledger,
                expected_gateway_id: gw_rig.gateway_id,
                custody_vault: &gw_rig.custody,
                identity_authority: &gw_rig.identity,
                resource_authority: &rt_rig.resource,
                task_commit_plan: Some((&rt_rig.task, plan_id)),
                verified_at_ms: 1_760,
            },
            &post_final_v4,
        ),
        Err(GatewayReconcileError::FinalReceiptNonSupersedeable {
            settled_version: 3,
            attempted_version: 4,
            ..
        })
    ));
    assert_eq!(
        rt_rig
            .resource
            .inspect_account(rt_rig.account.account_id)
            .expect("account settled once at 900")
            .available_credit,
        900
    );
}

#[test]
fn still_uncertain_quarantine_freezes_active_reservation_and_keeps_quarantined_funds_frozen() {
    let root = TempRoot::new("still-uncertain-quarantine");
    let gw_rig = GatewayRig::bootstrap(
        root.path(),
        [0x65; 32],
        [0xa5; 16],
        [0x76; 32],
        CustodyTier::OperatorFileSealed,
        1_000,
        20_000,
    );
    let rt_rig = ResourceTaskRig::new(root.path(), 0x51, 1_000);
    let (reservation, _, plan_id) = rt_rig.reserve_and_prepare_task_plan(root.path(), 0x52, 200);
    // Leave reservation ACTIVE (not yet quarantined) with consumption = 40.
    rt_rig.activate_consume_and_quarantine(&reservation, &[(1, 40)], false);
    assert_eq!(
        rt_rig
            .resource
            .inspect_reservation(reservation.reservation_id)
            .expect("active reservation")
            .state,
        ReservationState::Active
    );

    // Combining StillUncertainQuarantine with FinalNonSupersedeable is rejected at issuance.
    assert!(matches!(
        gw_rig.gateway.issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x41; 16],
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                fencing_token: reservation.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::StillUncertainQuarantine,
                receipt_version: 1,
                finality: ReceiptFinality::FinalNonSupersedeable,
                issued_at_ms: 1_600,
            },
        ),
        Err(GatewayReconcileError::InvalidDispositionFinalityCombination)
    ));

    let mut ledger = GatewayReconciliationLedger::new();
    let (uncertain_v1, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x42; 16],
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                fencing_token: reservation.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::StillUncertainQuarantine,
                receipt_version: 1,
                finality: ReceiptFinality::Provisional,
                issued_at_ms: 1_600,
            },
        )
        .expect("issue StillUncertainQuarantine v1");

    let decision = verify_and_apply_gateway_reconciliation(
        ApplyGatewayReconciliationContext {
            ledger: &mut ledger,
            expected_gateway_id: gw_rig.gateway_id,
            custody_vault: &gw_rig.custody,
            identity_authority: &gw_rig.identity,
            resource_authority: &rt_rig.resource,
            task_commit_plan: Some((&rt_rig.task, plan_id)),
            verified_at_ms: 1_610,
        },
        &uncertain_v1,
    )
    .expect("apply uncertain v1");
    let GatewayReconciliationDecision::FrozenQuarantined {
        quarantine_receipt,
        receipt_version,
    } = decision
    else {
        panic!("expected FrozenQuarantined");
    };
    assert_eq!(receipt_version, 1);
    assert_eq!(quarantine_receipt.high_water, 40);

    // Reservation transitioned Active -> Quarantined, credit stays 800, Task plan stays Planned!
    assert_eq!(
        rt_rig
            .resource
            .inspect_reservation(reservation.reservation_id)
            .expect("quarantined reservation")
            .state,
        ReservationState::Quarantined
    );
    assert_eq!(
        rt_rig
            .resource
            .inspect_account(rt_rig.account.account_id)
            .expect("account")
            .available_credit,
        800
    );
    assert_eq!(
        rt_rig
            .task
            .inspect_resource_commit_plan(plan_id)
            .expect("plan")
            .state,
        ResourceCommitPlanState::Planned
    );
}

#[test]
fn compromised_expired_rotated_wrong_gateway_and_stale_fence_attestations_fail_closed() {
    let root = TempRoot::new("adversarial-custody-fences");
    let gw_rig = GatewayRig::bootstrap(
        root.path(),
        [0x66; 32],
        [0xa6; 16],
        [0x77; 32],
        CustodyTier::HardwareHsmAttested,
        1_000,
        5_000,
    );
    let rt_rig = ResourceTaskRig::new(root.path(), 0x61, 1_000);
    let (reservation, _, plan_id) = rt_rig.reserve_and_prepare_task_plan(root.path(), 0x62, 200);
    rt_rig.activate_consume_and_quarantine(&reservation, &[(1, 50)], true);

    let mut ledger = GatewayReconciliationLedger::new();

    // 1. Wrong gateway_id expected by coordinator -> GatewayIdentityMismatch.
    let (valid_epoch1, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x51; 16],
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                fencing_token: reservation.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::EffectClosed {
                    final_seq: 1,
                    final_usage: 50,
                },
                receipt_version: 1,
                finality: ReceiptFinality::FinalNonSupersedeable,
                issued_at_ms: 1_700,
            },
        )
        .expect("issue epoch 1 attestation");
    assert!(matches!(
        verify_and_apply_gateway_reconciliation(
            ApplyGatewayReconciliationContext {
                ledger: &mut ledger,
                expected_gateway_id: [0xff; 16],
                custody_vault: &gw_rig.custody,
                identity_authority: &gw_rig.identity,
                resource_authority: &rt_rig.resource,
                task_commit_plan: Some((&rt_rig.task, plan_id)),
                verified_at_ms: 1_710,
            },
            &valid_epoch1,
        ),
        Err(GatewayReconcileError::GatewayIdentityMismatch { .. })
    ));

    // 2. Stale/forged fencing_token -> FencingTokenMismatch.
    let (bad_fence, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x52; 16],
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                fencing_token: [0x99; 32],
                disposition: GatewayOutcomeDisposition::EffectClosed {
                    final_seq: 1,
                    final_usage: 50,
                },
                receipt_version: 1,
                finality: ReceiptFinality::FinalNonSupersedeable,
                issued_at_ms: 1_720,
            },
        )
        .expect("issue wrong fencing_token attestation");
    assert!(matches!(
        verify_and_apply_gateway_reconciliation(
            ApplyGatewayReconciliationContext {
                ledger: &mut ledger,
                expected_gateway_id: gw_rig.gateway_id,
                custody_vault: &gw_rig.custody,
                identity_authority: &gw_rig.identity,
                resource_authority: &rt_rig.resource,
                task_commit_plan: Some((&rt_rig.task, plan_id)),
                verified_at_ms: 1_725,
            },
            &bad_fence,
        ),
        Err(GatewayReconcileError::FencingTokenMismatch { .. })
    ));

    // 3. Expired verification timestamp (verified_at_ms = 6_000 > valid_until_ms = 5_000) -> KeyExpired.
    assert!(matches!(
        verify_and_apply_gateway_reconciliation(
            ApplyGatewayReconciliationContext {
                ledger: &mut ledger,
                expected_gateway_id: gw_rig.gateway_id,
                custody_vault: &gw_rig.custody,
                identity_authority: &gw_rig.identity,
                resource_authority: &rt_rig.resource,
                task_commit_plan: Some((&rt_rig.task, plan_id)),
                verified_at_ms: 6_000,
            },
            &valid_epoch1,
        ),
        Err(GatewayReconcileError::Custody(
            CustodyVaultError::KeyExpired { .. }
        ))
    ));

    // 4. Rotate gateway key to epoch 2: old epoch 1 attestation now fails with KeyRotatedVerifyOnly!
    let key_id: KeyId = gw_rig.gateway.key_id();
    let (_, id_rot) = gw_rig
        .custody
        .rotate_key_epoch_with_identity(
            &gw_rig.identity,
            &gw_rig.binding,
            RotateCustodyKeyEpochRequest {
                current_key_id: key_id,
                expected_rotation_epoch: 1,
                new_key_id: key_id,
                new_custody_tier: CustodyTier::HardwareHsmAttested,
                new_valid_from_ms: 2_000,
                new_valid_until_ms: 10_000,
                new_seed: [0x78; 32],
                rotated_at_ms: 2_000,
            },
            IdempotencyKey::from_bytes([0x91; 16]),
            IdempotencyKey::from_bytes([0x92; 16]),
        )
        .expect("rotate to epoch 2");
    assert!(matches!(
        verify_and_apply_gateway_reconciliation(
            ApplyGatewayReconciliationContext {
                ledger: &mut ledger,
                expected_gateway_id: gw_rig.gateway_id,
                custody_vault: &gw_rig.custody,
                identity_authority: &gw_rig.identity,
                resource_authority: &rt_rig.resource,
                task_commit_plan: Some((&rt_rig.task, plan_id)),
                verified_at_ms: 2_100,
            },
            &valid_epoch1,
        ),
        Err(GatewayReconcileError::Custody(
            CustodyVaultError::KeyRotatedVerifyOnly {
                rotation_epoch: 1,
                ..
            }
        ))
    ));

    // 5. Issue epoch 2 attestation, then compromise epoch 2 before applying -> KeyCompromised!
    let (epoch2_att, _) = gw_rig
        .gateway
        .issue_attestation(
            &gw_rig.custody,
            IssueGatewayAttestationRequest {
                reconcile_id: [0x53; 16],
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                fencing_token: reservation.driver_fencing_token,
                disposition: GatewayOutcomeDisposition::EffectClosed {
                    final_seq: 1,
                    final_usage: 50,
                },
                receipt_version: 1,
                finality: ReceiptFinality::FinalNonSupersedeable,
                issued_at_ms: 2_200,
            },
        )
        .expect("issue epoch 2 attestation");
    let updated_binding = IdentityBinding {
        key_generation: id_rot.resulting_key_generation,
        identity_snapshot_id: id_rot.identity_snapshot_id,
        snapshot_generation: id_rot.snapshot_generation,
        public_key: id_rot.new_public_key,
        key_valid_from_ms: id_rot.new_valid_from_ms,
        key_valid_until_ms: id_rot.new_valid_until_ms,
        ..gw_rig.binding
    };
    gw_rig
        .custody
        .revoke_compromised_key_with_identity(
            &gw_rig.identity,
            &updated_binding,
            RevokeCompromisedKeyRequest {
                key_id,
                rotation_epoch: 2,
                revoked_from_ms: 2_150,
                reason_digest: [0xee; 32],
                recorded_at_ms: 2_300,
            },
            IdempotencyKey::from_bytes([0x93; 16]),
        )
        .expect("compromise epoch 2");

    assert!(matches!(
        verify_and_apply_gateway_reconciliation(
            ApplyGatewayReconciliationContext {
                ledger: &mut ledger,
                expected_gateway_id: gw_rig.gateway_id,
                custody_vault: &gw_rig.custody,
                identity_authority: &gw_rig.identity,
                resource_authority: &rt_rig.resource,
                task_commit_plan: Some((&rt_rig.task, plan_id)),
                verified_at_ms: 2_350,
            },
            &epoch2_att,
        ),
        Err(GatewayReconcileError::Custody(
            CustodyVaultError::KeyCompromised {
                rotation_epoch: 2,
                ..
            }
        ))
    ));

    // [BUD-UNCERTAIN-001]: Across all 5 rejected attempts, funds and Quarantined state stayed 100% frozen!
    assert_eq!(
        rt_rig
            .resource
            .inspect_reservation(reservation.reservation_id)
            .expect("reservation")
            .state,
        ReservationState::Quarantined
    );
    assert_eq!(
        rt_rig
            .resource
            .inspect_account(rt_rig.account.account_id)
            .expect("account")
            .available_credit,
        800
    );
    assert_eq!(
        rt_rig
            .task
            .inspect_resource_commit_plan(plan_id)
            .expect("plan")
            .state,
        ResourceCommitPlanState::Planned
    );
}
