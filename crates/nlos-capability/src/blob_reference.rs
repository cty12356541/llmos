//! Signed, durable Blob references and verified blocks from pinned workspace files.
//!
//! The embedding runtime authenticates callers, resolves workspace directories
//! and capability slots, and implements method/target/purpose policy. This
//! module validates those decisions and independently checks current authority
//! records. No wire data by itself can authorize a call.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use nlos_identity::{
    IdentityAuthority, IdentityAuthorityError, KeyPurpose, VerifiedCapabilityCommandSigner,
};
use nlos_schema::{
    MethodSemantics, SABI_ENVELOPE_SCHEMA, blob_reference_schema_identity,
    decode_mint_reference_request, decode_read_reference_request, decode_release_reference_request,
    encode_mint_reference_result, encode_read_reference_result, encode_release_reference_result,
    sabi::v1::{
        Envelope, MintReferenceRequest, MintReferenceResult, ReadReferenceRequest,
        ReadReferenceResult, ReleaseReferenceResult,
    },
    validate_sabi_request_context,
};
use nlos_types::{
    ApplicationId, Generation, IdempotencyKey, NamespaceId, PrincipalId, WorkspaceId,
};
use sha2::{Digest, Sha256};

use crate::blob_ledger::{
    self, AuditWrite, BlobLedger, OperationKey, ReadBlock, ReferenceId, SignedRecord, Token,
    check_grant,
};
use crate::blob_model::{MAX_BLOCK, RATE_WINDOW_MS};
use crate::blob_open::validate_relative_path;
use crate::service_call::{
    AuthorizationError, AuthorizedServiceCall, CallBinding, ServiceAction, ServiceAuthorizer,
    ServiceMethod, ServiceObject, ServiceTarget, ServiceTime, authorize_call,
};
use crate::{CapabilityAuthority, CommandSignatureProof, derive_id, verify_command_signature};

pub use crate::blob_model::{
    AuthorizedWorkspace, BlobClock, BlobCommandProof, BlobCommandScope, BlobConfig, BlobError,
    BlobReply, DEFAULT_BLOB_TTL_MS, SignedMintCommand, SignedReleaseCommand, WorkspaceBinding,
    WorkspaceResolver, mint_command_message, mint_input_digest, read_input_digest,
    release_command_message, release_input_digest,
};
pub use crate::blob_open::RootedWorkspaceOpener;

pub const BLOB_REFERENCE_SERVICE: &str = "blob_reference";

// A request's host deadline cannot be interpreted before its first sample.
const FIRST_CLOCK_WAIT: Duration = Duration::from_millis(100);
const MAINTENANCE_CLOCK_WAIT: Duration = Duration::from_secs(5);

/// The only live Blob writer and descriptor registry for an authority DB.
/// Host callbacks may inspect their authorities and must not recursively
/// invoke the service. Policy runs outside the capability and identity DB
/// locks. Only the descriptor mutex is held across a block read and its two
/// checks. No DB lock is held while opening or hashing workspace files.
pub struct BlobReferenceService {
    identity: Arc<IdentityAuthority>,
    authorizer: Arc<dyn ServiceAuthorizer>,
    workspace: Arc<dyn WorkspaceResolver>,
    opener: RootedWorkspaceOpener,
    clock: Arc<BlobClock>,
    last_time: Mutex<Option<ServiceTime>>,
    config: BlobConfig,
    active: Arc<Quota>,
    reading: Arc<Quota>,
    mints: Mutex<VecDeque<(u64, PrincipalId)>>,
    pending: Mutex<HashSet<MintScopeKey>>,
    pins: Mutex<HashMap<ReferenceId, Arc<Pin>>>,
    // Drop pinned descriptors before releasing the process-wide writer lease.
    ledger: BlobLedger,
}

struct Pin {
    token: Token,
    manifest: Vec<u8>,
    file: Mutex<File>,
    _permit: Permit,
}

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
struct MintScopeKey {
    issuer: PrincipalId,
    application: ApplicationId,
    workspace: WorkspaceId,
    namespace: NamespaceId,
    key: IdempotencyKey,
}

struct PendingMint<'a> {
    pending: &'a Mutex<HashSet<MintScopeKey>>,
    key: MintScopeKey,
}

/// Register the descriptor before the durable token becomes visible. Until
/// commit, the database denies both reads and releases of this identity.
struct PendingPin<'a> {
    pins: &'a Mutex<HashMap<ReferenceId, Arc<Pin>>>,
    reference: ReferenceId,
    committed: bool,
}

impl<'a> PendingPin<'a> {
    fn register(
        pins: &'a Mutex<HashMap<ReferenceId, Arc<Pin>>>,
        pin: Arc<Pin>,
        timing: &Timing,
    ) -> Result<Self, BlobError> {
        let reference = pin.token.reference;
        let mut registry = timing.lock(pins)?;
        let std::collections::hash_map::Entry::Vacant(entry) = registry.entry(reference) else {
            return Err(BlobError::CorruptRecord);
        };
        entry.insert(pin);
        Ok(Self {
            pins,
            reference,
            committed: false,
        })
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for PendingPin<'_> {
    fn drop(&mut self) {
        if !self.committed
            && let Ok(mut pins) = self.pins.lock()
        {
            pins.remove(&self.reference);
        }
    }
}

impl Drop for PendingMint<'_> {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.key);
        }
    }
}

