//! Durable single-node Process/AgentInstance/IsolationDomain binding authority.
//!
//! This crate owns authority-assigned execution identities and generation
//! fences for the delegated-task slice of v0.5 §8. It deliberately does not
//! claim the full cross-authority `BirthDecision`: resource, capability,
//! namespace and Task contract prepares remain separate acceptance gates.

mod model;
mod schema;

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_types::{
    AgentInstanceId, ExecutionFiberId, Generation, IdempotencyKey, IsolationDomainId, ProcessId,
    TaskAttemptId, TaskId,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

pub use model::{
    ActiveProcessBinding, CreateIsolationDomainRequest, FencingToken, FiberEntrySnapshotDecision,
    FiberEntrySnapshotRecord, FiberIncarnationDecision, FiberIncarnationRecord,
    IsolationDomainDecision, IsolationDomainRecord, IsolationDomainRotationDecision,
    ProcessBindingDecision, ProcessBindingEndpointProof, ProcessBindingRecord,
    RegisterDelegatedProcessRequest, RegisterFiberIncarnationRequest, RestoreProcessDecision,
    RestoreProcessRequest, RotateIsolationDomainRequest, WriteFiberEntrySnapshotRequest,
};

#[derive(Debug)]
pub enum ProcessAuthorityError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    SchemaVersionUnsupported(i64),
    IsolationDomainNotFound(IsolationDomainId),
    ProcessNotFound(ProcessId),
    IdempotencyConflict,
    IsolationDomainFenceConflict,
    ProcessFenceConflict,
    StaleIsolationDomain,
    StaleProcessBinding,
    GenerationExhausted,
    /// The fiber binding is the all-zero value, which is not a binding.
    InvalidFiberBinding,
    /// No fiber incarnation is registered for the binding under the process.
    FiberIncarnationNotFound,
    /// The presented durable fiber incarnation is not the binding's current
    /// one (ADR-0012 generation gate; fail-closed, zero side effect).
    StaleFiberIncarnation,
    /// No entry snapshot exists for the binding's current incarnation.
    FiberSnapshotNotFound,
    /// The entry snapshot input violates its durable contract.
    InvalidFiberSnapshot(&'static str),
    CorruptRecord(&'static str),
    LockPoisoned,
}

impl fmt::Display for ProcessAuthorityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite process authority failure: {error}"),
            Self::Io(error) => write!(formatter, "process authority I/O failure: {error}"),
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => write!(
                formatter,
                "WAL/FULL durability unavailable: journal_mode={journal_mode}, synchronous={synchronous}"
            ),
            Self::SchemaVersionUnsupported(version) => {
                write!(
                    formatter,
                    "unsupported process authority schema version {version}"
                )
            }
            Self::IsolationDomainNotFound(id) => {
                write!(formatter, "isolation domain {id:?} does not exist")
            }
            Self::ProcessNotFound(id) => write!(formatter, "process {id:?} does not exist"),
            Self::IdempotencyConflict => formatter.write_str(
                "idempotency key or authority-assigned identity was rebound to different input",
            ),
            Self::IsolationDomainFenceConflict => {
                formatter.write_str("isolation domain generation or fencing token is stale")
            }
            Self::ProcessFenceConflict => {
                formatter.write_str("process generation or fencing token is stale")
            }
            Self::StaleIsolationDomain => {
                formatter.write_str("process binding references a stale isolation domain")
            }
            Self::StaleProcessBinding => {
                formatter.write_str("process binding is not the authority current generation")
            }
            Self::GenerationExhausted => formatter.write_str("generation space exhausted"),
            Self::InvalidFiberBinding => {
                formatter.write_str("fiber binding must not be the all-zero value")
            }
            Self::FiberIncarnationNotFound => {
                formatter.write_str("no fiber incarnation is registered for this binding")
            }
            Self::StaleFiberIncarnation => formatter
                .write_str("the presented fiber incarnation is not the binding's current one"),
            Self::FiberSnapshotNotFound => formatter
                .write_str("no entry snapshot exists for the binding's current incarnation"),
            Self::InvalidFiberSnapshot(reason) => {
                write!(formatter, "invalid fiber entry snapshot: {reason}")
            }
            Self::CorruptRecord(reason) => write!(formatter, "corrupt durable record: {reason}"),
            Self::LockPoisoned => formatter.write_str("process authority writer lock is poisoned"),
        }
    }
}

impl Error for ProcessAuthorityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for ProcessAuthorityError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

pub struct ProcessAuthority {
    connection: Mutex<Connection>,
}

impl ProcessAuthority {
    /// Opens a local authority at `<root>/process-authority.db`, requiring
    /// `SQLite` WAL plus FULL synchronous durability.
    ///
    /// # Errors
    ///
    /// Fails when storage cannot be opened, the durability contract cannot
    /// be established, or the durable schema version is unsupported.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ProcessAuthorityError> {
        std::fs::create_dir_all(root.as_ref()).map_err(ProcessAuthorityError::Io)?;
        let mut connection = Connection::open(root.as_ref().join("process-authority.db"))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(ProcessAuthorityError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                schema::migrate_v1(&mut connection)?;
                schema::migrate_v2(&mut connection)?;
            }
            1 => schema::migrate_v2(&mut connection)?,
            schema::SCHEMA_VERSION => {}
            other => return Err(ProcessAuthorityError::SchemaVersionUnsupported(other)),
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Creates an authority-assigned isolation domain idempotently.
    ///
    /// # Errors
    ///
    /// Fails on idempotency rebinding, generation corruption, or storage
    /// failure.
    pub fn create_isolation_domain(
        &self,
        request: CreateIsolationDomainRequest,
    ) -> Result<IsolationDomainDecision, ProcessAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_domain_by_create_key(&transaction, request.idempotency_key)? {
            if existing.policy_digest != request.policy_digest {
                return Err(ProcessAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(IsolationDomainDecision::Replayed(existing));
        }

        let domain_id = IsolationDomainId::from_bytes(derive_id(
            b"nlos/isolation-domain/id/v1",
            &[request.idempotency_key.as_bytes(), &request.policy_digest],
        ));
        if load_domain_optional(&transaction, domain_id)?.is_some() {
            return Err(ProcessAuthorityError::IdempotencyConflict);
        }
        let generation = Generation::INITIAL;
        let fencing_token = derive_token(
            b"nlos/isolation-domain/fence/v1",
            &[
                domain_id.as_bytes(),
                &generation.get().to_be_bytes(),
                request.idempotency_key.as_bytes(),
            ],
        );
        let record = IsolationDomainRecord {
            isolation_domain_id: domain_id,
            generation,
            fencing_token,
            policy_digest: request.policy_digest,
            created_at_ms: request.created_at_ms,
        };
        transaction.execute(
            "INSERT INTO isolation_domains (
                isolation_domain_id, create_idempotency_key, current_generation,
                current_fencing_token, policy_digest, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                domain_id.as_bytes().as_slice(),
                request.idempotency_key.as_bytes().as_slice(),
                encode_generation(generation)?,
                fencing_token.as_slice(),
                request.policy_digest.as_slice(),
                encode_u64(request.created_at_ms)?,
            ],
        )?;
        insert_domain_generation(&transaction, &record)?;
        transaction.commit()?;
        Ok(IsolationDomainDecision::Created(record))
    }

