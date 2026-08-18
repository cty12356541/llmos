use nlos_types::{Generation, ReceiptId, TaskId, TaskParticipantId, TaskParticipantRegistryId};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use crate::store::encode_u64;
use crate::{TaskRecord, TaskStoreError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParticipantType {
    TaskStore,
    ArtifactHead,
    SemanticAdmission,
    ChannelTopic,
    DriverGateway,
    ResourceLedger,
    ProcessBinding,
    OperationBinding,
}

impl ParticipantType {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::TaskStore => 1,
            Self::ArtifactHead => 2,
            Self::SemanticAdmission => 3,
            Self::ChannelTopic => 4,
            Self::DriverGateway => 5,
            Self::ResourceLedger => 6,
            Self::ProcessBinding => 7,
            Self::OperationBinding => 8,
        }
    }

    /// One-byte wire encoding used by barrier observation signature preimages.
    pub(crate) const fn wire_code(self) -> u8 {
        match self {
            Self::TaskStore => 1,
            Self::ArtifactHead => 2,
            Self::SemanticAdmission => 3,
            Self::ChannelTopic => 4,
            Self::DriverGateway => 5,
            Self::ResourceLedger => 6,
            Self::ProcessBinding => 7,
            Self::OperationBinding => 8,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            1 => Ok(Self::TaskStore),
            2 => Ok(Self::ArtifactHead),
            3 => Ok(Self::SemanticAdmission),
            4 => Ok(Self::ChannelTopic),
            5 => Ok(Self::DriverGateway),
            6 => Ok(Self::ResourceLedger),
            7 => Ok(Self::ProcessBinding),
            8 => Ok(Self::OperationBinding),
            _ => Err(TaskStoreError::CorruptRecord("participant type")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParticipantRegistryState {
    Open,
    FrozenForPermit,
    FrozenForTakeover,
    Superseded,
}

impl ParticipantRegistryState {
    const fn code(self) -> i64 {
        match self {
            Self::Open => 1,
            Self::FrozenForPermit => 2,
            Self::FrozenForTakeover => 3,
            Self::Superseded => 4,
        }
    }

    fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            1 => Ok(Self::Open),
            2 => Ok(Self::FrozenForPermit),
            3 => Ok(Self::FrozenForTakeover),
            4 => Ok(Self::Superseded),
            _ => Err(TaskStoreError::CorruptRecord("participant registry state")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParticipantRegistryBinding {
    pub generation: u64,
    pub root: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParticipantRecord {
    pub participant_type: ParticipantType,
    pub participant_id: TaskParticipantId,
    pub participant_generation: Generation,
    pub admission_receipt_id: ReceiptId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParticipantRegistryRecord {
    pub registry_id: TaskParticipantRegistryId,
    pub task_id: TaskId,
    pub task_generation: Generation,
    pub generation: u64,
    pub prior_root: [u8; 32],
    pub participants: Vec<ParticipantRecord>,
    pub root: [u8; 32],
    pub state: ParticipantRegistryState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParticipantRegistrationDecision {
    Registered(ParticipantRegistryRecord),
    Replayed(ParticipantRegistryRecord),
}

impl ParticipantRegistrationDecision {
    #[must_use]
    pub const fn registry(&self) -> &ParticipantRegistryRecord {
        match self {
            Self::Registered(registry) | Self::Replayed(registry) => registry,
        }
    }
}

const MAX_PARTICIPANTS: usize = 256;

pub(crate) fn initialize_registry(
    transaction: &Transaction<'_>,
    task: &TaskRecord,
    now_ms: i64,
) -> Result<ParticipantRegistryRecord, TaskStoreError> {
    if let Some(existing) = load_current_registry(transaction, task.task_id)? {
        validate_registry(task, &existing)?;
        return Ok(existing);
    }
    let participant_id = task_store_participant_id(transaction)?;
    let admission_receipt_id = derived_receipt_id(
        b"llmos/task-store-participant-admission/v1",
        task.task_id,
        1,
        participant_id,
    );
    let participant = ParticipantRecord {
        participant_type: ParticipantType::TaskStore,
        participant_id,
        participant_generation: Generation::INITIAL,
        admission_receipt_id,
    };
    insert_registry_generation(
        transaction,
        task,
        1,
        empty_registry_root(),
        &[participant],
        now_ms,
    )
}

pub(crate) fn freeze_for_permit(
    transaction: &Transaction<'_>,
    task: &TaskRecord,
    now_ms: i64,
) -> Result<ParticipantRegistryBinding, TaskStoreError> {
    let mut registry = initialize_registry(transaction, task, now_ms)?;
    match registry.state {
        ParticipantRegistryState::Open => {}
        ParticipantRegistryState::FrozenForPermit | ParticipantRegistryState::Superseded => {
            let next = registry
                .generation
                .checked_add(1)
                .ok_or(TaskStoreError::EpochExhausted)?;
            transaction.execute(
                "UPDATE task_participant_registries
                 SET registry_state=?1, updated_at_ms=?2
                 WHERE registry_id=?3 AND registry_state=?4",
                params![
                    ParticipantRegistryState::Superseded.code(),
                    now_ms,
                    registry.registry_id.as_bytes().as_slice(),
                    registry.state.code(),
                ],
            )?;
            registry = insert_registry_generation(
                transaction,
                task,
                next,
                registry.root,
                &registry.participants,
                now_ms,
            )?;
        }
        ParticipantRegistryState::FrozenForTakeover => {
            return Err(TaskStoreError::ParticipantRegistryFrozen {
                state: registry.state,
            });
        }
    }
    let changed = transaction.execute(
        "UPDATE task_participant_registries
         SET registry_state=?1, updated_at_ms=?2
         WHERE registry_id=?3 AND registry_state=?4",
        params![
            ParticipantRegistryState::FrozenForPermit.code(),
            now_ms,
            registry.registry_id.as_bytes().as_slice(),
            ParticipantRegistryState::Open.code(),
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::ParticipantRegistryCasMismatch);
    }
    let self_participant = registry
        .participants
        .first()
        .ok_or(TaskStoreError::CorruptRecord("empty participant registry"))?;
    let receipt_id = derived_receipt_id(
        b"llmos/task-participant-registry-freeze/v1",
        task.task_id,
        registry.generation,
        self_participant.participant_id,
    );
    transaction.execute(
        "INSERT INTO task_participant_registry_receipts (
            receipt_id, registry_id, receipt_kind, registry_generation,
            registry_root, created_at_ms
         ) VALUES (?1, ?2, 2, ?3, ?4, ?5)",
        params![
            receipt_id.as_bytes().as_slice(),
            registry.registry_id.as_bytes().as_slice(),
            encode_u64(registry.generation).as_slice(),
            registry.root.as_slice(),
            now_ms,
        ],
    )?;
    Ok(ParticipantRegistryBinding {
        generation: registry.generation,
        root: registry.root,
    })
}

/// Freezes the current registry before a local authority takeover fence.
///
/// The CAS deliberately keeps the existing generation/root: the future
/// assignment/takeover path must create a successor registry only after its
/// barrier evidence is complete. Repeating the exact request after the
/// registry is already frozen is a read-only replay.
pub(crate) fn freeze_for_takeover(
    transaction: &Transaction<'_>,
    task: &TaskRecord,
    expected: ParticipantRegistryBinding,
    now_ms: i64,
) -> Result<ParticipantRegistryRecord, TaskStoreError> {
    let registry = initialize_registry(transaction, task, now_ms)?;
    if registry.generation != expected.generation || registry.root != expected.root {
        return Err(TaskStoreError::ParticipantRegistryCasMismatch);
    }
    match registry.state {
        ParticipantRegistryState::FrozenForTakeover => Ok(registry),
        ParticipantRegistryState::Open | ParticipantRegistryState::FrozenForPermit => {
            let changed = transaction.execute(
                "UPDATE task_participant_registries
                 SET registry_state=?1, updated_at_ms=?2
                 WHERE registry_id=?3 AND registry_generation=?4
                   AND participant_registry_root=?5
                   AND registry_state IN (?6, ?7)",
                params![
                    ParticipantRegistryState::FrozenForTakeover.code(),
                    now_ms,
                    registry.registry_id.as_bytes().as_slice(),
                    encode_u64(registry.generation).as_slice(),
                    registry.root.as_slice(),
                    ParticipantRegistryState::Open.code(),
                    ParticipantRegistryState::FrozenForPermit.code(),
                ],
            )?;
            if changed != 1 {
                return Err(TaskStoreError::ParticipantRegistryCasMismatch);
            }
            Ok(ParticipantRegistryRecord {
                state: ParticipantRegistryState::FrozenForTakeover,
                updated_at_ms: now_ms,
                ..registry
            })
        }
        ParticipantRegistryState::Superseded => Err(TaskStoreError::ParticipantRegistryFrozen {
            state: registry.state,
        }),
    }
}

/// Creates the successor-term registry generation after a takeover receipt
/// has reached `Complete`.
///
/// The participant tuple is copied byte-for-byte from the frozen registry;
/// this is deliberately a local hand-off primitive, not a fresh remote
/// endpoint attestation. The caller must separately prove the completed
/// takeover and rotate the active assignment in the same transaction.
pub(crate) fn reopen_after_takeover(
    transaction: &Transaction<'_>,
    task: &TaskRecord,
    expected: ParticipantRegistryBinding,
    now_ms: i64,
) -> Result<ParticipantRegistryRecord, TaskStoreError> {
    let registry = initialize_registry(transaction, task, now_ms)?;
    if registry.generation != expected.generation || registry.root != expected.root {
        return Err(TaskStoreError::ParticipantRegistryBindingMismatch);
    }
    if registry.state != ParticipantRegistryState::FrozenForTakeover {
        return Err(TaskStoreError::CorruptRecord(
            "successor registry is not frozen for takeover",
        ));
    }
    let next_generation = registry
        .generation
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    let changed = transaction.execute(
        "UPDATE task_participant_registries
         SET registry_state=?1, updated_at_ms=?2
         WHERE registry_id=?3 AND registry_generation=?4
           AND participant_registry_root=?5
           AND registry_state=?6",
        params![
            ParticipantRegistryState::Superseded.code(),
            now_ms,
            registry.registry_id.as_bytes().as_slice(),
            encode_u64(registry.generation).as_slice(),
            registry.root.as_slice(),
            ParticipantRegistryState::FrozenForTakeover.code(),
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::ParticipantRegistryCasMismatch);
    }
    insert_registry_generation(
        transaction,
        task,
        next_generation,
        registry.root,
        &registry.participants,
        now_ms,
    )
}

/// Rejects new mutations against a registry that has entered the takeover
/// fence. Existing exact replay paths intentionally call this only after
/// their durable result has been found.
pub(crate) fn reject_takeover_fence(
    connection: &Connection,
    task_id: TaskId,
) -> Result<(), TaskStoreError> {
    let registry = load_current_registry(connection, task_id)?
        .ok_or(TaskStoreError::ParticipantRegistryNotFound)?;
    if matches!(
        registry.state,
        ParticipantRegistryState::FrozenForTakeover | ParticipantRegistryState::Superseded
    ) {
        return Err(TaskStoreError::ParticipantRegistryFrozen {
            state: registry.state,
        });
    }
    Ok(())
}

pub(crate) fn register_verified_participant(
    transaction: &Transaction<'_>,
    task: &TaskRecord,
    expected: ParticipantRegistryBinding,
    participant: ParticipantRecord,
    now_ms: i64,
) -> Result<ParticipantRegistrationDecision, TaskStoreError> {
    if participant.participant_type == ParticipantType::TaskStore {
        return Err(TaskStoreError::ParticipantEndpointConflict);
    }
    let registry = initialize_registry(transaction, task, now_ms)?;
    if registry.participants.contains(&participant) {
        return Ok(ParticipantRegistrationDecision::Replayed(registry));
    }
    let replacement = registry
        .participants
        .iter()
        .position(|existing| existing.participant_id == participant.participant_id);
    if let Some(index) = replacement {
        let existing = registry.participants[index];
        if existing.participant_type != participant.participant_type
            || existing.participant_generation.get() >= participant.participant_generation.get()
        {
            return Err(TaskStoreError::ParticipantEndpointConflict);
        }
    }
    if registry
        .participants
        .iter()
        .any(|existing| existing.admission_receipt_id == participant.admission_receipt_id)
    {
        return Err(TaskStoreError::ParticipantEndpointConflict);
    }
    if registry.generation != expected.generation || registry.root != expected.root {
        return Err(TaskStoreError::ParticipantRegistryCasMismatch);
    }
    if registry.state != ParticipantRegistryState::Open {
        return Err(TaskStoreError::ParticipantRegistryFrozen {
            state: registry.state,
        });
    }
    if replacement.is_none() && registry.participants.len() >= MAX_PARTICIPANTS {
        return Err(TaskStoreError::ParticipantRegistryFull);
    }
    let changed = transaction.execute(
        "UPDATE task_participant_registries
         SET registry_state=?1, updated_at_ms=?2
         WHERE registry_id=?3 AND registry_state=?4",
        params![
            ParticipantRegistryState::Superseded.code(),
            now_ms,
            registry.registry_id.as_bytes().as_slice(),
            ParticipantRegistryState::Open.code(),
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::ParticipantRegistryCasMismatch);
    }
    let next_generation = registry
        .generation
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    let mut participants = registry.participants.clone();
    if let Some(index) = replacement {
        participants[index] = participant;
    } else {
        participants.push(participant);
    }
    participants.sort_unstable_by_key(|item| {
        (
            item.participant_type.code(),
            item.participant_id.into_bytes(),
        )
    });
    let next = insert_registry_generation(
        transaction,
        task,
        next_generation,
        registry.root,
        &participants,
        now_ms,
    )?;
    Ok(ParticipantRegistrationDecision::Registered(next))
}

pub(crate) fn inspect_registry(
    connection: &Connection,
    task: &TaskRecord,
) -> Result<ParticipantRegistryRecord, TaskStoreError> {
    let registry = load_current_registry(connection, task.task_id)?
        .ok_or(TaskStoreError::ParticipantRegistryNotFound)?;
    validate_registry(task, &registry)?;
    Ok(registry)
}

pub(crate) fn has_participant(
    registry: &ParticipantRegistryRecord,
    participant: ParticipantRecord,
) -> bool {
    registry.participants.contains(&participant)
}

/// Computes the two local takeover roots without inventing a distributed
/// barrier proof. The outstanding set is keyed by stable participant type/id;
/// a generation or admission-receipt conflict is corruption rather than a
/// silently widened fence.
pub(crate) fn takeover_fence_roots(
    registry: &ParticipantRegistryRecord,
    outstanding: &[ParticipantRecord],
) -> Result<([u8; 32], [u8; 32]), TaskStoreError> {
    let outstanding = canonicalize_participants(outstanding)?;
    let union = takeover_fence_members(registry, outstanding.as_slice())?;
    Ok((
        participant_set_root(
            b"llmos/task-takeover-outstanding-participants/v1",
            &outstanding,
        ),
        participant_set_root(b"llmos/task-takeover-exact-fence-set/v1", &union),
    ))
}

pub(crate) fn takeover_fence_members(
    registry: &ParticipantRegistryRecord,
    outstanding: &[ParticipantRecord],
) -> Result<Vec<ParticipantRecord>, TaskStoreError> {
    let mut union = registry.participants.clone();
    union.extend(outstanding.iter().copied());
    canonicalize_participants(&union)
}

pub(crate) fn takeover_fence_set_root(
    participants: &[ParticipantRecord],
) -> Result<[u8; 32], TaskStoreError> {
    let canonical = canonicalize_participants(participants)?;
    Ok(participant_set_root(
        b"llmos/task-takeover-exact-fence-set/v1",
        &canonical,
    ))
}

fn canonicalize_participants(
    participants: &[ParticipantRecord],
) -> Result<Vec<ParticipantRecord>, TaskStoreError> {
    let mut by_identity = BTreeMap::new();
    for participant in participants {
        let key = (
            participant.participant_type.code(),
            participant.participant_id.into_bytes(),
        );
        if let Some(previous) = by_identity.insert(key, *participant)
            && previous != *participant
        {
            return Err(TaskStoreError::CorruptRecord(
                "takeover participant generation conflict",
            ));
        }
    }
    Ok(by_identity.into_values().collect())
}

fn participant_set_root(domain: &[u8], participants: &[ParticipantRecord]) -> [u8; 32] {
    if participants.is_empty() {
        return [0; 32];
    }
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update((participants.len() as u64).to_be_bytes());
    for participant in participants {
        hasher.update(participant.participant_type.code().to_be_bytes());
        hasher.update(participant.participant_id.as_bytes());
        hasher.update(participant.participant_generation.get().to_be_bytes());
        hasher.update(participant.admission_receipt_id.as_bytes());
    }
    hasher.finalize().into()
}

pub(crate) fn validate_frozen_binding(
    connection: &Connection,
    task: &TaskRecord,
    binding: Option<ParticipantRegistryBinding>,
) -> Result<ParticipantRegistryBinding, TaskStoreError> {
    let binding = binding.ok_or(TaskStoreError::ParticipantRegistryBindingMissing)?;
    let registry = load_current_registry(connection, task.task_id)?
        .ok_or(TaskStoreError::ParticipantRegistryNotFound)?;
    validate_registry(task, &registry)?;
    if registry.generation != binding.generation || registry.root != binding.root {
        return Err(TaskStoreError::ParticipantRegistryBindingMismatch);
    }
    if registry.state != ParticipantRegistryState::FrozenForPermit {
        return Err(TaskStoreError::ParticipantRegistryFrozen {
            state: registry.state,
        });
    }
    Ok(binding)
}

pub(crate) fn validate_copied_binding(
    parent: Option<ParticipantRegistryBinding>,
    copied: Option<ParticipantRegistryBinding>,
) -> Result<ParticipantRegistryBinding, TaskStoreError> {
    let parent = parent.ok_or(TaskStoreError::ParticipantRegistryBindingMissing)?;
    if copied != Some(parent) {
        return Err(TaskStoreError::ParticipantRegistryBindingMismatch);
    }
    Ok(parent)
}

fn insert_registry_generation(
    transaction: &Transaction<'_>,
    task: &TaskRecord,
    generation: u64,
    prior_root: [u8; 32],
    participants: &[ParticipantRecord],
    now_ms: i64,
) -> Result<ParticipantRegistryRecord, TaskStoreError> {
    let self_participant = participants
        .first()
        .ok_or(TaskStoreError::CorruptRecord("empty participant registry"))?;
    let registry_id = derived_registry_id(task.task_id, generation);
    let root = registry_root(
        task.task_id,
        task.task_generation,
        generation,
        prior_root,
        participants,
    );
    transaction.execute(
        "INSERT INTO task_participant_registries (
            registry_id, task_id, task_generation, registry_generation,
            prior_registry_root, participant_registry_root, registry_state,
            created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8)",
        params![
            registry_id.as_bytes().as_slice(),
            task.task_id.as_bytes().as_slice(),
            encode_u64(task.task_generation.get()).as_slice(),
            encode_u64(generation).as_slice(),
            prior_root.as_slice(),
            root.as_slice(),
            now_ms,
            now_ms,
        ],
    )?;
    for (sequence, participant) in participants.iter().enumerate() {
        transaction.execute(
            "INSERT INTO task_participants (
                registry_id, participant_seq, participant_type, participant_id,
                participant_generation, admission_receipt_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                registry_id.as_bytes().as_slice(),
                i64::try_from(sequence)
                    .map_err(|_| TaskStoreError::CorruptRecord("participant sequence"))?,
                participant.participant_type.code(),
                participant.participant_id.as_bytes().as_slice(),
                encode_u64(participant.participant_generation.get()).as_slice(),
                participant.admission_receipt_id.as_bytes().as_slice(),
            ],
        )?;
    }
    let receipt_id = derived_receipt_id(
        b"llmos/task-participant-registry-create/v1",
        task.task_id,
        generation,
        self_participant.participant_id,
    );
    transaction.execute(
        "INSERT INTO task_participant_registry_receipts (
            receipt_id, registry_id, receipt_kind, registry_generation,
            registry_root, created_at_ms
         ) VALUES (?1, ?2, 1, ?3, ?4, ?5)",
        params![
            receipt_id.as_bytes().as_slice(),
            registry_id.as_bytes().as_slice(),
            encode_u64(generation).as_slice(),
            root.as_slice(),
            now_ms,
        ],
    )?;
    Ok(ParticipantRegistryRecord {
        registry_id,
        task_id: task.task_id,
        task_generation: task.task_generation,
        generation,
        prior_root,
        participants: participants.to_vec(),
        root,
        state: ParticipantRegistryState::Open,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    })
}

fn validate_registry(
    task: &TaskRecord,
    registry: &ParticipantRegistryRecord,
) -> Result<(), TaskStoreError> {
    if registry.task_id != task.task_id || registry.task_generation != task.task_generation {
        return Err(TaskStoreError::CorruptRecord(
            "participant registry task binding",
        ));
    }
    if registry.participants.is_empty() {
        return Err(TaskStoreError::CorruptRecord("empty participant registry"));
    }
    let expected_root = registry_root(
        registry.task_id,
        registry.task_generation,
        registry.generation,
        registry.prior_root,
        &registry.participants,
    );
    if registry.root != expected_root {
        return Err(TaskStoreError::CorruptRecord(
            "participant registry root mismatch",
        ));
    }
    Ok(())
}

fn load_current_registry(
    connection: &Connection,
    task_id: TaskId,
) -> Result<Option<ParticipantRegistryRecord>, TaskStoreError> {
    let row = connection
        .query_row(
            "SELECT registry_id, task_generation, registry_generation,
                    prior_registry_root, participant_registry_root, registry_state,
                    created_at_ms, updated_at_ms
             FROM task_participant_registries
             WHERE task_id=?1 ORDER BY registry_generation DESC LIMIT 1",
            [task_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = row else { return Ok(None) };
    let registry_id = TaskParticipantRegistryId::from_bytes(
        row.0
            .try_into()
            .map_err(|_| TaskStoreError::CorruptRecord("registry id"))?,
    );
    let participants = load_participants(connection, registry_id)?;
    Ok(Some(ParticipantRegistryRecord {
        registry_id,
        task_id,
        task_generation: generation_from_bytes(row.1)?,
        generation: u64_from_bytes(row.2)?,
        prior_root: row
            .3
            .try_into()
            .map_err(|_| TaskStoreError::CorruptRecord("prior registry root"))?,
        participants,
        root: row
            .4
            .try_into()
            .map_err(|_| TaskStoreError::CorruptRecord("registry root"))?,
        state: ParticipantRegistryState::from_code(row.5)?,
        created_at_ms: row.6,
        updated_at_ms: row.7,
    }))
}

fn load_participants(
    connection: &Connection,
    registry_id: TaskParticipantRegistryId,
) -> Result<Vec<ParticipantRecord>, TaskStoreError> {
    let mut statement = connection.prepare(
        "SELECT participant_type, participant_id, participant_generation, admission_receipt_id
         FROM task_participants WHERE registry_id=?1 ORDER BY participant_seq",
    )?;
    let rows = statement.query_map([registry_id.as_bytes().as_slice()], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, Vec<u8>>(3)?,
        ))
    })?;
    rows.map(|row| {
        let row = row?;
        Ok(ParticipantRecord {
            participant_type: ParticipantType::from_code(row.0)?,
            participant_id: TaskParticipantId::from_bytes(
                row.1
                    .try_into()
                    .map_err(|_| TaskStoreError::CorruptRecord("participant id"))?,
            ),
            participant_generation: generation_from_bytes(row.2)?,
            admission_receipt_id: ReceiptId::from_bytes(
                row.3
                    .try_into()
                    .map_err(|_| TaskStoreError::CorruptRecord("participant receipt"))?,
            ),
        })
    })
    .collect()
}

fn task_store_participant_id(
    transaction: &Transaction<'_>,
) -> Result<TaskParticipantId, TaskStoreError> {
    let bytes: Vec<u8> = transaction.query_row(
        "SELECT participant_id FROM task_authority_identity WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    Ok(TaskParticipantId::from_bytes(bytes.try_into().map_err(
        |_| TaskStoreError::CorruptRecord("task authority participant id"),
    )?))
}

fn registry_root(
    task_id: TaskId,
    task_generation: Generation,
    registry_generation: u64,
    prior_root: [u8; 32],
    participants: &[ParticipantRecord],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-participant-registry/v1");
    hasher.update(task_id.as_bytes());
    hasher.update(task_generation.get().to_be_bytes());
    hasher.update(registry_generation.to_be_bytes());
    hasher.update(prior_root);
    hasher.update((participants.len() as u64).to_be_bytes());
    for participant in participants {
        hasher.update(participant.participant_type.code().to_be_bytes());
        hasher.update(participant.participant_id.as_bytes());
        hasher.update(participant.participant_generation.get().to_be_bytes());
        hasher.update(participant.admission_receipt_id.as_bytes());
    }
    hasher.finalize().into()
}

fn empty_registry_root() -> [u8; 32] {
    Sha256::digest(b"llmos/task-participant-registry/empty/v1").into()
}

fn derived_registry_id(task_id: TaskId, generation: u64) -> TaskParticipantRegistryId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-participant-registry-id/v1");
    hasher.update(task_id.as_bytes());
    hasher.update(generation.to_be_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    TaskParticipantRegistryId::from_bytes(digest[..16].try_into().expect("16-byte prefix"))
}

fn derived_receipt_id(
    domain: &[u8],
    task_id: TaskId,
    generation: u64,
    participant_id: TaskParticipantId,
) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(task_id.as_bytes());
    hasher.update(generation.to_be_bytes());
    hasher.update(participant_id.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    ReceiptId::from_bytes(digest[..16].try_into().expect("16-byte prefix"))
}

fn generation_from_bytes(bytes: Vec<u8>) -> Result<Generation, TaskStoreError> {
    let value = u64_from_bytes(bytes)?;
    std::num::NonZeroU64::new(value)
        .map(Generation::new)
        .ok_or(TaskStoreError::CorruptRecord("zero participant generation"))
}

fn u64_from_bytes(bytes: Vec<u8>) -> Result<u64, TaskStoreError> {
    Ok(u64::from_be_bytes(
        bytes
            .try_into()
            .map_err(|_| TaskStoreError::CorruptRecord("u64 blob"))?,
    ))
}

pub(crate) const SCHEMA_V11_SQL: &str = "CREATE TABLE task_authority_identity (
        singleton INTEGER PRIMARY KEY NOT NULL CHECK(singleton = 1),
        participant_id BLOB NOT NULL UNIQUE CHECK(length(participant_id) = 16),
        participant_generation BLOB NOT NULL CHECK(length(participant_generation) = 8)
    ) STRICT;
    INSERT INTO task_authority_identity VALUES (1, randomblob(16), X'0000000000000001');

    CREATE TABLE task_participant_registries (
        registry_id BLOB PRIMARY KEY NOT NULL CHECK(length(registry_id) = 16),
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
        registry_generation BLOB NOT NULL CHECK(length(registry_generation) = 8),
        prior_registry_root BLOB NOT NULL CHECK(length(prior_registry_root) = 32),
        participant_registry_root BLOB NOT NULL CHECK(length(participant_registry_root) = 32),
        registry_state INTEGER NOT NULL CHECK(registry_state BETWEEN 1 AND 4),
        created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
        updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
        UNIQUE(task_id, registry_generation),
        FOREIGN KEY(task_id) REFERENCES tasks(task_id)
    ) STRICT;

    CREATE TABLE task_participants (
        registry_id BLOB NOT NULL CHECK(length(registry_id) = 16),
        participant_seq INTEGER NOT NULL CHECK(participant_seq >= 0),
        participant_type INTEGER NOT NULL CHECK(participant_type BETWEEN 1 AND 6),
        participant_id BLOB NOT NULL CHECK(length(participant_id) = 16),
        participant_generation BLOB NOT NULL CHECK(length(participant_generation) = 8),
        admission_receipt_id BLOB NOT NULL CHECK(length(admission_receipt_id) = 16),
        PRIMARY KEY(registry_id, participant_seq),
        UNIQUE(registry_id, participant_type, participant_id),
        FOREIGN KEY(registry_id) REFERENCES task_participant_registries(registry_id)
    ) STRICT;

    CREATE TABLE task_participant_registry_receipts (
        receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
        registry_id BLOB NOT NULL CHECK(length(registry_id) = 16),
        receipt_kind INTEGER NOT NULL CHECK(receipt_kind IN (1, 2)),
        registry_generation BLOB NOT NULL CHECK(length(registry_generation) = 8),
        registry_root BLOB NOT NULL CHECK(length(registry_root) = 32),
        created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
        UNIQUE(registry_id, receipt_kind),
        FOREIGN KEY(registry_id) REFERENCES task_participant_registries(registry_id)
    ) STRICT;

    CREATE TRIGGER task_authority_identity_immutable BEFORE UPDATE ON task_authority_identity
    BEGIN SELECT RAISE(ABORT, 'task authority identity is immutable'); END;
    CREATE TRIGGER task_authority_identity_no_delete BEFORE DELETE ON task_authority_identity
    BEGIN SELECT RAISE(ABORT, 'task authority identity is immutable'); END;
    CREATE TRIGGER task_participant_registry_identity_immutable
    BEFORE UPDATE ON task_participant_registries
    WHEN NEW.registry_id != OLD.registry_id OR NEW.task_id != OLD.task_id
      OR NEW.task_generation != OLD.task_generation
      OR NEW.registry_generation != OLD.registry_generation
      OR NEW.prior_registry_root != OLD.prior_registry_root
      OR NEW.participant_registry_root != OLD.participant_registry_root
      OR NEW.created_at_ms != OLD.created_at_ms
    BEGIN SELECT RAISE(ABORT, 'participant registry identity is immutable'); END;
    CREATE TRIGGER task_participant_registry_no_delete BEFORE DELETE ON task_participant_registries
    BEGIN SELECT RAISE(ABORT, 'participant registry is immutable'); END;
    CREATE TRIGGER task_participants_immutable_update BEFORE UPDATE ON task_participants
    BEGIN SELECT RAISE(ABORT, 'task participant is immutable'); END;
    CREATE TRIGGER task_participants_immutable_delete BEFORE DELETE ON task_participants
    BEGIN SELECT RAISE(ABORT, 'task participant is immutable'); END;
    CREATE TRIGGER task_participant_registry_receipts_immutable_update
    BEFORE UPDATE ON task_participant_registry_receipts
    BEGIN SELECT RAISE(ABORT, 'participant registry receipt is immutable'); END;
    CREATE TRIGGER task_participant_registry_receipts_immutable_delete
    BEFORE DELETE ON task_participant_registry_receipts
    BEGIN SELECT RAISE(ABORT, 'participant registry receipt is immutable'); END;

    ALTER TABLE commit_permits ADD COLUMN participant_registry_generation BLOB
        CHECK(participant_registry_generation IS NULL OR length(participant_registry_generation) = 8);
    ALTER TABLE commit_permits ADD COLUMN participant_registry_root BLOB
        CHECK(participant_registry_root IS NULL OR length(participant_registry_root) = 32);
    PRAGMA user_version = 11;";

