//! Adversarial isolation and TCB boundary conformance test suite for
//! `nlos-compat-shim` (`[DRV-SHIM-001]`, `[DRV-CRED-001]`, `[DRV-BOUND-001]`,
//! `[DRV-ACTIVE-001]`, `[DRV-LIFE-001]`, `[KABI-UNCERTAIN-001]`,
//! `[TASK-PROVIDER-001]`, `ADR-0027` section 2.2.2).
//!
//! Exercises the 4-layer architecture end-to-end against real SQLite-backed
//! `IdentityAuthority`, `CapabilityAuthority`, `SigningKeyCustodyVault`,
//! `ResourceAuthority`, and fenced `OperationRegistry`.
#![forbid(unsafe_code)]

use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_capability::custody::{
    BootstrapCustodyPrincipalRequest, CustodySignRequest, CustodyTier, SigningKeyCustodyVault,
};
use nlos_capability::{
    CapabilityAuthority, CapabilityAuthorityError, CapabilityHandle, CapabilityRecord,
    CapabilityRights, CapabilityTarget, DelegateCapabilityRequest, IssueRootCapabilityRequest,
    RevokeCapabilityRequest, SignedDelegateCapabilityRequest, SignedRevokeCapabilityRequest,
    delegate_command_message, revoke_command_message,
};
use nlos_compat_shim::driver_shim::{
    COMPLETE_PROVIDER_METHOD, CompleteOperationResultWire, DRIVER_PROVIDER_SERVICE,
    OUTCOME_CODE_COMPLETED, PROVIDER_ISOLATION_UNVERIFIED_TAG, ProviderIsolationAttestation,
    REGISTER_PROVIDER_METHOD, RegisterOperationWire, STATE_CODE_COMPLETED,
    encode_complete_ack_proto, encode_complete_result, encode_register_request,
};
use nlos_compat_shim::{
    BoundaryViolation, DriverProtocolShim, LegacyCompleteBridgeContext, MediatedDriverDispatchAuth,
    OutOfCoreCompatShimRuntime, SabiEpochAdapterCell, SecretCredential, SignedMeteredUsageProof,
    TrustedEnforcementGate, build_driver_request_envelope, compute_dispatch_semantic_event_id,
    compute_metered_usage_proof_digest,
};
use nlos_driver_mock::provider::CompleteProviderOutcome;
use nlos_identity::{
    IdentityAuthority, IdentityAuthorityError, IdentityBinding, KeyPurpose, RevokeKeyRequest,
    semantic_signature_message,
};
use nlos_operation::{
    CompletionOutcome, OperationHandle, OperationRegistry, OperationSpec, OperationState,
};
use nlos_resource::{
    AccountRecord, CreateAccountRequest, CreateQuoteRequest, DriverRecord, RegisterDriverRequest,
    ReservationRecord, ReservationState, ReserveRequest, ResourceAuthority, ResourceAuthorityError,
    ResourceDemand, RotateDriverRequest,
};
use nlos_runtime::FiberHandle;
use nlos_schema::SABI_ENVELOPE_SCHEMA;
use nlos_schema::sabi::v1::{
    DriverProviderResponse, Envelope, RetryDirective, SabiErrorCode, SabiResponseContext,
    SchemaIdentity, envelope,
};
use nlos_types::{
    CallId, CallbackId, CancellationScopeId, CapabilityId, ExecutionFiberId, Generation,
    IdempotencyKey, NamespaceId, OperationId, ReceiptId, ReservationId, TaskParticipantId,
};
use prost::Message;

static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

fn gen_u64(raw: u64) -> Generation {
    Generation::new(NonZeroU64::new(raw).expect("non-zero generation"))
}

fn id16(tag: u8, seq: u64) -> [u8; 16] {
    let mut out = [tag; 16];
    out[8..16].copy_from_slice(&seq.to_be_bytes());
    out
}

struct TempSqliteDir {
    path: PathBuf,
}

