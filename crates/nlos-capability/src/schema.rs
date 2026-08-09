use rusqlite::{Connection, TransactionBehavior};

use crate::CapabilityAuthorityError;

#[allow(clippy::too_many_lines)] // One auditable transaction contains the complete v1 DDL.
pub(crate) fn migrate_v1(connection: &mut Connection) -> Result<(), CapabilityAuthorityError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE capability_heads (
            capability_id BLOB PRIMARY KEY NOT NULL CHECK(length(capability_id) = 16),
            current_generation INTEGER NOT NULL CHECK(current_generation >= 1),
            issuer_principal_id BLOB NOT NULL CHECK(length(issuer_principal_id) = 16),
            issuer_control_domain_id BLOB NOT NULL CHECK(length(issuer_control_domain_id) = 16),
            holder_principal_id BLOB NOT NULL CHECK(length(holder_principal_id) = 16),
            holder_control_domain_id BLOB NOT NULL CHECK(length(holder_control_domain_id) = 16),
            target_kind INTEGER NOT NULL CHECK(target_kind IN (1, 2)),
            target_id BLOB NOT NULL CHECK(length(target_id) = 16),
            rights INTEGER NOT NULL CHECK(rights > 0 AND rights <= 15),
            purpose_digest BLOB CHECK(purpose_digest IS NULL OR length(purpose_digest) = 32),
            valid_from_ms INTEGER NOT NULL CHECK(valid_from_ms >= 0),
            valid_until_ms INTEGER NOT NULL CHECK(valid_until_ms >= valid_from_ms),
            delegation_depth_remaining INTEGER NOT NULL CHECK(delegation_depth_remaining BETWEEN 0 AND 255),
            call_limit INTEGER CHECK(call_limit IS NULL OR call_limit > 0),
            parent_capability_id BLOB CHECK(parent_capability_id IS NULL OR length(parent_capability_id) = 16),
            parent_generation INTEGER CHECK(parent_generation IS NULL OR parent_generation >= 1),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            CHECK((parent_capability_id IS NULL) = (parent_generation IS NULL)),
            FOREIGN KEY(parent_capability_id) REFERENCES capability_heads(capability_id)
        ) STRICT;

        CREATE TABLE capability_versions (
            capability_id BLOB NOT NULL CHECK(length(capability_id) = 16),
            generation INTEGER NOT NULL CHECK(generation >= 1),
            revoked_at_ms INTEGER CHECK(revoked_at_ms IS NULL OR revoked_at_ms >= 0),
            PRIMARY KEY(capability_id, generation),
            FOREIGN KEY(capability_id) REFERENCES capability_heads(capability_id),
            CHECK((generation = 1) = (revoked_at_ms IS NULL))
        ) STRICT;

        CREATE TABLE capability_issue_receipts (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key) = 16),
            request_digest BLOB NOT NULL CHECK(length(request_digest) = 32),
            receipt_id BLOB NOT NULL UNIQUE CHECK(length(receipt_id) = 16),
            capability_id BLOB NOT NULL UNIQUE CHECK(length(capability_id) = 16),
            generation INTEGER NOT NULL CHECK(generation = 1),
            parent_capability_id BLOB CHECK(parent_capability_id IS NULL OR length(parent_capability_id) = 16),
            parent_generation INTEGER CHECK(parent_generation IS NULL OR parent_generation >= 1),
            issued_at_ms INTEGER NOT NULL CHECK(issued_at_ms >= 0),
            CHECK((parent_capability_id IS NULL) = (parent_generation IS NULL)),
            FOREIGN KEY(capability_id, generation) REFERENCES capability_versions(capability_id, generation),
            FOREIGN KEY(parent_capability_id) REFERENCES capability_heads(capability_id)
        ) STRICT;

        CREATE TABLE capability_revocation_receipts (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key) = 16),
            request_digest BLOB NOT NULL CHECK(length(request_digest) = 32),
            receipt_id BLOB NOT NULL UNIQUE CHECK(length(receipt_id) = 16),
            capability_id BLOB NOT NULL CHECK(length(capability_id) = 16),
            prior_generation INTEGER NOT NULL CHECK(prior_generation >= 1),
            resulting_generation INTEGER NOT NULL CHECK(resulting_generation = prior_generation + 1),
            revoker_principal_id BLOB NOT NULL CHECK(length(revoker_principal_id) = 16),
            revoked_at_ms INTEGER NOT NULL CHECK(revoked_at_ms >= 0),
            FOREIGN KEY(capability_id, resulting_generation)
                REFERENCES capability_versions(capability_id, generation)
        ) STRICT;

        CREATE TRIGGER capability_heads_identity_immutable BEFORE UPDATE ON capability_heads
        WHEN NEW.capability_id != OLD.capability_id
          OR NEW.issuer_principal_id != OLD.issuer_principal_id
          OR NEW.issuer_control_domain_id != OLD.issuer_control_domain_id
          OR NEW.holder_principal_id != OLD.holder_principal_id
          OR NEW.holder_control_domain_id != OLD.holder_control_domain_id
          OR NEW.target_kind != OLD.target_kind OR NEW.target_id != OLD.target_id
          OR NEW.rights != OLD.rights OR NEW.purpose_digest IS NOT OLD.purpose_digest
          OR NEW.valid_from_ms != OLD.valid_from_ms OR NEW.valid_until_ms != OLD.valid_until_ms
          OR NEW.delegation_depth_remaining != OLD.delegation_depth_remaining
          OR NEW.call_limit IS NOT OLD.call_limit
          OR NEW.parent_capability_id IS NOT OLD.parent_capability_id
          OR NEW.parent_generation IS NOT OLD.parent_generation
          OR NEW.created_at_ms != OLD.created_at_ms
        BEGIN SELECT RAISE(ABORT, 'capability identity is immutable'); END;
        CREATE TRIGGER capability_heads_no_delete BEFORE DELETE ON capability_heads
        BEGIN SELECT RAISE(ABORT, 'capability history is immutable'); END;
        CREATE TRIGGER capability_versions_immutable_update BEFORE UPDATE ON capability_versions
        BEGIN SELECT RAISE(ABORT, 'capability version is immutable'); END;
        CREATE TRIGGER capability_versions_immutable_delete BEFORE DELETE ON capability_versions
        BEGIN SELECT RAISE(ABORT, 'capability version is immutable'); END;
        CREATE TRIGGER capability_issue_receipts_immutable_update BEFORE UPDATE ON capability_issue_receipts
        BEGIN SELECT RAISE(ABORT, 'capability issue receipt is immutable'); END;
        CREATE TRIGGER capability_issue_receipts_immutable_delete BEFORE DELETE ON capability_issue_receipts
        BEGIN SELECT RAISE(ABORT, 'capability issue receipt is immutable'); END;
        CREATE TRIGGER capability_revocations_immutable_update BEFORE UPDATE ON capability_revocation_receipts
        BEGIN SELECT RAISE(ABORT, 'capability revocation receipt is immutable'); END;
        CREATE TRIGGER capability_revocations_immutable_delete BEFORE DELETE ON capability_revocation_receipts
        BEGIN SELECT RAISE(ABORT, 'capability revocation receipt is immutable'); END;

        PRAGMA user_version = 1;",
    )?;
    transaction.commit()?;
    Ok(())
}
