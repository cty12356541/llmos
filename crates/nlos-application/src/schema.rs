use rusqlite::{Connection, TransactionBehavior};

use crate::ApplicationAuthorityError;

pub(crate) const SCHEMA_VERSION: i64 = 8;

/// Creates the durable application/installation authority schema v1: the
/// per-package `applications` singleton (current installation generation +
/// lifecycle status, §23.1 minimal subset) and the immutable
/// `installation_receipts` that make [`crate::ApplicationAuthority::
/// install_application`] replayable.
///
/// Design notes (mirroring the clock watermark and the channel generation
/// precedents):
///
/// - The application row is the *current-state* carrier: one row per package
///   identity (`package_id` UNIQUE), its current installation generation
///   monotonic under a read-then-write CAS, and the §23.1 minimal status
///   (`installed` = 1, `disabled` = 2). DDL guards abort any decreasing
///   generation, any identity rebinding, any delete, and any status
///   transition outside the minimal legal set (`installed → installed`
///   generation advance; `installed → disabled`; `disabled` is terminal in
///   this slice — re-enabling belongs to the update/uninstall policy engine
///   that is explicitly out of scope).
/// - The installation receipt is the *fact* carrier: immutable, durable, one
///   row per idempotency key, unique per `(application, generation)`, and an
///   AFTER INSERT guard ties every receipt to the application's *current*
///   generation — a receipt and the generation advance it proves commit in
///   one transaction and live and die together, exactly like the clock's
///   watermark-bounded tick receipts.
#[allow(clippy::too_many_lines)] // One auditable transaction contains the complete v1 DDL.
pub(crate) fn migrate_v1(connection: &mut Connection) -> Result<(), ApplicationAuthorityError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name IN ('applications', 'installation_receipts')",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'applications_monotonic_generation',
            'applications_frozen_identity',
            'applications_legal_status_transition',
            'applications_no_delete',
            'installation_receipts_immutable_update',
            'installation_receipts_no_delete',
            'installation_receipts_generation_bounds'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_count == 2 && trigger_count == 7 {
        connection.pragma_update(None, "user_version", 1)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(ApplicationAuthorityError::CorruptRecord(
            "partial application authority schema",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE applications (
            application_id BLOB PRIMARY KEY NOT NULL CHECK(length(application_id)=16),
            package_id BLOB NOT NULL UNIQUE CHECK(length(package_id)=16),
            package_manifest_digest BLOB NOT NULL CHECK(length(package_manifest_digest)=32),
            current_installation_generation INTEGER NOT NULL
                CHECK(current_installation_generation >= 1),
            status INTEGER NOT NULL CHECK(status IN (1, 2)),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
        ) STRICT;

        CREATE TABLE installation_receipts (
            installation_id BLOB PRIMARY KEY NOT NULL CHECK(length(installation_id)=16),
            idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key)=16),
            application_id BLOB NOT NULL CHECK(length(application_id)=16),
            installation_generation INTEGER NOT NULL CHECK(installation_generation >= 1),
            package_id BLOB NOT NULL CHECK(length(package_id)=16),
            package_manifest_digest BLOB NOT NULL CHECK(length(package_manifest_digest)=32),
            package_version INTEGER NOT NULL CHECK(package_version >= 0),
            entry_count INTEGER NOT NULL CHECK(entry_count >= 1),
            package_verification_receipt_id BLOB NOT NULL
                CHECK(length(package_verification_receipt_id)=16),
            installer_principal BLOB NOT NULL CHECK(length(installer_principal)=16),
            installed_at_ms INTEGER NOT NULL CHECK(installed_at_ms >= 0),
            UNIQUE(application_id, installation_generation),
            FOREIGN KEY(application_id) REFERENCES applications(application_id)
        ) STRICT;

        CREATE TRIGGER applications_monotonic_generation
        BEFORE UPDATE ON applications
        WHEN NEW.current_installation_generation < OLD.current_installation_generation
        BEGIN
            SELECT RAISE(ABORT, 'application installation generation is monotonic');
        END;
        CREATE TRIGGER applications_frozen_identity
        BEFORE UPDATE ON applications
        WHEN NEW.application_id != OLD.application_id OR NEW.package_id != OLD.package_id
        BEGIN
            SELECT RAISE(ABORT, 'application identity is frozen');
        END;
        CREATE TRIGGER applications_legal_status_transition
        BEFORE UPDATE ON applications
        WHEN OLD.status = 2
            OR NEW.status NOT IN (1, 2)
            OR (NEW.status = 2
                AND NEW.current_installation_generation
                    != OLD.current_installation_generation)
        BEGIN
            SELECT RAISE(ABORT, 'application status transition is not legal');
        END;
        CREATE TRIGGER applications_no_delete
        BEFORE DELETE ON applications BEGIN
            SELECT RAISE(ABORT, 'application row is durable (uninstall is out of scope)');
        END;
        CREATE TRIGGER installation_receipts_immutable_update
        BEFORE UPDATE ON installation_receipts BEGIN
            SELECT RAISE(ABORT, 'installation receipt is immutable');
        END;
        CREATE TRIGGER installation_receipts_no_delete
        BEFORE DELETE ON installation_receipts BEGIN
            SELECT RAISE(ABORT, 'installation receipt is durable');
        END;
        CREATE TRIGGER installation_receipts_generation_bounds
        AFTER INSERT ON installation_receipts
        WHEN NEW.installation_generation != (
            SELECT current_installation_generation FROM applications
            WHERE application_id = NEW.application_id
        )
        BEGIN
            SELECT RAISE(ABORT, 'installation receipt exceeds the application generation');
        END;

        PRAGMA user_version=1;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Adds schema v2: the immutable `application_disable_receipts` table that
/// makes [`crate::ApplicationAuthority::disable_application`] replayable
/// (the staged-migration precedent of the artifact authority: one function
/// and one `user_version` per migration, applied forward).
///
/// - The disable receipt is the *fact* carrier of the one
///   `installed → disabled` transition: immutable, durable, at most one
///   per application (`application_id` PRIMARY KEY — the status is
///   terminal, so the DDL itself encodes the terminality), one row per
///   idempotency key (`UNIQUE`). No synthetic id is derived: the fact is
///   uniquely addressed by the application identity it disabled, and the
///   receipt records the generation at disable time (unchanged by the
///   transition — the state-machine trigger already forbids moving it).
/// - The AFTER INSERT state-bounds guard ties every disable receipt to an
///   application that is *already disabled at its current generation* —
///   the status CAS and the receipt commit in one transaction and live
///   and die together, mirroring `installation_receipts_generation_bounds`.
pub(crate) fn migrate_v2(connection: &mut Connection) -> Result<(), ApplicationAuthorityError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name = 'application_disable_receipts'",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'application_disable_receipts_immutable_update',
            'application_disable_receipts_no_delete',
            'application_disable_receipts_state_bounds'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_count == 1 && trigger_count == 3 {
        connection.pragma_update(None, "user_version", 2)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(ApplicationAuthorityError::CorruptRecord(
            "partial application disable receipt schema",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE application_disable_receipts (
            application_id BLOB PRIMARY KEY NOT NULL CHECK(length(application_id)=16),
            idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key)=16),
            application_generation INTEGER NOT NULL CHECK(application_generation >= 1),
            disabled_at_ms INTEGER NOT NULL CHECK(disabled_at_ms >= 0),
            FOREIGN KEY(application_id) REFERENCES applications(application_id)
        ) STRICT;

        CREATE TRIGGER application_disable_receipts_immutable_update
        BEFORE UPDATE ON application_disable_receipts BEGIN
            SELECT RAISE(ABORT, 'application disable receipt is immutable');
        END;
        CREATE TRIGGER application_disable_receipts_no_delete
        BEFORE DELETE ON application_disable_receipts BEGIN
            SELECT RAISE(ABORT, 'application disable receipt is durable');
        END;
        CREATE TRIGGER application_disable_receipts_state_bounds
        AFTER INSERT ON application_disable_receipts
        WHEN (SELECT status FROM applications
              WHERE application_id = NEW.application_id) != 2
            OR NEW.application_generation != (
                SELECT current_installation_generation FROM applications
                WHERE application_id = NEW.application_id
            )
        BEGIN
            SELECT RAISE(ABORT, 'application disable receipt requires the disabled application at its current generation');
        END;

        PRAGMA user_version=2;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Adds schema v3: the immutable `application_uninstall_receipts` table,
/// extends application status with `uninstalled` (= 3), and relaxes the
/// status-machine trigger so `disabled → uninstalled` is legal (generation
/// unchanged on both disable and uninstall transitions).
///
/// - The uninstall receipt is the *fact* carrier of the terminal
///   `installed|disabled → uninstalled` transition: immutable, durable, at
///   most one per application (`application_id` PRIMARY KEY), one row per
///   idempotency key (`UNIQUE`). The receipt records the generation at
///   uninstall time (unchanged by the transition).
/// - The AFTER INSERT state-bounds guard ties every uninstall receipt to an
///   application that is *already uninstalled at its current generation*.
/// - The `applications` table is rebuilt so the `status` `CHECK` accepts
///   `(1, 2, 3)`; foreign keys are briefly disabled during the rebuild
///   (child receipt tables reference `applications`).
#[allow(clippy::too_many_lines)]
pub(crate) fn migrate_v3(connection: &mut Connection) -> Result<(), ApplicationAuthorityError> {
    let applications_sql: String = connection.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='applications'",
        [],
        |row| row.get(0),
    )?;
    let status_allows_uninstalled = applications_sql.contains("1, 2, 3");
    let uninstall_table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name = 'application_uninstall_receipts'",
        [],
        |row| row.get(0),
    )?;
    let uninstall_trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'application_uninstall_receipts_immutable_update',
            'application_uninstall_receipts_no_delete',
            'application_uninstall_receipts_state_bounds'
         )",
        [],
        |row| row.get(0),
    )?;
    if status_allows_uninstalled && uninstall_table_count == 1 && uninstall_trigger_count == 3 {
        connection.pragma_update(None, "user_version", 3)?;
        return Ok(());
    }
    if uninstall_table_count != 0 || uninstall_trigger_count != 0 {
        return Err(ApplicationAuthorityError::CorruptRecord(
            "partial application uninstall receipt schema",
        ));
    }

    connection.pragma_update(None, "foreign_keys", "OFF")?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "DROP TRIGGER IF EXISTS applications_monotonic_generation;
        DROP TRIGGER IF EXISTS applications_frozen_identity;
        DROP TRIGGER IF EXISTS applications_legal_status_transition;
        DROP TRIGGER IF EXISTS applications_no_delete;
        DROP TRIGGER IF EXISTS installation_receipts_generation_bounds;
        DROP TRIGGER IF EXISTS application_disable_receipts_state_bounds;

        CREATE TABLE applications_v3 (
            application_id BLOB PRIMARY KEY NOT NULL CHECK(length(application_id)=16),
            package_id BLOB NOT NULL UNIQUE CHECK(length(package_id)=16),
            package_manifest_digest BLOB NOT NULL CHECK(length(package_manifest_digest)=32),
            current_installation_generation INTEGER NOT NULL
                CHECK(current_installation_generation >= 1),
            status INTEGER NOT NULL CHECK(status IN (1, 2, 3)),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
        ) STRICT;

        INSERT INTO applications_v3
            SELECT application_id, package_id, package_manifest_digest,
                   current_installation_generation, status,
                   created_at_ms, updated_at_ms
            FROM applications;
        DROP TABLE applications;
        ALTER TABLE applications_v3 RENAME TO applications;

        CREATE TRIGGER applications_monotonic_generation
        BEFORE UPDATE ON applications
        WHEN NEW.current_installation_generation < OLD.current_installation_generation
        BEGIN
            SELECT RAISE(ABORT, 'application installation generation is monotonic');
        END;
        CREATE TRIGGER applications_frozen_identity
        BEFORE UPDATE ON applications
        WHEN NEW.application_id != OLD.application_id OR NEW.package_id != OLD.package_id
        BEGIN
            SELECT RAISE(ABORT, 'application identity is frozen');
        END;
        CREATE TRIGGER applications_legal_status_transition
        BEFORE UPDATE ON applications
        WHEN OLD.status = 3
            OR (OLD.status = 2 AND NEW.status != 3)
            OR NEW.status NOT IN (1, 2, 3)
            OR (NEW.status = 2
                AND NEW.current_installation_generation
                    != OLD.current_installation_generation)
            OR (NEW.status = 3
                AND NEW.current_installation_generation
                    != OLD.current_installation_generation)
        BEGIN
            SELECT RAISE(ABORT, 'application status transition is not legal');
        END;
        CREATE TRIGGER applications_no_delete
        BEFORE DELETE ON applications BEGIN
            SELECT RAISE(ABORT, 'application row is durable (physical delete is out of scope)');
        END;

        CREATE TRIGGER installation_receipts_generation_bounds
        AFTER INSERT ON installation_receipts
        WHEN NEW.installation_generation != (
            SELECT current_installation_generation FROM applications
            WHERE application_id = NEW.application_id
        )
        BEGIN
            SELECT RAISE(ABORT, 'installation receipt exceeds the application generation');
        END;
        CREATE TRIGGER application_disable_receipts_state_bounds
        AFTER INSERT ON application_disable_receipts
        WHEN (SELECT status FROM applications
              WHERE application_id = NEW.application_id) != 2
            OR NEW.application_generation != (
                SELECT current_installation_generation FROM applications
                WHERE application_id = NEW.application_id
            )
        BEGIN
            SELECT RAISE(ABORT, 'application disable receipt requires the disabled application at its current generation');
        END;

        CREATE TABLE application_uninstall_receipts (
            application_id BLOB PRIMARY KEY NOT NULL CHECK(length(application_id)=16),
            idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key)=16),
            application_generation INTEGER NOT NULL CHECK(application_generation >= 1),
            uninstalled_at_ms INTEGER NOT NULL CHECK(uninstalled_at_ms >= 0),
            FOREIGN KEY(application_id) REFERENCES applications(application_id)
        ) STRICT;

        CREATE TRIGGER application_uninstall_receipts_immutable_update
        BEFORE UPDATE ON application_uninstall_receipts BEGIN
            SELECT RAISE(ABORT, 'application uninstall receipt is immutable');
        END;
        CREATE TRIGGER application_uninstall_receipts_no_delete
        BEFORE DELETE ON application_uninstall_receipts BEGIN
            SELECT RAISE(ABORT, 'application uninstall receipt is durable');
        END;
        CREATE TRIGGER application_uninstall_receipts_state_bounds
        AFTER INSERT ON application_uninstall_receipts
        WHEN (SELECT status FROM applications
              WHERE application_id = NEW.application_id) != 3
            OR NEW.application_generation != (
                SELECT current_installation_generation FROM applications
                WHERE application_id = NEW.application_id
            )
        BEGIN
            SELECT RAISE(ABORT, 'application uninstall receipt requires the uninstalled application at its current generation');
        END;

        PRAGMA user_version=3;",
    )?;
    transaction.commit()?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// Adds schema v4: the immutable `application_rollback_receipts` table and