impl TempSqliteDir {
    fn new(label: &str) -> Self {
        let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("nlos-e2-shim-test-{pid}-{seq}-{label}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp test dir");
        Self { path: dir }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempSqliteDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct FourLayerHarness {
    _temp_dir: TempSqliteDir,
    client_epoch: u32,
    host_epoch: u32,
    identity: Arc<IdentityAuthority>,
    capability: Arc<CapabilityAuthority>,
    resource: Arc<ResourceAuthority>,
    custody: SigningKeyCustodyVault,
    issuer_binding: IdentityBinding,
    caller_binding: IdentityBinding,
    bystander_binding: IdentityBinding,
    meter_binding: IdentityBinding,
    account: AccountRecord,
    driver: DriverRecord,
    process_id: TaskParticipantId,
    process_generation: Generation,
    namespace_a: CapabilityTarget,
    namespace_b: CapabilityTarget,
    purpose_digest: [u8; 32],
    root_capability: CapabilityRecord,
    gate: TrustedEnforcementGate,
    shim: OutOfCoreCompatShimRuntime,
}

fn bootstrap_test_principal(
    custody: &SigningKeyCustodyVault,
    identity: &IdentityAuthority,
    tag: u8,
    seed_byte: u8,
) -> IdentityBinding {
    let (_, binding) = custody
        .bootstrap_principal_under_custody(
            identity,
            BootstrapCustodyPrincipalRequest {
                authority_id: [tag; 16],
                principal_profile_digest: [tag; 32],
                control_domain_policy_digest: [tag.wrapping_add(1); 32],
                key_purpose: KeyPurpose::SemanticSigning,
                custody_tier: CustodyTier::OperatorFileSealed,
                valid_from_ms: 1_000,
                valid_until_ms: 1_000_000,
                seed: [seed_byte; 32],
                bootstrap_idempotency_key: IdempotencyKey::from_bytes(id16(tag, 1)),
                custody_binding_idempotency_key: IdempotencyKey::from_bytes(id16(tag, 2)),
                provisioned_at_ms: 1_000,
            },
        )
        .expect("bootstrap principal under custody");
    binding
}

impl FourLayerHarness {
    fn bootstrap(label: &str) -> Self {
        let temp_dir = TempSqliteDir::new(label);
        let identity = Arc::new(
            IdentityAuthority::open(temp_dir.path().join("id")).expect("open IdentityAuthority"),
        );
        let capability = Arc::new(
            CapabilityAuthority::open(temp_dir.path().join("cap"))
                .expect("open CapabilityAuthority"),
        );
        let resource = Arc::new(
            ResourceAuthority::open(temp_dir.path().join("res")).expect("open ResourceAuthority"),
        );
        let operations = Arc::new(OperationRegistry::new());
        let custody = SigningKeyCustodyVault::new_in_memory([0x42; 32]).expect("open custody");

        let issuer_binding = bootstrap_test_principal(&custody, &identity, 0x10, 0xA1);
        let caller_binding = bootstrap_test_principal(&custody, &identity, 0x20, 0xB2);
        let bystander_binding = bootstrap_test_principal(&custody, &identity, 0x30, 0xC3);
        let meter_binding = bootstrap_test_principal(&custody, &identity, 0x40, 0xD4);

        let namespace_a = CapabilityTarget::Namespace(NamespaceId::from_bytes([0xAA; 16]));
        let namespace_b = CapabilityTarget::Namespace(NamespaceId::from_bytes([0xBB; 16]));
        let purpose_digest = [0x7E; 32];

        let (issue_decision, _) = custody
            .issue_root_capability_under_custody(
                &capability,
                &identity,
                IssueRootCapabilityRequest {
                    issuer_key_id: issuer_binding.key_id,
                    holder_key_id: caller_binding.key_id,
                    target: namespace_a,
                    rights: CapabilityRights::SEMANTIC_APPEND
                        .union(CapabilityRights::SEMANTIC_ADJUDICATE)
                        .union(CapabilityRights::DELEGATE),
                    purpose_digest: Some(purpose_digest),
                    valid_from_ms: 1_000,
                    valid_until_ms: 900_000,
                    delegation_depth_remaining: 2,
                    call_limit: None,
                    idempotency_key: IdempotencyKey::from_bytes(id16(0x50, 1)),
                    issued_at_ms: 2_000,
                },
            )
            .expect("issue root capability");
        let root_capability = issue_decision.record();

        let account = resource
            .create_account(CreateAccountRequest {
                initial_credit: 1_000,
                idempotency_key: IdempotencyKey::from_bytes(id16(0x60, 1)),
                created_at_ms: 2_000,
            })
            .expect("create resource account");
        let driver = resource
            .register_driver(RegisterDriverRequest {
                profile_digest: [0x61; 32],
                idempotency_key: IdempotencyKey::from_bytes(id16(0x62, 1)),
                created_at_ms: 2_000,
            })
            .expect("register driver")
            .record();

        let client_epoch = 1_u32;
        let host_epoch = 2_u32;
        let gate = TrustedEnforcementGate::new(
            host_epoch,
            Arc::clone(&identity),
            Arc::clone(&capability),
            Arc::clone(&resource),
            operations,
            meter_binding.principal_id,
        );

        let process_id = TaskParticipantId::from_bytes([0x91; 16]);
        let process_generation = gen_u64(1);
        gate.register_process_incarnation(process_id, process_generation);

        let sabi_adapter =
            SabiEpochAdapterCell::with_canonical_manifest(client_epoch, host_epoch).expect("cell");
        let driver_shim =
            DriverProtocolShim::new_legacy_minicbor(host_epoch).expect("legacy driver shim");
        let shim = OutOfCoreCompatShimRuntime::new(sabi_adapter, driver_shim);

        Self {
            _temp_dir: temp_dir,
            client_epoch,
            host_epoch,
            identity,
            capability,
            resource,
            custody,
            issuer_binding,
            caller_binding,
            bystander_binding,
            meter_binding,
            account,
            driver,
            process_id,
            process_generation,
            namespace_a,
            namespace_b,
            purpose_digest,
            root_capability,
            gate,
            shim,
        }
    }

    fn make_operation_spec(&self, seq: u64) -> OperationSpec {
        let base_gen = u64::from(self.client_epoch);
        OperationSpec {
            operation_id: OperationId::from_bytes(id16(0x70, seq)),
            generation: gen_u64(base_gen),
            owner_fiber: FiberHandle {
                fiber_id: ExecutionFiberId::from_bytes(id16(0x71, seq)),
                generation: gen_u64(base_gen),
            },
            cancellation_scope_id: CancellationScopeId::from_bytes(id16(0x72, seq)),
            cancellation_generation: gen_u64(base_gen),
        }
    }

    fn reserve_for_operation(
        &self,
        seq: u64,
        op_id: OperationId,
        upper_bound: u64,
    ) -> ReservationRecord {
        let quote = self
            .resource
            .create_quote(CreateQuoteRequest {
                driver_id: self.driver.driver_id,
                driver_generation: self.driver.generation,
                driver_fencing_token: self.driver.fencing_token,
                operation_proposal_digest: [0x81; 32],
                pricing_version: [0x82; 32],
                upper_bound,
                demand_capacity: ResourceDemand {
                    cpu_shares: 100,
                    memory_mib: 256,
                    io_weight: 50,
                },
                valid_until_ms: 500_000,
                idempotency_key: IdempotencyKey::from_bytes(id16(0x83, seq)),
                created_at_ms: 3_000,
            })
            .expect("create quote")
            .record();

        self.resource
            .reserve(ReserveRequest {
                account_id: self.account.account_id,
                quote_id: quote.quote_id,
                call_id: CallId::from_bytes(id16(0x84, seq)),
                operation_id: op_id,
                idempotency_key: IdempotencyKey::from_bytes(id16(0x85, seq)),
                demand: ResourceDemand {
                    cpu_shares: 50,
                    memory_mib: 128,
                    io_weight: 20,
                },
                reserved_at_ms: 3_500,
            })
            .expect("reserve credit")
            .record()
    }

    fn build_client_legacy_register_envelope(
        &self,
        seq: u64,
        spec: &OperationSpec,
        idempotency_key: IdempotencyKey,
        deadline_monotonic_ns: u64,
        activity_context: Vec<u8>,
    ) -> ([u8; 16], Vec<u8>) {
        let req_id = id16(0x95, seq);
        let reg_cbor = encode_register_request(&RegisterOperationWire {
            operation_id: spec.operation_id.into_bytes(),
            operation_generation: spec.generation.get(),
            owner_fiber_id: spec.owner_fiber.fiber_id.into_bytes(),
            owner_fiber_generation: spec.owner_fiber.generation.get(),
            cancellation_scope_id: spec.cancellation_scope_id.into_bytes(),
            cancellation_generation: spec.cancellation_generation.get(),
        })
        .expect("encode legacy register cbor");

        let env = build_driver_request_envelope(
            self.client_epoch,
            0,
            REGISTER_PROVIDER_METHOD,
            req_id,
            idempotency_key,
            deadline_monotonic_ns,
            activity_context,
            reg_cbor,
        );
        (req_id, env.encode_to_vec())
    }

    #[allow(clippy::too_many_arguments)]
    fn sign_dispatch_auth(
        &self,
        signer_binding: &IdentityBinding,
        request_id: [u8; 16],
        idempotency_key: IdempotencyKey,
        deadline_monotonic_ns: u64,
        activity_context: &[u8],
        spec: &OperationSpec,
        mut auth: MediatedDriverDispatchAuth,
    ) -> MediatedDriverDispatchAuth {
        auth.caller_principal = signer_binding.principal_id;
        auth.caller_control_domain = signer_binding.control_domain_id;
        auth.caller_key_id = signer_binding.key_id;

        let event_id = compute_dispatch_semantic_event_id(
            request_id,
            idempotency_key,
            deadline_monotonic_ns,
            activity_context,
            spec,
            &auth,
        );
        let msg = semantic_signature_message(event_id);
        let audit = self
            .custody
            .sign_digest_under_custody(CustodySignRequest {
                key_id: signer_binding.key_id,
                expected_rotation_epoch: None,
                domain_tag: b"nlos/drv-shim/test-dispatch-sign/v1",
                message_digest: msg,
                signed_at_ms: auth.admitted_at_ms,
            })
            .expect("sign dispatch under custody");
        auth.caller_signature = audit.signature;
        auth
    }

    fn default_unsigned_auth(
        &self,
        seq: u64,
        reservation: &ReservationRecord,
        cap_handle: CapabilityHandle,
    ) -> MediatedDriverDispatchAuth {
        MediatedDriverDispatchAuth {
            caller_principal: self.caller_binding.principal_id,
            caller_control_domain: self.caller_binding.control_domain_id,
            caller_key_id: self.caller_binding.key_id,
            caller_signature: [0_u8; 64],
            process_id: self.process_id,
            process_generation: self.process_generation,
            capability_handle: cap_handle,
            capability_target: self.namespace_a,
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: Some(self.purpose_digest),
            reservation_id: reservation.reservation_id,
            call_id: reservation.call_id,
            activation_token: reservation.activation_token,
            driver_id: reservation.driver_id,
            driver_generation: reservation.driver_generation,
            driver_fencing_token: reservation.driver_fencing_token,
            credential_id: None,
            callback_id: CallbackId::from_bytes(id16(0x99, seq)),
            allow_verified_isolation: false,
            admitted_at_ms: 10_000,
            now_monotonic_ns: 50_000,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn sign_usage_proof(
        &self,
        signer_binding: &IdentityBinding,
        reservation_id: ReservationId,
        operation_id: OperationId,
        activation_receipt_id: ReceiptId,
        execution_receipt_id: ReceiptId,
        sequence: u64,
        cumulative_usage: u64,
        settled_at_ms: u64,
    ) -> SignedMeteredUsageProof {
        let effect_closed_proof_digest = [0xEE; 32];
        let digest = compute_metered_usage_proof_digest(
            reservation_id,
            operation_id,
            activation_receipt_id,
            execution_receipt_id,
            sequence,
            cumulative_usage,
            effect_closed_proof_digest,
        );
        let audit = self
            .custody
            .sign_digest_under_custody(CustodySignRequest {
                key_id: signer_binding.key_id,
                expected_rotation_epoch: None,
                domain_tag: b"nlos/drv-shim/test-meter-sign/v1",
                message_digest: digest,
                signed_at_ms: settled_at_ms,
            })
            .expect("sign meter proof under custody");
        SignedMeteredUsageProof {
            meter_principal: signer_binding.principal_id,
            meter_control_domain: signer_binding.control_domain_id,
            meter_key_id: signer_binding.key_id,
            meter_signature: audit.signature,
            reservation_id,
            operation_id,
            activation_receipt_id,
            execution_receipt_id,
            sequence,
            cumulative_usage,
            effect_closed_proof_digest,
            settled_at_ms,
        }
    }

    fn delegate_attenuated_capability(
        &self,
        seq: u64,
        recipient: &IdentityBinding,
        target: CapabilityTarget,
        rights: CapabilityRights,
        call_limit: Option<u64>,
    ) -> CapabilityRecord {
        let cmd = DelegateCapabilityRequest {
            parent: self.root_capability.handle,
            delegator_key_id: self.caller_binding.key_id,
            recipient_key_id: recipient.key_id,
            target,
            rights,
            purpose_digest: Some(self.purpose_digest),
            valid_from_ms: 2_000,
            valid_until_ms: 800_000,
            delegation_depth_remaining: 0,
            call_limit,
            idempotency_key: IdempotencyKey::from_bytes(id16(0xA7, seq)),
            delegated_at_ms: 4_000,
        };
        let digest = delegate_command_message(cmd);
        let audit = self
            .custody
            .sign_digest_under_custody(CustodySignRequest {
                key_id: self.caller_binding.key_id,
                expected_rotation_epoch: None,
                domain_tag: b"nlos/capability/delegate/v1",
                message_digest: digest,
                signed_at_ms: 4_000,
            })
            .expect("sign delegate");
        self.capability
            .delegate_signed(
                &self.identity,
                SignedDelegateCapabilityRequest {
                    command: cmd,
                    signer: self.caller_binding.principal_id,
                    signature: audit.signature,
                },
            )
            .expect("delegate capability")
            .record()
    }
}

// ============================================================================
// Baseline End-to-End 4-Layer Positive & Canonical Host Epoch Boundary Proofs
// ============================================================================

#[test]
fn four_layer_e2e_n_minus_1_legacy_minicbor_dispatch_and_metered_completion_succeeds() {
    let mut h = FourLayerHarness::bootstrap("e2e-pos");
    let spec = h.make_operation_spec(1);
    let reservation = h.reserve_for_operation(1, spec.operation_id, 100);
    let idemp_key = IdempotencyKey::from_bytes(id16(0xB1, 1));
    let activity = b"trace-span-01".to_vec();
    let deadline_ns = 90_000;

    let (req_id, client_wire) =
        h.build_client_legacy_register_envelope(1, &spec, idemp_key, deadline_ns, activity.clone());
    let host_wire = h
        .shim
        .translate_legacy_client_dispatch_to_host(&client_wire)
        .expect("shim upgrades N-1 + minicbor -> N + DriverProviderRequest");

    let unsigned = h.default_unsigned_auth(1, &reservation, h.root_capability.handle);
    let signed_auth = h.sign_dispatch_auth(
        &h.caller_binding,
        req_id,
        idemp_key,
        deadline_ns,
        &activity,
        &spec,
        unsigned,
    );

    let dispatch_receipt = h
        .gate
        .admit_mediated_driver_dispatch(&host_wire, &signed_auth)
        .expect("TCB gate admits mediated dispatch");
    assert!(!dispatch_receipt.replayed);

    // Exact idempotent replay returns replayed == true without double-activating.
    let replay_receipt = h
        .gate
        .admit_mediated_driver_dispatch(&host_wire, &signed_auth)
        .expect("idempotent replay succeeds");
    assert!(replay_receipt.replayed);
    assert_eq!(
        replay_receipt.activation_receipt.receipt_id,
        dispatch_receipt.activation_receipt.receipt_id
    );

    // Driver completes in Stage B minicbor; shim translates to frozen CompleteAck + downgraded client frame.
    let exec_receipt_id = ReceiptId::from_bytes(id16(0xC1, 1));
    let legacy_comp_cbor = encode_complete_result(&CompleteOperationResultWire {
        replayed: false,
        outcome_code: OUTCOME_CODE_COMPLETED,
        receipt_id: exec_receipt_id.into_bytes(),
        state_code: STATE_CODE_COMPLETED,
    })
    .expect("encode legacy complete result");

    let (host_resp_wire, client_resp_wire) = h
        .shim
        .translate_legacy_driver_completion_bidirectional(
            req_id,
            &legacy_comp_cbor,
            LegacyCompleteBridgeContext {
                handle: OperationHandle {
                    operation_id: spec.operation_id,
                    generation: spec.generation,
                },
                callback_id: signed_auth.callback_id,
                seed: [0x5A; 32],
                fenced_reconciliation_only: false,
            },
        )
        .expect("bidirectional completion translation");

    let usage_proof = h.sign_usage_proof(
        &h.meter_binding,
        reservation.reservation_id,
        spec.operation_id,
        dispatch_receipt.activation_receipt.receipt_id,
        exec_receipt_id,
        1,
        35,
        20_000,
    );

    let completion = h
        .gate
        .complete_mediated_driver_call(&host_resp_wire, &usage_proof)
        .expect("TCB gate settles usage and completes operation");
    assert_eq!(completion.finalization_receipt.final_usage, 35);
    assert_eq!(completion.finalization_receipt.refund_credit, 65);
    assert_eq!(
        completion.provider_execution_receipt.isolation_marker,
        PROVIDER_ISOLATION_UNVERIFIED_TAG
    );

    let op_snap = h
        .gate
        .operations()
        .inspect(OperationHandle {
            operation_id: spec.operation_id,
            generation: spec.generation,
        })
        .expect("inspect completed operation");
    assert_eq!(
        op_snap.state,
        OperationState::Completed {
            receipt_id: exec_receipt_id
        }
    );

    let client_decoded = Envelope::decode(client_resp_wire.as_slice()).expect("client envelope");
    assert_eq!(client_decoded.schema.expect("schema").major, h.client_epoch);
}

#[test]
fn tcb_enforcement_gate_rejects_non_canonical_client_epoch_frames_directly() {
    let h = FourLayerHarness::bootstrap("non-canonical-epoch");
    let spec = h.make_operation_spec(2);
    let reservation = h.reserve_for_operation(2, spec.operation_id, 80);
    let idemp_key = IdempotencyKey::from_bytes(id16(0xB2, 2));
    let activity = b"direct-bypass-attempt".to_vec();
    let (req_id, raw_n_minus_1_client_wire) =
        h.build_client_legacy_register_envelope(2, &spec, idemp_key, 90_000, activity.clone());

    let unsigned = h.default_unsigned_auth(2, &reservation, h.root_capability.handle);
    let signed_auth = h.sign_dispatch_auth(
        &h.caller_binding,
        req_id,
        idemp_key,
        90_000,
        &activity,
        &spec,
        unsigned,
    );

    let err = h
        .gate
        .admit_mediated_driver_dispatch(&raw_n_minus_1_client_wire, &signed_auth)
        .expect_err("untranslated N-1 frame must be rejected by TCB gate");
    assert!(matches!(
        err,
        BoundaryViolation::NonCanonicalHostEpoch {
            expected: 2,
            actual: 1
        }
    ));
    assert_eq!(err.to_sabi_failure().code, SabiErrorCode::AbiVersion as i32);
}

// ============================================================================
// Category 1: [DRV-CRED-001] Zero Raw Secret Leakage Across Out-of-Core Shim
// ============================================================================

#[test]
fn drv_cred_001_zero_secret_bytes_appear_in_any_sabi_or_driver_shim_wire_frame() {
    let mut h = FourLayerHarness::bootstrap("cred-zero-leak");
    let raw_api_secret = b"sk-live-SUPER-SECRET-TCB-CREDENTIAL-998877665544332211".to_vec();
    let raw_oauth_secret = b"oauth2-refresh-ULTRA-SENSITIVE-TOKEN-AABBCCDDEEFF".to_vec();
    let cred_id_1 = [0xC1; 16];
    let cred_id_2 = [0xC2; 16];

    h.gate.register_secret_credential(
        SecretCredential::new(cred_id_1, raw_api_secret.clone()).expect("cred 1"),
    );
    h.gate.register_secret_credential(
        SecretCredential::new(cred_id_2, raw_oauth_secret.clone()).expect("cred 2"),
    );

    let spec = h.make_operation_spec(3);
    let reservation = h.reserve_for_operation(3, spec.operation_id, 120);
    let idemp_key = IdempotencyKey::from_bytes(id16(0xB3, 3));
    let activity = b"cred-bound-call".to_vec();

    let (req_id, client_wire) =
        h.build_client_legacy_register_envelope(3, &spec, idemp_key, 95_000, activity.clone());
    let host_wire = h
        .shim
        .translate_legacy_client_dispatch_to_host(&client_wire)
        .expect("shim translation");

    let mut unsigned = h.default_unsigned_auth(3, &reservation, h.root_capability.handle);
    unsigned.credential_id = Some(cred_id_1);
    let signed_auth = h.sign_dispatch_auth(
        &h.caller_binding,
        req_id,
        idemp_key,
        95_000,
        &activity,
        &spec,
        unsigned,
    );

    let dispatch_receipt = h
        .gate
        .admit_mediated_driver_dispatch(&host_wire, &signed_auth)
        .expect("TCB gate binds credential inside TCB");
    assert!(dispatch_receipt.tcb_credential_commitment.is_some());

    let exec_receipt_id = ReceiptId::from_bytes(id16(0xC3, 3));
    let legacy_comp_cbor = encode_complete_result(&CompleteOperationResultWire {
        replayed: false,
        outcome_code: OUTCOME_CODE_COMPLETED,
        receipt_id: exec_receipt_id.into_bytes(),
        state_code: STATE_CODE_COMPLETED,
    })
    .expect("encode complete");
    let (host_resp_wire, _) = h
        .shim
        .translate_legacy_driver_completion_bidirectional(
            req_id,
            &legacy_comp_cbor,
            LegacyCompleteBridgeContext {
                handle: OperationHandle {
                    operation_id: spec.operation_id,
                    generation: spec.generation,
                },
                callback_id: signed_auth.callback_id,
                seed: [0x11; 32],
                fenced_reconciliation_only: false,
            },
        )
        .expect("translate completion");

    let usage_proof = h.sign_usage_proof(
        &h.meter_binding,
        reservation.reservation_id,
        spec.operation_id,
        dispatch_receipt.activation_receipt.receipt_id,
        exec_receipt_id,
        1,
        25,
        21_000,
    );
    let _ = h
        .gate
        .complete_mediated_driver_call(&host_resp_wire, &usage_proof)
        .expect("complete");

    // Scan 100% of raw frames captured across all 6 directional queues of the out-of-core shim!
    let transcript = h.shim.transcript();
    assert_ne!(transcript.client_ingress_frames.len(), 0);
    assert_ne!(transcript.driver_ingress_frames.len(), 0);
    assert_ne!(transcript.host_egress_frames.len(), 0);
    assert_ne!(transcript.host_ingress_frames.len(), 0);
    assert_ne!(transcript.driver_egress_frames.len(), 0);
    assert_ne!(transcript.client_egress_frames.len(), 0);
    assert_eq!(transcript.all_frames().len(), 7);
    assert!(!transcript.contains_byte_subsequence(&raw_api_secret));
    assert!(!transcript.contains_byte_subsequence(&raw_oauth_secret));
    assert!(!transcript.contains_byte_subsequence(b"SUPER-SECRET-TCB"));
    assert!(!transcript.contains_byte_subsequence(b"ULTRA-SENSITIVE"));
    for frame in transcript.all_frames() {
        assert!(!h.gate.wire_contains_any_registered_secret(frame));
    }
}

#[test]
fn drv_cred_001_secret_credential_redacts_debug_display_and_rejects_empty_or_unknown_handles() {
    assert!(matches!(
        SecretCredential::new([0x01; 16], Vec::new()),
        Err(BoundaryViolation::EmptySecretCredential)
    ));

    let secret_str = "TOP-SECRET-KEY-MATERIAL-DO-NOT-PRINT";
    let cred =
        SecretCredential::new([0xAB; 16], secret_str.as_bytes().to_vec()).expect("valid secret");
    assert_eq!(cred.secret_len(), secret_str.len());
    let dbg_out = format!("{cred:?}");
    let disp_out = format!("{cred}");
    assert!(dbg_out.contains("<redacted-secret>"));
    assert!(disp_out.contains("<redacted-secret>"));
    assert!(!dbg_out.contains(secret_str));
    assert!(!disp_out.contains(secret_str));

    // Referencing an unknown credential_id fails closed without consuming capability or reservation.
    let mut h = FourLayerHarness::bootstrap("cred-unknown");
    let spec = h.make_operation_spec(4);
    let reservation = h.reserve_for_operation(4, spec.operation_id, 50);
    let idemp_key = IdempotencyKey::from_bytes(id16(0xB4, 4));
    let activity = b"unknown-cred".to_vec();
    let (req_id, client_wire) =
        h.build_client_legacy_register_envelope(4, &spec, idemp_key, 90_000, activity.clone());
    let host_wire = h
        .shim
        .translate_legacy_client_dispatch_to_host(&client_wire)
        .expect("shim");

    let mut unsigned = h.default_unsigned_auth(4, &reservation, h.root_capability.handle);
    unsigned.credential_id = Some([0xFF; 16]);
    let signed_auth = h.sign_dispatch_auth(
        &h.caller_binding,
        req_id,
        idemp_key,
        90_000,
        &activity,
        &spec,
        unsigned,
    );
    let err = h
        .gate
        .admit_mediated_driver_dispatch(&host_wire, &signed_auth)
        .expect_err("unknown credential handle must fail closed");
    assert!(matches!(err, BoundaryViolation::UnknownCredential(_)));
    assert_eq!(err.to_sabi_failure().code, SabiErrorCode::Rights as i32);
    assert_eq!(
        h.resource
            .inspect_permit_binding(reservation.reservation_id)
            .expect("reservation untouched")
            .state,
        ReservationState::Reserved
    );
}

// ============================================================================
// Category 2: [DRV-SHIM-001] Capability Tampering, Forgery, Revocation & Attenuation
// ============================================================================

#[test]
fn drv_shim_001_mid_transit_shim_tampering_of_capability_id_fails_closed() {
    let mut h = FourLayerHarness::bootstrap("tamper-cap-id");
    let spec = h.make_operation_spec(5);
    let reservation = h.reserve_for_operation(5, spec.operation_id, 50);
    let idemp_key = IdempotencyKey::from_bytes(id16(0xB5, 5));
    let activity = b"tamper-cap-id".to_vec();
    let (req_id, client_wire) =
        h.build_client_legacy_register_envelope(5, &spec, idemp_key, 90_000, activity.clone());
    let host_wire = h
        .shim
        .translate_legacy_client_dispatch_to_host(&client_wire)
        .expect("shim");

    let unsigned = h.default_unsigned_auth(5, &reservation, h.root_capability.handle);
    let mut tampered_auth = h.sign_dispatch_auth(
        &h.caller_binding,
        req_id,
        idemp_key,
        90_000,
        &activity,
        &spec,
        unsigned,
    );
    // Compromised shim mutates CapabilityId in transit!
    tampered_auth.capability_handle.capability_id = CapabilityId::from_bytes([0xEE; 16]);

    let err = h
        .gate
        .admit_mediated_driver_dispatch(&host_wire, &tampered_auth)
        .expect_err("mid-transit CapabilityId tampering must fail signature check");
    assert!(matches!(
        err,
        BoundaryViolation::Identity(IdentityAuthorityError::InvalidSignature)
    ));
    assert_eq!(
        h.resource
            .inspect_permit_binding(reservation.reservation_id)
            .expect("reservation still Reserved")
            .state,
        ReservationState::Reserved
    );
}

#[test]
fn drv_shim_001_mid_transit_shim_tampering_of_capability_generation_fails_closed() {
    let mut h = FourLayerHarness::bootstrap("tamper-cap-gen");
    let spec = h.make_operation_spec(6);
    let reservation = h.reserve_for_operation(6, spec.operation_id, 50);
    let idemp_key = IdempotencyKey::from_bytes(id16(0xB6, 6));
    let activity = b"tamper-cap-gen".to_vec();
    let (req_id, client_wire) =
        h.build_client_legacy_register_envelope(6, &spec, idemp_key, 90_000, activity.clone());
    let host_wire = h
        .shim
        .translate_legacy_client_dispatch_to_host(&client_wire)
        .expect("shim");

    let unsigned = h.default_unsigned_auth(6, &reservation, h.root_capability.handle);
    let mut tampered_auth = h.sign_dispatch_auth(
        &h.caller_binding,
        req_id,
        idemp_key,
        90_000,
        &activity,
        &spec,
        unsigned,
    );
    // Compromised shim bumps Capability Generation in transit!
    tampered_auth.capability_handle.generation = gen_u64(99);

    let err = h
        .gate
        .admit_mediated_driver_dispatch(&host_wire, &tampered_auth)
        .expect_err("mid-transit Capability Generation tampering must fail closed");
    assert!(matches!(
        err,
        BoundaryViolation::Identity(IdentityAuthorityError::InvalidSignature)
    ));
}

#[test]
fn drv_shim_001_forged_unknown_capability_handle_signed_by_caller_fails_closed() {
    let mut h = FourLayerHarness::bootstrap("forged-cap");
    let spec = h.make_operation_spec(7);
    let reservation = h.reserve_for_operation(7, spec.operation_id, 50);
    let idemp_key = IdempotencyKey::from_bytes(id16(0xB7, 7));
    let activity = b"forged-cap".to_vec();
    let (req_id, client_wire) =
        h.build_client_legacy_register_envelope(7, &spec, idemp_key, 90_000, activity.clone());
    let host_wire = h
        .shim
        .translate_legacy_client_dispatch_to_host(&client_wire)
        .expect("shim");

    let forged_handle = CapabilityHandle {
        capability_id: CapabilityId::from_bytes([0xFD; 16]),
        generation: gen_u64(1),
    };
    let unsigned = h.default_unsigned_auth(7, &reservation, forged_handle);
    let signed_auth = h.sign_dispatch_auth(
        &h.caller_binding,
        req_id,
        idemp_key,
        90_000,
        &activity,
        &spec,
        unsigned,
    );

    let err = h
        .gate
        .admit_mediated_driver_dispatch(&host_wire, &signed_auth)
        .expect_err("fabricated CapabilityHandle signed by caller must fail closed");
    assert!(matches!(
        err,
        BoundaryViolation::Capability(CapabilityAuthorityError::CapabilityNotFound(_))
    ));
    assert_eq!(
        h.resource
            .inspect_permit_binding(reservation.reservation_id)
            .expect("reservation untouched")
            .state,
        ReservationState::Reserved
    );
}

#[test]
fn drv_shim_001_revoked_capability_handle_replay_fails_closed() {
    let mut h = FourLayerHarness::bootstrap("revoked-cap");
    let revoke_cmd = RevokeCapabilityRequest {
        handle: h.root_capability.handle,
        revoker_key_id: h.issuer_binding.key_id,
        idempotency_key: IdempotencyKey::from_bytes(id16(0xD1, 8)),
        revoked_at_ms: 5_000,
    };
    let revoke_digest = revoke_command_message(revoke_cmd);
    let revoke_sig = h
        .custody
        .sign_digest_under_custody(CustodySignRequest {
            key_id: h.issuer_binding.key_id,
            expected_rotation_epoch: None,
            domain_tag: b"nlos/capability/revoke/v1",
            message_digest: revoke_digest,
            signed_at_ms: 5_000,
        })
        .expect("sign revoke")
        .signature;
    h.capability
        .revoke_signed(
            &h.identity,
            SignedRevokeCapabilityRequest {
                command: revoke_cmd,
                signer: h.issuer_binding.principal_id,
                signature: revoke_sig,
            },
        )
        .expect("revoke capability");

    let spec = h.make_operation_spec(8);
    let reservation = h.reserve_for_operation(8, spec.operation_id, 50);
    let idemp_key = IdempotencyKey::from_bytes(id16(0xB8, 8));
    let activity = b"revoked-cap".to_vec();
    let (req_id, client_wire) =
        h.build_client_legacy_register_envelope(8, &spec, idemp_key, 90_000, activity.clone());
    let host_wire = h
        .shim
        .translate_legacy_client_dispatch_to_host(&client_wire)
        .expect("shim");

    let unsigned = h.default_unsigned_auth(8, &reservation, h.root_capability.handle);
    let signed_auth = h.sign_dispatch_auth(
        &h.caller_binding,
        req_id,
        idemp_key,
        90_000,
        &activity,
        &spec,
        unsigned,
    );

    let err = h
        .gate
        .admit_mediated_driver_dispatch(&host_wire, &signed_auth)
        .expect_err("revoked CapabilityHandle must fail closed");
    assert!(matches!(err, BoundaryViolation::Capability(_)));
    assert_eq!(err.to_sabi_failure().code, SabiErrorCode::Rights as i32);
    assert_eq!(
        h.resource
            .inspect_permit_binding(reservation.reservation_id)
            .expect("reservation untouched")
            .state,
        ReservationState::Reserved
    );
}

#[test]
fn drv_shim_001_revoked_caller_signing_key_fails_closed() {
    let mut h = FourLayerHarness::bootstrap("revoked-key");
    let spec = h.make_operation_spec(9);
    let reservation = h.reserve_for_operation(9, spec.operation_id, 50);
    let idemp_key = IdempotencyKey::from_bytes(id16(0xB9, 9));
    let activity = b"revoked-key".to_vec();
    let (req_id, client_wire) =
        h.build_client_legacy_register_envelope(9, &spec, idemp_key, 90_000, activity.clone());
    let host_wire = h
        .shim
        .translate_legacy_client_dispatch_to_host(&client_wire)
        .expect("shim");

    // Pre-sign while key is still active, then revoke key in IdentityAuthority before TCB admission!
    let unsigned = h.default_unsigned_auth(9, &reservation, h.root_capability.handle);
    let signed_auth = h.sign_dispatch_auth(
        &h.caller_binding,
        req_id,
        idemp_key,
        90_000,
        &activity,
        &spec,
        unsigned,
    );

    h.identity
        .revoke_key(RevokeKeyRequest {
            key_id: h.caller_binding.key_id,
            expected_key_generation: h.caller_binding.key_generation,
            expected_identity_snapshot_id: h.caller_binding.identity_snapshot_id,
            idempotency_key: IdempotencyKey::from_bytes(id16(0xD2, 9)),
            revoked_at_ms: 9_000,
        })
        .expect("revoke caller key in IdentityAuthority");

    let err = h
        .gate
        .admit_mediated_driver_dispatch(&host_wire, &signed_auth)
        .expect_err("revoked caller key must fail closed");
    assert!(matches!(
        err,
        BoundaryViolation::Identity(IdentityAuthorityError::KeyRevoked)
    ));
}

#[test]
fn drv_shim_001_wrong_holder_principal_stealing_capability_handle_fails_closed() {
    let mut h = FourLayerHarness::bootstrap("wrong-holder");
    let spec = h.make_operation_spec(10);
    let reservation = h.reserve_for_operation(10, spec.operation_id, 50);
    let idemp_key = IdempotencyKey::from_bytes(id16(0xBA, 10));
    let activity = b"stolen-cap".to_vec();
    let (req_id, client_wire) =
        h.build_client_legacy_register_envelope(10, &spec, idemp_key, 90_000, activity.clone());
    let host_wire = h
        .shim
        .translate_legacy_client_dispatch_to_host(&client_wire)
        .expect("shim");

    // Bystander principal signs a valid request trying to exercise caller_binding's CapabilityHandle!
    let unsigned = h.default_unsigned_auth(10, &reservation, h.root_capability.handle);
    let stolen_auth = h.sign_dispatch_auth(
        &h.bystander_binding,
        req_id,
        idemp_key,
        90_000,
        &activity,
        &spec,
        unsigned,
    );

    let err = h
        .gate
        .admit_mediated_driver_dispatch(&host_wire, &stolen_auth)
        .expect_err("non-holder principal exercising stolen handle must fail closed");
    assert!(matches!(
        err,
        BoundaryViolation::Capability(CapabilityAuthorityError::HolderMismatch)
    ));
}

#[test]
fn drv_shim_001_attenuated_delegated_capability_missing_required_right_or_wrong_target_fails_closed()
 {
    let mut h = FourLayerHarness::bootstrap("attenuated-cap");
    let child_cap = h.delegate_attenuated_capability(
        11,
        &h.bystander_binding,
        h.namespace_a,
        CapabilityRights::SEMANTIC_APPEND,
        None,
    );

    // 1. Missing right: child only has SEMANTIC_APPEND, attempts SEMANTIC_ADJUDICATE.
    let spec1 = h.make_operation_spec(11);
    let first_reservation = h.reserve_for_operation(11, spec1.operation_id, 50);
    let idemp1 = IdempotencyKey::from_bytes(id16(0xBB, 11));
    let act1 = b"missing-right".to_vec();
    let (req1, cwire1) =
        h.build_client_legacy_register_envelope(11, &spec1, idemp1, 90_000, act1.clone());
    let hwire1 = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire1)
        .expect("shim");

    let mut unsigned1 = h.default_unsigned_auth(11, &first_reservation, child_cap.handle);
    unsigned1.required_right = CapabilityRights::SEMANTIC_ADJUDICATE;
    let signed1 = h.sign_dispatch_auth(
        &h.bystander_binding,
        req1,
        idemp1,
        90_000,
        &act1,
        &spec1,
        unsigned1,
    );
    let err1 = h
        .gate
        .admit_mediated_driver_dispatch(&hwire1, &signed1)
        .expect_err("escalated right must fail closed");
    assert!(matches!(
        err1,
        BoundaryViolation::Capability(CapabilityAuthorityError::RequiredRightMissing)
    ));

    // 2. Wrong target: child is scoped to namespace_a, attempts namespace_b.
    let mut unsigned2 = h.default_unsigned_auth(11, &first_reservation, child_cap.handle);
    unsigned2.capability_target = h.namespace_b;
    let idemp2 = IdempotencyKey::from_bytes(id16(0xBB, 112));
    let (req2, cwire2) =
        h.build_client_legacy_register_envelope(112, &spec1, idemp2, 90_000, act1.clone());
    let hwire2 = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire2)
        .expect("shim");
    let signed2 = h.sign_dispatch_auth(
        &h.bystander_binding,
        req2,
        idemp2,
        90_000,
        &act1,
        &spec1,
        unsigned2,
    );
    let err2 = h
        .gate
        .admit_mediated_driver_dispatch(&hwire2, &signed2)
        .expect_err("cross-namespace target mismatch must fail closed");
    assert!(matches!(
        err2,
        BoundaryViolation::Capability(CapabilityAuthorityError::TargetMismatch)
    ));
}

#[test]
fn drv_shim_001_exhausted_capability_call_limit_blocks_second_dispatch_without_activating_reservation()
 {
    let mut h = FourLayerHarness::bootstrap("call-limit");
    let one_shot_cap = h.delegate_attenuated_capability(
        12,
        &h.bystander_binding,
        h.namespace_a,
        CapabilityRights::SEMANTIC_APPEND,
        Some(1),
    );

    // First call consumes the single permitted call_limit == 1.
    let spec1 = h.make_operation_spec(121);
    let first_reservation = h.reserve_for_operation(121, spec1.operation_id, 40);
    let idemp1 = IdempotencyKey::from_bytes(id16(0xBC, 121));
    let act = b"one-shot".to_vec();
    let (req1, cwire1) =
        h.build_client_legacy_register_envelope(121, &spec1, idemp1, 90_000, act.clone());
    let hwire1 = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire1)
        .expect("shim");
    let auth1 = h.sign_dispatch_auth(
        &h.bystander_binding,
        req1,
        idemp1,
        90_000,
        &act,
        &spec1,
        h.default_unsigned_auth(121, &first_reservation, one_shot_cap.handle),
    );
    let ok1 = h
        .gate
        .admit_mediated_driver_dispatch(&hwire1, &auth1)
        .expect("first dispatch within call_limit=1 succeeds");
    assert_eq!(ok1.capability_receipt.remaining, Some(0));

    // Second call with a fresh Reserved reservation must fail closed on CallLimitExhausted
    // BEFORE activating second_reservation!
    let spec2 = h.make_operation_spec(122);
    let second_reservation = h.reserve_for_operation(122, spec2.operation_id, 40);
    let idemp2 = IdempotencyKey::from_bytes(id16(0xBC, 122));
    let (req2, cwire2) =
        h.build_client_legacy_register_envelope(122, &spec2, idemp2, 90_000, act.clone());
    let hwire2 = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire2)
        .expect("shim");
    let auth2 = h.sign_dispatch_auth(
        &h.bystander_binding,
        req2,
        idemp2,
        90_000,
        &act,
        &spec2,
        h.default_unsigned_auth(122, &second_reservation, one_shot_cap.handle),
    );
    let err2 = h
        .gate
        .admit_mediated_driver_dispatch(&hwire2, &auth2)
        .expect_err("exhausted call_limit must block second dispatch");
    assert!(matches!(
        err2,
        BoundaryViolation::Capability(CapabilityAuthorityError::CallLimitExhausted)
    ));
    assert_eq!(
        h.resource
            .inspect_permit_binding(second_reservation.reservation_id)
            .expect("second_reservation was NOT activated on CallLimitExhausted")
            .state,
        ReservationState::Reserved
    );
}

