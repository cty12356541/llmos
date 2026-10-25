//! Enforcement-gateway signed outcome attestation and bounded `Resource` /
//! `Task` settlement reconciliation (`#17 C-CUSTODY-GATEWAY`, ADR-0017
//! Constraint 4 & Review Trigger 2, `v0.5 §10.2` `[BUD-CLOSE-001]` /
//! `[BUD-UNCERTAIN-001]` / `[BUD-REBATE-001]` / `[BUD-REBATE-002]` /
//! `[BUD-LATE-001]`).
//!
//! An [`EnforcementGatewayReconciliationAuthority`] never holds raw `Ed25519`
//! private keys: it signs [`GatewayReconciliationAttestation`] payloads
//! exclusively through [`SigningKeyCustodyVault::sign_digest_under_custody`],
//! binding every attestation to a monotonic custody `audit_sign_seq` and
//! `custody_rotation_epoch`.
//!
//! [`verify_and_apply_gateway_reconciliation`] enforces dual custody + identity
//! signature verification, monotonic supersession versioning, provisional
//! audit-only projection (`[BUD-REBATE-001]`), fail-closed quarantine freeze on
//! uncertain outcomes (`[BUD-UNCERTAIN-001]`), and bounded `R-B` settlement
//! across [`ResourceAuthority`] and [`SqliteTaskAuthority`] on
//! [`ReceiptFinality::FinalNonSupersedeable`].

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use nlos_capability::custody::{
    CustodySignAuditReceipt, CustodySignRequest, CustodyVaultError, Ed25519Signature,
    IdentityAuthority, IdentityAuthorityError, SigningKeyCustodyVault,
    VerifySemanticAuthoritySignatureRequest, custody_domain_sha256,
};
use nlos_resource::{
    FencingToken, FinalizationReceipt, FinalizeReservationRequest, QuarantineReceipt,
    QuarantineReservationRequest, ReservationState, ResourceAuthority, ResourceAuthorityError,
};
use nlos_task::{
    ResourceCommitPlanId, ResourceConvergeDecision, SqliteTaskAuthority, TaskStoreError,
};
use nlos_types::{ControlDomainId, KeyId, OperationId, PrincipalId, ReservationId};

const DOMAIN_GATEWAY_ATTESTATION: &[u8] = b"nlos/gateway/reconciliation-attestation/v1";
const DOMAIN_GATEWAY_SIGN_TAG: &[u8] = b"nlos/gateway/reconcile-sign/v1";
const DOMAIN_EFFECT_CLOSED_PROOF: &[u8] = b"nlos/gateway/effect-closed-proof/v1";
const DOMAIN_UNCERTAIN_QUARANTINE_REASON: &[u8] = b"nlos/gateway/uncertain-quarantine-reason/v1";

/// Endpoint outcome disposition attested by an enforcement gateway.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayOutcomeDisposition {
    /// External side-effect is proven closed at monotonic `(final_seq, final_usage)`.
    EffectClosed { final_seq: u64, final_usage: u64 },
    /// External side-effect is proven never executed (`final_seq = 0, final_usage = 0`).
    ConfirmedNoEffect,
    /// External side-effect remains uncertain; reservation must stay or become
    /// `Quarantined` with zero fund release (`[BUD-UNCERTAIN-001]`).
    StillUncertainQuarantine,
}

impl GatewayOutcomeDisposition {
    const fn tag(self) -> u8 {
        match self {
            Self::EffectClosed { .. } => 1,
            Self::ConfirmedNoEffect => 2,
            Self::StillUncertainQuarantine => 3,
        }
    }

    const fn encoded_seq_and_usage(self) -> (u64, u64) {
        match self {
            Self::EffectClosed {
                final_seq,
                final_usage,
            } => (final_seq, final_usage),
            Self::ConfirmedNoEffect | Self::StillUncertainQuarantine => (0, 0),
        }
    }
}

/// Finality tier of a [`GatewayReconciliationAttestation`] (`[BUD-REBATE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptFinality {
    /// Advisory / projected outcome: updates audit projection only and never
    /// releases quarantined holds, moves credit, or converges Task commit plans.
    Provisional,
    /// Terminal settlement proof: settles [`ResourceAuthority`], converges
    /// [`SqliteTaskAuthority`] when due, and permanently locks the reservation
    /// against any subsequent supersession version.
    FinalNonSupersedeable,
}

impl ReceiptFinality {
    const fn tag(self) -> u8 {
        match self {
            Self::Provisional => 1,
            Self::FinalNonSupersedeable => 2,
        }
    }
}