#[cfg(test)]
mod takeover_root_tests {
    use super::*;
    use std::num::NonZeroU64;

    fn participant(
        participant_type: ParticipantType,
        id: u8,
        generation: u64,
        receipt: u8,
    ) -> ParticipantRecord {
        ParticipantRecord {
            participant_type,
            participant_id: TaskParticipantId::from_bytes([id; 16]),
            participant_generation: Generation::new(
                NonZeroU64::new(generation).expect("non-zero generation"),
            ),
            admission_receipt_id: ReceiptId::from_bytes([receipt; 16]),
        }
    }

    fn registry(participants: Vec<ParticipantRecord>) -> ParticipantRegistryRecord {
        ParticipantRegistryRecord {
            registry_id: TaskParticipantRegistryId::from_bytes([0x10; 16]),
            task_id: TaskId::from_bytes([0x11; 16]),
            task_generation: Generation::INITIAL,
            generation: 1,
            prior_root: [0x12; 32],
            participants,
            root: [0x13; 32],
            state: ParticipantRegistryState::FrozenForTakeover,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn takeover_roots_are_order_independent_and_deduplicate_union() {
        let task_store = participant(ParticipantType::TaskStore, 1, 1, 2);
        let artifact = participant(ParticipantType::ArtifactHead, 2, 1, 3);
        let first = takeover_fence_roots(&registry(vec![task_store]), &[artifact, task_store])
            .expect("roots");
        let second = takeover_fence_roots(&registry(vec![task_store]), &[task_store, artifact])
            .expect("roots");
        assert_eq!(first, second);
        assert_ne!(first.0, [0; 32]);
        assert_ne!(first.1, [0; 32]);
    }

    #[test]
    fn takeover_roots_reject_identity_generation_conflicts() {
        let registry_participant = participant(ParticipantType::TaskStore, 1, 1, 2);
        let conflicting = participant(ParticipantType::TaskStore, 1, 2, 3);
        assert!(matches!(
            takeover_fence_roots(&registry(vec![registry_participant]), &[conflicting]),
            Err(TaskStoreError::CorruptRecord(
                "takeover participant generation conflict"
            ))
        ));
    }

    #[test]
    fn canonical_fence_manifest_root_matches_takeover_root() {
        let task_store = participant(ParticipantType::TaskStore, 1, 1, 2);
        let artifact = participant(ParticipantType::ArtifactHead, 2, 1, 3);
        let members = vec![artifact, task_store];
        let manifest_root = takeover_fence_set_root(&members).expect("manifest root");
        let (_, takeover_root) =
            takeover_fence_roots(&registry(vec![task_store]), &[artifact]).expect("takeover root");
        assert_eq!(manifest_root, takeover_root);
    }
}
