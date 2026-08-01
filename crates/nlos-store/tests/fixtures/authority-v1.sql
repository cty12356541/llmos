PRAGMA user_version = 1;

CREATE TABLE operations (
    operation_id BLOB PRIMARY KEY NOT NULL CHECK(length(operation_id) = 16),
    generation BLOB NOT NULL CHECK(length(generation) = 8),
    owner_fiber_id BLOB NOT NULL CHECK(length(owner_fiber_id) = 16),
    owner_fiber_generation BLOB NOT NULL CHECK(length(owner_fiber_generation) = 8),
    cancellation_scope_id BLOB NOT NULL CHECK(length(cancellation_scope_id) = 16),
    cancellation_generation BLOB NOT NULL CHECK(length(cancellation_generation) = 8),
    cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
    state_kind INTEGER NOT NULL,
    receipt_id BLOB CHECK(receipt_id IS NULL OR length(receipt_id) = 16),
    issued_callback_id BLOB CHECK(issued_callback_id IS NULL OR length(issued_callback_id) = 16),
    issued_cancel_epoch BLOB CHECK(issued_cancel_epoch IS NULL OR length(issued_cancel_epoch) = 8),
    accepted_callback_id BLOB CHECK(accepted_callback_id IS NULL OR length(accepted_callback_id) = 16),
    revision INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE operation_outbox (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    kind INTEGER NOT NULL,
    operation_id BLOB NOT NULL CHECK(length(operation_id) = 16),
    operation_generation BLOB NOT NULL CHECK(length(operation_generation) = 8),
    owner_fiber_id BLOB NOT NULL CHECK(length(owner_fiber_id) = 16),
    owner_fiber_generation BLOB NOT NULL CHECK(length(owner_fiber_generation) = 8),
    callback_id BLOB CHECK(callback_id IS NULL OR length(callback_id) = 16),
    state_kind INTEGER NOT NULL,
    receipt_id BLOB NOT NULL CHECK(length(receipt_id) = 16),
    acknowledged INTEGER NOT NULL DEFAULT 0 CHECK(acknowledged IN (0, 1))
) STRICT;

CREATE INDEX operation_outbox_pending ON operation_outbox(acknowledged, sequence);

INSERT INTO operations VALUES (
    X'11111111111111111111111111111111', X'0000000000000001',
    X'12121212121212121212121212121212', X'0000000000000001',
    X'13131313131313131313131313131313', X'0000000000000001',
    X'0000000000000000', 10, X'15151515151515151515151515151515',
    X'14141414141414141414141414141414', X'0000000000000000',
    X'14141414141414141414141414141414', 2
);

INSERT INTO operation_outbox (
    sequence, kind, operation_id, operation_generation, owner_fiber_id,
    owner_fiber_generation, callback_id, state_kind, receipt_id, acknowledged
) VALUES (
    7, 0, X'11111111111111111111111111111111', X'0000000000000001',
    X'12121212121212121212121212121212', X'0000000000000001',
    X'14141414141414141414141414141414', 10,
    X'15151515151515151515151515151515', 0
);
