-- Frozen golden copy of the B-TASK-003 v3 schema additions (schema v3
-- plane), intentionally duplicated so the fixture cannot drift with the
-- live source. Prepend golden_v1_ddl.sql + golden_v2_ddl.sql for a full
-- v3 database.

CREATE TABLE effect_history (
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            effect_history_seq BLOB NOT NULL CHECK(length(effect_history_seq) = 8),
            logical_effect_id BLOB NOT NULL CHECK(length(logical_effect_id) = 32),
            retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
            action_proposal_digest BLOB NOT NULL CHECK(length(action_proposal_digest) = 32),
            idempotency_identity_digest BLOB NOT NULL CHECK(length(idempotency_identity_digest) = 32),
            operation_id BLOB CHECK(operation_id IS NULL OR length(operation_id) = 16),
            outcome INTEGER NOT NULL,
            authoritative_effect_receipt_id BLOB NOT NULL CHECK(length(authoritative_effect_receipt_id) = 16),
            compensation_receipt_id BLOB CHECK(compensation_receipt_id IS NULL OR length(compensation_receipt_id) = 16),
            created_at_ms INTEGER NOT NULL,
            PRIMARY KEY(task_id, effect_history_seq),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE INDEX effect_history_by_logical
            ON effect_history(task_id, logical_effect_id);

        CREATE TRIGGER effect_history_is_immutable
        BEFORE UPDATE ON effect_history
        BEGIN
            SELECT RAISE(ABORT, 'effect history entry is immutable');
        END;

        CREATE TABLE task_quarantine_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
            permit_id BLOB NOT NULL CHECK(length(permit_id) = 16),
            permit_epoch BLOB NOT NULL CHECK(length(permit_epoch) = 8),
            effect_set_root BLOB NOT NULL CHECK(length(effect_set_root) = 32),
            outstanding_effect_quarantine_root BLOB NOT NULL CHECK(length(outstanding_effect_quarantine_root) = 32),
            conflicting_target_digest BLOB NOT NULL CHECK(length(conflicting_target_digest) = 32),
            known_effect_receipts BLOB NOT NULL,
            unknown_slots BLOB NOT NULL,
            fenced_participant_digest BLOB NOT NULL CHECK(length(fenced_participant_digest) = 32),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TRIGGER task_quarantine_receipt_is_immutable
        BEFORE UPDATE ON task_quarantine_receipts
        BEGIN
            SELECT RAISE(ABORT, 'quarantine receipt is immutable');
        END;

        CREATE TABLE task_adoption_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            original_permit_id BLOB NOT NULL CHECK(length(original_permit_id) = 16),
            original_permit_epoch BLOB NOT NULL CHECK(length(original_permit_epoch) = 8),
            original_control_epoch BLOB NOT NULL CHECK(length(original_control_epoch) = 8),
            original_cancel_epoch BLOB NOT NULL CHECK(length(original_cancel_epoch) = 8),
            effect_set_root BLOB NOT NULL CHECK(length(effect_set_root) = 32),
            observed_effect_slot_state_root BLOB NOT NULL CHECK(length(observed_effect_slot_state_root) = 32),
            adoption_epoch BLOB NOT NULL CHECK(length(adoption_epoch) = 8),
            created_at_ms INTEGER NOT NULL,
            UNIQUE(task_id, idempotency_key),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE INDEX task_adoption_receipts_by_permit
            ON task_adoption_receipts(task_id, original_permit_id);

        CREATE TRIGGER task_adoption_receipt_is_immutable
        BEFORE UPDATE ON task_adoption_receipts
        BEGIN
            SELECT RAISE(ABORT, 'adoption receipt is immutable');
        END;

        CREATE TABLE task_reconcile_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            permit_id BLOB NOT NULL CHECK(length(permit_id) = 16),
            permit_epoch BLOB NOT NULL CHECK(length(permit_epoch) = 8),
            permit_adoption_receipt_id BLOB NOT NULL CHECK(length(permit_adoption_receipt_id) = 16),
            effect_slot_id BLOB NOT NULL CHECK(length(effect_slot_id) = 16),
            effect_seq BLOB NOT NULL CHECK(length(effect_seq) = 8),
            logical_effect_id BLOB NOT NULL CHECK(length(logical_effect_id) = 32),
            retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
            effect_set_root BLOB NOT NULL CHECK(length(effect_set_root) = 32),
            outcome INTEGER NOT NULL,
            closure_proof_digest BLOB NOT NULL CHECK(length(closure_proof_digest) = 32),
            effect_receipt_id BLOB CHECK(effect_receipt_id IS NULL OR length(effect_receipt_id) = 16),
            effect_slot_state_root_after BLOB NOT NULL CHECK(length(effect_slot_state_root_after) = 32),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE INDEX task_reconcile_receipts_by_slot
            ON task_reconcile_receipts(permit_id, effect_seq);

        CREATE TRIGGER task_reconcile_receipt_is_immutable
        BEFORE UPDATE ON task_reconcile_receipts
        BEGIN
            SELECT RAISE(ABORT, 'reconcile receipt is immutable');
        END;

        CREATE TABLE task_effect_sequences (
            task_id BLOB PRIMARY KEY NOT NULL CHECK(length(task_id) = 16),
            effect_history_seq BLOB NOT NULL CHECK(length(effect_history_seq) = 8),
            adoption_epoch BLOB NOT NULL CHECK(length(adoption_epoch) = 8),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TABLE task_finalize_proofs (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            proof_digest BLOB NOT NULL CHECK(length(proof_digest) = 32),
            FOREIGN KEY(receipt_id) REFERENCES task_receipts(receipt_id)
        ) STRICT;

        PRAGMA user_version = 3;
