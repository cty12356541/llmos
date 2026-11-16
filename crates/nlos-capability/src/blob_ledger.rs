use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, MutexGuard};
use std::time::Duration;

use nlos_identity::{Ed25519Signature, VerifiedCapabilityCommandSigner};
use nlos_schema::sabi::v1::MintReferenceRequest;
use nlos_types::{
    ApplicationId, CapabilityId, Generation, IdempotencyKey, NamespaceId, PrincipalId, ReceiptId,
    WorkspaceId,
};
use rusqlite::{
    Connection, ErrorCode, OptionalExtension, Row, Transaction, TransactionBehavior, params,
};

use crate::blob_model::{BlobConfig, BlobError, RATE_WINDOW_MS, WorkspaceBinding};
use crate::blob_reference::Timing;
use crate::service_call::RuntimeCaller;
use crate::{
    CapabilityAuthority, CapabilityHandle, CapabilityRecord, CapabilityTarget, derive_id,
    encode_u64, load_exact_current, validate_active_chain,
};

pub(super) type ReferenceId = [u8; 16];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Token {
    pub reference: ReferenceId,
    pub receipt: ReceiptId,
    pub epoch: u64,
    pub issuer: PrincipalId,
    pub holder: PrincipalId,
    pub workspace: WorkspaceBinding,
    pub source: CapabilityHandle,
    pub size: u64,
    pub block_bytes: u64,
    pub max_reads: u64,
    pub expiry: u64,
    pub minted_at: u64,
}

#[derive(Clone, Copy)]
pub(super) struct OperationKey {
    pub actor: PrincipalId,
    pub application: ApplicationId,
    pub key: IdempotencyKey,
    pub digest: [u8; 32],
}

pub(super) struct SignedRecord<'a> {
    pub signer: VerifiedCapabilityCommandSigner,
    pub signature: &'a Ed25519Signature,
    pub signed_at: u64,
    pub caller: RuntimeCaller,
}

#[derive(Clone, Copy)]
pub(super) struct ReadBlock {
    pub offset: u64,
    pub size: u64,
    pub digest: [u8; 32],
}

pub(super) struct BlobLedger {
    pub authority: Arc<CapabilityAuthority>,
    pub epoch: u64,
    db_path: PathBuf,
    wal_path: PathBuf,
    page_size: u64,
    _lease: File,
}

const RECEIPT_ESTIMATE: u64 = 1024;
const MINT_OVERHEAD: u64 = 4096;
const WAL_HEADER: u64 = 32;
const FRAME_HEADER: u64 = 24;
const WAL_WINDOW_FRAMES: u64 = 64;
// Bounds from the pinned SQLite B-tree implementation (BTCURSOR_MAX_DEPTH,
// balance_nonroot NB+2), and the tables and indices in migrate_v3.
const BTREE_LEVELS: u64 = 20;
const MAX_OUTPUT_SIBLINGS: u64 = 5;
const MAX_ALLOCATION_PER_LEVEL: u64 = 2;
const MAX_SECTOR: u64 = 65_536;
const BLOB_TREE_ROOTS: u64 = 13;
// Two table pages per record (leaf and internal), up to five index pages
// per record and up to one overflow page per index key, plus rounding for
// the table record's overflow chain. Count overflow payload separately.
const STRUCTURAL_PAGES_PER_ROW: u64 = 13;

#[derive(Clone, Copy)]
struct WriteFootprint {
    main_growth: u64,
    wal_growth: u64,
}

