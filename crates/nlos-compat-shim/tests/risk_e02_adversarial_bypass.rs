//! `RISK-E-02` adversarial security zero-bypass test suite covering all six
//! mandatory compromise vectors (`a`..`f`) plus legacy Stage B `minicbor` <->
//! `nlos.sabi.DriverProvider` bidirectional translation (`[DRV-SHIM-001]`).

use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_capability::{
    CapabilityAuthority, CapabilityRights, CapabilityTarget, IssueRootCapabilityRequest,
    SignedIssueRootCapabilityRequest, issue_root_command_message,
};
use nlos_compat_shim::driver_shim::{
    CancelOperationResultWire, CancelOperationWire, CompleteOperationResultWire,
    CompleteOperationWire, CustodiedCredentialRef, DispatchOperationResultWire,
    DispatchOperationWire, DriverAdmissionRequest, DriverCompletionSubmission, DriverMethod,
    DriverProtocolCompatShim, EnforcementGateError, IsolationEnforcementPolicy,
    IsolationReportWire, LegacyDriverWireFormat, OUTCOME_CODE_COMPLETED, QueryIsolationWire,
    ReconcileUncertainRequest, RegisterOperationResultWire, RegisterOperationWire,
    ReservationActivationToken, STATE_CODE_CANCELLED_BEFORE_EFFECT, STATE_CODE_COMPLETED,
    TrustedDriverEnforcementGate, compute_driver_authorization_digest, decode_cancel_request,
    decode_cancel_result, decode_complete_request, decode_complete_result, decode_dispatch_request,
    decode_dispatch_result, decode_isolation_report, decode_query_isolation_request,
    decode_register_request, decode_register_result, encode_cancel_request, encode_cancel_result,
    encode_complete_request, encode_complete_result, encode_dispatch_request,
    encode_dispatch_result, encode_isolation_report, encode_query_isolation_request,
    encode_register_request, encode_register_result,
};
use nlos_driver_mock::{
    PROVIDER_ISOLATION_UNVERIFIED_TAG, PROVIDER_ISOLATION_VERIFIED_TAG, ProviderIsolationProfile,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose};
use nlos_operation::{CompletionOutcome, OperationHandle, OperationState};
use nlos_resource::{
    CreateAccountRequest, CreateQuoteRequest, DriverRecord, RegisterDriverRequest,
    ReservationRecord, ReservationState, ReserveRequest, ResourceAuthority, ResourceDemand,
    RotateDriverRequest,
};
use nlos_schema::encode_driver_provider_response;
use nlos_schema::sabi::v1::{
    DriverProviderCompleteAck, DriverProviderResponse, DriverProviderTerminalOutcome,
    OperationLifecycleState, ProviderIsolationAttestationWire, driver_provider_response,
};
use nlos_types::{CallId, Generation, IdempotencyKey, NamespaceId, OperationId, ReceiptId};

static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

struct TempAuthorityRoot(PathBuf);

impl TempAuthorityRoot {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let seq = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "nlos-compat-shim-{label}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempAuthorityRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct TestHarness {
    _root: TempAuthorityRoot,
    identity: IdentityAuthority,
    capability: CapabilityAuthority,
    resource: ResourceAuthority,
    caller_key: SigningKey,
    caller_binding: IdentityBinding,
    cap_handle: nlos_capability::CapabilityHandle,
    target: CapabilityTarget,
    driver: DriverRecord,
    account_id: nlos_types::ResourceAccountId,
}

