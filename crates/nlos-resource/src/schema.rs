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
