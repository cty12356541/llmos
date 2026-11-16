//! Durable local tool-operation transitions for trusted service embeddings.

use std::collections::HashMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use nlos_capability::service_call::RuntimeCaller;
use nlos_schema::{
    MAX_TOOL_SANDBOX_PAYLOAD_BYTES, decode_invoke_tool_result, encode_invoke_tool_result,
    tool_sandbox_schema_identity,
};
use nlos_types::{
    ApplicationId, Generation, IdempotencyKey, NamespaceId, OperationId, PrincipalId, ProcessId,
    SessionId,
};
use sha2::{Digest, Sha256};

const MIB: u64 = 1024 * 1024;
const MAX_RECORD: usize = 128 * 1024;
const HEADER: usize = 52;
const CHECKSUM: usize = 32;
const BEGIN: u8 = 1;
const ADVANCE: u8 = 2;
// At most two small nonterminal frames, then one maximum encoded result.
const RESERVED_BYTES: u64 = 2 * 192 + 192 + MAX_TOOL_SANDBOX_PAYLOAD_BYTES as u64;
const RESERVED_RECORDS: u64 = 3;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ToolOwner {
    pub principal: PrincipalId,
    pub application: ApplicationId,
    pub namespace: NamespaceId,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ToolOperationKey {
    pub owner: ToolOwner,
    /// Stable ID of the registry entry in this authorization namespace.
    pub registration: [u8; 16],
    pub key: IdempotencyKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolOperationSpec {
    pub origin: RuntimeCaller,
    pub wasm_digest: [u8; 32],
    pub registry_generation: Generation,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ToolOperationHandle {
    pub id: OperationId,
    pub generation: Generation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolReceipt {
    pub revision: u64,
    pub digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ToolFailure {
    Denied = 1,
    Deadline = 2,
    Cancelled = 3,
    HostLost = 4,
    GuestFailed = 5,
    StdoutLimit = 6,
    StderrLimit = 7,
    FileWriteLimit = 8,
    FrameTooLarge = 9,
    InvalidJson = 10,
    ArtifactChanged = 11,
    Interrupted = 12,
}

impl ToolFailure {
    fn decode(value: u8) -> Result<Self, StoreError> {
        match value {
            1 => Ok(Self::Denied),
            2 => Ok(Self::Deadline),
            3 => Ok(Self::Cancelled),
            4 => Ok(Self::HostLost),
            5 => Ok(Self::GuestFailed),
            6 => Ok(Self::StdoutLimit),
            7 => Ok(Self::StderrLimit),
            8 => Ok(Self::FileWriteLimit),
            9 => Ok(Self::FrameTooLarge),
            10 => Ok(Self::InvalidJson),
            11 => Ok(Self::ArtifactChanged),
            12 => Ok(Self::Interrupted),
            _ => Err(StoreError::Corrupt),
        }
    }
}

/// The complete canonical encoded result. Debug output redacts its contents.
#[derive(Clone, Eq, PartialEq)]
pub struct DurableToolResult(Arc<[u8]>);

impl DurableToolResult {
    /// # Errors
    /// Rejects a malformed or oversized invoke result.
    pub fn from_wire(wire: Vec<u8>) -> Result<Self, StoreError> {
        let result = decode_invoke_tool_result(&wire).map_err(|_| StoreError::InvalidTransition)?;
        if result.schema != Some(tool_sandbox_schema_identity())
            || encode_invoke_tool_result(&result).map_err(|_| StoreError::InvalidTransition)?
                != wire
        {
            return Err(StoreError::InvalidTransition);
        }
        Ok(Self(wire.into()))
    }

    #[must_use]
    pub fn wire_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for DurableToolResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DurableToolResult")
            .field("bytes", &self.0.len())
            .finish_non_exhaustive()
    }
}

/// A directory observation can inform reconciliation without resolving uncertainty.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DirectoryEvidence {
    pub before: Option<[u8; 32]>,
    pub after: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolOperationState {
    Registered,
    Dispatched,
    CancelRequested,
    Completed(DurableToolResult),
    Failed(ToolFailure),
    CancelledBeforeEffect,
    EffectUnknown {
        reason: ToolFailure,
        directory: DirectoryEvidence,
    },
}

impl ToolOperationState {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        !matches!(
            self,
            Self::Registered | Self::Dispatched | Self::CancelRequested
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolOperation {
    pub handle: ToolOperationHandle,
    pub key: ToolOperationKey,
    pub request_digest: [u8; 32],
    pub spec: ToolOperationSpec,
    pub state: ToolOperationState,
    pub receipt: ToolReceipt,
    pub directory_before: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BeginDecision {
    Registered(ToolOperation),
    Existing(ToolOperation),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreError {
    Busy,
    Durability,
    Corrupt,
    Quota,
    Conflict,
    NotFound,
    Stale,
    InvalidTransition,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tool operation store: {self:?}")
    }
}

impl std::error::Error for StoreError {}

/// In-memory host admission shared by all services for one store authority.
#[derive(Default)]
pub struct ToolHostQuota {
    limits: OnceLock<(usize, usize)>,
    active: Mutex<ActiveHosts>,
}

#[derive(Default)]
struct ActiveHosts {
    count: usize,
    by_principal: HashMap<PrincipalId, usize>,
}

impl ToolHostQuota {
    pub(crate) fn bind(&self, global: usize, per_principal: usize) -> bool {
        *self.limits.get_or_init(|| (global, per_principal)) == (global, per_principal)
    }

    pub(crate) fn try_acquire(&self, principal: PrincipalId) -> Option<ToolHostPermit<'_>> {
        let &(global, per_principal) = self.limits.get()?;
        let mut active = self.active.lock().ok()?;
        let count = active.by_principal.get(&principal).copied().unwrap_or(0);
        if active.count >= global || count >= per_principal {
            return None;
        }
        active.count += 1;
        active.by_principal.insert(principal, count + 1);
        Some(ToolHostPermit {
            active: &self.active,
            principal,
        })
    }
}

pub(crate) struct ToolHostPermit<'a> {
    active: &'a Mutex<ActiveHosts>,
    principal: PrincipalId,
}

impl Drop for ToolHostPermit<'_> {
    fn drop(&mut self) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = active.by_principal.get_mut(&self.principal) {
            *count -= 1;
            if *count == 0 {
                active.by_principal.remove(&self.principal);
            }
        }
        active.count -= 1;
    }
}

/// Trusted storage adapter. Mutations must linearize by revision and return
/// only after durable commit. The service, rather than the store, authenticates
/// the current caller before exposing records or requesting cancellation.
pub trait ToolOperationStore: Send + Sync {
    /// Returns the quota owned by this backend authority. Adapters forwarding
    /// operations to one authority must forward the same quota allocation.
    /// The authority retains it across all associated service lifetimes.
    fn host_quota(&self) -> Arc<ToolHostQuota>;

    /// # Errors
    /// Returns a storage error or a conflict for the same raw key with a different binding.
    fn find(
        &self,
        key: ToolOperationKey,
        digest: [u8; 32],
        spec: ToolOperationSpec,
    ) -> Result<Option<ToolOperation>, StoreError>;

    /// # Errors
    /// Returns a durability, admission or idempotency error.
    fn begin(
        &self,
        key: ToolOperationKey,
        digest: [u8; 32],
        spec: ToolOperationSpec,
    ) -> Result<BeginDecision, StoreError>;

    /// # Errors
    /// Returns a storage or stale revision error.
    fn mark_dispatched(
        &self,
        operation: &ToolOperation,
        before: Option<[u8; 32]>,
    ) -> Result<ToolOperation, StoreError>;

    /// # Errors
    /// Returns a storage or stale revision error.
    fn request_cancel(&self, operation: &ToolOperation) -> Result<ToolOperation, StoreError>;

    /// # Errors
    /// Returns a storage error, stale revision, or invalid transition.
    fn complete(
        &self,
        operation: &ToolOperation,
        terminal: ToolOperationState,
    ) -> Result<ToolOperation, StoreError>;

    /// # Errors
    /// Returns a storage error, missing handle, or stale generation.
    fn query(&self, handle: ToolOperationHandle) -> Result<ToolOperation, StoreError>;
}

#[derive(Clone, Copy, Debug)]
pub struct ToolJournalLimits {
    pub admission_bytes: u64,
    pub admission_records: u64,
    pub hard_bytes: u64,
    pub hard_records: u64,
}

impl Default for ToolJournalLimits {
    fn default() -> Self {
        Self {
            admission_bytes: 192 * MIB,
            admission_records: 196_608,
            hard_bytes: 256 * MIB,
            hard_records: 262_144,
        }
    }
}

impl ToolJournalLimits {
    fn valid(self) -> bool {
        let max = Self::default();
        self.admission_bytes > 0
            && self.admission_bytes < self.hard_bytes
            && self.admission_records > 0
            && self.admission_records < self.hard_records
            && self.hard_bytes <= max.hard_bytes
            && self.hard_records <= max.hard_records
    }
}

pub struct JournalToolOperationStore {
    inner: Mutex<Journal>,
    host_quota: Arc<ToolHostQuota>,
}

struct LockedJournalFile {
    file: File,
    #[cfg(unix)]
    owner_pid: u32,
}

impl Drop for LockedJournalFile {
    fn drop(&mut self) {
        // A forked copy may close its own descriptor without releasing
        // the lock owned by the original process.
        #[cfg(unix)]
        if self.owner_pid != std::process::id() {
            return;
        }
        // Passive copies can outlive the writer, so release before close.
        // A successor must acquire its own lock even if unlock fails.
        let _ = self.file.unlock();
    }
}

struct Journal {
    locked_file: LockedJournalFile,
    limits: ToolJournalLimits,
    keys: HashMap<ToolOperationKey, OperationId>,
    ops: HashMap<OperationId, ToolOperation>,
    revision: u64,
    hash: [u8; 32],
    bytes: u64,
    pending: u64,
    poisoned: bool,
    #[cfg(test)]
    append_failure_handle: Option<File>,
    #[cfg(test)]
    sync_failure_handle: Option<File>,
}

impl JournalToolOperationStore {
    /// Opens the journal in a directory controlled by the trusted embedding.
    /// This writer holds an exclusive file lock until drop. Reopening repairs
    /// only an incomplete final frame and reconciles any unfinished operations.
    ///
    /// # Errors
    /// Returns an error for a busy writer, invalid limits, malformed history,
    /// or an I/O failure. A failed open never authorizes execution.
    pub fn open(path: impl AsRef<Path>, limits: ToolJournalLimits) -> Result<Self, StoreError> {
        Self::restore_writer(Self::open_file(path.as_ref(), limits)?)
    }

    fn open_file(path: &Path, limits: ToolJournalLimits) -> Result<Journal, StoreError> {
        if !limits.valid() {
            return Err(StoreError::Quota);
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(path).map_err(|_| StoreError::Durability)?;
        let locked_file = match file.try_lock() {
            Ok(()) => LockedJournalFile {
                file,
                #[cfg(unix)]
                owner_pid: std::process::id(),
            },
            Err(std::fs::TryLockError::WouldBlock) => return Err(StoreError::Busy),
            Err(std::fs::TryLockError::Error(_)) => return Err(StoreError::Durability),
        };
        #[cfg(unix)]
        File::open(
            path.parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )
        .and_then(|directory| directory.sync_all())
        .map_err(|_| StoreError::Durability)?;
        Ok(Journal {
            locked_file,
            limits,
            keys: HashMap::new(),
            ops: HashMap::new(),
            revision: 0,
            hash: [0; 32],
            bytes: 0,
            pending: 0,
            poisoned: false,
            #[cfg(test)]
            append_failure_handle: None,
            #[cfg(test)]
            sync_failure_handle: None,
        })
    }

    fn restore_writer(mut journal: Journal) -> Result<Self, StoreError> {
        journal.restore()?;
        Ok(Self {
            inner: Mutex::new(journal),
            host_quota: Arc::default(),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Journal>, StoreError> {
        let journal = self.inner.lock().map_err(|_| StoreError::Durability)?;
        if journal.poisoned {
            return Err(StoreError::Durability);
        }
        Ok(journal)
    }
}

impl ToolOperationStore for JournalToolOperationStore {
    fn host_quota(&self) -> Arc<ToolHostQuota> {
        Arc::clone(&self.host_quota)
    }

    fn find(
        &self,
        key: ToolOperationKey,
        digest: [u8; 32],
        spec: ToolOperationSpec,
    ) -> Result<Option<ToolOperation>, StoreError> {
        self.lock()?.find(key, digest, spec)
    }

    fn begin(
        &self,
        key: ToolOperationKey,
        digest: [u8; 32],
        spec: ToolOperationSpec,
    ) -> Result<BeginDecision, StoreError> {
        let mut journal = self.lock()?;
        if let Some(existing) = journal.find(key, digest, spec)? {
            return Ok(BeginDecision::Existing(existing));
        }
        if spec.origin.principal != key.owner.principal
            || spec.origin.application != key.owner.application
        {
            return Err(StoreError::InvalidTransition);
        }
        let record = encode_begin(key, digest, spec);
        let bytes_after = journal.bytes + (record.len() + HEADER + CHECKSUM) as u64;
        let projected_pending = journal.pending + 1;
        let next_record = journal.revision + 1;
        if bytes_after > journal.limits.admission_bytes
            || next_record > journal.limits.admission_records
            || bytes_after + projected_pending * RESERVED_BYTES > journal.limits.hard_bytes
            || next_record + projected_pending * RESERVED_RECORDS > journal.limits.hard_records
        {
            return Err(StoreError::Quota);
        }
        let handle = operation_handle(key);
        if journal.ops.contains_key(&handle.id) {
            return Err(StoreError::Conflict);
        }
        let receipt = journal.append(&record)?;
        let operation = ToolOperation {
            handle,
            key,
            request_digest: digest,
            spec,
            state: ToolOperationState::Registered,
            receipt,
            directory_before: None,
        };
        journal.insert(operation.clone());
        Ok(BeginDecision::Registered(operation))
    }

    fn mark_dispatched(
        &self,
        operation: &ToolOperation,
        before: Option<[u8; 32]>,
    ) -> Result<ToolOperation, StoreError> {
        self.lock()?
            .advance(operation, ToolOperationState::Dispatched, before)
    }

    fn request_cancel(&self, operation: &ToolOperation) -> Result<ToolOperation, StoreError> {
        let mut journal = self.lock()?;
        let current = journal.get(operation.handle)?.clone();
        let next = match current.state {
            ToolOperationState::Registered => ToolOperationState::CancelledBeforeEffect,
            ToolOperationState::Dispatched => ToolOperationState::CancelRequested,
            _ => return Ok(current),
        };
        journal.advance(operation, next, current.directory_before)
    }

    fn complete(
        &self,
        operation: &ToolOperation,
        terminal: ToolOperationState,
    ) -> Result<ToolOperation, StoreError> {
        if !terminal.is_terminal() {
            return Err(StoreError::InvalidTransition);
        }
        let mut journal = self.lock()?;
        let current = journal.get(operation.handle)?;
        if current.state.is_terminal() {
            return Ok(current.clone());
        }
        journal.advance(operation, terminal, operation.directory_before)
    }

    fn query(&self, handle: ToolOperationHandle) -> Result<ToolOperation, StoreError> {
        Ok(self.lock()?.get(handle)?.clone())
    }
}

impl Journal {
    fn get(&self, handle: ToolOperationHandle) -> Result<&ToolOperation, StoreError> {
        let op = self.ops.get(&handle.id).ok_or(StoreError::NotFound)?;
        if op.handle != handle {
            return Err(StoreError::Stale);
        }
        Ok(op)
    }

    fn find(
        &self,
        key: ToolOperationKey,
        digest: [u8; 32],
        spec: ToolOperationSpec,
    ) -> Result<Option<ToolOperation>, StoreError> {
        let Some(id) = self.keys.get(&key) else {
            return Ok(None);
        };
        let op = self.ops.get(id).ok_or(StoreError::Corrupt)?;
        if op.request_digest != digest
            || op.spec.wasm_digest != spec.wasm_digest
            || op.spec.registry_generation != spec.registry_generation
        {
            return Err(StoreError::Conflict);
        }
        Ok(Some(op.clone()))
    }

    fn insert(&mut self, op: ToolOperation) {
        self.pending += 1;
        self.keys.insert(op.key, op.handle.id);
        self.ops.insert(op.handle.id, op);
    }

    fn advance(
        &mut self,
        previous: &ToolOperation,
        next: ToolOperationState,
        before: Option<[u8; 32]>,
    ) -> Result<ToolOperation, StoreError> {
        let current = self.get(previous.handle)?.clone();
        if current.receipt != previous.receipt {
            return Err(StoreError::Stale);
        }
        check_transition(&current, &next, before)?;
        let body = encode_advance(&current, &next, before);
        let receipt = self.append(&body)?;
        let updated = ToolOperation {
            state: next,
            receipt,
            directory_before: before,
            ..current
        };
        self.replace(updated.clone());
        Ok(updated)
    }

    fn replace(&mut self, op: ToolOperation) {
        self.pending -= u64::from(op.state.is_terminal());
        self.ops.insert(op.handle.id, op);
    }

    fn append(&mut self, body: &[u8]) -> Result<ToolReceipt, StoreError> {
        let len = u32::try_from(body.len()).map_err(|_| StoreError::Quota)?;
        let next_bytes = self.bytes + (body.len() + HEADER + CHECKSUM) as u64;
        let revision = self.revision + 1;
        if body.len() + HEADER + CHECKSUM > MAX_RECORD
            || next_bytes > self.limits.hard_bytes
            || revision > self.limits.hard_records
        {
            return Err(StoreError::Quota);
        }
        let mut frame = Vec::with_capacity(body.len() + HEADER + CHECKSUM);
        frame.extend_from_slice(b"NTO1");
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&(!len).to_le_bytes());
        frame.extend_from_slice(&revision.to_le_bytes());
        frame.extend_from_slice(&self.hash);
        frame.extend_from_slice(body);
        let digest = frame_digest(&frame);
        frame.extend_from_slice(&digest);
        let sink = &mut self.locked_file.file;
        #[cfg(test)]
        let sink = self.append_failure_handle.as_mut().unwrap_or(sink);
        if sink
            .write_all(&frame)
            .and_then(|()| self.sync_file())
            .is_err()
        {
            self.poisoned = true;
            return Err(StoreError::Durability);
        }
        self.bytes = next_bytes;
        self.revision = revision;
        self.hash = digest;
        Ok(ToolReceipt { revision, digest })
    }

    fn sync_file(&self) -> std::io::Result<()> {
        #[cfg(test)]
        if let Some(handle) = &self.sync_failure_handle {
            return handle.sync_all();
        }
        self.locked_file.file.sync_all()
    }

    fn restore(&mut self) -> Result<(), StoreError> {
        let length = self
            .locked_file
            .file
            .metadata()
            .map_err(|_| StoreError::Durability)?
            .len();
        if length > self.limits.hard_bytes {
            return Err(StoreError::Corrupt);
        }
        while self.bytes < length {
            if !self.restore_frame(length)? {
                self.locked_file
                    .file
                    .set_len(self.bytes)
                    .map_err(|_| StoreError::Durability)?;
                break;
            }
        }
        self.locked_file
            .file
            .seek(SeekFrom::Start(self.bytes))
            .map_err(|_| StoreError::Durability)?;
        // A complete frame can survive a failed sync. Make the recovered
        // prefix durable before returning a writer that can replay receipts.
        self.sync_file().map_err(|_| StoreError::Durability)?;
        let mut unfinished: Vec<_> = self
            .ops
            .values()
            .filter(|op| !op.state.is_terminal())
            .cloned()
            .collect();
        unfinished.sort_unstable_by_key(|op| op.receipt.revision);
        for op in unfinished {
            let next = if op.state == ToolOperationState::Registered {
                ToolOperationState::Failed(ToolFailure::Interrupted)
            } else {
                ToolOperationState::EffectUnknown {
                    reason: if op.state == ToolOperationState::CancelRequested {
                        ToolFailure::Cancelled
                    } else {
                        ToolFailure::HostLost
                    },
                    directory: DirectoryEvidence {
                        before: op.directory_before,
                        after: None,
                    },
                }
            };
            self.advance(&op, next, op.directory_before)?;
        }
        Ok(())
    }

    fn restore_frame(&mut self, length: u64) -> Result<bool, StoreError> {
        let remaining = length - self.bytes;
        let mut header = [0; HEADER];
        let read_len =
            usize::try_from(remaining.min(HEADER as u64)).map_err(|_| StoreError::Corrupt)?;
        self.locked_file
            .file
            .read_exact(&mut header[..read_len])
            .map_err(|_| StoreError::Durability)?;
        if header[..read_len.min(4)] != b"NTO1"[..read_len.min(4)] {
            return Err(StoreError::Corrupt);
        }
        if read_len < HEADER {
            return Ok(false);
        }
        let mut input = Input(&header[4..]);
        let len = input.u32()?;
        if len != !input.u32()? {
            return Err(StoreError::Corrupt);
        }
        let revision = input.u64()?;
        if revision != self.revision + 1
            || revision > self.limits.hard_records
            || input.bytes::<32>()? != self.hash
        {
            return Err(StoreError::Corrupt);
        }
        let frame_len = len as usize + HEADER + CHECKSUM;
        if len == 0 || frame_len > MAX_RECORD {
            return Err(StoreError::Corrupt);
        }
        if remaining < frame_len as u64 {
            return Ok(false);
        }
        let mut frame = vec![0; frame_len];
        frame[..HEADER].copy_from_slice(&header);
        self.locked_file
            .file
            .read_exact(&mut frame[HEADER..])
            .map_err(|_| StoreError::Durability)?;
        let checksum_at = frame_len - CHECKSUM;
        let digest = frame_digest(&frame[..checksum_at]);
        if digest != frame[checksum_at..] {
            return Err(StoreError::Corrupt);
        }
        self.replay(
            &frame[HEADER..checksum_at],
            ToolReceipt { revision, digest },
        )?;
        self.hash = digest;
        self.revision = revision;
        self.bytes += frame_len as u64;
        Ok(true)
    }

    fn replay(&mut self, body: &[u8], receipt: ToolReceipt) -> Result<(), StoreError> {
        let mut input = Input(body);
        match input.byte()? {
            BEGIN => {
                let (key, request_digest, spec) = decode_begin(&mut input)?;
                let handle = operation_handle(key);
                if !input.0.is_empty()
                    || self.keys.contains_key(&key)
                    || self.ops.contains_key(&handle.id)
                {
                    return Err(StoreError::Corrupt);
                }
                self.insert(ToolOperation {
                    handle,
                    key,
                    request_digest,
                    spec,
                    state: ToolOperationState::Registered,
                    receipt,
                    directory_before: None,
                });
            }
            ADVANCE => {
                let id = OperationId::from_bytes(input.bytes()?);
                let current = self.ops.get(&id).ok_or(StoreError::Corrupt)?.clone();
                if input.u64()? != current.receipt.revision {
                    return Err(StoreError::Corrupt);
                }
                let before = input.optional_digest()?;
                let state = decode_state(&mut input, before)?;
                if !input.0.is_empty() {
                    return Err(StoreError::Corrupt);
                }
                check_transition(&current, &state, before).map_err(|_| StoreError::Corrupt)?;
                self.replace(ToolOperation {
                    state,
                    receipt,
                    directory_before: before,
                    ..current
                });
            }
            _ => return Err(StoreError::Corrupt),
        }
        Ok(())
    }
}

fn frame_digest(frame: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"nlos/tool-operation-journal/v1\0");
    hash.update(frame);
    hash.finalize().into()
}

fn operation_handle(key: ToolOperationKey) -> ToolOperationHandle {
    let mut raw = Vec::with_capacity(80);
    encode_key(key, &mut raw);
    let mut hash = Sha256::new();
    hash.update(b"nlos/tool-operation-id/v1\0");
    hash.update(raw);
    let id = hash.finalize()[..16]
        .try_into()
        .expect("16-byte operation ID");
    ToolOperationHandle {
        id: OperationId::from_bytes(id),
        generation: Generation::INITIAL,
    }
}

fn check_transition(
    current: &ToolOperation,
    next: &ToolOperationState,
    before: Option<[u8; 32]>,
) -> Result<(), StoreError> {
    use ToolOperationState::{
        CancelRequested, CancelledBeforeEffect, Completed, Dispatched, EffectUnknown, Failed,
        Registered,
    };
    let valid = match (&current.state, next) {
        (Registered, Dispatched) => true,
        (Registered, Failed(_) | CancelledBeforeEffect)
        | (Dispatched, CancelRequested | Completed(_) | Failed(_)) => {
            before == current.directory_before
        }
        (Dispatched | CancelRequested, EffectUnknown { directory, .. }) => {
            before == current.directory_before && directory.before == before
        }
        _ => false,
    };
    valid.then_some(()).ok_or(StoreError::InvalidTransition)
}

fn encode_key(key: ToolOperationKey, bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(key.owner.principal.as_bytes());
    bytes.extend_from_slice(key.owner.application.as_bytes());
    bytes.extend_from_slice(key.owner.namespace.as_bytes());
    bytes.extend_from_slice(&key.registration);
    bytes.extend_from_slice(key.key.as_bytes());
}

fn encode_begin(key: ToolOperationKey, digest: [u8; 32], spec: ToolOperationSpec) -> Vec<u8> {
    let mut bytes = vec![BEGIN];
    encode_key(key, &mut bytes);
    bytes.extend_from_slice(&digest);
    bytes.extend_from_slice(spec.origin.process.as_bytes());
    bytes.extend_from_slice(&spec.origin.process_generation.get().to_le_bytes());
    bytes.extend_from_slice(spec.origin.session.as_bytes());
    bytes.extend_from_slice(&spec.wasm_digest);
    bytes.extend_from_slice(&spec.registry_generation.get().to_le_bytes());
    bytes
}

fn decode_begin(
    input: &mut Input<'_>,
) -> Result<(ToolOperationKey, [u8; 32], ToolOperationSpec), StoreError> {
    let principal = PrincipalId::from_bytes(input.bytes()?);
    let application = ApplicationId::from_bytes(input.bytes()?);
    let namespace = NamespaceId::from_bytes(input.bytes()?);
    let owner = ToolOwner {
        principal,
        application,
        namespace,
    };
    let key = ToolOperationKey {
        owner,
        registration: input.bytes()?,
        key: IdempotencyKey::from_bytes(input.bytes()?),
    };
    let digest = input.bytes()?;
    let origin = RuntimeCaller {
        principal,
        application,
        process: ProcessId::from_bytes(input.bytes()?),
        process_generation: input.generation()?,
        session: SessionId::from_bytes(input.bytes()?),
    };
    let spec = ToolOperationSpec {
        origin,
        wasm_digest: input.bytes()?,
        registry_generation: input.generation()?,
    };
    Ok((key, digest, spec))
}

fn encode_advance(
    op: &ToolOperation,
    next: &ToolOperationState,
    before: Option<[u8; 32]>,
) -> Vec<u8> {
    let mut body = vec![ADVANCE];
    body.extend_from_slice(op.handle.id.as_bytes());
    body.extend_from_slice(&op.receipt.revision.to_le_bytes());
    encode_optional_digest(before, &mut body);
    match next {
        ToolOperationState::Registered => unreachable!("cannot register twice"),
        ToolOperationState::Dispatched => body.push(1),
        ToolOperationState::CancelRequested => body.push(2),
        ToolOperationState::Completed(result) => {
            body.push(3);
            body.extend_from_slice(result.wire_bytes());
        }
        ToolOperationState::Failed(failure) => body.extend_from_slice(&[4, *failure as u8]),
        ToolOperationState::CancelledBeforeEffect => body.push(5),
        ToolOperationState::EffectUnknown { reason, directory } => {
            body.extend_from_slice(&[6, *reason as u8]);
            encode_optional_digest(directory.after, &mut body);
        }
    }
    body
}

fn encode_optional_digest(digest: Option<[u8; 32]>, out: &mut Vec<u8>) {
    out.push(u8::from(digest.is_some()));
    if let Some(digest) = digest {
        out.extend_from_slice(&digest);
    }
}

fn decode_state(
    input: &mut Input<'_>,
    before: Option<[u8; 32]>,
) -> Result<ToolOperationState, StoreError> {
    Ok(match input.byte()? {
        1 => ToolOperationState::Dispatched,
        2 => ToolOperationState::CancelRequested,
        3 => {
            let bytes = std::mem::take(&mut input.0).to_vec();
            ToolOperationState::Completed(
                DurableToolResult::from_wire(bytes).map_err(|_| StoreError::Corrupt)?,
            )
        }
        4 => ToolOperationState::Failed(ToolFailure::decode(input.byte()?)?),
        5 => ToolOperationState::CancelledBeforeEffect,
        6 => ToolOperationState::EffectUnknown {
            reason: ToolFailure::decode(input.byte()?)?,
            directory: DirectoryEvidence {
                before,
                after: input.optional_digest()?,
            },
        },
        _ => return Err(StoreError::Corrupt),
    })
}

struct Input<'a>(&'a [u8]);

impl Input<'_> {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], StoreError> {
        let (front, rest) = self.0.split_at_checked(N).ok_or(StoreError::Corrupt)?;
        self.0 = rest;
        front.try_into().map_err(|_| StoreError::Corrupt)
    }

    fn byte(&mut self) -> Result<u8, StoreError> {
        self.bytes::<1>().map(|value| value[0])
    }

    fn u32(&mut self) -> Result<u32, StoreError> {
        self.bytes().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, StoreError> {
        self.bytes().map(u64::from_le_bytes)
    }

    fn generation(&mut self) -> Result<Generation, StoreError> {
        self.u64().and_then(|n| {
            NonZeroU64::new(n)
                .map(Generation::new)
                .ok_or(StoreError::Corrupt)
        })
    }

    fn optional_digest(&mut self) -> Result<Option<[u8; 32]>, StoreError> {
        match self.byte()? {
            0 => Ok(None),
            1 => self.bytes().map(Some),
            _ => Err(StoreError::Corrupt),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> (ToolOperationKey, ToolOperationSpec) {
        let principal = PrincipalId::from_bytes([1; 16]);
        let application = ApplicationId::from_bytes([2; 16]);
        let key = ToolOperationKey {
            owner: ToolOwner {
                principal,
                application,
                namespace: NamespaceId::from_bytes([3; 16]),
            },
            registration: [4; 16],
            key: IdempotencyKey::from_bytes([5; 16]),
        };
        let spec = ToolOperationSpec {
            origin: RuntimeCaller {
                principal,
                application,
                process: ProcessId::from_bytes([6; 16]),
                process_generation: Generation::INITIAL,
                session: SessionId::from_bytes([7; 16]),
            },
            wasm_digest: [8; 32],
            registry_generation: Generation::INITIAL,
        };
        (key, spec)
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn last_journal_owner_releases_writer_lock_with_passive_clone_alive() {
        let path =
            std::env::temp_dir().join(format!("nlos-journal-owner-release-{}", std::process::id()));
        let open = || JournalToolOperationStore::open(&path, ToolJournalLimits::default());
        let first = Arc::new(open().unwrap());
        let passive = first
            .inner
            .lock()
            .unwrap()
            .locked_file
            .file
            .try_clone()
            .unwrap();
        let last = Arc::clone(&first);

        drop(first);
        assert!(matches!(open(), Err(StoreError::Busy)));
        drop(last);
        let successor = open().unwrap_or_else(|error| {
            panic!("successor could not lock after final writer drop with a passive clone alive: {error:?}")
        });
        drop(passive);
        assert!(matches!(open(), Err(StoreError::Busy)));
        drop(successor);

        let final_writer = open().unwrap();
        drop(final_writer);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn os_append_failure_prevents_receipt_and_poisons_this_writer() {
        let path =
            std::env::temp_dir().join(format!("nlos-journal-io-failure-{}", std::process::id()));
        std::fs::write(&path, []).unwrap();
        let store = JournalToolOperationStore::open(&path, ToolJournalLimits::default()).unwrap();
        let read_only = File::open(&path).unwrap();
        store.inner.lock().unwrap().append_failure_handle = Some(read_only);

        let (key, spec) = identity();
        assert_eq!(store.begin(key, [9; 32], spec), Err(StoreError::Durability));
        assert_eq!(store.find(key, [9; 32], spec), Err(StoreError::Durability));
        drop(store);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(any(windows, unix))]
    struct SyncFixture {
        directory: std::path::PathBuf,
        dispatch: ToolOperation,
        dispatch_bytes: u64,
        result: ToolOperationState,
    }

    #[cfg(any(windows, unix))]
    impl SyncFixture {
        fn new(name: &str) -> Self {
            use nlos_schema::sabi::v1::InvokeToolResult;

            let directory = std::env::temp_dir()
                .join(format!("nlos-journal-sync-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join("journal");
            let store =
                JournalToolOperationStore::open(&path, ToolJournalLimits::default()).unwrap();
            let (key, spec) = identity();
            let BeginDecision::Registered(registered) = store.begin(key, [9; 32], spec).unwrap()
            else {
                panic!("new operation should be registered");
            };
            let dispatch = store.mark_dispatched(&registered, Some([10; 32])).unwrap();
            let dispatch_bytes = std::fs::metadata(&path).unwrap().len();
            let wire = encode_invoke_tool_result(&InvokeToolResult {
                schema: Some(tool_sandbox_schema_identity()),
                output_json: r#"{"answer":42}"#.into(),
            })
            .unwrap();
            let result = ToolOperationState::Completed(DurableToolResult::from_wire(wire).unwrap());

            // The operation writes to the real journal File. Only the sync
            // target changes so the OS reports a real barrier error.
            store.inner.lock().unwrap().sync_failure_handle = Some(real_sync_failure_handle(&path));
            assert_eq!(
                store.complete(&dispatch, result.clone()),
                Err(StoreError::Durability)
            );
            assert!(
                std::fs::metadata(&path).unwrap().len() > dispatch_bytes,
                "the frame write must have reached the file"
            );
            assert_eq!(store.query(dispatch.handle), Err(StoreError::Durability));
            assert_eq!(store.begin(key, [9; 32], spec), Err(StoreError::Durability));
            drop(store);
            Self {
                directory,
                dispatch,
                dispatch_bytes,
                result,
            }
        }

        fn path(&self) -> std::path::PathBuf {
            self.directory.join("journal")
        }
    }

    #[cfg(any(windows, unix))]
    impl Drop for SyncFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[cfg(windows)]
    fn real_sync_failure_handle(path: &Path) -> File {
        // Windows FlushFileBuffers requires a write-capable handle.
        let file = File::open(path).unwrap();
        let error = file
            .sync_all()
            .expect_err("read-only handle must reject sync");
        assert_eq!(error.raw_os_error(), Some(5));
        file
    }

    #[cfg(unix)]
    fn real_sync_failure_handle(_path: &Path) -> File {
        use std::os::fd::OwnedFd;

        let (read_end, _write_end) = std::io::pipe().unwrap();
        let file = File::from(OwnedFd::from(read_end));
        file.sync_all().expect_err("pipe must reject fsync");
        file
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn reopen_requires_sync_before_publishing_retained_completion() {
        let fixture = SyncFixture::new("retained");
        let path = fixture.path();
        let mut writer =
            JournalToolOperationStore::open_file(&path, ToolJournalLimits::default()).unwrap();
        let passive = writer.locked_file.file.try_clone().unwrap();
        writer.sync_failure_handle = Some(real_sync_failure_handle(&path));
        assert!(
            matches!(
                JournalToolOperationStore::restore_writer(writer),
                Err(StoreError::Durability)
            ),
            "recovery published a writer although the retained completion could not be synced",
        );

        let reopened = JournalToolOperationStore::open(path, ToolJournalLimits::default())
            .expect("failed recovery must release the acquired writer lock");
        drop(passive);
        let completed = reopened.query(fixture.dispatch.handle).unwrap();
        assert_eq!(completed.state, fixture.result);
        let op = &fixture.dispatch;
        assert_eq!(
            reopened.begin(op.key, op.request_digest, op.spec),
            Ok(BeginDecision::Existing(completed.clone()))
        );
        assert_eq!(
            reopened.begin(op.key, [99; 32], op.spec),
            Err(StoreError::Conflict)
        );
        assert_eq!(
            reopened.mark_dispatched(&completed, None),
            Err(StoreError::InvalidTransition)
        );
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn lost_completion_after_sync_failure_keeps_dispatch_indeterminate() {
        let fixture = SyncFixture::new("lost");
        let path = fixture.path();
        // Model the failure retaining only the earlier, successfully
        // synchronized dispatch prefix. No power failure is induced.
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(fixture.dispatch_bytes)
            .unwrap();
        let reopened = JournalToolOperationStore::open(path, ToolJournalLimits::default()).unwrap();
        let op = &fixture.dispatch;
        let recovered = reopened.query(op.handle).unwrap();
        assert_eq!(
            recovered.state,
            ToolOperationState::EffectUnknown {
                reason: ToolFailure::HostLost,
                directory: DirectoryEvidence {
                    before: Some([10; 32]),
                    after: None
                },
            }
        );
        assert_eq!(
            reopened.begin(op.key, op.request_digest, op.spec),
            Ok(BeginDecision::Existing(recovered.clone()))
        );
        assert_eq!(
            reopened.mark_dispatched(&recovered, None),
            Err(StoreError::InvalidTransition)
        );
    }
}
