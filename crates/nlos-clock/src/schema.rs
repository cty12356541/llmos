use rusqlite::{Connection, TransactionBehavior};

use crate::AuthorityClockError;

pub(crate) const SCHEMA_VERSION: i64 = 2;

/// Creates the durable clock authority schema v1: the single-row watermark
/// (the durable monotonic high-water, seeded with reading 0 — "no reading
/// issued yet") and the per-key tick receipts that make
/// [`crate::AuthorityClock::now`] replayable.
///
/// Design note (single-row watermark over an append-only tick log): the
/// watermark carries the monotonicity invariant in the row itself — a crash
/// can only leave it at the old or the new committed value (transaction
/// atomicity), and the DDL guards below abort any delete, re-insert,
/// singleton rebinding or decreasing update, so not even raw SQL can move
/// the reading backwards.  An append-only log would derive the same property
/// only through a max-scan over an unbounded table plus a predecessor guard
/// against lower inserts.
///
/// The tick receipt's reading is bounded by an AFTER INSERT trigger to the
/// current watermark (receipts never record readings beyond it), and
/// receipts are immutable and durable — the recorded reading of an executed
/// key can never be rewritten or lost while the store exists.
pub(crate) fn migrate_v1(connection: &mut Connection) -> Result<(), AuthorityClockError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name IN ('watermark', 'tick_receipts')",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'watermark_no_insert',
            'watermark_monotonic_update',
            'watermark_no_delete',
            'tick_receipts_immutable_update',
            'tick_receipts_no_delete',
            'tick_receipts_reading_bounds'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_count == 2 && trigger_count == 6 {
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(AuthorityClockError::CorruptRecord(
            "partial clock authority schema",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE watermark (
            singleton INTEGER PRIMARY KEY NOT NULL CHECK(singleton = 1),
            reading INTEGER NOT NULL CHECK(reading >= 0)
        ) STRICT;
        INSERT INTO watermark (singleton, reading) VALUES (1, 0);

        CREATE TABLE tick_receipts (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key)=16),
            reading INTEGER NOT NULL CHECK(reading >= 1)
        ) STRICT;

        CREATE TRIGGER watermark_no_insert
        BEFORE INSERT ON watermark BEGIN
            SELECT RAISE(ABORT, 'watermark is the single durable row');
        END;
        CREATE TRIGGER watermark_monotonic_update
        BEFORE UPDATE ON watermark
        WHEN NEW.singleton != OLD.singleton OR NEW.reading < OLD.reading
        BEGIN
            SELECT RAISE(ABORT, 'watermark reading is monotonic');
        END;
        CREATE TRIGGER watermark_no_delete
        BEFORE DELETE ON watermark BEGIN
            SELECT RAISE(ABORT, 'watermark row is durable');
        END;
        CREATE TRIGGER tick_receipts_immutable_update
        BEFORE UPDATE ON tick_receipts BEGIN
            SELECT RAISE(ABORT, 'tick receipt is immutable');
        END;
        CREATE TRIGGER tick_receipts_no_delete
        BEFORE DELETE ON tick_receipts BEGIN
            SELECT RAISE(ABORT, 'tick receipt is durable');
        END;
        CREATE TRIGGER tick_receipts_reading_bounds
        AFTER INSERT ON tick_receipts
        WHEN NEW.reading > (SELECT reading FROM watermark WHERE singleton = 1)
        BEGIN
            SELECT RAISE(ABORT, 'tick receipt exceeds the watermark');
        END;

        PRAGMA user_version=1;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Creates the durable wall-clock domain (schema v2, additive over v1): the
/// single-row wall watermark — seeded with `reading_ms` 0, "no wall reading
/// issued yet" — and the per-key wall receipts that make
/// [`crate::AuthorityClock::wall_now`] replayable.
///
/// Design note (separate wall tables instead of a `kind` column on the tick
/// tables): the single-row watermark carries its monotonicity invariant in
/// the row itself, so one row can hold exactly one domain — sharing the tick
/// watermark would either break `now`'s dense +1 semantics (a wall ms reading
/// of ~1.8e12 would vault the logical counter) or require a second watermark
/// row the no-insert guard forbids.  The tick receipts are declared
/// immutable (`BEFORE UPDATE` abort), so retro-fitting a `kind` column would
/// reinterpret already-durable rows after the fact; separate tables keep the
/// tick domain's durable surface byte-stable, and each domain's trigger set
/// stays self-contained.  Wall guards mirror the tick guards exactly, with
/// the receipt bound `reading_ms >= 1`: a system clock at exactly the epoch
/// would be indistinguishable from "no reading" and fails closed instead.
pub(crate) fn migrate_v2(connection: &mut Connection) -> Result<(), AuthorityClockError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name IN ('wall_watermark', 'wall_receipts')",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'wall_watermark_no_insert',
            'wall_watermark_monotonic_update',
            'wall_watermark_no_delete',
            'wall_receipts_immutable_update',
            'wall_receipts_no_delete',
            'wall_receipts_reading_bounds'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_count == 2 && trigger_count == 6 {
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(AuthorityClockError::CorruptRecord(
            "partial clock authority wall schema",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE wall_watermark (
            singleton INTEGER PRIMARY KEY NOT NULL CHECK(singleton = 1),
            reading_ms INTEGER NOT NULL CHECK(reading_ms >= 0)
        ) STRICT;
        INSERT INTO wall_watermark (singleton, reading_ms) VALUES (1, 0);

        CREATE TABLE wall_receipts (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key)=16),
            reading_ms INTEGER NOT NULL CHECK(reading_ms >= 1)
        ) STRICT;

        CREATE TRIGGER wall_watermark_no_insert
        BEFORE INSERT ON wall_watermark BEGIN
            SELECT RAISE(ABORT, 'wall watermark is the single durable row');
        END;
        CREATE TRIGGER wall_watermark_monotonic_update
        BEFORE UPDATE ON wall_watermark
        WHEN NEW.singleton != OLD.singleton OR NEW.reading_ms < OLD.reading_ms
        BEGIN
            SELECT RAISE(ABORT, 'wall watermark reading is monotonic');
        END;
        CREATE TRIGGER wall_watermark_no_delete
        BEFORE DELETE ON wall_watermark BEGIN
            SELECT RAISE(ABORT, 'wall watermark row is durable');
        END;
        CREATE TRIGGER wall_receipts_immutable_update
        BEFORE UPDATE ON wall_receipts BEGIN
            SELECT RAISE(ABORT, 'wall receipt is immutable');
        END;
        CREATE TRIGGER wall_receipts_no_delete
        BEFORE DELETE ON wall_receipts BEGIN
            SELECT RAISE(ABORT, 'wall receipt is durable');
        END;
        CREATE TRIGGER wall_receipts_reading_bounds
        AFTER INSERT ON wall_receipts
        WHEN NEW.reading_ms > (SELECT reading_ms FROM wall_watermark WHERE singleton = 1)
        BEGIN
            SELECT RAISE(ABORT, 'wall receipt exceeds the wall watermark');
        END;

        PRAGMA user_version=2;",
    )?;
    transaction.commit()?;
    Ok(())
}
