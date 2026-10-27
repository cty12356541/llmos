//! Cross-host remote three-family lease protocol (`[LEASE-PROTO-001]`,
//! `[LEASE-RENEW-001]`, `[TCB-ENDPOINT-001]`, `[DIST-FAIL-001]`, `ADR-0026`
//! §2.3 & §6.3) over `nlos-net` TLS 1.3 mTLS + 5-axis Exporter `CellFence`.
//!
//! # Security & Convergence Guarantees
//!
//! 1. **Mutual Identity Binding (`[TCB-ENDPOINT-001]`)**:
//!    [`RemoteLeaseProtocolServer`] cross-checks every incoming RPC's pinned
//!    [`nlos_net::AuthenticatedPeerSession`] (`peer_claims.cell_identity`,
//!    `peer_claims.node_boot_generation`, `peer_claims.principal_id`) against
//!    the request envelope's `caller_cell`, verifies the domain-separated
//!    Ed25519 custody signature via [`IdentityPeerVerifier`], pins
//!    `caller_cell <-> PrincipalId`, and enforces `caller_cell == lease.holder`
//!    before any state transition. Cross-Cell impersonation attempts fail
//!    closed with zero ledger mutation.
//! 2. **Durable Idempotency & Monotonic `durable_state_seq` (`[LEASE-PROTO-001]`)**:
//!    Every mutating command carries a mandatory 16-byte [`IdempotencyKey`],
//!    `expected_grantor_epoch`, `authority_expiry_tick`, and `caller_boot_generation`.
//!    Committed responses are persisted atomically in `SQLite WAL + FULL`
//!    (`cross_host_proto_idempotency` + `cross_host_proto_state_seq`) and
//!    replayed byte-identically on retry; conflicting payload reuse on an
//!    existing `IdempotencyKey` fails closed.
//! 3. **Uncertain Outcome Convergence (`[DIST-FAIL-001]`)**:
//!    When a post-write socket fault (`AfterFirstPayloadByte` TCP `RST` / read
//!    timeout) yields `RemoteCallOutcome::DeadlineExceededUncertain` with
//!    `RemoteRetryDirective::ReconcileBeforeRetry`, blind retries are blocked by
//!    [`nlos_net::PendingReconciliationTracker`].
//!    [`RemoteLeaseProtocolClient::reconcile_and_replay_uncertain`] queries
//!    authoritative server state (`QueryLeaseReconciliationState`,
//!    `side_effecting = false`), clears the tracker gate, and replays the
//!    original `IdempotencyKey` to converge with zero double-deduction.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use nlos_cell::{
    CellEpoch, CellFence, CellFencingToken, CellIdentity, RemoteCallContract, RemoteCallOutcome,
};
use nlos_clock::AuthorityClock;
use nlos_net::{
    AuthenticatedPeerSession, CellRpcTransport, IdentityPeerVerifier, IncomingRpcRequest,
    MtlsCellTransport, PeerDigestSigner, PeerIdentityError, RpcHandlerReply, RpcRequestHandler,
    RpcResponsePayload, RpcWireStatus, TransportDispatchRefusal,
};
use nlos_types::{
    CapacityLeaseId, DeviceId, ExclusiveDeviceLeaseId, Generation, IdempotencyKey, KeyId,
    PrincipalId, QuotaLeaseId, ReceiptId, SchedulerDomainId,
};
use sha2::{Digest, Sha256};

use crate::clock_horizon::{
    AuditWallTimeHint, ClockHorizonError, ClockSkewPolicy, CrossHostGrantParams,
    CrossHostLeaseFamily, FenceScopeCoverageStatus, GrantorClockBoundedCoordinator,
    GrantorHorizonRecord, HolderClockBoundedLease, MultiEndpointFenceScope,
    PhysicalCleanupProofKind, QuarantineDisposition, SignedFenceBarrierAck,
    SignedLeaseCloseReceipt,
};
use crate::cross_host_store::StoredIdempotencyRow;
use crate::{
    CapacityLeaseGrant, CapacityLeaseState, CapacityReconciliationReceipt, CleanupAttestation,
    DeviceReconciliationReceipt, DeviceResetReceipt, ExclusiveDeviceLeaseGrant,
    ExclusiveDeviceLeaseState, FenceBarrierAck, FenceScope, HostAttachReceipt, LeaseInstant,
    QuotaLeaseGrant, QuotaLeaseState, QuotaReconciliationReceipt,
};

/// Wire schema version for `[LEASE-PROTO-001]` envelopes.
pub const LEASE_PROTO_SCHEMA_VERSION: u16 = 1;

/// Domain separator for holder/operator Ed25519 request signatures.
pub const REMOTE_LEASE_CMD_SIGNING_DOMAIN: &[u8] = b"llmos/lease/remote-proto-command/v1";

/// Domain separator for grantor Ed25519 response signatures.
pub const REMOTE_LEASE_RESP_SIGNING_DOMAIN: &[u8] = b"llmos/lease/remote-proto-response/v1";

/// Reconciliation variants carried by [`RemoteLeaseCommand::ReconcileQuarantinedLease`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteReconcilePayload {
    /// Path A: Holder-signed Ed25519 close receipt settling a `QuotaLease`.
    QuotaViaSignedClose {
        /// Verified holder close receipt.
        close_receipt: SignedLeaseCloseReceipt,
    },
    /// Path B: Advanced epoch + 100% `MultiEndpointFenceScope` barrier `QuotaLease` reconciliation.
    QuotaViaBarrier {
        /// Typed quota reconciliation receipt.
        receipt: QuotaReconciliationReceipt,
    },
    /// Path B: Advanced epoch + 100% barrier + `CleanupAttestation` `CapacityLease` reconciliation.
    CapacityViaBarrier {
        /// Typed capacity reconciliation receipt.
        receipt: CapacityReconciliationReceipt,
    },
    /// Path B: Advanced epoch + 100% barrier + zeroization `DeviceResetReceipt` reconciliation.
    DeviceViaBarrier {
        /// Typed exclusive-device reconciliation receipt.
        receipt: DeviceReconciliationReceipt,
    },
}

/// `[LEASE-PROTO-001]` remote control-plane commands across all three lease families.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteLeaseCommand {
    /// Deducts `face_value` from `AVAILABLE` and grants an `ACTIVE` cross-host `QuotaLease`.
    GrantQuotaLease {
        /// Stable quota lease identifier.
        lease_id: QuotaLeaseId,
        /// Prepaid face value deducted once.
        face_value: u64,
        /// Requested nominal `ttl_grant_ms` (`> 2 * max_clock_skew + safety_margin`).
        ttl_grant_ms: u64,
        /// Required enforcement gateway IDs (`empty` defaults to `[caller_cell]`).
        multi_endpoint_gateways: Vec<[u8; 16]>,
    },
    /// Extends `ttl_grant_ms` without implicitly increasing `face_value` (`[LEASE-RENEW-001]`).
    RenewQuotaLease {
        /// Target `QuotaLeaseId`.
        lease_id: QuotaLeaseId,
        /// New nominal `ttl_grant_ms` (`> 2 * max_clock_skew + safety_margin`).
        new_ttl_grant_ms: u64,
    },
    /// Reports monotonic spent `high_water_spent` (`[LEASE-REPORT-001]`).
    ReportQuotaHighWater {
        /// Target `QuotaLeaseId`.
        lease_id: QuotaLeaseId,
        /// Monotonic spent high-water (`<= face_value`).
        high_water_spent: u64,
    },
    /// Closes a `QuotaLease` with an Ed25519-signed holder [`SignedLeaseCloseReceipt`].
    CloseQuotaLease {
        /// Holder-signed close receipt.
        close_receipt: SignedLeaseCloseReceipt,
    },
    /// Deducts `amount` from `pool_remaining` and prepares a cross-host `CapacityLease`.
    GrantCapacityLease {
        /// Stable capacity lease identifier.
        capacity_lease_id: CapacityLeaseId,
        /// Reserved capacity amount.
        amount: u64,
        /// Requested nominal `ttl_grant_ms`.
        ttl_grant_ms: u64,
        /// Required enforcement gateway IDs (`empty` defaults to `[caller_cell]`).
        multi_endpoint_gateways: Vec<[u8; 16]>,
    },
    /// Submits [`HostAttachReceipt`] (`TARGET_PREPARED -> ACTIVE`).
    AttachCapacityHost {
        /// Host attach receipt binding all 5 lease axes + full `amount`.
        attach_receipt: HostAttachReceipt,
    },
    /// Cleanly returns an active/prepared `CapacityLease` (`RETURNING -> RETURNED`).
    ReturnCapacityLease {
        /// Target `CapacityLeaseId`.
        capacity_lease_id: CapacityLeaseId,
    },
    /// Claims a `FREE` `DeviceLeaseHead` and issues an `ExclusiveDeviceLease`.
    GrantExclusiveDeviceLease {
        /// Target `DeviceId`.
        device_id: DeviceId,
        /// Stable device lease identifier.
        device_lease_id: ExclusiveDeviceLeaseId,
        /// Requested nominal `ttl_grant_ms`.
        ttl_grant_ms: u64,
        /// Required enforcement gateway IDs (`empty` defaults to `[caller_cell]`).
        multi_endpoint_gateways: Vec<[u8; 16]>,
    },
    /// Submits [`DeviceResetReceipt`] (`DEVICE_RESERVED -> RESETTING -> RETURNED`).
    SubmitDeviceResetReceipt {
        /// Target `DeviceId`.
        device_id: DeviceId,
        /// Holder reset+zeroization receipt.
        reset_receipt: DeviceResetReceipt,
    },
    /// Submits an Ed25519-signed gateway [`SignedFenceBarrierAck`].
    SubmitFenceBarrierAck {
        /// Signed gateway barrier ACK.
        signed_ack: SignedFenceBarrierAck,
    },
    /// Reconciles a `QUARANTINED` lease via signed close receipt or 100% barrier + cleanup proof.
    ReconcileQuarantinedLease {
        /// Family-specific reconciliation payload.
        payload: RemoteReconcilePayload,
    },
    /// Read-only authoritative state + idempotency query (`side_effecting = false`, `[DIST-FAIL-001]`).
    QueryLeaseReconciliationState {
        /// Target lease family.
        family: CrossHostLeaseFamily,
        /// 16-byte lease identifier.
        lease_id: [u8; 16],
        /// Optional uncertain `IdempotencyKey` to probe for prior commit.
        probed_idempotency_key: Option<IdempotencyKey>,
    },
}

impl RemoteLeaseCommand {
    /// Returns `true` if this command mutates grantor lease/pool state.
    #[must_use]
    pub const fn is_side_effecting(&self) -> bool {
        !matches!(self, Self::QueryLeaseReconciliationState { .. })
    }

    fn target_lease_identity(&self) -> Option<(CrossHostLeaseFamily, [u8; 16])> {
        match self {
            Self::GrantQuotaLease { lease_id, .. }
            | Self::RenewQuotaLease { lease_id, .. }
            | Self::ReportQuotaHighWater { lease_id, .. } => {
                Some((CrossHostLeaseFamily::Quota, *lease_id.as_bytes()))
            }
            Self::CloseQuotaLease { close_receipt } => Some((
                CrossHostLeaseFamily::Quota,
                *close_receipt.lease_id.as_bytes(),
            )),
            Self::GrantCapacityLease {
                capacity_lease_id, ..
            }
            | Self::ReturnCapacityLease { capacity_lease_id } => Some((
                CrossHostLeaseFamily::Capacity,
                *capacity_lease_id.as_bytes(),
            )),
            Self::AttachCapacityHost { attach_receipt } => Some((
                CrossHostLeaseFamily::Capacity,
                *attach_receipt.capacity_lease_id().as_bytes(),
            )),
            Self::GrantExclusiveDeviceLease {
                device_lease_id, ..
            } => Some((
                CrossHostLeaseFamily::ExclusiveDevice,
                *device_lease_id.as_bytes(),
            )),
            Self::SubmitDeviceResetReceipt { reset_receipt, .. } => Some((
                CrossHostLeaseFamily::ExclusiveDevice,
                *reset_receipt.device_lease_id().as_bytes(),
            )),
            Self::SubmitFenceBarrierAck { signed_ack } => {
                Some((signed_ack.family, signed_ack.ack.lease_id()))
            }
            Self::ReconcileQuarantinedLease { payload } => match payload {
                RemoteReconcilePayload::QuotaViaSignedClose { close_receipt } => Some((
                    CrossHostLeaseFamily::Quota,
                    *close_receipt.lease_id.as_bytes(),
                )),
                RemoteReconcilePayload::QuotaViaBarrier { receipt } => {
                    Some((CrossHostLeaseFamily::Quota, *receipt.lease_id().as_bytes()))
                }
                RemoteReconcilePayload::CapacityViaBarrier { receipt } => Some((
                    CrossHostLeaseFamily::Capacity,
                    *receipt.capacity_lease_id().as_bytes(),
                )),
                RemoteReconcilePayload::DeviceViaBarrier { receipt } => Some((
                    CrossHostLeaseFamily::ExclusiveDevice,
                    *receipt.device_lease_id().as_bytes(),
                )),
            },
            Self::QueryLeaseReconciliationState { .. } => None,
        }
    }
}

/// Authoritative lease + idempotency snapshot returned by
/// [`RemoteLeaseCommand::QueryLeaseReconciliationState`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeaseReconciliationSnapshot {
    /// Whether `(family, lease_id)` exists on the grantor.
    pub lease_known: bool,
    /// Current `QuotaLeaseGrant`, if `family == Quota` and known.
    pub quota_grant: Option<QuotaLeaseGrant>,
    /// Current `CapacityLeaseGrant`, if `family == Capacity` and known.
    pub capacity_grant: Option<CapacityLeaseGrant>,
    /// Current `ExclusiveDeviceLeaseGrant`, if `family == ExclusiveDevice` and known.
    pub device_grant: Option<ExclusiveDeviceLeaseGrant>,
    /// Current honest [`QuarantineDisposition`], if known.
    pub disposition: Option<QuarantineDisposition>,
    /// Current renewal sequence, if known.
    pub renewal_seq: Option<u64>,
    /// Current multi-endpoint barrier coverage, if known.
    pub barrier_coverage: Option<FenceScopeCoverageStatus>,
    /// `Some(durable_state_seq)` if `probed_idempotency_key` already committed
    /// on the grantor; `None` if never committed.
    pub probed_idempotency_committed_seq: Option<u64>,
}

