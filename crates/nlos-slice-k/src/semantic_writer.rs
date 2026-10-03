//! The production Semantic write side of the slice (D6 first slice, W49):
//! one dedicated writer principal that turns the payload-execution lane's
//! terminal operation receipt into one admitted Semantic assertion.
//!
//! Composition, nothing invented: every gate the semantic authority already
//! owns (canonical CBOR/EventId binding, signature, execution fence,
//! capability `authorize_semantic`, content digest, lineage, signed
//! admission receipt) runs for real. This module only supplies the writer
//! identity, the root capability, and the deterministic event inputs:
//!
//! * **Key material** ([`SemanticWriterKey`]): `<root>/keys/semantic-writer.key`,
//!   mode `0600`, created once from OS entropy (std's per-instance
//!   `RandomState` mixed with wall time and pid — the workspace carries no
//!   `rand`/`getrandom` dependency), then load-or-create on every reopen.
//!   The production path never derives a seed deterministically; tests
//!   inject keys through [`SemanticWriterKey::from_seed_for_tests`]. The
//!   file stores `seed ‖ valid_from_ms ‖ valid_until_ms` (48 bytes) so the
//!   whole bootstrap request set stays byte-identical across reopens —
//!   every authority replay below compares full request digests.
//! * **Identity**: one `SemanticSigning` principal bootstrapped through the
//!   runtime's identity authority with the bootstrap idempotency key (and
//!   every derived identity) domain-separated from the public key.
//! * **Execution fence**: one isolation domain plus one delegated process
//!   binding, registered so `append_assertion`'s `issuer_execution` gate
//!   resolves an active binding.
//! * **Root capability** ([`Self::application_capability`]): self-issued per
//!   application namespace through `issue_root_signed`, rights exactly
//!   `SEMANTIC_APPEND`, `call_limit = SEMANTIC_WRITER_ROOT_CALL_LIMIT`
//!   (W51: a finite per-application budget, no longer unlimited), validity
//!   the writer key's stored window (taken from the clock wall at key
//!   creation, the widest window that keeps replays byte stable). The
//!   limit is a compiled constant, never runtime configuration: the
//!   issuance request must stay byte-identical across reopens, and a
//!   mutable quota would idempotency-conflict every already-issued root.
//!   When a budget exhausts, the upgrade path is a future delegate/rotate
//!   lane — not editing the constant against live roots. The W49 open
//!   item "the consume ledger stays unwired" closed in W51: the bridge
//!   exercises the capability once per admission through `consume`
//!   *before* appending (see [`Self::append_operation_receipt`]), and the
//!   remaining budget is observable through
//!   [`SliceKRuntime::semantic_writer_budget`]. The other W49 open item
//!   (the `semantic_outbox` row's missing production consumer) closed in
//!   W50: `crate::semantic_stream` is that consumer.
//! * **Bridge** ([`Self::append_operation_receipt`]): one assertion per
//!   terminal payload operation, `FactFromTool` with the driver's terminal
//!   receipt as execution evidence, no lineage parent, and every
//!   replay-sensitive input (`nonce`, `issued_at_unix_ns`, the clock key
//!   behind `admitted_at_ms`, and the capability-consume idempotency key)
//!   derived from `(application_id, installation_generation, entry,
//!   operation_id)` — an exact re-run replays the same `EventId` (and the
//!   same consume receipt) instead of double-writing or double-charging.
//!
//! Fail-closed: any refusal surfaces as a typed [`SliceKError`]; the
//! payload lane propagates it instead of reporting an unrecorded run.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_capability::{
    CapabilityRecord, CapabilityRights, CapabilityTarget, ConsumeCapabilityRequest,
    IssueRootCapabilityRequest, SignedIssueRootCapabilityRequest, issue_root_command_message,
};
use nlos_clock::{AuthorityClock, NowRequest};
use nlos_identity::{
    BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose,
    VerifySemanticSignatureRequest, semantic_signature_message,
};
use nlos_operation::{CompletionOutcome, OperationState};
use nlos_process::{
    CreateIsolationDomainRequest, ProcessAuthority, ProcessBindingRecord,
    RegisterDelegatedProcessRequest,
};
use nlos_semantic::{
    AppendAssertionRequest, AppendDecision, AssertionMode, LocalProcessRef, StoreSigner,
    StoreSignerError, TaintFlags, UnsignedAssertionEvent, content_digest,
    encode_unsigned_assertion_event, semantic_event_id,
};
use nlos_types::{
    ApplicationId, ArtifactId, CallbackId, Generation, IdempotencyKey, NamespaceId, OperationId,
    PackageId, PrincipalId, ReceiptId, SemanticEventId, TaskAttemptId, TaskId,
};
use sha2::{Digest, Sha256};

use crate::error::{SliceKError, SliceKResult};
use crate::runtime::SliceKRuntime;