impl TestHarness {
    fn setup(label: &str) -> Self {
        let root = TempAuthorityRoot::new(label);
        let identity = IdentityAuthority::open(root.0.join("id")).expect("open identity");
        let capability = CapabilityAuthority::open(root.0.join("cap")).expect("open capability");
        let resource = ResourceAuthority::open(root.0.join("res")).expect("open resource");

        let caller_key = SigningKey::from_bytes(&[0x31; 32]);
        let caller_binding = identity
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: [0x11; 32],
                control_domain_policy_digest: [0x12; 32],
                public_key: caller_key.verifying_key().to_bytes(),
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: 0,
                key_valid_until_ms: 100_000,
                idempotency_key: IdempotencyKey::from_bytes([0x13; 16]),
                created_at_ms: 100,
            })
            .expect("bootstrap caller")
            .binding();

        let target = CapabilityTarget::Namespace(NamespaceId::from_bytes([0x44; 16]));
        let issue_cmd = IssueRootCapabilityRequest {
            issuer_key_id: caller_binding.key_id,
            holder_key_id: caller_binding.key_id,
            target,
            rights: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: None,
            valid_from_ms: 0,
            valid_until_ms: 100_000,
            delegation_depth_remaining: 2,
            call_limit: Some(8),
            idempotency_key: IdempotencyKey::from_bytes([0x14; 16]),
            issued_at_ms: 200,
        };
        let issue_sig = caller_key
            .sign(&issue_root_command_message(issue_cmd))
            .to_bytes();
        let cap_record = capability
            .issue_root_signed(
                &identity,
                SignedIssueRootCapabilityRequest {
                    command: issue_cmd,
                    signer: caller_binding.principal_id,
                    signature: issue_sig,
                },
            )
            .expect("issue root capability")
            .record();

        let driver = resource
            .register_driver(RegisterDriverRequest {
                profile_digest: [0x51; 32],
                idempotency_key: IdempotencyKey::from_bytes([0x52; 16]),
                created_at_ms: 200,
            })
            .expect("register driver")
            .record();
        let account = resource
            .create_account(CreateAccountRequest {
                initial_credit: 10_000,
                idempotency_key: IdempotencyKey::from_bytes([0x53; 16]),
                created_at_ms: 200,
            })
            .expect("create account");

        Self {
            _root: root,
            identity,
            capability,
            resource,
            caller_key,
            caller_binding,
            cap_handle: cap_record.handle,
            target,
            driver,
            account_id: account.account_id,
        }
    }

    fn reserve_for_operation(&self, seed: u8, operation_id: OperationId) -> ReservationRecord {
        let quote = self
            .resource
            .create_quote(CreateQuoteRequest {
                driver_id: self.driver.driver_id,
                driver_generation: self.driver.generation,
                driver_fencing_token: self.driver.fencing_token,
                operation_proposal_digest: [seed; 32],
                pricing_version: [0x60; 32],
                upper_bound: 100,
                demand_capacity: ResourceDemand::default(),
                valid_until_ms: 100_000,
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(1); 16]),
                created_at_ms: 300,
            })
            .expect("create quote")
            .record();
        self.resource
            .reserve(ReserveRequest {
                account_id: self.account_id,
                quote_id: quote.quote_id,
                call_id: CallId::from_bytes([seed.wrapping_add(2); 16]),
                operation_id,
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
                demand: ResourceDemand::default(),
                reserved_at_ms: 400,
            })
            .expect("reserve")
            .record()
    }

    fn make_signed_admission(
        &self,
        op_byte: u8,
        reservation: &ReservationRecord,
        idempotency_key: IdempotencyKey,
        credential_ref: Option<CustodiedCredentialRef>,
    ) -> DriverAdmissionRequest {
        let (handle, reg_proto, disp_proto) = make_register_and_dispatch_proto(op_byte);
        let deadline_monotonic_ns = 50_000;
        let auth_digest = compute_driver_authorization_digest(
            handle,
            idempotency_key,
            deadline_monotonic_ns,
            Generation::INITIAL,
            &self.driver.fencing_token,
            &disp_proto,
        );
        let signature = self.caller_key.sign(&auth_digest).to_bytes();

        DriverAdmissionRequest {
            caller_principal: self.caller_binding.principal_id,
            caller_control_domain: self.caller_binding.control_domain_id,
            caller_key_id: self.caller_binding.key_id,
            process_incarnation: Generation::INITIAL,
            capability_handle: self.cap_handle,
            target: self.target,
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: None,
            idempotency_key,
            deadline_monotonic_ns,
            now_monotonic_ns: 10_000,
            admitted_at_ms: 1_000,
            reservation_id: reservation.reservation_id,
            call_id: reservation.call_id,
            driver_id: self.driver.driver_id,
            driver_generation: self.driver.generation,
            driver_fencing_token: self.driver.fencing_token,
            activation_token: ReservationActivationToken::from_bytes(reservation.activation_token),
            credential_ref,
            canonical_register_wire: reg_proto,
            canonical_dispatch_wire: disp_proto,
            authorization_digest: auth_digest,
            caller_signature: signature,
        }
    }
}

fn make_register_and_dispatch_proto(op_byte: u8) -> (OperationHandle, Vec<u8>, Vec<u8>) {
    let shim = DriverProtocolCompatShim::stage_b_minicbor(1).expect("shim");
    let reg_cbor = encode_register_request(&RegisterOperationWire {
        operation_id: [op_byte; 16],
        operation_generation: 1,
        owner_fiber_id: [0x71; 16],
        owner_fiber_generation: 1,
        cancellation_scope_id: [0x72; 16],
        cancellation_generation: 1,
    })
    .expect("reg cbor");
    let disp_cbor = encode_dispatch_request(&DispatchOperationWire {
        operation_id: [op_byte; 16],
        operation_generation: 1,
        callback_id: [op_byte.wrapping_add(0x10); 16],
    })
    .expect("disp cbor");

    let reg_proto = shim
        .cbor_request_to_proto_wire(DriverMethod::Register, &reg_cbor)
        .expect("reg proto");
    let disp_proto = shim
        .cbor_request_to_proto_wire(DriverMethod::Dispatch, &disp_cbor)
        .expect("disp proto");
    let handle = OperationHandle {
        operation_id: OperationId::from_bytes([op_byte; 16]),
        generation: Generation::INITIAL,
    };
    (handle, reg_proto, disp_proto)
}