impl BlobReferenceService {
    /// Starts a new writer incarnation with an empty file registry.
    ///
    /// # Errors
    ///
    /// Fails for invalid limits, durability failure, or an active writer for
    /// the same authority DB. No provider is supplied implicitly.
    pub fn new(
        authority: Arc<CapabilityAuthority>,
        identity: Arc<IdentityAuthority>,
        authorizer: Arc<dyn ServiceAuthorizer>,
        workspace: Arc<dyn WorkspaceResolver>,
        opener: RootedWorkspaceOpener,
        clock: Arc<BlobClock>,
        config: BlobConfig,
    ) -> Result<Self, BlobError> {
        let config = config.validate()?;
        Ok(Self {
            identity,
            authorizer,
            workspace,
            opener,
            clock,
            config,
            last_time: Mutex::new(None),
            active: Arc::new(Quota::new(
                config.active_per_principal,
                config.active_global,
            )),
            reading: Arc::new(Quota::new(config.reads_per_principal, config.reads_global)),
            mints: Mutex::new(VecDeque::new()),
            pending: Mutex::new(HashSet::new()),
            pins: Mutex::new(HashMap::new()),
            ledger: BlobLedger::start(authority)?,
        })
    }

    /// Dispatches the three bounded payloads and their independent signatures.
    /// The transport has already decoded the envelope and authenticated the
    /// runtime binding. A successful reply carries the durable receipt ID.
    ///
    /// # Errors
    ///
    /// Fails closed for incompatible input, missing proof, or a rejection by
    /// the selected method.
    pub fn handle_for_ipc(
        &self,
        call: &CallBinding,
        envelope: &Envelope,
        proof: Option<BlobCommandProof<'_>>,
    ) -> Result<BlobReply, BlobError> {
        match (envelope.method.as_str(), proof) {
            ("mint_reference", Some(BlobCommandProof::Mint(proof))) => {
                self.mint_reference(call, envelope, Some(proof))
            }
            ("mint_reference", None) => self.mint_reference(call, envelope, None),
            ("read_reference", None) => self.read_reference(call, envelope),
            ("release_reference", Some(BlobCommandProof::Release(proof))) => {
                self.release_reference(call, envelope, Some(proof))
            }
            ("release_reference", None) => self.release_reference(call, envelope, None),
            _ => Err(BlobError::InvalidRequest),
        }
    }