/// Cryptographically signed enforcement-gateway reconciliation attestation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GatewayReconciliationAttestation {
    pub reconcile_id: [u8; 16],
    pub gateway_id: [u8; 16],
    pub gateway_principal: PrincipalId,
    pub gateway_control_domain: ControlDomainId,
    pub key_id: KeyId,
    pub custody_rotation_epoch: u64,
    pub reservation_id: ReservationId,
    pub operation_id: OperationId,
    pub fencing_token: FencingToken,
    pub disposition: GatewayOutcomeDisposition,
    pub receipt_version: u64,
    pub finality: ReceiptFinality,
    pub issued_at_ms: u64,
    pub audit_sign_seq: u64,
    pub signature: Ed25519Signature,
}

impl GatewayReconciliationAttestation {
    /// Computes the canonical 32-byte domain-separated digest signed by the
    /// gateway's custody-managed `Ed25519` key.
    #[must_use]
    pub fn signing_digest(&self) -> [u8; 32] {
        let (seq, usage) = self.disposition.encoded_seq_and_usage();
        custody_domain_sha256(
            DOMAIN_GATEWAY_ATTESTATION,
            &[
                self.reconcile_id.as_slice(),
                self.gateway_id.as_slice(),
                self.gateway_principal.as_bytes(),
                self.gateway_control_domain.as_bytes(),
                self.key_id.as_bytes(),
                &self.custody_rotation_epoch.to_be_bytes(),
                self.reservation_id.as_bytes(),
                self.operation_id.as_bytes(),
                self.fencing_token.as_slice(),
                &[self.disposition.tag()],
                &seq.to_be_bytes(),
                &usage.to_be_bytes(),
                &self.receipt_version.to_be_bytes(),
                &[self.finality.tag()],
                &self.issued_at_ms.to_be_bytes(),
            ],
        )
    }

    /// Derives the 32-byte proof digest anchored into
    /// [`FinalizeReservationRequest::effect_closed_proof_digest`], binding both
    /// the canonical attestation preimage and its verified `Ed25519` signature.
    #[must_use]
    pub fn effect_closed_proof_digest(&self) -> [u8; 32] {
        let signing_digest = self.signing_digest();
        custody_domain_sha256(
            DOMAIN_EFFECT_CLOSED_PROOF,
            &[signing_digest.as_slice(), self.signature.as_slice()],
        )
    }

    /// Derives the 32-byte quarantine reason digest for
    /// [`GatewayOutcomeDisposition::StillUncertainQuarantine`].
    #[must_use]
    pub fn uncertain_quarantine_reason_digest(&self) -> [u8; 32] {
        let signing_digest = self.signing_digest();
        custody_domain_sha256(
            DOMAIN_UNCERTAIN_QUARANTINE_REASON,
            &[signing_digest.as_slice(), self.signature.as_slice()],
        )
    }

    fn semantic_fields_equal(&self, other: &Self) -> bool {
        self.reconcile_id == other.reconcile_id
            && self.gateway_id == other.gateway_id
            && self.gateway_principal == other.gateway_principal
            && self.gateway_control_domain == other.gateway_control_domain
            && self.key_id == other.key_id
            && self.custody_rotation_epoch == other.custody_rotation_epoch
            && self.reservation_id == other.reservation_id
            && self.operation_id == other.operation_id
            && self.fencing_token == other.fencing_token
            && self.disposition == other.disposition
            && self.receipt_version == other.receipt_version
            && self.finality == other.finality
            && self.issued_at_ms == other.issued_at_ms
            && self.signature == other.signature
    }
}

/// Input to issue a signed [`GatewayReconciliationAttestation`] via
/// [`EnforcementGatewayReconciliationAuthority`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IssueGatewayAttestationRequest {
    pub reconcile_id: [u8; 16],
    pub reservation_id: ReservationId,
    pub operation_id: OperationId,
    pub fencing_token: FencingToken,
    pub disposition: GatewayOutcomeDisposition,
    pub receipt_version: u64,
    pub finality: ReceiptFinality,
    pub issued_at_ms: u64,
}

/// Audit-only projected rebate record created by [`ReceiptFinality::Provisional`]
/// attestations (`[BUD-REBATE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectedReconciliationAudit {
    pub reservation_id: ReservationId,
    pub operation_id: OperationId,
    pub receipt_version: u64,
    pub projected_seq: u64,
    pub projected_usage: u64,
    pub projected_rebate: u64,
    pub recorded_at_ms: u64,
    pub attestation_digest: [u8; 32],
}