/// Domain separator of the per-application namespace policy mapping: the
/// v1 policy maps one application to exactly one Semantic namespace by a
/// one-way domain-separated SHA-256 truncation. It is a policy choice, not
/// a general hash: nothing ever maps a namespace back to an application.
pub const APPLICATION_NAMESPACE_DOMAIN: &[u8] = b"llmos/slice-k/semantic-writer/namespace/v1";

/// Domain separator of the writer principal's identity bootstrap inputs.
const WRITER_IDENTITY_DOMAIN: &[u8] = b"llmos/slice-k/semantic-writer/identity/v1";

/// Domain separator of the writer's execution-fence registration inputs.
const WRITER_PROCESS_DOMAIN: &[u8] = b"llmos/slice-k/semantic-writer/process/v1";

/// Domain separator of the per-application root capability idempotency key.
const WRITER_CAPABILITY_DOMAIN: &[u8] = b"llmos/slice-k/semantic-writer/capability/v1";

/// Domain separator of the deterministic per-operation event inputs
/// (`nonce`, `issued_at_unix_ns`, and the clock key behind
/// `admitted_at_ms`): everything an exact re-run must re-derive
/// bit-identically to hit the semantic replay path instead of writing a
/// second event.
const OPERATION_RECEIPT_NONCE_DOMAIN: &[u8] = b"llmos/slice-k/operation-receipt/inputs/v1";

/// Domain separator of the per-operation capability-consume idempotency
/// key: the same replay-critical operation tuple and byte framing as
/// [`OPERATION_RECEIPT_NONCE_DOMAIN`] under a distinct domain string, so
/// one ledger charge per operation tuple replays exactly — it can never
/// collide with (or be replayed as) an event-input derivation, and vice
/// versa.
const OPERATION_RECEIPT_CONSUME_DOMAIN: &[u8] = b"llmos/slice-k/operation-receipt/consume/v1";

/// Domain separator of the canonical operation-receipt content bytes.
const OPERATION_RECEIPT_CONTENT_DOMAIN: &[u8] = b"llmos/slice-k/operation-receipt/v1";

/// Domain separator of the v1 authorization policy digest the bridge
/// records on every admission.
const AUTHZ_POLICY_DOMAIN: &[u8] = b"llmos/slice-k/semantic-authz-policy/v1";

/// Media type of the bridged assertion content: the canonical operation
/// receipt bytes produced by [`encode_operation_receipt_content`].
pub const OPERATION_RECEIPT_MEDIA_TYPE: &str = "application/x-nlos-operation-receipt";

/// Validity span of a production writer key: ten years of milliseconds,
/// saturated at the widest timestamp the durable ledgers store.
const WRITER_VALIDITY_SPAN_MS: u64 = 10 * 365 * 24 * 60 * 60 * 1000;

/// The widest millisecond timestamp the durable ledgers store
/// (`i64::MAX`, ≈ year 2262; `u64::MAX` would not round-trip SQLite).
const WIDEST_STORED_MS: u64 = i64::MAX as u64;

/// Width of the on-disk writer key file: `seed` (32) ‖ `valid_from` (8) ‖
/// `valid_until` (8).
const WRITER_KEY_FILE_BYTES: usize = 48;

/// The root capability's per-application call budget (W51): 1 Mi
/// admitted semantic events per application namespace before
/// [`CapabilityAuthority::consume`] fails closed with
/// `CallLimitExhausted`.
///
/// Deliberately a compiled constant, not configuration: the per-
/// application issuance request (and therefore its idempotent replay)
/// covers `call_limit`, so a runtime-tunable value would conflict with
/// every root an earlier configuration already issued. The limit is also
/// deliberately not stored in the key file — a reopen must rebuild the
/// identical issuance request from the file bytes plus this constant
/// alone. When an application exhausts its budget, the upgrade path is a
/// future delegate/rotate lane (issue a fresh root or delegate a slice of
/// a new one); this constant must not be edited against live roots.
pub const SEMANTIC_WRITER_ROOT_CALL_LIMIT: u64 = 1 << 20;

/// The v1 authorization policy digest recorded on every bridged admission.
fn authz_policy_digest() -> [u8; 32] {
    hash32(AUTHZ_POLICY_DOMAIN, &[])
}

/// The one-way v1 policy mapping from an application identity to its
/// Semantic namespace.
#[must_use]
pub fn application_namespace(application_id: ApplicationId) -> NamespaceId {
    NamespaceId::from_bytes(hash16(
        APPLICATION_NAMESPACE_DOMAIN,
        &[application_id.as_bytes()],
    ))
}

/// The per-application purpose digest the root capability and every
/// bridged event under it carry (capability admission requires an exact
/// purpose match, so this binds the capability to the application too).
fn application_purpose_digest(application_id: ApplicationId) -> [u8; 32] {
    hash32(
        APPLICATION_NAMESPACE_DOMAIN,
        &[&application_purpose_input(application_id)],
    )
}

fn application_purpose_input(application_id: ApplicationId) -> [u8; 17] {
    let mut input = [0_u8; 17];
    input[..16].copy_from_slice(application_id.as_bytes());
    input[16] = 0x01;
    input
}