impl WriteFootprint {
    fn for_write(page_size: u64, write: AuditWrite) -> Self {
        let (trees, overflow) = match write {
            // Table and five indices; every key can use one overflow page.
            AuditWrite::Mint { manifest_bytes, .. } => (
                6,
                (2 * (MINT_OVERHEAD + manifest_bytes)).div_ceil(page_size) + 5,
            ),
            // Read table and three indices; release uses one fewer index.
            AuditWrite::Read { .. } | AuditWrite::Release => (4, 4),
        };
        // At most two new siblings per level plus root growth. Count page
        // allocation/reuse against the main file even before checkpoint.
        let allocated = trees * (BTREE_LEVELS * MAX_ALLOCATION_PER_LEVEL + 1) + overflow;
        // At most five output siblings, their parent, and freelist pages
        // for two allocated and two discarded siblings per level. Count
        // root allocation, per-overflow freelist access, the audit counter
        // and the DB header. Cache spill is off during these transactions.
        let frames = trees * (BTREE_LEVELS * (MAX_OUTPUT_SIBLINGS + 5) + 2) + 2 * overflow + 2;
        Self {
            main_growth: allocated * page_size,
            wal_growth: WAL_HEADER
                + frames * (page_size + FRAME_HEADER)
                + MAX_SECTOR
                + page_size
                + FRAME_HEADER,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum AuditWrite {
    Mint {
        now: u64,
        manifest_bytes: u64,
        max_reads: u64,
    },
    Read {
        now: u64,
    },
    Release,
}

impl BlobLedger {
    pub fn start(authority: Arc<CapabilityAuthority>) -> Result<Self, BlobError> {
        let (db_path, page_size) = {
            let db = authority.lock()?;
            let path = db
                .path()
                .filter(|name| !name.is_empty())
                .map(PathBuf::from)
                .ok_or(BlobError::InvalidConfig)?;
            let page_size = db.pragma_query_value(None, "page_size", |row| row_u64(row, 0))?;
            let auto_vacuum = db.pragma_query_value(None, "auto_vacuum", |row| row_u64(row, 0))?;
            // Authority WAL fixes the page size. The footprint excludes
            // pointer-map traffic and therefore requires the default mode.
            if !(512..=65_536).contains(&page_size)
                || !page_size.is_power_of_two()
                || auto_vacuum != 0
            {
                return Err(BlobError::InvalidConfig);
            }
            (path, page_size)
        };
        // A stable sibling lock in the authority's managed DB directory is
        // released by the OS on process exit. Never unlink or replace it.
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(db_path.with_extension("blob.lock"))?;
        match lease.try_lock() {
            Ok(()) => (),
            Err(TryLockError::WouldBlock) => return Err(BlobError::InstanceActive),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let epoch = {
            let mut connection = authority.lock()?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let epoch: u64 = transaction.query_row(
                "UPDATE blob_boot SET epoch=epoch+1 WHERE singleton=1 RETURNING epoch",
                [],
                |row| row_u64(row, 0),
            )?;
            if epoch == 0 {
                return Err(BlobError::CorruptRecord);
            }
            transaction.commit()?;
            epoch
        };
        let mut wal = db_path.as_os_str().to_os_string();
        wal.push("-wal");
        Ok(Self {
            authority,
            epoch,
            db_path,
            wal_path: PathBuf::from(wal),
            page_size,
            _lease: lease,
        })
    }

    pub fn write<T>(
        &self,
        timing: &Timing,
        action: impl FnOnce(&Transaction<'_>) -> Result<T, BlobError>,
    ) -> Result<T, BlobError> {
        let mut action = Some(action);
        loop {
            let mut connection = self.lock(timing)?;
            let result = with_blob_writer_settings(&mut connection, |connection| {
                if !self.trim_wal(connection)? {
                    return Ok(None);
                }
                let transaction =
                    match connection.transaction_with_behavior(TransactionBehavior::Immediate) {
                        Ok(transaction) => transaction,
                        Err(error) if is_busy(&error) => return Ok(None),
                        Err(error) => return Err(error.into()),
                    };
                timing.check_elapsed()?;
                let epoch = transaction.query_row(
                    "SELECT epoch FROM blob_boot WHERE singleton=1",
                    [],
                    |row| row_u64(row, 0),
                )?;
                if epoch != self.epoch {
                    return Err(BlobError::NotFound);
                }
                let action = action.take().ok_or(BlobError::CorruptRecord)?;
                let result = action(&transaction)?;
                timing.check_elapsed()?;
                transaction.commit()?;
                Ok(Some(result))
            })?;
            if let Some(result) = result {
                return Ok(result);
            }
            drop(connection);
            timing.pause()?;
        }
    }

    pub fn lock(&self, timing: &Timing) -> Result<MutexGuard<'_, Connection>, BlobError> {
        loop {
            timing.check_elapsed()?;
            if let Some(connection) = self.authority.try_lock()? {
                return Ok(connection);
            }
            timing.pause()?;
        }
    }

    fn wal_window_bytes(&self) -> u64 {
        WAL_HEADER + WAL_WINDOW_FRAMES * (self.page_size + FRAME_HEADER)
    }

    fn trim_wal(&self, db: &Connection) -> Result<bool, BlobError> {
        if wal_size(&self.wal_path)? <= self.wal_window_bytes() {
            return Ok(true);
        }
        // Outside the write transaction, with SQLite busy waiting disabled.
        // A blocking reader/writer causes a timed retry without the mutex.
        match db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row_u64(row, 0)) {
            Ok(busy) => Ok(busy == 0),
            Err(error) if is_busy(&error) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub fn guard_storage(
        &self,
        db: &Connection,
        config: BlobConfig,
        write: AuditWrite,
    ) -> Result<(), BlobError> {
        let (rows, estimated): (u64, u64) = db.query_row(
            "SELECT rows, estimated_bytes FROM blob_audit_usage WHERE singleton=1",
            [],
            |row| Ok((row_u64(row, 0)?, row_u64(row, 1)?)),
        )?;
        if estimated < rows.saturating_mul(RECEIPT_ESTIMATE) {
            return Err(BlobError::CorruptRecord);
        }
        let main_bytes = fs::metadata(&self.db_path)?.len();
        let wal_bytes = wal_size(&self.wal_path)?;
        let observed = main_bytes.saturating_add(wal_bytes);
        let bytes = observed.max(estimated);
        let pages = db.pragma_query_value(None, "page_count", |row| row_u64(row, 0))?;
        let main = main_bytes.max(pages.saturating_mul(self.page_size));
        // A checkpoint may grow the DB to the logical page count before it
        // truncates the WAL. Count both copies when checking peak usage.
        let mutation = WriteFootprint::for_write(self.page_size, write);
        let required_for_commit = main
            .saturating_add(mutation.main_growth)
            .saturating_add(wal_bytes)
            .saturating_add(mutation.wal_growth);
        if required_for_commit > config.audit_bytes {
            return Err(BlobError::AuditFull);
        }
        let (now, manifest_bytes, new_reads) = match write {
            AuditWrite::Mint {
                now,
                manifest_bytes,
                max_reads,
            } => (now, manifest_bytes, max_reads),
            AuditWrite::Read { now } => {
                // Reads can spend their reservation while leaving room for
                // every currently available release, even near the byte cap.
                let releases = db.query_row(
                    "SELECT count(*) FROM blob_tokens AS t WHERE epoch=?1 AND valid_until_ms>?2
                        AND NOT EXISTS(SELECT 1 FROM blob_revocations AS v WHERE v.reference_id=t.reference_id)",
                    params![encode_u64(self.epoch)?, encode_u64(now)?],
                    |row| row_u64(row, 0),
                )?;
                return guard_receipts(config, rows, bytes, releases.saturating_add(1));
            }
            AuditWrite::Release => return guard_receipts(config, rows, bytes, 1),
        };
        let mint_bytes = MINT_OVERHEAD + manifest_bytes;
        let reserved: u64 = db.query_row(
            "SELECT COALESCE(SUM(1 + max_reads - COALESCE(
                (SELECT max(ordinal) FROM blob_read_receipts AS r WHERE r.reference_id=t.reference_id), 0)),0)
             FROM blob_tokens AS t
             WHERE epoch=?1 AND valid_until_ms>?2 AND NOT EXISTS(
                SELECT 1 FROM blob_revocations AS v WHERE v.reference_id=t.reference_id)",
            params![encode_u64(self.epoch)?, encode_u64(now)?], |row| row_u64(row, 0),
        )?;
        let projected_rows = rows.saturating_add(1);
        let projected_bytes = bytes.saturating_add(mint_bytes);
        let all_reservations = reserved.saturating_add(new_reads).saturating_add(1);
        let final_rows = projected_rows.saturating_add(all_reservations);
        let final_estimate = estimated
            .saturating_add(mint_bytes)
            .saturating_add(all_reservations.saturating_mul(RECEIPT_ESTIMATE));
        // Count the existing DB as baseline even though it contains part of
        // this history. The envelope can therefore survive redistribution of
        // every earlier Blob row without depending on current fill ratios.
        // Even at the smallest SQLite usable page size, one overflow page
        // carries at least half a page of logical payload.
        let full_trees = self
            .page_size
            .saturating_mul(
                BLOB_TREE_ROOTS.saturating_add(final_rows.saturating_mul(STRUCTURAL_PAGES_PER_ROW)),
            )
            .saturating_add(final_estimate.saturating_mul(2));
        let receipt = WriteFootprint::for_write(self.page_size, AuditWrite::Release);
        // A previous split can leave reusable pages allocated at the file's
        // high-water mark. Allow the larger mint/receipt transient as well
        // as the next receipt's conservative growth check.
        let fully_reserved = main
            .saturating_add(full_trees)
            .saturating_add(mutation.main_growth.max(receipt.main_growth))
            .saturating_add(receipt.main_growth)
            .saturating_add(self.wal_window_bytes())
            .saturating_add(receipt.wal_growth);
        if projected_rows > config.audit_rows * 3 / 4
            || projected_bytes > config.audit_bytes * 3 / 4
            || final_rows > config.audit_rows
            || final_estimate > config.audit_bytes
            || fully_reserved > config.audit_bytes
        {
            return Err(BlobError::AuditFull);
        }
        Ok(())
    }

    pub fn guard_mint_rate(
        db: &Connection,
        config: BlobConfig,
        actor: PrincipalId,
        now: u64,
    ) -> Result<(), BlobError> {
        let (global, own): (u64, u64) = db.query_row(
            "SELECT count(*), COALESCE(sum(issuer=?1),0) FROM blob_tokens WHERE minted_at_ms>=?2",
            params![
                actor.as_bytes(),
                encode_u64(now.saturating_add(1).saturating_sub(RATE_WINDOW_MS))?
            ],
            |row| Ok((row_u64(row, 0)?, row_u64(row, 1)?)),
        )?;
        if global >= config.mints_global as u64 || own >= config.mints_per_principal as u64 {
            return Err(BlobError::RateLimited);
        }
        Ok(())
    }
}

fn with_blob_writer_settings<T>(
    db: &mut Connection,
    action: impl FnOnce(&mut Connection) -> Result<T, BlobError>,
) -> Result<T, BlobError> {
    let previous = db.pragma_query_value(None, "busy_timeout", |row| row_u64(row, 0))?;
    let previous_spill: i64 = db.pragma_query_value(None, "cache_spill", |row| row.get(0))?;
    db.busy_timeout(Duration::ZERO)?;
    let result = db
        .pragma_update(None, "cache_spill", false)
        .map_err(BlobError::from)
        .and_then(|()| action(db));
    let restore_spill = db.pragma_update(None, "cache_spill", previous_spill);
    let restore_busy = db.busy_timeout(Duration::from_millis(previous));
    restore_spill?;
    restore_busy?;
    result
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn guard_receipts(
    config: BlobConfig,
    rows: u64,
    bytes: u64,
    receipt_count: u64,
) -> Result<(), BlobError> {
    if rows.saturating_add(receipt_count) > config.audit_rows
        || bytes.saturating_add(receipt_count.saturating_mul(RECEIPT_ESTIMATE)) > config.audit_bytes
    {
        return Err(BlobError::AuditFull);
    }
    Ok(())
}

fn wal_size(path: &Path) -> Result<u64, BlobError> {
    match fs::metadata(path) {
        Ok(meta) => Ok(meta.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn check_grant(
    db: &Connection,
    source: CapabilityHandle,
    holder: PrincipalId,
    namespace: NamespaceId,
    now: u64,
) -> Result<CapabilityRecord, BlobError> {
    let record = load_exact_current(db, source)?;
    validate_active_chain(db, record, now)?;
    if record.holder != holder {
        return Err(crate::CapabilityAuthorityError::HolderMismatch.into());
    }
    if record.target != CapabilityTarget::Namespace(namespace) {
        return Err(crate::CapabilityAuthorityError::TargetMismatch.into());
    }
    Ok(record)
}

pub(super) fn mint_replay(
    db: &Connection,
    workspace: WorkspaceBinding,
    op: OperationKey,
) -> Result<Option<ReferenceId>, BlobError> {
    let stored = db.query_row(
        "SELECT reference_id, request_digest FROM blob_tokens WHERE issuer=?1 AND application=?2
            AND workspace=?3 AND namespace=?4 AND idempotency_key=?5",
        params![op.actor.as_bytes(), op.application.as_bytes(), workspace.workspace.as_bytes(), workspace.namespace.as_bytes(), op.key.as_bytes()],
        |row| Ok((row.get::<_, ReferenceId>(0)?, row.get::<_, [u8; 32]>(1)?)),
    ).optional()?;
    match stored {
        Some((reference, digest)) if digest == op.digest => Ok(Some(reference)),
        Some(_) => Err(BlobError::IdempotencyConflict),
        None => Ok(None),
    }
}

pub(super) fn insert_mint(
    db: &Connection,
    token: &Token,
    op: OperationKey,
    request: &MintReferenceRequest,
    manifest: &[u8],
    proof: &SignedRecord<'_>,
) -> Result<(), BlobError> {
    db.execute(
        "INSERT INTO blob_tokens VALUES (
            ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27)",
        params![
            token.reference, token.receipt.as_bytes(), encode_u64(token.epoch)?, token.issuer.as_bytes(),
            op.application.as_bytes(), token.holder.as_bytes(), token.workspace.workspace.as_bytes(),
            token.workspace.namespace.as_bytes(), encode_u64(token.workspace.revision.get())?, token.source.capability_id.as_bytes(),
            encode_u64(token.source.generation.get())?, &request.path_handle, &request.file_digest_sha256, encode_u64(token.size)?,
            encode_u64(token.block_bytes)?, manifest, encode_u64(token.max_reads)?, encode_u64(token.expiry)?, encode_u64(token.minted_at)?, encode_u64(proof.signed_at)?,
            proof.signer.key_id().as_bytes(), encode_u64(proof.signer.key_generation().get())?, proof.signature, op.key.as_bytes(), op.digest,
            proof.caller.process.as_bytes(), encode_u64(proof.caller.process_generation.get())?,
        ],
    )?;
    Ok(())
}

pub(super) fn load_token(db: &Connection, reference: ReferenceId) -> Result<Token, BlobError> {
    db.query_row(
        "SELECT receipt_id,epoch,issuer,holder,workspace,namespace,workspace_revision,source,
                source_generation,size_bytes,block_bytes,max_reads,valid_until_ms,minted_at_ms
         FROM blob_tokens WHERE reference_id=?1",
        [reference],
        |row| {
            Ok(Token {
                reference,
                receipt: ReceiptId::from_bytes(row.get(0)?),
                epoch: row_u64(row, 1)?,
                issuer: PrincipalId::from_bytes(row.get(2)?),
                holder: PrincipalId::from_bytes(row.get(3)?),
                workspace: WorkspaceBinding {
                    workspace: WorkspaceId::from_bytes(row.get(4)?),
                    namespace: NamespaceId::from_bytes(row.get(5)?),
                    revision: generation(row_u64(row, 6)?)?,
                },
                source: CapabilityHandle {
                    capability_id: CapabilityId::from_bytes(row.get(7)?),
                    generation: generation(row_u64(row, 8)?)?,
                },
                size: row_u64(row, 9)?,
                block_bytes: row_u64(row, 10)?,
                max_reads: row_u64(row, 11)?,
                expiry: row_u64(row, 12)?,
                minted_at: row_u64(row, 13)?,
            })
        },
    )
    .optional()?
    .ok_or(BlobError::NotFound)
}

fn generation(number: u64) -> Result<Generation, rusqlite::Error> {
    std::num::NonZeroU64::new(number)
        .map(Generation::new)
        .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, 0))
}

pub(super) fn latest_ordinal(db: &Connection, reference: ReferenceId) -> Result<u64, BlobError> {
    Ok(db.query_row(
        "SELECT COALESCE(max(ordinal),0) FROM blob_read_receipts WHERE reference_id=?1",
        [reference],
        |row| row_u64(row, 0),
    )?)
}

pub(super) fn is_released(db: &Connection, reference: ReferenceId) -> Result<bool, BlobError> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM blob_revocations WHERE reference_id=?1)",
        [reference],
        |row| row.get(0),
    )?)
}