/// Typed positive payloads inside [`RemoteLeaseResponseEnvelope`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteLeaseSuccess {
    /// Committed `QuotaLeaseGrant` + horizon metadata (`Grant`, `Renew`, `Report`, `Close`, `Reconcile`).
    Quota {
        /// Grantor-committed quota lease snapshot.
        grant: QuotaLeaseGrant,
        /// Nominal `ttl_grant_ms`.
        ttl_grant_ms: u64,
        /// Monotonic renewal sequence (`0` on initial grant).
        renewal_seq: u64,
        /// Remaining grantor `AVAILABLE` pool.
        grantor_available: u64,
        /// Honest quarantine disposition.
        disposition: QuarantineDisposition,
    },
    /// Committed `CapacityLeaseGrant` + horizon metadata (`Grant`, `Attach`, `Return`, `Reconcile`).
    Capacity {
        /// Grantor-committed capacity lease snapshot.
        grant: CapacityLeaseGrant,
        /// Nominal `ttl_grant_ms`.
        ttl_grant_ms: u64,
        /// Remaining grantor capacity pool.
        grantor_pool_remaining: u64,
        /// Honest quarantine disposition.
        disposition: QuarantineDisposition,
    },
    /// Committed `ExclusiveDeviceLeaseGrant` + head metadata (`Grant`, `Reset`, `Reconcile`).
    ExclusiveDevice {
        /// Grantor-committed exclusive-device lease snapshot.
        grant: ExclusiveDeviceLeaseGrant,
        /// Nominal `ttl_grant_ms`.
        ttl_grant_ms: u64,
        /// Whether `DeviceLeaseHead` is now `FREE`.
        head_is_free: bool,
        /// Current `DeviceLeaseHead` `reset_generation`.
        head_reset_generation: Generation,
        /// Honest quarantine disposition.
        disposition: QuarantineDisposition,
    },
    /// Recorded gateway `FenceBarrierAck` coverage status.
    BarrierAckRecorded {
        /// Updated multi-endpoint coverage snapshot.
        coverage: FenceScopeCoverageStatus,
    },
    /// Authoritative read-only reconciliation snapshot.
    ReconciliationSnapshot(Box<LeaseReconciliationSnapshot>),
}

/// Fail-closed protocol rejection categories returned inside
/// [`RemoteLeaseResponseEnvelope`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteLeaseReject {
    /// Wire payload or schema version could not be decoded.
    MalformedWire(String),
    /// Authenticated TLS 1.3 `peer_session` disagreed with `envelope.caller_cell`
    /// or `signer_principal_id` (`[TCB-ENDPOINT-001]`).
    PeerSessionIdentityMismatch(String),
    /// Caller attempted to operate on another Cell's lease (`[TCB-ENDPOINT-001]`).
    CrossCellImpersonation {
        /// Authenticated caller Cell.
        caller_cell: CellIdentity,
        /// Bound lease holder Cell.
        expected_holder_cell: CellIdentity,
    },
    /// Ed25519 request signature or custody key check failed.
    SignatureRejected(String),
    /// `IdempotencyKey` was reused with a different caller or command payload.
    IdempotencyConflict,
    /// `expected_grantor_epoch` is stale or mismatched against grantor `CellAuthority`.
    StaleOrMismatchedEpoch {
        /// Caller's `expected_grantor_epoch`.
        expected: CellEpoch,
        /// Grantor's authoritative `CellEpoch`.
        actual: CellEpoch,
    },
    /// Grantor `AuthorityClock` logical watermark exceeded `authority_expiry_tick`.
    AuthorityTickExpired {
        /// Request's `authority_expiry_tick`.
        expiry_tick: u64,
        /// Grantor's current `AuthorityClock` tick.
        current_tick: u64,
    },
    /// `RTT / 2 > max_clock_skew` or monotonic rollback triggered fail-closed quarantine.
    ClockSkewQuarantined(String),
    /// Horizon / ledger / reconciliation state machine rejected the command.
    CoordinatorRejected(String),
}

/// Signed `[LEASE-PROTO-001]` request envelope transmitted over `ControlRpc`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteLeaseRequestEnvelope {
    /// Wire schema version (`LEASE_PROTO_SCHEMA_VERSION`).
    pub schema_version: u16,
    /// Mandatory 16-byte idempotency key matching `IncomingRpcRequest::idempotency_key`.
    pub idempotency_key: IdempotencyKey,
    /// Authenticated caller `CellIdentity`.
    pub caller_cell: CellIdentity,
    /// Authenticated caller `node_boot_generation`.
    pub caller_boot_generation: Generation,
    /// Expected grantor `CellEpoch`.
    pub expected_grantor_epoch: CellEpoch,
    /// `AuthorityClock` logical tick ceiling for request admission.
    pub authority_expiry_tick: u64,
    /// Optional caller-observed control-plane RTT sample (`ms`) for skew gating.
    pub measured_rtt_hint_ms: Option<u64>,
    /// Non-authoritative wall-clock audit hint (`[MODEL-TIME-001]`).
    pub audit_wall_hint: AuditWallTimeHint,
    /// Typed `[LEASE-PROTO-001]` command payload.
    pub command: RemoteLeaseCommand,
    /// Signing `PrincipalId` of `caller_cell`.
    pub signer_principal_id: PrincipalId,
    /// Signing custody `KeyId`.
    pub signer_key_id: KeyId,
    /// Signing custody key generation (`rotation_epoch`).
    pub signer_key_generation: Generation,
    /// Ed25519 signature over [`Self::signing_digest`] under [`REMOTE_LEASE_CMD_SIGNING_DOMAIN`].
    pub ed25519_signature: [u8; 64],
}

impl RemoteLeaseRequestEnvelope {
    /// Signs a `[LEASE-PROTO-001]` request envelope inside `signer`'s custody vault.
    ///
    /// # Errors
    ///
    /// Fails closed if custody signing fails.
    #[allow(clippy::too_many_arguments)]
    pub fn sign(
        idempotency_key: IdempotencyKey,
        caller_cell: CellIdentity,
        caller_boot_generation: Generation,
        expected_grantor_epoch: CellEpoch,
        authority_expiry_tick: u64,
        measured_rtt_hint_ms: Option<u64>,
        audit_wall_hint: AuditWallTimeHint,
        command: RemoteLeaseCommand,
        signer: &dyn PeerDigestSigner,
    ) -> Result<Self, PeerIdentityError> {
        let mut env = Self {
            schema_version: LEASE_PROTO_SCHEMA_VERSION,
            idempotency_key,
            caller_cell,
            caller_boot_generation,
            expected_grantor_epoch,
            authority_expiry_tick,
            measured_rtt_hint_ms,
            audit_wall_hint,
            command,
            signer_principal_id: signer.principal_id(),
            signer_key_id: signer.key_id(),
            signer_key_generation: signer.key_generation(),
            ed25519_signature: [0u8; 64],
        };
        let digest = env.signing_digest();
        env.ed25519_signature = signer.sign_domain_digest(
            REMOTE_LEASE_CMD_SIGNING_DOMAIN,
            digest,
            audit_wall_hint.unix_epoch_ms(),
        )?;
        Ok(env)
    }

    /// Deterministic `SHA-256` digest of `(caller_cell, expected_grantor_epoch, command)`
    /// used to detect conflicting payload reuse on the same `IdempotencyKey`.
    #[must_use]
    pub fn command_digest(&self) -> [u8; 32] {
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(b"llmos/lease/idempotency-cmd-digest/v1");
        buf.extend_from_slice(self.caller_cell.as_bytes());
        buf.extend_from_slice(&self.caller_boot_generation.get().to_be_bytes());
        buf.extend_from_slice(&self.expected_grantor_epoch.get().to_be_bytes());
        encode_command(&mut buf, &self.command);
        Sha256::digest(&buf).into()
    }

    /// Canonical `SHA-256` digest signed by `ed25519_signature`.
    #[must_use]
    pub fn signing_digest(&self) -> [u8; 32] {
        let mut buf = Vec::with_capacity(320);
        buf.extend_from_slice(REMOTE_LEASE_CMD_SIGNING_DOMAIN);
        self.encode_unsigned(&mut buf);
        Sha256::digest(&buf).into()
    }

    fn encode_unsigned(&self, out: &mut Vec<u8>) {
        put_u16(out, self.schema_version);
        out.extend_from_slice(self.idempotency_key.as_bytes());
        out.extend_from_slice(self.caller_cell.as_bytes());
        put_u64(out, self.caller_boot_generation.get());
        put_u64(out, self.expected_grantor_epoch.get());
        put_u64(out, self.authority_expiry_tick);
        match self.measured_rtt_hint_ms {
            Some(rtt) => {
                put_u8(out, 1);
                put_u64(out, rtt);
            }
            None => put_u8(out, 0),
        }
        put_u64(out, self.audit_wall_hint.unix_epoch_ms());
        encode_command(out, &self.command);
        out.extend_from_slice(self.signer_principal_id.as_bytes());
        out.extend_from_slice(self.signer_key_id.as_bytes());
        put_u64(out, self.signer_key_generation.get());
    }

    /// Encodes the full signed envelope to deterministic wire bytes.
    #[must_use]
    pub fn to_wire_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(384);
        self.encode_unsigned(&mut out);
        out.extend_from_slice(&self.ed25519_signature);
        out
    }

    /// Decodes a signed request envelope from exact wire bytes.
    ///
    /// # Errors
    ///
    /// Fails closed on truncated, trailing, or malformed wire bytes.
    pub fn from_wire_bytes(bytes: &[u8]) -> Result<Self, &'static str> {
        let mut r = ByteReader::new(bytes);
        let schema_version = r.read_u16()?;
        let idempotency_key = IdempotencyKey::from_bytes(r.read_16()?);
        let caller_cell = read_cell(&mut r)?;
        let caller_boot_generation = read_gen(&mut r)?;
        let expected_grantor_epoch = read_epoch(&mut r)?;
        let authority_expiry_tick = r.read_u64()?;
        let measured_rtt_hint_ms = match r.read_u8()? {
            0 => None,
            1 => Some(r.read_u64()?),
            _ => return Err("invalid option tag for measured_rtt_hint_ms"),
        };
        let audit_wall_hint = AuditWallTimeHint::new(r.read_u64()?);
        let command = decode_command(&mut r)?;
        let signer_principal_id = PrincipalId::from_bytes(r.read_16()?);
        let signer_key_id = KeyId::from_bytes(r.read_16()?);
        let signer_key_generation = read_gen(&mut r)?;
        let ed25519_signature = r.read_64()?;
        r.finish()?;
        Ok(Self {
            schema_version,
            idempotency_key,
            caller_cell,
            caller_boot_generation,
            expected_grantor_epoch,
            authority_expiry_tick,
            measured_rtt_hint_ms,
            audit_wall_hint,
            command,
            signer_principal_id,
            signer_key_id,
            signer_key_generation,
            ed25519_signature,
        })
    }
}

/// Signed `[LEASE-PROTO-001]` response envelope returned over `ControlRpc`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteLeaseResponseEnvelope {
    /// Wire schema version (`LEASE_PROTO_SCHEMA_VERSION`).
    pub schema_version: u16,
    /// Echoed request `IdempotencyKey`.
    pub idempotency_key: IdempotencyKey,
    /// Authoritative grantor `CellIdentity`.
    pub grantor_cell: CellIdentity,
    /// Authoritative grantor `node_boot_generation`.
    pub grantor_boot_generation: Generation,
    /// Authoritative grantor `CellEpoch`.
    pub grantor_epoch: CellEpoch,
    /// Authoritative grantor `CellFencingToken`.
    pub grantor_fencing_token: CellFencingToken,
    /// Monotonic durable protocol state sequence number.
    pub durable_state_seq: u64,
    /// `true` when replayed from the durable idempotency table without re-execution.
    pub idempotency_replayed: bool,
    /// Typed positive or fail-closed rejection result.
    pub result: Result<RemoteLeaseSuccess, RemoteLeaseReject>,
    /// Grantor signing `PrincipalId`.
    pub signer_principal_id: PrincipalId,
    /// Grantor signing custody `KeyId`.
    pub signer_key_id: KeyId,
    /// Grantor signing custody key generation.
    pub signer_key_generation: Generation,
    /// Ed25519 signature over [`Self::signing_digest`] under [`REMOTE_LEASE_RESP_SIGNING_DOMAIN`].
    pub ed25519_signature: [u8; 64],
}

impl RemoteLeaseResponseEnvelope {
    fn sign(
        idempotency_key: IdempotencyKey,
        fence: CellFence,
        durable_state_seq: u64,
        idempotency_replayed: bool,
        result: Result<RemoteLeaseSuccess, RemoteLeaseReject>,
        signer: &dyn PeerDigestSigner,
        signed_at_ms: u64,
    ) -> Result<Self, PeerIdentityError> {
        let mut env = Self {
            schema_version: LEASE_PROTO_SCHEMA_VERSION,
            idempotency_key,
            grantor_cell: fence.identity(),
            grantor_boot_generation: fence.node_boot_generation(),
            grantor_epoch: fence.epoch(),
            grantor_fencing_token: fence.fencing_token(),
            durable_state_seq,
            idempotency_replayed,
            result,
            signer_principal_id: signer.principal_id(),
            signer_key_id: signer.key_id(),
            signer_key_generation: signer.key_generation(),
            ed25519_signature: [0u8; 64],
        };
        let digest = env.signing_digest();
        env.ed25519_signature =
            signer.sign_domain_digest(REMOTE_LEASE_RESP_SIGNING_DOMAIN, digest, signed_at_ms)?;
        Ok(env)
    }

    /// Canonical `SHA-256` digest signed by the grantor's custody key.
    #[must_use]
    pub fn signing_digest(&self) -> [u8; 32] {
        let mut buf = Vec::with_capacity(320);
        buf.extend_from_slice(REMOTE_LEASE_RESP_SIGNING_DOMAIN);
        self.encode_unsigned(&mut buf);
        Sha256::digest(&buf).into()
    }

    /// Verifies the grantor's Ed25519 signature and nominal `grantor_cell` identity.
    ///
    /// # Errors
    ///
    /// Fails closed on mismatched `grantor_cell` or invalid/revoked grantor key.
    pub fn verify_grantor_signature(
        &self,
        expected_grantor_cell: CellIdentity,
        verifier: &dyn IdentityPeerVerifier,
        verified_at_ms: u64,
    ) -> Result<(), RemoteLeaseClientError> {
        if self.schema_version != LEASE_PROTO_SCHEMA_VERSION {
            return Err(RemoteLeaseClientError::MalformedResponse(
                "unsupported response schema_version",
            ));
        }
        if self.grantor_cell != expected_grantor_cell {
            return Err(RemoteLeaseClientError::GrantorIdentityMismatch {
                expected: expected_grantor_cell,
                actual: self.grantor_cell,
            });
        }
        verifier
            .verify_peer_digest(
                self.signer_principal_id,
                self.signer_key_id,
                self.signer_key_generation,
                self.signing_digest(),
                self.ed25519_signature,
                verified_at_ms,
            )
            .map_err(RemoteLeaseClientError::SignatureVerification)?;
        Ok(())
    }

    fn encode_unsigned(&self, out: &mut Vec<u8>) {
        put_u16(out, self.schema_version);
        out.extend_from_slice(self.idempotency_key.as_bytes());
        out.extend_from_slice(self.grantor_cell.as_bytes());
        put_u64(out, self.grantor_boot_generation.get());
        put_u64(out, self.grantor_epoch.get());
        put_u64(out, self.grantor_fencing_token.get());
        put_u64(out, self.durable_state_seq);
        put_bool(out, self.idempotency_replayed);
        encode_result(out, &self.result);
        out.extend_from_slice(self.signer_principal_id.as_bytes());
        out.extend_from_slice(self.signer_key_id.as_bytes());
        put_u64(out, self.signer_key_generation.get());
    }