fn hash32(domain: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn hash16(domain: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let digest = hash32(domain, parts);
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest[..16]);
    id
}

/// Reads exactly 8 bytes as a big-endian `u64`; callers guarantee the
/// slice width (the key-file layout check ran first).
fn be_u64(bytes: &[u8]) -> u64 {
    let mut wide = [0_u8; 8];
    wide.copy_from_slice(bytes);
    u64::from_be_bytes(wide)
}

/// The durable writer key material: an Ed25519 seed plus the validity
/// window every derived bootstrap/issuance request reuses, so a reopen
/// replays byte-identical requests instead of conflicting with itself.
/// The root capability's call budget rides along as a derivation input,
/// never as a file byte: production keys carry
/// [`SEMANTIC_WRITER_ROOT_CALL_LIMIT`], and only the test-injection
/// constructors below may carry anything else.
#[derive(Clone)]
pub struct SemanticWriterKey {
    seed: [u8; 32],
    valid_from_ms: u64,
    valid_until_ms: u64,
    root_call_limit: u64,
}

impl SemanticWriterKey {
    /// Loads `<root>/keys/semantic-writer.key`, creating it once from OS
    /// entropy when absent. `now_ms` is consulted only on creation (the
    /// window's start is the clock wall of the creating moment) and may
    /// therefore take a durable clock reading.
    ///
    /// # Errors
    ///
    /// Fails typed with [`SliceKError::SemanticWriter`] when the file
    /// exists with a different width, or with [`SliceKError::Io`] when the
    /// filesystem refuses the read/write/permission steps.
    pub fn load_or_create(
        path: &Path,
        now_ms: impl FnOnce() -> SliceKResult<u64>,
    ) -> SliceKResult<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                if bytes.len() != WRITER_KEY_FILE_BYTES {
                    return Err(SliceKError::SemanticWriter(
                        "semantic-writer.key must be exactly 48 bytes (seed ‖ valid_from ‖ valid_until)",
                    ));
                }
                let mut seed = [0_u8; 32];
                seed.copy_from_slice(&bytes[..32]);
                let key = Self {
                    seed,
                    valid_from_ms: be_u64(&bytes[32..40]),
                    valid_until_ms: be_u64(&bytes[40..48]),
                    root_call_limit: SEMANTIC_WRITER_ROOT_CALL_LIMIT,
                };
                restrict_to_owner(path);
                Ok(key)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let key = Self::fresh(now_ms()?);
                key.write_file(path)?;
                restrict_to_owner(path);
                Ok(key)
            }
            Err(error) => Err(SliceKError::Io(error)),
        }
    }

    /// Test/fixture injection: a deterministic seed with the widest
    /// validity window and the production call budget. Production code
    /// never calls this — the production seed comes from
    /// [`Self::load_or_create`] only.
    #[must_use]
    pub fn from_seed_for_tests(seed: [u8; 32]) -> Self {
        Self {
            seed,
            valid_from_ms: 0,
            valid_until_ms: WIDEST_STORED_MS,
            root_call_limit: SEMANTIC_WRITER_ROOT_CALL_LIMIT,
        }
    }

    /// Test/fixture injection of the same family as
    /// [`Self::from_seed_for_tests`], with an overridden root-capability
    /// call budget — the only way to observe a budget exhausting inside a
    /// test (production roots always carry
    /// [`SEMANTIC_WRITER_ROOT_CALL_LIMIT`]). The limit must be non-zero:
    /// the capability authority's bounds gate refuses `Some(0)` typed at
    /// issuance.
    #[must_use]
    pub fn from_seed_with_root_call_limit_for_tests(seed: [u8; 32], root_call_limit: u64) -> Self {
        Self {
            seed,
            valid_from_ms: 0,
            valid_until_ms: WIDEST_STORED_MS,
            root_call_limit,
        }
    }

    /// The writer's Ed25519 public key.
    #[must_use]
    pub fn public_key(&self) -> [u8; 32] {
        SigningKey::from_bytes(&self.seed)
            .verifying_key()
            .to_bytes()
    }

    fn fresh(now_ms: u64) -> Self {
        let valid_until_ms = now_ms
            .saturating_add(WRITER_VALIDITY_SPAN_MS)
            .min(WIDEST_STORED_MS);
        Self {
            seed: os_random_seed(),
            valid_from_ms: now_ms,
            valid_until_ms,
            root_call_limit: SEMANTIC_WRITER_ROOT_CALL_LIMIT,
        }
    }

    fn write_file(&self, path: &Path) -> SliceKResult<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut bytes = Vec::with_capacity(WRITER_KEY_FILE_BYTES);
        bytes.extend_from_slice(&self.seed);
        bytes.extend_from_slice(&self.valid_from_ms.to_be_bytes());
        bytes.extend_from_slice(&self.valid_until_ms.to_be_bytes());
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let written = match options.open(path) {
            Ok(mut file) => {
                use std::io::Write as _;
                file.write_all(&bytes)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // A concurrent creator won the create_new race; the file on
                // disk is the winner either way.
                return Ok(());
            }
            Err(error) => return Err(SliceKError::Io(error)),
        };
        written.map_err(SliceKError::Io)
    }
}