pub(super) fn read_replay(
    db: &Connection,
    reference: ReferenceId,
    op: OperationKey,
    block: ReadBlock,
) -> Result<Option<ReceiptId>, BlobError> {
    let row = db.query_row(
        "SELECT receipt_id, request_digest, block_offset, block_size, block_digest
         FROM blob_read_receipts WHERE reference_id=?1 AND actor=?2 AND application=?3 AND idempotency_key=?4",
        params![reference, op.actor.as_bytes(), op.application.as_bytes(), op.key.as_bytes()],
        |row| Ok((row.get(0)?, row.get::<_, [u8; 32]>(1)?, row_u64(row, 2)?, row_u64(row, 3)?, row.get::<_, [u8; 32]>(4)?)),
    ).optional()?;
    if let Some((receipt, digest, offset, size, hash)) = row {
        if digest != op.digest {
            return Err(BlobError::IdempotencyConflict);
        }
        if (offset, size, hash) != (block.offset, block.size, block.digest) {
            return Err(BlobError::CorruptRecord);
        }
        return Ok(Some(ReceiptId::from_bytes(receipt)));
    }
    Ok(None)
}

pub(super) fn durable_block_digest(
    db: &Connection,
    token: &Token,
    block_offset: u64,
) -> Result<[u8; 32], BlobError> {
    let start = (block_offset / token.block_bytes) * 32 + 1;
    Ok(db.query_row(
        "SELECT substr(manifest,?2,32) FROM blob_tokens WHERE reference_id=?1",
        params![token.reference, encode_u64(start)?],
        |row| row.get(0),
    )?)
}