fn encode_complete_response_proto(
    receipt_byte: u8,
    isolation: ProviderIsolationAttestationWire,
) -> Vec<u8> {
    let response = DriverProviderResponse {
        schema: Some(nlos_schema::driver_provider_schema_identity()),
        response: Some(driver_provider_response::Response::CompleteAck(
            DriverProviderCompleteAck {
                replayed: false,
                outcome: DriverProviderTerminalOutcome::Completed.into(),
                receipt_id: vec![receipt_byte; 16],
                state: OperationLifecycleState::Completed.into(),
                compute_digest: vec![0xab; 32],
                isolation: Some(isolation),
                fenced_reconciliation_only: false,
            },
        )),
    };
    encode_driver_provider_response(&response).expect("encode complete response")
}

#[test]
fn driver_protocol_compat_shim_roundtrips_register_and_dispatch_cbor() {
    let shim = DriverProtocolCompatShim::new(1, 3, 2, LegacyDriverWireFormat::StageBMinicbor)
        .expect("N-2 driver shim");
    assert!(
        std::mem::size_of::<DriverProtocolCompatShim>() <= 16,
        "DriverProtocolCompatShim must remain a tiny Copy value with zero secret/authority handles"
    );

    let reg = RegisterOperationWire {
        operation_id: [1; 16],
        operation_generation: 2,
        owner_fiber_id: [3; 16],
        owner_fiber_generation: 4,
        cancellation_scope_id: [5; 16],
        cancellation_generation: 6,
    };
    let reg_proto = shim
        .cbor_request_to_proto_wire(
            DriverMethod::Register,
            &encode_register_request(&reg).expect("enc"),
        )
        .expect("reg -> proto");
    let (m, back_cbor) = shim
        .proto_wire_to_cbor_request(&reg_proto)
        .expect("proto -> reg");
    assert_eq!(m, DriverMethod::Register);
    assert_eq!(decode_register_request(&back_cbor).expect("dec"), reg);

    let reg_res = RegisterOperationResultWire {
        replayed: false,
        operation_id: [1; 16],
        operation_generation: 2,
        admission_receipt_id: [7; 16],
    };
    let reg_res_proto = shim
        .cbor_response_to_proto_wire(
            DriverMethod::Register,
            &encode_register_result(&reg_res).expect("enc"),
        )
        .expect("res -> proto");
    let (_, back_res) = shim
        .proto_wire_to_cbor_response(&reg_res_proto)
        .expect("proto -> res");
    assert_eq!(decode_register_result(&back_res).expect("dec"), reg_res);

    let disp = DispatchOperationWire {
        operation_id: [1; 16],
        operation_generation: 2,
        callback_id: [8; 16],
    };
    let disp_proto = shim
        .cbor_request_to_proto_wire(
            DriverMethod::Dispatch,
            &encode_dispatch_request(&disp).expect("enc"),
        )
        .expect("disp -> proto");
    let (_, back_disp) = shim.proto_wire_to_cbor_request(&disp_proto).expect("back");
    assert_eq!(decode_dispatch_request(&back_disp).expect("dec"), disp);

    let disp_res = DispatchOperationResultWire {
        replayed: true,
        preparation_receipt_id: [9; 16],
        activation_receipt_id: [10; 16],
        callback_id: [8; 16],
        operation_id: [1; 16],
        operation_generation: 2,
        owner_fiber_id: [3; 16],
        owner_fiber_generation: 4,
        cancel_epoch: 0,
    };
    let disp_res_proto = shim
        .cbor_response_to_proto_wire(
            DriverMethod::Dispatch,
            &encode_dispatch_result(&disp_res).expect("enc"),
        )
        .expect("proto");
    let (_, back_disp_res) = shim
        .proto_wire_to_cbor_response(&disp_res_proto)
        .expect("back");
    assert_eq!(
        decode_dispatch_result(&back_disp_res).expect("dec"),
        disp_res
    );
}