    /// Encodes the full signed response envelope to deterministic wire bytes.
    #[must_use]
    pub fn to_wire_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(384);
        self.encode_unsigned(&mut out);
        out.extend_from_slice(&self.ed25519_signature);
        out
    }

    /// Decodes a signed response envelope from exact wire bytes.
    ///
    /// # Errors
    ///
    /// Fails closed on truncated, trailing, or malformed bytes.
    pub fn from_wire_bytes(bytes: &[u8]) -> Result<Self, &'static str> {
        let mut r = ByteReader::new(bytes);
        let schema_version = r.read_u16()?;
        let idempotency_key = IdempotencyKey::from_bytes(r.read_16()?);
        let grantor_cell = read_cell(&mut r)?;
        let grantor_boot_generation = read_gen(&mut r)?;
        let grantor_epoch = read_epoch(&mut r)?;
        let grantor_fencing_token = read_token(&mut r)?;
        let durable_state_seq = r.read_u64()?;
        let idempotency_replayed = r.read_bool()?;
        let result = decode_result(&mut r)?;
        let signer_principal_id = PrincipalId::from_bytes(r.read_16()?);
        let signer_key_id = KeyId::from_bytes(r.read_16()?);
        let signer_key_generation = read_gen(&mut r)?;
        let ed25519_signature = r.read_64()?;
        r.finish()?;
        Ok(Self {
            schema_version,
            idempotency_key,
            grantor_cell,
            grantor_boot_generation,
            grantor_epoch,
            grantor_fencing_token,
            durable_state_seq,
            idempotency_replayed,
            result,
            signer_principal_id,
            signer_key_id,
            signer_key_generation,
            ed25519_signature,
        })
    }
}

struct ServerMutableState {
    coordinator: GrantorClockBoundedCoordinator,
    idempotency_records: BTreeMap<IdempotencyKey, StoredIdempotencyRow>,
    last_state_seq: u64,
}

/// Grantor-side `[LEASE-PROTO-001]` RPC handler mounted on
/// [`nlos_net::MtlsEndpointListener`] (`impl RpcRequestHandler`).
pub struct RemoteLeaseProtocolServer {
    state: Arc<Mutex<ServerMutableState>>,
    authority_clock: Arc<AuthorityClock>,
    default_skew_policy: ClockSkewPolicy,
    grantor_mono_clock: Arc<dyn Fn() -> crate::clock_horizon::MonotonicInstantMs + Send + Sync>,
    verify_clock_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    local_signer: Arc<dyn PeerDigestSigner>,
    peer_verifier: Arc<dyn IdentityPeerVerifier>,
}

impl fmt::Debug for RemoteLeaseProtocolServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteLeaseProtocolServer")
            .field("default_skew_policy", &self.default_skew_policy)
            .finish_non_exhaustive()
    }
}

