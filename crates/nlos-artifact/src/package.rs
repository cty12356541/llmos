//! Minimal signed Package envelope verification (B-ARTIFACT-003).
//!
//! This is only the smallest §23.2 prefix, not the full Package model: a
//! [`PackageManifest`] declares entries (name + role + content digest), the
//! acting principal signs the domain-separated canonical manifest digest
//! with Ed25519, and [`ArtifactStore::verify_package`] binds every declared
//! digest to the artifact store's actual content head. A successful
//! verification leaves one immutable `package_verification_receipts` row
//! (schema v4).
//!
//! The additive `tasks` template segment face (ADR-0016 决定 1, W28-B) is
//! a parallel shape: a [`SignedPackageWithTasks`] carries the same base
//! manifest plus [`PackageTaskTemplate`]s, signed as one message over
//! [`package_manifest_with_tasks_message`] and verified by
//! [`ArtifactStore::verify_package_with_tasks`]. The legacy face above is
//! byte-for-byte untouched (G6: old signed packages verify and install
//! identically), and the two message domains make cross-face signature
//! reuse impossible.
//!
//! Verification is fail-closed in a fixed order (both faces share one
//! pipeline; only the shape validation and the signed digest differ):
//!
//! 1. Manifest shape validation (bounded entry names, non-empty, unique
//!    names; the templated face additionally validates the task-template
//!    segment shape via [`validate_task_templates`]).
//! 2. Idempotent replay: an existing receipt for the caller's idempotency
//!    key is the durable authority and replays **without re-verification**
//!    (ADR-0010 replay precedent), so a receipt stays replayable even after
//!    the signing key is later revoked.
//! 3. Signature verification through `nlos-identity` under the signing
//!    principal's *current* key binding: unknown principal, revoked key,
//!    and invalid signature are typed fail-closed errors before any durable
//!    write (`PackagePrincipalUnknown` / `PackageKeyRevoked` /
//!    `PackageSignatureInvalid`, mirroring ADR-0010).
//! 4. Content binding: every entry's declared digest must equal the current
//!    head digest of its artifact; reads and the receipt insert share one
//!    `BEGIN IMMEDIATE` transaction, so a concurrent head advance cannot
//!    slip between the check and the commit. Any mismatch is a typed
//!    `PackageTampered` (or `ArtifactNotFound`) with zero durable state.
//!
//! Explicitly out of scope for later Slice K slices: installation/update
//! lifecycle, the full §23.2 manifest (applications, components, imports,
//! exports, resources, data, lifecycle, security), trust-root and
//! signature-chain policy (exactly one signing principal is verified),
//! cross-process transport of the signed envelope, and compiling the task
//! templates into plan proposals (the `nlos-application` template half of
//! W28-B; this crate owns only the manifest face and its shape authority).

use std::collections::HashSet;

use nlos_identity::{
    Ed25519Signature, IdentityAuthority, IdentityAuthorityError,
    VerifyCapabilityCommandSignatureRequest,
};
use nlos_types::{Generation, IdempotencyKey, KeyId, PackageId, PrincipalId, ReceiptId};
use rusqlite::{Row, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::ArtifactError;
use crate::model::ContentDigest;
use crate::query::{SqlRead, load_artifact_optional};
use crate::store::{ArtifactStore, encode_u64};

/// Domain separator for signed Package manifest messages. Framing is
/// canonical: fixed-width big-endian fields, a length-prefixed entry name
/// (the one variable-length field), and an entry count, so no two distinct
/// manifests can produce the same byte stream (mirrors the Capability
/// command message style, extended with length prefixes for names).
const MANIFEST_MESSAGE_DOMAIN: &[u8] = b"llmos/artifact/package-manifest/v1";

/// Domain separator for the task-templated manifest message (the additive
/// `tasks` segment face, ADR-0016 决定 1). The domain is distinct from
/// [`MANIFEST_MESSAGE_DOMAIN`], so a templated digest can never collide
/// with a legacy manifest digest: a signature over one message is useless
/// as a signature over the other, which is exactly the strip/inject
/// tamper boundary the segment needs.
const TASK_TEMPLATED_MANIFEST_DOMAIN: &[u8] = b"llmos/artifact/package-manifest-with-tasks/v1";

/// Structural admission bound on one manifest's task-template segment.
/// Mirrors `nlos-plan`'s `MAX_DECLARED_NODES_PER_REVISION` (a segment
/// compiles 1:1 into plan nodes); each crate owns its own constant so a
/// later plan-side retune never silently moves the manifest face.
pub const MAX_TASK_TEMPLATES_PER_MANIFEST: usize = 100_000;

/// Structural admission bound on one template's dependency edges. Mirrors
/// `nlos-plan`'s `MAX_DEPENDENCIES_PER_NODE`.
pub const MAX_TASK_DEPENDENCIES_PER_TEMPLATE: usize = 256;

/// Role of one manifest entry. A minimal §23.2 component-kind subset; the
/// role is declarative metadata inside the signed digest, not enforced
/// behavior in this slice.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum PackageEntryRole {
    Executable = 1,
    BackgroundService = 2,
    Data = 3,
}