/// One OS-entropy seed for a fresh writer key. The workspace carries no
/// `rand`/`getrandom` dependency, so entropy comes from std's per-instance
/// `RandomState` (OS-seeded `SipHash` keys, a fresh pair per call) mixed
/// with the wall clock and the process id — never from a fixed seed.
fn os_random_seed() -> [u8; 32] {
    let nanos = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0_u128, |elapsed| elapsed.as_nanos()),
    )
    .unwrap_or(0);
    let pid = u64::from(std::process::id());
    let mut seed = [0_u8; 32];
    for (index, chunk) in seed.chunks_mut(8).enumerate() {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write(b"llmos/slice-k/semantic-writer-key/v1");
        hasher.write(&nanos.to_be_bytes());
        hasher.write(&pid.wrapping_add(index as u64).to_be_bytes());
        chunk.copy_from_slice(&hasher.finish().to_be_bytes());
    }
    seed
}

#[cfg_attr(not(unix), allow(unused_variables))]
fn restrict_to_owner(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o600);
            let _ = std::fs::set_permissions(path, permissions);
        }
    }
}

/// The terminal-receipt fact one payload execution asserts: "operation X
/// terminated with outcome Y". Every field is authority-sourced by the
/// payload lane; the deterministic content encoding below fixes their
/// byte image.
pub struct OperationReceiptFact<'a> {
    pub application_id: ApplicationId,
    pub package_id: PackageId,
    pub package_version: u64,
    pub installation_generation: Generation,
    pub entry_name: &'a str,
    pub artifact_id: ArtifactId,
    pub payload_revision: u64,
    pub payload_digest: [u8; 32],
    pub payload_size_bytes: u64,
    pub operation_id: OperationId,
    pub operation_generation: Generation,
    pub callback_id: CallbackId,
    pub outcome: CompletionOutcome,
    pub terminal_state: OperationState,
}

/// The semantic admission outcome of one bridged receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationReceiptAppend {
    pub event_id: SemanticEventId,
    pub admission_receipt_id: ReceiptId,
    pub log_seq: u64,
    /// `true` when the semantic authority replayed the identical event
    /// instead of admitting a new one (exact re-execution).
    pub replayed: bool,
}

/// Loads (or creates once) the runtime's writer key file under
/// `<keys_dir>/semantic-writer.key`, taking the creation wall reading from
/// the runtime's authority clock under the fixed-purpose key documented on
/// [`SemanticWriter::open`]. Shared by [`SliceKRuntime::open`] and
/// [`SemanticWriter::open`] so both derive the identical file.
pub(crate) fn load_runtime_writer_key(
    clock: &AuthorityClock,
    keys_dir: &Path,
) -> SliceKResult<SemanticWriterKey> {
    SemanticWriterKey::load_or_create(&keys_dir.join("semantic-writer.key"), || {
        let decision = clock.wall_now(NowRequest {
            idempotency_key: IdempotencyKey::from_bytes(hash16(
                WRITER_IDENTITY_DOMAIN,
                &[b"key-creation-clock"],
            )),
        })?;
        Ok(decision.reading().as_u64())
    })
}

/// The dedicated Semantic writer principal of one runtime.
pub struct SemanticWriter {
    key: SigningKey,
    binding: IdentityBinding,
    process_binding: ProcessBindingRecord,
    valid_until_ms: u64,
    /// The per-application root-capability call budget this writer issues
    /// with (production: [`SEMANTIC_WRITER_ROOT_CALL_LIMIT`]; tests may
    /// inject a smaller one through [`SemanticWriterKey`]).
    root_call_limit: u64,
}

impl SemanticWriter {
    /// Assembles the writer the production runtime holds: load-or-create
    /// the key file under `<root>/keys/semantic-writer.key`, then
    /// idempotently bootstrap the principal and its execution fence.
    ///
    /// The creation wall reading rides a fixed-purpose clock key: it is
    /// taken at most once per root (the file is only created when absent),
    /// and the clock's monotonic watermark keeps every later reading —
    /// including every admission timestamp — at or above it, so the key
    /// window stays valid without storing per-creation clock state.
    ///
    /// # Errors
    ///
    /// Fails typed on key-file, clock, identity, or process-authority
    /// refusals.
    pub fn open(runtime: &SliceKRuntime) -> SliceKResult<Self> {
        let key = load_runtime_writer_key(&runtime.clock, &runtime.root().join("keys"))?;
        Self::open_with_key(runtime, &key)
    }

    /// Assembles the writer from an injected key (test/fixture surface;
    /// see [`SemanticWriterKey::from_seed_for_tests`]).
    ///
    /// # Errors
    ///
    /// Fails typed on identity or process-authority refusals.
    pub fn open_with_key(runtime: &SliceKRuntime, key: &SemanticWriterKey) -> SliceKResult<Self> {
        Self::assemble(&runtime.identity, &runtime.process, key)
    }