/// Terminal settlement evidence produced when a
/// [`ReceiptFinality::FinalNonSupersedeable`] attestation closes a reservation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewaySettlementOutcome {
    pub reservation_id: ReservationId,
    pub operation_id: OperationId,
    pub receipt_version: u64,
    pub finalization_receipt: FinalizationReceipt,
    pub task_convergence: Option<ResourceConvergeDecision>,
}

/// Outcome returned by [`verify_and_apply_gateway_reconciliation`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GatewayReconciliationDecision {
    /// Provisional attestation recorded as projected audit state only; zero
    /// credit moved and reservation state unchanged (`[BUD-REBATE-001]`).
    ProjectedProvisional(ProjectedReconciliationAudit),
    /// Exact idempotent replay of an already-recorded provisional attestation.
    ReplayedProvisional(ProjectedReconciliationAudit),
    /// Uncertain outcome kept or transitioned the reservation into `Quarantined`
    /// with zero credit movement (`[BUD-UNCERTAIN-001]`).
    FrozenQuarantined {
        quarantine_receipt: QuarantineReceipt,
        receipt_version: u64,
    },
    /// Exact idempotent replay of an already-recorded uncertain quarantine
    /// attestation.
    ReplayedQuarantined {
        quarantine_receipt: QuarantineReceipt,
        receipt_version: u64,
    },
    /// Authoritative `FinalNonSupersedeable` settlement applied across
    /// [`ResourceAuthority`] (and converged on [`SqliteTaskAuthority`] if supplied).
    SettledFinal(GatewaySettlementOutcome),
    /// Exact idempotent replay of an already-settled `FinalNonSupersedeable`
    /// attestation; zero duplicate refund or mutation occurred.
    ReplayedFinal(GatewaySettlementOutcome),
}

/// Typed fail-closed errors of enforcement-gateway attestation issuance and
/// reconciliation application.
#[derive(Debug)]
pub enum GatewayReconcileError {
    InvalidReceiptVersion(u64),
    InvalidDispositionFinalityCombination,
    InvalidTimestamp(u64),
    GatewayIdentityMismatch {
        expected: [u8; 16],
        actual: [u8; 16],
    },
    SignerBindingMismatch,
    Custody(CustodyVaultError),
    Identity(IdentityAuthorityError),
    Resource(ResourceAuthorityError),
    Task(TaskStoreError),
    OperationBindingMismatch {
        expected: OperationId,
        actual: OperationId,
    },
    FencingTokenMismatch {
        expected: FencingToken,
        actual: FencingToken,
    },
    ReservationNotActivated(ReservationId),
    ReservationAlreadyFinalizedOutOfBand(ReservationId),
    ConflictingSameVersionAttestation {
        reservation_id: ReservationId,
        receipt_version: u64,
    },
    StaleReceiptVersion {
        reservation_id: ReservationId,
        current_version: u64,
        attempted_version: u64,
    },
    FinalReceiptNonSupersedeable {
        reservation_id: ReservationId,
        settled_version: u64,
        attempted_version: u64,
    },
    BelowHighWaterMark {
        high_water_seq: u64,
        high_water_usage: u64,
        reported_seq: u64,
        reported_usage: u64,
    },
    ExceedsUpperBound {
        upper_bound: u64,
        reported_usage: u64,
    },
}