    /// Advances a domain generation and fencing token with compare-and-swap.
    ///
    /// # Errors
    ///
    /// Fails when the expected fence is stale, the key is rebound, the
    /// generation is exhausted, or storage fails.
    pub fn rotate_isolation_domain(
        &self,
        request: RotateIsolationDomainRequest,
    ) -> Result<IsolationDomainRotationDecision, ProcessAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(replay) = load_rotation_by_key(&transaction, request.idempotency_key)? {
            let exact = replay.isolation_domain_id == request.isolation_domain_id
                && replay.expected_generation == request.expected_generation
                && replay.expected_fencing_token == request.expected_fencing_token;
            if !exact {
                return Err(ProcessAuthorityError::IdempotencyConflict);
            }
            let record = load_domain_generation(
                &transaction,
                replay.isolation_domain_id,
                replay.resulting_generation,
            )?;
            if record.fencing_token != replay.resulting_fencing_token {
                return Err(ProcessAuthorityError::CorruptRecord(
                    "rotation receipt disagrees with domain generation",
                ));
            }
            transaction.commit()?;
            return Ok(IsolationDomainRotationDecision::Replayed(record));
        }

        let current = load_domain_optional(&transaction, request.isolation_domain_id)?.ok_or(
            ProcessAuthorityError::IsolationDomainNotFound(request.isolation_domain_id),
        )?;
        if current.generation != request.expected_generation
            || current.fencing_token != request.expected_fencing_token
        {
            return Err(ProcessAuthorityError::IsolationDomainFenceConflict);
        }
        let generation = current
            .generation
            .checked_next()
            .ok_or(ProcessAuthorityError::GenerationExhausted)?;
        let fencing_token = derive_token(
            b"nlos/isolation-domain/fence/v1",
            &[
                current.isolation_domain_id.as_bytes(),
                &generation.get().to_be_bytes(),
                request.idempotency_key.as_bytes(),
            ],
        );
        let record = IsolationDomainRecord {
            isolation_domain_id: current.isolation_domain_id,
            generation,
            fencing_token,
            policy_digest: current.policy_digest,
            created_at_ms: request.rotated_at_ms,
        };
        insert_domain_generation(&transaction, &record)?;
        transaction.execute(
            "INSERT INTO isolation_domain_rotations (
                idempotency_key, isolation_domain_id, expected_generation,
                expected_fencing_token, resulting_generation, resulting_fencing_token,
                rotated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                request.idempotency_key.as_bytes().as_slice(),
                request.isolation_domain_id.as_bytes().as_slice(),
                encode_generation(request.expected_generation)?,
                request.expected_fencing_token.as_slice(),
                encode_generation(generation)?,
                fencing_token.as_slice(),
                encode_u64(request.rotated_at_ms)?,
            ],
        )?;
        let changed = transaction.execute(
            "UPDATE isolation_domains
             SET current_generation = ?1, current_fencing_token = ?2, updated_at_ms = ?3
             WHERE isolation_domain_id = ?4
               AND current_generation = ?5 AND current_fencing_token = ?6",
            params![
                encode_generation(generation)?,
                fencing_token.as_slice(),
                encode_u64(request.rotated_at_ms)?,
                request.isolation_domain_id.as_bytes().as_slice(),
                encode_generation(request.expected_generation)?,
                request.expected_fencing_token.as_slice(),
            ],
        )?;
        if changed != 1 {
            return Err(ProcessAuthorityError::IsolationDomainFenceConflict);
        }
        transaction.commit()?;
        Ok(IsolationDomainRotationDecision::Rotated(record))
    }

    /// Allocates and durably binds one delegated Process/AgentInstance pair.
    ///
    /// # Errors
    ///
    /// Fails when the domain is stale, a key/identity is rebound, or storage
    /// cannot commit the binding.
    pub fn register_delegated_process(
        &self,
        request: RegisterDelegatedProcessRequest,
    ) -> Result<ProcessBindingDecision, ProcessAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_binding_by_key(&transaction, request.idempotency_key)? {
            if !binding_matches_registration(&existing, &request) {
                return Err(ProcessAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(ProcessBindingDecision::Replayed(existing));
        }
        ensure_domain_active(
            &transaction,
            request.isolation_domain_id,
            request.isolation_domain_generation,
            request.isolation_domain_fencing_token,
        )?;

        let process_id = ProcessId::from_bytes(derive_id(
            b"nlos/process/id/v1",
            &[
                request.idempotency_key.as_bytes(),
                request.task_id.as_bytes(),
                request.task_attempt_id.as_bytes(),
                &request.attempt_generation.get().to_be_bytes(),
            ],
        ));
        let agent_instance_id = AgentInstanceId::from_bytes(derive_id(
            b"nlos/agent-instance/id/v1",
            &[request.idempotency_key.as_bytes(), process_id.as_bytes()],
        ));
        if load_process_head_optional(&transaction, process_id)?.is_some() {
            return Err(ProcessAuthorityError::IdempotencyConflict);
        }
        let process_generation = Generation::INITIAL;
        let process_fencing_token = derive_token(
            b"nlos/process/fence/v1",
            &[
                process_id.as_bytes(),
                &process_generation.get().to_be_bytes(),
                request.idempotency_key.as_bytes(),
            ],
        );
        let record = ProcessBindingRecord {
            process_id,
            process_generation,
            process_fencing_token,
            agent_instance_id,
            agent_instance_generation: Generation::INITIAL,
            task_id: request.task_id,
            task_attempt_id: request.task_attempt_id,
            attempt_generation: request.attempt_generation,
            isolation_domain_id: request.isolation_domain_id,
            isolation_domain_generation: request.isolation_domain_generation,
            isolation_domain_fencing_token: request.isolation_domain_fencing_token,
            prior_process_generation: None,
            idempotency_key: request.idempotency_key,
            created_at_ms: request.created_at_ms,
        };
        insert_process_head(&transaction, &record)?;
        insert_process_binding(&transaction, &record)?;
        transaction.commit()?;
        Ok(ProcessBindingDecision::Registered(record))
    }

    /// Restores a Process as a new Process and `AgentInstance` generation.
    ///
    /// # Errors
    ///
    /// Fails when either expected fence is stale, replay bytes conflict,
    /// generation space is exhausted, or storage cannot commit atomically.
    pub fn restore_process(
        &self,
        request: RestoreProcessRequest,
    ) -> Result<RestoreProcessDecision, ProcessAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_binding_by_key(&transaction, request.idempotency_key)? {
            let prior_generation =
                existing
                    .prior_process_generation
                    .ok_or(ProcessAuthorityError::CorruptRecord(
                        "restore binding has no predecessor",
                    ))?;
            let prior = load_process_binding(&transaction, existing.process_id, prior_generation)?;
            if !binding_matches_restore(&existing, &request)
                || prior.process_fencing_token != request.expected_process_fencing_token
            {
                return Err(ProcessAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(RestoreProcessDecision::Replayed(existing));
        }
        ensure_domain_active(
            &transaction,
            request.isolation_domain_id,
            request.isolation_domain_generation,
            request.isolation_domain_fencing_token,
        )?;
        let head = load_process_head_optional(&transaction, request.process_id)?
            .ok_or(ProcessAuthorityError::ProcessNotFound(request.process_id))?;
        if head.process_generation != request.expected_process_generation
            || head.process_fencing_token != request.expected_process_fencing_token
        {
            return Err(ProcessAuthorityError::ProcessFenceConflict);
        }
        let prior = load_process_binding(
            &transaction,
            request.process_id,
            request.expected_process_generation,
        )?;
        let process_generation = prior
            .process_generation
            .checked_next()
            .ok_or(ProcessAuthorityError::GenerationExhausted)?;
        let agent_instance_generation = prior
            .agent_instance_generation
            .checked_next()
            .ok_or(ProcessAuthorityError::GenerationExhausted)?;
        let process_fencing_token = derive_token(
            b"nlos/process/fence/v1",
            &[
                prior.process_id.as_bytes(),
                &process_generation.get().to_be_bytes(),
                request.idempotency_key.as_bytes(),
            ],
        );
        let record = ProcessBindingRecord {
            process_id: prior.process_id,
            process_generation,
            process_fencing_token,
            agent_instance_id: prior.agent_instance_id,
            agent_instance_generation,
            task_id: prior.task_id,
            task_attempt_id: prior.task_attempt_id,
            attempt_generation: prior.attempt_generation,
            isolation_domain_id: request.isolation_domain_id,
            isolation_domain_generation: request.isolation_domain_generation,
            isolation_domain_fencing_token: request.isolation_domain_fencing_token,
            prior_process_generation: Some(prior.process_generation),
            idempotency_key: request.idempotency_key,
            created_at_ms: request.restored_at_ms,
        };
        insert_process_binding(&transaction, &record)?;
        let changed = transaction.execute(
            "UPDATE process_heads SET
                current_generation = ?1, current_fencing_token = ?2,
                current_agent_generation = ?3, updated_at_ms = ?4
             WHERE process_id = ?5 AND current_generation = ?6 AND current_fencing_token = ?7",
            params![
                encode_generation(record.process_generation)?,
                record.process_fencing_token.as_slice(),
                encode_generation(record.agent_instance_generation)?,
                encode_u64(record.created_at_ms)?,
                record.process_id.as_bytes().as_slice(),
                encode_generation(request.expected_process_generation)?,
                request.expected_process_fencing_token.as_slice(),
            ],
        )?;
        if changed != 1 {
            return Err(ProcessAuthorityError::ProcessFenceConflict);
        }
        transaction.commit()?;
        Ok(RestoreProcessDecision::Restored(record))
    }

    /// Returns the current binding only when both the Process and its
    /// `IsolationDomain` generation/token are still authoritative.
    ///
    /// # Errors
    ///
    /// Fails when the Process is unknown, its referenced domain is stale, or
    /// the durable record cannot be decoded.
    pub fn inspect_active_process_binding(
        &self,
        process_id: ProcessId,
    ) -> Result<ProcessBindingRecord, ProcessAuthorityError> {
        let connection = self.lock()?;
        let head = load_process_head_optional(&connection, process_id)?
            .ok_or(ProcessAuthorityError::ProcessNotFound(process_id))?;
        let record = load_process_binding(&connection, process_id, head.process_generation)?;
        if record.process_fencing_token != head.process_fencing_token
            || record.agent_instance_id != head.agent_instance_id
            || record.agent_instance_generation != head.agent_instance_generation
        {
            return Err(ProcessAuthorityError::CorruptRecord(
                "process head disagrees with current immutable binding",
            ));
        }
        ensure_domain_active(
            &connection,
            record.isolation_domain_id,
            record.isolation_domain_generation,
            record.isolation_domain_fencing_token,
        )?;
        Ok(record)
    }

    /// Fail-closed readback intended for a later `TaskWriteSet` builder.
    ///
    /// # Errors
    ///
    /// Fails when any Process, `AgentInstance`, or `IsolationDomain` identity,
    /// generation, or fence differs from the current authority record.
    pub fn verify_active_process_binding(
        &self,
        expected: &ActiveProcessBinding,
    ) -> Result<ProcessBindingRecord, ProcessAuthorityError> {
        let record = self.inspect_active_process_binding(expected.process_id)?;
        if ActiveProcessBinding::from(&record) != *expected {
            return Err(ProcessAuthorityError::StaleProcessBinding);
        }
        Ok(record)
    }

    /// Reads the authority-derived participant proof for the current Process
    /// binding. The proof is deterministic from the immutable Process identity
    /// and current generation, but is only authoritative after this readback.
    ///
    /// # Errors
    ///
    /// Fails when the Process or its current domain fence is stale, unknown,
    /// or corrupt.
    pub fn inspect_binding_endpoint_proof(
        &self,
        process_id: ProcessId,
    ) -> Result<ProcessBindingEndpointProof, ProcessAuthorityError> {
        let record = self.inspect_active_process_binding(process_id)?;
        let participant_id = nlos_types::TaskParticipantId::from_bytes(derive_id(
            b"nlos/process-binding/participant/v1",
            &[record.process_id.as_bytes()],
        ));
        let admission_receipt_id = nlos_types::ReceiptId::from_bytes(derive_id(
            b"nlos/process-binding/admission/v1",
            &[
                record.process_id.as_bytes(),
                &record.process_generation.get().to_be_bytes(),
                record.process_fencing_token.as_slice(),
            ],
        ));
        Ok(ProcessBindingEndpointProof {
            process_id: record.process_id,
            participant_id,
            participant_generation: record.process_generation,
            admission_receipt_id,
        })
    }

    /// Reads the current domain generation and fence.
    ///
    /// # Errors
    ///
    /// Fails when the domain does not exist or its durable record is invalid.
    pub fn inspect_isolation_domain(
        &self,
        domain_id: IsolationDomainId,
    ) -> Result<IsolationDomainRecord, ProcessAuthorityError> {
        let connection = self.lock()?;
        load_domain_optional(&connection, domain_id)?
            .ok_or(ProcessAuthorityError::IsolationDomainNotFound(domain_id))
    }

    /// Registers one fiber incarnation for `binding` under `process_id`
    /// (ADR-0012 decision 3): the durable generation/fence authority
    /// B-PROCESS-001 in its fiber-borrowing role. The registration CAS's
    /// against the process binding's current generation/fencing token, and
    /// the binding's incarnation generation advances by exactly one per
    /// registration (`1`, then `prior + 1`), mirroring the process
    /// generation rotation mechanism family.
    ///
    /// Order of the fail-closed gates, all of which run before any durable
    /// write:
    ///
    /// 1. the all-zero binding is rejected
    ///    ([`ProcessAuthorityError::InvalidFiberBinding`]);
    /// 2. an exact idempotency replay returns the original record
    ///    ([`FiberIncarnationDecision::Replayed`]); a key rebound to a
    ///    different process, binding or process fence is an
    ///    [`ProcessAuthorityError::IdempotencyConflict`];
    /// 3. an unknown process fails closed
    ///    ([`ProcessAuthorityError::ProcessNotFound`]);
    /// 4. the process binding's current generation/fencing token must equal
    ///    the presented one ([`ProcessAuthorityError::StaleProcessBinding`]
    ///    otherwise) — a stale incarnation registration takes zero durable
    ///    side effect, the ADR-0012 gate the replay re-drive leans on;
    /// 5. the incarnation increment commits with a compare-and-swap on the
    ///    binding's head row.
    ///
    /// The binding's entry snapshot slot is deliberately left untouched: the
    /// ADR-0012 latest-only slot is shared across incarnations precisely so
    /// a new incarnation can consume the previous invocation's snapshot in
    /// the crash-window recovery; only the explicit terminal GC (or the next
    /// invocation's overwrite) removes it.
    ///
    /// # Errors
    /// Fails closed for an invalid binding, idempotency rebinding, an
    /// unknown process, a stale process fence, an exhausted generation
    /// space, or a storage/corruption failure.
    pub fn register_fiber_incarnation(
        &self,
        request: RegisterFiberIncarnationRequest,
    ) -> Result<FiberIncarnationDecision, ProcessAuthorityError> {
        if is_zero_binding(request.binding) {
            return Err(ProcessAuthorityError::InvalidFiberBinding);
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_incarnation_by_key(&transaction, request.idempotency_key)? {
            let exact = existing.process_id == request.process_id
                && existing.binding == request.binding
                && existing.process_generation == request.expected_process_generation
                && existing.process_fencing_token == request.expected_process_fencing_token;
            if !exact {
                return Err(ProcessAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(FiberIncarnationDecision::Replayed(existing));
        }

        let head = load_process_head_optional(&transaction, request.process_id)?
            .ok_or(ProcessAuthorityError::ProcessNotFound(request.process_id))?;
        if head.process_generation != request.expected_process_generation
            || head.process_fencing_token != request.expected_process_fencing_token
        {
            return Err(ProcessAuthorityError::StaleProcessBinding);
        }
        let current =
            load_incarnation_head_optional(&transaction, request.process_id, request.binding)?;
        let (incarnation_generation, prior) = match current {
            None => (Generation::INITIAL, None),
            Some(head) => (
                head.current_incarnation
                    .checked_next()
                    .ok_or(ProcessAuthorityError::GenerationExhausted)?,
                Some(head.current_incarnation),
            ),
        };
        let fencing_token = derive_token(
            b"nlos/process/fiber-incarnation/fence/v1",
            &[
                request.process_id.as_bytes(),
                request.binding.as_bytes(),
                &incarnation_generation.get().to_be_bytes(),
                request.idempotency_key.as_bytes(),
            ],
        );
        let record = FiberIncarnationRecord {
            process_id: request.process_id,
            binding: request.binding,
            incarnation_generation,
            fencing_token,
            process_generation: head.process_generation,
            process_fencing_token: head.process_fencing_token,
            prior_incarnation_generation: prior,
            idempotency_key: request.idempotency_key,
            created_at_ms: request.registered_at_ms,
        };
        insert_incarnation(&transaction, &record)?;
        commit_incarnation_head(&transaction, &record, current)?;
        transaction.commit()?;
        Ok(FiberIncarnationDecision::Registered(record))
    }

    /// Reads the binding's current durable incarnation after cross-checking
    /// the mutable head row against the immutable incarnation row.
    ///
    /// # Errors
    ///
    /// Fails closed for an invalid binding, an unregistered binding, or a
    /// head/row disagreement.
    pub fn inspect_fiber_incarnation(
        &self,
        process_id: ProcessId,
        binding: ExecutionFiberId,
    ) -> Result<FiberIncarnationRecord, ProcessAuthorityError> {
        if is_zero_binding(binding) {
            return Err(ProcessAuthorityError::InvalidFiberBinding);
        }
        let connection = self.lock()?;
        let head = load_incarnation_head_optional(&connection, process_id, binding)?
            .ok_or(ProcessAuthorityError::FiberIncarnationNotFound)?;
        let record =
            load_incarnation_row(&connection, process_id, binding, head.current_incarnation)?;
        if record.fencing_token != head.current_fencing_token {
            return Err(ProcessAuthorityError::CorruptRecord(
                "fiber incarnation head disagrees with its immutable row",
            ));
        }
        Ok(record)
    }

    /// Writes the binding's handler-entry snapshot (ADR-0012 decision 2, the
    /// B path's durable face). Latest-only per invocation: the binding owns
    /// exactly one snapshot slot and every write overwrites it.
    ///
    /// Fail-closed gates before any durable write: the all-zero binding
    /// ([`ProcessAuthorityError::InvalidFiberBinding`]), an empty input
    /// ([`ProcessAuthorityError::InvalidFiberSnapshot`]), an unregistered
    /// binding ([`ProcessAuthorityError::FiberIncarnationNotFound`]), and the
    /// incarnation CAS — `expected_incarnation_generation` must equal the
    /// binding's current registered incarnation
    /// ([`ProcessAuthorityError::StaleFiberIncarnation`], zero side effect).
    /// Writing the same input bytes again replays
    /// ([`FiberEntrySnapshotDecision::Replayed`]); different bytes overwrite
    /// (latest wins).
    ///
    /// # Errors
    ///
    /// Fails closed for an invalid binding, an invalid input, an unregistered
    /// binding, a stale incarnation, or a storage/corruption failure.
    pub fn write_fiber_entry_snapshot(
        &self,
        request: WriteFiberEntrySnapshotRequest,
    ) -> Result<FiberEntrySnapshotDecision, ProcessAuthorityError> {
        if is_zero_binding(request.binding) {
            return Err(ProcessAuthorityError::InvalidFiberBinding);
        }
        if request.handler_input.is_empty() {
            return Err(ProcessAuthorityError::InvalidFiberSnapshot(
                "handler entry input must be non-empty",
            ));
        }
        let input_digest = derive_token(
            b"nlos/process/fiber-entry-snapshot/v1",
            &[request.handler_input.as_slice()],
        );
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let head =
            load_incarnation_head_optional(&transaction, request.process_id, request.binding)?
                .ok_or(ProcessAuthorityError::FiberIncarnationNotFound)?;
        if head.current_incarnation != request.expected_incarnation_generation {
            return Err(ProcessAuthorityError::StaleFiberIncarnation);
        }
        let existing = load_snapshot_row(&transaction, request.process_id, request.binding)?;
        if let Some(existing) = &existing
            && existing.handler_input == request.handler_input
        {
            transaction.commit()?;
            return Ok(FiberEntrySnapshotDecision::Replayed(existing.clone()));
        }
        let record = FiberEntrySnapshotRecord {
            process_id: request.process_id,
            binding: request.binding,
            handler_input: request.handler_input,
            input_digest,
            written_by_incarnation: head.current_incarnation,
            written_at_ms: request.written_at_ms,
        };
        transaction.execute(
            "INSERT INTO fiber_entry_snapshots (
                process_id, binding_id, handler_input, input_digest,
                written_by_incarnation, written_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(process_id, binding_id) DO UPDATE SET
                handler_input = excluded.handler_input,
                input_digest = excluded.input_digest,
                written_by_incarnation = excluded.written_by_incarnation,
                written_at_ms = excluded.written_at_ms",
            params![
                record.process_id.as_bytes().as_slice(),
                record.binding.as_bytes().as_slice(),
                record.handler_input.as_slice(),
                record.input_digest.as_slice(),
                encode_generation(record.written_by_incarnation)?,
                encode_u64(record.written_at_ms)?,
            ],
        )?;
        transaction.commit()?;
        Ok(FiberEntrySnapshotDecision::Written(record))
    }

    /// Reads the binding's latest handler-entry snapshot (the restore-side
    /// read of the B path). The snapshot is revalidated against its stored
    /// integrity digest before it is returned.
    ///
    /// # Errors
    ///
    /// Fails closed for an invalid binding, an unregistered binding
    /// ([`ProcessAuthorityError::FiberIncarnationNotFound`]), a missing
    /// snapshot ([`ProcessAuthorityError::FiberSnapshotNotFound`]), or a
    /// tampered row.
    pub fn inspect_fiber_entry_snapshot(
        &self,
        process_id: ProcessId,
        binding: ExecutionFiberId,
    ) -> Result<FiberEntrySnapshotRecord, ProcessAuthorityError> {
        if is_zero_binding(binding) {
            return Err(ProcessAuthorityError::InvalidFiberBinding);
        }
        let connection = self.lock()?;
        load_incarnation_head_optional(&connection, process_id, binding)?
            .ok_or(ProcessAuthorityError::FiberIncarnationNotFound)?;
        let record = load_snapshot_row(&connection, process_id, binding)?
            .ok_or(ProcessAuthorityError::FiberSnapshotNotFound)?;
        let expected = derive_token(
            b"nlos/process/fiber-entry-snapshot/v1",
            &[record.handler_input.as_slice()],
        );
        if record.input_digest != expected {
            return Err(ProcessAuthorityError::CorruptRecord(
                "entry snapshot input digest disagrees with its bytes",
            ));
        }
        Ok(record)
    }

    /// Garbage-collects the binding's handler-entry snapshot (the terminal
    /// GC of the latest-only retention policy: the snapshot is either
    /// consumed by its recovery or disappears with the fiber's terminal
    /// state). Returns whether a snapshot row was deleted; deleting an
    /// already-absent snapshot is the idempotent `false`.
    ///
    /// # Errors
    ///
    /// Fails closed for an invalid binding or a storage failure.
    pub fn gc_fiber_entry_snapshot(
        &self,
        process_id: ProcessId,
        binding: ExecutionFiberId,
    ) -> Result<bool, ProcessAuthorityError> {
        if is_zero_binding(binding) {
            return Err(ProcessAuthorityError::InvalidFiberBinding);
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "DELETE FROM fiber_entry_snapshots
             WHERE process_id = ?1 AND binding_id = ?2",
            params![
                process_id.as_bytes().as_slice(),
                binding.as_bytes().as_slice(),
            ],
        )?;
        transaction.commit()?;
        Ok(changed > 0)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, ProcessAuthorityError> {
        self.connection
            .lock()
            .map_err(|_| ProcessAuthorityError::LockPoisoned)
    }
}

#[derive(Clone)]
struct ProcessHead {
    process_generation: Generation,
    process_fencing_token: FencingToken,
    agent_instance_id: AgentInstanceId,
    agent_instance_generation: Generation,
}

struct RotationReplay {
    isolation_domain_id: IsolationDomainId,
    expected_generation: Generation,
    expected_fencing_token: FencingToken,
    resulting_generation: Generation,
    resulting_fencing_token: FencingToken,
}

fn ensure_domain_active(
    connection: &Connection,
    domain_id: IsolationDomainId,
    generation: Generation,
    token: FencingToken,
) -> Result<(), ProcessAuthorityError> {
    let current = load_domain_optional(connection, domain_id)?
        .ok_or(ProcessAuthorityError::IsolationDomainNotFound(domain_id))?;
    if current.generation != generation || current.fencing_token != token {
        return Err(ProcessAuthorityError::StaleIsolationDomain);
    }
    Ok(())
}

fn is_zero_binding(binding: ExecutionFiberId) -> bool {
    binding.as_bytes().iter().all(|&byte| byte == 0)
}

#[derive(Clone, Copy)]
struct IncarnationHead {
    current_incarnation: Generation,
    current_fencing_token: FencingToken,
}

fn load_incarnation_head_optional(
    connection: &Connection,
    process_id: ProcessId,
    binding: ExecutionFiberId,
) -> Result<Option<IncarnationHead>, ProcessAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT current_incarnation, current_fencing_token
             FROM fiber_incarnation_heads WHERE process_id = ?1 AND binding_id = ?2",
            params![
                process_id.as_bytes().as_slice(),
                binding.as_bytes().as_slice()
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    raw.map(|(generation, token)| {
        Ok(IncarnationHead {
            current_incarnation: decode_generation(generation)?,
            current_fencing_token: array32(token)?,
        })
    })
    .transpose()
}

fn load_incarnation_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<FiberIncarnationRecord>, ProcessAuthorityError> {
    let identity = connection
        .query_row(
            "SELECT process_id, binding_id, incarnation_generation
             FROM fiber_incarnations WHERE idempotency_key = ?1",
            [key.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;
    identity
        .map(|(process_id, binding, generation)| {
            load_incarnation_row(
                connection,
                ProcessId::from_bytes(array16(process_id)?),
                ExecutionFiberId::from_bytes(array16(binding)?),
                decode_generation(generation)?,
            )
        })
        .transpose()
}

fn load_incarnation_row(
    connection: &Connection,
    process_id: ProcessId,
    binding: ExecutionFiberId,
    incarnation_generation: Generation,
) -> Result<FiberIncarnationRecord, ProcessAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT fencing_token, process_generation, process_fencing_token,
                    prior_incarnation, idempotency_key, created_at_ms
             FROM fiber_incarnations
             WHERE process_id = ?1 AND binding_id = ?2 AND incarnation_generation = ?3",
            params![
                process_id.as_bytes().as_slice(),
                binding.as_bytes().as_slice(),
                encode_generation(incarnation_generation)?,
            ],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()?
        .ok_or(ProcessAuthorityError::CorruptRecord(
            "fiber incarnation head references an absent immutable row",
        ))?;
    Ok(FiberIncarnationRecord {
        process_id,
        binding,
        incarnation_generation,
        fencing_token: array32(raw.0)?,
        process_generation: decode_generation(raw.1)?,
        process_fencing_token: array32(raw.2)?,
        prior_incarnation_generation: raw.3.map(decode_generation).transpose()?,
        idempotency_key: IdempotencyKey::from_bytes(array16(raw.4)?),
        created_at_ms: decode_u64(raw.5)?,
    })
}

fn commit_incarnation_head(
    transaction: &Transaction<'_>,
    record: &FiberIncarnationRecord,
    current: Option<IncarnationHead>,
) -> Result<(), ProcessAuthorityError> {
    match current {
        None => {
            transaction.execute(
                "INSERT INTO fiber_incarnation_heads (
                        process_id, binding_id, current_incarnation,
                        current_fencing_token, updated_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    record.process_id.as_bytes().as_slice(),
                    record.binding.as_bytes().as_slice(),
                    encode_generation(record.incarnation_generation)?,
                    record.fencing_token.as_slice(),
                    encode_u64(record.created_at_ms)?,
                ],
            )?;
        }
        Some(head) => {
            let changed = transaction.execute(
                "UPDATE fiber_incarnation_heads SET
                        current_incarnation = ?1, current_fencing_token = ?2,
                        updated_at_ms = ?3
                     WHERE process_id = ?4 AND binding_id = ?5
                       AND current_incarnation = ?6",
                params![
                    encode_generation(record.incarnation_generation)?,
                    record.fencing_token.as_slice(),
                    encode_u64(record.created_at_ms)?,
                    record.process_id.as_bytes().as_slice(),
                    record.binding.as_bytes().as_slice(),
                    encode_generation(head.current_incarnation)?,
                ],
            )?;
            if changed != 1 {
                return Err(ProcessAuthorityError::CorruptRecord(
                    "fiber incarnation head compare-and-swap lost",
                ));
            }
        }
    }
    Ok(())
}

fn insert_incarnation(
    transaction: &Transaction<'_>,
    record: &FiberIncarnationRecord,
) -> Result<(), ProcessAuthorityError> {
    transaction.execute(
        "INSERT INTO fiber_incarnations (
            process_id, binding_id, incarnation_generation, fencing_token,
            process_generation, process_fencing_token, prior_incarnation,
            idempotency_key, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            record.process_id.as_bytes().as_slice(),
            record.binding.as_bytes().as_slice(),
            encode_generation(record.incarnation_generation)?,
            record.fencing_token.as_slice(),
            encode_generation(record.process_generation)?,
            record.process_fencing_token.as_slice(),
            record
                .prior_incarnation_generation
                .map(encode_generation)
                .transpose()?,
            record.idempotency_key.as_bytes().as_slice(),
            encode_u64(record.created_at_ms)?,
        ],
    )?;
    Ok(())
}

fn load_snapshot_row(
    connection: &Connection,
    process_id: ProcessId,
    binding: ExecutionFiberId,
) -> Result<Option<FiberEntrySnapshotRecord>, ProcessAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT handler_input, input_digest, written_by_incarnation, written_at_ms
             FROM fiber_entry_snapshots WHERE process_id = ?1 AND binding_id = ?2",
            params![
                process_id.as_bytes().as_slice(),
                binding.as_bytes().as_slice()
            ],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    raw.map(|(input, digest, incarnation, written_at)| {
        Ok(FiberEntrySnapshotRecord {
            process_id,
            binding,
            handler_input: input,
            input_digest: array32(digest)?,
            written_by_incarnation: decode_generation(incarnation)?,
            written_at_ms: decode_u64(written_at)?,
        })
    })
    .transpose()
}

fn insert_domain_generation(
    transaction: &Transaction<'_>,
    record: &IsolationDomainRecord,
) -> Result<(), ProcessAuthorityError> {
    transaction.execute(
        "INSERT INTO isolation_domain_generations (
            isolation_domain_id, generation, fencing_token, policy_digest, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            record.isolation_domain_id.as_bytes().as_slice(),
            encode_generation(record.generation)?,
            record.fencing_token.as_slice(),
            record.policy_digest.as_slice(),
            encode_u64(record.created_at_ms)?,
        ],
    )?;
    Ok(())
}

fn insert_process_head(
    transaction: &Transaction<'_>,
    record: &ProcessBindingRecord,
) -> Result<(), ProcessAuthorityError> {
    transaction.execute(
        "INSERT INTO process_heads (
            process_id, current_generation, current_fencing_token, agent_instance_id,
            current_agent_generation, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![
            record.process_id.as_bytes().as_slice(),
            encode_generation(record.process_generation)?,
            record.process_fencing_token.as_slice(),
            record.agent_instance_id.as_bytes().as_slice(),
            encode_generation(record.agent_instance_generation)?,
            encode_u64(record.created_at_ms)?,
        ],
    )?;
    Ok(())
}

fn insert_process_binding(
    transaction: &Transaction<'_>,
    record: &ProcessBindingRecord,
) -> Result<(), ProcessAuthorityError> {
    transaction.execute(
        "INSERT INTO process_bindings (
            process_id, process_generation, process_fencing_token,
            agent_instance_id, agent_instance_generation, task_id, task_attempt_id,
            attempt_generation, isolation_domain_id, isolation_domain_generation,
            isolation_domain_fencing_token, prior_process_generation,
            idempotency_key, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            record.process_id.as_bytes().as_slice(),
            encode_generation(record.process_generation)?,
            record.process_fencing_token.as_slice(),
            record.agent_instance_id.as_bytes().as_slice(),
            encode_generation(record.agent_instance_generation)?,
            record.task_id.as_bytes().as_slice(),
            record.task_attempt_id.as_bytes().as_slice(),
            encode_generation(record.attempt_generation)?,
            record.isolation_domain_id.as_bytes().as_slice(),
            encode_generation(record.isolation_domain_generation)?,
            record.isolation_domain_fencing_token.as_slice(),
            record
                .prior_process_generation
                .map(encode_generation)
                .transpose()?,
            record.idempotency_key.as_bytes().as_slice(),
            encode_u64(record.created_at_ms)?,
        ],
    )?;
    Ok(())
}

fn load_domain_by_create_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<IsolationDomainRecord>, ProcessAuthorityError> {
    let domain_id = connection
        .query_row(
            "SELECT isolation_domain_id FROM isolation_domains WHERE create_idempotency_key = ?1",
            [key.as_bytes().as_slice()],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    domain_id
        .map(|bytes| {
            load_domain_generation(
                connection,
                IsolationDomainId::from_bytes(array16(bytes)?),
                Generation::INITIAL,
            )
            .map(Some)
        })
        .transpose()
        .map(Option::flatten)
}

fn load_domain_optional(
    connection: &Connection,
    domain_id: IsolationDomainId,
) -> Result<Option<IsolationDomainRecord>, ProcessAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT d.current_generation, d.current_fencing_token, d.policy_digest, g.created_at_ms
             FROM isolation_domains d
             JOIN isolation_domain_generations g
               ON g.isolation_domain_id = d.isolation_domain_id
              AND g.generation = d.current_generation
             WHERE d.isolation_domain_id = ?1",
            [domain_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    raw.map(|(generation, token, policy, created)| {
        Ok(IsolationDomainRecord {
            isolation_domain_id: domain_id,
            generation: decode_generation(generation)?,
            fencing_token: array32(token)?,
            policy_digest: array32(policy)?,
            created_at_ms: decode_u64(created)?,
        })
    })
    .transpose()
}

fn load_domain_generation(
    connection: &Connection,
    domain_id: IsolationDomainId,
    generation: Generation,
) -> Result<IsolationDomainRecord, ProcessAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT fencing_token, policy_digest, created_at_ms
             FROM isolation_domain_generations
             WHERE isolation_domain_id = ?1 AND generation = ?2",
            params![
                domain_id.as_bytes().as_slice(),
                encode_generation(generation)?
            ],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or(ProcessAuthorityError::CorruptRecord(
            "domain generation referenced by receipt is absent",
        ))?;
    Ok(IsolationDomainRecord {
        isolation_domain_id: domain_id,
        generation,
        fencing_token: array32(raw.0)?,
        policy_digest: array32(raw.1)?,
        created_at_ms: decode_u64(raw.2)?,
    })
}

fn load_rotation_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<RotationReplay>, ProcessAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT isolation_domain_id, expected_generation, expected_fencing_token,
                    resulting_generation, resulting_fencing_token
             FROM isolation_domain_rotations WHERE idempotency_key = ?1",
            [key.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            },
        )
        .optional()?;
    raw.map(|raw| {
        Ok(RotationReplay {
            isolation_domain_id: IsolationDomainId::from_bytes(array16(raw.0)?),
            expected_generation: decode_generation(raw.1)?,
            expected_fencing_token: array32(raw.2)?,
            resulting_generation: decode_generation(raw.3)?,
            resulting_fencing_token: array32(raw.4)?,
        })
    })
    .transpose()
}