impl RemoteLeaseProtocolServer {
    /// Creates a `[LEASE-PROTO-001]` server wrapping `coordinator` (rehydrating
    /// durable `cross_host_proto_idempotency` and `cross_host_proto_state_seq`
    /// automatically when `coordinator` is backed by [`crate::SqliteLeaseStore`]).
    ///
    /// # Errors
    ///
    /// Fails closed if durable idempotency table rehydration fails.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        coordinator: GrantorClockBoundedCoordinator,
        authority_clock: Arc<AuthorityClock>,
        default_skew_policy: ClockSkewPolicy,
        grantor_mono_clock: Arc<dyn Fn() -> crate::clock_horizon::MonotonicInstantMs + Send + Sync>,
        verify_clock_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
        local_signer: Arc<dyn PeerDigestSigner>,
        peer_verifier: Arc<dyn IdentityPeerVerifier>,
    ) -> Result<Self, crate::LeaseStoreError> {
        let (idempotency_records, last_state_seq) = if let Some(store) = coordinator.store() {
            let rehydrated = store.ensure_and_rehydrate_cross_host()?;
            (rehydrated.idempotency_records, rehydrated.last_state_seq)
        } else {
            (BTreeMap::new(), 0)
        };
        Ok(Self {
            state: Arc::new(Mutex::new(ServerMutableState {
                coordinator,
                idempotency_records,
                last_state_seq,
            })),
            authority_clock,
            default_skew_policy,
            grantor_mono_clock,
            verify_clock_ms,
            local_signer,
            peer_verifier,
        })
    }

    /// Executes `f` with locked access to the underlying
    /// [`GrantorClockBoundedCoordinator`] (for grantor horizon sweeps, epoch
    /// advances, and audit inspection).
    pub fn with_coordinator<R>(
        &self,
        f: impl FnOnce(&mut GrantorClockBoundedCoordinator) -> R,
    ) -> R {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard.coordinator)
    }

    /// Synchronous handler core invoked by [`RpcRequestHandler::handle_rpc`].
    #[must_use]
    pub fn process_incoming_sync(&self, request: &IncomingRpcRequest) -> RpcHandlerReply {
        let now_verify_ms = (self.verify_clock_ms)();
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let current_fence = guard.coordinator.current_fence();

        let envelope = match RemoteLeaseRequestEnvelope::from_wire_bytes(&request.payload) {
            Ok(env) => env,
            Err(reason) => {
                return self.build_reply(
                    request.idempotency_key,
                    current_fence,
                    guard.last_state_seq,
                    false,
                    Err(RemoteLeaseReject::MalformedWire(reason.to_owned())),
                    now_verify_ms,
                );
            }
        };

        if envelope.schema_version != LEASE_PROTO_SCHEMA_VERSION {
            return self.build_reply(
                request.idempotency_key,
                current_fence,
                guard.last_state_seq,
                false,
                Err(RemoteLeaseReject::MalformedWire(
                    "unsupported schema_version".to_owned(),
                )),
                now_verify_ms,
            );
        }
        if envelope.idempotency_key != request.idempotency_key {
            return self.build_reply(
                request.idempotency_key,
                current_fence,
                guard.last_state_seq,
                false,
                Err(RemoteLeaseReject::MalformedWire(
                    "frame idempotency_key != envelope idempotency_key".to_owned(),
                )),
                now_verify_ms,
            );
        }

        if let Err(reject) =
            self.authenticate_caller(&request.peer_session, &envelope, now_verify_ms)
        {
            return self.build_reply(
                envelope.idempotency_key,
                current_fence,
                guard.last_state_seq,
                false,
                Err(reject),
                now_verify_ms,
            );
        }

        if let Err(err) = guard
            .coordinator
            .pin_holder_principal(envelope.caller_cell, envelope.signer_principal_id)
        {
            return self.build_reply(
                envelope.idempotency_key,
                current_fence,
                guard.last_state_seq,
                false,
                Err(RemoteLeaseReject::PeerSessionIdentityMismatch(
                    err.to_string(),
                )),
                now_verify_ms,
            );
        }

        let cmd_digest = envelope.command_digest();
        if let Some(reply) = self.try_replay_cached_idempotency(
            &guard,
            &envelope,
            cmd_digest,
            current_fence,
            now_verify_ms,
        ) {
            return reply;
        }

        self.execute_authenticated_command(&mut guard, &envelope, cmd_digest, now_verify_ms)
    }

    fn try_replay_cached_idempotency(
        &self,
        guard: &ServerMutableState,
        envelope: &RemoteLeaseRequestEnvelope,
        cmd_digest: [u8; 32],
        current_fence: CellFence,
        now_verify_ms: u64,
    ) -> Option<RpcHandlerReply> {
        let cached = guard
            .idempotency_records
            .get(&envelope.idempotency_key)
            .cloned()?;
        if cached.caller_cell != envelope.caller_cell || cached.command_digest != cmd_digest {
            return Some(self.build_reply(
                envelope.idempotency_key,
                current_fence,
                guard.last_state_seq,
                false,
                Err(RemoteLeaseReject::IdempotencyConflict),
                now_verify_ms,
            ));
        }
        let mut prev_resp =
            RemoteLeaseResponseEnvelope::from_wire_bytes(&cached.response_bytes).ok()?;
        prev_resp.idempotency_replayed = true;
        let resigned = RemoteLeaseResponseEnvelope::sign(
            prev_resp.idempotency_key,
            current_fence,
            cached.durable_state_seq,
            true,
            prev_resp.result,
            self.local_signer.as_ref(),
            now_verify_ms,
        )
        .ok()?;
        Some(RpcHandlerReply {
            observed_epoch: current_fence.epoch(),
            idempotency_replayed: true,
            status: RpcWireStatus::Ok,
            payload: resigned.to_wire_bytes(),
        })
    }

    fn authenticate_caller(
        &self,
        peer_session: &AuthenticatedPeerSession,
        envelope: &RemoteLeaseRequestEnvelope,
        now_verify_ms: u64,
    ) -> Result<(), RemoteLeaseReject> {
        if peer_session.peer_claims.cell_identity != envelope.caller_cell {
            return Err(RemoteLeaseReject::PeerSessionIdentityMismatch(format!(
                "TLS peer cell {:?} != envelope caller_cell {:?}",
                peer_session.peer_claims.cell_identity, envelope.caller_cell
            )));
        }
        if peer_session.peer_claims.node_boot_generation != envelope.caller_boot_generation {
            return Err(RemoteLeaseReject::PeerSessionIdentityMismatch(
                "TLS peer node_boot_generation != envelope caller_boot_generation".to_owned(),
            ));
        }
        if peer_session.peer_claims.principal_id != envelope.signer_principal_id {
            return Err(RemoteLeaseReject::PeerSessionIdentityMismatch(
                "TLS peer principal_id != envelope signer_principal_id".to_owned(),
            ));
        }
        self.peer_verifier
            .verify_peer_digest(
                envelope.signer_principal_id,
                envelope.signer_key_id,
                envelope.signer_key_generation,
                envelope.signing_digest(),
                envelope.ed25519_signature,
                now_verify_ms,
            )
            .map_err(|err| RemoteLeaseReject::SignatureRejected(err.to_string()))?;
        Ok(())
    }

    fn execute_authenticated_command(
        &self,
        guard: &mut ServerMutableState,
        envelope: &RemoteLeaseRequestEnvelope,
        cmd_digest: [u8; 32],
        now_verify_ms: u64,
    ) -> RpcHandlerReply {
        let current_fence = guard.coordinator.current_fence();

        // Read-only reconciliation query bypasses epoch-lockout & RTT quarantine so
        // callers can always inspect authoritative state during recovery!
        if let RemoteLeaseCommand::QueryLeaseReconciliationState {
            family,
            lease_id,
            probed_idempotency_key,
        } = &envelope.command
        {
            let snapshot = Self::build_reconciliation_snapshot(
                guard,
                *family,
                *lease_id,
                *probed_idempotency_key,
            );
            return self.build_reply(
                envelope.idempotency_key,
                current_fence,
                guard.last_state_seq,
                false,
                Ok(RemoteLeaseSuccess::ReconciliationSnapshot(Box::new(
                    snapshot,
                ))),
                now_verify_ms,
            );
        }

        if let Err(reject) =
            self.check_mutating_preflight(&mut guard.coordinator, &current_fence, envelope)
        {
            return self.build_reply(
                envelope.idempotency_key,
                current_fence,
                guard.last_state_seq,
                false,
                Err(reject),
                now_verify_ms,
            );
        }

        let now_grantor_mono = (self.grantor_mono_clock)();
        let exec_outcome = self.dispatch_mutating_command(
            &mut guard.coordinator,
            &current_fence,
            envelope,
            now_grantor_mono,
            now_verify_ms,
        );

        match exec_outcome {
            Ok(success) => {
                self.persist_and_sign_success(guard, envelope, cmd_digest, success, now_verify_ms)
            }
            Err(reject) => self.build_reply(
                envelope.idempotency_key,
                guard.coordinator.current_fence(),
                guard.last_state_seq,
                false,
                Err(reject),
                now_verify_ms,
            ),
        }
    }

    fn check_mutating_preflight(
        &self,
        coordinator: &mut GrantorClockBoundedCoordinator,
        current_fence: &CellFence,
        envelope: &RemoteLeaseRequestEnvelope,
    ) -> Result<(), RemoteLeaseReject> {
        if envelope.expected_grantor_epoch != current_fence.epoch() {
            return Err(RemoteLeaseReject::StaleOrMismatchedEpoch {
                expected: envelope.expected_grantor_epoch,
                actual: current_fence.epoch(),
            });
        }

        let current_tick = self
            .authority_clock
            .inspect()
            .map_err(|err| RemoteLeaseReject::ClockSkewQuarantined(err.to_string()))?
            .as_u64();
        if current_tick > envelope.authority_expiry_tick {
            return Err(RemoteLeaseReject::AuthorityTickExpired {
                expiry_tick: envelope.authority_expiry_tick,
                current_tick,
            });
        }

        if let Some(rtt_ms) = envelope.measured_rtt_hint_ms
            && let Err(violation) = self.default_skew_policy.verify_rtt_sample(rtt_ms)
        {
            if let Some((fam, id)) = envelope.command.target_lease_identity() {
                let _ =
                    coordinator.quarantine_on_clock_violation(current_fence, fam, id, &violation);
            }
            return Err(RemoteLeaseReject::ClockSkewQuarantined(
                violation.to_string(),
            ));
        }
        Ok(())
    }

    fn persist_and_sign_success(
        &self,
        guard: &mut ServerMutableState,
        envelope: &RemoteLeaseRequestEnvelope,
        cmd_digest: [u8; 32],
        success: RemoteLeaseSuccess,
        now_verify_ms: u64,
    ) -> RpcHandlerReply {
        let prev_seq = guard.last_state_seq;
        let next_seq = prev_seq.saturating_add(1);
        let updated_fence = guard.coordinator.current_fence();
        let Ok(signed_resp) = RemoteLeaseResponseEnvelope::sign(
            envelope.idempotency_key,
            updated_fence,
            next_seq,
            false,
            Ok(success),
            self.local_signer.as_ref(),
            now_verify_ms,
        ) else {
            return self.build_reply(
                envelope.idempotency_key,
                updated_fence,
                prev_seq,
                false,
                Err(RemoteLeaseReject::SignatureRejected(
                    "grantor custody signing failed".to_owned(),
                )),
                now_verify_ms,
            );
        };
        let wire_bytes = signed_resp.to_wire_bytes();
        let row = StoredIdempotencyRow {
            idempotency_key: envelope.idempotency_key,
            caller_cell: envelope.caller_cell,
            command_digest: cmd_digest,
            durable_state_seq: next_seq,
            response_bytes: wire_bytes.clone(),
        };
        if let Some(store) = guard.coordinator.store()
            && let Err(store_err) = store.persist_cross_host_idempotency(&row, prev_seq)
        {
            return self.build_reply(
                envelope.idempotency_key,
                updated_fence,
                prev_seq,
                false,
                Err(RemoteLeaseReject::CoordinatorRejected(
                    store_err.to_string(),
                )),
                now_verify_ms,
            );
        }
        guard.last_state_seq = next_seq;
        guard
            .idempotency_records
            .insert(envelope.idempotency_key, row);
        RpcHandlerReply {
            observed_epoch: updated_fence.epoch(),
            idempotency_replayed: false,
            status: RpcWireStatus::Ok,
            payload: wire_bytes,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn dispatch_mutating_command(
        &self,
        coordinator: &mut GrantorClockBoundedCoordinator,
        fence: &CellFence,
        envelope: &RemoteLeaseRequestEnvelope,
        now_grantor_mono: crate::clock_horizon::MonotonicInstantMs,
        now_verify_ms: u64,
    ) -> Result<RemoteLeaseSuccess, RemoteLeaseReject> {
        let build_scope =
            |gateways: &[[u8; 16]]| -> Result<MultiEndpointFenceScope, RemoteLeaseReject> {
                if gateways.is_empty() {
                    Ok(MultiEndpointFenceScope::from_single_cell(FenceScope::cell(
                        envelope.caller_cell,
                    )))
                } else {
                    MultiEndpointFenceScope::with_gateways(
                        envelope.caller_cell,
                        gateways.iter().copied(),
                    )
                    .map_err(map_horizon_err)
                }
            };

        match &envelope.command {
            RemoteLeaseCommand::GrantQuotaLease {
                lease_id,
                face_value,
                ttl_grant_ms,
                multi_endpoint_gateways,
            } => {
                let scope = build_scope(multi_endpoint_gateways)?;
                let (grant, rec) = coordinator
                    .grant_quota_cross_host(
                        fence,
                        *lease_id,
                        *face_value,
                        CrossHostGrantParams {
                            holder_cell: envelope.caller_cell,
                            holder_boot_generation: envelope.caller_boot_generation,
                            grantor_commit_mono: now_grantor_mono,
                            ttl_grant_ms: *ttl_grant_ms,
                            policy: self.default_skew_policy,
                            authority_expiry_tick: envelope.authority_expiry_tick,
                            multi_endpoint_scope: scope,
                        },
                    )
                    .map_err(map_horizon_err)?;
                Ok(RemoteLeaseSuccess::Quota {
                    grant,
                    ttl_grant_ms: rec.ttl_grant_ms,
                    renewal_seq: rec.renewal_seq,
                    grantor_available: coordinator.quota().available(),
                    disposition: rec.disposition,
                })
            }
            RemoteLeaseCommand::RenewQuotaLease {
                lease_id,
                new_ttl_grant_ms,
            } => {
                let (grant, rec) = coordinator
                    .renew_quota_cross_host(
                        fence,
                        *lease_id,
                        envelope.caller_cell,
                        now_grantor_mono,
                        *new_ttl_grant_ms,
                        envelope.authority_expiry_tick,
                    )
                    .map_err(map_horizon_err)?;
                Ok(RemoteLeaseSuccess::Quota {
                    grant,
                    ttl_grant_ms: rec.ttl_grant_ms,
                    renewal_seq: rec.renewal_seq,
                    grantor_available: coordinator.quota().available(),
                    disposition: rec.disposition,
                })
            }
            RemoteLeaseCommand::ReportQuotaHighWater {
                lease_id,
                high_water_spent,
            } => {
                let grant = coordinator
                    .report_quota_high_water_for_holder(
                        fence,
                        *lease_id,
                        envelope.caller_cell,
                        *high_water_spent,
                    )
                    .map_err(map_horizon_err)?;
                let rec = coordinator
                    .horizon_record(CrossHostLeaseFamily::Quota, *lease_id.as_bytes())
                    .cloned()
                    .ok_or_else(|| {
                        RemoteLeaseReject::CoordinatorRejected("missing quota horizon".to_owned())
                    })?;
                Ok(RemoteLeaseSuccess::Quota {
                    grant,
                    ttl_grant_ms: rec.ttl_grant_ms,
                    renewal_seq: rec.renewal_seq,
                    grantor_available: coordinator.quota().available(),
                    disposition: rec.disposition,
                })
            }
            RemoteLeaseCommand::CloseQuotaLease { close_receipt } => {
                if close_receipt.holder_cell != envelope.caller_cell {
                    return Err(RemoteLeaseReject::CrossCellImpersonation {
                        caller_cell: envelope.caller_cell,
                        expected_holder_cell: close_receipt.holder_cell,
                    });
                }
                let grant = coordinator
                    .settle_quota_with_signed_close_receipt(
                        fence,
                        close_receipt,
                        self.peer_verifier.as_ref(),
                        now_verify_ms,
                    )
                    .map_err(map_horizon_err)?;
                let rec = coordinator
                    .horizon_record(CrossHostLeaseFamily::Quota, *grant.lease_id().as_bytes())
                    .cloned()
                    .ok_or_else(|| {
                        RemoteLeaseReject::CoordinatorRejected("missing quota horizon".to_owned())
                    })?;
                Ok(RemoteLeaseSuccess::Quota {
                    grant,
                    ttl_grant_ms: rec.ttl_grant_ms,
                    renewal_seq: rec.renewal_seq,
                    grantor_available: coordinator.quota().available(),
                    disposition: rec.disposition,
                })
            }
            RemoteLeaseCommand::GrantCapacityLease {
                capacity_lease_id,
                amount,
                ttl_grant_ms,
                multi_endpoint_gateways,
            } => {
                let scope = build_scope(multi_endpoint_gateways)?;
                let (grant, rec) = coordinator
                    .grant_capacity_cross_host(
                        fence,
                        *capacity_lease_id,
                        *amount,
                        CrossHostGrantParams {
                            holder_cell: envelope.caller_cell,
                            holder_boot_generation: envelope.caller_boot_generation,
                            grantor_commit_mono: now_grantor_mono,
                            ttl_grant_ms: *ttl_grant_ms,
                            policy: self.default_skew_policy,
                            authority_expiry_tick: envelope.authority_expiry_tick,
                            multi_endpoint_scope: scope,
                        },
                    )
                    .map_err(map_horizon_err)?;
                Ok(RemoteLeaseSuccess::Capacity {
                    grant,
                    ttl_grant_ms: rec.ttl_grant_ms,
                    grantor_pool_remaining: coordinator.capacity().pool_remaining(),
                    disposition: rec.disposition,
                })
            }
            RemoteLeaseCommand::AttachCapacityHost { attach_receipt } => {
                let grant = coordinator
                    .attach_capacity_for_holder(fence, envelope.caller_cell, *attach_receipt)
                    .map_err(map_horizon_err)?;
                let rec = coordinator
                    .horizon_record(
                        CrossHostLeaseFamily::Capacity,
                        *grant.capacity_lease_id().as_bytes(),
                    )
                    .cloned()
                    .ok_or_else(|| {
                        RemoteLeaseReject::CoordinatorRejected(
                            "missing capacity horizon".to_owned(),
                        )
                    })?;
                Ok(RemoteLeaseSuccess::Capacity {
                    grant,
                    ttl_grant_ms: rec.ttl_grant_ms,
                    grantor_pool_remaining: coordinator.capacity().pool_remaining(),
                    disposition: rec.disposition,
                })
            }
            RemoteLeaseCommand::ReturnCapacityLease { capacity_lease_id } => {
                let grant = coordinator
                    .return_capacity_for_holder(fence, *capacity_lease_id, envelope.caller_cell)
                    .map_err(map_horizon_err)?;
                let rec = coordinator
                    .horizon_record(
                        CrossHostLeaseFamily::Capacity,
                        *capacity_lease_id.as_bytes(),
                    )
                    .cloned()
                    .ok_or_else(|| {
                        RemoteLeaseReject::CoordinatorRejected(
                            "missing capacity horizon".to_owned(),
                        )
                    })?;
                Ok(RemoteLeaseSuccess::Capacity {
                    grant,
                    ttl_grant_ms: rec.ttl_grant_ms,
                    grantor_pool_remaining: coordinator.capacity().pool_remaining(),
                    disposition: rec.disposition,
                })
            }
            RemoteLeaseCommand::GrantExclusiveDeviceLease {
                device_id,
                device_lease_id,
                ttl_grant_ms,
                multi_endpoint_gateways,
            } => {
                let scope = build_scope(multi_endpoint_gateways)?;
                let (grant, rec) = coordinator
                    .grant_device_cross_host(
                        fence,
                        *device_id,
                        *device_lease_id,
                        CrossHostGrantParams {
                            holder_cell: envelope.caller_cell,
                            holder_boot_generation: envelope.caller_boot_generation,
                            grantor_commit_mono: now_grantor_mono,
                            ttl_grant_ms: *ttl_grant_ms,
                            policy: self.default_skew_policy,
                            authority_expiry_tick: envelope.authority_expiry_tick,
                            multi_endpoint_scope: scope,
                        },
                    )
                    .map_err(map_horizon_err)?;
                let dev = coordinator.device(*device_id).ok_or_else(|| {
                    RemoteLeaseReject::CoordinatorRejected("unknown device head".to_owned())
                })?;
                Ok(RemoteLeaseSuccess::ExclusiveDevice {
                    grant,
                    ttl_grant_ms: rec.ttl_grant_ms,
                    head_is_free: dev.is_free(),
                    head_reset_generation: dev.reset_generation(),
                    disposition: rec.disposition,
                })
            }
            RemoteLeaseCommand::SubmitDeviceResetReceipt {
                device_id,
                reset_receipt,
            } => {
                let grant = coordinator
                    .submit_device_reset_for_holder(
                        fence,
                        *device_id,
                        envelope.caller_cell,
                        *reset_receipt,
                    )
                    .map_err(map_horizon_err)?;
                let rec = coordinator
                    .horizon_record(
                        CrossHostLeaseFamily::ExclusiveDevice,
                        *grant.device_lease_id().as_bytes(),
                    )
                    .cloned()
                    .ok_or_else(|| {
                        RemoteLeaseReject::CoordinatorRejected("missing device horizon".to_owned())
                    })?;
                let dev = coordinator.device(*device_id).ok_or_else(|| {
                    RemoteLeaseReject::CoordinatorRejected("unknown device head".to_owned())
                })?;
                Ok(RemoteLeaseSuccess::ExclusiveDevice {
                    grant,
                    ttl_grant_ms: rec.ttl_grant_ms,
                    head_is_free: dev.is_free(),
                    head_reset_generation: dev.reset_generation(),
                    disposition: rec.disposition,
                })
            }
            RemoteLeaseCommand::SubmitFenceBarrierAck { signed_ack } => {
                let coverage = coordinator
                    .record_signed_fence_barrier_ack(
                        signed_ack,
                        self.peer_verifier.as_ref(),
                        now_verify_ms,
                    )
                    .map_err(map_horizon_err)?;
                Ok(RemoteLeaseSuccess::BarrierAckRecorded { coverage })
            }
            RemoteLeaseCommand::ReconcileQuarantinedLease { payload } => match payload {
                RemoteReconcilePayload::QuotaViaSignedClose { close_receipt } => {
                    let grant = coordinator
                        .settle_quota_with_signed_close_receipt(
                            fence,
                            close_receipt,
                            self.peer_verifier.as_ref(),
                            now_verify_ms,
                        )
                        .map_err(map_horizon_err)?;
                    let rec = coordinator
                        .horizon_record(CrossHostLeaseFamily::Quota, *grant.lease_id().as_bytes())
                        .cloned()
                        .ok_or_else(|| {
                            RemoteLeaseReject::CoordinatorRejected(
                                "missing quota horizon".to_owned(),
                            )
                        })?;
                    Ok(RemoteLeaseSuccess::Quota {
                        grant,
                        ttl_grant_ms: rec.ttl_grant_ms,
                        renewal_seq: rec.renewal_seq,
                        grantor_available: coordinator.quota().available(),
                        disposition: rec.disposition,
                    })
                }
                RemoteReconcilePayload::QuotaViaBarrier { receipt } => {
                    let grant = coordinator
                        .reconcile_quarantined_quota_via_barrier(fence, *receipt)
                        .map_err(map_horizon_err)?;
                    let rec = coordinator
                        .horizon_record(CrossHostLeaseFamily::Quota, *grant.lease_id().as_bytes())
                        .cloned()
                        .ok_or_else(|| {
                            RemoteLeaseReject::CoordinatorRejected(
                                "missing quota horizon".to_owned(),
                            )
                        })?;
                    Ok(RemoteLeaseSuccess::Quota {
                        grant,
                        ttl_grant_ms: rec.ttl_grant_ms,
                        renewal_seq: rec.renewal_seq,
                        grantor_available: coordinator.quota().available(),
                        disposition: rec.disposition,
                    })
                }
                RemoteReconcilePayload::CapacityViaBarrier { receipt } => {
                    let grant = coordinator
                        .reconcile_quarantined_capacity_via_barrier(fence, *receipt)
                        .map_err(map_horizon_err)?;
                    let rec = coordinator
                        .horizon_record(
                            CrossHostLeaseFamily::Capacity,
                            *grant.capacity_lease_id().as_bytes(),
                        )
                        .cloned()
                        .ok_or_else(|| {
                            RemoteLeaseReject::CoordinatorRejected(
                                "missing capacity horizon".to_owned(),
                            )
                        })?;
                    Ok(RemoteLeaseSuccess::Capacity {
                        grant,
                        ttl_grant_ms: rec.ttl_grant_ms,
                        grantor_pool_remaining: coordinator.capacity().pool_remaining(),
                        disposition: rec.disposition,
                    })
                }
                RemoteReconcilePayload::DeviceViaBarrier { receipt } => {
                    let dev_id = receipt.device_id();
                    let grant = coordinator
                        .reconcile_quarantined_device_via_barrier(fence, *receipt)
                        .map_err(map_horizon_err)?;
                    let rec = coordinator
                        .horizon_record(
                            CrossHostLeaseFamily::ExclusiveDevice,
                            *grant.device_lease_id().as_bytes(),
                        )
                        .cloned()
                        .ok_or_else(|| {
                            RemoteLeaseReject::CoordinatorRejected(
                                "missing device horizon".to_owned(),
                            )
                        })?;
                    let dev = coordinator.device(dev_id).ok_or_else(|| {
                        RemoteLeaseReject::CoordinatorRejected("unknown device head".to_owned())
                    })?;
                    Ok(RemoteLeaseSuccess::ExclusiveDevice {
                        grant,
                        ttl_grant_ms: rec.ttl_grant_ms,
                        head_is_free: dev.is_free(),
                        head_reset_generation: dev.reset_generation(),
                        disposition: rec.disposition,
                    })
                }
            },
            RemoteLeaseCommand::QueryLeaseReconciliationState { .. } => {
                Err(RemoteLeaseReject::CoordinatorRejected(
                    "query handled in read-only path".to_owned(),
                ))
            }
        }
    }

    fn build_reconciliation_snapshot(
        guard: &ServerMutableState,
        family: CrossHostLeaseFamily,
        lease_id: [u8; 16],
        probed_key: Option<IdempotencyKey>,
    ) -> LeaseReconciliationSnapshot {
        let rec: Option<&GrantorHorizonRecord> = guard.coordinator.horizon_record(family, lease_id);
        let quota_grant = if family == CrossHostLeaseFamily::Quota {
            guard
                .coordinator
                .quota()
                .query(QuotaLeaseId::from_bytes(lease_id))
                .ok()
        } else {
            None
        };
        let capacity_grant = if family == CrossHostLeaseFamily::Capacity {
            guard
                .coordinator
                .capacity()
                .query(CapacityLeaseId::from_bytes(lease_id))
                .ok()
        } else {
            None
        };
        let device_grant = if family == CrossHostLeaseFamily::ExclusiveDevice {
            let d_id = ExclusiveDeviceLeaseId::from_bytes(lease_id);
            find_device_in_coordinator(&guard.coordinator, d_id)
        } else {
            None
        };
        let lease_known = rec.is_some()
            || quota_grant.is_some()
            || capacity_grant.is_some()
            || device_grant.is_some();
        let probed_idempotency_committed_seq = probed_key
            .and_then(|k| guard.idempotency_records.get(&k))
            .map(|r| r.durable_state_seq);

        LeaseReconciliationSnapshot {
            lease_known,
            quota_grant,
            capacity_grant,
            device_grant,
            disposition: rec.map(|r| r.disposition),
            renewal_seq: rec.map(|r| r.renewal_seq),
            barrier_coverage: guard
                .coordinator
                .fence_barrier_coverage(family, lease_id)
                .ok(),
            probed_idempotency_committed_seq,
        }
    }

    fn build_reply(
        &self,
        idempotency_key: IdempotencyKey,
        fence: CellFence,
        durable_state_seq: u64,
        replayed: bool,
        result: Result<RemoteLeaseSuccess, RemoteLeaseReject>,
        now_verify_ms: u64,
    ) -> RpcHandlerReply {
        let payload = RemoteLeaseResponseEnvelope::sign(
            idempotency_key,
            fence,
            durable_state_seq,
            replayed,
            result,
            self.local_signer.as_ref(),
            now_verify_ms,
        )
        .map_or_else(|_| Vec::new(), |env| env.to_wire_bytes());

        RpcHandlerReply {
            observed_epoch: fence.epoch(),
            idempotency_replayed: replayed,
            status: RpcWireStatus::Ok,
            payload,
        }
    }
}

fn find_device_in_coordinator(
    coordinator: &GrantorClockBoundedCoordinator,
    lease_id: ExclusiveDeviceLeaseId,
) -> Option<ExclusiveDeviceLeaseGrant> {
    // Coordinator exposes device(device_id); check via find helper on GrantorClockBoundedCoordinator
    coordinator.query_any_device_lease(lease_id)
}

fn map_horizon_err(err: ClockHorizonError) -> RemoteLeaseReject {
    match err {
        ClockHorizonError::HolderCellImpersonation {
            caller_cell,
            expected_holder_cell,
        } => RemoteLeaseReject::CrossCellImpersonation {
            caller_cell,
            expected_holder_cell,
        },
        ClockHorizonError::PeerSignatureRejected(e) => {
            RemoteLeaseReject::SignatureRejected(e.to_string())
        }
        ClockHorizonError::ClockViolation(v) => {
            RemoteLeaseReject::ClockSkewQuarantined(v.to_string())
        }
        other => RemoteLeaseReject::CoordinatorRejected(other.to_string()),
    }
}

impl GrantorClockBoundedCoordinator {
    pub(crate) fn query_any_device_lease(
        &self,
        lease_id: ExclusiveDeviceLeaseId,
    ) -> Option<ExclusiveDeviceLeaseGrant> {
        self.find_device_lease_opt(lease_id)
    }
}

impl RpcRequestHandler for RemoteLeaseProtocolServer {
    fn handle_rpc(
        &self,
        request: IncomingRpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcHandlerReply> + Send + 'static>> {
        let reply = self.process_incoming_sync(&request);
        Box::pin(async move { reply })
    }
}

/// Fail-closed client errors returned by [`RemoteLeaseProtocolClient`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteLeaseClientError {
    /// Pre-dispatch transport refusal (`SingleHostDesktop` or `BlindRetryBlocked`).
    DispatchRefused(TransportDispatchRefusal),
    /// Non-succeeded `RemoteCallOutcome` (e.g. `Unavailable`, `DeadlineExceededUncertain`).
    TransportOutcome(RemoteCallOutcome<RpcResponsePayload>),
    /// Local custody signing failed.
    LocalSigning(PeerIdentityError),
    /// Grantor response signature verification failed.
    SignatureVerification(PeerIdentityError),
    /// Grantor `CellIdentity` in response disagreed with target `grantor_cell`.
    GrantorIdentityMismatch {
        /// Expected nominal grantor `CellIdentity`.
        expected: CellIdentity,
        /// Actual `CellIdentity` on signed response.
        actual: CellIdentity,
    },
    /// Response payload was malformed or had unexpected variant.
    MalformedResponse(&'static str),
    /// Grantor explicitly rejected the `[LEASE-PROTO-001]` command.
    ServerRejected(RemoteLeaseReject),
    /// Client-side conservative clock-skew / shrunk-horizon error.
    Horizon(ClockHorizonError),
    /// Client-side holder lease state error.
    HolderAdmit(crate::clock_horizon::HolderLeaseAdmitError),
}

impl fmt::Display for RemoteLeaseClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DispatchRefused(e) => write!(formatter, "transport dispatch refused: {e}"),
            Self::TransportOutcome(o) => {
                write!(formatter, "non-succeeded transport outcome: {o:?}")
            }
            Self::LocalSigning(e) => write!(formatter, "holder custody signing failed: {e}"),
            Self::SignatureVerification(e) => {
                write!(formatter, "grantor signature verification failed: {e}")
            }
            Self::GrantorIdentityMismatch { expected, actual } => {
                write!(
                    formatter,
                    "grantor CellIdentity mismatch: expected {expected:?}, got {actual:?}"
                )
            }
            Self::MalformedResponse(reason) => write!(formatter, "malformed response: {reason}"),
            Self::ServerRejected(r) => write!(formatter, "grantor rejected command: {r:?}"),
            Self::Horizon(e) => write!(formatter, "holder clock horizon error: {e}"),
            Self::HolderAdmit(e) => write!(formatter, "holder lease error: {e}"),
        }
    }
}