// ============================================================================
// Category 3: [DRV-BOUND-001] & [DRV-ACTIVE-001] Reservation, Usage & Incarnation Fences
// ============================================================================

#[test]
fn drv_active_001_missing_or_unknown_reservation_fails_closed_without_consuming_capability() {
    let mut h = FourLayerHarness::bootstrap("missing-res");
    let limited_cap = h.delegate_attenuated_capability(
        13,
        &h.bystander_binding,
        h.namespace_a,
        CapabilityRights::SEMANTIC_APPEND,
        Some(1),
    );

    let spec = h.make_operation_spec(13);
    let real_res = h.reserve_for_operation(13, spec.operation_id, 40);
    let idemp_bad = IdempotencyKey::from_bytes(id16(0xBD, 130));
    let act = b"verify-then-commit".to_vec();
    let (req_bad, cwire_bad) =
        h.build_client_legacy_register_envelope(130, &spec, idemp_bad, 90_000, act.clone());
    let hwire_bad = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire_bad)
        .expect("shim");

    let mut bad_unsigned = h.default_unsigned_auth(130, &real_res, limited_cap.handle);
    bad_unsigned.reservation_id = ReservationId::from_bytes([0xFE; 16]);
    let bad_auth = h.sign_dispatch_auth(
        &h.bystander_binding,
        req_bad,
        idemp_bad,
        90_000,
        &act,
        &spec,
        bad_unsigned,
    );

    let err = h
        .gate
        .admit_mediated_driver_dispatch(&hwire_bad, &bad_auth)
        .expect_err("unknown ReservationId must fail closed in Verify Phase");
    assert!(matches!(
        err,
        BoundaryViolation::Resource(ResourceAuthorityError::ReservationNotFound)
    ));

    // Prove Verify-Then-Commit preserved limited_cap's single call_limit=1!
    let idemp_good = IdempotencyKey::from_bytes(id16(0xBD, 131));
    let (req_good, cwire_good) =
        h.build_client_legacy_register_envelope(131, &spec, idemp_good, 90_000, act.clone());
    let hwire_good = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire_good)
        .expect("shim");
    let good_auth = h.sign_dispatch_auth(
        &h.bystander_binding,
        req_good,
        idemp_good,
        90_000,
        &act,
        &spec,
        h.default_unsigned_auth(131, &real_res, limited_cap.handle),
    );
    h.gate
        .admit_mediated_driver_dispatch(&hwire_good, &good_auth)
        .expect("single call_limit=1 was preserved across prior ReservationNotFound rejection");
}

