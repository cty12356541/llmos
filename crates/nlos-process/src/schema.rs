use rusqlite::{Connection, TransactionBehavior};

use crate::ProcessAuthorityError;

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