impl Error for RemoteLeaseClientError {}

/// Holder / operator `[LEASE-PROTO-001]` client over [`CellRpcTransport`]
/// (`MtlsCellTransport`).
pub struct RemoteLeaseProtocolClient {
    caller_cell: CellIdentity,
    caller_boot_generation: Generation,
    grantor_cell: CellIdentity,
    transport: Arc<dyn CellRpcTransport>,
    policy: ClockSkewPolicy,
    holder_mono_clock: Arc<dyn Fn() -> crate::clock_horizon::MonotonicInstantMs + Send + Sync>,
    verify_clock_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    signer: Arc<dyn PeerDigestSigner>,
    peer_verifier: Arc<dyn IdentityPeerVerifier>,
}

impl fmt::Debug for RemoteLeaseProtocolClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteLeaseProtocolClient")
            .field("caller_cell", &self.caller_cell)
            .field("grantor_cell", &self.grantor_cell)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl RemoteLeaseProtocolClient {
    /// Constructs a `[LEASE-PROTO-001]` client bound to `caller_cell` and
    /// targeting nominal `grantor_cell` (`[DIST-NAME-001]`).
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        caller_cell: CellIdentity,
        caller_boot_generation: Generation,
        grantor_cell: CellIdentity,
        transport: Arc<dyn CellRpcTransport>,
        policy: ClockSkewPolicy,
        holder_mono_clock: Arc<dyn Fn() -> crate::clock_horizon::MonotonicInstantMs + Send + Sync>,
        verify_clock_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
        signer: Arc<dyn PeerDigestSigner>,
        peer_verifier: Arc<dyn IdentityPeerVerifier>,
    ) -> Self {
        Self {
            caller_cell,
            caller_boot_generation,
            grantor_cell,
            transport,
            policy,
            holder_mono_clock,
            verify_clock_ms,
            signer,
            peer_verifier,
        }
    }

    /// Signs and dispatches a `[LEASE-PROTO-001]` command, verifying the
    /// grantor's Ed25519 response signature and returning
    /// `(t_req_holder_mono, t_recv_holder_mono, RemoteLeaseResponseEnvelope)`.
    ///
    /// # Errors
    ///
    /// Returns [`RemoteLeaseClientError`] on pre-dispatch refusal, uncertain or
    /// unavailable socket outcome (`[DIST-FAIL-001]`), invalid grantor
    /// signature, or explicit server rejection.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_signed_command(
        &self,
        call_id: [u8; 16],
        idempotency_key: IdempotencyKey,
        expected_grantor_epoch: CellEpoch,
        authority_expiry_tick: u64,
        measured_rtt_hint_ms: Option<u64>,
        deadline_tick: u64,
        command: RemoteLeaseCommand,
    ) -> Result<
        (
            crate::clock_horizon::MonotonicInstantMs,
            crate::clock_horizon::MonotonicInstantMs,
            RemoteLeaseResponseEnvelope,
        ),
        RemoteLeaseClientError,
    > {
        let side_effecting = command.is_side_effecting();
        let now_wall = AuditWallTimeHint::new((self.verify_clock_ms)());
        let req_env = RemoteLeaseRequestEnvelope::sign(
            idempotency_key,
            self.caller_cell,
            self.caller_boot_generation,
            expected_grantor_epoch,
            authority_expiry_tick,
            measured_rtt_hint_ms,
            now_wall,
            command,
            self.signer.as_ref(),
        )
        .map_err(RemoteLeaseClientError::LocalSigning)?;

        let t_req_mono = (self.holder_mono_clock)();
        let resp_env = self
            .dispatch_raw_envelope(call_id, &req_env, deadline_tick, side_effecting)
            .await?;
        let t_recv_mono = (self.holder_mono_clock)();
        Ok((t_req_mono, t_recv_mono, resp_env))
    }

    /// Dispatches an already-signed [`RemoteLeaseRequestEnvelope`] (used both
    /// for initial calls and post-reconciliation idempotent replays).
    ///
    /// # Errors
    ///
    /// Fails closed on dispatch refusal, non-succeeded transport outcome,
    /// malformed response, grantor signature failure, or server rejection.
    pub async fn dispatch_raw_envelope(
        &self,
        call_id: [u8; 16],
        req_env: &RemoteLeaseRequestEnvelope,
        deadline_tick: u64,
        side_effecting: bool,
    ) -> Result<RemoteLeaseResponseEnvelope, RemoteLeaseClientError> {
        let contract = RemoteCallContract::new(
            call_id,
            req_env.idempotency_key,
            deadline_tick,
            3,
            0,
            side_effecting,
        );
        let outcome = self
            .transport
            .call_cell_rpc(self.grantor_cell, &contract, req_env.to_wire_bytes())
            .await
            .map_err(RemoteLeaseClientError::DispatchRefused)?;

        let RemoteCallOutcome::Succeeded { value, .. } = outcome else {
            return Err(RemoteLeaseClientError::TransportOutcome(outcome));
        };

        let resp_env = RemoteLeaseResponseEnvelope::from_wire_bytes(&value.payload)
            .map_err(RemoteLeaseClientError::MalformedResponse)?;
        if resp_env.idempotency_key != req_env.idempotency_key {
            return Err(RemoteLeaseClientError::MalformedResponse(
                "response idempotency_key mismatch",
            ));
        }
        resp_env.verify_grantor_signature(
            self.grantor_cell,
            self.peer_verifier.as_ref(),
            (self.verify_clock_ms)(),
        )?;
        if let Err(reject) = &resp_env.result {
            return Err(RemoteLeaseClientError::ServerRejected(reject.clone()));
        }
        Ok(resp_env)
    }

    /// Requests a remote `QuotaLease` and wraps the committed grant inside a
    /// [`HolderClockBoundedLease`] anchored at `t_req_holder_mono` with shrunk
    /// `ttl_holder = ttl_grant - 2 * max_clock_skew - safety_margin`.
    ///
    /// # Errors
    ///
    /// Fails closed on transport/signature error, server rejection, or RTT/skew
    /// envelope violation.
    #[allow(clippy::too_many_arguments)]
    pub async fn grant_quota_lease(
        &self,
        call_id: [u8; 16],
        idempotency_key: IdempotencyKey,
        expected_grantor_epoch: CellEpoch,
        authority_expiry_tick: u64,
        deadline_tick: u64,
        lease_id: QuotaLeaseId,
        face_value: u64,
        ttl_grant_ms: u64,
        multi_endpoint_gateways: Vec<[u8; 16]>,
    ) -> Result<(HolderClockBoundedLease, RemoteLeaseResponseEnvelope), RemoteLeaseClientError>
    {
        let (t_req, t_recv, resp) = self
            .send_signed_command(
                call_id,
                idempotency_key,
                expected_grantor_epoch,
                authority_expiry_tick,
                None,
                deadline_tick,
                RemoteLeaseCommand::GrantQuotaLease {
                    lease_id,
                    face_value,
                    ttl_grant_ms,
                    multi_endpoint_gateways,
                },
            )
            .await?;
        let Ok(RemoteLeaseSuccess::Quota {
            grant,
            ttl_grant_ms: confirmed_ttl,
            ..
        }) = &resp.result
        else {
            return Err(RemoteLeaseClientError::MalformedResponse(
                "expected Quota success payload",
            ));
        };
        let holder =
            HolderClockBoundedLease::new_quota(*grant, self.policy, t_req, t_recv, *confirmed_ttl)
                .map_err(RemoteLeaseClientError::Horizon)?;
        Ok((holder, resp))
    }

    /// Renews `holder_lease` (`[LEASE-RENEW-001]`): extends `ttl_grant_ms` and
    /// re-anchors `t_req_holder_mono` while verifying `face_value` is unchanged.
    ///
    /// # Errors
    ///
    /// Fails closed on transport/signature error, server rejection, or RTT/skew
    /// violation.
    #[allow(clippy::too_many_arguments)]
    pub async fn renew_quota_lease(
        &self,
        holder_lease: &mut HolderClockBoundedLease,
        call_id: [u8; 16],
        idempotency_key: IdempotencyKey,
        expected_grantor_epoch: CellEpoch,
        authority_expiry_tick: u64,
        deadline_tick: u64,
        lease_id: QuotaLeaseId,
        new_ttl_grant_ms: u64,
    ) -> Result<RemoteLeaseResponseEnvelope, RemoteLeaseClientError> {
        let (t_req, t_recv, resp) = self
            .send_signed_command(
                call_id,
                idempotency_key,
                expected_grantor_epoch,
                authority_expiry_tick,
                None,
                deadline_tick,
                RemoteLeaseCommand::RenewQuotaLease {
                    lease_id,
                    new_ttl_grant_ms,
                },
            )
            .await?;
        let Ok(RemoteLeaseSuccess::Quota {
            grant,
            ttl_grant_ms: confirmed_ttl,
            renewal_seq,
            ..
        }) = &resp.result
        else {
            return Err(RemoteLeaseClientError::MalformedResponse(
                "expected Quota renewal payload",
            ));
        };
        holder_lease
            .apply_verified_renewal(
                t_req,
                t_recv,
                *confirmed_ttl,
                *renewal_seq,
                grant.face_value(),
            )
            .map_err(RemoteLeaseClientError::HolderAdmit)?;
        Ok(resp)
    }

    /// Signs a [`SignedLeaseCloseReceipt`] from `holder_lease` and closes the
    /// remote `QuotaLease` on the grantor.
    ///
    /// # Errors
    ///
    /// Fails closed on holder clock rollback, signing failure, transport error,
    /// or server rejection.
    #[allow(clippy::too_many_arguments)]
    pub async fn close_quota_lease(
        &self,
        holder_lease: &mut HolderClockBoundedLease,
        receipt_id: ReceiptId,
        call_id: [u8; 16],
        idempotency_key: IdempotencyKey,
        expected_grantor_epoch: CellEpoch,
        authority_expiry_tick: u64,
        deadline_tick: u64,
    ) -> Result<(SignedLeaseCloseReceipt, RemoteLeaseResponseEnvelope), RemoteLeaseClientError>
    {
        let now_mono = (self.holder_mono_clock)();
        let wall_hint = AuditWallTimeHint::new((self.verify_clock_ms)());
        let receipt = holder_lease
            .sign_quota_close_receipt(now_mono, receipt_id, self.signer.as_ref(), wall_hint)
            .map_err(RemoteLeaseClientError::HolderAdmit)?;
        let (_, _, resp) = self
            .send_signed_command(
                call_id,
                idempotency_key,
                expected_grantor_epoch,
                authority_expiry_tick,
                None,
                deadline_tick,
                RemoteLeaseCommand::CloseQuotaLease {
                    close_receipt: receipt,
                },
            )
            .await?;
        Ok((receipt, resp))
    }

    /// Grants a remote `CapacityLease` and wraps it in a [`HolderClockBoundedLease`].
    ///
    /// # Errors
    ///
    /// Fails closed on transport/signature error, server rejection, or skew violation.
    #[allow(clippy::too_many_arguments)]
    pub async fn grant_capacity_lease(
        &self,
        call_id: [u8; 16],
        idempotency_key: IdempotencyKey,
        expected_grantor_epoch: CellEpoch,
        authority_expiry_tick: u64,
        deadline_tick: u64,
        capacity_lease_id: CapacityLeaseId,
        amount: u64,
        ttl_grant_ms: u64,
        multi_endpoint_gateways: Vec<[u8; 16]>,
    ) -> Result<(HolderClockBoundedLease, RemoteLeaseResponseEnvelope), RemoteLeaseClientError>
    {
        let (t_req, t_recv, resp) = self
            .send_signed_command(
                call_id,
                idempotency_key,
                expected_grantor_epoch,
                authority_expiry_tick,
                None,
                deadline_tick,
                RemoteLeaseCommand::GrantCapacityLease {
                    capacity_lease_id,
                    amount,
                    ttl_grant_ms,
                    multi_endpoint_gateways,
                },
            )
            .await?;
        let Ok(RemoteLeaseSuccess::Capacity {
            grant,
            ttl_grant_ms: confirmed_ttl,
            ..
        }) = &resp.result
        else {
            return Err(RemoteLeaseClientError::MalformedResponse(
                "expected Capacity success payload",
            ));
        };
        let holder = HolderClockBoundedLease::new_capacity(
            *grant,
            self.policy,
            t_req,
            t_recv,
            *confirmed_ttl,
        )
        .map_err(RemoteLeaseClientError::Horizon)?;
        Ok((holder, resp))
    }

    /// Grants a remote `ExclusiveDeviceLease` and wraps it in a [`HolderClockBoundedLease`].
    ///
    /// # Errors
    ///
    /// Fails closed on transport/signature error, server rejection, or skew violation.
    #[allow(clippy::too_many_arguments)]
    pub async fn grant_exclusive_device_lease(
        &self,
        call_id: [u8; 16],
        idempotency_key: IdempotencyKey,
        expected_grantor_epoch: CellEpoch,
        authority_expiry_tick: u64,
        deadline_tick: u64,
        device_id: DeviceId,
        device_lease_id: ExclusiveDeviceLeaseId,
        ttl_grant_ms: u64,
        multi_endpoint_gateways: Vec<[u8; 16]>,
    ) -> Result<(HolderClockBoundedLease, RemoteLeaseResponseEnvelope), RemoteLeaseClientError>
    {
        let (t_req, t_recv, resp) = self
            .send_signed_command(
                call_id,
                idempotency_key,
                expected_grantor_epoch,
                authority_expiry_tick,
                None,
                deadline_tick,
                RemoteLeaseCommand::GrantExclusiveDeviceLease {
                    device_id,
                    device_lease_id,
                    ttl_grant_ms,
                    multi_endpoint_gateways,
                },
            )
            .await?;
        let Ok(RemoteLeaseSuccess::ExclusiveDevice {
            grant,
            ttl_grant_ms: confirmed_ttl,
            ..
        }) = &resp.result
        else {
            return Err(RemoteLeaseClientError::MalformedResponse(
                "expected ExclusiveDevice success payload",
            ));
        };
        let holder = HolderClockBoundedLease::new_exclusive_device(
            *grant,
            self.policy,
            t_req,
            t_recv,
            *confirmed_ttl,
        )
        .map_err(RemoteLeaseClientError::Horizon)?;
        Ok((holder, resp))
    }

    /// Queries authoritative lease & idempotency state (`side_effecting = false`).
    ///
    /// # Errors
    ///
    /// Fails closed on transport or signature verification error.
    #[allow(clippy::too_many_arguments)]
    pub async fn query_reconciliation_state(
        &self,
        query_call_id: [u8; 16],
        query_idempotency_key: IdempotencyKey,
        expected_grantor_epoch: CellEpoch,
        authority_expiry_tick: u64,
        deadline_tick: u64,
        family: CrossHostLeaseFamily,
        lease_id: [u8; 16],
        probed_idempotency_key: Option<IdempotencyKey>,
    ) -> Result<LeaseReconciliationSnapshot, RemoteLeaseClientError> {
        let (_, _, resp) = self
            .send_signed_command(
                query_call_id,
                query_idempotency_key,
                expected_grantor_epoch,
                authority_expiry_tick,
                None,
                deadline_tick,
                RemoteLeaseCommand::QueryLeaseReconciliationState {
                    family,
                    lease_id,
                    probed_idempotency_key,
                },
            )
            .await?;
        let Ok(RemoteLeaseSuccess::ReconciliationSnapshot(snapshot)) = resp.result else {
            return Err(RemoteLeaseClientError::MalformedResponse(
                "expected ReconciliationSnapshot",
            ));
        };
        Ok(*snapshot)
    }

    /// Resolves a `DeadlineExceededUncertain` (`ReconcileBeforeRetry`) outcome
    /// (`[DIST-FAIL-001]`):
    /// 1. Queries authoritative grantor state (`QueryLeaseReconciliationState`,
    ///    `side_effecting = false`) to inspect whether `original_envelope.idempotency_key`
    ///    committed before the socket drop;
    /// 2. Marks `original_envelope.idempotency_key` reconciled in
    ///    `mtls_transport.reconciliation_tracker()`;
    /// 3. Replays `original_envelope` with the exact same `IdempotencyKey` to
    ///    obtain the single-execution committed response with zero double-deduction.
    ///
    /// # Errors
    ///
    /// Fails closed if either the authoritative query or idempotent replay fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn reconcile_and_replay_uncertain(
        &self,
        mtls_transport: &MtlsCellTransport,
        family: CrossHostLeaseFamily,
        lease_id: [u8; 16],
        query_call_id: [u8; 16],
        query_idempotency_key: IdempotencyKey,
        replay_call_id: [u8; 16],
        original_envelope: &RemoteLeaseRequestEnvelope,
        deadline_tick: u64,
    ) -> Result<(LeaseReconciliationSnapshot, RemoteLeaseResponseEnvelope), RemoteLeaseClientError>
    {
        let snapshot = self
            .query_reconciliation_state(
                query_call_id,
                query_idempotency_key,
                original_envelope.expected_grantor_epoch,
                original_envelope.authority_expiry_tick,
                deadline_tick,
                family,
                lease_id,
                Some(original_envelope.idempotency_key),
            )
            .await?;

        let _ = mtls_transport
            .reconciliation_tracker()
            .mark_reconciled(original_envelope.idempotency_key);

        let replayed_resp = self
            .dispatch_raw_envelope(
                replay_call_id,
                original_envelope,
                deadline_tick,
                original_envelope.command.is_side_effecting(),
            )
            .await?;

        Ok((snapshot, replayed_resp))
    }
}