    /// Pins an authorized workspace object and durably commits its manifest.
    ///
    /// # Errors
    ///
    /// Fails closed for invalid input, rights or signature failure, stale
    /// workspace, quotas, content mismatch, deadline, or storage failure.
    #[allow(clippy::too_many_lines)] // Keep the two admission points visible.
    pub fn mint_reference(
        &self,
        call: &CallBinding,
        envelope: &Envelope,
        proof: Option<&SignedMintCommand>,
    ) -> Result<BlobReply, BlobError> {
        let request = decode_mint_reference_request(&envelope.payload)?;
        let common = validate_call(call, envelope, "mint_reference", true)?;
        let proof = proof.ok_or(BlobError::MissingSignature)?;
        let scope = proof.scope;
        let op = operation(call, common, mint_input_digest(&request, scope))?;
        let holder = PrincipalId::from_bytes(array(&request.recipient_principal_id)?);
        let blocks = self.validate_mint(&request)?;
        let action = ServiceAction {
            method: ServiceMethod::MintReference,
            target: ServiceTarget {
                namespace: scope.workspace.namespace,
                object: ServiceObject::Workspace(scope.workspace.workspace),
                generation: scope.workspace.revision,
            },
            input_digest: op.digest,
        };
        let timing = self.begin(call, Duration::from_secs(30))?;
        let (now, grant) = self.authorize(call, &action, &timing)?;
        let signature_digest = mint_command_message(&call.caller, op.key, &request, scope);
        self.mint_identity(proof, op, holder, &signature_digest, now.wall_ms)?;
        let claim = self.claim_mint(op, scope.workspace, &timing)?;

        let retained = {
            let db = self.ledger.lock(&timing)?;
            self.check_mint_grant(&db, &grant, scope, now.wall_ms, request.valid_until_ms)?;
            if let Some(reference) = blob_ledger::mint_replay(&db, scope.workspace, op)? {
                let token = blob_ledger::load_token(&db, reference)?;
                if token.holder != holder
                    || token.workspace != scope.workspace
                    || token.issuer != op.actor
                    || token.source != scope.source
                    || token.size != request.size_bytes
                    || token.block_bytes != request.max_chunk_bytes
                    || token.max_reads != request.max_reads
                    || token.expiry != request.valid_until_ms
                {
                    return Err(BlobError::CorruptRecord);
                }
                Some(mint_reply(&token)?)
            } else {
                BlobLedger::guard_mint_rate(&db, self.config, op.actor, now.wall_ms)?;
                self.ledger.guard_storage(
                    &db,
                    self.config,
                    AuditWrite::Mint {
                        now: now.wall_ms,
                        manifest_bytes: blocks * 32,
                        max_reads: request.max_reads,
                    },
                )?;
                None
            }
        };
        if let Some(reply) = retained {
            self.check_delivery(&timing, &grant, Some(request.valid_until_ms))?;
            return Ok(reply);
        }
        let permit = self.active.acquire(op.actor, holder, &timing)?;
        self.record_mint_attempt(op.actor, now.monotonic_ns, &timing)?;
        let directory = self.workspace.resolve(&call.caller, scope.workspace)?;
        if directory.binding != scope.workspace {
            return Err(BlobError::BindingMismatch);
        }
        let (now, grant) = self.authorize(call, &action, &timing)?;
        self.mint_identity(proof, op, holder, &signature_digest, now.wall_ms)?;
        {
            let db = self.ledger.lock(&timing)?;
            self.check_mint_grant(&db, &grant, scope, now.wall_ms, request.valid_until_ms)?;
        }
        self.check_delivery(&timing, &grant, Some(request.valid_until_ms))?;
        let mut file = self.opener.open(
            &directory.directory,
            &request.path_handle,
            request.size_bytes,
        )?;
        drop(directory);
        let manifest = self.hash_file(&mut file, &request, blocks, &timing)?;

        // Revalidate the resolver revision and all authority decisions after IO.
        let directory = self.workspace.resolve(&call.caller, scope.workspace)?;
        if directory.binding != scope.workspace {
            return Err(BlobError::BindingMismatch);
        }
        drop(directory);
        let (now, grant) = self.authorize(call, &action, &timing)?;
        let signer = self.mint_identity(proof, op, holder, &signature_digest, now.wall_ms)?;
        let audit_proof = SignedRecord {
            signer,
            signature: &proof.signature,
            signed_at: scope.issued_at_ms,
            caller: call.caller,
        };
        let reference = derive_id(
            b"nlos/blob/reference/v1",
            &[
                op.actor.as_bytes(),
                op.application.as_bytes(),
                scope.workspace.workspace.as_bytes(),
                scope.workspace.namespace.as_bytes(),
                op.key.as_bytes(),
                &op.digest,
                &self.ledger.epoch.to_be_bytes(),
            ],
        );
        let token = Token {
            reference,
            receipt: blob_ledger::operation_receipt(b"nlos/blob/mint/receipt/v1", reference, op),
            epoch: self.ledger.epoch,
            issuer: op.actor,
            holder,
            workspace: scope.workspace,
            source: scope.source,
            size: request.size_bytes,
            block_bytes: request.max_chunk_bytes,
            max_reads: request.max_reads,
            expiry: request.valid_until_ms,
            minted_at: now.wall_ms,
        };
        let reply = mint_reply(&token)?;
        let pin = Arc::new(Pin {
            token,
            manifest,
            file: Mutex::new(file),
            _permit: permit,
        });
        let pending_pin = PendingPin::register(&self.pins, Arc::clone(&pin), &timing)?;
        self.ledger.write(&timing, |db| {
            timing.check_elapsed()?;
            self.check_mint_grant(db, &grant, scope, now.wall_ms, token.expiry)?;
            BlobLedger::guard_mint_rate(db, self.config, op.actor, now.wall_ms)?;
            self.ledger.guard_storage(
                db,
                self.config,
                AuditWrite::Mint {
                    now: now.wall_ms,
                    manifest_bytes: blocks * 32,
                    max_reads: request.max_reads,
                },
            )?;
            timing.check_elapsed()?;
            blob_ledger::insert_mint(db, &token, op, &request, &pin.manifest, &audit_proof)
        })?;
        pending_pin.commit();
        drop(pin);
        drop(claim);
        self.check_delivery(&timing, &grant, Some(token.expiry))?;
        Ok(reply)
    }