/// relaxes the generation/status triggers so `disabled|uninstalled →
/// installed` with a one-step generation decrease is legal (the rollback
/// prefix — not the full `[PKG-UPDATE-001]` policy engine).
///
/// - The rollback receipt is the *fact* carrier of one generation step
///   back to the previous durable installation: immutable, durable, one
///   row per idempotency key, many rows per application over time.
/// - The AFTER INSERT state-bounds guard ties every rollback receipt to an
///   application that is *already installed at the target generation*.
/// - Generation decrease is permitted only when status moves from
///   `disabled` or `uninstalled` to `installed` and the generation drops
///   by exactly one.
#[allow(clippy::too_many_lines)]
pub(crate) fn migrate_v4(connection: &mut Connection) -> Result<(), ApplicationAuthorityError> {
    let rollback_table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name = 'application_rollback_receipts'",
        [],
        |row| row.get(0),
    )?;
    let rollback_trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'application_rollback_receipts_immutable_update',
            'application_rollback_receipts_no_delete',
            'application_rollback_receipts_state_bounds'
         )",
        [],
        |row| row.get(0),
    )?;
    let monotonic_allows_rollback: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='trigger' AND name='applications_monotonic_generation'
           AND sql LIKE '%OLD.current_installation_generation - 1%'",
        [],
        |row| row.get(0),
    )?;
    if rollback_table_count == 1 && rollback_trigger_count == 3 && monotonic_allows_rollback == 1 {
        connection.pragma_update(None, "user_version", 4)?;
        return Ok(());
    }
    if rollback_table_count != 0 || rollback_trigger_count != 0 {
        return Err(ApplicationAuthorityError::CorruptRecord(
            "partial application rollback receipt schema",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "DROP TRIGGER IF EXISTS applications_monotonic_generation;
        DROP TRIGGER IF EXISTS applications_legal_status_transition;
        DROP TRIGGER IF EXISTS application_disable_receipts_state_bounds;
        DROP TRIGGER IF EXISTS application_uninstall_receipts_state_bounds;

        CREATE TRIGGER applications_monotonic_generation
        BEFORE UPDATE ON applications
        WHEN NEW.current_installation_generation < OLD.current_installation_generation
            AND NOT (
                NEW.status = 1
                AND OLD.status IN (2, 3)
                AND NEW.current_installation_generation
                    = OLD.current_installation_generation - 1
            )
        BEGIN
            SELECT RAISE(ABORT, 'application installation generation is monotonic');
        END;
        CREATE TRIGGER applications_legal_status_transition
        BEFORE UPDATE ON applications
        WHEN (OLD.status = 3
                AND NOT (
                    NEW.status = 1
                    AND NEW.current_installation_generation
                        = OLD.current_installation_generation - 1
                ))
            OR (OLD.status = 2
                AND NEW.status NOT IN (1, 3))
            OR (OLD.status = 2
                AND NEW.status = 3
                AND NEW.current_installation_generation
                    != OLD.current_installation_generation)
            OR (OLD.status = 2
                AND NEW.status = 1
                AND NEW.current_installation_generation
                    != OLD.current_installation_generation - 1)
            OR (OLD.status = 1
                AND NEW.status = 2
                AND NEW.current_installation_generation
                    != OLD.current_installation_generation)
            OR (OLD.status = 1
                AND NEW.status = 3
                AND NEW.current_installation_generation
                    != OLD.current_installation_generation)
            OR (OLD.status = 1
                AND NEW.status = 1
                AND NEW.current_installation_generation
                    <= OLD.current_installation_generation)
            OR NEW.status NOT IN (1, 2, 3)
        BEGIN
            SELECT RAISE(ABORT, 'application status transition is not legal');
        END;

        CREATE TRIGGER application_disable_receipts_state_bounds
        AFTER INSERT ON application_disable_receipts
        WHEN (SELECT status FROM applications
              WHERE application_id = NEW.application_id) != 2
            OR NEW.application_generation != (
                SELECT current_installation_generation FROM applications
                WHERE application_id = NEW.application_id
            )
        BEGIN
            SELECT RAISE(ABORT, 'application disable receipt requires the disabled application at its current generation');
        END;
        CREATE TRIGGER application_uninstall_receipts_state_bounds
        AFTER INSERT ON application_uninstall_receipts
        WHEN (SELECT status FROM applications
              WHERE application_id = NEW.application_id) != 3
            OR NEW.application_generation != (
                SELECT current_installation_generation FROM applications
                WHERE application_id = NEW.application_id
            )
        BEGIN
            SELECT RAISE(ABORT, 'application uninstall receipt requires the uninstalled application at its current generation');
        END;

        CREATE TABLE application_rollback_receipts (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key)=16),
            application_id BLOB NOT NULL CHECK(length(application_id)=16),
            from_generation INTEGER NOT NULL CHECK(from_generation >= 2),
            to_generation INTEGER NOT NULL
                CHECK(to_generation >= 1 AND from_generation = to_generation + 1),
            rollback_at_ms INTEGER NOT NULL CHECK(rollback_at_ms >= 0),
            FOREIGN KEY(application_id) REFERENCES applications(application_id)
        ) STRICT;

        CREATE TRIGGER application_rollback_receipts_immutable_update
        BEFORE UPDATE ON application_rollback_receipts BEGIN
            SELECT RAISE(ABORT, 'application rollback receipt is immutable');
        END;
        CREATE TRIGGER application_rollback_receipts_no_delete
        BEFORE DELETE ON application_rollback_receipts BEGIN
            SELECT RAISE(ABORT, 'application rollback receipt is durable');
        END;
        CREATE TRIGGER application_rollback_receipts_state_bounds
        AFTER INSERT ON application_rollback_receipts
        WHEN (SELECT status FROM applications
              WHERE application_id = NEW.application_id) != 1
            OR NEW.to_generation != (
                SELECT current_installation_generation FROM applications
                WHERE application_id = NEW.application_id
            )
        BEGIN
            SELECT RAISE(ABORT, 'application rollback receipt requires the installed application at its target generation');
        END;

        PRAGMA user_version=4;",
    )?;
    transaction.commit()?;
    Ok(())
}