#[test]
fn drv_active_001_cross_operation_hijacked_reservation_or_wrong_activation_token_fails_closed() {
    let mut h = FourLayerHarness::bootstrap("hijacked-res");
    let spec_a = h.make_operation_spec(141);
    let spec_b = h.make_operation_spec(142);
    let res_for_a = h.reserve_for_operation(141, spec_a.operation_id, 50);

    // 1. Present res_for_a while dispatching spec_b -> ReservationBindingMismatch.
    let idemp1 = IdempotencyKey::from_bytes(id16(0xBE, 141));
    let act = b"hijack".to_vec();
    let (req1, cwire1) =
        h.build_client_legacy_register_envelope(141, &spec_b, idemp1, 90_000, act.clone());
    let hwire1 = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire1)
        .expect("shim");
    let auth1 = h.sign_dispatch_auth(
        &h.caller_binding,
        req1,
        idemp1,
        90_000,
        &act,
        &spec_b,
        h.default_unsigned_auth(141, &res_for_a, h.root_capability.handle),
    );
    let err1 = h
        .gate
        .admit_mediated_driver_dispatch(&hwire1, &auth1)
        .expect_err("cross-operation hijacked reservation must fail closed");
    assert!(matches!(
        err1,
        BoundaryViolation::Resource(ResourceAuthorityError::ReservationBindingMismatch)
    ));

    // 2. Present res_for_a for spec_a, but with wrong activation_token -> ReservationBindingMismatch.
    let idemp2 = IdempotencyKey::from_bytes(id16(0xBE, 142));
    let (req2, cwire2) =
        h.build_client_legacy_register_envelope(142, &spec_a, idemp2, 90_000, act.clone());
    let hwire2 = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire2)
        .expect("shim");
    let mut bad_tok = h.default_unsigned_auth(142, &res_for_a, h.root_capability.handle);
    bad_tok.activation_token[0] ^= 0xFF;
    let auth2 = h.sign_dispatch_auth(
        &h.caller_binding,
        req2,
        idemp2,
        90_000,
        &act,
        &spec_a,
        bad_tok,
    );
    let err2 = h
        .gate
        .admit_mediated_driver_dispatch(&hwire2, &auth2)
        .expect_err("wrong activation_token must fail closed");
    assert!(matches!(
        err2,
        BoundaryViolation::Resource(ResourceAuthorityError::ReservationBindingMismatch)
    ));
}