    /// Reads and authorizes exactly one mint-time block.
    ///
    /// # Errors
    ///
    /// Fails for unusable tokens, invalid ranges, stale authorization, changed
    /// bytes, over-budget reads, deadlines, or durable receipt failure.
    pub fn read_reference(
        &self,
        call: &CallBinding,
        envelope: &Envelope,
    ) -> Result<BlobReply, BlobError> {
        let request = decode_read_reference_request(&envelope.payload)?;
        let common = validate_call(call, envelope, "read_reference", false)?;
        let op = operation(call, common, read_input_digest(&request))?;
        let reference = array(&request.reference_id)?;
        let timing = self.begin(call, Duration::from_secs(5))?;
        let _permit = self.reading.acquire(op.actor, op.actor, &timing)?;
        let token = self.token(reference, &timing)?;
        if token.holder != op.actor {
            return Err(BlobError::NotFound);
        }
        let action = token_action(ServiceMethod::ReadReference, &token, op.digest);
        let pin = timing.lock(&self.pins)?.get(&reference).cloned();
        let Some(pin) = pin else {
            let (now, grant) = self.authorize(call, &action, &timing)?;
            self.require_active_identity(op.actor, now.wall_ms)?;
            let db = self.ledger.lock(&timing)?;
            self.validate_read_state(&db, &token, &grant, now.wall_ms)?;
            return Err(BlobError::NotFound);
        };
        if pin.token != token {
            return Err(BlobError::CorruptRecord);
        }
        let mut file = timing.lock(&pin.file)?;
        // A read admitted on this descriptor is serialized through its durable
        // count. A release can run during IO; the final transaction sees it.
        let (now, grant) = self.authorize(call, &action, &timing)?;
        self.require_active_identity(op.actor, now.wall_ms)?;
        let block = {
            let db = self.ledger.lock(&timing)?;
            self.validate_read_state(&db, &token, &grant, now.wall_ms)?;
            let block = read_block_range(&pin, &request)?;
            blob_ledger::read_replay(&db, reference, op, block)?;
            block
        };
        timing.check_elapsed()?;
        let chunk = read_verified_block(&mut file, &token, block)?;
        let payload = encode_read_reference_result(&ReadReferenceResult {
            schema: Some(blob_reference_schema_identity()),
            chunk,
            offset: block.offset,
            chunk_digest_sha256: block.digest.to_vec(),
        })?;
        let (now, grant) = self.authorize(call, &action, &timing)?;
        self.require_active_identity(op.actor, now.wall_ms)?;
        let (receipt_id, exhausted) = self.ledger.write(&timing, |db| {
            timing.check_elapsed()?;
            let ordinal = self.validate_read_state(db, &token, &grant, now.wall_ms)?;
            if blob_ledger::durable_block_digest(db, &token, block.offset)? != block.digest {
                return Err(BlobError::CorruptRecord);
            }
            if let Some(receipt) = blob_ledger::read_replay(db, reference, op, block)? {
                return Ok((receipt, false));
            }
            self.ledger
                .guard_storage(db, self.config, AuditWrite::Read { now: now.wall_ms })?;
            timing.check_elapsed()?;
            let next = ordinal + 1;
            let receipt = blob_ledger::insert_read(db, reference, op, block, next, now.wall_ms)?;
            Ok((receipt, next == token.max_reads))
        })?;
        drop(file);
        if exhausted {
            lock(&self.pins)?.remove(&reference);
        }
        self.check_delivery(&timing, &grant, Some(token.expiry))?;
        Ok(BlobReply {
            payload,
            receipt_id,
        })
    }

    /// Releases a reference using the current issuer's or holder's signed
    /// command and current release authorization.
    ///
    /// # Errors
    ///
    /// Fails for invalid proof, stale rights, unavailable references, or
    /// durable revocation failure.
    pub fn release_reference(
        &self,
        call: &CallBinding,
        envelope: &Envelope,
        proof: Option<&SignedReleaseCommand>,
    ) -> Result<BlobReply, BlobError> {
        let request = decode_release_reference_request(&envelope.payload)?;
        let common = validate_call(call, envelope, "release_reference", false)?;
        let proof = proof.ok_or(BlobError::MissingSignature)?;
        let op = operation(call, common, release_input_digest(&request, proof.scope))?;
        let reference = array(&request.reference_id)?;
        let timing = self.begin(call, Duration::from_secs(5))?;
        let token = self.token(reference, &timing)?;
        if ![token.issuer, token.holder].contains(&op.actor) {
            return Err(BlobError::NotFound);
        }
        if token.workspace != proof.scope.workspace || token.source != proof.scope.source {
            return Err(BlobError::BindingMismatch);
        }
        let action = token_action(ServiceMethod::ReleaseReference, &token, op.digest);
        let (now, grant) = self.authorize(call, &action, &timing)?;
        Self::validate_signature_actor(op, proof.signer, proof.scope.issued_at_ms, now.wall_ms)?;
        let signer = verify_command_signature(
            &self.identity,
            CommandSignatureProof {
                signer: proof.signer,
                signature: proof.signature,
            },
            &release_command_message(&call.caller, op.key, &request, proof.scope),
            now.wall_ms,
        )?;
        let audit_proof = SignedRecord {
            signer,
            signature: &proof.signature,
            signed_at: proof.scope.issued_at_ms,
            caller: call.caller,
        };
        let payload = encode_release_reference_result(&ReleaseReferenceResult {
            schema: Some(blob_reference_schema_identity()),
            released: true,
        })?;
        let receipt_id = self.ledger.write(&timing, |db| {
            timing.check_elapsed()?;
            if blob_ledger::load_token(db, reference)? != token {
                return Err(BlobError::CorruptRecord);
            }
            check_grant(
                db,
                grant.capability(),
                op.actor,
                token.workspace.namespace,
                now.wall_ms,
            )?;
            if let Some(receipt) = blob_ledger::release_replay(db, reference, op)? {
                return Ok(receipt);
            }
            self.live_token(&token, now.wall_ms)?;
            self.ledger
                .guard_storage(db, self.config, AuditWrite::Release)?;
            timing.check_elapsed()?;
            blob_ledger::insert_release(db, reference, op, now.wall_ms, &audit_proof)
        })?;
        lock(&self.pins)?.remove(&reference);
        self.check_delivery(&timing, &grant, None)?;
        Ok(BlobReply {
            payload,
            receipt_id,
        })
    }