// ============================================================================
// Deterministic Exact Binary Wire Codec (Panic-Free, Zero Ambiguity)
// ============================================================================

fn put_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}

fn put_bool(out: &mut Vec<u8>, v: bool) {
    out.push(u8::from(v));
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    put_u32(out, len);
    out.extend_from_slice(&bytes[..len as usize]);
}

fn encode_gateways_vec(out: &mut Vec<u8>, gateways: &[[u8; 16]]) {
    let count = u32::try_from(gateways.len()).unwrap_or(u32::MAX);
    put_u32(out, count);
    for gw in gateways.iter().take(count as usize) {
        out.extend_from_slice(gw);
    }
}

fn encode_close_receipt(out: &mut Vec<u8>, r: &SignedLeaseCloseReceipt) {
    out.extend_from_slice(r.receipt_id.as_bytes());
    out.extend_from_slice(r.lease_id.as_bytes());
    out.extend_from_slice(r.holder_cell.as_bytes());
    put_u64(out, r.holder_boot_generation.get());
    put_u64(out, r.epoch.get());
    put_u64(out, r.fencing_token.get());
    put_u64(out, r.face_value);
    put_u64(out, r.final_high_water_spent);
    put_u64(out, r.returned_unspent);
    out.extend_from_slice(r.signer_principal_id.as_bytes());
    out.extend_from_slice(r.signer_key_id.as_bytes());
    put_u64(out, r.signer_key_generation.get());
    out.extend_from_slice(&r.ed25519_signature);
}

fn encode_attach_receipt(out: &mut Vec<u8>, r: &HostAttachReceipt) {
    out.extend_from_slice(r.receipt_id().as_bytes());
    out.extend_from_slice(r.capacity_lease_id().as_bytes());
    out.extend_from_slice(r.target_node().as_bytes());
    put_u64(out, r.target_node_boot_generation().get());
    put_u64(out, r.capacity_epoch().get());
    put_u64(out, r.fencing_token().get());
    put_u64(out, r.attached_amount());
}

fn encode_device_reset_receipt(out: &mut Vec<u8>, r: &DeviceResetReceipt) {
    out.extend_from_slice(r.receipt_id().as_bytes());
    out.extend_from_slice(r.device_lease_id().as_bytes());
    out.extend_from_slice(r.holder_node().as_bytes());
    put_u64(out, r.holder_node_boot_generation().get());
    put_u64(out, r.exclusivity_epoch().get());
    put_u64(out, r.fencing_token().get());
    put_u64(out, r.reset_generation().get());
}

fn encode_fence_ack(out: &mut Vec<u8>, ack: &FenceBarrierAck) {
    out.extend_from_slice(&ack.lease_id());
    out.extend_from_slice(ack.fence_scope().as_cell().as_bytes());
    put_u64(out, ack.epoch().get());
    put_u64(out, ack.fencing_token().get());
    out.extend_from_slice(&ack.gateway_id());
    put_u64(out, ack.observed_high_water());
    out.extend_from_slice(&ack.ack_digest());
}

fn encode_signed_fence_ack(out: &mut Vec<u8>, s: &SignedFenceBarrierAck) {
    put_u8(out, s.family.wire_tag());
    encode_fence_ack(out, &s.ack);
    out.extend_from_slice(s.signer_principal_id.as_bytes());
    out.extend_from_slice(s.signer_key_id.as_bytes());
    put_u64(out, s.signer_key_generation.get());
    out.extend_from_slice(&s.ed25519_signature);
}

fn encode_reconcile_payload(out: &mut Vec<u8>, p: &RemoteReconcilePayload) {
    match p {
        RemoteReconcilePayload::QuotaViaSignedClose { close_receipt } => {
            put_u8(out, 1);
            encode_close_receipt(out, close_receipt);
        }
        RemoteReconcilePayload::QuotaViaBarrier { receipt } => {
            put_u8(out, 2);
            out.extend_from_slice(receipt.receipt_id().as_bytes());
            out.extend_from_slice(receipt.lease_id().as_bytes());
            put_u64(out, receipt.reconcile_seq());
            out.extend_from_slice(receipt.cell().as_bytes());
            put_u64(out, receipt.node_boot_generation().get());
            put_u64(out, receipt.epoch().get());
            put_u64(out, receipt.fencing_token().get());
            put_u64(out, receipt.face_value());
            put_u64(out, receipt.high_water_spent());
            put_u64(out, receipt.settled_spent());
            put_u64(out, receipt.returned_to_available());
            put_u64(out, receipt.remaining_quarantined());
            encode_fence_ack(out, &receipt.fence_barrier_ack());
        }
        RemoteReconcilePayload::CapacityViaBarrier { receipt } => {
            put_u8(out, 3);
            out.extend_from_slice(receipt.receipt_id().as_bytes());
            out.extend_from_slice(receipt.capacity_lease_id().as_bytes());
            out.extend_from_slice(receipt.target_node().as_bytes());
            put_u64(out, receipt.target_node_boot_generation().get());
            put_u64(out, receipt.capacity_epoch().get());
            put_u64(out, receipt.fencing_token().get());
            put_u64(out, receipt.amount());
            let att = receipt.cleanup_attestation();
            out.extend_from_slice(att.capacity_lease_id().as_bytes());
            out.extend_from_slice(att.target_node().as_bytes());
            put_u64(out, att.target_node_boot_generation().get());
            put_u64(out, att.capacity_epoch().get());
            put_u64(out, att.fencing_token().get());
            put_u64(out, att.cleaned_amount());
            put_bool(out, att.holder_processes_terminated());
            out.extend_from_slice(&att.attestation_digest());
            encode_fence_ack(out, &receipt.fence_barrier_ack());
        }
        RemoteReconcilePayload::DeviceViaBarrier { receipt } => {
            put_u8(out, 4);
            out.extend_from_slice(receipt.receipt_id().as_bytes());
            out.extend_from_slice(receipt.device_lease_id().as_bytes());
            out.extend_from_slice(receipt.device_id().as_bytes());
            out.extend_from_slice(receipt.holder_node().as_bytes());
            put_u64(out, receipt.holder_node_boot_generation().get());
            put_u64(out, receipt.exclusivity_epoch().get());
            put_u64(out, receipt.fencing_token().get());
            put_u64(out, receipt.reset_generation().get());
            put_bool(out, receipt.endpoint_fenced_old_token());
            put_bool(out, receipt.queue_drained());
            put_bool(out, receipt.vram_and_secrets_zeroized());
            encode_device_reset_receipt(out, &receipt.reset_receipt());
            encode_fence_ack(out, &receipt.fence_barrier_ack());
        }
    }
}

fn encode_command(out: &mut Vec<u8>, cmd: &RemoteLeaseCommand) {
    match cmd {
        RemoteLeaseCommand::GrantQuotaLease {
            lease_id,
            face_value,
            ttl_grant_ms,
            multi_endpoint_gateways,
        } => {
            put_u8(out, 1);
            out.extend_from_slice(lease_id.as_bytes());
            put_u64(out, *face_value);
            put_u64(out, *ttl_grant_ms);
            encode_gateways_vec(out, multi_endpoint_gateways);
        }
        RemoteLeaseCommand::RenewQuotaLease {
            lease_id,
            new_ttl_grant_ms,
        } => {
            put_u8(out, 2);
            out.extend_from_slice(lease_id.as_bytes());
            put_u64(out, *new_ttl_grant_ms);
        }
        RemoteLeaseCommand::ReportQuotaHighWater {
            lease_id,
            high_water_spent,
        } => {
            put_u8(out, 3);
            out.extend_from_slice(lease_id.as_bytes());
            put_u64(out, *high_water_spent);
        }
        RemoteLeaseCommand::CloseQuotaLease { close_receipt } => {
            put_u8(out, 4);
            encode_close_receipt(out, close_receipt);
        }
        RemoteLeaseCommand::GrantCapacityLease {
            capacity_lease_id,
            amount,
            ttl_grant_ms,
            multi_endpoint_gateways,
        } => {
            put_u8(out, 5);
            out.extend_from_slice(capacity_lease_id.as_bytes());
            put_u64(out, *amount);
            put_u64(out, *ttl_grant_ms);
            encode_gateways_vec(out, multi_endpoint_gateways);
        }
        RemoteLeaseCommand::AttachCapacityHost { attach_receipt } => {
            put_u8(out, 6);
            encode_attach_receipt(out, attach_receipt);
        }
        RemoteLeaseCommand::ReturnCapacityLease { capacity_lease_id } => {
            put_u8(out, 7);
            out.extend_from_slice(capacity_lease_id.as_bytes());
        }
        RemoteLeaseCommand::GrantExclusiveDeviceLease {
            device_id,
            device_lease_id,
            ttl_grant_ms,
            multi_endpoint_gateways,
        } => {
            put_u8(out, 8);
            out.extend_from_slice(device_id.as_bytes());
            out.extend_from_slice(device_lease_id.as_bytes());
            put_u64(out, *ttl_grant_ms);
            encode_gateways_vec(out, multi_endpoint_gateways);
        }
        RemoteLeaseCommand::SubmitDeviceResetReceipt {
            device_id,
            reset_receipt,
        } => {
            put_u8(out, 9);
            out.extend_from_slice(device_id.as_bytes());
            encode_device_reset_receipt(out, reset_receipt);
        }
        RemoteLeaseCommand::SubmitFenceBarrierAck { signed_ack } => {
            put_u8(out, 10);
            encode_signed_fence_ack(out, signed_ack);
        }
        RemoteLeaseCommand::ReconcileQuarantinedLease { payload } => {
            put_u8(out, 11);
            encode_reconcile_payload(out, payload);
        }
        RemoteLeaseCommand::QueryLeaseReconciliationState {
            family,
            lease_id,
            probed_idempotency_key,
        } => {
            put_u8(out, 12);
            put_u8(out, family.wire_tag());
            out.extend_from_slice(lease_id);
            match probed_idempotency_key {
                Some(k) => {
                    put_u8(out, 1);
                    out.extend_from_slice(k.as_bytes());
                }
                None => put_u8(out, 0),
            }
        }
    }
}