impl PackageEntryRole {
    #[must_use]
    pub const fn encode(self) -> u8 {
        self as u8
    }
}

/// One declared content binding of a [`PackageManifest`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageManifestEntry {
    /// Bounded, NUL-free entry name; unique within the manifest.
    pub name: String,
    /// The artifact whose current content head this entry binds to.
    pub artifact_id: nlos_types::ArtifactId,
    /// The declared content digest; must equal the artifact's actual head
    /// digest at verification time.
    pub digest: ContentDigest,
    /// Declarative role of the entry inside the package.
    pub role: PackageEntryRole,
}

/// Minimal §23.2 subset: package identity, a version, and the declared
/// content entries. Everything else in the §23.2 manifest schema
/// (applications, components, imports, exports, resources, data, lifecycle,
/// security) is a later Slice K slice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageManifest {
    pub package_id: PackageId,
    pub version: u64,
    pub entries: Vec<PackageManifestEntry>,
}

/// A [`PackageManifest`] plus the acting principal's Ed25519 signature over
/// [`package_manifest_message`]. The signature is verified under the
/// signer's *current* identity key binding, so callers never pin a key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignedPackage {
    pub manifest: PackageManifest,
    pub signer: PrincipalId,
    pub signature: Ed25519Signature,
}

/// What a declared template node executes as. The two-value surface and
/// its one-byte wire encoding mirror `nlos-plan`'s `PlanNodeKind`
/// (`AgentRole = 1`, `Executable = 2`); the manifest face keeps its own
/// enum so the signed package schema stays independent of the plan
/// authority's type evolution, while the compiler's total mapping keeps
/// the two faces from drifting apart (`[PLAN-OVERRIDE-001]`).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum PackageTaskKind {
    /// An `AgentRole` binding.
    AgentRole = 1,
    /// An executable binding.
    Executable = 2,
}

impl PackageTaskKind {
    #[must_use]
    pub const fn encode(self) -> u8 {
        self as u8
    }
}

/// One declared task-node template of a package's `tasks` segment
/// (ADR-0016 决定 1). The fields are exactly the declaration fields of
/// the plan face (`nlos-plan`'s `PlanNodeDeclaration`): the segment
/// answers *where a declaration comes from* — signed and immutable with
/// the package — never *what a declaration is*, which stays owned by the
/// plan authority (`[PLAN-OVERRIDE-001]`: no second declaration dialect).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageTaskTemplate {
    /// Declaration-local stable identity, unique within the segment. The
    /// plan authority derives the durable `TaskNodeId` from
    /// `(plan_id, node_key)`, so the same key addresses the same logical
    /// node across plan revisions.
    pub node_key: [u8; 16],
    pub kind: PackageTaskKind,
    /// Digest of the bound `AgentRole`/executable identity.
    pub binding_digest: [u8; 32],
    /// Dependencies by declaration-local `node_key`, in declared order
    /// (declaration order participates in proposal equality exactly as a
    /// direct declaration's order does).
    pub dependency_keys: Vec<[u8; 16]>,
    /// Digest of the input-selector list.
    pub input_selectors_digest: [u8; 32],
    /// Digest of the output contract.
    pub output_contract_digest: [u8; 32],
    /// Digest of the failure/retry/reducer policy.
    pub policy_digest: [u8; 32],
    /// Digest of the requested resource upper bound.
    pub resource_ceiling_digest: [u8; 32],
}

/// A [`PackageManifest`] carrying the additive `tasks` template segment,
/// plus the acting principal's Ed25519 signature over
/// [`package_manifest_with_tasks_message`] — one signature covering the
/// base manifest and the segment as a single message, so neither can be
/// stripped or altered without invalidating it. The struct is flat on
/// purpose: embedding a [`SignedPackage`] would carry a second, legacy
/// signature whose verification would silently drop the segment (a
/// downgrade surface).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignedPackageWithTasks {
    pub manifest: PackageManifest,
    pub tasks: Vec<PackageTaskTemplate>,
    pub signer: PrincipalId,
    pub signature: Ed25519Signature,
}