#[test]
fn drv_active_001_duplicate_activation_of_already_active_reservation_with_new_idempotency_key_fails_closed()
 {
    let mut h = FourLayerHarness::bootstrap("dup-activate");
    let spec1 = h.make_operation_spec(151);
    let res = h.reserve_for_operation(151, spec1.operation_id, 60);
    let idemp1 = IdempotencyKey::from_bytes(id16(0xBF, 151));
    let act = b"activate-once".to_vec();
    let (req1, cwire1) =
        h.build_client_legacy_register_envelope(151, &spec1, idemp1, 90_000, act.clone());
    let hwire1 = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire1)
        .expect("shim");
    let auth1 = h.sign_dispatch_auth(
        &h.caller_binding,
        req1,
        idemp1,
        90_000,
        &act,
        &spec1,
        h.default_unsigned_auth(151, &res, h.root_capability.handle),
    );
    h.gate
        .admit_mediated_driver_dispatch(&hwire1, &auth1)
        .expect("initial activation succeeds");

    // Attempt to reuse the already-Active reservation on a new operation & new idempotency key!
    let spec2 = h.make_operation_spec(152);
    let idemp2 = IdempotencyKey::from_bytes(id16(0xBF, 152));
    let (req2, cwire2) =
        h.build_client_legacy_register_envelope(152, &spec2, idemp2, 90_000, act.clone());
    let hwire2 = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire2)
        .expect("shim");
    let auth2 = h.sign_dispatch_auth(
        &h.caller_binding,
        req2,
        idemp2,
        90_000,
        &act,
        &spec2,
        h.default_unsigned_auth(152, &res, h.root_capability.handle),
    );
    let err = h
        .gate
        .admit_mediated_driver_dispatch(&hwire2, &auth2)
        .expect_err("already-Active reservation reuse must fail closed");
    assert!(matches!(
        err,
        BoundaryViolation::Resource(ResourceAuthorityError::ReservationAlreadyActive)
    ));
}

