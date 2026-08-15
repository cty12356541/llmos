use rusqlite::{Connection, TransactionBehavior};

use crate::ResourceAuthorityError;

#[allow(clippy::too_many_lines)] // One auditable transaction contains the complete v1 DDL.
pub(crate) fn migrate_v1(connection: &mut Connection) -> Result<(), ResourceAuthorityError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE drivers (
            driver_id BLOB PRIMARY KEY NOT NULL CHECK(length(driver_id) = 16),
            device_id BLOB NOT NULL UNIQUE CHECK(length(device_id) = 16),
            idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
            current_generation INTEGER NOT NULL CHECK(current_generation >= 1),
            current_fencing_token BLOB NOT NULL CHECK(length(current_fencing_token) = 32),
            profile_digest BLOB NOT NULL CHECK(length(profile_digest) = 32),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
        ) STRICT;

        CREATE TABLE driver_generations (
            driver_id BLOB NOT NULL CHECK(length(driver_id) = 16),
            generation INTEGER NOT NULL CHECK(generation >= 1),
            fencing_token BLOB NOT NULL CHECK(length(fencing_token) = 32),
            profile_digest BLOB NOT NULL CHECK(length(profile_digest) = 32),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            PRIMARY KEY(driver_id, generation),
            UNIQUE(driver_id, fencing_token),
            FOREIGN KEY(driver_id) REFERENCES drivers(driver_id)
        ) STRICT;

        CREATE TABLE driver_rotations (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key) = 16),
            driver_id BLOB NOT NULL CHECK(length(driver_id) = 16),
            expected_generation INTEGER NOT NULL CHECK(expected_generation >= 1),
            expected_fencing_token BLOB NOT NULL CHECK(length(expected_fencing_token) = 32),
            resulting_generation INTEGER NOT NULL CHECK(resulting_generation = expected_generation + 1),
            resulting_fencing_token BLOB NOT NULL CHECK(length(resulting_fencing_token) = 32),
            rotated_at_ms INTEGER NOT NULL CHECK(rotated_at_ms >= 0),
            FOREIGN KEY(driver_id, resulting_generation) REFERENCES driver_generations(driver_id, generation)
        ) STRICT;

        CREATE TABLE resource_accounts (
            account_id BLOB PRIMARY KEY NOT NULL CHECK(length(account_id) = 16),
            idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
            initial_credit INTEGER NOT NULL CHECK(initial_credit >= 0),
            available_credit INTEGER NOT NULL CHECK(available_credit >= 0),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0)
        ) STRICT;

        CREATE TABLE quotes (
            quote_id BLOB PRIMARY KEY NOT NULL CHECK(length(quote_id) = 16),
            idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
            driver_id BLOB NOT NULL CHECK(length(driver_id) = 16),
            device_id BLOB NOT NULL CHECK(length(device_id) = 16),
            driver_generation INTEGER NOT NULL CHECK(driver_generation >= 1),
            driver_fencing_token BLOB NOT NULL CHECK(length(driver_fencing_token) = 32),
            operation_proposal_digest BLOB NOT NULL CHECK(length(operation_proposal_digest) = 32),
            pricing_version BLOB NOT NULL CHECK(length(pricing_version) = 32),
            upper_bound INTEGER NOT NULL CHECK(upper_bound > 0),
            valid_until_ms INTEGER NOT NULL CHECK(valid_until_ms >= 0),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            FOREIGN KEY(driver_id, driver_generation) REFERENCES driver_generations(driver_id, generation)
        ) STRICT;

        CREATE TABLE reservations (
            reservation_id BLOB PRIMARY KEY NOT NULL CHECK(length(reservation_id) = 16),
            idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
            account_id BLOB NOT NULL CHECK(length(account_id) = 16),
            quote_id BLOB NOT NULL CHECK(length(quote_id) = 16),
            call_id BLOB NOT NULL UNIQUE CHECK(length(call_id) = 16),
            operation_id BLOB NOT NULL UNIQUE CHECK(length(operation_id) = 16),
            driver_id BLOB NOT NULL CHECK(length(driver_id) = 16),
            device_id BLOB NOT NULL CHECK(length(device_id) = 16),
            driver_generation INTEGER NOT NULL CHECK(driver_generation >= 1),
            driver_fencing_token BLOB NOT NULL CHECK(length(driver_fencing_token) = 32),
            upper_bound INTEGER NOT NULL CHECK(upper_bound > 0),
            activation_token BLOB NOT NULL UNIQUE CHECK(length(activation_token) = 32),
            state INTEGER NOT NULL CHECK(state IN (0, 1)),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            activated_at_ms INTEGER CHECK(activated_at_ms IS NULL OR activated_at_ms >= created_at_ms),
            activation_receipt_id BLOB UNIQUE CHECK(activation_receipt_id IS NULL OR length(activation_receipt_id) = 16),
            FOREIGN KEY(account_id) REFERENCES resource_accounts(account_id),
            FOREIGN KEY(quote_id) REFERENCES quotes(quote_id),
            FOREIGN KEY(driver_id, driver_generation) REFERENCES driver_generations(driver_id, generation),
            CHECK((state = 0) = (activated_at_ms IS NULL)),
            CHECK((state = 0) = (activation_receipt_id IS NULL))
        ) STRICT;

        CREATE TABLE reservation_activation_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            reservation_id BLOB NOT NULL UNIQUE CHECK(length(reservation_id) = 16),
            call_id BLOB NOT NULL CHECK(length(call_id) = 16),
            operation_id BLOB NOT NULL CHECK(length(operation_id) = 16),
            driver_id BLOB NOT NULL CHECK(length(driver_id) = 16),
            driver_generation INTEGER NOT NULL CHECK(driver_generation >= 1),
            activation_token BLOB NOT NULL CHECK(length(activation_token) = 32),
            activated_at_ms INTEGER NOT NULL CHECK(activated_at_ms >= 0),
            FOREIGN KEY(reservation_id) REFERENCES reservations(reservation_id)
        ) STRICT;

        CREATE TRIGGER driver_generations_immutable_update BEFORE UPDATE ON driver_generations
        BEGIN SELECT RAISE(ABORT, 'driver generation is immutable'); END;
        CREATE TRIGGER driver_generations_immutable_delete BEFORE DELETE ON driver_generations
        BEGIN SELECT RAISE(ABORT, 'driver generation is immutable'); END;
        CREATE TRIGGER driver_rotations_immutable_update BEFORE UPDATE ON driver_rotations
        BEGIN SELECT RAISE(ABORT, 'driver rotation is immutable'); END;
        CREATE TRIGGER driver_rotations_immutable_delete BEFORE DELETE ON driver_rotations
        BEGIN SELECT RAISE(ABORT, 'driver rotation is immutable'); END;
        CREATE TRIGGER quotes_immutable_update BEFORE UPDATE ON quotes
        BEGIN SELECT RAISE(ABORT, 'quote is immutable'); END;
        CREATE TRIGGER quotes_immutable_delete BEFORE DELETE ON quotes
        BEGIN SELECT RAISE(ABORT, 'quote is immutable'); END;
        CREATE TRIGGER reservation_identity_immutable BEFORE UPDATE ON reservations
        WHEN NEW.reservation_id != OLD.reservation_id
          OR NEW.idempotency_key != OLD.idempotency_key
          OR NEW.account_id != OLD.account_id OR NEW.quote_id != OLD.quote_id
          OR NEW.call_id != OLD.call_id OR NEW.operation_id != OLD.operation_id
          OR NEW.driver_id != OLD.driver_id OR NEW.device_id != OLD.device_id
          OR NEW.driver_generation != OLD.driver_generation
          OR NEW.driver_fencing_token != OLD.driver_fencing_token
          OR NEW.upper_bound != OLD.upper_bound OR NEW.activation_token != OLD.activation_token
          OR NEW.created_at_ms != OLD.created_at_ms
        BEGIN SELECT RAISE(ABORT, 'reservation identity is immutable'); END;
        CREATE TRIGGER reservations_no_delete BEFORE DELETE ON reservations
        BEGIN SELECT RAISE(ABORT, 'reservation is immutable history'); END;
        CREATE TRIGGER activation_receipts_immutable_update BEFORE UPDATE ON reservation_activation_receipts
        BEGIN SELECT RAISE(ABORT, 'activation receipt is immutable'); END;
        CREATE TRIGGER activation_receipts_immutable_delete BEFORE DELETE ON reservation_activation_receipts
        BEGIN SELECT RAISE(ABORT, 'activation receipt is immutable'); END;

        PRAGMA user_version = 1;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Adds authority-assigned Driver gateway and Resource/Ledger endpoint proofs.