pub(super) fn insert_read(
    db: &Connection,
    reference: ReferenceId,
    op: OperationKey,
    block: ReadBlock,
    ordinal: u64,
    now: u64,
) -> Result<ReceiptId, BlobError> {
    let receipt = operation_receipt(b"nlos/blob/read/receipt/v1", reference, op);
    db.execute(
        "INSERT INTO blob_read_receipts VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            reference,
            op.actor.as_bytes(),
            op.application.as_bytes(),
            op.key.as_bytes(),
            op.digest,
            receipt.as_bytes(),
            encode_u64(ordinal)?,
            encode_u64(block.offset)?,
            encode_u64(block.size)?,
            block.digest,
            encode_u64(now)?
        ],
    )?;
    Ok(receipt)
}

pub(super) fn release_replay(
    db: &Connection,
    reference: ReferenceId,
    op: OperationKey,
) -> Result<Option<ReceiptId>, BlobError> {
    let row = db.query_row(
        "SELECT receipt_id,actor,application,idempotency_key,request_digest FROM blob_revocations WHERE reference_id=?1",
        [reference],
        |row| Ok((row.get(0)?, row.get::<_, [u8; 16]>(1)?, row.get::<_, [u8; 16]>(2)?, row.get::<_, [u8; 16]>(3)?, row.get::<_, [u8; 32]>(4)?)),
    ).optional()?;
    match row {
        Some((receipt, actor, application, key, digest))
            if actor == op.actor.into_bytes()
                && application == op.application.into_bytes()
                && key == op.key.into_bytes() =>
        {
            if digest != op.digest {
                return Err(BlobError::IdempotencyConflict);
            }
            Ok(Some(ReceiptId::from_bytes(receipt)))
        }
        Some(_) => Err(BlobError::Released),
        None => Ok(None),
    }
}