impl fmt::Display for GatewayReconcileError {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidReceiptVersion(version) => {
                write!(formatter, "receipt_version must be >= 1, got {version}")
            }
            Self::InvalidDispositionFinalityCombination => formatter.write_str(
                "StillUncertainQuarantine cannot be combined with FinalNonSupersedeable",
            ),
            Self::InvalidTimestamp(ts) => {
                write!(formatter, "timestamp {ts} exceeds signed 64-bit Task bound")
            }
            Self::GatewayIdentityMismatch { expected, actual } => write!(
                formatter,
                "gateway_id mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::SignerBindingMismatch => {
                formatter.write_str("gateway principal/control-domain/key binding mismatch")
            }
            Self::Custody(error) => {
                write!(formatter, "custody vault verification failure: {error}")
            }
            Self::Identity(error) => {
                write!(
                    formatter,
                    "identity authority verification failure: {error}"
                )
            }
            Self::Resource(error) => {
                write!(
                    formatter,
                    "resource authority reconciliation failure: {error}"
                )
            }
            Self::Task(error) => {
                write!(formatter, "task authority convergence failure: {error}")
            }
            Self::OperationBindingMismatch { expected, actual } => write!(
                formatter,
                "operation_id mismatch: reservation bound to {expected:?}, attestation claimed {actual:?}"
            ),
            Self::FencingTokenMismatch { .. } => {
                formatter.write_str("attestation fencing_token disagrees with reservation fence")
            }
            Self::ReservationNotActivated(id) => {
                write!(formatter, "reservation {id:?} was never activated")
            }
            Self::ReservationAlreadyFinalizedOutOfBand(id) => write!(
                formatter,
                "reservation {id:?} was already finalized outside this reconciliation ledger"
            ),
            Self::ConflictingSameVersionAttestation {
                reservation_id,
                receipt_version,
            } => write!(
                formatter,
                "conflicting attestation content at same receipt_version {receipt_version} for {reservation_id:?}"
            ),
            Self::StaleReceiptVersion {
                reservation_id,
                current_version,
                attempted_version,
            } => write!(
                formatter,
                "non-monotonic receipt_version {attempted_version} < {current_version} for {reservation_id:?}"
            ),
            Self::FinalReceiptNonSupersedeable {
                reservation_id,
                settled_version,
                attempted_version,
            } => write!(
                formatter,
                "reservation {reservation_id:?} settled at FinalNonSupersedeable version {settled_version}; version {attempted_version} rejected"
            ),
            Self::BelowHighWaterMark {
                high_water_seq,
                high_water_usage,
                reported_seq,
                reported_usage,
            } => write!(
                formatter,
                "attested outcome ({reported_seq}, {reported_usage}) regresses below durable high-water ({high_water_seq}, {high_water_usage})"
            ),
            Self::ExceedsUpperBound {
                upper_bound,
                reported_usage,
            } => write!(
                formatter,
                "attested usage {reported_usage} exceeds reservation upper_bound {upper_bound}"
            ),
        }
    }
}

impl Error for GatewayReconcileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Custody(error) => Some(error),
            Self::Identity(error) => Some(error),
            Self::Resource(error) => Some(error),
            Self::Task(error) => Some(error),
            _ => None,
        }
    }
}

impl From<CustodyVaultError> for GatewayReconcileError {
    fn from(error: CustodyVaultError) -> Self {
        Self::Custody(error)
    }
}

impl From<IdentityAuthorityError> for GatewayReconcileError {
    fn from(error: IdentityAuthorityError) -> Self {
        Self::Identity(error)
    }
}

impl From<ResourceAuthorityError> for GatewayReconcileError {
    fn from(error: ResourceAuthorityError) -> Self {
        Self::Resource(error)
    }
}

impl From<TaskStoreError> for GatewayReconcileError {
    fn from(error: TaskStoreError) -> Self {
        Self::Task(error)
    }
}

/// Enforcement-gateway signing authority bound to a custody-managed key slot.
///
/// Holds only public identity metadata (`gateway_id`, `gateway_principal`,
/// `gateway_control_domain`, `key_id`); signing always delegates to
/// [`SigningKeyCustodyVault::sign_digest_under_custody`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnforcementGatewayReconciliationAuthority {
    gateway_id: [u8; 16],
    gateway_principal: PrincipalId,
    gateway_control_domain: ControlDomainId,
    key_id: KeyId,
}

impl EnforcementGatewayReconciliationAuthority {
    #[must_use]
    pub const fn new(
        gateway_id: [u8; 16],
        gateway_principal: PrincipalId,
        gateway_control_domain: ControlDomainId,
        key_id: KeyId,
    ) -> Self {
        Self {
            gateway_id,
            gateway_principal,
            gateway_control_domain,
            key_id,
        }
    }

    #[must_use]
    pub const fn gateway_id(&self) -> [u8; 16] {
        self.gateway_id
    }

    #[must_use]
    pub const fn gateway_principal(&self) -> PrincipalId {
        self.gateway_principal
    }

    #[must_use]
    pub const fn gateway_control_domain(&self) -> ControlDomainId {
        self.gateway_control_domain
    }

    #[must_use]
    pub const fn key_id(&self) -> KeyId {
        self.key_id
    }

    /// Issues a signed [`GatewayReconciliationAttestation`] using the
    /// currently `Active` epoch of `self.key_id` inside `custody_vault`
    /// (or `explicit_epoch` when supplied for negative/historical tests).
    ///
    /// # Errors
    ///
    /// Fails closed if `receipt_version == 0`, `(StillUncertainQuarantine, FinalNonSupersedeable)`
    /// is requested, the custody slot's authority/principal metadata disagrees
    /// with this gateway, or [`SigningKeyCustodyVault::sign_digest_under_custody`]
    /// rejects signing.
    pub fn issue_attestation(
        &self,
        custody_vault: &SigningKeyCustodyVault,
        request: IssueGatewayAttestationRequest,
    ) -> Result<(GatewayReconciliationAttestation, CustodySignAuditReceipt), GatewayReconcileError>
    {
        self.issue_attestation_at_epoch(custody_vault, request, None)
    }