#[test]
fn drv_active_001_forged_wrong_signer_or_tampered_usage_receipt_fails_closed() {
    let mut h = FourLayerHarness::bootstrap("forged-usage");
    let spec = h.make_operation_spec(16);
    let res = h.reserve_for_operation(16, spec.operation_id, 100);
    let idemp = IdempotencyKey::from_bytes(id16(0xC0, 16));
    let act = b"metered".to_vec();
    let (req_id, cwire) =
        h.build_client_legacy_register_envelope(16, &spec, idemp, 90_000, act.clone());
    let hwire = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire)
        .expect("shim");
    let auth = h.sign_dispatch_auth(
        &h.caller_binding,
        req_id,
        idemp,
        90_000,
        &act,
        &spec,
        h.default_unsigned_auth(16, &res, h.root_capability.handle),
    );
    let disp = h
        .gate
        .admit_mediated_driver_dispatch(&hwire, &auth)
        .expect("dispatch");

    let exec_receipt_id = ReceiptId::from_bytes(id16(0xC6, 16));
    let comp_cbor = encode_complete_result(&CompleteOperationResultWire {
        replayed: false,
        outcome_code: OUTCOME_CODE_COMPLETED,
        receipt_id: exec_receipt_id.into_bytes(),
        state_code: STATE_CODE_COMPLETED,
    })
    .expect("cbor");
    let (host_resp_wire, _) = h
        .shim
        .translate_legacy_driver_completion_bidirectional(
            req_id,
            &comp_cbor,
            LegacyCompleteBridgeContext {
                handle: OperationHandle {
                    operation_id: spec.operation_id,
                    generation: spec.generation,
                },
                callback_id: auth.callback_id,
                seed: [0x22; 32],
                fenced_reconciliation_only: false,
            },
        )
        .expect("resp");

    // 1. Forged by untrusted caller_binding instead of trusted_meter_principal!
    let wrong_signer_proof = h.sign_usage_proof(
        &h.caller_binding,
        res.reservation_id,
        spec.operation_id,
        disp.activation_receipt.receipt_id,
        exec_receipt_id,
        1,
        10,
        20_000,
    );
    let err1 = h
        .gate
        .complete_mediated_driver_call(&host_resp_wire, &wrong_signer_proof)
        .expect_err("caller-signed usage proof must be rejected");
    assert!(matches!(
        err1,
        BoundaryViolation::UntrustedUsageMeterSigner { .. }
    ));

    // 2. Signed by real TCB meter for usage=50, but shim tampers cumulative_usage down to 1!
    let mut tampered_proof = h.sign_usage_proof(
        &h.meter_binding,
        res.reservation_id,
        spec.operation_id,
        disp.activation_receipt.receipt_id,
        exec_receipt_id,
        1,
        50,
        20_000,
    );
    tampered_proof.cumulative_usage = 1;
    let err2 = h
        .gate
        .complete_mediated_driver_call(&host_resp_wire, &tampered_proof)
        .expect_err("tampered cumulative_usage must fail Ed25519 verification");
    assert!(matches!(
        err2,
        BoundaryViolation::Identity(IdentityAuthorityError::InvalidSignature)
    ));
}

