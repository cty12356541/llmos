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

/// v2 (B-CAPABILITY-002 call-limit ledger): an additive, append-only
/// consumption ledger. Each admitted exercise inserts one immutable row;
/// remaining call-limit budget is derived by counting rows, so there is no
/// mutable counter to drift across restarts. Replay idempotency follows the
/// receipt precedent: the idempotency key is the primary key and a rebound
/// request digest fails closed.
#[allow(clippy::too_many_lines)] // One auditable transaction contains the complete v2 DDL.
pub(crate) fn migrate_v2(connection: &mut Connection) -> Result<(), CapabilityAuthorityError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE capability_consumption_rows (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key) = 16),
            request_digest BLOB NOT NULL CHECK(length(request_digest) = 32),
            receipt_id BLOB NOT NULL UNIQUE CHECK(length(receipt_id) = 16),
            capability_id BLOB NOT NULL CHECK(length(capability_id) = 16),
            generation INTEGER NOT NULL CHECK(generation >= 1),
            remaining INTEGER CHECK(remaining IS NULL OR remaining >= 0),
            consumed_at_ms INTEGER NOT NULL CHECK(consumed_at_ms >= 0),
            FOREIGN KEY(capability_id, generation)
                REFERENCES capability_versions(capability_id, generation)
        ) STRICT;

        CREATE INDEX capability_consumption_capability_idx
            ON capability_consumption_rows(capability_id);

        CREATE TRIGGER capability_consumption_immutable_update
            BEFORE UPDATE ON capability_consumption_rows
        BEGIN SELECT RAISE(ABORT, 'capability consumption row is immutable'); END;
        CREATE TRIGGER capability_consumption_immutable_delete
            BEFORE DELETE ON capability_consumption_rows
        BEGIN SELECT RAISE(ABORT, 'capability consumption row is immutable'); END;

        PRAGMA user_version = 2;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Independent append-only Blob ledger.
