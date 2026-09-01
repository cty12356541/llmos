use rusqlite::{Connection, TransactionBehavior};

use crate::ApplicationAuthorityError;

pub(crate) const SCHEMA_VERSION: i64 = 2;

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
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
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
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
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