#[test]
fn driver_protocol_compat_shim_roundtrips_complete_cbor() {
    let shim = DriverProtocolCompatShim::new(1, 3, 2, LegacyDriverWireFormat::StageBMinicbor)
        .expect("N-2 driver shim");

    let comp = CompleteOperationWire {
        operation_id: [1; 16],
        operation_generation: 2,
        callback_id: [8; 16],
        seed: [0xcd; 32],
    };
    let comp_proto = shim
        .cbor_request_to_proto_wire(
            DriverMethod::Complete,
            &encode_complete_request(&comp).expect("enc"),
        )
        .expect("proto");
    let (_, back_comp) = shim.proto_wire_to_cbor_request(&comp_proto).expect("back");
    assert_eq!(decode_complete_request(&back_comp).expect("dec"), comp);

    let comp_res = CompleteOperationResultWire {
        replayed: false,
        outcome_code: OUTCOME_CODE_COMPLETED,
        receipt_id: [11; 16],
        state_code: STATE_CODE_COMPLETED,
    };
    let comp_res_proto = shim
        .cbor_response_to_proto_wire(
            DriverMethod::Complete,
            &encode_complete_result(&comp_res).expect("enc"),
        )
        .expect("proto");
    let (_, back_comp_res) = shim
        .proto_wire_to_cbor_response(&comp_res_proto)
        .expect("back");
    assert_eq!(
        decode_complete_result(&back_comp_res).expect("dec"),
        comp_res
    );
}

#[test]
fn driver_protocol_compat_shim_roundtrips_cancel_query_isolation_and_cross_epoch_proto() {
    let shim = DriverProtocolCompatShim::new(1, 3, 2, LegacyDriverWireFormat::StageBMinicbor)
        .expect("N-2 driver shim");

    let cancel = CancelOperationWire {
        operation_id: [1; 16],
        operation_generation: 2,
        expected_cancel_epoch: 0,
        no_effect_receipt_id: [12; 16],
    };
    let cancel_proto = shim
        .cbor_request_to_proto_wire(
            DriverMethod::Cancel,
            &encode_cancel_request(&cancel).expect("enc"),
        )
        .expect("proto");
    let (_, back_cancel) = shim
        .proto_wire_to_cbor_request(&cancel_proto)
        .expect("back");
    assert_eq!(decode_cancel_request(&back_cancel).expect("dec"), cancel);

    let cancel_res = CancelOperationResultWire {
        replayed: false,
        already_terminal: true,
        operation_id: [1; 16],
        operation_generation: 2,
        state_code: STATE_CODE_CANCELLED_BEFORE_EFFECT,
        cancel_epoch: 1,
        terminal_receipt_id: [12; 16],
    };
    let cancel_res_proto = shim
        .cbor_response_to_proto_wire(
            DriverMethod::Cancel,
            &encode_cancel_result(&cancel_res).expect("enc"),
        )
        .expect("proto");
    let (_, back_cancel_res) = shim
        .proto_wire_to_cbor_response(&cancel_res_proto)
        .expect("back");
    assert_eq!(
        decode_cancel_result(&back_cancel_res).expect("dec"),
        cancel_res
    );

    let qi_proto = shim
        .cbor_request_to_proto_wire(
            DriverMethod::QueryIsolation,
            &encode_query_isolation_request(&QueryIsolationWire).expect("enc"),
        )
        .expect("proto");
    let (qi_method, back_qi) = shim.proto_wire_to_cbor_request(&qi_proto).expect("back");
    assert_eq!(qi_method, DriverMethod::QueryIsolation);
    assert_eq!(
        decode_query_isolation_request(&back_qi).expect("dec"),
        QueryIsolationWire
    );

    let iso_rep = IsolationReportWire {
        tenant_isolated: true,
        session_ephemeral: true,
        cache_partitioned: false,
        zero_data_retention: true,
        attestation_digest: [0x55; 32],
        provider_profile: "legacy-worker".to_owned(),
    };
    let iso_proto = shim
        .cbor_response_to_proto_wire(
            DriverMethod::QueryIsolation,
            &encode_isolation_report(&iso_rep).expect("enc"),
        )
        .expect("proto");
    let (_, back_iso) = shim.proto_wire_to_cbor_response(&iso_proto).expect("back");
    assert_eq!(decode_isolation_report(&back_iso).expect("dec"), iso_rep);

    // Cross-epoch N-2 (v1) <-> N (v3) DriverProvider Protobuf round-trip
    let (_, v1_reg_proto, _) = make_register_and_dispatch_proto(0x2a);
    let v3_reg_proto = shim
        .translate_driver_request_upstream(&v1_reg_proto)
        .expect("v1 -> v3");
    assert_ne!(v3_reg_proto, v1_reg_proto);
    let v1_resp_proto = encode_complete_response_proto(
        0x2b,
        ProviderIsolationProfile::Verified
            .to_attestation(None)
            .to_wire(),
    );
    let v3_resp_proto = shim
        .cbor_response_to_proto_wire(
            DriverMethod::QueryIsolation,
            &encode_isolation_report(&iso_rep).expect("enc"),
        )
        .expect("v3 resp");
    let back_to_v1 = shim
        .translate_driver_response_downstream(&v3_resp_proto)
        .expect("v3 -> v1");
    assert_ne!(back_to_v1, v1_resp_proto);
}