    /// The shared bootstrap: one `SemanticSigning` principal, one isolation
    /// domain, one delegated process binding — every request derived from
    /// the public key and the stored window, so a reopen replays exactly.
    pub(crate) fn assemble(
        identity: &IdentityAuthority,
        process: &ProcessAuthority,
        key: &SemanticWriterKey,
    ) -> SliceKResult<Self> {
        let signing = SigningKey::from_bytes(&key.seed);
        let public_key = signing.verifying_key().to_bytes();
        let valid_from = key.valid_from_ms;
        let valid_until = key.valid_until_ms;
        let binding = identity
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: hash32(
                    WRITER_IDENTITY_DOMAIN,
                    &[b"profile", &public_key],
                ),
                control_domain_policy_digest: hash32(
                    WRITER_IDENTITY_DOMAIN,
                    &[b"policy", &public_key],
                ),
                public_key,
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: valid_from,
                key_valid_until_ms: valid_until,
                idempotency_key: IdempotencyKey::from_bytes(hash16(
                    WRITER_IDENTITY_DOMAIN,
                    &[b"bootstrap", &public_key],
                )),
                created_at_ms: valid_from,
            })?
            .binding();
        let domain = process
            .create_isolation_domain(CreateIsolationDomainRequest {
                policy_digest: hash32(WRITER_PROCESS_DOMAIN, &[b"domain-policy", &public_key]),
                idempotency_key: IdempotencyKey::from_bytes(hash16(
                    WRITER_PROCESS_DOMAIN,
                    &[b"domain", &public_key],
                )),
                created_at_ms: valid_from,
            })?
            .record()
            .clone();
        let process_binding = process
            .register_delegated_process(RegisterDelegatedProcessRequest {
                task_id: TaskId::from_bytes(hash16(WRITER_PROCESS_DOMAIN, &[b"task", &public_key])),
                task_attempt_id: TaskAttemptId::from_bytes(hash16(
                    WRITER_PROCESS_DOMAIN,
                    &[b"attempt", &public_key],
                )),
                attempt_generation: Generation::INITIAL,
                isolation_domain_id: domain.isolation_domain_id,
                isolation_domain_generation: domain.generation,
                isolation_domain_fencing_token: domain.fencing_token,
                idempotency_key: IdempotencyKey::from_bytes(hash16(
                    WRITER_PROCESS_DOMAIN,
                    &[b"process", &public_key],
                )),
                created_at_ms: valid_from,
            })?
            .record()
            .clone();
        Ok(Self {
            key: signing,
            binding,
            process_binding,
            valid_until_ms: valid_until,
            root_call_limit: key.root_call_limit,
        })
    }

    /// The bootstrapped writer principal.
    #[must_use]
    pub fn principal_id(&self) -> PrincipalId {
        self.binding.principal_id
    }

    /// The writer's current semantic signing key id.
    #[must_use]
    pub fn key_id(&self) -> nlos_types::KeyId {
        self.binding.key_id
    }

    /// The writer's control domain.
    #[must_use]
    pub fn control_domain_id(&self) -> nlos_types::ControlDomainId {
        self.binding.control_domain_id
    }

    /// The writer's public key (matches the key file's seed).
    #[must_use]
    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    /// The execution fence every bridged event's `issuer_execution` names.
    #[must_use]
    pub fn process_binding(&self) -> &ProcessBindingRecord {
        &self.process_binding
    }

    /// The per-application call budget this writer's root capabilities
    /// carry (production: [`SEMANTIC_WRITER_ROOT_CALL_LIMIT`]; the
    /// test-injection key constructors may carry a smaller one).
    #[must_use]
    pub const fn root_call_limit(&self) -> u64 {
        self.root_call_limit
    }

    /// Idempotently issues (or replays) the writer's per-application root
    /// capability: self-signed, `SEMANTIC_APPEND` only, call budget
    /// [`Self::root_call_limit`] (production:
    /// [`SEMANTIC_WRITER_ROOT_CALL_LIMIT`], W51 — the consume ledger the
    /// bridge charges lives behind this limit), target the application's
    /// v1 policy namespace.
    ///
    /// # Errors
    ///
    /// Propagates capability-authority refusals typed.
    pub fn application_capability(
        &self,
        runtime: &SliceKRuntime,
        application_id: ApplicationId,
    ) -> SliceKResult<CapabilityRecord> {
        let command = IssueRootCapabilityRequest {
            issuer_key_id: self.binding.key_id,
            holder_key_id: self.binding.key_id,
            target: CapabilityTarget::Namespace(application_namespace(application_id)),
            rights: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: Some(application_purpose_digest(application_id)),
            valid_from_ms: self.binding.key_valid_from_ms,
            valid_until_ms: self.valid_until_ms,
            delegation_depth_remaining: 0,
            call_limit: Some(self.root_call_limit),
            idempotency_key: IdempotencyKey::from_bytes(hash16(
                WRITER_CAPABILITY_DOMAIN,
                &[
                    &self.key.verifying_key().to_bytes(),
                    application_id.as_bytes(),
                ],
            )),
            issued_at_ms: self.binding.key_valid_from_ms,
        };
        let signature = self
            .key
            .sign(&issue_root_command_message(command))
            .to_bytes();
        let decision = runtime.capability().issue_root_signed(
            &runtime.identity,
            SignedIssueRootCapabilityRequest {
                command,
                signer: self.binding.principal_id,
                signature,
            },
        )?;
        Ok(decision.record())
    }

    /// Bridges one terminal payload-operation receipt into exactly one
    /// Semantic assertion (admitting or replaying; never double-writing),
    /// charging the application's capability budget exactly once per
    /// operation tuple through the consume ledger (never double-charging).
    ///
    /// # Errors
    ///
    /// Fail-closed typed: capability issuance, signature, canonical, or
    /// admission refusals propagate as [`SliceKError`] variants — the
    /// caller reports no run the ledger did not record. An exhausted
    /// per-application budget refuses before any semantic write as
    /// `SliceKError::Capability(CapabilityAuthorityError::CallLimitExhausted)`.
    pub fn append_operation_receipt(
        &self,
        runtime: &SliceKRuntime,
        fact: &OperationReceiptFact<'_>,
    ) -> SliceKResult<OperationReceiptAppend> {
        let capability = self.application_capability(runtime, fact.application_id)?;
        let content = encode_operation_receipt_content(fact);
        let event = UnsignedAssertionEvent {
            scope: capability.target,
            issuer: self.binding.principal_id,
            issuer_execution: LocalProcessRef {
                process_id: self.process_binding.process_id,
                generation: self.process_binding.process_generation,
            },
            control_domain: self.binding.control_domain_id,
            issued_at_unix_ns: receipt_input_u64(fact, b"issued-at"),
            nonce: event_input(fact, b"nonce")[..16].to_vec(),
            declared_parents: Vec::new(),
            declassification_receipt_id: None,
            valid_until_ms: Some(self.valid_until_ms),
            purpose_digest: capability.purpose_digest,
            content_digest: content_digest(OPERATION_RECEIPT_MEDIA_TYPE, &content)
                .map_err(SliceKError::Semantic)?,
            assertion_mode: AssertionMode::FactFromTool,
            execution_evidence_receipt_id: Some(outcome_receipt_id(fact.outcome)),
            confidence_bp: None,
            key_id: self.binding.key_id,
        };
        let canonical_unsigned_event =
            encode_unsigned_assertion_event(&event).map_err(SliceKError::Semantic)?;
        let claimed_event_id = semantic_event_id(&canonical_unsigned_event);
        let signature = self
            .key
            .sign(&semantic_signature_message(claimed_event_id))
            .to_bytes();
        // The admission timestamp rides the authority clock under a
        // replay-stable key derived from the same operation tuple: an
        // exact re-run replays the identical reading (and receipt).
        let admitted_at_ms =
            runtime.wall_now_ms(receipt_idempotency_key(fact, b"admission-clock"))?;
        // Consume-before-append (W51): exercise the root capability once
        // per admission, BEFORE any semantic write — an exhausted budget
        // fails the bridge closed here with zero semantic state. The
        // charge carries the same handle/target/right/purpose the
        // admission's own `authorize_semantic` gate will check, and its
        // idempotency key derives from the same operation tuple as the
        // event inputs under a distinct domain: an exact re-run, a reopen
        // re-run, or a retry after a failed append replays the same
        // consumption receipt and never charges twice. The signer proof
        // is the exact signature over the claimed event id the semantic
        // admission verifies below, at the same authority time.
        let consume_signer =
            runtime
                .identity
                .verify_semantic_signature(VerifySemanticSignatureRequest {
                    event_id: claimed_event_id,
                    issuer: self.binding.principal_id,
                    control_domain_id: self.binding.control_domain_id,
                    key_id: self.binding.key_id,
                    signature,
                    admitted_at_ms,
                })?;
        runtime.capability().consume(ConsumeCapabilityRequest {
            handle: capability.handle,
            signer: consume_signer,
            target: capability.target,
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: capability.purpose_digest,
            idempotency_key: receipt_consume_idempotency_key(fact),
            consumed_at_ms: admitted_at_ms,
        })?;
        let request = AppendAssertionRequest {
            canonical_unsigned_event,
            claimed_event_id,
            signature,
            capability: capability.handle,
            content_media_type: OPERATION_RECEIPT_MEDIA_TYPE.to_owned(),
            content_bytes: content,
            captured_inputs: Vec::new(),
            ingress_taint: TaintFlags::default(),
            authz_policy_digest: authz_policy_digest(),
            admission_limit_ms: None,
            admitted_at_ms,
        };
        let decision = runtime.semantic().append_assertion(
            &runtime.identity,
            runtime.capability(),
            &runtime.process,
            self,
            &request,
        )?;
        // Every admission wrote a semantic_outbox row: hint a running
        // semantic stream pump (see `crate::semantic_stream`) so the
        // notification does not wait for the fallback poll. The hint is
        // bounded and best-effort, exactly like the payload lane's
        // outbox-pump hint.
        let _ = runtime.hint_semantic_stream();
        let (receipt, replayed) = match decision {
            AppendDecision::Admitted(receipt) => (receipt, false),
            AppendDecision::Replayed(receipt) => (receipt, true),
        };
        Ok(OperationReceiptAppend {
            event_id: claimed_event_id,
            admission_receipt_id: receipt.receipt_id,
            log_seq: receipt.log_seq,
            replayed,
        })
    }
}