fn load_process_head_optional(
    connection: &Connection,
    process_id: ProcessId,
) -> Result<Option<ProcessHead>, ProcessAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT current_generation, current_fencing_token,
                    agent_instance_id, current_agent_generation
             FROM process_heads WHERE process_id = ?1",
            [process_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    raw.map(|(generation, token, agent_instance_id, agent_generation)| {
        Ok(ProcessHead {
            process_generation: decode_generation(generation)?,
            process_fencing_token: array32(token)?,
            agent_instance_id: AgentInstanceId::from_bytes(array16(agent_instance_id)?),
            agent_instance_generation: decode_generation(agent_generation)?,
        })
    })
    .transpose()
}

fn load_binding_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<ProcessBindingRecord>, ProcessAuthorityError> {
    let identity = connection
        .query_row(
            "SELECT process_id, process_generation FROM process_bindings WHERE idempotency_key = ?1",
            [key.as_bytes().as_slice()],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    identity
        .map(|(process_id, generation)| {
            load_process_binding(
                connection,
                ProcessId::from_bytes(array16(process_id)?),
                decode_generation(generation)?,
            )
        })
        .transpose()
}

fn load_process_binding(
    connection: &Connection,
    process_id: ProcessId,
    generation: Generation,
) -> Result<ProcessBindingRecord, ProcessAuthorityError> {
    type RawBinding = (
        Vec<u8>,
        Vec<u8>,
        i64,
        Vec<u8>,
        Vec<u8>,
        i64,
        Vec<u8>,
        i64,
        Vec<u8>,
        Option<i64>,
        Vec<u8>,
        i64,
    );
    let raw: RawBinding = connection
        .query_row(
            "SELECT process_fencing_token, agent_instance_id, agent_instance_generation,
                    task_id, task_attempt_id, attempt_generation, isolation_domain_id,
                    isolation_domain_generation, isolation_domain_fencing_token,
                    prior_process_generation, idempotency_key, created_at_ms
             FROM process_bindings WHERE process_id = ?1 AND process_generation = ?2",
            params![
                process_id.as_bytes().as_slice(),
                encode_generation(generation)?
            ],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                ))
            },
        )
        .optional()?
        .ok_or(ProcessAuthorityError::CorruptRecord(
            "process head references absent binding",
        ))?;
    Ok(ProcessBindingRecord {
        process_id,
        process_generation: generation,
        process_fencing_token: array32(raw.0)?,
        agent_instance_id: AgentInstanceId::from_bytes(array16(raw.1)?),
        agent_instance_generation: decode_generation(raw.2)?,
        task_id: TaskId::from_bytes(array16(raw.3)?),
        task_attempt_id: TaskAttemptId::from_bytes(array16(raw.4)?),
        attempt_generation: decode_generation(raw.5)?,
        isolation_domain_id: IsolationDomainId::from_bytes(array16(raw.6)?),
        isolation_domain_generation: decode_generation(raw.7)?,
        isolation_domain_fencing_token: array32(raw.8)?,
        prior_process_generation: raw.9.map(decode_generation).transpose()?,
        idempotency_key: IdempotencyKey::from_bytes(array16(raw.10)?),
        created_at_ms: decode_u64(raw.11)?,
    })
}