#[test]
fn vector_a_compromised_shim_cannot_read_forge_or_reflect_custodied_credentials() {
    let harness = TestHarness::setup("vec-a");
    let mut gate =
        TrustedDriverEnforcementGate::new(IsolationEnforcementPolicy::TagReceiptFailClosed);
    let raw_secret = b"sk-live-nlos-tcb-vault-secret-material-998877";
    let cred_ref = gate
        .provision_credential([0xa1; 16], raw_secret)
        .expect("provision secret");

    // 1. Debug and Display representations never expose raw secret bytes
    let debug_text = format!("{cred_ref:?}");
    let display_text = format!("{cred_ref}");
    let secret_str = std::str::from_utf8(raw_secret).expect("utf8");
    assert!(!debug_text.contains(secret_str));
    assert!(!display_text.contains(secret_str));
    assert!(debug_text.contains("<REDACTED_TCB_ONLY>"));

    // 2. Sanitized dispatch ticket contains zero raw secret bytes
    let op_id = OperationId::from_bytes([0x81; 16]);
    let res = harness.reserve_for_operation(0x21, op_id);
    let req = harness.make_signed_admission(
        0x81,
        &res,
        IdempotencyKey::from_bytes([0x91; 16]),
        Some(cred_ref),
    );
    let ticket = gate
        .admit_dispatch(
            &harness.identity,
            &harness.capability,
            &harness.resource,
            req,
        )
        .expect("admit with valid redacted ref");
    assert_eq!(ticket.credential_ref, Some(cred_ref));
    gate.assert_zero_secret_reflection(&ticket.canonical_dispatch_wire)
        .expect("sanitized wire holds zero secret");

    // 3. Forged fingerprint reference is rejected fail-closed
    let forged_ref = CustodiedCredentialRef::from_raw_parts([0xa1; 16], [0xff; 32]);
    let op_id2 = OperationId::from_bytes([0x82; 16]);
    let res2 = harness.reserve_for_operation(0x25, op_id2);
    let bad_req = harness.make_signed_admission(
        0x82,
        &res2,
        IdempotencyKey::from_bytes([0x92; 16]),
        Some(forged_ref),
    );
    let err = gate
        .admit_dispatch(
            &harness.identity,
            &harness.capability,
            &harness.resource,
            bad_req,
        )
        .expect_err("forged credential fingerprint must fail closed");
    assert!(matches!(
        err,
        EnforcementGateError::InvalidCredentialReference
    ));

    // 4. Compromised response attempting to echo/reflect raw secret is blocked
    let mut leaked_wire = encode_complete_response_proto(
        0x77,
        ProviderIsolationProfile::Verified
            .to_attestation(None)
            .to_wire(),
    );
    leaked_wire.extend_from_slice(raw_secret);
    let reflect_err = gate
        .admit_completion(&DriverCompletionSubmission {
            driver_id: harness.driver.driver_id,
            driver_generation: harness.driver.generation,
            driver_fencing_token: harness.driver.fencing_token,
            process_incarnation: Generation::INITIAL,
            now_monotonic_ns: 20_000,
            canonical_complete_response_wire: leaked_wire,
            untrusted_shim_claimed_verified: true,
        })
        .expect_err("secret reflection must be blocked");
    assert!(matches!(
        reflect_err,
        EnforcementGateError::CredentialMaterialLeakBlocked
    ));
}

#[test]
fn vector_b_compromised_shim_cannot_forge_or_tamper_capability_or_authorization_digest() {
    let harness = TestHarness::setup("vec-b");
    let mut gate =
        TrustedDriverEnforcementGate::new(IsolationEnforcementPolicy::TagReceiptFailClosed);
    let op_id = OperationId::from_bytes([0x83; 16]);
    let res = harness.reserve_for_operation(0x31, op_id);

    // 1. Compromised shim mutates canonical_dispatch_wire callback_id after caller signed digest
    let mut tampered =
        harness.make_signed_admission(0x83, &res, IdempotencyKey::from_bytes([0x93; 16]), None);
    let shim = DriverProtocolCompatShim::stage_b_minicbor(1).expect("shim");
    let mutated_disp_cbor = encode_dispatch_request(&DispatchOperationWire {
        operation_id: [0x83; 16],
        operation_generation: 1,
        callback_id: [0xfe; 16], // tampered by shim!
    })
    .expect("cbor");
    tampered.canonical_dispatch_wire = shim
        .cbor_request_to_proto_wire(DriverMethod::Dispatch, &mutated_disp_cbor)
        .expect("proto");
    let err = gate
        .admit_dispatch(
            &harness.identity,
            &harness.capability,
            &harness.resource,
            tampered,
        )
        .expect_err("tampered dispatch payload must invalidate authorization_digest");
    assert!(matches!(
        err,
        EnforcementGateError::AuthorizationDigestMismatch
    ));

    // 2. Compromised shim recomputes authorization_digest without caller private key (forged sig)
    let mut forged_sig =
        harness.make_signed_admission(0x83, &res, IdempotencyKey::from_bytes([0x93; 16]), None);
    forged_sig.caller_signature = [0xaa; 64];
    let err2 = gate
        .admit_dispatch(
            &harness.identity,
            &harness.capability,
            &harness.resource,
            forged_sig,
        )
        .expect_err("forged Ed25519 signature must fail closed");
    assert!(matches!(
        err2,
        EnforcementGateError::IdentityVerificationFailed(_)
    ));

    // 3. Compromised shim escalates target scope beyond capability grant
    let mut wrong_target =
        harness.make_signed_admission(0x83, &res, IdempotencyKey::from_bytes([0x93; 16]), None);
    wrong_target.target = CapabilityTarget::Namespace(NamespaceId::from_bytes([0xff; 16]));
    let err3 = gate
        .admit_dispatch(
            &harness.identity,
            &harness.capability,
            &harness.resource,
            wrong_target,
        )
        .expect_err("target escalation must fail closed");
    assert!(matches!(
        err3,
        EnforcementGateError::CapabilityTargetMismatch
    ));
}