    /// Issues a signed [`GatewayReconciliationAttestation`] targeting an
    /// optional explicit custody `rotation_epoch`.
    ///
    /// # Errors
    ///
    /// Returns [`GatewayReconcileError`] on invalid version/finality or custody
    /// signing rejection.
    pub fn issue_attestation_at_epoch(
        &self,
        custody_vault: &SigningKeyCustodyVault,
        request: IssueGatewayAttestationRequest,
        explicit_epoch: Option<u64>,
    ) -> Result<(GatewayReconciliationAttestation, CustodySignAuditReceipt), GatewayReconcileError>
    {
        if request.receipt_version == 0 {
            return Err(GatewayReconcileError::InvalidReceiptVersion(0));
        }
        if request.disposition == GatewayOutcomeDisposition::StillUncertainQuarantine
            && request.finality == ReceiptFinality::FinalNonSupersedeable
        {
            return Err(GatewayReconcileError::InvalidDispositionFinalityCombination);
        }
        let slot = match explicit_epoch {
            Some(epoch) => custody_vault.inspect_key_slot_epoch(self.key_id, epoch)?,
            None => custody_vault.inspect_latest_key_slot(self.key_id)?,
        };
        if slot.authority_id != self.gateway_id
            || slot.principal_id != self.gateway_principal
            || slot.control_domain_id != self.gateway_control_domain
        {
            return Err(GatewayReconcileError::SignerBindingMismatch);
        }
        let mut attestation = GatewayReconciliationAttestation {
            reconcile_id: request.reconcile_id,
            gateway_id: self.gateway_id,
            gateway_principal: self.gateway_principal,
            gateway_control_domain: self.gateway_control_domain,
            key_id: self.key_id,
            custody_rotation_epoch: slot.rotation_epoch,
            reservation_id: request.reservation_id,
            operation_id: request.operation_id,
            fencing_token: request.fencing_token,
            disposition: request.disposition,
            receipt_version: request.receipt_version,
            finality: request.finality,
            issued_at_ms: request.issued_at_ms,
            audit_sign_seq: 0,
            signature: [0u8; 64],
        };
        let signing_digest = attestation.signing_digest();
        let audit = custody_vault.sign_digest_under_custody(CustodySignRequest {
            key_id: self.key_id,
            expected_rotation_epoch: Some(slot.rotation_epoch),
            domain_tag: DOMAIN_GATEWAY_SIGN_TAG,
            message_digest: signing_digest,
            signed_at_ms: request.issued_at_ms,
        })?;
        attestation.audit_sign_seq = audit.sign_seq;
        attestation.signature = audit.signature;
        Ok((attestation, audit))
    }
}

#[derive(Clone, Debug)]
enum NonTerminalEntry {
    Provisional(ProjectedReconciliationAudit),
    Quarantined(QuarantineReceipt),
}

#[derive(Clone, Debug)]
struct ReservationReconcileState {
    latest_version: u64,
    latest_attestation: GatewayReconciliationAttestation,
    projected_audit: Option<ProjectedReconciliationAudit>,
    non_terminal: Option<NonTerminalEntry>,
    terminal_settlement: Option<GatewaySettlementOutcome>,
}

/// Coordinator-side reconciliation supersession ledger enforcing monotonic
/// `receipt_version` chains, audit-only `Provisional` rebate tracking, and
/// non-supersedeable `FinalNonSupersedeable` terminal locks.
#[derive(Clone, Debug, Default)]
pub struct GatewayReconciliationLedger {
    reservations: BTreeMap<[u8; 16], ReservationReconcileState>,
}

impl GatewayReconciliationLedger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the latest provisional rebate projection for `reservation_id`,
    /// if the reservation has not yet settled via `FinalNonSupersedeable`.
    #[must_use]
    pub fn inspect_projected_audit(
        &self,
        reservation_id: ReservationId,
    ) -> Option<ProjectedReconciliationAudit> {
        self.reservations
            .get(reservation_id.as_bytes())
            .and_then(|entry| entry.projected_audit)
    }

    /// Returns the terminal settlement outcome for `reservation_id`, if settled.
    #[must_use]
    pub fn inspect_terminal_settlement(
        &self,
        reservation_id: ReservationId,
    ) -> Option<&GatewaySettlementOutcome> {
        self.reservations
            .get(reservation_id.as_bytes())
            .and_then(|entry| entry.terminal_settlement.as_ref())
    }

    /// Returns the highest accepted `receipt_version` for `reservation_id`.
    #[must_use]
    pub fn latest_version(&self, reservation_id: ReservationId) -> Option<u64> {
        self.reservations
            .get(reservation_id.as_bytes())
            .map(|entry| entry.latest_version)
    }
}