#[test]
fn drv_active_001_usage_exceeding_reservation_upper_bound_fails_closed_with_budget_error() {
    let mut h = FourLayerHarness::bootstrap("over-budget");
    let spec = h.make_operation_spec(17);
    let res = h.reserve_for_operation(17, spec.operation_id, 50);
    let idemp = IdempotencyKey::from_bytes(id16(0xC1, 17));
    let act = b"over-budget".to_vec();
    let (req_id, cwire) =
        h.build_client_legacy_register_envelope(17, &spec, idemp, 90_000, act.clone());
    let hwire = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire)
        .expect("shim");
    let auth = h.sign_dispatch_auth(
        &h.caller_binding,
        req_id,
        idemp,
        90_000,
        &act,
        &spec,
        h.default_unsigned_auth(17, &res, h.root_capability.handle),
    );
    let disp = h
        .gate
        .admit_mediated_driver_dispatch(&hwire, &auth)
        .expect("dispatch");

    let exec_receipt_id = ReceiptId::from_bytes(id16(0xC7, 17));
    let comp_cbor = encode_complete_result(&CompleteOperationResultWire {
        replayed: false,
        outcome_code: OUTCOME_CODE_COMPLETED,
        receipt_id: exec_receipt_id.into_bytes(),
        state_code: STATE_CODE_COMPLETED,
    })
    .expect("cbor");
    let (host_resp_wire, _) = h
        .shim
        .translate_legacy_driver_completion_bidirectional(
            req_id,
            &comp_cbor,
            LegacyCompleteBridgeContext {
                handle: OperationHandle {
                    operation_id: spec.operation_id,
                    generation: spec.generation,
                },
                callback_id: auth.callback_id,
                seed: [0x33; 32],
                fenced_reconciliation_only: false,
            },
        )
        .expect("resp");

    // Meter reports cumulative_usage = 99 > upper_bound = 50 -> UsageExceedsUpperBound!
    let over_budget_proof = h.sign_usage_proof(
        &h.meter_binding,
        res.reservation_id,
        spec.operation_id,
        disp.activation_receipt.receipt_id,
        exec_receipt_id,
        1,
        99,
        20_000,
    );
    let err = h
        .gate
        .complete_mediated_driver_call(&host_resp_wire, &over_budget_proof)
        .expect_err("usage exceeding reservation upper_bound must fail closed");
    assert!(matches!(
        err,
        BoundaryViolation::Resource(ResourceAuthorityError::UsageExceedsUpperBound {
            usage: 99,
            upper_bound: 50
        })
    ));
    assert_eq!(err.to_sabi_failure().code, SabiErrorCode::Budget as i32);
}

#[test]
fn drv_bound_001_expired_deadline_or_missing_activity_context_fails_closed() {
    let mut h = FourLayerHarness::bootstrap("deadline-activity");
    let spec = h.make_operation_spec(18);
    let res = h.reserve_for_operation(18, spec.operation_id, 50);

    // 1. Expired deadline (deadline_monotonic_ns = 40_000 < now_monotonic_ns = 50_000).
    let idemp1 = IdempotencyKey::from_bytes(id16(0xC2, 181));
    let act1 = b"valid-activity".to_vec();
    let (req1, cwire1) =
        h.build_client_legacy_register_envelope(181, &spec, idemp1, 40_000, act1.clone());
    let hwire1 = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire1)
        .expect("shim");
    let auth1 = h.sign_dispatch_auth(
        &h.caller_binding,
        req1,
        idemp1,
        40_000,
        &act1,
        &spec,
        h.default_unsigned_auth(181, &res, h.root_capability.handle),
    );
    let err1 = h
        .gate
        .admit_mediated_driver_dispatch(&hwire1, &auth1)
        .expect_err("expired deadline must fail closed");
    assert!(matches!(err1, BoundaryViolation::DeadlineExpired { .. }));
    assert_eq!(err1.to_sabi_failure().code, SabiErrorCode::Deadline as i32);

    // 2. Empty activity_context -> MissingActivityContext.
    let idemp2 = IdempotencyKey::from_bytes(id16(0xC2, 182));
    let empty_act = Vec::new();
    let (req2, cwire2) =
        h.build_client_legacy_register_envelope(182, &spec, idemp2, 90_000, empty_act.clone());
    let hwire2 = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire2)
        .expect("shim");
    let auth2 = h.sign_dispatch_auth(
        &h.caller_binding,
        req2,
        idemp2,
        90_000,
        &empty_act,
        &spec,
        h.default_unsigned_auth(182, &res, h.root_capability.handle),
    );
    let err2 = h
        .gate
        .admit_mediated_driver_dispatch(&hwire2, &auth2)
        .expect_err("missing activity_context must fail closed");
    assert!(matches!(err2, BoundaryViolation::MissingActivityContext));
}

#[test]
fn drv_bound_001_stale_process_incarnation_generation_fenced_at_tcb_gate() {
    let mut h = FourLayerHarness::bootstrap("stale-proc");
    let spec = h.make_operation_spec(19);
    let res = h.reserve_for_operation(19, spec.operation_id, 50);
    let idemp = IdempotencyKey::from_bytes(id16(0xC3, 19));
    let act = b"stale-proc".to_vec();
    let (req_id, cwire) =
        h.build_client_legacy_register_envelope(19, &spec, idemp, 90_000, act.clone());
    let hwire = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire)
        .expect("shim");

    // Sign with process_generation = 1, then rotate process incarnation to generation = 2!
    let auth_gen_1 = h.sign_dispatch_auth(
        &h.caller_binding,
        req_id,
        idemp,
        90_000,
        &act,
        &spec,
        h.default_unsigned_auth(19, &res, h.root_capability.handle),
    );
    h.gate
        .rotate_process_incarnation(h.process_id, gen_u64(2))
        .expect("rotate process to generation 2");

    let err = h
        .gate
        .admit_mediated_driver_dispatch(&hwire, &auth_gen_1)
        .expect_err("stale process generation 1 must be fenced");
    assert!(matches!(
        err,
        BoundaryViolation::StaleProcessIncarnation { .. }
    ));
    assert_eq!(err.to_sabi_failure().code, SabiErrorCode::Fenced as i32);
    assert_eq!(
        h.resource
            .inspect_permit_binding(res.reservation_id)
            .expect("reservation untouched")
            .state,
        ReservationState::Reserved
    );
}

#[test]
fn drv_bound_001_stale_driver_generation_and_rotated_fencing_token_fenced_by_resource_authority() {
    let mut h = FourLayerHarness::bootstrap("stale-driver");
    let spec = h.make_operation_spec(20);
    let res_old_driver = h.reserve_for_operation(20, spec.operation_id, 50);
    let idemp = IdempotencyKey::from_bytes(id16(0xC4, 20));
    let act = b"stale-driver".to_vec();
    let (req_id, cwire) =
        h.build_client_legacy_register_envelope(20, &spec, idemp, 90_000, act.clone());
    let hwire = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire)
        .expect("shim");
    let auth_old_driver = h.sign_dispatch_auth(
        &h.caller_binding,
        req_id,
        idemp,
        90_000,
        &act,
        &spec,
        h.default_unsigned_auth(20, &res_old_driver, h.root_capability.handle),
    );

    // Rotate driver generation + fencing_token in ResourceAuthority before dispatch!
    let rotated = h
        .resource
        .rotate_driver(RotateDriverRequest {
            driver_id: h.driver.driver_id,
            expected_generation: h.driver.generation,
            expected_fencing_token: h.driver.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes(id16(0xD8, 20)),
            rotated_at_ms: 8_000,
        })
        .expect("rotate driver")
        .record();
    assert_eq!(rotated.generation, gen_u64(2));
    assert_ne!(rotated.fencing_token, h.driver.fencing_token);

    let err = h
        .gate
        .admit_mediated_driver_dispatch(&hwire, &auth_old_driver)
        .expect_err("stale driver generation/fencing_token must fail closed");
    assert!(matches!(
        err,
        BoundaryViolation::Resource(ResourceAuthorityError::StaleDriver)
    ));
    assert_eq!(err.to_sabi_failure().code, SabiErrorCode::Fenced as i32);
}

// ============================================================================
// Category 4: [TASK-PROVIDER-001] Anti-Elevation & [DRV-LIFE-001]/[KABI-UNCERTAIN-001] Crash Quarantine
// ============================================================================