#[test]
fn vector_c_compromised_shim_cannot_replay_activation_token_or_misappropriate_across_operations() {
    let harness = TestHarness::setup("vec-c");
    let mut gate =
        TrustedDriverEnforcementGate::new(IsolationEnforcementPolicy::TagReceiptFailClosed);

    // First legitimate dispatch consumes Operation 0x84's activation token
    let op_alpha = OperationId::from_bytes([0x84; 16]);
    let reservation_alpha = harness.reserve_for_operation(0x41, op_alpha);
    let admission_alpha = harness.make_signed_admission(
        0x84,
        &reservation_alpha,
        IdempotencyKey::from_bytes([0x94; 16]),
        None,
    );
    gate.admit_dispatch(
        &harness.identity,
        &harness.capability,
        &harness.resource,
        admission_alpha.clone(),
    )
    .expect("first activation succeeds");

    // 1. Replay of the consumed one-shot ReservationActivationToken is blocked
    let replay_err = gate
        .admit_dispatch(
            &harness.identity,
            &harness.capability,
            &harness.resource,
            admission_alpha,
        )
        .expect_err("replaying consumed one-shot activation token must fail closed");
    assert!(matches!(
        replay_err,
        EnforcementGateError::ActivationTokenReplayed { .. }
    ));

    // 2. Cross-operation misappropriation: token reserved for op_beta presented for op_c
    let op_beta = OperationId::from_bytes([0x85; 16]);
    let reservation_beta = harness.reserve_for_operation(0x45, op_beta);
    let cross_op_req = harness.make_signed_admission(
        0x86, // op_c != op_beta!
        &reservation_beta,
        IdempotencyKey::from_bytes([0x95; 16]),
        None,
    );
    let cross_err = gate
        .admit_dispatch(
            &harness.identity,
            &harness.capability,
            &harness.resource,
            cross_op_req,
        )
        .expect_err("cross-operation activation token misappropriation must fail closed");
    assert!(matches!(
        cross_err,
        EnforcementGateError::ActivationTokenOperationMismatch { .. }
    ));
}