pub(crate) fn migrate_v5(connection: &mut Connection) -> Result<(), ApplicationAuthorityError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name = 'application_background_task_registrations'",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'application_background_task_registrations_immutable_update',
            'application_background_task_registrations_no_delete',
            'application_background_task_registrations_state_bounds'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_count == 1 && trigger_count == 3 {
        connection.pragma_update(None, "user_version", 5)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(ApplicationAuthorityError::CorruptRecord(
            "partial application background task registration schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE application_background_task_registrations (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key)=16),
            application_id BLOB NOT NULL CHECK(length(application_id)=16),
            task_id BLOB NOT NULL CHECK(length(task_id)=16),
            registrant_principal BLOB NOT NULL CHECK(length(registrant_principal)=16),
            application_generation INTEGER NOT NULL CHECK(application_generation >= 1),
            registered_at_ms INTEGER NOT NULL CHECK(registered_at_ms >= 0),
            UNIQUE(application_id, task_id, application_generation),
            FOREIGN KEY(application_id) REFERENCES applications(application_id)
        ) STRICT;
        CREATE TRIGGER application_background_task_registrations_immutable_update
        BEFORE UPDATE ON application_background_task_registrations BEGIN
            SELECT RAISE(ABORT, 'application background task registration is immutable');
        END;
        CREATE TRIGGER application_background_task_registrations_no_delete
        BEFORE DELETE ON application_background_task_registrations BEGIN
            SELECT RAISE(ABORT, 'application background task registration is durable');
        END;
        CREATE TRIGGER application_background_task_registrations_state_bounds
        AFTER INSERT ON application_background_task_registrations
        WHEN (SELECT status FROM applications WHERE application_id = NEW.application_id) != 1
            OR NEW.application_generation != (
                SELECT current_installation_generation FROM applications
                WHERE application_id = NEW.application_id)
        BEGIN
            SELECT RAISE(ABORT, 'application background task registration requires the installed application at its current generation');
        END;
        PRAGMA user_version=5;",
    )?;
    transaction.commit()?;
    Ok(())
}