fn binding_matches_registration(
    record: &ProcessBindingRecord,
    request: &RegisterDelegatedProcessRequest,
) -> bool {
    record.prior_process_generation.is_none()
        && record.task_id == request.task_id
        && record.task_attempt_id == request.task_attempt_id
        && record.attempt_generation == request.attempt_generation
        && record.isolation_domain_id == request.isolation_domain_id
        && record.isolation_domain_generation == request.isolation_domain_generation
        && record.isolation_domain_fencing_token == request.isolation_domain_fencing_token
}

fn binding_matches_restore(record: &ProcessBindingRecord, request: &RestoreProcessRequest) -> bool {
    record.process_id == request.process_id
        && record.prior_process_generation == Some(request.expected_process_generation)
        && record.isolation_domain_id == request.isolation_domain_id
        && record.isolation_domain_generation == request.isolation_domain_generation
        && record.isolation_domain_fencing_token == request.isolation_domain_fencing_token
}

fn derive_id(tag: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let digest = derive_token(tag, parts);
    digest[..16].try_into().expect("slice has fixed length")
}

fn derive_token(tag: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update((tag.len() as u64).to_be_bytes());
    hasher.update(tag);
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn encode_generation(generation: Generation) -> Result<i64, ProcessAuthorityError> {
    encode_u64(generation.get())
}

fn encode_u64(value: u64) -> Result<i64, ProcessAuthorityError> {
    i64::try_from(value).map_err(|_| ProcessAuthorityError::CorruptRecord("u64 exceeds SQLite i64"))
}

fn decode_u64(value: i64) -> Result<u64, ProcessAuthorityError> {
    u64::try_from(value).map_err(|_| ProcessAuthorityError::CorruptRecord("negative integer"))
}

fn decode_generation(value: i64) -> Result<Generation, ProcessAuthorityError> {
    let value = decode_u64(value)?;
    NonZeroU64::new(value)
        .map(Generation::new)
        .ok_or(ProcessAuthorityError::CorruptRecord("zero generation"))
}

fn array16(bytes: Vec<u8>) -> Result<[u8; 16], ProcessAuthorityError> {
    bytes
        .try_into()
        .map_err(|_| ProcessAuthorityError::CorruptRecord("identity length is not 16"))
}

fn array32(bytes: Vec<u8>) -> Result<[u8; 32], ProcessAuthorityError> {
    bytes
        .try_into()
        .map_err(|_| ProcessAuthorityError::CorruptRecord("digest/token length is not 32"))
}