#[test]
fn vector_d_compromised_shim_cannot_spoof_verified_isolation_with_zero_digest_or_shared_cache() {
    // Subcase 1: TagReceiptFailClosed overwrites spoofed claim when digest is [0u8; 32]
    let harness1 = TestHarness::setup("vec-d1");
    let mut tag_gate =
        TrustedDriverEnforcementGate::new(IsolationEnforcementPolicy::TagReceiptFailClosed);
    let op1 = OperationId::from_bytes([0x87; 16]);
    let res1 = harness1.reserve_for_operation(0x51, op1);
    tag_gate
        .admit_dispatch(
            &harness1.identity,
            &harness1.capability,
            &harness1.resource,
            harness1.make_signed_admission(
                0x87,
                &res1,
                IdempotencyKey::from_bytes([0x97; 16]),
                None,
            ),
        )
        .expect("admit 1");

    let zero_digest_iso = ProviderIsolationAttestationWire {
        tenant_isolated: true,
        session_ephemeral: true,
        cache_partitioned: true,
        zero_data_retention: true,
        attestation_digest: vec![0_u8; 32],
        provider_profile: "spoofed-verified".to_owned(),
    };
    let tagged = tag_gate
        .admit_completion(&DriverCompletionSubmission {
            driver_id: harness1.driver.driver_id,
            driver_generation: harness1.driver.generation,
            driver_fencing_token: harness1.driver.fencing_token,
            process_incarnation: Generation::INITIAL,
            now_monotonic_ns: 20_000,
            canonical_complete_response_wire: encode_complete_response_proto(0x61, zero_digest_iso),
            untrusted_shim_claimed_verified: true,
        })
        .expect("tagged completion");
    assert!(!tagged.isolation_posture.is_verified());
    assert_eq!(tagged.isolation_marker, PROVIDER_ISOLATION_UNVERIFIED_TAG);
    assert_ne!(tagged.isolation_marker, PROVIDER_ISOLATION_VERIFIED_TAG);

    // Subcase 2: RequireVerifiedIsolation hard-rejects shared cache (`cache_partitioned = false`)
    let harness2 = TestHarness::setup("vec-d2");
    let mut strict_gate =
        TrustedDriverEnforcementGate::new(IsolationEnforcementPolicy::RequireVerifiedIsolation);
    let op2 = OperationId::from_bytes([0x88; 16]);
    let res2 = harness2.reserve_for_operation(0x55, op2);
    strict_gate
        .admit_dispatch(
            &harness2.identity,
            &harness2.capability,
            &harness2.resource,
            harness2.make_signed_admission(
                0x88,
                &res2,
                IdempotencyKey::from_bytes([0x98; 16]),
                None,
            ),
        )
        .expect("admit 2");

    let shared_cache_iso = ProviderIsolationProfile::SharedCacheUnverified
        .to_attestation(Some([0xee; 32]))
        .to_wire();
    let strict_err = strict_gate
        .admit_completion(&DriverCompletionSubmission {
            driver_id: harness2.driver.driver_id,
            driver_generation: harness2.driver.generation,
            driver_fencing_token: harness2.driver.fencing_token,
            process_incarnation: Generation::INITIAL,
            now_monotonic_ns: 20_000,
            canonical_complete_response_wire: encode_complete_response_proto(
                0x62,
                shared_cache_iso,
            ),
            untrusted_shim_claimed_verified: true,
        })
        .expect_err("shared cache must be rejected under RequireVerifiedIsolation");
    assert!(matches!(
        strict_err,
        EnforcementGateError::UnverifiedIsolationRejected { .. }
    ));
}

#[test]
fn vector_e_compromised_shim_cannot_commit_completion_with_stale_fencing_token_or_incarnation() {
    let harness = TestHarness::setup("vec-e");
    let mut gate =
        TrustedDriverEnforcementGate::new(IsolationEnforcementPolicy::TagReceiptFailClosed);
    let op_id = OperationId::from_bytes([0x89; 16]);
    let res = harness.reserve_for_operation(0x61, op_id);
    gate.admit_dispatch(
        &harness.identity,
        &harness.capability,
        &harness.resource,
        harness.make_signed_admission(0x89, &res, IdempotencyKey::from_bytes([0x99; 16]), None),
    )
    .expect("admit");

    let complete_wire = encode_complete_response_proto(
        0x65,
        ProviderIsolationProfile::Verified
            .to_attestation(None)
            .to_wire(),
    );

    // 1. Expired deadline fails closed before touching OperationMachine
    let deadline_err = gate
        .admit_completion(&DriverCompletionSubmission {
            driver_id: harness.driver.driver_id,
            driver_generation: harness.driver.generation,
            driver_fencing_token: harness.driver.fencing_token,
            process_incarnation: Generation::INITIAL,
            now_monotonic_ns: 50_000, // == deadline_monotonic_ns => expired!
            canonical_complete_response_wire: complete_wire.clone(),
            untrusted_shim_claimed_verified: false,
        })
        .expect_err("expired deadline must fail closed");
    assert!(matches!(
        deadline_err,
        EnforcementGateError::DeadlineExpired { .. }
    ));

    // 2. Advance process_incarnation => stale incarnation 1 rejected
    let inc2 = Generation::new(NonZeroU64::new(2).expect("nz"));
    gate.advance_process_incarnation(inc2);
    let inc_err = gate
        .admit_completion(&DriverCompletionSubmission {
            driver_id: harness.driver.driver_id,
            driver_generation: harness.driver.generation,
            driver_fencing_token: harness.driver.fencing_token,
            process_incarnation: Generation::INITIAL,
            now_monotonic_ns: 20_000,
            canonical_complete_response_wire: complete_wire.clone(),
            untrusted_shim_claimed_verified: false,
        })
        .expect_err("stale process_incarnation must fail closed");
    assert!(matches!(
        inc_err,
        EnforcementGateError::StaleProcessIncarnation { .. }
    ));

    // 3. Rotate driver fence in ResourceAuthority => stale fencing_token rejected
    let _ = gate
        .rotate_driver_fence(
            &harness.resource,
            RotateDriverRequest {
                driver_id: harness.driver.driver_id,
                expected_generation: harness.driver.generation,
                expected_fencing_token: harness.driver.fencing_token,
                idempotency_key: IdempotencyKey::from_bytes([0x77; 16]),
                rotated_at_ms: 2_000,
            },
        )
        .expect("rotate fence");
    let fence_err = gate
        .admit_completion(&DriverCompletionSubmission {
            driver_id: harness.driver.driver_id,
            driver_generation: harness.driver.generation,
            driver_fencing_token: harness.driver.fencing_token, // stale pre-rotation token!
            process_incarnation: inc2,
            now_monotonic_ns: 20_000,
            canonical_complete_response_wire: complete_wire,
            untrusted_shim_claimed_verified: false,
        })
        .expect_err("stale fencing_token must fail closed");
    assert!(matches!(
        fence_err,
        EnforcementGateError::StaleFencingToken { .. }
    ));
    assert_eq!(
        gate.inspect_operation(op_id).expect("snap").state,
        OperationState::Dispatched
    );
}