/// Request to verify one signed package against the artifact store heads.
#[derive(Clone, Copy, Debug)]
pub struct VerifyPackageRequest<'a> {
    pub signed: &'a SignedPackage,
    /// Caller-supplied exactly-once key for the verification receipt.
    pub idempotency_key: IdempotencyKey,
    /// Caller-supplied verification timestamp (ms since Unix epoch); also
    /// gates the signing key's validity window in `nlos-identity`.
    pub verified_at_ms: u64,
}

/// Request to verify one task-templated signed package against the
/// artifact store heads.
#[derive(Clone, Copy, Debug)]
pub struct VerifyPackageWithTasksRequest<'a> {
    pub signed: &'a SignedPackageWithTasks,
    /// Caller-supplied exactly-once key for the verification receipt.
    pub idempotency_key: IdempotencyKey,
    /// Caller-supplied verification timestamp (ms since Unix epoch); also
    /// gates the signing key's validity window in `nlos-identity`.
    pub verified_at_ms: u64,
}

/// Immutable proof that a signed package was verified against artifact
/// heads at `verified_at_ms` under the recorded key binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageVerificationReceipt {
    pub receipt_id: ReceiptId,
    /// The exact domain-separated manifest digest that was signed and
    /// verified.
    pub manifest_digest: ContentDigest,
    pub package_id: PackageId,
    pub package_version: u64,
    pub entry_count: u64,
    pub signer: PrincipalId,
    pub key_id: KeyId,
    pub key_generation: Generation,
    /// The verified signature bytes; replay equality is decided on
    /// (manifest digest, signer, signature).
    pub signature: Ed25519Signature,
    pub verified_at_ms: u64,
}

/// Outcome of [`ArtifactStore::verify_package`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PackageVerificationDecision {
    /// The signature and every content binding were verified by this call
    /// and the receipt was committed.
    Verified(PackageVerificationReceipt),
    /// The same signed package (same idempotency key, manifest digest,
    /// signer, signature) was already verified; the durable receipt is
    /// replayed unchanged without re-verification.
    Replayed(PackageVerificationReceipt),
}