    /// Closes expired pinned handles in a scan bounded by the active-token
    /// limit. Embedders should schedule it even during idle periods.
    ///
    /// # Errors
    ///
    /// Fails if trusted time or service state is unavailable.
    pub fn maintain(&self) -> Result<usize, BlobError> {
        let timing = Timing::local_only(Instant::now() + MAINTENANCE_CLOCK_WAIT);
        let now = self.time(&timing)?;
        self.evict_expired(now.wall_ms)
    }

    fn validate_mint(&self, request: &MintReferenceRequest) -> Result<u64, BlobError> {
        validate_relative_path(&request.path_handle)?;
        let _digest: [u8; 32] = array(&request.file_digest_sha256)?;
        if !(1..=MAX_BLOCK).contains(&request.max_chunk_bytes)
            || !(1..=self.config.max_reads).contains(&request.max_reads)
            || request.valid_until_ms > i64::MAX.unsigned_abs()
        {
            return Err(BlobError::InvalidRequest);
        }
        let blocks = request.size_bytes.div_ceil(request.max_chunk_bytes);
        if request.size_bytes > self.config.file_bytes || blocks > self.config.manifest_blocks {
            return Err(BlobError::QuotaExceeded);
        }
        Ok(blocks)
    }

    fn validate_signature_actor(
        op: OperationKey,
        signer: PrincipalId,
        issued_at: u64,
        now: u64,
    ) -> Result<(), BlobError> {
        if signer != op.actor || issued_at > now {
            return Err(BlobError::BindingMismatch);
        }
        Ok(())
    }

    fn mint_identity(
        &self,
        proof: &SignedMintCommand,
        op: OperationKey,
        holder: PrincipalId,
        digest: &[u8; 32],
        now: u64,
    ) -> Result<VerifiedCapabilityCommandSigner, BlobError> {
        Self::validate_signature_actor(op, proof.signer, proof.scope.issued_at_ms, now)?;
        let signer = verify_command_signature(
            &self.identity,
            CommandSignatureProof {
                signer: proof.signer,
                signature: proof.signature,
            },
            digest,
            now,
        )?;
        self.require_active_identity(holder, now)?;
        Ok(signer)
    }

    fn require_active_identity(&self, principal: PrincipalId, now: u64) -> Result<(), BlobError> {
        let record = self.identity.inspect_current_principal_binding(principal)?;
        if record.principal_id != principal {
            return Err(BlobError::CorruptRecord);
        }
        if record.key_purpose != KeyPurpose::SemanticSigning {
            return Err(IdentityAuthorityError::KeyPurposeMismatch.into());
        }
        if record.key_revoked_at_ms.is_some() {
            return Err(IdentityAuthorityError::KeyRevoked.into());
        }
        if now < record.key_valid_from_ms {
            return Err(IdentityAuthorityError::KeyNotYetValid.into());
        }
        if now > record.key_valid_until_ms {
            return Err(IdentityAuthorityError::KeyExpired.into());
        }
        Ok(())
    }

    fn check_mint_grant(
        &self,
        db: &rusqlite::Connection,
        grant: &AuthorizedServiceCall,
        scope: BlobCommandScope,
        now: u64,
        expiry: u64,
    ) -> Result<(), BlobError> {
        if grant.capability() != scope.source {
            return Err(BlobError::BindingMismatch);
        }
        let issuer = grant.binding().call.caller.principal;
        let record = check_grant(db, scope.source, issuer, scope.workspace.namespace, now)?;
        if expiry <= now {
            return Err(BlobError::Expired);
        }
        if expiry - now > self.config.ttl_max_ms
            || expiry > record.valid_until_ms
            || expiry > grant.valid_until_ms()
        {
            return Err(BlobError::InvalidRequest);
        }
        Ok(())
    }

    fn token(&self, reference: ReferenceId, timing: &Timing) -> Result<Token, BlobError> {
        let db = self.ledger.lock(timing)?;
        blob_ledger::load_token(&db, reference)
    }

    fn live_token(&self, token: &Token, now: u64) -> Result<(), BlobError> {
        if token.epoch != self.ledger.epoch {
            return Err(BlobError::NotFound);
        }
        if now < token.minted_at {
            return Err(BlobError::Authorization(
                AuthorizationError::TimeUnavailable,
            ));
        }
        if now >= token.expiry {
            return Err(BlobError::Expired);
        }
        Ok(())
    }

    fn validate_read_state(
        &self,
        db: &rusqlite::Connection,
        token: &Token,
        grant: &AuthorizedServiceCall,
        now: u64,
    ) -> Result<u64, BlobError> {
        self.live_token(token, now)?;
        if blob_ledger::load_token(db, token.reference)? != *token {
            return Err(BlobError::CorruptRecord);
        }
        if blob_ledger::is_released(db, token.reference)? {
            return Err(BlobError::Released);
        }
        check_grant(
            db,
            token.source,
            token.issuer,
            token.workspace.namespace,
            now,
        )?;
        check_grant(
            db,
            grant.capability(),
            token.holder,
            token.workspace.namespace,
            now,
        )?;
        let ordinal = blob_ledger::latest_ordinal(db, token.reference)?;
        if ordinal >= token.max_reads {
            return Err(BlobError::Exhausted);
        }
        Ok(ordinal)
    }

