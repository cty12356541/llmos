CREATE TABLE effect_slots (
            permit_id BLOB NOT NULL CHECK(length(permit_id) = 16),
            effect_seq BLOB NOT NULL CHECK(length(effect_seq) = 8),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            effect_slot_id BLOB NOT NULL CHECK(length(effect_slot_id) = 16),
            logical_effect_id BLOB NOT NULL CHECK(length(logical_effect_id) = 32),
            idempotency_identity_digest BLOB NOT NULL CHECK(length(idempotency_identity_digest) = 32),
            required INTEGER NOT NULL,
            required_condition_digest BLOB CHECK(required_condition_digest IS NULL OR length(required_condition_digest) = 32),
            success_criteria_digest BLOB NOT NULL CHECK(length(success_criteria_digest) = 32),
            action_proposal_digest BLOB NOT NULL CHECK(length(action_proposal_digest) = 32),
            slot_state INTEGER NOT NULL,
            state_seq INTEGER NOT NULL,
            effect_permit_id BLOB CHECK(effect_permit_id IS NULL OR length(effect_permit_id) = 16),
            dispatch_token_digest BLOB CHECK(dispatch_token_digest IS NULL OR length(dispatch_token_digest) = 32),
            effect_receipt_id BLOB CHECK(effect_receipt_id IS NULL OR length(effect_receipt_id) = 16),
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            PRIMARY KEY(permit_id, effect_seq),
            UNIQUE(permit_id, logical_effect_id),
            FOREIGN KEY(permit_id) REFERENCES commit_permits(permit_id),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TABLE permit_effect_sets (
            permit_id BLOB PRIMARY KEY NOT NULL CHECK(length(permit_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            effect_set_root BLOB NOT NULL CHECK(length(effect_set_root) = 32),
            effect_slot_state_root BLOB NOT NULL CHECK(length(effect_slot_state_root) = 32),
            required_effect_count INTEGER NOT NULL,
            satisfied_required_effect_count INTEGER NOT NULL,
            terminal_effect_count INTEGER NOT NULL,
            issued_effect_root BLOB NOT NULL CHECK(length(issued_effect_root) = 32),
            dispatched_effect_root BLOB NOT NULL CHECK(length(dispatched_effect_root) = 32),
            closed_effect_root BLOB NOT NULL CHECK(length(closed_effect_root) = 32),
            outstanding_effect_root BLOB NOT NULL CHECK(length(outstanding_effect_root) = 32),
            revision INTEGER NOT NULL DEFAULT 0,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            FOREIGN KEY(permit_id) REFERENCES commit_permits(permit_id)
        ) STRICT;

        CREATE TABLE effect_permits (
            effect_permit_id BLOB PRIMARY KEY NOT NULL CHECK(length(effect_permit_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            permit_id BLOB NOT NULL CHECK(length(permit_id) = 16),
            permit_epoch BLOB NOT NULL CHECK(length(permit_epoch) = 8),
            attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            effect_slot_id BLOB NOT NULL CHECK(length(effect_slot_id) = 16),
            effect_seq BLOB NOT NULL CHECK(length(effect_seq) = 8),
            logical_effect_id BLOB NOT NULL CHECK(length(logical_effect_id) = 32),
            retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
            idempotency_identity_digest BLOB NOT NULL CHECK(length(idempotency_identity_digest) = 32),
            effect_set_root BLOB NOT NULL CHECK(length(effect_set_root) = 32),
            action_proposal_digest BLOB NOT NULL CHECK(length(action_proposal_digest) = 32),
            control_epoch BLOB NOT NULL CHECK(length(control_epoch) = 8),
            cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
            dispatch_token_digest BLOB NOT NULL CHECK(length(dispatch_token_digest) = 32),
            valid_until_ms INTEGER NOT NULL,
            created_at_ms INTEGER NOT NULL,
            UNIQUE(task_id, idempotency_key),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id),
            FOREIGN KEY(permit_id) REFERENCES commit_permits(permit_id)
        ) STRICT;

        CREATE TABLE effect_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            permit_id BLOB NOT NULL CHECK(length(permit_id) = 16),
            effect_slot_id BLOB NOT NULL CHECK(length(effect_slot_id) = 16),
            effect_seq BLOB NOT NULL CHECK(length(effect_seq) = 8),
            logical_effect_id BLOB NOT NULL CHECK(length(logical_effect_id) = 32),
            receipt_kind INTEGER NOT NULL,
            prior_slot_state INTEGER NOT NULL,
            no_effect_reason INTEGER,
            proof_digest BLOB NOT NULL CHECK(length(proof_digest) = 32),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE INDEX effect_receipts_by_slot
            ON effect_receipts(permit_id, effect_seq);

        CREATE TRIGGER effect_receipt_is_immutable
        BEFORE UPDATE ON effect_receipts
        BEGIN
            SELECT RAISE(ABORT, 'effect receipt is immutable');
        END;

        PRAGMA user_version = 2;