fn encode_disposition(out: &mut Vec<u8>, d: QuarantineDisposition) {
    match d {
        QuarantineDisposition::ActiveOrSettled => put_u8(out, 0),
        QuarantineDisposition::LedgerQuarantinedFenceOnly => put_u8(out, 1),
        QuarantineDisposition::PhysicallyCleanedVerified {
            proof_kind,
            proof_digest,
        } => {
            put_u8(out, 2);
            put_u8(out, proof_kind as u8);
            out.extend_from_slice(&proof_digest);
        }
    }
}

fn encode_quota_grant(out: &mut Vec<u8>, g: &QuotaLeaseGrant) {
    out.extend_from_slice(g.lease_id().as_bytes());
    out.extend_from_slice(g.cell().as_bytes());
    put_u64(out, g.node_boot_generation().get());
    put_u64(out, g.face_value());
    put_u64(out, g.remaining());
    put_u64(out, g.spent());
    put_u64(out, g.returned());
    put_u64(out, g.epoch().get());
    put_u64(out, g.fencing_token().get());
    out.extend_from_slice(g.fence_scope().as_cell().as_bytes());
    match g.expires_at() {
        Some(inst) => {
            put_u8(out, 1);
            put_u64(out, inst.get());
        }
        None => put_u8(out, 0),
    }
    let st = match g.state() {
        QuotaLeaseState::Issued => 1,
        QuotaLeaseState::Active => 2,
        QuotaLeaseState::Closing => 3,
        QuotaLeaseState::Closed => 4,
        QuotaLeaseState::Cancelled => 5,
        QuotaLeaseState::Fenced => 6,
        QuotaLeaseState::Quarantined => 7,
    };
    put_u8(out, st);
}

fn encode_capacity_grant(out: &mut Vec<u8>, g: &CapacityLeaseGrant) {
    out.extend_from_slice(g.capacity_lease_id().as_bytes());
    out.extend_from_slice(g.target_node().as_bytes());
    put_u64(out, g.target_node_boot_generation().get());
    put_u64(out, g.amount());
    put_u64(out, g.capacity_epoch().get());
    put_u64(out, g.fencing_token().get());
    out.extend_from_slice(g.fence_scope().as_cell().as_bytes());
    let st = match g.state() {
        CapacityLeaseState::GlobalReserved => 1,
        CapacityLeaseState::TargetPrepared => 2,
        CapacityLeaseState::Active => 3,
        CapacityLeaseState::Returning => 4,
        CapacityLeaseState::Fenced => 5,
        CapacityLeaseState::Quarantined => 6,
        CapacityLeaseState::Returned => 7,
    };
    put_u8(out, st);
}

fn encode_device_grant(out: &mut Vec<u8>, g: &ExclusiveDeviceLeaseGrant) {
    out.extend_from_slice(g.device_lease_id().as_bytes());
    out.extend_from_slice(g.device_id().as_bytes());
    out.extend_from_slice(g.holder_node().as_bytes());
    put_u64(out, g.holder_node_boot_generation().get());
    put_u64(out, g.exclusivity_epoch().get());
    put_u64(out, g.fencing_token().get());
    out.extend_from_slice(g.fence_scope().as_cell().as_bytes());
    put_u64(out, g.reset_generation().get());
    let st = match g.state() {
        ExclusiveDeviceLeaseState::DeviceReserved => 1,
        ExclusiveDeviceLeaseState::Resetting => 2,
        ExclusiveDeviceLeaseState::Fencing => 3,
        ExclusiveDeviceLeaseState::Quarantined => 4,
        ExclusiveDeviceLeaseState::Returned => 5,
    };
    put_u8(out, st);
}

fn encode_coverage(out: &mut Vec<u8>, c: &FenceScopeCoverageStatus) {
    put_u32(out, u32::try_from(c.acked_count).unwrap_or(u32::MAX));
    put_u32(out, u32::try_from(c.required_count).unwrap_or(u32::MAX));
    encode_gateways_vec(out, &c.missing_gateways);
}

#[allow(clippy::too_many_lines)]
fn encode_result(out: &mut Vec<u8>, res: &Result<RemoteLeaseSuccess, RemoteLeaseReject>) {
    match res {
        Ok(success) => {
            put_u8(out, 1);
            match success {
                RemoteLeaseSuccess::Quota {
                    grant,
                    ttl_grant_ms,
                    renewal_seq,
                    grantor_available,
                    disposition,
                } => {
                    put_u8(out, 1);
                    encode_quota_grant(out, grant);
                    put_u64(out, *ttl_grant_ms);
                    put_u64(out, *renewal_seq);
                    put_u64(out, *grantor_available);
                    encode_disposition(out, *disposition);
                }
                RemoteLeaseSuccess::Capacity {
                    grant,
                    ttl_grant_ms,
                    grantor_pool_remaining,
                    disposition,
                } => {
                    put_u8(out, 2);
                    encode_capacity_grant(out, grant);
                    put_u64(out, *ttl_grant_ms);
                    put_u64(out, *grantor_pool_remaining);
                    encode_disposition(out, *disposition);
                }
                RemoteLeaseSuccess::ExclusiveDevice {
                    grant,
                    ttl_grant_ms,
                    head_is_free,
                    head_reset_generation,
                    disposition,
                } => {
                    put_u8(out, 3);
                    encode_device_grant(out, grant);
                    put_u64(out, *ttl_grant_ms);
                    put_bool(out, *head_is_free);
                    put_u64(out, head_reset_generation.get());
                    encode_disposition(out, *disposition);
                }
                RemoteLeaseSuccess::BarrierAckRecorded { coverage } => {
                    put_u8(out, 4);
                    encode_coverage(out, coverage);
                }
                RemoteLeaseSuccess::ReconciliationSnapshot(s) => {
                    put_u8(out, 5);
                    put_bool(out, s.lease_known);
                    match &s.quota_grant {
                        Some(g) => {
                            put_u8(out, 1);
                            encode_quota_grant(out, g);
                        }
                        None => put_u8(out, 0),
                    }
                    match &s.capacity_grant {
                        Some(g) => {
                            put_u8(out, 1);
                            encode_capacity_grant(out, g);
                        }
                        None => put_u8(out, 0),
                    }
                    match &s.device_grant {
                        Some(g) => {
                            put_u8(out, 1);
                            encode_device_grant(out, g);
                        }
                        None => put_u8(out, 0),
                    }
                    match s.disposition {
                        Some(d) => {
                            put_u8(out, 1);
                            encode_disposition(out, d);
                        }
                        None => put_u8(out, 0),
                    }
                    match s.renewal_seq {
                        Some(seq) => {
                            put_u8(out, 1);
                            put_u64(out, seq);
                        }
                        None => put_u8(out, 0),
                    }
                    match &s.barrier_coverage {
                        Some(c) => {
                            put_u8(out, 1);
                            encode_coverage(out, c);
                        }
                        None => put_u8(out, 0),
                    }
                    match s.probed_idempotency_committed_seq {
                        Some(seq) => {
                            put_u8(out, 1);
                            put_u64(out, seq);
                        }
                        None => put_u8(out, 0),
                    }
                }
            }
        }
        Err(reject) => {
            put_u8(out, 0);
            match reject {
                RemoteLeaseReject::MalformedWire(s) => {
                    put_u8(out, 1);
                    put_str(out, s);
                }
                RemoteLeaseReject::PeerSessionIdentityMismatch(s) => {
                    put_u8(out, 2);
                    put_str(out, s);
                }
                RemoteLeaseReject::CrossCellImpersonation {
                    caller_cell,
                    expected_holder_cell,
                } => {
                    put_u8(out, 3);
                    out.extend_from_slice(caller_cell.as_bytes());
                    out.extend_from_slice(expected_holder_cell.as_bytes());
                }
                RemoteLeaseReject::SignatureRejected(s) => {
                    put_u8(out, 4);
                    put_str(out, s);
                }
                RemoteLeaseReject::IdempotencyConflict => put_u8(out, 5),
                RemoteLeaseReject::StaleOrMismatchedEpoch { expected, actual } => {
                    put_u8(out, 6);
                    put_u64(out, expected.get());
                    put_u64(out, actual.get());
                }
                RemoteLeaseReject::AuthorityTickExpired {
                    expiry_tick,
                    current_tick,
                } => {
                    put_u8(out, 7);
                    put_u64(out, *expiry_tick);
                    put_u64(out, *current_tick);
                }
                RemoteLeaseReject::ClockSkewQuarantined(s) => {
                    put_u8(out, 8);
                    put_str(out, s);
                }
                RemoteLeaseReject::CoordinatorRejected(s) => {
                    put_u8(out, 9);
                    put_str(out, s);
                }
            }
        }
    }
}

struct ByteReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], &'static str> {
        let end = self.pos.checked_add(len).ok_or("wire length overflow")?;
        if end > self.buf.len() {
            return Err("truncated wire payload");
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.take(1)?[0])
    }

    fn read_bool(&mut self) -> Result<bool, &'static str> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("invalid wire bool"),
        }
    }

    fn read_u16(&mut self) -> Result<u16, &'static str> {
        let b: [u8; 2] = self.take(2)?.try_into().map_err(|_| "u16 slice")?;
        Ok(u16::from_be_bytes(b))
    }

    fn read_u32(&mut self) -> Result<u32, &'static str> {
        let b: [u8; 4] = self.take(4)?.try_into().map_err(|_| "u32 slice")?;
        Ok(u32::from_be_bytes(b))
    }

    fn read_u64(&mut self) -> Result<u64, &'static str> {
        let b: [u8; 8] = self.take(8)?.try_into().map_err(|_| "u64 slice")?;
        Ok(u64::from_be_bytes(b))
    }

    fn read_16(&mut self) -> Result<[u8; 16], &'static str> {
        self.take(16)?.try_into().map_err(|_| "16B slice")
    }

    fn read_32(&mut self) -> Result<[u8; 32], &'static str> {
        self.take(32)?.try_into().map_err(|_| "32B slice")
    }

    fn read_64(&mut self) -> Result<[u8; 64], &'static str> {
        self.take(64)?.try_into().map_err(|_| "64B slice")
    }

    fn read_str(&mut self) -> Result<String, &'static str> {
        let len = usize::try_from(self.read_u32()?).map_err(|_| "str len")?;
        if len > 65_536 {
            return Err("wire string exceeds 64 KiB cap");
        }
        let raw = self.take(len)?;
        std::str::from_utf8(raw)
            .map(str::to_owned)
            .map_err(|_| "invalid UTF-8 in wire string")
    }

    const fn finish(&self) -> Result<(), &'static str> {
        if self.pos != self.buf.len() {
            return Err("trailing bytes in wire envelope");
        }
        Ok(())
    }
}