#[test]
fn task_provider_001_malicious_shim_forging_verified_isolation_on_legacy_driver_completion_fails_closed()
 {
    let mut h = FourLayerHarness::bootstrap("forged-isolation");
    let spec = h.make_operation_spec(21);
    let res = h.reserve_for_operation(21, spec.operation_id, 60);
    let idemp = IdempotencyKey::from_bytes(id16(0xC5, 21));
    let act = b"legacy-unverified".to_vec();
    let (req_id, cwire) =
        h.build_client_legacy_register_envelope(21, &spec, idemp, 90_000, act.clone());
    let hwire = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire)
        .expect("shim");
    let auth = h.sign_dispatch_auth(
        &h.caller_binding,
        req_id,
        idemp,
        90_000,
        &act,
        &spec,
        h.default_unsigned_auth(21, &res, h.root_capability.handle),
    );
    let disp = h
        .gate
        .admit_mediated_driver_dispatch(&hwire, &auth)
        .expect("dispatch with allow_verified_isolation=false");

    // Compromised shim bypasses DriverProtocolShim and crafts a canonical host-epoch
    // DriverProviderResponse claiming PROVIDER_ISOLATION_VERIFIED_TAG!
    let exec_receipt_id = ReceiptId::from_bytes(id16(0xC8, 21));
    let forged_v1_payload = encode_complete_ack_proto(
        &CompleteProviderOutcome {
            replayed: false,
            outcome: CompletionOutcome::Completed {
                receipt_id: exec_receipt_id,
            },
            state: OperationState::Completed {
                receipt_id: exec_receipt_id,
            },
        },
        [0x77; 32],
        false,
        &ProviderIsolationAttestation {
            tenant_isolated: true,
            session_ephemeral: true,
            cache_partitioned: true,
            zero_data_retention: true,
            attestation_digest: [0xAB; 32],
            provider_profile: "forged-enclave".to_owned(),
        },
    )
    .expect("encode forged CompleteAck");
    let mut forged_resp =
        DriverProviderResponse::decode(forged_v1_payload.as_slice()).expect("decode");
    forged_resp.schema.as_mut().expect("schema").major = h.host_epoch;

    let forged_host_env = Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: h.host_epoch,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: req_id.to_vec(),
        service: DRIVER_PROVIDER_SERVICE.to_owned(),
        method: COMPLETE_PROVIDER_METHOD.to_owned(),
        common_context: Some(envelope::CommonContext::ResponseContext(
            SabiResponseContext {
                correlation_id: req_id.to_vec(),
                operation: None,
                receipts: Vec::new(),
                failure: None,
            },
        )),
        payload: forged_resp.encode_to_vec(),
    };

    let usage_proof = h.sign_usage_proof(
        &h.meter_binding,
        res.reservation_id,
        spec.operation_id,
        disp.activation_receipt.receipt_id,
        exec_receipt_id,
        1,
        20,
        20_000,
    );

    let err = h
        .gate
        .complete_mediated_driver_call(&forged_host_env.encode_to_vec(), &usage_proof)
        .expect_err("TCB gate must reject forged VERIFIED isolation from unverified shim");
    assert!(matches!(
        err,
        BoundaryViolation::ForbiddenIsolationElevation(_)
    ));
    assert_eq!(err.to_sabi_failure().code, SabiErrorCode::Rights as i32);
}

#[test]
fn drv_life_001_and_kabi_uncertain_001_mid_flight_shim_crash_transitions_to_effect_unknown_and_quarantined_and_blocks_blind_retry()
 {
    let mut h = FourLayerHarness::bootstrap("crash-quarantine");
    let spec = h.make_operation_spec(22);
    let res = h.reserve_for_operation(22, spec.operation_id, 90);
    let orig_idemp = IdempotencyKey::from_bytes(id16(0xC6, 22));
    let act = b"crash-test".to_vec();
    let (req_id, cwire) =
        h.build_client_legacy_register_envelope(22, &spec, orig_idemp, 90_000, act.clone());
    let hwire = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire)
        .expect("shim");
    let auth = h.sign_dispatch_auth(
        &h.caller_binding,
        req_id,
        orig_idemp,
        90_000,
        &act,
        &spec,
        h.default_unsigned_auth(22, &res, h.root_capability.handle),
    );
    let disp = h
        .gate
        .admit_mediated_driver_dispatch(&hwire, &auth)
        .expect("dispatch succeeds");

    // Out-of-core shim crashes mid-flight!
    let crash_receipt_id = ReceiptId::from_bytes(id16(0xE1, 22));
    let crash = h
        .gate
        .fence_shim_or_driver_crash(spec.operation_id, crash_receipt_id, [0x9F; 32], 15_000)
        .expect("crash transitions operation to EffectUnknown and reservation to Quarantined");

    assert_eq!(
        crash.operation_snapshot.state,
        OperationState::EffectUnknown {
            receipt_id: crash_receipt_id
        }
    );
    assert_eq!(crash.original_idempotency_key, orig_idemp);
    let durable_quarantine = h
        .resource
        .inspect_quarantine_receipt(res.reservation_id)
        .expect("durable QuarantineReceipt in SQLite ResourceAuthority");
    assert_eq!(
        durable_quarantine.receipt_id,
        crash.quarantine_receipt.receipt_id
    );

    // 1. Blind dispatch retry with a NEW IdempotencyKey for the crashed operation is forbidden!
    let blind_new_idemp = IdempotencyKey::from_bytes(id16(0xC6, 2299));
    let (req_blind, cwire_blind) =
        h.build_client_legacy_register_envelope(2299, &spec, blind_new_idemp, 90_000, act.clone());
    let hwire_blind = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire_blind)
        .expect("shim");
    let auth_blind = h.sign_dispatch_auth(
        &h.caller_binding,
        req_blind,
        blind_new_idemp,
        90_000,
        &act,
        &spec,
        h.default_unsigned_auth(2299, &res, h.root_capability.handle),
    );
    let blind_disp_err = h
        .gate
        .admit_mediated_driver_dispatch(&hwire_blind, &auth_blind)
        .expect_err("blind dispatch retry with new IdempotencyKey must fail closed");
    assert!(matches!(
        blind_disp_err,
        BoundaryViolation::BlindRetryWithNewIdempotencyKeyForbidden { .. }
    ));
    let failure = blind_disp_err.to_sabi_failure();
    assert_eq!(failure.code, SabiErrorCode::EffectUnknown as i32);
    assert_eq!(
        failure.retry,
        RetryDirective::QueryOperationOrRetrySameIdempotencyKey as i32
    );

    // 2. Reconciliation attempt presenting a NEW IdempotencyKey is ALSO forbidden!
    let recon_proof = h.sign_usage_proof(
        &h.meter_binding,
        res.reservation_id,
        spec.operation_id,
        disp.activation_receipt.receipt_id,
        crash_receipt_id,
        0,
        0,
        25_000,
    );
    let blind_recon_err = h
        .gate
        .reconcile_uncertain_operation(spec.operation_id, blind_new_idemp, &recon_proof)
        .expect_err("reconciliation with new IdempotencyKey must fail closed");
    assert!(matches!(
        blind_recon_err,
        BoundaryViolation::BlindRetryWithNewIdempotencyKeyForbidden { .. }
    ));
}

#[test]
fn kabi_uncertain_001_reconciliation_with_exact_original_128bit_idempotency_key_settles_quarantined_reservation_idempotently()
 {
    let mut h = FourLayerHarness::bootstrap("reconcile-orig-key");
    let spec = h.make_operation_spec(23);
    let res = h.reserve_for_operation(23, spec.operation_id, 100);
    let orig_idemp = IdempotencyKey::from_bytes(id16(0xC7, 23));
    let act = b"reconcile-test".to_vec();
    let (req_id, cwire) =
        h.build_client_legacy_register_envelope(23, &spec, orig_idemp, 90_000, act.clone());
    let hwire = h
        .shim
        .translate_legacy_client_dispatch_to_host(&cwire)
        .expect("shim");
    let auth = h.sign_dispatch_auth(
        &h.caller_binding,
        req_id,
        orig_idemp,
        90_000,
        &act,
        &spec,
        h.default_unsigned_auth(23, &res, h.root_capability.handle),
    );
    let disp = h
        .gate
        .admit_mediated_driver_dispatch(&hwire, &auth)
        .expect("dispatch");

    let crash_receipt_id = ReceiptId::from_bytes(id16(0xE2, 23));
    let _ = h
        .gate
        .fence_shim_or_driver_crash(spec.operation_id, crash_receipt_id, [0xAA; 32], 16_000)
        .expect("quarantined");

    // Reconcile using the EXACT original 128-bit IdempotencyKey + TCB-meter proof (final_usage = 15).
    let recon_proof = h.sign_usage_proof(
        &h.meter_binding,
        res.reservation_id,
        spec.operation_id,
        disp.activation_receipt.receipt_id,
        crash_receipt_id,
        1,
        15,
        26_000,
    );
    let reconciled = h
        .gate
        .reconcile_uncertain_operation(spec.operation_id, orig_idemp, &recon_proof)
        .expect("reconciliation under exact original IdempotencyKey succeeds");
    assert!(!reconciled.replayed);
    assert_eq!(reconciled.idempotency_key, orig_idemp);
    assert_eq!(reconciled.finalization_receipt.final_usage, 15);
    assert_eq!(reconciled.finalization_receipt.refund_credit, 85);

    // Repeating reconciliation with the exact same original IdempotencyKey replays idempotently.
    let replayed = h
        .gate
        .reconcile_uncertain_operation(spec.operation_id, orig_idemp, &recon_proof)
        .expect("idempotent reconciliation replay");
    assert!(replayed.replayed);
    assert_eq!(
        replayed.finalization_receipt.receipt_id,
        reconciled.finalization_receipt.receipt_id
    );
}

// ============================================================================
// Category 5: Static Zero-Historical-Branch-Pollution Core Crate Audit
// ============================================================================

#[test]
fn zero_core_crate_dependency_pollution_static_cargo_toml_audit() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let crates_root = manifest_dir
        .parent()
        .expect("crates/ directory exists above nlos-compat-shim");

    let audited_core_crates = [
        "nlos-cell",
        "nlos-task",
        "nlos-capability",
        "nlos-resource",
        "nlos-consensus",
        "nlos-system-control",
        "nlos-identity",
        "nlos-operation",
    ];

    for crate_name in audited_core_crates {
        let cargo_toml_path = crates_root.join(crate_name).join("Cargo.toml");
        let content = fs::read_to_string(&cargo_toml_path)
            .unwrap_or_else(|err| panic!("failed reading {}: {err}", cargo_toml_path.display()));
        assert!(
            !content.contains("nlos-compat-shim"),
            "core TCB authority crate {crate_name} MUST NOT depend on out-of-core nlos-compat-shim"
        );
    }
}