impl StoreSigner for SemanticWriter {
    fn principal_id(&self) -> PrincipalId {
        self.binding.principal_id
    }

    fn control_domain_id(&self) -> nlos_types::ControlDomainId {
        self.binding.control_domain_id
    }

    fn key_id(&self) -> nlos_types::KeyId {
        self.binding.key_id
    }

    /// Signs with the writer key (v1 local-owner model: the writer also
    /// seals the admission receipts of the events it appends).
    fn sign(&self, message_digest: &[u8; 32]) -> Result<[u8; 64], StoreSignerError> {
        Ok(self.key.sign(message_digest).to_bytes())
    }
}

/// Domain-separated SHA-256 over the replay-critical operation tuple
/// `(application_id, installation_generation, entry_name, operation_id)`.
/// The `domain` picks the derivation family (event inputs vs the consume
/// ledger key); the framing is shared so both stay byte-comparable
/// inputs of the same tuple, never of each other.
fn receipt_input(domain: &[u8], fact: &OperationReceiptFact<'_>, tag: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(tag);
    hasher.update(fact.application_id.as_bytes());
    hasher.update(fact.installation_generation.get().to_be_bytes());
    hasher.update(
        u32::try_from(fact.entry_name.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    hasher.update(fact.entry_name.as_bytes());
    hasher.update(fact.operation_id.as_bytes());
    hasher.finalize().into()
}

/// The event-input derivation of the operation tuple (see
/// [`OPERATION_RECEIPT_NONCE_DOMAIN`]).
fn event_input(fact: &OperationReceiptFact<'_>, tag: &[u8]) -> [u8; 32] {
    receipt_input(OPERATION_RECEIPT_NONCE_DOMAIN, fact, tag)
}

/// Deterministic u64 derivation input (used for `issued_at_unix_ns`):
/// masked into the `i64` range SQLite's integer encoding stores. The value
/// is a replay-stable derivation label, not wall time — the authoritative
/// wall facts of the admission live in `admitted_at_ms` and the receipt.
fn receipt_input_u64(fact: &OperationReceiptFact<'_>, tag: &[u8]) -> u64 {
    let digest = event_input(fact, tag);
    be_u64(&digest[..8]) & WIDEST_STORED_MS
}

/// The 16-byte idempotency key of one operation tuple derivation (the
/// admission-clock key of [`SemanticWriter::append_operation_receipt`]).
fn receipt_idempotency_key(fact: &OperationReceiptFact<'_>, tag: &[u8]) -> IdempotencyKey {
    let digest = event_input(fact, tag);
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    IdempotencyKey::from_bytes(bytes)
}

/// The 16-byte capability-consume idempotency key of one operation tuple
/// (see [`OPERATION_RECEIPT_CONSUME_DOMAIN`]): same tuple and framing as
/// [`receipt_idempotency_key`] under the consume-only domain, so the
/// ledger charge replays per operation tuple without ever colliding with
/// an event-input or clock derivation of the same tuple.
fn receipt_consume_idempotency_key(fact: &OperationReceiptFact<'_>) -> IdempotencyKey {
    let digest = receipt_input(
        OPERATION_RECEIPT_CONSUME_DOMAIN,
        fact,
        b"capability-consume",
    );
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    IdempotencyKey::from_bytes(bytes)
}

/// Encodes the operation receipt's canonical content bytes: a fixed-order,
/// length-deliminated binary image of exactly the facts the assertion
/// claims (deterministic for a fixed durable state, so the content digest
/// is replay-stable).
fn encode_operation_receipt_content(fact: &OperationReceiptFact<'_>) -> Vec<u8> {
    fn put16(out: &mut Vec<u8>, bytes: &[u8; 16]) {
        out.extend_from_slice(bytes);
    }
    let mut out = Vec::with_capacity(192);
    out.extend_from_slice(OPERATION_RECEIPT_CONTENT_DOMAIN);
    out.push(1); // content version
    put16(&mut out, fact.application_id.as_bytes());
    out.extend_from_slice(&fact.installation_generation.get().to_be_bytes());
    put16(&mut out, fact.package_id.as_bytes());
    out.extend_from_slice(&fact.package_version.to_be_bytes());
    put16(&mut out, fact.artifact_id.as_bytes());
    out.extend_from_slice(&fact.payload_revision.to_be_bytes());
    out.extend_from_slice(&fact.payload_digest);
    out.extend_from_slice(&fact.payload_size_bytes.to_be_bytes());
    put16(&mut out, fact.operation_id.as_bytes());
    out.extend_from_slice(&fact.operation_generation.get().to_be_bytes());
    put16(&mut out, fact.callback_id.as_bytes());
    out.push(completion_outcome_kind(fact.outcome));
    put16(&mut out, outcome_receipt_id(fact.outcome).as_bytes());
    out.push(terminal_state_kind(fact.terminal_state));
    out.extend_from_slice(
        &u32::try_from(fact.entry_name.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    out.extend_from_slice(fact.entry_name.as_bytes());
    out
}

fn completion_outcome_kind(outcome: CompletionOutcome) -> u8 {
    match outcome {
        CompletionOutcome::Completed { .. } => 1,
        CompletionOutcome::Failed { .. } => 2,
        CompletionOutcome::CancelledBeforeEffect { .. } => 3,
        CompletionOutcome::PartialEffect { .. } => 4,
        CompletionOutcome::EffectUnknown { .. } => 5,
    }
}

fn terminal_state_kind(state: OperationState) -> u8 {
    match state {
        OperationState::Registered => 1,
        OperationState::Dispatched => 2,
        OperationState::CancelRequested => 3,
        OperationState::Completed { .. } => 4,
        OperationState::Failed { .. } => 5,
        OperationState::CancelledBeforeEffect { .. } => 6,
        OperationState::PartialEffect { .. } => 7,
        OperationState::EffectUnknown { .. } => 8,
    }
}

fn outcome_receipt_id(outcome: CompletionOutcome) -> ReceiptId {
    let receipt_id = match outcome {
        CompletionOutcome::Completed { receipt_id }
        | CompletionOutcome::Failed { receipt_id }
        | CompletionOutcome::CancelledBeforeEffect { receipt_id }
        | CompletionOutcome::PartialEffect { receipt_id }
        | CompletionOutcome::EffectUnknown { receipt_id } => receipt_id,
    };
    ReceiptId::from_bytes(*receipt_id.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(operation_byte: u8, entry: &'static str) -> OperationReceiptFact<'static> {
        OperationReceiptFact {
            application_id: ApplicationId::from_bytes([0x51; 16]),
            package_id: PackageId::from_bytes([0x52; 16]),
            package_version: 1,
            installation_generation: Generation::INITIAL,
            entry_name: entry,
            artifact_id: ArtifactId::from_bytes([0x53; 16]),
            payload_revision: 1,
            payload_digest: [0x54; 32],
            payload_size_bytes: 1,
            operation_id: OperationId::from_bytes([operation_byte; 16]),
            operation_generation: Generation::INITIAL,
            callback_id: CallbackId::from_bytes([0x55; 16]),
            outcome: CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes([0x56; 16]),
            },
            terminal_state: OperationState::Completed {
                receipt_id: ReceiptId::from_bytes([0x56; 16]),
            },
        }
    }

    /// The consume-ledger key and every event-input derivation of the same
    /// operation tuple come from distinct domain strings: across a spread
    /// of tuples and tags, no 16-byte key of one family equals a key of
    /// the other — a collision would let a ledger charge be rebound as a
    /// clock/event input (typed `IdempotencyConflict`) or vice versa.
    /// Inside a family the derivation stays stable per tuple and distinct
    /// across tuples.
    #[test]
    fn consume_key_domain_never_collides_with_event_input_domain() {
        let mut consume_keys = Vec::new();
        let mut event_keys = Vec::new();
        for (entry_index, entry) in ["entry-a", "entry-b"].iter().enumerate() {
            for operation_byte in [0x01_u8, 0x02, 0x03] {
                let tuple = fact(
                    operation_byte.wrapping_add(u8::try_from(entry_index * 16).expect("small")),
                    entry,
                );
                consume_keys.push(receipt_consume_idempotency_key(&tuple));
                for tag in [b"nonce".as_slice(), b"issued-at", b"admission-clock"] {
                    event_keys.push(receipt_idempotency_key(&tuple, tag));
                }
            }
        }
        assert_eq!(consume_keys.len(), 6);
        assert_eq!(event_keys.len(), 18);
        for consume in &consume_keys {
            for event in &event_keys {
                assert_ne!(
                    consume.as_bytes(),
                    event.as_bytes(),
                    "consume-domain key must never equal an event-domain key"
                );
            }
        }
        let mut seen = std::collections::HashSet::new();
        for key in consume_keys.iter().chain(event_keys.iter()) {
            assert!(seen.insert(key.as_bytes()), "keys must be family-distinct");
        }
        assert_eq!(
            receipt_consume_idempotency_key(&fact(0x01, "entry-a")).as_bytes(),
            receipt_consume_idempotency_key(&fact(0x01, "entry-a")).as_bytes(),
            "same tuple derives the same consume key"
        );
    }
}