pub(super) fn insert_release(
    db: &Connection,
    reference: ReferenceId,
    op: OperationKey,
    now: u64,
    proof: &SignedRecord<'_>,
) -> Result<ReceiptId, BlobError> {
    let receipt = operation_receipt(b"nlos/blob/release/receipt/v1", reference, op);
    db.execute(
        "INSERT INTO blob_revocations VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![
            reference,
            receipt.as_bytes(),
            op.actor.as_bytes(),
            op.application.as_bytes(),
            op.key.as_bytes(),
            op.digest,
            proof.signer.key_id().as_bytes(),
            encode_u64(proof.signer.key_generation().get())?,
            proof.signature,
            encode_u64(now)?,
            encode_u64(proof.signed_at)?,
            proof.caller.process.as_bytes(),
            encode_u64(proof.caller.process_generation.get())?
        ],
    )?;
    Ok(receipt)
}

pub(super) fn operation_receipt(
    domain: &[u8],
    reference: ReferenceId,
    op: OperationKey,
) -> ReceiptId {
    ReceiptId::from_bytes(derive_id(
        domain,
        &[
            &reference,
            op.actor.as_bytes(),
            op.application.as_bytes(),
            op.key.as_bytes(),
        ],
    ))
}

fn row_u64(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_writer_disables_sqlite_busy_wait_and_restores_it_on_error() {
        let mut db = Connection::open_in_memory().unwrap();
        db.busy_timeout(Duration::from_millis(250)).unwrap();

        let result: Result<(), BlobError> = with_blob_writer_settings(&mut db, |writer| {
            let busy_ms: i64 = writer
                .pragma_query_value(None, "busy_timeout", |row| row.get(0))
                .unwrap();
            assert_eq!(busy_ms, 0);
            Err(BlobError::Busy)
        });

        assert!(matches!(result, Err(BlobError::Busy)));
        let restored_ms: i64 = db
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        assert_eq!(restored_ms, 250);
    }
}