/// Parameters for [`verify_and_apply_gateway_reconciliation`].
pub struct ApplyGatewayReconciliationContext<'a> {
    pub ledger: &'a mut GatewayReconciliationLedger,
    pub expected_gateway_id: [u8; 16],
    pub custody_vault: &'a SigningKeyCustodyVault,
    pub identity_authority: &'a IdentityAuthority,
    pub resource_authority: &'a ResourceAuthority,
    pub task_commit_plan: Option<(&'a SqliteTaskAuthority, ResourceCommitPlanId)>,
    pub verified_at_ms: u64,
}

/// Verifies a [`GatewayReconciliationAttestation`] against both
/// [`SigningKeyCustodyVault`] and [`IdentityAuthority`], enforces supersession
/// and `[BUD-CLOSE-001]` / `[BUD-UNCERTAIN-001]` / `[BUD-REBATE-001]` rules,
/// and applies the bounded outcome to [`ResourceAuthority`] and optional
/// [`SqliteTaskAuthority`].
///
/// # Errors
///
/// Fails closed with zero resource settlement or fund movement when:
/// - `receipt_version == 0` or `(StillUncertainQuarantine, FinalNonSupersedeable)` is supplied;
/// - `gateway_id` or principal/control-domain/key bindings disagree;
/// - the custody key is `CompromisedRevoked`, `RotatedVerifyOnly` (stale epoch),
///   `Expired`, not yet valid, or fails strict `Ed25519` verification;
/// - `IdentityAuthority` rejects the key generation, validity, revocation, or signature;
/// - `receipt_version` conflicts at the same version, regresses below the
///   current version, or attempts to supersede a `FinalNonSupersedeable` receipt;
/// - `operation_id` or `fencing_token` disagrees with the owner reservation;
/// - `(final_seq, final_usage)` regresses below durable high-water or exceeds `upper_bound`.
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)] // Context bundles `&mut GatewayReconciliationLedger` + authority refs linearly.
pub fn verify_and_apply_gateway_reconciliation(
    ctx: ApplyGatewayReconciliationContext<'_>,
    attestation: &GatewayReconciliationAttestation,
) -> Result<GatewayReconciliationDecision, GatewayReconcileError> {
    if attestation.receipt_version == 0 {
        return Err(GatewayReconcileError::InvalidReceiptVersion(0));
    }
    if attestation.disposition == GatewayOutcomeDisposition::StillUncertainQuarantine
        && attestation.finality == ReceiptFinality::FinalNonSupersedeable
    {
        return Err(GatewayReconcileError::InvalidDispositionFinalityCombination);
    }
    if attestation.gateway_id != ctx.expected_gateway_id {
        return Err(GatewayReconcileError::GatewayIdentityMismatch {
            expected: ctx.expected_gateway_id,
            actual: attestation.gateway_id,
        });
    }

    // Gate 1: Verify custody vault active epoch, non-compromise, validity, and strict Ed25519.
    let signing_digest = attestation.signing_digest();
    let verified_custody = ctx.custody_vault.verify_active_epoch_signature(
        attestation.key_id,
        attestation.custody_rotation_epoch,
        &signing_digest,
        &attestation.signature,
        ctx.verified_at_ms,
    )?;
    if verified_custody.authority_id != ctx.expected_gateway_id {
        return Err(GatewayReconcileError::GatewayIdentityMismatch {
            expected: ctx.expected_gateway_id,
            actual: verified_custody.authority_id,
        });
    }
    if verified_custody.principal_id != attestation.gateway_principal
        || verified_custody.control_domain_id != attestation.gateway_control_domain
    {
        return Err(GatewayReconcileError::SignerBindingMismatch);
    }

    // Gate 2: Verify IdentityAuthority active binding, generation, non-revocation, and dalek strict Ed25519.
    let verified_identity = ctx.identity_authority.verify_semantic_authority_signature(
        VerifySemanticAuthoritySignatureRequest {
            issuer: attestation.gateway_principal,
            control_domain_id: attestation.gateway_control_domain,
            key_id: attestation.key_id,
            message_digest: signing_digest,
            signature: attestation.signature,
            verified_at_ms: ctx.verified_at_ms,
        },
    )?;
    if verified_identity.key_generation().get() != attestation.custody_rotation_epoch {
        return Err(GatewayReconcileError::SignerBindingMismatch);
    }

    // Gate 3: Supersession, idempotency replay, and terminal non-supersedeability.
    let key_bytes = attestation.reservation_id.into_bytes();
    if let Some(existing) = ctx.ledger.reservations.get_mut(&key_bytes) {
        if let Some(settled) = &mut existing.terminal_settlement {
            if attestation.receipt_version == existing.latest_version {
                if !existing
                    .latest_attestation
                    .semantic_fields_equal(attestation)
                {
                    return Err(GatewayReconcileError::ConflictingSameVersionAttestation {
                        reservation_id: attestation.reservation_id,
                        receipt_version: attestation.receipt_version,
                    });
                }
                if let Some((task_auth, plan_id)) = ctx.task_commit_plan {
                    let now_i64 = i64::try_from(ctx.verified_at_ms)
                        .map_err(|_| GatewayReconcileError::InvalidTimestamp(ctx.verified_at_ms))?;
                    let converged = task_auth.converge_resource_commit_plan(
                        ctx.resource_authority,
                        plan_id,
                        now_i64,
                    )?;
                    settled.task_convergence = Some(converged);
                }
                return Ok(GatewayReconciliationDecision::ReplayedFinal(
                    settled.clone(),
                ));
            }
            return Err(GatewayReconcileError::FinalReceiptNonSupersedeable {
                reservation_id: attestation.reservation_id,
                settled_version: existing.latest_version,
                attempted_version: attestation.receipt_version,
            });
        }

        if attestation.receipt_version == existing.latest_version {
            if !existing
                .latest_attestation
                .semantic_fields_equal(attestation)
            {
                return Err(GatewayReconcileError::ConflictingSameVersionAttestation {
                    reservation_id: attestation.reservation_id,
                    receipt_version: attestation.receipt_version,
                });
            }
            if let Some(non_terminal) = &existing.non_terminal {
                return match non_terminal {
                    NonTerminalEntry::Provisional(audit) => {
                        Ok(GatewayReconciliationDecision::ReplayedProvisional(*audit))
                    }
                    NonTerminalEntry::Quarantined(receipt) => {
                        Ok(GatewayReconciliationDecision::ReplayedQuarantined {
                            quarantine_receipt: *receipt,
                            receipt_version: existing.latest_version,
                        })
                    }
                };
            }
        }

        if attestation.receipt_version < existing.latest_version {
            return Err(GatewayReconcileError::StaleReceiptVersion {
                reservation_id: attestation.reservation_id,
                current_version: existing.latest_version,
                attempted_version: attestation.receipt_version,
            });
        }
    }

    // Gate 4: Owner reservation binding & fencing check.
    let reservation = ctx
        .resource_authority
        .inspect_reservation(attestation.reservation_id)?;
    if reservation.operation_id != attestation.operation_id {
        return Err(GatewayReconcileError::OperationBindingMismatch {
            expected: reservation.operation_id,
            actual: attestation.operation_id,
        });
    }
    if reservation.driver_fencing_token != attestation.fencing_token {
        return Err(GatewayReconcileError::FencingTokenMismatch {
            expected: reservation.driver_fencing_token,
            actual: attestation.fencing_token,
        });
    }
    if reservation.state == ReservationState::Finalized {
        return Err(GatewayReconcileError::ReservationAlreadyFinalizedOutOfBand(
            attestation.reservation_id,
        ));
    }
    let activation_receipt_id =
        reservation
            .activation_receipt_id
            .ok_or(GatewayReconcileError::ReservationNotActivated(
                attestation.reservation_id,
            ))?;

    // Gate 5A: Uncertain disposition -> freeze Active -> Quarantined or preserve Quarantined ([BUD-UNCERTAIN-001]).
    if attestation.disposition == GatewayOutcomeDisposition::StillUncertainQuarantine {
        let quarantine_receipt = match reservation.state {
            ReservationState::Active => ctx
                .resource_authority
                .quarantine(QuarantineReservationRequest {
                    reservation_id: attestation.reservation_id,
                    operation_id: attestation.operation_id,
                    activation_receipt_id,
                    reason_digest: attestation.uncertain_quarantine_reason_digest(),
                    quarantined_at_ms: attestation.issued_at_ms,
                })?
                .receipt(),
            ReservationState::Quarantined => ctx
                .resource_authority
                .inspect_quarantine_receipt(attestation.reservation_id)?,
            ReservationState::Reserved | ReservationState::Finalized => {
                return Err(GatewayReconcileError::ReservationNotActivated(
                    attestation.reservation_id,
                ));
            }
        };
        let prev_projected = ctx
            .ledger
            .reservations
            .get(&key_bytes)
            .and_then(|s| s.projected_audit);
        ctx.ledger.reservations.insert(
            key_bytes,
            ReservationReconcileState {
                latest_version: attestation.receipt_version,
                latest_attestation: *attestation,
                projected_audit: prev_projected,
                non_terminal: Some(NonTerminalEntry::Quarantined(quarantine_receipt)),
                terminal_settlement: None,
            },
        );
        return Ok(GatewayReconciliationDecision::FrozenQuarantined {
            quarantine_receipt,
            receipt_version: attestation.receipt_version,
        });
    }

    let (reported_seq, reported_usage) = match attestation.disposition {
        GatewayOutcomeDisposition::EffectClosed {
            final_seq,
            final_usage,
        } => (final_seq, final_usage),
        GatewayOutcomeDisposition::ConfirmedNoEffect => (0, 0),
        GatewayOutcomeDisposition::StillUncertainQuarantine => unreachable!("handled above"),
    };

    // [BUD-CLOSE-001]: Enforce monotonic high-water & upper-bound invariants before any projection or settlement.
    if reported_seq < reservation.usage_high_water_seq
        || reported_usage < reservation.usage_high_water
    {
        return Err(GatewayReconcileError::BelowHighWaterMark {
            high_water_seq: reservation.usage_high_water_seq,
            high_water_usage: reservation.usage_high_water,
            reported_seq,
            reported_usage,
        });
    }
    if reported_usage > reservation.upper_bound {
        return Err(GatewayReconcileError::ExceedsUpperBound {
            upper_bound: reservation.upper_bound,
            reported_usage,
        });
    }

    // Gate 5B: Provisional finality -> record projected audit state ONLY ([BUD-REBATE-001]).
    if attestation.finality == ReceiptFinality::Provisional {
        let audit = ProjectedReconciliationAudit {
            reservation_id: attestation.reservation_id,
            operation_id: attestation.operation_id,
            receipt_version: attestation.receipt_version,
            projected_seq: reported_seq,
            projected_usage: reported_usage,
            projected_rebate: reservation.upper_bound - reported_usage,
            recorded_at_ms: ctx.verified_at_ms,
            attestation_digest: signing_digest,
        };
        ctx.ledger.reservations.insert(
            key_bytes,
            ReservationReconcileState {
                latest_version: attestation.receipt_version,
                latest_attestation: *attestation,
                projected_audit: Some(audit),
                non_terminal: Some(NonTerminalEntry::Provisional(audit)),
                terminal_settlement: None,
            },
        );
        return Ok(GatewayReconciliationDecision::ProjectedProvisional(audit));
    }

    // Gate 5C: FinalNonSupersedeable -> settle ResourceAuthority + converge Task plan (R-B Bounded Settlement).
    let now_i64 = if ctx.task_commit_plan.is_some() {
        Some(
            i64::try_from(ctx.verified_at_ms)
                .map_err(|_| GatewayReconcileError::InvalidTimestamp(ctx.verified_at_ms))?,
        )
    } else {
        None
    };

    let finalization_receipt = ctx
        .resource_authority
        .finalize_reservation(FinalizeReservationRequest {
            reservation_id: attestation.reservation_id,
            operation_id: attestation.operation_id,
            activation_receipt_id,
            effect_closed_proof_digest: attestation.effect_closed_proof_digest(),
            final_seq: reported_seq,
            final_usage: reported_usage,
            finalized_at_ms: attestation.issued_at_ms,
        })?
        .receipt();

    let task_convergence =
        if let (Some((task_auth, plan_id)), Some(converge_ms)) = (ctx.task_commit_plan, now_i64) {
            Some(task_auth.converge_resource_commit_plan(
                ctx.resource_authority,
                plan_id,
                converge_ms,
            )?)
        } else {
            None
        };

    let outcome = GatewaySettlementOutcome {
        reservation_id: attestation.reservation_id,
        operation_id: attestation.operation_id,
        receipt_version: attestation.receipt_version,
        finalization_receipt,
        task_convergence,
    };
    ctx.ledger.reservations.insert(
        key_bytes,
        ReservationReconcileState {
            latest_version: attestation.receipt_version,
            latest_attestation: *attestation,
            projected_audit: None,
            non_terminal: None,
            terminal_settlement: Some(outcome.clone()),
        },
    );
    Ok(GatewayReconciliationDecision::SettledFinal(outcome))
}