fn read_cell(r: &mut ByteReader<'_>) -> Result<CellIdentity, &'static str> {
    let raw = r.read_16()?;
    if raw == [0u8; 16] {
        return Err("zero CellIdentity in wire payload");
    }
    Ok(CellIdentity::from_domain(SchedulerDomainId::from_bytes(
        raw,
    )))
}

fn read_gen(r: &mut ByteReader<'_>) -> Result<Generation, &'static str> {
    NonZeroU64::new(r.read_u64()?)
        .map(Generation::new)
        .ok_or("zero Generation in wire payload")
}

fn read_epoch(r: &mut ByteReader<'_>) -> Result<CellEpoch, &'static str> {
    CellEpoch::from_u64(r.read_u64()?).ok_or("zero CellEpoch in wire payload")
}

fn read_token(r: &mut ByteReader<'_>) -> Result<CellFencingToken, &'static str> {
    CellFencingToken::from_u64(r.read_u64()?).ok_or("zero CellFencingToken in wire payload")
}

fn decode_gateways_vec(r: &mut ByteReader<'_>) -> Result<Vec<[u8; 16]>, &'static str> {
    let count = usize::try_from(r.read_u32()?).map_err(|_| "gateway count")?;
    if count > 1024 {
        return Err("gateway count exceeds 1024 cap");
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(r.read_16()?);
    }
    Ok(out)
}

fn decode_close_receipt(r: &mut ByteReader<'_>) -> Result<SignedLeaseCloseReceipt, &'static str> {
    Ok(SignedLeaseCloseReceipt {
        receipt_id: ReceiptId::from_bytes(r.read_16()?),
        lease_id: QuotaLeaseId::from_bytes(r.read_16()?),
        holder_cell: read_cell(r)?,
        holder_boot_generation: read_gen(r)?,
        epoch: read_epoch(r)?,
        fencing_token: read_token(r)?,
        face_value: r.read_u64()?,
        final_high_water_spent: r.read_u64()?,
        returned_unspent: r.read_u64()?,
        signer_principal_id: PrincipalId::from_bytes(r.read_16()?),
        signer_key_id: KeyId::from_bytes(r.read_16()?),
        signer_key_generation: read_gen(r)?,
        ed25519_signature: r.read_64()?,
    })
}

fn decode_attach_receipt(r: &mut ByteReader<'_>) -> Result<HostAttachReceipt, &'static str> {
    Ok(HostAttachReceipt::new(
        ReceiptId::from_bytes(r.read_16()?),
        CapacityLeaseId::from_bytes(r.read_16()?),
        read_cell(r)?,
        read_gen(r)?,
        read_epoch(r)?,
        read_token(r)?,
        r.read_u64()?,
    ))
}

fn decode_device_reset_receipt(r: &mut ByteReader<'_>) -> Result<DeviceResetReceipt, &'static str> {
    Ok(DeviceResetReceipt::new(
        ReceiptId::from_bytes(r.read_16()?),
        ExclusiveDeviceLeaseId::from_bytes(r.read_16()?),
        read_cell(r)?,
        read_gen(r)?,
        read_epoch(r)?,
        read_token(r)?,
        read_gen(r)?,
    ))
}

fn decode_fence_ack(r: &mut ByteReader<'_>) -> Result<FenceBarrierAck, &'static str> {
    Ok(FenceBarrierAck::new(
        r.read_16()?,
        FenceScope::cell(read_cell(r)?),
        read_epoch(r)?,
        read_token(r)?,
        r.read_16()?,
        r.read_u64()?,
        r.read_32()?,
    ))
}

fn decode_signed_fence_ack(r: &mut ByteReader<'_>) -> Result<SignedFenceBarrierAck, &'static str> {
    let family = CrossHostLeaseFamily::from_wire_tag(r.read_u8()?).ok_or("unknown family tag")?;
    let ack = decode_fence_ack(r)?;
    Ok(SignedFenceBarrierAck {
        family,
        ack,
        signer_principal_id: PrincipalId::from_bytes(r.read_16()?),
        signer_key_id: KeyId::from_bytes(r.read_16()?),
        signer_key_generation: read_gen(r)?,
        ed25519_signature: r.read_64()?,
    })
}

fn decode_reconcile_payload(
    r: &mut ByteReader<'_>,
) -> Result<RemoteReconcilePayload, &'static str> {
    match r.read_u8()? {
        1 => Ok(RemoteReconcilePayload::QuotaViaSignedClose {
            close_receipt: decode_close_receipt(r)?,
        }),
        2 => Ok(RemoteReconcilePayload::QuotaViaBarrier {
            receipt: QuotaReconciliationReceipt::new(
                ReceiptId::from_bytes(r.read_16()?),
                QuotaLeaseId::from_bytes(r.read_16()?),
                r.read_u64()?,
                read_cell(r)?,
                read_gen(r)?,
                read_epoch(r)?,
                read_token(r)?,
                r.read_u64()?,
                r.read_u64()?,
                r.read_u64()?,
                r.read_u64()?,
                r.read_u64()?,
                decode_fence_ack(r)?,
            ),
        }),
        3 => {
            let receipt_id = ReceiptId::from_bytes(r.read_16()?);
            let capacity_lease_id = CapacityLeaseId::from_bytes(r.read_16()?);
            let target_node = read_cell(r)?;
            let target_node_boot_generation = read_gen(r)?;
            let capacity_epoch = read_epoch(r)?;
            let fencing_token = read_token(r)?;
            let amount = r.read_u64()?;
            let att = CleanupAttestation::new(
                CapacityLeaseId::from_bytes(r.read_16()?),
                read_cell(r)?,
                read_gen(r)?,
                read_epoch(r)?,
                read_token(r)?,
                r.read_u64()?,
                r.read_bool()?,
                r.read_32()?,
            );
            let ack = decode_fence_ack(r)?;
            Ok(RemoteReconcilePayload::CapacityViaBarrier {
                receipt: CapacityReconciliationReceipt::new(
                    receipt_id,
                    capacity_lease_id,
                    target_node,
                    target_node_boot_generation,
                    capacity_epoch,
                    fencing_token,
                    amount,
                    att,
                    ack,
                ),
            })
        }
        4 => Ok(RemoteReconcilePayload::DeviceViaBarrier {
            receipt: DeviceReconciliationReceipt::new(
                ReceiptId::from_bytes(r.read_16()?),
                ExclusiveDeviceLeaseId::from_bytes(r.read_16()?),
                DeviceId::from_bytes(r.read_16()?),
                read_cell(r)?,
                read_gen(r)?,
                read_epoch(r)?,
                read_token(r)?,
                read_gen(r)?,
                r.read_bool()?,
                r.read_bool()?,
                r.read_bool()?,
                decode_device_reset_receipt(r)?,
                decode_fence_ack(r)?,
            ),
        }),
        _ => Err("unknown RemoteReconcilePayload tag"),
    }
}

fn decode_command(r: &mut ByteReader<'_>) -> Result<RemoteLeaseCommand, &'static str> {
    match r.read_u8()? {
        1 => Ok(RemoteLeaseCommand::GrantQuotaLease {
            lease_id: QuotaLeaseId::from_bytes(r.read_16()?),
            face_value: r.read_u64()?,
            ttl_grant_ms: r.read_u64()?,
            multi_endpoint_gateways: decode_gateways_vec(r)?,
        }),
        2 => Ok(RemoteLeaseCommand::RenewQuotaLease {
            lease_id: QuotaLeaseId::from_bytes(r.read_16()?),
            new_ttl_grant_ms: r.read_u64()?,
        }),
        3 => Ok(RemoteLeaseCommand::ReportQuotaHighWater {
            lease_id: QuotaLeaseId::from_bytes(r.read_16()?),
            high_water_spent: r.read_u64()?,
        }),
        4 => Ok(RemoteLeaseCommand::CloseQuotaLease {
            close_receipt: decode_close_receipt(r)?,
        }),
        5 => Ok(RemoteLeaseCommand::GrantCapacityLease {
            capacity_lease_id: CapacityLeaseId::from_bytes(r.read_16()?),
            amount: r.read_u64()?,
            ttl_grant_ms: r.read_u64()?,
            multi_endpoint_gateways: decode_gateways_vec(r)?,
        }),
        6 => Ok(RemoteLeaseCommand::AttachCapacityHost {
            attach_receipt: decode_attach_receipt(r)?,
        }),
        7 => Ok(RemoteLeaseCommand::ReturnCapacityLease {
            capacity_lease_id: CapacityLeaseId::from_bytes(r.read_16()?),
        }),
        8 => Ok(RemoteLeaseCommand::GrantExclusiveDeviceLease {
            device_id: DeviceId::from_bytes(r.read_16()?),
            device_lease_id: ExclusiveDeviceLeaseId::from_bytes(r.read_16()?),
            ttl_grant_ms: r.read_u64()?,
            multi_endpoint_gateways: decode_gateways_vec(r)?,
        }),
        9 => Ok(RemoteLeaseCommand::SubmitDeviceResetReceipt {
            device_id: DeviceId::from_bytes(r.read_16()?),
            reset_receipt: decode_device_reset_receipt(r)?,
        }),
        10 => Ok(RemoteLeaseCommand::SubmitFenceBarrierAck {
            signed_ack: decode_signed_fence_ack(r)?,
        }),
        11 => Ok(RemoteLeaseCommand::ReconcileQuarantinedLease {
            payload: decode_reconcile_payload(r)?,
        }),
        12 => {
            let family =
                CrossHostLeaseFamily::from_wire_tag(r.read_u8()?).ok_or("unknown family tag")?;
            let lease_id = r.read_16()?;
            let probed_idempotency_key = match r.read_u8()? {
                0 => None,
                1 => Some(IdempotencyKey::from_bytes(r.read_16()?)),
                _ => return Err("invalid option tag for probed_idempotency_key"),
            };
            Ok(RemoteLeaseCommand::QueryLeaseReconciliationState {
                family,
                lease_id,
                probed_idempotency_key,
            })
        }
        _ => Err("unknown RemoteLeaseCommand wire tag"),
    }
}

fn decode_disposition(r: &mut ByteReader<'_>) -> Result<QuarantineDisposition, &'static str> {
    match r.read_u8()? {
        0 => Ok(QuarantineDisposition::ActiveOrSettled),
        1 => Ok(QuarantineDisposition::LedgerQuarantinedFenceOnly),
        2 => {
            let proof_kind = PhysicalCleanupProofKind::from_u8(r.read_u8()?)
                .ok_or("unknown PhysicalCleanupProofKind")?;
            let proof_digest = r.read_32()?;
            Ok(QuarantineDisposition::PhysicallyCleanedVerified {
                proof_kind,
                proof_digest,
            })
        }
        _ => Err("unknown QuarantineDisposition wire tag"),
    }
}

fn decode_quota_grant(r: &mut ByteReader<'_>) -> Result<QuotaLeaseGrant, &'static str> {
    let lease_id = QuotaLeaseId::from_bytes(r.read_16()?);
    let cell = read_cell(r)?;
    let node_boot_generation = read_gen(r)?;
    let face_value = r.read_u64()?;
    let remaining = r.read_u64()?;
    let spent = r.read_u64()?;
    let returned = r.read_u64()?;
    let epoch = read_epoch(r)?;
    let fencing_token = read_token(r)?;
    let fence_scope = FenceScope::cell(read_cell(r)?);
    let expires_at = match r.read_u8()? {
        0 => None,
        1 => Some(LeaseInstant::new(r.read_u64()?)),
        _ => return Err("invalid expires_at tag"),
    };
    let state = match r.read_u8()? {
        1 => QuotaLeaseState::Issued,
        2 => QuotaLeaseState::Active,
        3 => QuotaLeaseState::Closing,
        4 => QuotaLeaseState::Closed,
        5 => QuotaLeaseState::Cancelled,
        6 => QuotaLeaseState::Fenced,
        7 => QuotaLeaseState::Quarantined,
        _ => return Err("invalid QuotaLeaseState tag"),
    };
    Ok(QuotaLeaseGrant {
        lease_id,
        cell,
        node_boot_generation,
        face_value,
        remaining,
        spent,
        returned,
        epoch,
        fencing_token,
        fence_scope,
        expires_at,
        state,
    })
}

fn decode_capacity_grant(r: &mut ByteReader<'_>) -> Result<CapacityLeaseGrant, &'static str> {
    let capacity_lease_id = CapacityLeaseId::from_bytes(r.read_16()?);
    let target_node = read_cell(r)?;
    let target_node_boot_generation = read_gen(r)?;
    let amount = r.read_u64()?;
    let capacity_epoch = read_epoch(r)?;
    let fencing_token = read_token(r)?;
    let fence_scope = FenceScope::cell(read_cell(r)?);
    let state = match r.read_u8()? {
        1 => CapacityLeaseState::GlobalReserved,
        2 => CapacityLeaseState::TargetPrepared,
        3 => CapacityLeaseState::Active,
        4 => CapacityLeaseState::Returning,
        5 => CapacityLeaseState::Fenced,
        6 => CapacityLeaseState::Quarantined,
        7 => CapacityLeaseState::Returned,
        _ => return Err("invalid CapacityLeaseState tag"),
    };
    Ok(CapacityLeaseGrant {
        capacity_lease_id,
        target_node,
        target_node_boot_generation,
        amount,
        capacity_epoch,
        fencing_token,
        fence_scope,
        state,
    })
}

fn decode_device_grant(r: &mut ByteReader<'_>) -> Result<ExclusiveDeviceLeaseGrant, &'static str> {
    let device_lease_id = ExclusiveDeviceLeaseId::from_bytes(r.read_16()?);
    let device_id = DeviceId::from_bytes(r.read_16()?);
    let holder_node = read_cell(r)?;
    let holder_node_boot_generation = read_gen(r)?;
    let exclusivity_epoch = read_epoch(r)?;
    let fencing_token = read_token(r)?;
    let fence_scope = FenceScope::cell(read_cell(r)?);
    let reset_generation = read_gen(r)?;
    let state = match r.read_u8()? {
        1 => ExclusiveDeviceLeaseState::DeviceReserved,
        2 => ExclusiveDeviceLeaseState::Resetting,
        3 => ExclusiveDeviceLeaseState::Fencing,
        4 => ExclusiveDeviceLeaseState::Quarantined,
        5 => ExclusiveDeviceLeaseState::Returned,
        _ => return Err("invalid ExclusiveDeviceLeaseState tag"),
    };
    Ok(ExclusiveDeviceLeaseGrant {
        device_lease_id,
        device_id,
        holder_node,
        holder_node_boot_generation,
        exclusivity_epoch,
        fencing_token,
        fence_scope,
        reset_generation,
        state,
    })
}

fn decode_coverage(r: &mut ByteReader<'_>) -> Result<FenceScopeCoverageStatus, &'static str> {
    let acked_count = usize::try_from(r.read_u32()?).map_err(|_| "acked_count")?;
    let required_count = usize::try_from(r.read_u32()?).map_err(|_| "required_count")?;
    let missing_gateways = decode_gateways_vec(r)?;
    Ok(FenceScopeCoverageStatus {
        acked_count,
        required_count,
        missing_gateways,
    })
}

#[allow(clippy::too_many_lines)]
fn decode_result(
    r: &mut ByteReader<'_>,
) -> Result<Result<RemoteLeaseSuccess, RemoteLeaseReject>, &'static str> {
    match r.read_u8()? {
        1 => {
            let ok = match r.read_u8()? {
                1 => RemoteLeaseSuccess::Quota {
                    grant: decode_quota_grant(r)?,
                    ttl_grant_ms: r.read_u64()?,
                    renewal_seq: r.read_u64()?,
                    grantor_available: r.read_u64()?,
                    disposition: decode_disposition(r)?,
                },
                2 => RemoteLeaseSuccess::Capacity {
                    grant: decode_capacity_grant(r)?,
                    ttl_grant_ms: r.read_u64()?,
                    grantor_pool_remaining: r.read_u64()?,
                    disposition: decode_disposition(r)?,
                },
                3 => RemoteLeaseSuccess::ExclusiveDevice {
                    grant: decode_device_grant(r)?,
                    ttl_grant_ms: r.read_u64()?,
                    head_is_free: r.read_bool()?,
                    head_reset_generation: read_gen(r)?,
                    disposition: decode_disposition(r)?,
                },
                4 => RemoteLeaseSuccess::BarrierAckRecorded {
                    coverage: decode_coverage(r)?,
                },
                5 => RemoteLeaseSuccess::ReconciliationSnapshot(Box::new(
                    LeaseReconciliationSnapshot {
                        lease_known: r.read_bool()?,
                        quota_grant: match r.read_u8()? {
                            0 => None,
                            1 => Some(decode_quota_grant(r)?),
                            _ => return Err("opt quota_grant"),
                        },
                        capacity_grant: match r.read_u8()? {
                            0 => None,
                            1 => Some(decode_capacity_grant(r)?),
                            _ => return Err("opt capacity_grant"),
                        },
                        device_grant: match r.read_u8()? {
                            0 => None,
                            1 => Some(decode_device_grant(r)?),
                            _ => return Err("opt device_grant"),
                        },
                        disposition: match r.read_u8()? {
                            0 => None,
                            1 => Some(decode_disposition(r)?),
                            _ => return Err("opt disposition"),
                        },
                        renewal_seq: match r.read_u8()? {
                            0 => None,
                            1 => Some(r.read_u64()?),
                            _ => return Err("opt renewal_seq"),
                        },
                        barrier_coverage: match r.read_u8()? {
                            0 => None,
                            1 => Some(decode_coverage(r)?),
                            _ => return Err("opt barrier_coverage"),
                        },
                        probed_idempotency_committed_seq: match r.read_u8()? {
                            0 => None,
                            1 => Some(r.read_u64()?),
                            _ => return Err("opt probed_idempotency_committed_seq"),
                        },
                    },
                )),
                _ => return Err("unknown RemoteLeaseSuccess tag"),
            };
            Ok(Ok(ok))
        }
        0 => {
            let err = match r.read_u8()? {
                1 => RemoteLeaseReject::MalformedWire(r.read_str()?),
                2 => RemoteLeaseReject::PeerSessionIdentityMismatch(r.read_str()?),
                3 => RemoteLeaseReject::CrossCellImpersonation {
                    caller_cell: read_cell(r)?,
                    expected_holder_cell: read_cell(r)?,
                },
                4 => RemoteLeaseReject::SignatureRejected(r.read_str()?),
                5 => RemoteLeaseReject::IdempotencyConflict,
                6 => RemoteLeaseReject::StaleOrMismatchedEpoch {
                    expected: read_epoch(r)?,
                    actual: read_epoch(r)?,
                },
                7 => RemoteLeaseReject::AuthorityTickExpired {
                    expiry_tick: r.read_u64()?,
                    current_tick: r.read_u64()?,
                },
                8 => RemoteLeaseReject::ClockSkewQuarantined(r.read_str()?),
                9 => RemoteLeaseReject::CoordinatorRejected(r.read_str()?),
                _ => return Err("unknown RemoteLeaseReject tag"),
            };
            Ok(Err(err))
        }
        _ => Err("invalid Result tag in response envelope"),
    }
}
