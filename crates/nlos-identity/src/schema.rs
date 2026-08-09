use rusqlite::{Connection, TransactionBehavior};

use crate::IdentityAuthorityError;

#[allow(clippy::too_many_lines)] // One auditable transaction contains the complete v1 DDL.
pub(crate) fn migrate_v1(connection: &mut Connection) -> Result<(), IdentityAuthorityError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE principals (
            principal_id BLOB PRIMARY KEY NOT NULL CHECK(length(principal_id) = 16),
            bootstrap_idempotency_key BLOB NOT NULL UNIQUE CHECK(length(bootstrap_idempotency_key) = 16),
            profile_digest BLOB NOT NULL CHECK(length(profile_digest) = 32),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0)
        ) STRICT;

        CREATE TABLE control_domains (
            control_domain_id BLOB PRIMARY KEY NOT NULL CHECK(length(control_domain_id) = 16),
            current_snapshot_id BLOB NOT NULL UNIQUE CHECK(length(current_snapshot_id) = 16),
            current_generation INTEGER NOT NULL CHECK(current_generation >= 1),
            policy_digest BLOB NOT NULL CHECK(length(policy_digest) = 32),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
        ) STRICT;

        CREATE TABLE identity_snapshots (
            identity_snapshot_id BLOB PRIMARY KEY NOT NULL CHECK(length(identity_snapshot_id) = 16),
            control_domain_id BLOB NOT NULL CHECK(length(control_domain_id) = 16),
            generation INTEGER NOT NULL CHECK(generation >= 1),
            prior_snapshot_id BLOB CHECK(prior_snapshot_id IS NULL OR length(prior_snapshot_id) = 16),
            policy_digest BLOB NOT NULL CHECK(length(policy_digest) = 32),
            effective_at_ms INTEGER NOT NULL CHECK(effective_at_ms >= 0),
            change_kind INTEGER NOT NULL CHECK(change_kind IN (1, 2)),
            UNIQUE(control_domain_id, generation),
            FOREIGN KEY(control_domain_id) REFERENCES control_domains(control_domain_id),
            FOREIGN KEY(prior_snapshot_id) REFERENCES identity_snapshots(identity_snapshot_id),
            CHECK((generation = 1) = (prior_snapshot_id IS NULL))
        ) STRICT;

        CREATE TABLE snapshot_principals (
            identity_snapshot_id BLOB NOT NULL CHECK(length(identity_snapshot_id) = 16),
            principal_id BLOB NOT NULL CHECK(length(principal_id) = 16),
            PRIMARY KEY(identity_snapshot_id, principal_id),
            FOREIGN KEY(identity_snapshot_id) REFERENCES identity_snapshots(identity_snapshot_id),
            FOREIGN KEY(principal_id) REFERENCES principals(principal_id)
        ) STRICT;

        CREATE TABLE key_heads (
            key_id BLOB PRIMARY KEY NOT NULL CHECK(length(key_id) = 16),
            principal_id BLOB NOT NULL CHECK(length(principal_id) = 16),
            control_domain_id BLOB NOT NULL CHECK(length(control_domain_id) = 16),
            current_generation INTEGER NOT NULL CHECK(current_generation >= 1),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            FOREIGN KEY(principal_id) REFERENCES principals(principal_id),
            FOREIGN KEY(control_domain_id) REFERENCES control_domains(control_domain_id)
        ) STRICT;

        CREATE TABLE key_versions (
            key_id BLOB NOT NULL CHECK(length(key_id) = 16),
            generation INTEGER NOT NULL CHECK(generation >= 1),
            purpose INTEGER NOT NULL CHECK(purpose = 1),
            algorithm INTEGER NOT NULL CHECK(algorithm = 1),
            public_key BLOB NOT NULL CHECK(length(public_key) = 32),
            valid_from_ms INTEGER NOT NULL CHECK(valid_from_ms >= 0),
            valid_until_ms INTEGER NOT NULL CHECK(valid_until_ms >= valid_from_ms),
            revoked_at_ms INTEGER CHECK(revoked_at_ms IS NULL OR revoked_at_ms >= valid_from_ms),
            PRIMARY KEY(key_id, generation),
            FOREIGN KEY(key_id) REFERENCES key_heads(key_id)
        ) STRICT;

        CREATE TABLE snapshot_key_bindings (
            identity_snapshot_id BLOB NOT NULL CHECK(length(identity_snapshot_id) = 16),
            key_id BLOB NOT NULL CHECK(length(key_id) = 16),
            key_generation INTEGER NOT NULL CHECK(key_generation >= 1),
            PRIMARY KEY(identity_snapshot_id, key_id),
            FOREIGN KEY(identity_snapshot_id) REFERENCES identity_snapshots(identity_snapshot_id),
            FOREIGN KEY(key_id, key_generation) REFERENCES key_versions(key_id, generation)
        ) STRICT;

        CREATE TABLE key_revocations (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key) = 16),
            receipt_id BLOB NOT NULL UNIQUE CHECK(length(receipt_id) = 16),
            key_id BLOB NOT NULL CHECK(length(key_id) = 16),
            expected_key_generation INTEGER NOT NULL CHECK(expected_key_generation >= 1),
            expected_snapshot_id BLOB NOT NULL CHECK(length(expected_snapshot_id) = 16),
            resulting_key_generation INTEGER NOT NULL CHECK(resulting_key_generation = expected_key_generation + 1),
            resulting_snapshot_id BLOB NOT NULL CHECK(length(resulting_snapshot_id) = 16),
            resulting_snapshot_generation INTEGER NOT NULL CHECK(resulting_snapshot_generation >= 2),
            revoked_at_ms INTEGER NOT NULL CHECK(revoked_at_ms >= 0),
            FOREIGN KEY(key_id, resulting_key_generation) REFERENCES key_versions(key_id, generation),
            FOREIGN KEY(resulting_snapshot_id) REFERENCES identity_snapshots(identity_snapshot_id)
        ) STRICT;

        CREATE TRIGGER principals_immutable_update BEFORE UPDATE ON principals
        BEGIN SELECT RAISE(ABORT, 'principal is immutable'); END;
        CREATE TRIGGER principals_immutable_delete BEFORE DELETE ON principals
        BEGIN SELECT RAISE(ABORT, 'principal is immutable'); END;
        CREATE TRIGGER identity_snapshots_immutable_update BEFORE UPDATE ON identity_snapshots
        BEGIN SELECT RAISE(ABORT, 'identity snapshot is immutable'); END;
        CREATE TRIGGER identity_snapshots_immutable_delete BEFORE DELETE ON identity_snapshots
        BEGIN SELECT RAISE(ABORT, 'identity snapshot is immutable'); END;
        CREATE TRIGGER snapshot_principals_immutable_update BEFORE UPDATE ON snapshot_principals
        BEGIN SELECT RAISE(ABORT, 'snapshot principal is immutable'); END;
        CREATE TRIGGER snapshot_principals_immutable_delete BEFORE DELETE ON snapshot_principals
        BEGIN SELECT RAISE(ABORT, 'snapshot principal is immutable'); END;
        CREATE TRIGGER key_versions_immutable_update BEFORE UPDATE ON key_versions
        BEGIN SELECT RAISE(ABORT, 'key version is immutable'); END;
        CREATE TRIGGER key_versions_immutable_delete BEFORE DELETE ON key_versions
        BEGIN SELECT RAISE(ABORT, 'key version is immutable'); END;
        CREATE TRIGGER snapshot_key_bindings_immutable_update BEFORE UPDATE ON snapshot_key_bindings
        BEGIN SELECT RAISE(ABORT, 'snapshot key binding is immutable'); END;
        CREATE TRIGGER snapshot_key_bindings_immutable_delete BEFORE DELETE ON snapshot_key_bindings
        BEGIN SELECT RAISE(ABORT, 'snapshot key binding is immutable'); END;
        CREATE TRIGGER key_revocations_immutable_update BEFORE UPDATE ON key_revocations
        BEGIN SELECT RAISE(ABORT, 'key revocation is immutable'); END;
        CREATE TRIGGER key_revocations_immutable_delete BEFORE DELETE ON key_revocations
        BEGIN SELECT RAISE(ABORT, 'key revocation is immutable'); END;

        PRAGMA user_version = 1;",
    )?;
    transaction.commit()?;
    Ok(())
}