#[allow(clippy::too_many_lines)]
pub(crate) fn migrate_v3(connection: &mut Connection) -> Result<(), CapabilityAuthorityError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE blob_boot (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            epoch INTEGER NOT NULL CHECK(epoch >= 0)
        ) STRICT;
        INSERT INTO blob_boot VALUES (1, 0);
        CREATE TRIGGER blob_boot_update BEFORE UPDATE ON blob_boot
            WHEN new.singleton != old.singleton OR new.epoch != old.epoch + 1
        BEGIN SELECT RAISE(ABORT, 'blob epoch must advance by one'); END;

        CREATE TABLE blob_audit_usage (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            rows INTEGER NOT NULL CHECK(rows BETWEEN 0 AND 2000000),
            estimated_bytes INTEGER NOT NULL CHECK(estimated_bytes BETWEEN 0 AND 536870912)
        ) STRICT;
        INSERT INTO blob_audit_usage VALUES (1, 0, 0);
        CREATE TRIGGER blob_audit_usage_update BEFORE UPDATE ON blob_audit_usage
            WHEN new.singleton != old.singleton OR new.rows != old.rows + 1
              OR new.estimated_bytes <= old.estimated_bytes
        BEGIN SELECT RAISE(ABORT, 'blob audit usage must advance'); END;

        CREATE TABLE blob_tokens (
            reference_id BLOB PRIMARY KEY NOT NULL CHECK(length(reference_id) = 16),
            receipt_id BLOB NOT NULL UNIQUE CHECK(length(receipt_id) = 16),
            epoch INTEGER NOT NULL CHECK(epoch >= 1),
            issuer BLOB NOT NULL CHECK(length(issuer) = 16),
            application BLOB NOT NULL CHECK(length(application) = 16),
            holder BLOB NOT NULL CHECK(length(holder) = 16),
            workspace BLOB NOT NULL CHECK(length(workspace) = 16),
            namespace BLOB NOT NULL CHECK(length(namespace) = 16),
            workspace_revision INTEGER NOT NULL CHECK(workspace_revision >= 1),
            source BLOB NOT NULL CHECK(length(source) = 16),
            source_generation INTEGER NOT NULL CHECK(source_generation >= 1),
            path TEXT NOT NULL CHECK(length(CAST(path AS BLOB)) BETWEEN 1 AND 1024),
            file_digest BLOB NOT NULL CHECK(length(file_digest) = 32),
            size_bytes INTEGER NOT NULL CHECK(size_bytes BETWEEN 0 AND 67108864),
            block_bytes INTEGER NOT NULL CHECK(block_bytes BETWEEN 1 AND 65408),
            manifest BLOB NOT NULL CHECK(length(manifest) =
                ((size_bytes + block_bytes - 1) / block_bytes) * 32 AND length(manifest) <= 131072),
            max_reads INTEGER NOT NULL CHECK(max_reads BETWEEN 1 AND 4096),
            valid_until_ms INTEGER NOT NULL CHECK(valid_until_ms > minted_at_ms),
            minted_at_ms INTEGER NOT NULL CHECK(minted_at_ms >= 0),
            signed_at_ms INTEGER NOT NULL CHECK(signed_at_ms BETWEEN 0 AND minted_at_ms),
            key_id BLOB NOT NULL CHECK(length(key_id) = 16),
            key_generation INTEGER NOT NULL CHECK(key_generation >= 1),
            signature BLOB NOT NULL CHECK(length(signature) = 64),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            request_digest BLOB NOT NULL CHECK(length(request_digest) = 32),
            command_process BLOB NOT NULL CHECK(length(command_process) = 16),
            command_process_generation INTEGER NOT NULL CHECK(command_process_generation >= 1),
            UNIQUE(issuer, application, workspace, namespace, idempotency_key),
            FOREIGN KEY(source, source_generation)
                REFERENCES capability_versions(capability_id, generation)
        ) STRICT;
        CREATE INDEX blob_tokens_epoch_expiry ON blob_tokens(epoch, valid_until_ms);
        CREATE INDEX blob_tokens_mints ON blob_tokens(minted_at_ms, issuer);
        CREATE TRIGGER blob_tokens_no_replace BEFORE INSERT ON blob_tokens
            WHEN new.epoch != (SELECT epoch FROM blob_boot WHERE singleton=1)
              OR EXISTS(SELECT 1 FROM blob_tokens WHERE reference_id=new.reference_id
                    OR receipt_id=new.receipt_id
                    OR (issuer=new.issuer AND application=new.application AND workspace=new.workspace
                        AND namespace=new.namespace AND idempotency_key=new.idempotency_key))
        BEGIN SELECT RAISE(ABORT, 'blob mint record conflicts'); END;
        CREATE TRIGGER blob_tokens_audit AFTER INSERT ON blob_tokens
        BEGIN UPDATE blob_audit_usage SET rows=rows+1,
            estimated_bytes=estimated_bytes+4096+length(new.manifest) WHERE singleton=1; END;

        CREATE TABLE blob_revocations (
            reference_id BLOB PRIMARY KEY NOT NULL CHECK(length(reference_id) = 16),
            receipt_id BLOB NOT NULL UNIQUE CHECK(length(receipt_id) = 16),
            actor BLOB NOT NULL CHECK(length(actor) = 16),
            application BLOB NOT NULL CHECK(length(application) = 16),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            request_digest BLOB NOT NULL CHECK(length(request_digest) = 32),
            key_id BLOB NOT NULL CHECK(length(key_id) = 16),
            key_generation INTEGER NOT NULL CHECK(key_generation >= 1),
            signature BLOB NOT NULL CHECK(length(signature) = 64),
            released_at_ms INTEGER NOT NULL CHECK(released_at_ms >= 0),
            signed_at_ms INTEGER NOT NULL CHECK(signed_at_ms BETWEEN 0 AND released_at_ms),
            command_process BLOB NOT NULL CHECK(length(command_process) = 16),
            command_process_generation INTEGER NOT NULL CHECK(command_process_generation >= 1),
            FOREIGN KEY(reference_id) REFERENCES blob_tokens(reference_id)
        ) STRICT;
        CREATE TRIGGER blob_revocations_guard BEFORE INSERT ON blob_revocations
            WHEN EXISTS(SELECT 1 FROM blob_revocations WHERE reference_id=new.reference_id
                OR receipt_id=new.receipt_id)
              OR NOT EXISTS(SELECT 1 FROM blob_tokens AS token
                WHERE token.reference_id=new.reference_id
                  AND new.actor IN (token.issuer, token.holder)
                  AND token.epoch=(SELECT epoch FROM blob_boot WHERE singleton=1)
                  AND new.released_at_ms >= token.minted_at_ms
                  AND new.released_at_ms < token.valid_until_ms)
        BEGIN SELECT RAISE(ABORT, 'invalid blob revocation'); END;
        CREATE TRIGGER blob_revocations_audit AFTER INSERT ON blob_revocations
        BEGIN UPDATE blob_audit_usage SET rows=rows+1, estimated_bytes=estimated_bytes+1024
            WHERE singleton=1; END;

        CREATE TABLE blob_read_receipts (
            reference_id BLOB NOT NULL CHECK(length(reference_id) = 16),
            actor BLOB NOT NULL CHECK(length(actor) = 16),
            application BLOB NOT NULL CHECK(length(application) = 16),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            request_digest BLOB NOT NULL CHECK(length(request_digest) = 32),
            receipt_id BLOB NOT NULL UNIQUE CHECK(length(receipt_id) = 16),
            ordinal INTEGER NOT NULL CHECK(ordinal BETWEEN 1 AND 4096),
            block_offset INTEGER NOT NULL CHECK(block_offset >= 0),
            block_size INTEGER NOT NULL CHECK(block_size BETWEEN 1 AND 65408),
            block_digest BLOB NOT NULL CHECK(length(block_digest) = 32),
            read_at_ms INTEGER NOT NULL CHECK(read_at_ms >= 0),
            PRIMARY KEY(reference_id, actor, application, idempotency_key),
            UNIQUE(reference_id, ordinal),
            FOREIGN KEY(reference_id) REFERENCES blob_tokens(reference_id)
        ) STRICT;
        CREATE TRIGGER blob_read_receipts_guard BEFORE INSERT ON blob_read_receipts
            WHEN new.ordinal != COALESCE((SELECT max(ordinal) FROM blob_read_receipts
                WHERE reference_id=new.reference_id),0)+1
              OR EXISTS(SELECT 1 FROM blob_revocations WHERE reference_id=new.reference_id)
              OR EXISTS(SELECT 1 FROM blob_read_receipts WHERE receipt_id=new.receipt_id
                OR (reference_id=new.reference_id AND actor=new.actor AND application=new.application
                    AND idempotency_key=new.idempotency_key))
              OR NOT EXISTS(SELECT 1 FROM blob_tokens AS token
                WHERE token.reference_id=new.reference_id AND token.holder=new.actor
                  AND token.epoch=(SELECT epoch FROM blob_boot WHERE singleton=1)
                  AND new.read_at_ms >= token.minted_at_ms AND new.read_at_ms < token.valid_until_ms
                  AND new.ordinal <= token.max_reads
                  AND new.block_offset < token.size_bytes
                  AND new.block_offset % token.block_bytes = 0
                  AND new.block_size=min(token.block_bytes, token.size_bytes-new.block_offset)
                  AND new.block_digest=substr(token.manifest,
                    (new.block_offset/token.block_bytes)*32+1, 32))
        BEGIN SELECT RAISE(ABORT, 'invalid blob read receipt'); END;
        CREATE TRIGGER blob_read_receipts_audit AFTER INSERT ON blob_read_receipts
        BEGIN UPDATE blob_audit_usage SET rows=rows+1, estimated_bytes=estimated_bytes+1024
            WHERE singleton=1; END;

        PRAGMA user_version = 3;",
    )?;
    for table in ["blob_tokens", "blob_read_receipts", "blob_revocations"] {
        transaction.execute_batch(&format!(
            "CREATE TRIGGER {table}_immutable_update BEFORE UPDATE ON {table}
             BEGIN SELECT RAISE(ABORT, 'blob audit rows are immutable'); END;
             CREATE TRIGGER {table}_immutable_delete BEFORE DELETE ON {table}
             BEGIN SELECT RAISE(ABORT, 'blob audit rows are immutable'); END;"
        ))?;
    }
    for table in ["blob_boot", "blob_audit_usage"] {
        transaction.execute_batch(&format!(
            "CREATE TRIGGER {table}_no_delete BEFORE DELETE ON {table}
             BEGIN SELECT RAISE(ABORT, 'blob state cannot be removed'); END;
             CREATE TRIGGER {table}_no_replace BEFORE INSERT ON {table}
             WHEN EXISTS(SELECT 1 FROM {table})
             BEGIN SELECT RAISE(ABORT, 'blob state cannot be replaced'); END;"
        ))?;
    }
    transaction.commit()?;
    Ok(())
}