#[test]
fn vector_f_compromised_shim_cannot_skip_uncertain_after_crash_or_blind_retry_with_new_key() {
    let harness = TestHarness::setup("vec-f");
    let mut gate =
        TrustedDriverEnforcementGate::new(IsolationEnforcementPolicy::TagReceiptFailClosed);
    let op_id = OperationId::from_bytes([0x8a; 16]);
    let res = harness.reserve_for_operation(0x71, op_id);
    let original_key = IdempotencyKey::from_bytes([0x9a; 16]);

    gate.admit_dispatch(
        &harness.identity,
        &harness.capability,
        &harness.resource,
        harness.make_signed_admission(0x8a, &res, original_key, None),
    )
    .expect("admit in-flight operation");

    // Simulate adapter/shim crash => in-flight operation forced to UNCERTAIN & reservation Quarantined
    let crashed = gate
        .on_adapter_or_shim_crash(&harness.resource, [0xc0; 32], 3_000)
        .expect("crash convergence");
    assert_eq!(crashed.len(), 1);
    assert_eq!(crashed[0].0, op_id);
    assert!(matches!(
        gate.inspect_operation(op_id).expect("snap").state,
        OperationState::EffectUnknown { .. }
    ));
    assert_eq!(
        harness
            .resource
            .inspect_reservation(res.reservation_id)
            .expect("inspect res")
            .state,
        ReservationState::Quarantined
    );

    // 1. Compromised shim attempts to skip UNCERTAIN and spoof Completed via admit_completion
    let inc2 = Generation::new(NonZeroU64::new(2).expect("nz"));
    let skip_err = gate
        .admit_completion(&DriverCompletionSubmission {
            driver_id: harness.driver.driver_id,
            driver_generation: harness.driver.generation,
            driver_fencing_token: harness.driver.fencing_token,
            process_incarnation: inc2,
            now_monotonic_ns: 20_000,
            canonical_complete_response_wire: encode_complete_response_proto(
                0x68,
                ProviderIsolationProfile::Verified
                    .to_attestation(None)
                    .to_wire(),
            ),
            untrusted_shim_claimed_verified: false,
        })
        .expect_err("cannot skip UNCERTAIN without explicit reconciliation");
    assert!(matches!(
        skip_err,
        EnforcementGateError::UncertainOperationRequiresReconciliation { .. }
    ));

    // 2. Caller/shim attempts blind retry/reconciliation with a NEW IdempotencyKey
    let fresh_key = IdempotencyKey::from_bytes([0x9b; 16]);
    let blind_retry_err = gate
        .reconcile_uncertain_operation(ReconcileUncertainRequest {
            operation_id: op_id,
            idempotency_key: fresh_key,
            driver_id: harness.driver.driver_id,
            driver_generation: harness.driver.generation,
            driver_fencing_token: harness.driver.fencing_token,
            process_incarnation: inc2,
            reconciled_outcome: CompletionOutcome::CancelledBeforeEffect {
                receipt_id: ReceiptId::from_bytes([0xd1; 16]),
            },
        })
        .expect_err("blind retry with fresh IdempotencyKey must be forbidden");
    assert!(matches!(
        blind_retry_err,
        EnforcementGateError::BlindRetryWithNewIdempotencyKeyForbidden { .. }
    ));

    // 3. Explicit fenced reconciliation with the EXACT original IdempotencyKey succeeds
    let reconciled_receipt = ReceiptId::from_bytes([0xd2; 16]);
    let resolved = gate
        .reconcile_uncertain_operation(ReconcileUncertainRequest {
            operation_id: op_id,
            idempotency_key: original_key,
            driver_id: harness.driver.driver_id,
            driver_generation: harness.driver.generation,
            driver_fencing_token: harness.driver.fencing_token,
            process_incarnation: inc2,
            reconciled_outcome: CompletionOutcome::Completed {
                receipt_id: reconciled_receipt,
            },
        })
        .expect("explicit same-idempotency-key reconciliation succeeds");
    assert_eq!(
        resolved.state,
        OperationState::Completed {
            receipt_id: reconciled_receipt
        }
    );
}