    fn hash_file(
        &self,
        file: &mut File,
        request: &MintReferenceRequest,
        blocks: u64,
        timing: &Timing,
    ) -> Result<Vec<u8>, BlobError> {
        let mut buffer = vec![0; bounded_usize(request.size_bytes.min(request.max_chunk_bytes))?];
        let mut manifest = Vec::with_capacity(bounded_usize(blocks * 32)?);
        let mut remaining = request.size_bytes;
        let mut full = Sha256::new();
        while remaining > 0 {
            timing.check(self.time(timing)?)?;
            let size = remaining.min(request.max_chunk_bytes);
            let chunk = &mut buffer[..bounded_usize(size)?];
            file.read_exact(chunk)?;
            remaining -= size;
            full.update(&*chunk);
            manifest.extend_from_slice(&Sha256::digest(chunk));
        }
        if file.metadata()?.len() != request.size_bytes
            || full.finalize().as_slice() != request.file_digest_sha256
        {
            return Err(BlobError::ContentChanged);
        }
        Ok(manifest)
    }

    fn claim_mint(
        &self,
        op: OperationKey,
        workspace: WorkspaceBinding,
        timing: &Timing,
    ) -> Result<PendingMint<'_>, BlobError> {
        let key = MintScopeKey {
            issuer: op.actor,
            application: op.application,
            workspace: workspace.workspace,
            namespace: workspace.namespace,
            key: op.key,
        };
        let mut pending = timing.lock(&self.pending)?;
        if pending.len() >= self.config.active_global || !pending.insert(key) {
            return Err(BlobError::Busy);
        }
        Ok(PendingMint {
            pending: &self.pending,
            key,
        })
    }

    fn record_mint_attempt(
        &self,
        actor: PrincipalId,
        monotonic_ns: u64,
        timing: &Timing,
    ) -> Result<(), BlobError> {
        let mut recent = timing.lock(&self.mints)?;
        while recent.front().is_some_and(|(when, _)| {
            monotonic_ns.saturating_sub(*when) >= RATE_WINDOW_MS * 1_000_000
        }) {
            recent.pop_front();
        }
        if recent.len() >= self.config.mints_global
            || recent
                .iter()
                .filter(|(_, principal)| *principal == actor)
                .count()
                >= self.config.mints_per_principal
        {
            return Err(BlobError::RateLimited);
        }
        let ordered_time = recent
            .back()
            .map_or(monotonic_ns, |(when, _)| (*when).max(monotonic_ns));
        recent.push_back((ordered_time, actor));
        Ok(())
    }

    fn time(&self, timing: &Timing) -> Result<ServiceTime, BlobError> {
        // Serialize sampling with validation and publication. An earlier
        // sample cannot be overtaken by a later callback completing first.
        let mut last = timing.lock(&self.last_time)?;
        let now = (self.clock)()?;
        timing.check_elapsed()?;
        if now.wall_ms > i64::MAX.unsigned_abs()
            || last.is_some_and(|last| {
                last.wall_ms > now.wall_ms || last.monotonic_ns > now.monotonic_ns
            })
        {
            return Err(AuthorizationError::TimeUnavailable.into());
        }
        *last = Some(now);
        Ok(now)
    }

    fn begin(&self, call: &CallBinding, max: Duration) -> Result<Timing, BlobError> {
        let started = Instant::now();
        let first_sample = Timing::local_only(started + FIRST_CLOCK_WAIT);
        let now = self.time(&first_sample).map_err(|error| match error {
            BlobError::Authorization(AuthorizationError::DeadlineExpired) => {
                AuthorizationError::TimeUnavailable.into()
            }
            other => other,
        })?;
        let max_ns = u64::try_from(max.as_nanos()).map_err(|_| BlobError::InvalidConfig)?;
        let host_end = now
            .monotonic_ns
            .checked_add(max_ns)
            .ok_or(AuthorizationError::TimeUnavailable)?;
        let stop_ns = call
            .deadline_monotonic_ns
            .map_or(host_end, |client| client.min(host_end));
        let local_budget = Duration::from_nanos(stop_ns.saturating_sub(now.monotonic_ns));
        let timing = Timing {
            stop_ns,
            stop_elapsed: started + local_budget,
        };
        timing.check(now)?;
        let mut pins = timing.lock(&self.pins)?;
        pins.retain(|_, pin| now.wall_ms < pin.token.expiry);
        drop(pins);
        Ok(timing)
    }

    fn authorize(
        &self,
        call: &CallBinding,
        action: &ServiceAction,
        timing: &Timing,
    ) -> Result<(ServiceTime, AuthorizedServiceCall), BlobError> {
        let now = self.time(timing)?;
        timing.check(now)?;
        let grant = authorize_call(self.authorizer.as_ref(), call, action, now)?;
        let now = self.time(timing)?;
        timing.check(now)?;
        if now.wall_ms > grant.valid_until_ms() {
            return Err(AuthorizationError::GrantExpired.into());
        }
        Ok((now, grant))
    }

    fn evict_expired(&self, now: u64) -> Result<usize, BlobError> {
        let mut pins = lock(&self.pins)?;
        let before = pins.len();
        pins.retain(|_, pin| now < pin.token.expiry);
        Ok(before - pins.len())
    }

    // A synchronous OS/SQLite write can outlast admission. Keep a committed
    // receipt for deduplication, but withhold the protected response when
    // the trusted clocks no longer allow delivery.
    fn check_delivery(
        &self,
        timing: &Timing,
        grant: &AuthorizedServiceCall,
        expiry: Option<u64>,
    ) -> Result<(), BlobError> {
        let now = self.time(timing)?;
        timing.check(now)?;
        if expiry.is_some_and(|expiry| now.wall_ms >= expiry) {
            self.evict_expired(now.wall_ms)?;
            return Err(BlobError::Expired);
        }
        if now.wall_ms > grant.valid_until_ms() {
            return Err(AuthorizationError::GrantExpired.into());
        }
        Ok(())
    }
}

