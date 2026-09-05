use rusqlite::{Connection, TransactionBehavior};

use crate::ProcessAuthorityError;

pub(crate) const SCHEMA_VERSION: i64 = 4;

pub(crate) fn migrate_v4(connection: &mut Connection) -> Result<(), ProcessAuthorityError> {
    let table: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name = 'fiber_incarnation_cancel_receipts'",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'fiber_incarnation_cancel_receipts_immutable_update',
            'fiber_incarnation_cancel_receipts_immutable_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    if table == 1 && trigger_count == 2 {
        connection.pragma_update(None, "user_version", 4)?;
        return Ok(());
    }
    if table != 0 || trigger_count != 0 {
        return Err(ProcessAuthorityError::CorruptRecord(
            "partial fiber incarnation cancel receipt schema",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE fiber_incarnation_cancel_receipts (
            process_id BLOB NOT NULL CHECK(length(process_id) = 16),
            process_generation INTEGER NOT NULL CHECK(process_generation >= 1),
            binding_id BLOB NOT NULL CHECK(length(binding_id) = 16),
            incarnation_generation INTEGER NOT NULL CHECK(incarnation_generation >= 1),
            incarnation_fencing_token BLOB NOT NULL
                CHECK(length(incarnation_fencing_token) = 32),
            lifecycle_state INTEGER NOT NULL CHECK(lifecycle_state IN (1, 2)),
            batch_idempotency_key BLOB NOT NULL CHECK(length(batch_idempotency_key) = 16),
            cancelled_at_ms INTEGER NOT NULL CHECK(cancelled_at_ms >= 0),
            PRIMARY KEY(process_id, process_generation, binding_id),
            FOREIGN KEY(process_id) REFERENCES process_heads(process_id)
        ) STRICT;

        CREATE INDEX fiber_incarnation_cancel_receipts_batch_key
            ON fiber_incarnation_cancel_receipts(batch_idempotency_key);

        CREATE TRIGGER fiber_incarnation_cancel_receipts_immutable_update
        BEFORE UPDATE ON fiber_incarnation_cancel_receipts BEGIN
            SELECT RAISE(ABORT, 'fiber incarnation cancel receipt is immutable');
        END;
        CREATE TRIGGER fiber_incarnation_cancel_receipts_immutable_delete
        BEFORE DELETE ON fiber_incarnation_cancel_receipts BEGIN
            SELECT RAISE(ABORT, 'fiber incarnation cancel receipt is immutable');
        END;

        PRAGMA user_version = 4;",
    )?;
    transaction.commit()?;
    Ok(())
}

pub(crate) fn migrate_v3(connection: &mut Connection) -> Result<(), ProcessAuthorityError> {
    let marker_table: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name = 'process_terminal_markers'",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'process_terminal_markers_immutable_update',
            'process_terminal_markers_immutable_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    let lifecycle_column: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('process_heads')
         WHERE name = 'lifecycle_state'",
        [],
        |row| row.get(0),
    )?;
    if marker_table == 1 && trigger_count == 2 && lifecycle_column == 1 {
        connection.pragma_update(None, "user_version", 3)?;
        return Ok(());
    }
    if marker_table != 0 || trigger_count != 0 || lifecycle_column != 0 {
        return Err(ProcessAuthorityError::CorruptRecord(
            "partial process terminal lifecycle schema",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "ALTER TABLE process_heads
         ADD COLUMN lifecycle_state INTEGER NOT NULL DEFAULT 0
             CHECK(lifecycle_state >= 0 AND lifecycle_state <= 2);

        CREATE TABLE process_terminal_markers (
            process_id BLOB NOT NULL CHECK(length(process_id) = 16),
            process_generation INTEGER NOT NULL CHECK(process_generation >= 1),
            process_fencing_token BLOB NOT NULL CHECK(length(process_fencing_token) = 32),
            lifecycle_state INTEGER NOT NULL CHECK(lifecycle_state IN (1, 2)),
            idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
            marked_at_ms INTEGER NOT NULL CHECK(marked_at_ms >= 0),
            PRIMARY KEY(process_id, process_generation),
            FOREIGN KEY(process_id) REFERENCES process_heads(process_id)
        ) STRICT;

        CREATE TRIGGER process_terminal_markers_immutable_update
        BEFORE UPDATE ON process_terminal_markers BEGIN
            SELECT RAISE(ABORT, 'process terminal marker is immutable');
        END;
        CREATE TRIGGER process_terminal_markers_immutable_delete
        BEFORE DELETE ON process_terminal_markers BEGIN
            SELECT RAISE(ABORT, 'process terminal marker is immutable');
        END;

        PRAGMA user_version = 3;",
    )?;
    transaction.commit()?;
    Ok(())
}

pub(crate) fn migrate_v2(connection: &mut Connection) -> Result<(), ProcessAuthorityError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name IN (
            'fiber_incarnations', 'fiber_incarnation_heads', 'fiber_entry_snapshots'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'fiber_incarnations_immutable_update',
            'fiber_incarnations_immutable_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_count == 3 && trigger_count == 2 {
        connection.pragma_update(None, "user_version", 2)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(ProcessAuthorityError::CorruptRecord(
            "partial fiber incarnation schema",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE fiber_incarnation_heads (
            process_id BLOB NOT NULL CHECK(length(process_id) = 16),
            binding_id BLOB NOT NULL CHECK(length(binding_id) = 16),
            current_incarnation INTEGER NOT NULL CHECK(current_incarnation >= 1),
            current_fencing_token BLOB NOT NULL CHECK(length(current_fencing_token) = 32),
            updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= 0),
            PRIMARY KEY(process_id, binding_id)
        ) STRICT;

        CREATE TABLE fiber_incarnations (
            process_id BLOB NOT NULL CHECK(length(process_id) = 16),
            binding_id BLOB NOT NULL CHECK(length(binding_id) = 16),
            incarnation_generation INTEGER NOT NULL CHECK(incarnation_generation >= 1),
            fencing_token BLOB NOT NULL CHECK(length(fencing_token) = 32),
            process_generation INTEGER NOT NULL CHECK(process_generation >= 1),
            process_fencing_token BLOB NOT NULL CHECK(length(process_fencing_token) = 32),
            prior_incarnation INTEGER
                CHECK(prior_incarnation IS NULL OR prior_incarnation >= 1),
            idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            PRIMARY KEY(process_id, binding_id, incarnation_generation),
            UNIQUE(process_id, binding_id, fencing_token),
            FOREIGN KEY(process_id) REFERENCES process_heads(process_id),
            CHECK((incarnation_generation = 1) = (prior_incarnation IS NULL)),
            CHECK(prior_incarnation IS NULL OR incarnation_generation = prior_incarnation + 1)
        ) STRICT;

        CREATE TRIGGER fiber_incarnations_immutable_update
        BEFORE UPDATE ON fiber_incarnations BEGIN
            SELECT RAISE(ABORT, 'fiber incarnation is immutable');
        END;
        CREATE TRIGGER fiber_incarnations_immutable_delete
        BEFORE DELETE ON fiber_incarnations BEGIN
            SELECT RAISE(ABORT, 'fiber incarnation is immutable');
        END;

        CREATE TABLE fiber_entry_snapshots (
            process_id BLOB NOT NULL CHECK(length(process_id) = 16),
            binding_id BLOB NOT NULL CHECK(length(binding_id) = 16),
            handler_input BLOB NOT NULL CHECK(length(handler_input) > 0),
            input_digest BLOB NOT NULL CHECK(length(input_digest) = 32),
            written_by_incarnation INTEGER NOT NULL CHECK(written_by_incarnation >= 1),
            written_at_ms INTEGER NOT NULL CHECK(written_at_ms >= 0),
            PRIMARY KEY(process_id, binding_id)
        ) STRICT;

        PRAGMA user_version = 2;",
    )?;
    transaction.commit()?;
    Ok(())
}

pub(crate) fn migrate_v1(connection: &mut Connection) -> Result<(), ProcessAuthorityError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE isolation_domains (
            isolation_domain_id BLOB PRIMARY KEY NOT NULL CHECK(length(isolation_domain_id) = 16),
            create_idempotency_key BLOB NOT NULL UNIQUE CHECK(length(create_idempotency_key) = 16),
            current_generation INTEGER NOT NULL CHECK(current_generation >= 1),
            current_fencing_token BLOB NOT NULL CHECK(length(current_fencing_token) = 32),
            policy_digest BLOB NOT NULL CHECK(length(policy_digest) = 32),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
        ) STRICT;

        CREATE TABLE isolation_domain_generations (
            isolation_domain_id BLOB NOT NULL CHECK(length(isolation_domain_id) = 16),
            generation INTEGER NOT NULL CHECK(generation >= 1),
            fencing_token BLOB NOT NULL CHECK(length(fencing_token) = 32),
            policy_digest BLOB NOT NULL CHECK(length(policy_digest) = 32),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            PRIMARY KEY(isolation_domain_id, generation),
            UNIQUE(isolation_domain_id, fencing_token),
            FOREIGN KEY(isolation_domain_id) REFERENCES isolation_domains(isolation_domain_id)
        ) STRICT;

        CREATE TABLE isolation_domain_rotations (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key) = 16),
            isolation_domain_id BLOB NOT NULL CHECK(length(isolation_domain_id) = 16),
            expected_generation INTEGER NOT NULL CHECK(expected_generation >= 1),
            expected_fencing_token BLOB NOT NULL CHECK(length(expected_fencing_token) = 32),
            resulting_generation INTEGER NOT NULL CHECK(resulting_generation = expected_generation + 1),
            resulting_fencing_token BLOB NOT NULL CHECK(length(resulting_fencing_token) = 32),
            rotated_at_ms INTEGER NOT NULL CHECK(rotated_at_ms >= 0),
            FOREIGN KEY(isolation_domain_id, resulting_generation)
                REFERENCES isolation_domain_generations(isolation_domain_id, generation)
        ) STRICT;

        CREATE TABLE process_heads (
            process_id BLOB PRIMARY KEY NOT NULL CHECK(length(process_id) = 16),
            current_generation INTEGER NOT NULL CHECK(current_generation >= 1),
            current_fencing_token BLOB NOT NULL CHECK(length(current_fencing_token) = 32),
            agent_instance_id BLOB NOT NULL CHECK(length(agent_instance_id) = 16),
            current_agent_generation INTEGER NOT NULL CHECK(current_agent_generation >= 1),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
        ) STRICT;

        CREATE TABLE process_bindings (
            process_id BLOB NOT NULL CHECK(length(process_id) = 16),
            process_generation INTEGER NOT NULL CHECK(process_generation >= 1),
            process_fencing_token BLOB NOT NULL CHECK(length(process_fencing_token) = 32),
            agent_instance_id BLOB NOT NULL CHECK(length(agent_instance_id) = 16),
            agent_instance_generation INTEGER NOT NULL CHECK(agent_instance_generation >= 1),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            task_attempt_id BLOB NOT NULL CHECK(length(task_attempt_id) = 16),
            attempt_generation INTEGER NOT NULL CHECK(attempt_generation >= 1),
            isolation_domain_id BLOB NOT NULL CHECK(length(isolation_domain_id) = 16),
            isolation_domain_generation INTEGER NOT NULL CHECK(isolation_domain_generation >= 1),
            isolation_domain_fencing_token BLOB NOT NULL CHECK(length(isolation_domain_fencing_token) = 32),
            prior_process_generation INTEGER CHECK(prior_process_generation IS NULL OR prior_process_generation >= 1),
            idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            PRIMARY KEY(process_id, process_generation),
            UNIQUE(process_id, process_fencing_token),
            FOREIGN KEY(process_id) REFERENCES process_heads(process_id),
            FOREIGN KEY(isolation_domain_id, isolation_domain_generation)
                REFERENCES isolation_domain_generations(isolation_domain_id, generation),
            CHECK((process_generation = 1) = (prior_process_generation IS NULL)),
            CHECK(prior_process_generation IS NULL OR process_generation = prior_process_generation + 1)
        ) STRICT;

        CREATE TRIGGER isolation_domain_generations_immutable_update
        BEFORE UPDATE ON isolation_domain_generations BEGIN
            SELECT RAISE(ABORT, 'isolation domain generation is immutable');
        END;
        CREATE TRIGGER isolation_domain_generations_immutable_delete
        BEFORE DELETE ON isolation_domain_generations BEGIN
            SELECT RAISE(ABORT, 'isolation domain generation is immutable');
        END;
        CREATE TRIGGER isolation_domain_rotations_immutable_update
        BEFORE UPDATE ON isolation_domain_rotations BEGIN
            SELECT RAISE(ABORT, 'isolation domain rotation is immutable');
        END;
        CREATE TRIGGER isolation_domain_rotations_immutable_delete
        BEFORE DELETE ON isolation_domain_rotations BEGIN
            SELECT RAISE(ABORT, 'isolation domain rotation is immutable');
        END;
        CREATE TRIGGER process_bindings_immutable_update
        BEFORE UPDATE ON process_bindings BEGIN
            SELECT RAISE(ABORT, 'process binding is immutable');
        END;
        CREATE TRIGGER process_bindings_immutable_delete
        BEFORE DELETE ON process_bindings BEGIN
            SELECT RAISE(ABORT, 'process binding is immutable');
        END;

        PRAGMA user_version = 1;",
    )?;
    transaction.commit()?;
    Ok(())
}
