CREATE TABLE tasks (
            task_id BLOB PRIMARY KEY NOT NULL CHECK(length(task_id) = 16),
            task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
            head_commit_seq BLOB NOT NULL CHECK(length(head_commit_seq) = 8),
            head_effect_history_root BLOB NOT NULL CHECK(length(head_effect_history_root) = 32),
            retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
            control_epoch BLOB NOT NULL CHECK(length(control_epoch) = 8),
            cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
            permit_epoch BLOB NOT NULL CHECK(length(permit_epoch) = 8),
            task_state INTEGER NOT NULL,
            revision INTEGER NOT NULL DEFAULT 0,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL
        ) STRICT;

        CREATE TABLE task_snapshots (
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            snapshot_id BLOB NOT NULL CHECK(length(snapshot_id) = 16),
            snapshot_digest BLOB NOT NULL CHECK(length(snapshot_digest) = 32),
            expected_head_commit_seq BLOB NOT NULL CHECK(length(expected_head_commit_seq) = 8),
            effect_history_root BLOB NOT NULL CHECK(length(effect_history_root) = 32),
            retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
            created_at_ms INTEGER NOT NULL,
            PRIMARY KEY(task_id, snapshot_id),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TRIGGER task_snapshot_is_immutable
        BEFORE UPDATE ON task_snapshots
        BEGIN
            SELECT RAISE(ABORT, 'task snapshot is immutable');
        END;

        CREATE TABLE task_attempts (
            attempt_id BLOB PRIMARY KEY NOT NULL CHECK(length(attempt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            snapshot_id BLOB NOT NULL CHECK(length(snapshot_id) = 16),
            cancellation_scope_id BLOB NOT NULL CHECK(length(cancellation_scope_id) = 16),
            cancellation_generation BLOB NOT NULL CHECK(length(cancellation_generation) = 8),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            attempt_state INTEGER NOT NULL,
            receipt_id BLOB CHECK(receipt_id IS NULL OR length(receipt_id) = 16),
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            UNIQUE(task_id, idempotency_key),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TABLE commit_permits (
            permit_id BLOB PRIMARY KEY NOT NULL CHECK(length(permit_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            expected_head_commit_seq BLOB NOT NULL CHECK(length(expected_head_commit_seq) = 8),
            expected_effect_history_root BLOB NOT NULL CHECK(length(expected_effect_history_root) = 32),
            expected_retry_fence_epoch BLOB NOT NULL CHECK(length(expected_retry_fence_epoch) = 8),
            write_set_root BLOB NOT NULL CHECK(length(write_set_root) = 32),
            permit_epoch BLOB NOT NULL CHECK(length(permit_epoch) = 8),
            control_epoch BLOB NOT NULL CHECK(length(control_epoch) = 8),
            cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
            valid_until_ms INTEGER NOT NULL,
            permit_state INTEGER NOT NULL,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            UNIQUE(task_id, idempotency_key),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE UNIQUE INDEX commit_permits_single_active
            ON commit_permits(task_id) WHERE permit_state = 0;

        CREATE TABLE task_cancels (
            task_id BLOB PRIMARY KEY NOT NULL CHECK(length(task_id) = 16),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            cancel_epoch_after BLOB NOT NULL CHECK(length(cancel_epoch_after) = 8),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TABLE task_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            permit_id BLOB CHECK(permit_id IS NULL OR length(permit_id) = 16),
            attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            outcome INTEGER NOT NULL,
            prior_head_commit_seq BLOB NOT NULL CHECK(length(prior_head_commit_seq) = 8),
            prior_effect_history_root BLOB NOT NULL CHECK(length(prior_effect_history_root) = 32),
            prior_retry_fence_epoch BLOB NOT NULL CHECK(length(prior_retry_fence_epoch) = 8),
            new_head_commit_seq BLOB NOT NULL CHECK(length(new_head_commit_seq) = 8),
            new_effect_history_root BLOB NOT NULL CHECK(length(new_effect_history_root) = 32),
            new_retry_fence_epoch BLOB NOT NULL CHECK(length(new_retry_fence_epoch) = 8),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE INDEX task_receipts_by_permit
            ON task_receipts(task_id, permit_id);

        CREATE TRIGGER task_receipt_is_immutable
        BEFORE UPDATE ON task_receipts
        BEGIN
            SELECT RAISE(ABORT, 'task receipt is immutable');
        END;

        PRAGMA user_version = 1;