fn validate_call<'a>(
    call: &CallBinding,
    envelope: &'a Envelope,
    method: &str,
    long_running: bool,
) -> Result<&'a nlos_schema::sabi::v1::SabiRequestContext, BlobError> {
    let schema = envelope.schema.as_ref().ok_or(BlobError::InvalidRequest)?;
    if envelope.service != BLOB_REFERENCE_SERVICE
        || envelope.method != method
        || schema.name != SABI_ENVELOPE_SCHEMA
        || schema.major != 1
        || !schema.critical_extension_ids.is_empty()
        || envelope.request_id.len() != 16
    {
        return Err(BlobError::InvalidRequest);
    }
    let common = validate_sabi_request_context(
        envelope,
        MethodSemantics {
            side_effecting: true,
            long_running,
        },
        0,
    )?;
    if common.task_execution_binding.is_some() || common.reservation_handle.is_some() {
        return Err(BlobError::UnsupportedBinding);
    }
    let caller = common.caller.as_ref().ok_or(BlobError::InvalidRequest)?;
    if caller.principal_id.as_slice() != call.caller.principal.as_bytes()
        || caller.application_id.as_slice() != call.caller.application.as_bytes()
        || caller.process_id.as_slice() != call.caller.process.as_bytes()
        || caller.process_generation != call.caller.process_generation.get()
        || Some(common.idempotency_key.as_slice())
            != call
                .idempotency_key
                .as_ref()
                .map(|key| key.as_bytes().as_slice())
        || common.deadline_monotonic_ns != call.deadline_monotonic_ns.unwrap_or_default()
        || common.capability_handles.len() != call.capabilities.len()
        || common
            .capability_handles
            .iter()
            .zip(&call.capabilities)
            .any(|(wire, resolved)| wire.generation != resolved.generation.get())
    {
        return Err(BlobError::BindingMismatch);
    }
    Ok(common)
}

fn operation(
    call: &CallBinding,
    common: &nlos_schema::sabi::v1::SabiRequestContext,
    digest: [u8; 32],
) -> Result<OperationKey, BlobError> {
    if common.proposal_or_input_digest_sha256.as_slice() != digest {
        return Err(BlobError::BindingMismatch);
    }
    Ok(OperationKey {
        actor: call.caller.principal,
        application: call.caller.application,
        key: call
            .idempotency_key
            .ok_or(AuthorizationError::MissingIdempotencyKey)?,
        digest,
    })
}

fn token_action(method: ServiceMethod, token: &Token, digest: [u8; 32]) -> ServiceAction {
    ServiceAction {
        method,
        target: ServiceTarget {
            namespace: token.workspace.namespace,
            object: ServiceObject::BlobReference(token.reference),
            generation: Generation::INITIAL,
        },
        input_digest: digest,
    }
}

fn mint_reply(token: &Token) -> Result<BlobReply, BlobError> {
    let payload = encode_mint_reference_result(&MintReferenceResult {
        schema: Some(blob_reference_schema_identity()),
        reference_id: token.reference.to_vec(),
        minted_at_ms: token.minted_at,
    })?;
    Ok(BlobReply {
        payload,
        receipt_id: token.receipt,
    })
}

fn read_verified_block(
    file: &mut File,
    token: &Token,
    block: ReadBlock,
) -> Result<Vec<u8>, BlobError> {
    if file.metadata()?.len() != token.size {
        return Err(BlobError::ContentChanged);
    }
    let mut chunk = vec![0; bounded_usize(block.size)?];
    file.seek(SeekFrom::Start(block.offset))?;
    file.read_exact(&mut chunk)?;
    if Sha256::digest(&chunk).as_slice() != block.digest || file.metadata()?.len() != token.size {
        return Err(BlobError::ContentChanged);
    }
    Ok(chunk)
}