pub(crate) fn migrate_v6(connection: &mut Connection) -> Result<(), ApplicationAuthorityError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name = 'application_process_bindings'",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'application_process_bindings_immutable_update',
            'application_process_bindings_no_delete',
            'application_process_bindings_state_bounds'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_count == 1 && trigger_count == 3 {
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(ApplicationAuthorityError::CorruptRecord(
            "partial application process binding schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE application_process_bindings (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key)=16),
            application_id BLOB NOT NULL CHECK(length(application_id)=16),
            process_id BLOB NOT NULL CHECK(length(process_id)=16),
            registrant_principal BLOB NOT NULL CHECK(length(registrant_principal)=16),
            application_generation INTEGER NOT NULL CHECK(application_generation >= 1),
            registered_at_ms INTEGER NOT NULL CHECK(registered_at_ms >= 0),
            UNIQUE(application_id, process_id, application_generation),
            FOREIGN KEY(application_id) REFERENCES applications(application_id)
        ) STRICT;
        CREATE TRIGGER application_process_bindings_immutable_update
        BEFORE UPDATE ON application_process_bindings BEGIN
            SELECT RAISE(ABORT, 'application process binding is immutable');
        END;
        CREATE TRIGGER application_process_bindings_no_delete
        BEFORE DELETE ON application_process_bindings BEGIN
            SELECT RAISE(ABORT, 'application process binding is durable');
        END;
        CREATE TRIGGER application_process_bindings_state_bounds
        AFTER INSERT ON application_process_bindings
        WHEN (SELECT status FROM applications WHERE application_id = NEW.application_id) != 1
            OR NEW.application_generation != (
                SELECT current_installation_generation FROM applications
                WHERE application_id = NEW.application_id)
        BEGIN
            SELECT RAISE(ABORT, 'application process binding requires the installed application at its current generation');
        END;
        PRAGMA user_version=6;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Adds schema v7: the PKG migration-runner tables (W29-E, B1-3) —
/// `application_migrations` (the per-application staged-migration state
/// machine: pending/running/done/failed), `application_migration_steps`
/// (the durable per-step completion marks that make a drill resumable
/// across crashes), and `application_migration_rollback_receipts` (the
/// immutable fact of one failed-health PKG-level rollback, which is
/// deliberately distinct from the lifecycle
/// `application_rollback_receipts` of schema v4).
///
/// - The migration row is a *state-machine* row (like `applications`),
///   not an immutable receipt: `state`, `health_state`,
///   `health_checked_at_ms`, `activated_installation_id`, and
///   `updated_at_ms` are the only mutable columns; every binding column
///   (identities, baseline, target, declared step count, creation time)
///   is frozen by trigger, and the state lattice is
///   `pending → running → done|failed` with terminal `done`/`failed`.
/// - The health verdict is terminal once recorded (`unchecked →
///   passed|failed`, never re-recorded): the durable verdict is the
///   authority and replay never re-consults the probe. `done` requires a
///   passed verdict plus the activated installation; `failed` requires
///   the failed verdict.
/// - Step rows are immutable and strictly ordered: an insert is only
///   legal while the drill is live, at exactly `1 + completed` and
///   within the declared step count — a crash between steps converges by
///   replaying the recorded prefix and continuing at the next index.
/// - The partial unique index enforces one live drill per application;
///   terminal migrations accumulate as durable history.
/// - The rollback receipt's state-bounds guard ties it to a migration
///   that is *already failed at a failed verdict* — the state transition
///   and the receipt commit share one transaction and live and die
///   together.
#[allow(clippy::too_many_lines)]
pub(crate) fn migrate_v7(connection: &mut Connection) -> Result<(), ApplicationAuthorityError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name IN (
            'application_migrations',
            'application_migration_steps',
            'application_migration_rollback_receipts'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'application_migrations_frozen_binding',
            'application_migrations_legal_transition',
            'application_migrations_no_delete',
            'application_migration_steps_immutable_update',
            'application_migration_steps_no_delete',
            'application_migration_steps_live_ordered',
            'application_migration_rollback_receipts_immutable_update',
            'application_migration_rollback_receipts_no_delete',
            'application_migration_rollback_receipts_state_bounds'
         )",
        [],
        |row| row.get(0),
    )?;
    let index_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='index' AND name = 'application_migrations_one_live_per_application'",
        [],
        |row| row.get(0),
    )?;
    if table_count == 3 && trigger_count == 9 && index_count == 1 {
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 || index_count != 0 {
        return Err(ApplicationAuthorityError::CorruptRecord(
            "partial application migration schema",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE application_migrations (
            idempotency_key BLOB PRIMARY KEY NOT NULL
                CHECK(length(idempotency_key)=16),
            application_id BLOB NOT NULL CHECK(length(application_id)=16),
            package_id BLOB NOT NULL CHECK(length(package_id)=16),
            from_generation INTEGER NOT NULL CHECK(from_generation >= 1),
            from_manifest_digest BLOB NOT NULL
                CHECK(length(from_manifest_digest)=32),
            from_package_version INTEGER NOT NULL CHECK(from_package_version >= 0),
            package_verification_receipt_id BLOB NOT NULL
                CHECK(length(package_verification_receipt_id)=16),
            target_manifest_digest BLOB NOT NULL
                CHECK(length(target_manifest_digest)=32),
            target_package_version INTEGER NOT NULL CHECK(target_package_version >= 0),
            compatibility_window INTEGER NOT NULL CHECK(compatibility_window IN (1, 2)),
            step_count INTEGER NOT NULL CHECK(step_count >= 1),
            state INTEGER NOT NULL CHECK(state IN (1, 2, 3, 4)),
            health_state INTEGER NOT NULL CHECK(health_state IN (0, 1, 2)),
            health_checked_at_ms INTEGER,
            activated_installation_id BLOB
                CHECK(activated_installation_id IS NULL
                      OR length(activated_installation_id)=16),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
            FOREIGN KEY(application_id) REFERENCES applications(application_id),
            FOREIGN KEY(activated_installation_id)
                REFERENCES installation_receipts(installation_id)
        ) STRICT;

        CREATE TRIGGER application_migrations_frozen_binding
        BEFORE UPDATE ON application_migrations
        WHEN NEW.idempotency_key != OLD.idempotency_key
            OR NEW.application_id != OLD.application_id
            OR NEW.package_id != OLD.package_id
            OR NEW.from_generation != OLD.from_generation
            OR NEW.from_manifest_digest != OLD.from_manifest_digest
            OR NEW.from_package_version != OLD.from_package_version
            OR NEW.package_verification_receipt_id
                != OLD.package_verification_receipt_id
            OR NEW.target_manifest_digest != OLD.target_manifest_digest
            OR NEW.target_package_version != OLD.target_package_version
            OR NEW.compatibility_window != OLD.compatibility_window
            OR NEW.step_count != OLD.step_count
            OR NEW.created_at_ms != OLD.created_at_ms
        BEGIN
            SELECT RAISE(ABORT, 'application migration binding is frozen');
        END;
        CREATE TRIGGER application_migrations_legal_transition
        BEFORE UPDATE ON application_migrations
        WHEN (OLD.state = 1 AND NEW.state NOT IN (1, 2))
            OR (OLD.state = 2 AND NEW.state NOT IN (2, 3, 4))
            OR (OLD.state IN (3, 4) AND NEW.state != OLD.state)
            OR (NEW.state = 3
                AND (NEW.health_state != 1
                     OR NEW.activated_installation_id IS NULL))
            OR (NEW.state != 3 AND NEW.activated_installation_id IS NOT NULL)
            OR (NEW.state = 4 AND NEW.health_state != 2)
            OR (OLD.activated_installation_id IS NOT NULL
                AND NEW.activated_installation_id != OLD.activated_installation_id)
            OR (OLD.health_state = 0 AND NEW.health_state NOT IN (0, 1, 2))
            OR (OLD.health_state IN (1, 2)
                AND NEW.health_state != OLD.health_state)
            OR ((NEW.health_state = 0) != (NEW.health_checked_at_ms IS NULL))
            OR NEW.updated_at_ms < OLD.updated_at_ms
        BEGIN
            SELECT RAISE(ABORT, 'application migration transition is not legal');
        END;
        CREATE TRIGGER application_migrations_no_delete
        BEFORE DELETE ON application_migrations BEGIN
            SELECT RAISE(ABORT, 'application migration row is durable');
        END;

        CREATE UNIQUE INDEX application_migrations_one_live_per_application
        ON application_migrations(application_id) WHERE state IN (1, 2);

        CREATE TABLE application_migration_steps (
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key)=16),
            step_index INTEGER NOT NULL CHECK(step_index >= 1),
            completed_at_ms INTEGER NOT NULL CHECK(completed_at_ms >= 0),
            PRIMARY KEY(idempotency_key, step_index),
            FOREIGN KEY(idempotency_key)
                REFERENCES application_migrations(idempotency_key)
        ) STRICT;

        CREATE TRIGGER application_migration_steps_immutable_update
        BEFORE UPDATE ON application_migration_steps BEGIN
            SELECT RAISE(ABORT, 'application migration step is immutable');
        END;
        CREATE TRIGGER application_migration_steps_no_delete
        BEFORE DELETE ON application_migration_steps BEGIN
            SELECT RAISE(ABORT, 'application migration step is durable');
        END;
        CREATE TRIGGER application_migration_steps_live_ordered
        BEFORE INSERT ON application_migration_steps
        WHEN (SELECT state FROM application_migrations
              WHERE idempotency_key = NEW.idempotency_key) NOT IN (1, 2)
            OR NEW.step_index > (SELECT step_count FROM application_migrations
                                 WHERE idempotency_key = NEW.idempotency_key)
            OR NEW.step_index != 1 + (SELECT COUNT(*) FROM application_migration_steps
                                      WHERE idempotency_key = NEW.idempotency_key)
        BEGIN
            SELECT RAISE(ABORT, 'application migration step is out of the declared live drill order');
        END;

        CREATE TABLE application_migration_rollback_receipts (
            idempotency_key BLOB PRIMARY KEY NOT NULL
                CHECK(length(idempotency_key)=16),
            application_id BLOB NOT NULL CHECK(length(application_id)=16),
            abandoned_target_manifest_digest BLOB NOT NULL
                CHECK(length(abandoned_target_manifest_digest)=32),
            retained_generation INTEGER NOT NULL CHECK(retained_generation >= 1),
            retained_manifest_digest BLOB NOT NULL
                CHECK(length(retained_manifest_digest)=32),
            rolled_back_at_ms INTEGER NOT NULL CHECK(rolled_back_at_ms >= 0),
            FOREIGN KEY(idempotency_key)
                REFERENCES application_migrations(idempotency_key)
        ) STRICT;

        CREATE TRIGGER application_migration_rollback_receipts_immutable_update
        BEFORE UPDATE ON application_migration_rollback_receipts BEGIN
            SELECT RAISE(ABORT, 'application migration rollback receipt is immutable');
        END;
        CREATE TRIGGER application_migration_rollback_receipts_no_delete
        BEFORE DELETE ON application_migration_rollback_receipts BEGIN
            SELECT RAISE(ABORT, 'application migration rollback receipt is durable');
        END;
        CREATE TRIGGER application_migration_rollback_receipts_state_bounds
        AFTER INSERT ON application_migration_rollback_receipts
        WHEN (SELECT state FROM application_migrations
              WHERE idempotency_key = NEW.idempotency_key) != 4
            OR (SELECT health_state FROM application_migrations
                WHERE idempotency_key = NEW.idempotency_key) != 2
        BEGIN
            SELECT RAISE(ABORT, 'application migration rollback receipt requires the failed migration at a failed verdict');
        END;

        PRAGMA user_version=7;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Adds schema v8: the immutable `application_surface_registrations`
/// table that makes [`crate::ApplicationAuthority::register_surfaces`]
/// replayable (W32-F / B2-2, the UI-Surface dimension of ROAD-B-002).
///
/// - One row per declared surface of one registration call: the call's
///   idempotency key plus the declaration order (`surface_index`) is the
///   primary key, so one key addresses exactly one declared segment.
/// - The row is a *fact* carrier mirroring the v5 background-task and v6
///   process-binding registrations: immutable, durable, per-surface
///   identity unique within one installation generation
///   (`UNIQUE(application_id, surface_id, application_generation)`), and
///   bound to the exact installed package content by the manifest digest
///   that was current when the segment was registered.
/// - The AFTER INSERT state-bounds guard ties every registration row to
///   an application that is *installed at its current generation* — the
///   registration and the installation state it names commit in one
///   transaction and live and die together.
pub(crate) fn migrate_v8(connection: &mut Connection) -> Result<(), ApplicationAuthorityError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name = 'application_surface_registrations'",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'application_surface_registrations_immutable_update',
            'application_surface_registrations_no_delete',
            'application_surface_registrations_state_bounds'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_count == 1 && trigger_count == 3 {
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(ApplicationAuthorityError::CorruptRecord(
            "partial application surface registration schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE application_surface_registrations (
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key)=16),
            surface_index INTEGER NOT NULL CHECK(surface_index >= 0),
            application_id BLOB NOT NULL CHECK(length(application_id)=16),
            surface_id BLOB NOT NULL CHECK(length(surface_id)=16),
            surface_kind INTEGER NOT NULL CHECK(surface_kind IN (1, 2)),
            title TEXT NOT NULL
                CHECK(length(title) >= 1 AND length(title) <= 255
                      AND instr(title, char(0)) = 0),
            entry_name TEXT
                CHECK(entry_name IS NULL
                      OR (length(entry_name) >= 1 AND length(entry_name) <= 255
                          AND instr(entry_name, char(0)) = 0)),
            package_manifest_digest BLOB NOT NULL
                CHECK(length(package_manifest_digest)=32),
            registrant_principal BLOB NOT NULL CHECK(length(registrant_principal)=16),
            application_generation INTEGER NOT NULL CHECK(application_generation >= 1),
            registered_at_ms INTEGER NOT NULL CHECK(registered_at_ms >= 0),
            PRIMARY KEY(idempotency_key, surface_index),
            UNIQUE(application_id, surface_id, application_generation),
            FOREIGN KEY(application_id) REFERENCES applications(application_id)
        ) STRICT;
        CREATE TRIGGER application_surface_registrations_immutable_update
            BEFORE UPDATE ON application_surface_registrations BEGIN
            SELECT RAISE(ABORT, 'application surface registration is immutable');
        END;
        CREATE TRIGGER application_surface_registrations_no_delete
            BEFORE DELETE ON application_surface_registrations BEGIN
            SELECT RAISE(ABORT, 'application surface registration is durable');
        END;
        CREATE TRIGGER application_surface_registrations_state_bounds
            AFTER INSERT ON application_surface_registrations
            WHEN (SELECT status FROM applications WHERE application_id = NEW.application_id) != 1
                OR NEW.application_generation != (
                    SELECT current_installation_generation FROM applications
                    WHERE application_id = NEW.application_id)
                OR NEW.package_manifest_digest != (
                    SELECT package_manifest_digest FROM applications
                    WHERE application_id = NEW.application_id)
        BEGIN
            SELECT RAISE(ABORT, 'application surface registration requires the installed application at its current generation and manifest digest');
        END;
        PRAGMA user_version=8;",
    )?;
    transaction.commit()?;
    Ok(())
}