#[allow(clippy::too_many_lines)]
pub(crate) fn migrate_v2(connection: &mut Connection) -> Result<(), ResourceAuthorityError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN (
            'driver_gateway_identities', 'driver_gateway_endpoint_proofs',
            'resource_ledger_endpoint_proofs'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND (
            name LIKE 'driver_gateway_identities_%'
            OR name LIKE 'driver_gateway_endpoint_proofs_%'
            OR name LIKE 'resource_ledger_endpoint_proofs_%'
         )",
        [],
        |row| row.get(0),
    )?;
    let missing_coverage: i64 = if table_count == 3 {
        connection.query_row(
            "SELECT
                (SELECT COUNT(*) FROM drivers AS d LEFT JOIN driver_gateway_identities AS i
                 ON i.driver_id=d.driver_id WHERE i.driver_id IS NULL)
              + (SELECT COUNT(*) FROM driver_generations AS g
                 LEFT JOIN driver_gateway_endpoint_proofs AS p
                 ON p.driver_id=g.driver_id AND p.driver_generation=g.generation
                 WHERE p.driver_id IS NULL)
              + (SELECT COUNT(*) FROM resource_accounts AS a
                 LEFT JOIN resource_ledger_endpoint_proofs AS p ON p.account_id=a.account_id
                 WHERE p.account_id IS NULL)",
            [],
            |row| row.get(0),
        )?
    } else {
        0
    };
    if table_count == 3 && trigger_count == 6 && missing_coverage == 0 {
        connection.pragma_update(None, "user_version", 2)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 || missing_coverage != 0 {
        return Err(ResourceAuthorityError::CorruptRecord(
            "partial resource endpoint proof schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE driver_gateway_identities (
            driver_id BLOB PRIMARY KEY NOT NULL CHECK(length(driver_id) = 16),
            participant_id BLOB NOT NULL UNIQUE CHECK(length(participant_id) = 16),
            FOREIGN KEY(driver_id) REFERENCES drivers(driver_id)
        ) STRICT;
        CREATE TABLE driver_gateway_endpoint_proofs (
            driver_id BLOB NOT NULL CHECK(length(driver_id) = 16),
            driver_generation INTEGER NOT NULL CHECK(driver_generation >= 1),
            participant_id BLOB NOT NULL CHECK(length(participant_id) = 16),
            admission_receipt_id BLOB NOT NULL UNIQUE CHECK(length(admission_receipt_id) = 16),
            PRIMARY KEY(driver_id, driver_generation),
            FOREIGN KEY(driver_id) REFERENCES driver_gateway_identities(driver_id),
            FOREIGN KEY(driver_id, driver_generation)
                REFERENCES driver_generations(driver_id, generation)
        ) STRICT;
        CREATE TABLE resource_ledger_endpoint_proofs (
            account_id BLOB PRIMARY KEY NOT NULL CHECK(length(account_id) = 16),
            participant_id BLOB NOT NULL UNIQUE CHECK(length(participant_id) = 16),
            participant_generation BLOB NOT NULL CHECK(length(participant_generation) = 8),
            admission_receipt_id BLOB NOT NULL UNIQUE CHECK(length(admission_receipt_id) = 16),
            FOREIGN KEY(account_id) REFERENCES resource_accounts(account_id)
        ) STRICT;

        INSERT INTO driver_gateway_identities
            SELECT driver_id, randomblob(16) FROM drivers;
        INSERT INTO driver_gateway_endpoint_proofs
            SELECT g.driver_id, g.generation, i.participant_id, randomblob(16)
            FROM driver_generations AS g JOIN driver_gateway_identities AS i USING(driver_id);
        INSERT INTO resource_ledger_endpoint_proofs
            SELECT account_id, randomblob(16), X'0000000000000001', randomblob(16)
            FROM resource_accounts;

        CREATE TRIGGER driver_gateway_identities_immutable_update
        BEFORE UPDATE ON driver_gateway_identities
        BEGIN SELECT RAISE(ABORT, 'driver gateway identity is immutable'); END;
        CREATE TRIGGER driver_gateway_identities_immutable_delete
        BEFORE DELETE ON driver_gateway_identities
        BEGIN SELECT RAISE(ABORT, 'driver gateway identity is immutable'); END;
        CREATE TRIGGER driver_gateway_endpoint_proofs_immutable_update
        BEFORE UPDATE ON driver_gateway_endpoint_proofs
        BEGIN SELECT RAISE(ABORT, 'driver gateway endpoint proof is immutable'); END;
        CREATE TRIGGER driver_gateway_endpoint_proofs_immutable_delete
        BEFORE DELETE ON driver_gateway_endpoint_proofs
        BEGIN SELECT RAISE(ABORT, 'driver gateway endpoint proof is immutable'); END;
        CREATE TRIGGER resource_ledger_endpoint_proofs_immutable_update
        BEFORE UPDATE ON resource_ledger_endpoint_proofs
        BEGIN SELECT RAISE(ABORT, 'resource ledger endpoint proof is immutable'); END;
        CREATE TRIGGER resource_ledger_endpoint_proofs_immutable_delete
        BEFORE DELETE ON resource_ledger_endpoint_proofs
        BEGIN SELECT RAISE(ABORT, 'resource ledger endpoint proof is immutable'); END;
        PRAGMA user_version = 2;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Adds durable cumulative-usage high-water and immutable consume receipts.
pub(crate) fn migrate_v3(connection: &mut Connection) -> Result<(), ResourceAuthorityError> {
    let reservation_columns = {
        let mut statement = connection.prepare("PRAGMA table_info(reservations)")?;
        statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
    };
    let has_high_water_columns = reservation_columns
        .iter()
        .any(|name| name == "usage_high_water_seq")
        && reservation_columns
            .iter()
            .any(|name| name == "usage_high_water");
    let consumption_table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name='reservation_consumption_receipts'",
        [],
        |row| row.get(0),
    )?;
    let consumption_trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'reservation_consumption_receipts_immutable_update',
            'reservation_consumption_receipts_immutable_delete',
            'reservation_usage_high_water_bound_insert',
            'reservation_usage_high_water_bound_update'
         )",
        [],
        |row| row.get(0),
    )?;
    if has_high_water_columns && consumption_table_count == 1 && consumption_trigger_count == 4 {
        connection.pragma_update(None, "user_version", 3)?;
        return Ok(());
    }
    if has_high_water_columns
        || reservation_columns
            .iter()
            .any(|name| name == "usage_high_water_seq")
        || reservation_columns
            .iter()
            .any(|name| name == "usage_high_water")
        || consumption_table_count != 0
        || consumption_trigger_count != 0
    {
        return Err(ResourceAuthorityError::CorruptRecord(
            "partial resource consumption schema",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "ALTER TABLE reservations
             ADD COLUMN usage_high_water_seq INTEGER NOT NULL DEFAULT 0
             CHECK(usage_high_water_seq >= 0);
         ALTER TABLE reservations
             ADD COLUMN usage_high_water INTEGER NOT NULL DEFAULT 0
             CHECK(usage_high_water >= 0);

         CREATE TABLE reservation_consumption_receipts (
             receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
             reservation_id BLOB NOT NULL CHECK(length(reservation_id) = 16),
             operation_id BLOB NOT NULL CHECK(length(operation_id) = 16),
             activation_receipt_id BLOB NOT NULL CHECK(length(activation_receipt_id) = 16),
             sequence INTEGER NOT NULL CHECK(sequence >= 1),
             cumulative_usage INTEGER NOT NULL CHECK(cumulative_usage >= 0),
             consumed_at_ms INTEGER NOT NULL CHECK(consumed_at_ms >= 0),
             UNIQUE(reservation_id, sequence),
             FOREIGN KEY(reservation_id) REFERENCES reservations(reservation_id),
             FOREIGN KEY(activation_receipt_id)
                 REFERENCES reservation_activation_receipts(receipt_id)
         ) STRICT;

         CREATE TRIGGER reservation_usage_high_water_bound_insert
         BEFORE INSERT ON reservations
         WHEN NEW.usage_high_water > NEW.upper_bound
         BEGIN SELECT RAISE(ABORT, 'reservation usage exceeds upper bound'); END;
         CREATE TRIGGER reservation_usage_high_water_bound_update
         BEFORE UPDATE OF usage_high_water, upper_bound ON reservations
         WHEN NEW.usage_high_water > NEW.upper_bound
         BEGIN SELECT RAISE(ABORT, 'reservation usage exceeds upper bound'); END;
         CREATE TRIGGER reservation_consumption_receipts_immutable_update
         BEFORE UPDATE ON reservation_consumption_receipts
         BEGIN SELECT RAISE(ABORT, 'consumption receipt is immutable'); END;
         CREATE TRIGGER reservation_consumption_receipts_immutable_delete
         BEFORE DELETE ON reservation_consumption_receipts
         BEGIN SELECT RAISE(ABORT, 'consumption receipt is immutable'); END;

         PRAGMA user_version = 3;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Adds the fail-closed Reservation quarantine tombstone. The legacy
/// `reservations.state` check remains `RESERVED|ACTIVE`; a non-null
/// `quarantine_receipt_id` is the durable QUARANTINED overlay so v1 rows can
/// migrate without rewriting immutable Reservation identity/history.
#[allow(clippy::too_many_lines)]
pub(crate) fn migrate_v4(connection: &mut Connection) -> Result<(), ResourceAuthorityError> {
    let reservation_columns = {
        let mut statement = connection.prepare("PRAGMA table_info(reservations)")?;
        statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
    };
    let has_quarantine_columns = reservation_columns
        .iter()
        .any(|name| name == "quarantine_receipt_id")
        && reservation_columns
            .iter()
            .any(|name| name == "quarantined_at_ms");
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name='reservation_quarantine_receipts'",
        [],
        |row| row.get(0),
    )?;
    let index_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='index' AND name='reservations_quarantine_receipt_unique'",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'reservation_quarantine_receipts_immutable_update',
            'reservation_quarantine_receipts_immutable_delete',
            'reservation_quarantine_binding_insert',
            'reservation_quarantine_binding_update'
         )",
        [],
        |row| row.get(0),
    )?;
    if has_quarantine_columns && table_count == 1 && index_count == 1 && trigger_count == 4 {
        connection.pragma_update(None, "user_version", 4)?;
        return Ok(());
    }
    if has_quarantine_columns
        || reservation_columns
            .iter()
            .any(|name| name == "quarantine_receipt_id")
        || reservation_columns
            .iter()
            .any(|name| name == "quarantined_at_ms")
        || table_count != 0
        || index_count != 0
        || trigger_count != 0
    {
        return Err(ResourceAuthorityError::CorruptRecord(
            "partial resource quarantine schema",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "ALTER TABLE reservations
             ADD COLUMN quarantine_receipt_id BLOB
                 CHECK(quarantine_receipt_id IS NULL OR length(quarantine_receipt_id) = 16);
         ALTER TABLE reservations
             ADD COLUMN quarantined_at_ms INTEGER
                 CHECK(quarantined_at_ms IS NULL OR quarantined_at_ms >= 0);
         CREATE UNIQUE INDEX reservations_quarantine_receipt_unique
             ON reservations(quarantine_receipt_id)
             WHERE quarantine_receipt_id IS NOT NULL;

         CREATE TABLE reservation_quarantine_receipts (
             receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
             reservation_id BLOB NOT NULL UNIQUE CHECK(length(reservation_id) = 16),
             operation_id BLOB NOT NULL CHECK(length(operation_id) = 16),
             activation_receipt_id BLOB NOT NULL CHECK(length(activation_receipt_id) = 16),
             reason_digest BLOB NOT NULL CHECK(length(reason_digest) = 32),
             high_water_seq INTEGER NOT NULL CHECK(high_water_seq >= 0),
             high_water INTEGER NOT NULL CHECK(high_water >= 0),
             quarantined_at_ms INTEGER NOT NULL CHECK(quarantined_at_ms >= 0),
             FOREIGN KEY(reservation_id) REFERENCES reservations(reservation_id),
             FOREIGN KEY(activation_receipt_id)
                 REFERENCES reservation_activation_receipts(receipt_id)
         ) STRICT;

         CREATE TRIGGER reservation_quarantine_binding_insert
         BEFORE INSERT ON reservation_quarantine_receipts
         WHEN NOT EXISTS (
             SELECT 1 FROM reservations AS r
             WHERE r.reservation_id = NEW.reservation_id
               AND r.operation_id = NEW.operation_id
               AND r.activation_receipt_id = NEW.activation_receipt_id
               AND r.state = 1
         )
         BEGIN SELECT RAISE(ABORT, 'quarantine receipt binding mismatch'); END;
         CREATE TRIGGER reservation_quarantine_binding_update
         BEFORE UPDATE OF reservation_id, operation_id, activation_receipt_id
             ON reservation_quarantine_receipts
         BEGIN SELECT RAISE(ABORT, 'quarantine receipt is immutable'); END;
         CREATE TRIGGER reservation_quarantine_receipts_immutable_update
         BEFORE UPDATE ON reservation_quarantine_receipts
         BEGIN SELECT RAISE(ABORT, 'quarantine receipt is immutable'); END;
         CREATE TRIGGER reservation_quarantine_receipts_immutable_delete
         BEFORE DELETE ON reservation_quarantine_receipts
         BEGIN SELECT RAISE(ABORT, 'quarantine receipt is immutable'); END;

         PRAGMA user_version = 4;",
    )?;
    transaction.commit()?;
    Ok(())
}