fn read_block_range(pin: &Pin, request: &ReadReferenceRequest) -> Result<ReadBlock, BlobError> {
    let token = pin.token;
    let offset = request.offset;
    if offset == token.size {
        return Err(BlobError::EndOfFile);
    }
    if offset > token.size
        || !offset.is_multiple_of(token.block_bytes)
        || !(1..=MAX_BLOCK).contains(&request.length_bytes)
        || offset.checked_add(request.length_bytes).is_none()
    {
        return Err(BlobError::InvalidRange);
    }
    let size = token.block_bytes.min(token.size - offset);
    if request.length_bytes < size {
        return Err(BlobError::InvalidRange);
    }
    let index =
        usize::try_from(offset / token.block_bytes).map_err(|_| BlobError::CorruptRecord)? * 32;
    let digest = pin
        .manifest
        .get(index..index + 32)
        .ok_or(BlobError::CorruptRecord)?
        .try_into()
        .map_err(|_| BlobError::CorruptRecord)?;
    Ok(ReadBlock {
        offset,
        size,
        digest,
    })
}

fn bounded_usize(value: u64) -> Result<usize, BlobError> {
    usize::try_from(value).map_err(|_| BlobError::InvalidRequest)
}

fn array<const N: usize>(value: &[u8]) -> Result<[u8; N], BlobError> {
    value.try_into().map_err(|_| BlobError::InvalidRequest)
}

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, BlobError> {
    mutex.lock().map_err(|_| BlobError::Poisoned)
}

pub(super) struct Timing {
    stop_ns: u64,
    stop_elapsed: Instant,
}

impl Timing {
    const fn local_only(stop_elapsed: Instant) -> Self {
        Self {
            stop_ns: u64::MAX,
            stop_elapsed,
        }
    }

    fn check(&self, now: ServiceTime) -> Result<(), BlobError> {
        if now.monotonic_ns >= self.stop_ns {
            return Err(AuthorizationError::DeadlineExpired.into());
        }
        self.check_elapsed()
    }

    pub fn check_elapsed(&self) -> Result<(), BlobError> {
        if Instant::now() >= self.stop_elapsed {
            return Err(AuthorizationError::DeadlineExpired.into());
        }
        Ok(())
    }

    pub fn pause(&self) -> Result<(), BlobError> {
        std::thread::sleep(self.retry_delay_at(Instant::now())?);
        self.check_elapsed()
    }

    fn retry_delay_at(&self, now: Instant) -> Result<Duration, BlobError> {
        let remaining = self.stop_elapsed.saturating_duration_since(now);
        if remaining.is_zero() {
            return Err(AuthorizationError::DeadlineExpired.into());
        }
        Ok(Duration::from_millis(1).min(remaining))
    }

    fn lock<'a, T>(&self, mutex: &'a Mutex<T>) -> Result<MutexGuard<'a, T>, BlobError> {
        loop {
            self.check_elapsed()?;
            match mutex.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(TryLockError::Poisoned(_)) => return Err(BlobError::Poisoned),
                Err(TryLockError::WouldBlock) => self.pause()?,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_is_bounded_by_the_remaining_deadline() {
        let start = Instant::now();
        let deadline = start + Duration::from_millis(20);
        let timing = Timing::local_only(deadline);
        let final_budget = Duration::from_micros(250);
        let near_deadline = deadline.checked_sub(final_budget).unwrap();

        assert_eq!(
            timing.retry_delay_at(start).unwrap(),
            Duration::from_millis(1)
        );
        assert_eq!(timing.retry_delay_at(near_deadline).unwrap(), final_budget);
        for expired in [deadline, deadline + final_budget] {
            assert!(matches!(
                timing.retry_delay_at(expired),
                Err(BlobError::Authorization(
                    AuthorizationError::DeadlineExpired
                ))
            ));
        }
    }
}

struct Quota {
    per_principal: usize,
    global: usize,
    usage: Mutex<Usage>,
}

#[derive(Default)]
struct Usage {
    principals: HashMap<PrincipalId, usize>,
    total: usize,
}

impl Quota {
    fn new(per_principal: usize, global: usize) -> Self {
        Self {
            per_principal,
            global,
            usage: Mutex::new(Usage::default()),
        }
    }

    fn acquire(
        self: &Arc<Self>,
        issuer: PrincipalId,
        holder: PrincipalId,
        timing: &Timing,
    ) -> Result<Permit, BlobError> {
        let subjects = if issuer == holder {
            [Some(issuer), None]
        } else {
            [Some(issuer), Some(holder)]
        };
        let mut usage = timing.lock(&self.usage)?;
        if usage.total >= self.global
            || subjects.iter().flatten().any(|id| {
                usage.principals.get(id).copied().unwrap_or_default() >= self.per_principal
            })
        {
            return Err(BlobError::QuotaExceeded);
        }
        usage.total += 1;
        for subject in subjects.into_iter().flatten() {
            *usage.principals.entry(subject).or_default() += 1;
        }
        Ok(Permit {
            quota: Arc::clone(self),
            subjects,
        })
    }
}

struct Permit {
    quota: Arc<Quota>,
    subjects: [Option<PrincipalId>; 2],
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut usage) = self.quota.usage.lock() {
            usage.total -= 1;
            for subject in self.subjects.into_iter().flatten() {
                if let Some(count) = usage.principals.get_mut(&subject) {
                    *count -= 1;
                    if *count == 0 {
                        usage.principals.remove(&subject);
                    }
                }
            }
        }
    }
}
