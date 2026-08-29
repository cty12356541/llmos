use rusqlite::{Connection, TransactionBehavior};

use crate::WaitAuthorityError;

pub(crate) const SCHEMA_VERSION: i64 = 1;

/// Creates the durable wait registry schema v1: the wait state machine rows
/// (one per registered `(binding, channel, target sequence)` registration),
/// the per-channel notify receipts that make [`crate::WaitAuthority::
/// notify_commits`] replayable, and the per-wait cancellation receipts.
///
/// Wait rows live in this authority's own database; the Channel itself lives
/// in the Channel authority's database, so the `channel_id` binding is
/// enforced by owner readback ([`nlos_channel::ChannelAuthority::
/// inspect_channel`]) at every write instead of by a foreign key.
///
/// Mutation discipline mirrors the Channel queue entries: the only legal
/// `UPDATE` of a wait row is a state-machine flip out of `PENDING`
/// (`PENDING -> WOKEN` with its wake fields set, `PENDING -> CANCELLED` with
/// its cancellation timestamp set) and every `DELETE` aborts.  The
/// registration identity fields (`wait_id`, `binding_id`, registration key
/// and timestamp, binding digest) are additionally frozen against any
/// update; the remaining bound fields (`channel_id`, snapshot generation and
/// fence, `target_sequence`) are guarded by the stored `binding_digest`,
/// which is re-derived on every read so tampering surfaces as
/// [`WaitAuthorityError::CorruptRecord`] instead of a silently accepted
/// drift.
#[allow(clippy::too_many_lines)] // One atomic STRICT schema batch with its guards.
pub(crate) fn migrate_v1(connection: &mut Connection) -> Result<(), WaitAuthorityError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name IN (
            'waits', 'channel_notifies', 'wait_cancellations'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'waits_state_transition',
            'waits_no_delete',
            'channel_notifies_immutable_update',
            'channel_notifies_no_delete',
            'wait_cancellations_immutable_update',
            'wait_cancellations_no_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_count == 3 && trigger_count == 6 {
        connection.pragma_update(None, "user_version", 1)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(WaitAuthorityError::CorruptRecord(
            "partial wait authority schema",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE waits (
            wait_id BLOB PRIMARY KEY NOT NULL CHECK(length(wait_id)=16),
            binding_id BLOB NOT NULL
                CHECK(length(binding_id)=16
                      AND binding_id != x'00000000000000000000000000000000'),
            channel_id BLOB NOT NULL CHECK(length(channel_id)=16),
            channel_generation INTEGER NOT NULL CHECK(channel_generation >= 1),
            channel_fencing_token BLOB NOT NULL CHECK(length(channel_fencing_token)=32),
            target_sequence INTEGER NOT NULL CHECK(target_sequence >= 1),
            binding_digest BLOB NOT NULL CHECK(length(binding_digest)=32),
            status INTEGER NOT NULL CHECK(status IN (0, 1, 2)),
            register_idempotency_key BLOB NOT NULL UNIQUE
                CHECK(length(register_idempotency_key)=16),
            registered_at_ms INTEGER NOT NULL CHECK(registered_at_ms >= 0),
            woken_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(woken_at_ms >= 0),
            woken_up_to_sequence INTEGER NOT NULL DEFAULT 0
                CHECK(woken_up_to_sequence >= 0),
            cancelled_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(cancelled_at_ms >= 0)
        ) STRICT;

        CREATE TABLE channel_notifies (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key)=16),
            channel_id BLOB NOT NULL CHECK(length(channel_id)=16),
            up_to_sequence INTEGER NOT NULL CHECK(up_to_sequence >= 1),
            notified_at_ms INTEGER NOT NULL CHECK(notified_at_ms >= 1),
            woken_wait_ids BLOB NOT NULL CHECK(length(woken_wait_ids) % 16 = 0)
        ) STRICT;

        CREATE TABLE wait_cancellations (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key)=16),
            wait_id BLOB NOT NULL CHECK(length(wait_id)=16),
            cancelled_at_ms INTEGER NOT NULL CHECK(cancelled_at_ms >= 1),
            FOREIGN KEY(wait_id) REFERENCES waits(wait_id)
        ) STRICT;

        CREATE TRIGGER waits_state_transition
        BEFORE UPDATE ON waits
        WHEN NEW.wait_id != OLD.wait_id
            OR NEW.binding_id != OLD.binding_id
            OR NEW.register_idempotency_key != OLD.register_idempotency_key
            OR NEW.registered_at_ms != OLD.registered_at_ms
            OR NEW.binding_digest != OLD.binding_digest
            OR (OLD.status != 0 AND NEW.status != OLD.status)
            OR (OLD.status = 0 AND NEW.status = 1
                AND (NEW.woken_at_ms < 1 OR NEW.woken_up_to_sequence < 1
                     OR NEW.cancelled_at_ms != 0))
            OR (OLD.status = 0 AND NEW.status = 2
                AND (NEW.cancelled_at_ms < 1 OR NEW.woken_at_ms != 0
                     OR NEW.woken_up_to_sequence != 0))
            OR NEW.status NOT IN (0, 1, 2)
        BEGIN
            SELECT RAISE(
                ABORT,
                'wait row is immutable beyond the pending wake or cancel transition'
            );
        END;
        CREATE TRIGGER waits_no_delete
        BEFORE DELETE ON waits BEGIN
            SELECT RAISE(ABORT, 'wait row is durable');
        END;
        CREATE TRIGGER channel_notifies_immutable_update
        BEFORE UPDATE ON channel_notifies BEGIN
            SELECT RAISE(ABORT, 'channel notify record is immutable');
        END;
        CREATE TRIGGER channel_notifies_no_delete
        BEFORE DELETE ON channel_notifies BEGIN
            SELECT RAISE(ABORT, 'channel notify record is durable');
        END;
        CREATE TRIGGER wait_cancellations_immutable_update
        BEFORE UPDATE ON wait_cancellations BEGIN
            SELECT RAISE(ABORT, 'wait cancellation record is immutable');
        END;
        CREATE TRIGGER wait_cancellations_no_delete
        BEFORE DELETE ON wait_cancellations BEGIN
            SELECT RAISE(ABORT, 'wait cancellation record is durable');
        END;

        PRAGMA user_version=1;",
    )?;
    transaction.commit()?;
    Ok(())
}
