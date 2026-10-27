//! Durable `SQLite` (`WAL + FULL`) extension tables for cross-host conservative
//! clock-skew horizons, multi-endpoint [`FenceBarrierAck`] sets, signed holder
//! close receipts, and `[LEASE-PROTO-001]` idempotency records.

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use nlos_cell::{CellEpoch, CellFencingToken, CellIdentity};
use nlos_types::{
    Generation, IdempotencyKey, KeyId, PrincipalId, QuotaLeaseId, ReceiptId, SchedulerDomainId,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::{FenceBarrierAck, FenceScope, LeaseStoreError, SqliteLeaseStore};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum StoredLeaseFamily {
    Quota,
    Capacity,
    Device,
}

impl StoredLeaseFamily {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Quota => "QUOTA",
            Self::Capacity => "CAPACITY",
            Self::Device => "DEVICE",
        }
    }

    fn from_str(raw: &str) -> Result<Self, LeaseStoreError> {
        match raw {
            "QUOTA" => Ok(Self::Quota),
            "CAPACITY" => Ok(Self::Capacity),
            "DEVICE" => Ok(Self::Device),
            _ => Err(LeaseStoreError::CorruptRecord(
                "unknown cross-host lease_family",
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredHorizonRow {
    pub(crate) family: StoredLeaseFamily,
    pub(crate) lease_id: [u8; 16],
    pub(crate) holder_cell: CellIdentity,
    pub(crate) holder_boot_generation: Generation,
    pub(crate) grantor_commit_mono_ms: u64,
    pub(crate) ttl_grant_ms: u64,
    pub(crate) max_clock_skew_ms: NonZeroU64,
    pub(crate) safety_margin_ms: NonZeroU64,
    pub(crate) authority_expiry_tick: u64,
    pub(crate) renewal_seq: u64,
    pub(crate) multi_endpoint_gateways: Vec<[u8; 16]>,
    pub(crate) disposition_code: u8,
    pub(crate) cleanup_proof_kind: Option<u8>,
    pub(crate) cleanup_proof_digest: Option<[u8; 32]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StoredSignedCloseReceiptRow {
    pub(crate) lease_id: QuotaLeaseId,
    pub(crate) receipt_id: ReceiptId,
    pub(crate) holder_cell: CellIdentity,
    pub(crate) holder_boot_generation: Generation,
    pub(crate) epoch: CellEpoch,
    pub(crate) fencing_token: CellFencingToken,
    pub(crate) face_value: u64,
    pub(crate) final_high_water_spent: u64,
    pub(crate) returned_unspent: u64,
    pub(crate) receipt_digest: [u8; 32],
    pub(crate) signer_principal_id: PrincipalId,
    pub(crate) signer_key_id: KeyId,
    pub(crate) signer_key_generation: Generation,
    pub(crate) ed25519_signature: [u8; 64],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredIdempotencyRow {
    pub(crate) idempotency_key: IdempotencyKey,
    pub(crate) caller_cell: CellIdentity,
    pub(crate) command_digest: [u8; 32],
    pub(crate) durable_state_seq: u64,
    pub(crate) response_bytes: Vec<u8>,
}

pub(crate) struct RehydratedCrossHostState {
    pub(crate) horizons: BTreeMap<(StoredLeaseFamily, [u8; 16]), StoredHorizonRow>,
    pub(crate) barrier_acks:
        BTreeMap<(StoredLeaseFamily, [u8; 16]), BTreeMap<[u8; 16], FenceBarrierAck>>,
    pub(crate) close_receipts: BTreeMap<QuotaLeaseId, StoredSignedCloseReceiptRow>,
    pub(crate) idempotency_records: BTreeMap<IdempotencyKey, StoredIdempotencyRow>,
    pub(crate) last_state_seq: u64,
}

const CROSS_HOST_DDL: &str = "
CREATE TABLE IF NOT EXISTS cross_host_lease_horizons (
    lease_family TEXT NOT NULL CHECK(lease_family IN ('QUOTA', 'CAPACITY', 'DEVICE')),
    lease_id BLOB NOT NULL CHECK(length(lease_id) = 16),
    holder_cell BLOB NOT NULL CHECK(length(holder_cell) = 16),
    holder_boot_generation INTEGER NOT NULL CHECK(holder_boot_generation >= 1),
    grantor_commit_mono_ms INTEGER NOT NULL CHECK(grantor_commit_mono_ms >= 0),
    ttl_grant_ms INTEGER NOT NULL CHECK(ttl_grant_ms >= 1),
    max_clock_skew_ms INTEGER NOT NULL CHECK(max_clock_skew_ms >= 1),
    safety_margin_ms INTEGER NOT NULL CHECK(safety_margin_ms >= 1),
    authority_expiry_tick INTEGER NOT NULL CHECK(authority_expiry_tick >= 0),
    renewal_seq INTEGER NOT NULL CHECK(renewal_seq >= 0),
    multi_endpoint_scope_blob BLOB NOT NULL CHECK(
        length(multi_endpoint_scope_blob) >= 16 AND length(multi_endpoint_scope_blob) % 16 = 0
    ),
    disposition_code INTEGER NOT NULL CHECK(disposition_code IN (0, 1, 2)),
    cleanup_proof_kind INTEGER CHECK(cleanup_proof_kind IS NULL OR cleanup_proof_kind IN (1, 2, 3)),
    cleanup_proof_digest BLOB CHECK(cleanup_proof_digest IS NULL OR length(cleanup_proof_digest) = 32),
    CHECK(
        (disposition_code = 2 AND cleanup_proof_kind IS NOT NULL AND cleanup_proof_digest IS NOT NULL)
        OR (disposition_code IN (0, 1) AND cleanup_proof_kind IS NULL AND cleanup_proof_digest IS NULL)
    ),
    PRIMARY KEY (lease_family, lease_id)
) STRICT;

CREATE TRIGGER IF NOT EXISTS cross_host_lease_horizons_no_delete
BEFORE DELETE ON cross_host_lease_horizons BEGIN
    SELECT RAISE(ABORT, 'cross_host_lease_horizons rows are durable');
END;

CREATE TRIGGER IF NOT EXISTS cross_host_lease_horizons_monotonic
BEFORE UPDATE ON cross_host_lease_horizons
WHEN NEW.lease_family != OLD.lease_family
  OR NEW.lease_id != OLD.lease_id
  OR NEW.holder_cell != OLD.holder_cell
  OR NEW.holder_boot_generation != OLD.holder_boot_generation
  OR NEW.max_clock_skew_ms != OLD.max_clock_skew_ms
  OR NEW.safety_margin_ms != OLD.safety_margin_ms
  OR NEW.multi_endpoint_scope_blob != OLD.multi_endpoint_scope_blob
  OR NEW.grantor_commit_mono_ms < OLD.grantor_commit_mono_ms
  OR NEW.renewal_seq < OLD.renewal_seq
  OR (OLD.disposition_code = 2 AND NEW.disposition_code != 2)
BEGIN
    SELECT RAISE(ABORT, 'cross_host_lease_horizons monotonic or identity invariant violated');
END;

CREATE TABLE IF NOT EXISTS cross_host_fence_barrier_acks (
    lease_family TEXT NOT NULL CHECK(lease_family IN ('QUOTA', 'CAPACITY', 'DEVICE')),
    lease_id BLOB NOT NULL CHECK(length(lease_id) = 16),
    scope_cell BLOB NOT NULL CHECK(length(scope_cell) = 16),
    gateway_id BLOB NOT NULL CHECK(length(gateway_id) = 16),
    epoch INTEGER NOT NULL CHECK(epoch >= 1),
    fencing_token INTEGER NOT NULL CHECK(fencing_token >= 1),
    observed_high_water INTEGER NOT NULL CHECK(observed_high_water >= 0),
    ack_digest BLOB NOT NULL CHECK(length(ack_digest) = 32),
    PRIMARY KEY (lease_family, lease_id, gateway_id)
) STRICT;

CREATE TRIGGER IF NOT EXISTS cross_host_fence_barrier_acks_no_update
BEFORE UPDATE ON cross_host_fence_barrier_acks BEGIN
    SELECT RAISE(ABORT, 'cross_host_fence_barrier_acks is append-only');
END;

CREATE TRIGGER IF NOT EXISTS cross_host_fence_barrier_acks_no_delete
BEFORE DELETE ON cross_host_fence_barrier_acks BEGIN
    SELECT RAISE(ABORT, 'cross_host_fence_barrier_acks is durable');
END;

CREATE TABLE IF NOT EXISTS cross_host_close_receipts (
    lease_id BLOB PRIMARY KEY NOT NULL CHECK(length(lease_id) = 16),
    receipt_id BLOB NOT NULL UNIQUE CHECK(length(receipt_id) = 16),
    holder_cell BLOB NOT NULL CHECK(length(holder_cell) = 16),
    holder_boot_generation INTEGER NOT NULL CHECK(holder_boot_generation >= 1),
    epoch INTEGER NOT NULL CHECK(epoch >= 1),
    fencing_token INTEGER NOT NULL CHECK(fencing_token >= 1),
    face_value INTEGER NOT NULL CHECK(face_value >= 0),
    final_high_water_spent INTEGER NOT NULL CHECK(final_high_water_spent >= 0),
    returned_unspent INTEGER NOT NULL CHECK(returned_unspent >= 0),
    receipt_digest BLOB NOT NULL CHECK(length(receipt_digest) = 32),
    signer_principal_id BLOB NOT NULL CHECK(length(signer_principal_id) = 16),
    signer_key_id BLOB NOT NULL CHECK(length(signer_key_id) = 16),
    signer_key_generation INTEGER NOT NULL CHECK(signer_key_generation >= 1),
    ed25519_signature BLOB NOT NULL CHECK(length(ed25519_signature) = 64),
    CHECK(final_high_water_spent + returned_unspent = face_value)
) STRICT;

CREATE TRIGGER IF NOT EXISTS cross_host_close_receipts_no_update
BEFORE UPDATE ON cross_host_close_receipts BEGIN
    SELECT RAISE(ABORT, 'cross_host_close_receipts is immutable');
END;

CREATE TRIGGER IF NOT EXISTS cross_host_close_receipts_no_delete
BEFORE DELETE ON cross_host_close_receipts BEGIN
    SELECT RAISE(ABORT, 'cross_host_close_receipts is durable');
END;

CREATE TABLE IF NOT EXISTS cross_host_proto_state_seq (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK(singleton = 1),
    last_state_seq INTEGER NOT NULL CHECK(last_state_seq >= 0)
) STRICT;

CREATE TRIGGER IF NOT EXISTS cross_host_proto_state_seq_no_delete
BEFORE DELETE ON cross_host_proto_state_seq BEGIN
    SELECT RAISE(ABORT, 'cross_host_proto_state_seq is durable');
END;

CREATE TRIGGER IF NOT EXISTS cross_host_proto_state_seq_monotonic
BEFORE UPDATE ON cross_host_proto_state_seq
WHEN NEW.singleton != OLD.singleton OR NEW.last_state_seq < OLD.last_state_seq
BEGIN
    SELECT RAISE(ABORT, 'cross_host_proto_state_seq monotonic invariant violated');
END;

CREATE TABLE IF NOT EXISTS cross_host_proto_idempotency (
    idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key) = 16),
    caller_cell BLOB NOT NULL CHECK(length(caller_cell) = 16),
    command_digest BLOB NOT NULL CHECK(length(command_digest) = 32),
    durable_state_seq INTEGER NOT NULL CHECK(durable_state_seq >= 1),
    response_bytes BLOB NOT NULL
) STRICT;

CREATE TRIGGER IF NOT EXISTS cross_host_proto_idempotency_no_update
BEFORE UPDATE ON cross_host_proto_idempotency BEGIN
    SELECT RAISE(ABORT, 'cross_host_proto_idempotency is immutable');
END;

CREATE TRIGGER IF NOT EXISTS cross_host_proto_idempotency_no_delete
BEFORE DELETE ON cross_host_proto_idempotency BEGIN
    SELECT RAISE(ABORT, 'cross_host_proto_idempotency is durable');
END;
";

impl SqliteLeaseStore {
    pub(crate) fn ensure_and_rehydrate_cross_host(
        &self,
    ) -> Result<RehydratedCrossHostState, LeaseStoreError> {
        let mut conn = self.lock()?;
        ensure_cross_host_ddl(&mut conn)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let horizons = load_horizon_rows(&tx)?;
        let barrier_acks = load_barrier_ack_rows(&tx)?;
        let close_receipts = load_close_receipt_rows(&tx)?;
        let idempotency_records = load_idempotency_rows(&tx)?;
        let last_state_seq: i64 = tx.query_row(
            "SELECT last_state_seq FROM cross_host_proto_state_seq WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(RehydratedCrossHostState {
            horizons,
            barrier_acks,
            close_receipts,
            idempotency_records,
            last_state_seq: to_u64(last_state_seq)?,
        })
    }

    pub(crate) fn persist_cross_host_horizon(
        &self,
        row: &StoredHorizonRow,
    ) -> Result<(), LeaseStoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let scope_blob = encode_gateways(&row.multi_endpoint_gateways);
        tx.execute(
            "INSERT INTO cross_host_lease_horizons (
                lease_family, lease_id, holder_cell, holder_boot_generation,
                grantor_commit_mono_ms, ttl_grant_ms, max_clock_skew_ms, safety_margin_ms,
                authority_expiry_tick, renewal_seq, multi_endpoint_scope_blob,
                disposition_code, cleanup_proof_kind, cleanup_proof_digest
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                row.family.as_str(),
                row.lease_id.as_slice(),
                row.holder_cell.as_bytes().as_slice(),
                to_i64(row.holder_boot_generation.get())?,
                to_i64(row.grantor_commit_mono_ms)?,
                to_i64(row.ttl_grant_ms)?,
                to_i64(row.max_clock_skew_ms.get())?,
                to_i64(row.safety_margin_ms.get())?,
                to_i64(row.authority_expiry_tick)?,
                to_i64(row.renewal_seq)?,
                scope_blob.as_slice(),
                i64::from(row.disposition_code),
                row.cleanup_proof_kind.map(i64::from),
                row.cleanup_proof_digest.as_ref().map(<[u8; 32]>::as_slice),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn persist_cross_host_horizon_update(
        &self,
        row: &StoredHorizonRow,
    ) -> Result<(), LeaseStoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE cross_host_lease_horizons
             SET grantor_commit_mono_ms = ?1,
                 ttl_grant_ms = ?2,
                 authority_expiry_tick = ?3,
                 renewal_seq = ?4,
                 disposition_code = ?5,
                 cleanup_proof_kind = ?6,
                 cleanup_proof_digest = ?7
             WHERE lease_family = ?8 AND lease_id = ?9",
            params![
                to_i64(row.grantor_commit_mono_ms)?,
                to_i64(row.ttl_grant_ms)?,
                to_i64(row.authority_expiry_tick)?,
                to_i64(row.renewal_seq)?,
                i64::from(row.disposition_code),
                row.cleanup_proof_kind.map(i64::from),
                row.cleanup_proof_digest.as_ref().map(<[u8; 32]>::as_slice),
                row.family.as_str(),
                row.lease_id.as_slice(),
            ],
        )?;
        if changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "cross_host_lease_horizons update CAS lost",
            ));
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn persist_cross_host_barrier_ack(
        &self,
        family: StoredLeaseFamily,
        ack: &FenceBarrierAck,
    ) -> Result<(), LeaseStoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO cross_host_fence_barrier_acks (
                lease_family, lease_id, scope_cell, gateway_id, epoch,
                fencing_token, observed_high_water, ack_digest
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                family.as_str(),
                ack.lease_id().as_slice(),
                ack.fence_scope().as_cell().as_bytes().as_slice(),
                ack.gateway_id().as_slice(),
                to_i64(ack.epoch().get())?,
                to_i64(ack.fencing_token().get())?,
                to_i64(ack.observed_high_water())?,
                ack.ack_digest().as_slice(),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn persist_cross_host_close_receipt(
        &self,
        row: &StoredSignedCloseReceiptRow,
    ) -> Result<(), LeaseStoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO cross_host_close_receipts (
                lease_id, receipt_id, holder_cell, holder_boot_generation,
                epoch, fencing_token, face_value, final_high_water_spent,
                returned_unspent, receipt_digest, signer_principal_id,
                signer_key_id, signer_key_generation, ed25519_signature
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                row.lease_id.as_bytes().as_slice(),
                row.receipt_id.as_bytes().as_slice(),
                row.holder_cell.as_bytes().as_slice(),
                to_i64(row.holder_boot_generation.get())?,
                to_i64(row.epoch.get())?,
                to_i64(row.fencing_token.get())?,
                to_i64(row.face_value)?,
                to_i64(row.final_high_water_spent)?,
                to_i64(row.returned_unspent)?,
                row.receipt_digest.as_slice(),
                row.signer_principal_id.as_bytes().as_slice(),
                row.signer_key_id.as_bytes().as_slice(),
                to_i64(row.signer_key_generation.get())?,
                row.ed25519_signature.as_slice(),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn persist_cross_host_idempotency(
        &self,
        row: &StoredIdempotencyRow,
        expected_prev_seq: u64,
    ) -> Result<(), LeaseStoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE cross_host_proto_state_seq
             SET last_state_seq = ?1
             WHERE singleton = 1 AND last_state_seq = ?2",
            params![to_i64(row.durable_state_seq)?, to_i64(expected_prev_seq)?],
        )?;
        if changed != 1 {
            return Err(LeaseStoreError::CorruptRecord(
                "cross_host_proto_state_seq CAS lost",
            ));
        }
        tx.execute(
            "INSERT INTO cross_host_proto_idempotency (
                idempotency_key, caller_cell, command_digest, durable_state_seq, response_bytes
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                row.idempotency_key.as_bytes().as_slice(),
                row.caller_cell.as_bytes().as_slice(),
                row.command_digest.as_slice(),
                to_i64(row.durable_state_seq)?,
                row.response_bytes.as_slice(),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn ensure_cross_host_ddl(conn: &mut Connection) -> Result<(), LeaseStoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(CROSS_HOST_DDL)?;
    let existing: Option<i64> = tx
        .query_row(
            "SELECT last_state_seq FROM cross_host_proto_state_seq WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if existing.is_none() {
        tx.execute(
            "INSERT INTO cross_host_proto_state_seq (singleton, last_state_seq) VALUES (1, 0)",
            [],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn load_horizon_rows(
    tx: &Transaction<'_>,
) -> Result<BTreeMap<(StoredLeaseFamily, [u8; 16]), StoredHorizonRow>, LeaseStoreError> {
    let mut stmt = tx.prepare(
        "SELECT lease_family, lease_id, holder_cell, holder_boot_generation,
                grantor_commit_mono_ms, ttl_grant_ms, max_clock_skew_ms, safety_margin_ms,
                authority_expiry_tick, renewal_seq, multi_endpoint_scope_blob,
                disposition_code, cleanup_proof_kind, cleanup_proof_digest
         FROM cross_host_lease_horizons",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let family_str: String = row.get(0)?;
        let family = StoredLeaseFamily::from_str(&family_str)?;
        let lease_id = bytes_16(row.get(1)?)?;
        let holder_cell = cell_from_blob(row.get(2)?)?;
        let holder_boot_generation = nz_gen(row.get(3)?)?;
        let grantor_commit_mono_ms = to_u64(row.get(4)?)?;
        let ttl_grant_ms = to_u64(row.get(5)?)?;
        let max_clock_skew_ms = nz_u64(row.get(6)?)?;
        let safety_margin_ms = nz_u64(row.get(7)?)?;
        let authority_expiry_tick = to_u64(row.get(8)?)?;
        let renewal_seq = to_u64(row.get(9)?)?;
        let scope_blob: Vec<u8> = row.get(10)?;
        let multi_endpoint_gateways = decode_gateways(&scope_blob)?;
        let disp_raw: i64 = row.get(11)?;
        let disposition_code = u8::try_from(disp_raw)
            .map_err(|_| LeaseStoreError::CorruptRecord("invalid disposition_code"))?;
        let kind_opt: Option<i64> = row.get(12)?;
        let cleanup_proof_kind = kind_opt
            .map(|k| {
                u8::try_from(k)
                    .map_err(|_| LeaseStoreError::CorruptRecord("invalid cleanup_proof_kind"))
            })
            .transpose()?;
        let digest_opt: Option<Vec<u8>> = row.get(13)?;
        let cleanup_proof_digest = digest_opt.map(bytes_32).transpose()?;

        out.insert(
            (family, lease_id),
            StoredHorizonRow {
                family,
                lease_id,
                holder_cell,
                holder_boot_generation,
                grantor_commit_mono_ms,
                ttl_grant_ms,
                max_clock_skew_ms,
                safety_margin_ms,
                authority_expiry_tick,
                renewal_seq,
                multi_endpoint_gateways,
                disposition_code,
                cleanup_proof_kind,
                cleanup_proof_digest,
            },
        );
    }
    Ok(out)
}

type StoredBarrierAckMap =
    BTreeMap<(StoredLeaseFamily, [u8; 16]), BTreeMap<[u8; 16], FenceBarrierAck>>;

fn load_barrier_ack_rows(tx: &Transaction<'_>) -> Result<StoredBarrierAckMap, LeaseStoreError> {
    let mut stmt = tx.prepare(
        "SELECT lease_family, lease_id, scope_cell, gateway_id, epoch,
                fencing_token, observed_high_water, ack_digest
         FROM cross_host_fence_barrier_acks",
    )?;
    let mut rows = stmt.query([])?;
    let mut out: StoredBarrierAckMap = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let family_str: String = row.get(0)?;
        let family = StoredLeaseFamily::from_str(&family_str)?;
        let lease_id = bytes_16(row.get(1)?)?;
        let scope_cell = cell_from_blob(row.get(2)?)?;
        let gateway_id = bytes_16(row.get(3)?)?;
        let epoch = CellEpoch::from_u64(to_u64(row.get(4)?)?)
            .ok_or(LeaseStoreError::CorruptRecord("zero epoch in ack"))?;
        let fencing_token = CellFencingToken::from_u64(to_u64(row.get(5)?)?)
            .ok_or(LeaseStoreError::CorruptRecord("zero token in ack"))?;
        let observed_high_water = to_u64(row.get(6)?)?;
        let ack_digest = bytes_32(row.get(7)?)?;
        let ack = FenceBarrierAck::new(
            lease_id,
            FenceScope::cell(scope_cell),
            epoch,
            fencing_token,
            gateway_id,
            observed_high_water,
            ack_digest,
        );
        if !ack.verify_digest() {
            return Err(LeaseStoreError::CorruptRecord(
                "corrupt cross_host_fence_barrier_acks digest",
            ));
        }
        out.entry((family, lease_id))
            .or_default()
            .insert(gateway_id, ack);
    }
    Ok(out)
}

fn load_close_receipt_rows(
    tx: &Transaction<'_>,
) -> Result<BTreeMap<QuotaLeaseId, StoredSignedCloseReceiptRow>, LeaseStoreError> {
    let mut stmt = tx.prepare(
        "SELECT lease_id, receipt_id, holder_cell, holder_boot_generation,
                epoch, fencing_token, face_value, final_high_water_spent,
                returned_unspent, receipt_digest, signer_principal_id,
                signer_key_id, signer_key_generation, ed25519_signature
         FROM cross_host_close_receipts",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let lease_id = QuotaLeaseId::from_bytes(bytes_16(row.get(0)?)?);
        let receipt_id = ReceiptId::from_bytes(bytes_16(row.get(1)?)?);
        let holder_cell = cell_from_blob(row.get(2)?)?;
        let holder_boot_generation = nz_gen(row.get(3)?)?;
        let epoch = CellEpoch::from_u64(to_u64(row.get(4)?)?).ok_or(
            LeaseStoreError::CorruptRecord("zero epoch in close receipt"),
        )?;
        let fencing_token = CellFencingToken::from_u64(to_u64(row.get(5)?)?).ok_or(
            LeaseStoreError::CorruptRecord("zero token in close receipt"),
        )?;
        let face_value = to_u64(row.get(6)?)?;
        let final_high_water_spent = to_u64(row.get(7)?)?;
        let returned_unspent = to_u64(row.get(8)?)?;
        let receipt_digest = bytes_32(row.get(9)?)?;
        let signer_principal_id = PrincipalId::from_bytes(bytes_16(row.get(10)?)?);
        let signer_key_id = KeyId::from_bytes(bytes_16(row.get(11)?)?);
        let signer_key_generation = nz_gen(row.get(12)?)?;
        let ed25519_signature = bytes_64(row.get(13)?)?;
        out.insert(
            lease_id,
            StoredSignedCloseReceiptRow {
                lease_id,
                receipt_id,
                holder_cell,
                holder_boot_generation,
                epoch,
                fencing_token,
                face_value,
                final_high_water_spent,
                returned_unspent,
                receipt_digest,
                signer_principal_id,
                signer_key_id,
                signer_key_generation,
                ed25519_signature,
            },
        );
    }
    Ok(out)
}

fn load_idempotency_rows(
    tx: &Transaction<'_>,
) -> Result<BTreeMap<IdempotencyKey, StoredIdempotencyRow>, LeaseStoreError> {
    let mut stmt = tx.prepare(
        "SELECT idempotency_key, caller_cell, command_digest, durable_state_seq, response_bytes
         FROM cross_host_proto_idempotency",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let idempotency_key = IdempotencyKey::from_bytes(bytes_16(row.get(0)?)?);
        let caller_cell = cell_from_blob(row.get(1)?)?;
        let command_digest = bytes_32(row.get(2)?)?;
        let durable_state_seq = to_u64(row.get(3)?)?;
        let response_bytes: Vec<u8> = row.get(4)?;
        out.insert(
            idempotency_key,
            StoredIdempotencyRow {
                idempotency_key,
                caller_cell,
                command_digest,
                durable_state_seq,
                response_bytes,
            },
        );
    }
    Ok(out)
}

fn encode_gateways(gateways: &[[u8; 16]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(gateways.len() * 16);
    for gw in gateways {
        out.extend_from_slice(gw);
    }
    out
}

fn decode_gateways(blob: &[u8]) -> Result<Vec<[u8; 16]>, LeaseStoreError> {
    if blob.is_empty() || !blob.len().is_multiple_of(16) {
        return Err(LeaseStoreError::CorruptRecord(
            "invalid multi_endpoint_scope_blob length",
        ));
    }
    let mut out = Vec::with_capacity(blob.len() / 16);
    for &arr in blob.as_chunks::<16>().0 {
        if arr == [0u8; 16] {
            return Err(LeaseStoreError::CorruptRecord(
                "zero gateway_id in multi_endpoint_scope_blob",
            ));
        }
        out.push(arr);
    }
    Ok(out)
}

fn to_i64(value: u64) -> Result<i64, LeaseStoreError> {
    i64::try_from(value).map_err(|_| LeaseStoreError::CorruptRecord("u64 exceeds i64"))
}

fn to_u64(value: i64) -> Result<u64, LeaseStoreError> {
    u64::try_from(value).map_err(|_| LeaseStoreError::CorruptRecord("negative SQLite integer"))
}

fn nz_u64(value: i64) -> Result<NonZeroU64, LeaseStoreError> {
    NonZeroU64::new(to_u64(value)?).ok_or(LeaseStoreError::CorruptRecord("zero NonZeroU64"))
}

fn nz_gen(value: i64) -> Result<Generation, LeaseStoreError> {
    nz_u64(value).map(Generation::new)
}

fn bytes_16(raw: Vec<u8>) -> Result<[u8; 16], LeaseStoreError> {
    raw.try_into()
        .map_err(|_| LeaseStoreError::CorruptRecord("expected 16-byte blob"))
}

fn bytes_32(raw: Vec<u8>) -> Result<[u8; 32], LeaseStoreError> {
    raw.try_into()
        .map_err(|_| LeaseStoreError::CorruptRecord("expected 32-byte blob"))
}

fn bytes_64(raw: Vec<u8>) -> Result<[u8; 64], LeaseStoreError> {
    raw.try_into()
        .map_err(|_| LeaseStoreError::CorruptRecord("expected 64-byte signature blob"))
}

fn cell_from_blob(raw: Vec<u8>) -> Result<CellIdentity, LeaseStoreError> {
    let arr = bytes_16(raw)?;
    if arr == [0u8; 16] {
        return Err(LeaseStoreError::CorruptRecord(
            "zero CellIdentity in SQLite",
        ));
    }
    Ok(CellIdentity::from_domain(SchedulerDomainId::from_bytes(
        arr,
    )))
}