impl PackageVerificationDecision {
    #[must_use]
    pub const fn receipt(&self) -> &PackageVerificationReceipt {
        match self {
            Self::Verified(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// Computes the exact domain-separated digest signed by a package signer.
///
/// Framing: domain, `package_id`, version (u64 BE), entry count (u64 BE),
/// then per entry the name length (u64 BE), name bytes, artifact id,
/// content digest, and role byte. No two distinct manifests can produce the
/// same message bytes.
#[must_use]
pub fn package_manifest_message(manifest: &PackageManifest) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(MANIFEST_MESSAGE_DOMAIN);
    hasher.update(manifest.package_id.as_bytes());
    hasher.update(manifest.version.to_be_bytes());
    hasher.update(
        u64::try_from(manifest.entries.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for entry in &manifest.entries {
        hasher.update(
            u64::try_from(entry.name.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        hasher.update(entry.name.as_bytes());
        hasher.update(entry.artifact_id.as_bytes());
        hasher.update(entry.digest.as_bytes());
        hasher.update([entry.role.encode()]);
    }
    hasher.finalize().into()
}

/// Computes the exact domain-separated digest signed by the signer of a
/// [`SignedPackageWithTasks`].
///
/// Framing: the templated domain, the *legacy* [`package_manifest_message`]
/// digest of the base manifest (the segment chains the already-canonical
/// base digest, so the base framing exists in exactly one place), then the
/// task count (u64 BE) and, per template in declared order, the node key,
/// kind byte, binding digest, a length-prefixed dependency-key list (u64 BE
/// count, keys in declared order), and the selector/contract/policy/ceiling
/// digests. No two distinct templated packages can produce the same
/// message bytes, and no templated digest can collide with a legacy one
/// (distinct domains).
#[must_use]
pub fn package_manifest_with_tasks_message(
    manifest: &PackageManifest,
    tasks: &[PackageTaskTemplate],
) -> [u8; 32] {
    let base = package_manifest_message(manifest);
    let mut hasher = Sha256::new();
    hasher.update(TASK_TEMPLATED_MANIFEST_DOMAIN);
    hasher.update(base);
    hasher.update(u64::try_from(tasks.len()).unwrap_or(u64::MAX).to_be_bytes());
    for template in tasks {
        hasher.update(template.node_key);
        hasher.update([template.kind.encode()]);
        hasher.update(template.binding_digest);
        hasher.update(
            u64::try_from(template.dependency_keys.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        for dependency in &template.dependency_keys {
            hasher.update(*dependency);
        }
        hasher.update(template.input_selectors_digest);
        hasher.update(template.output_contract_digest);
        hasher.update(template.policy_digest);
        hasher.update(template.resource_ceiling_digest);
    }
    hasher.finalize().into()
}

/// Validates the `tasks` segment shape: non-empty, within the admission
/// bounds, unique node keys, no self-dependencies, and every dependency
/// resolving to a key declared in the same segment.
///
/// Deliberately *not* checked here: dependency cycles. DAG admission is
/// the plan authority's single-owner rule (`[PLAN-DAG-001]`); the manifest
/// face guarantees reference-locality only, and a compiled proposal flows
/// through the plan authority's own admission like any direct declaration.
/// This function is the one shape authority shared by
/// [`ArtifactStore::verify_package_with_tasks`] and the `nlos-application`
/// template compiler, so the two faces cannot drift apart.
///
/// # Errors
///
/// Returns [`ArtifactError::PackageManifestInvalid`] for every violation
/// above.
pub fn validate_task_templates(tasks: &[PackageTaskTemplate]) -> Result<(), ArtifactError> {
    if tasks.is_empty() {
        return Err(ArtifactError::PackageManifestInvalid(
            "task template segment must declare at least one template",
        ));
    }
    if tasks.len() > MAX_TASK_TEMPLATES_PER_MANIFEST {
        return Err(ArtifactError::PackageManifestInvalid(
            "too many task templates",
        ));
    }
    let mut seen = HashSet::with_capacity(tasks.len());
    for template in tasks {
        if !seen.insert(template.node_key) {
            return Err(ArtifactError::PackageManifestInvalid(
                "duplicate task template node key",
            ));
        }
        if template.dependency_keys.len() > MAX_TASK_DEPENDENCIES_PER_TEMPLATE {
            return Err(ArtifactError::PackageManifestInvalid(
                "task template dependency set exceeds the admission bound",
            ));
        }
        for dependency in &template.dependency_keys {
            if *dependency == template.node_key {
                return Err(ArtifactError::PackageManifestInvalid(
                    "task template depends on itself",
                ));
            }
        }
    }
    for template in tasks {
        for dependency in &template.dependency_keys {
            if !seen.contains(dependency) {
                return Err(ArtifactError::PackageManifestInvalid(
                    "task template dependency references an undeclared node key",
                ));
            }
        }
    }
    Ok(())
}

impl ArtifactStore {
    /// Verifies a signed package: manifest shape, the signer's current
    /// identity key binding (`nlos-identity`), and every entry's content
    /// binding against the artifact store heads. On success commits one
    /// immutable verification receipt.
    ///
    /// The dependency direction is authority-to-authority (artifact
    /// consumes identity verification), mirroring capability → identity.
    ///
    /// # Errors
    ///
    /// Typed fail-closed, before any durable write:
    /// [`ArtifactError::PackageManifestInvalid`] for malformed manifests,
    /// [`ArtifactError::PackagePrincipalUnknown`] /
    /// [`ArtifactError::PackageKeyRevoked`] /
    /// [`ArtifactError::PackageSignatureInvalid`] for signature failures
    /// (ADR-0010 semantics), [`ArtifactError::PackageIdentity`] for other
    /// identity-authority failures, [`ArtifactError::ArtifactNotFound`] for
    /// an entry naming an unknown artifact, and
    /// [`ArtifactError::PackageTampered`] when a declared digest does not
    /// match the artifact's actual content head (including an artifact with
    /// no head yet). Replays return
    /// [`PackageVerificationDecision::Replayed`] without re-verification.
    pub fn verify_package(
        &self,
        identity: &IdentityAuthority,
        request: VerifyPackageRequest<'_>,
    ) -> Result<PackageVerificationDecision, ArtifactError> {
        validate_manifest(&request.signed.manifest)?;
        let manifest_digest =
            ContentDigest::from_bytes(package_manifest_message(&request.signed.manifest));
        self.verify_signed_package(
            identity,
            &PackageVerificationCore {
                manifest: &request.signed.manifest,
                manifest_digest,
                signer: request.signed.signer,
                signature: request.signed.signature,
                idempotency_key: request.idempotency_key,
                verified_at_ms: request.verified_at_ms,
            },
        )
    }

    /// Verifies a task-templated signed package: base manifest shape,
    /// `tasks` segment shape, the signer's current identity key binding
    /// (`nlos-identity`) over the combined
    /// [`package_manifest_with_tasks_message`] digest, and every entry's
    /// content binding against the artifact store heads. On success
    /// commits one immutable verification receipt in the same table and
    /// shape as the legacy face — the receipt's `manifest_digest` is the
    /// templated digest, which covers the base manifest and the segment
    /// as one message.
    ///
    /// This is the additive face of ADR-0016 决定 1: the legacy
    /// [`ArtifactStore::verify_package`] path is untouched (old signed
    /// packages verify byte-identically, G6), and the two domains make a
    /// signature over one face useless over the other — a segment cannot
    /// be stripped from or injected into a verified package.
    ///
    /// # Errors
    ///
    /// Same typed fail-closed set as [`ArtifactStore::verify_package`],
    /// plus [`ArtifactError::PackageManifestInvalid`] for malformed task
    /// template segments.
    pub fn verify_package_with_tasks(
        &self,
        identity: &IdentityAuthority,
        request: VerifyPackageWithTasksRequest<'_>,
    ) -> Result<PackageVerificationDecision, ArtifactError> {
        validate_manifest(&request.signed.manifest)?;
        validate_task_templates(&request.signed.tasks)?;
        let manifest_digest = ContentDigest::from_bytes(package_manifest_with_tasks_message(
            &request.signed.manifest,
            &request.signed.tasks,
        ));
        self.verify_signed_package(
            identity,
            &PackageVerificationCore {
                manifest: &request.signed.manifest,
                manifest_digest,
                signer: request.signed.signer,
                signature: request.signed.signature,
                idempotency_key: request.idempotency_key,
                verified_at_ms: request.verified_at_ms,
            },
        )
    }

    /// The one verification pipeline both signed-package faces share.
    /// Only the manifest digest (and the shape validation that precedes
    /// this call) differs between the faces; content binding, replay, and
    /// the receipt commit are identical.
    fn verify_signed_package(
        &self,
        identity: &IdentityAuthority,
        core: &PackageVerificationCore<'_>,
    ) -> Result<PackageVerificationDecision, ArtifactError> {
        let mut connection = self.lock_connection()?;
        // Replay first, never re-verifying: the durable receipt is the
        // authority (ADR-0010 replay precedent), so a receipt stays
        // replayable even after the signing key is later revoked.
        if let Some(existing) = load_receipt_by_key(&*connection, core.idempotency_key)? {
            if !receipt_replays(&existing, core.manifest_digest, core.signer, core.signature) {
                return Err(ArtifactError::IdempotencyConflict);
            }
            return Ok(PackageVerificationDecision::Replayed(existing));
        }

        // Fresh path: verify under the signer's current key binding before
        // anything else. Unknown signer, revoked key, and bad signature are
        // typed fail-closed errors here.
        let verified = identity
            .verify_capability_command_signature(VerifyCapabilityCommandSignatureRequest {
                message_digest: core.manifest_digest.into_bytes(),
                principal: core.signer,
                signature: core.signature,
                verified_at_ms: core.verified_at_ms,
            })
            .map_err(package_signature_error)?;

        // Content binding + receipt insert share one BEGIN IMMEDIATE
        // transaction: a concurrent head advance cannot slip between the
        // binding check and the commit, and any failure rolls back to zero
        // durable state.
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // A concurrent verification may have committed the same idempotency
        // key while the identity check ran; the durable row wins.
        if let Some(existing) = load_receipt_by_key(&transaction, core.idempotency_key)? {
            if !receipt_replays(&existing, core.manifest_digest, core.signer, core.signature) {
                return Err(ArtifactError::IdempotencyConflict);
            }
            return Ok(PackageVerificationDecision::Replayed(existing));
        }
        for entry in &core.manifest.entries {
            let artifact = load_artifact_optional(&transaction, entry.artifact_id)?
                .ok_or(ArtifactError::ArtifactNotFound(entry.artifact_id))?;
            let actual = if artifact.head_revision == 0 {
                None
            } else {
                artifact.head_digest
            };
            if actual != Some(entry.digest) {
                return Err(ArtifactError::PackageTampered {
                    entry: entry.name.clone(),
                    expected: entry.digest,
                    actual,
                });
            }
        }

        let receipt = PackageVerificationReceipt {
            receipt_id: derive_receipt_id(core.idempotency_key, core.manifest_digest),
            manifest_digest: core.manifest_digest,
            package_id: core.manifest.package_id,
            package_version: core.manifest.version,
            entry_count: u64::try_from(core.manifest.entries.len())
                .map_err(|_| ArtifactError::PackageManifestInvalid("too many entries"))?,
            signer: verified.principal_id(),
            key_id: verified.key_id(),
            key_generation: verified.key_generation(),
            signature: core.signature,
            verified_at_ms: core.verified_at_ms,
        };
        insert_receipt(&transaction, &receipt, core.idempotency_key)?;
        transaction.commit()?;
        Ok(PackageVerificationDecision::Verified(receipt))
    }

    /// Reads one immutable package verification receipt.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactError::PackageVerificationReceiptNotFound`] or a
    /// storage error.
    pub fn inspect_package_verification_receipt(
        &self,
        receipt_id: ReceiptId,
    ) -> Result<PackageVerificationReceipt, ArtifactError> {
        let connection = self.lock_connection()?;
        load_receipt_optional(&*connection, receipt_id)?.ok_or(
            ArtifactError::PackageVerificationReceiptNotFound(receipt_id),
        )
    }
}

/// Validates manifest shape: at least one entry, bounded NUL-free names,
/// unique names. Digest/artifact validity is a binding-time concern.
fn validate_manifest(manifest: &PackageManifest) -> Result<(), ArtifactError> {
    if manifest.entries.is_empty() {
        return Err(ArtifactError::PackageManifestInvalid(
            "package must declare at least one entry",
        ));
    }
    let mut seen = HashSet::with_capacity(manifest.entries.len());
    for entry in &manifest.entries {
        crate::store::validate_text_component("package entry name", &entry.name)
            .map_err(|_| ArtifactError::PackageManifestInvalid("invalid entry name"))?;
        if !seen.insert(entry.name.as_str()) {
            return Err(ArtifactError::PackageManifestInvalid(
                "duplicate entry name",
            ));
        }
    }
    Ok(())
}

/// Everything the shared verification pipeline needs about one
/// signed-package face; only the manifest digest (and the shape
/// validation preceding the core) differs between the legacy face and
/// the task-templated face.
struct PackageVerificationCore<'a> {
    manifest: &'a PackageManifest,
    manifest_digest: ContentDigest,
    signer: PrincipalId,
    signature: Ed25519Signature,
    idempotency_key: IdempotencyKey,
    verified_at_ms: u64,
}

/// Signed entries must carry the declared signer's valid signature; a
/// signature by any other key is rejected even when it verifies
/// cryptographically, because `nlos-identity` resolves the signer's current
/// binding itself.
fn package_signature_error(error: IdentityAuthorityError) -> ArtifactError {
    match error {
        IdentityAuthorityError::InvalidSignature => ArtifactError::PackageSignatureInvalid,
        IdentityAuthorityError::PrincipalNotFound(id) => ArtifactError::PackagePrincipalUnknown(id),
        IdentityAuthorityError::KeyRevoked => ArtifactError::PackageKeyRevoked,
        other => ArtifactError::PackageIdentity(other),
    }
}

/// Replay equality is exactly (manifest digest, signer, signature): the
/// same signed package under the same key. Any other request shape under a
/// reused idempotency key is a conflict.
fn receipt_replays(
    existing: &PackageVerificationReceipt,
    manifest_digest: ContentDigest,
    signer: PrincipalId,
    signature: Ed25519Signature,
) -> bool {
    existing.manifest_digest == manifest_digest
        && existing.signer == signer
        && existing.signature == signature
}

fn derive_receipt_id(key: IdempotencyKey, manifest_digest: ContentDigest) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/artifact-package-verification-receipt/v1");
    hasher.update(key.as_bytes());
    hasher.update(manifest_digest.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    ReceiptId::from_bytes(bytes)
}

fn insert_receipt(
    transaction: &Transaction<'_>,
    receipt: &PackageVerificationReceipt,
    idempotency_key: IdempotencyKey,
) -> Result<(), ArtifactError> {
    transaction.execute(
        "INSERT INTO package_verification_receipts (
            receipt_id, idempotency_key, manifest_digest, package_id,
            package_version, entry_count, signer_principal, signer_key_id,
            signer_key_generation, signature, verified_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            receipt.receipt_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice(),
            receipt.manifest_digest.as_bytes().as_slice(),
            receipt.package_id.as_bytes().as_slice(),
            encode_u64(receipt.package_version)?,
            encode_u64(receipt.entry_count)?,
            receipt.signer.as_bytes().as_slice(),
            receipt.key_id.as_bytes().as_slice(),
            encode_u64(receipt.key_generation.get())?,
            receipt.signature.as_slice(),
            encode_u64(receipt.verified_at_ms)?,
        ],
    )?;
    Ok(())
}

fn load_receipt_by_key(
    source: &impl SqlRead,
    idempotency_key: IdempotencyKey,
) -> Result<Option<PackageVerificationReceipt>, ArtifactError> {
    let mut statement = source.prepare_statement(
        "SELECT receipt_id, manifest_digest, package_id, package_version,
                entry_count, signer_principal, signer_key_id,
                signer_key_generation, signature, verified_at_ms
         FROM package_verification_receipts WHERE idempotency_key = ?1",
    )?;
    let mut rows = statement.query([idempotency_key.as_bytes().as_slice()])?;
    rows.next()?.map(decode_receipt_row).transpose()
}

fn load_receipt_optional(
    source: &impl SqlRead,
    receipt_id: ReceiptId,
) -> Result<Option<PackageVerificationReceipt>, ArtifactError> {
    let mut statement = source.prepare_statement(
        "SELECT receipt_id, manifest_digest, package_id, package_version,
                entry_count, signer_principal, signer_key_id,
                signer_key_generation, signature, verified_at_ms
         FROM package_verification_receipts WHERE receipt_id = ?1",
    )?;
    let mut rows = statement.query([receipt_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_receipt_row).transpose()
}

fn decode_receipt_row(row: &Row<'_>) -> Result<PackageVerificationReceipt, ArtifactError> {
    Ok(PackageVerificationReceipt {
        receipt_id: ReceiptId::from_bytes(blob16(row, 0)?),
        manifest_digest: ContentDigest::from_bytes(blob32(row, 1)?),
        package_id: PackageId::from_bytes(blob16(row, 2)?),
        package_version: decode_u64(row, 3)?,
        entry_count: decode_u64(row, 4)?,
        signer: PrincipalId::from_bytes(blob16(row, 5)?),
        key_id: KeyId::from_bytes(blob16(row, 6)?),
        key_generation: decode_generation(row, 7)?,
        signature: blob64(row, 8)?,
        verified_at_ms: decode_u64(row, 9)?,
    })
}

fn decode_u64(row: &Row<'_>, index: usize) -> Result<u64, ArtifactError> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| ArtifactError::CorruptRecord("negative u64 column"))
}

fn decode_generation(row: &Row<'_>, index: usize) -> Result<Generation, ArtifactError> {
    let value = decode_u64(row, index)?;
    let nonzero = std::num::NonZeroU64::new(value).ok_or(ArtifactError::CorruptRecord(
        "zero package receipt key generation",
    ))?;
    Ok(Generation::new(nonzero))
}

fn blob_n<const N: usize>(row: &Row<'_>, index: usize) -> Result<[u8; N], ArtifactError> {
    let bytes: Vec<u8> = row.get(index)?;
    <[u8; N]>::try_from(bytes.as_slice())
        .map_err(|_| ArtifactError::CorruptRecord("package receipt blob length mismatch"))
}

fn blob16(row: &Row<'_>, index: usize) -> Result<[u8; 16], ArtifactError> {
    blob_n(row, index)
}

fn blob32(row: &Row<'_>, index: usize) -> Result<[u8; 32], ArtifactError> {
    blob_n(row, index)
}

fn blob64(row: &Row<'_>, index: usize) -> Result<[u8; 64], ArtifactError> {
    blob_n(row, index)
}

#[cfg(test)]
mod tests {
    use nlos_types::{ArtifactId, PackageId};

    use super::{
        ContentDigest, PackageEntryRole, PackageManifest, PackageManifestEntry, PackageTaskKind,
        PackageTaskTemplate, package_manifest_message, package_manifest_with_tasks_message,
        validate_task_templates,
    };

    fn manifest_with_names(names: &[&str]) -> PackageManifest {
        PackageManifest {
            package_id: PackageId::from_bytes([0x11; 16]),
            version: 1,
            entries: names
                .iter()
                .map(|name| PackageManifestEntry {
                    name: (*name).to_string(),
                    artifact_id: ArtifactId::from_bytes([0x22; 16]),
                    digest: ContentDigest::of_bytes(b"payload"),
                    role: PackageEntryRole::Executable,
                })
                .collect(),
        }
    }

    /// The length-prefixed name framing must make distinct manifests hash
    /// differently: name boundaries, entry count, entry order, and roles all
    /// participate in the digest.
    #[test]
    fn manifest_message_framing_is_canonical() {
        let base = manifest_with_names(&["alpha", "beta"]);
        let digest = package_manifest_message(&base);
        assert_eq!(digest, package_manifest_message(&base), "deterministic");

        // Name boundary shift: ("ab", "c") vs ("a", "bc").
        let shifted = manifest_with_names(&["ab", "c"]);
        assert_ne!(package_manifest_message(&shifted), digest);

        // Entry order participates.
        let reordered = manifest_with_names(&["beta", "alpha"]);
        assert_ne!(package_manifest_message(&reordered), digest);

        // Role participates.
        let mut role_flipped = manifest_with_names(&["alpha", "beta"]);
        role_flipped.entries[0].role = PackageEntryRole::Data;
        assert_ne!(package_manifest_message(&role_flipped), digest);

        // Version participates.
        let mut version_flipped = manifest_with_names(&["alpha", "beta"]);
        version_flipped.version = 2;
        assert_ne!(package_manifest_message(&version_flipped), digest);
    }

    fn template(node_key: [u8; 16], kind: PackageTaskKind) -> PackageTaskTemplate {
        PackageTaskTemplate {
            node_key,
            kind,
            binding_digest: ContentDigest::of_bytes(b"binding").into_bytes(),
            dependency_keys: vec![],
            input_selectors_digest: ContentDigest::of_bytes(b"selectors").into_bytes(),
            output_contract_digest: ContentDigest::of_bytes(b"contract").into_bytes(),
            policy_digest: ContentDigest::of_bytes(b"policy").into_bytes(),
            resource_ceiling_digest: ContentDigest::of_bytes(b"ceiling").into_bytes(),
        }
    }

    /// The templated message must chain the legacy digest and make every
    /// declared template field participate: kind, binding, dependency set
    /// and order, body digests, template count, and template order. The
    /// templated digest of any segment also differs from the legacy digest
    /// of the same base manifest (distinct domains, no cross-face reuse).
    #[test]
    fn templated_message_framing_is_canonical() {
        let base = manifest_with_names(&["alpha"]);
        let standalone = template([0x01; 16], PackageTaskKind::AgentRole);
        let digest = package_manifest_with_tasks_message(&base, std::slice::from_ref(&standalone));
        assert_eq!(
            digest,
            package_manifest_with_tasks_message(&base, std::slice::from_ref(&standalone)),
            "deterministic"
        );
        assert_ne!(
            digest,
            package_manifest_message(&base),
            "templated and legacy digests live in distinct domains"
        );

        // Base manifest changes move the templated digest too (chaining).
        let mut version_flipped = base.clone();
        version_flipped.version = 2;
        assert_ne!(
            package_manifest_with_tasks_message(
                &version_flipped,
                std::slice::from_ref(&standalone)
            ),
            digest
        );

        // Kind participates.
        let kind_flipped = template([0x01; 16], PackageTaskKind::Executable);
        assert_ne!(
            package_manifest_with_tasks_message(&base, &[kind_flipped]),
            digest
        );

        // Every digest-bound body participates.
        let mut policy_flipped = standalone.clone();
        policy_flipped.policy_digest = ContentDigest::of_bytes(b"other-policy").into_bytes();
        assert_ne!(
            package_manifest_with_tasks_message(&base, &[policy_flipped]),
            digest
        );

        // Node key, dependency set, dependency order, template count, and
        // template order all participate.
        let mut with_dependency = standalone.clone();
        with_dependency.dependency_keys = vec![[0x02; 16]];
        assert_ne!(
            package_manifest_with_tasks_message(&base, std::slice::from_ref(&with_dependency)),
            digest
        );
        let mut dependency_reordered = standalone.clone();
        dependency_reordered.node_key = [0x03; 16];
        dependency_reordered.dependency_keys = vec![[0x02; 16], [0x04; 16]];
        let ordered = package_manifest_with_tasks_message(&base, &[dependency_reordered.clone()]);
        dependency_reordered.dependency_keys = vec![[0x04; 16], [0x02; 16]];
        assert_ne!(
            package_manifest_with_tasks_message(&base, &[dependency_reordered]),
            ordered,
            "dependency order participates"
        );
        let keyed = template([0x05; 16], PackageTaskKind::AgentRole);
        assert_ne!(
            package_manifest_with_tasks_message(&base, std::slice::from_ref(&keyed)),
            digest
        );
        assert_ne!(
            package_manifest_with_tasks_message(&base, &[keyed.clone(), standalone.clone()]),
            package_manifest_with_tasks_message(&base, &[standalone, keyed]),
            "template order participates"
        );
    }

    #[test]
    fn task_template_shape_validation_matches_the_declared_rules() {
        let ok = template([0x01; 16], PackageTaskKind::AgentRole);
        assert!(validate_task_templates(std::slice::from_ref(&ok)).is_ok());
        assert!(
            validate_task_templates(&[]).is_err(),
            "an empty segment is malformed"
        );
        assert!(
            validate_task_templates(&[ok.clone(), ok.clone()]).is_err(),
            "duplicate node keys are malformed"
        );

        let mut self_dependent = template([0x02; 16], PackageTaskKind::AgentRole);
        self_dependent.dependency_keys = vec![[0x02; 16]];
        assert!(
            validate_task_templates(&[self_dependent]).is_err(),
            "a self-dependency is malformed"
        );

        let mut dangling = template([0x03; 16], PackageTaskKind::AgentRole);
        dangling.dependency_keys = vec![[0x9f; 16]];
        assert!(
            validate_task_templates(&[dangling]).is_err(),
            "a dangling dependency is malformed"
        );
    }
}
