-- A complete v5 authority database: the v1 golden data (one operation and
-- its outbox row, parked under the v5 one-way schema) plus the v2/v3/v4/v5
-- schema objects exactly as the store's own migrations create them.
-- Opening this file exercises the v5->v6 controlled-unpark upgrade path
-- and nothing else. The parked row is stamped inline (an INSERT may carry
-- the parking columns; the v5 trigger fences only UPDATEs), so the upgrade
-- must backfill its park count to exactly 1 without rewriting any fact.
PRAGMA user_version = 5;

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
    acknowledged INTEGER NOT NULL DEFAULT 0 CHECK(acknowledged IN (0, 1)),
    parked_at_ms INTEGER
        CHECK(parked_at_ms IS NULL OR parked_at_ms >= 0),
    park_reason TEXT
        CHECK(park_reason IS NULL OR (
            length(park_reason) BETWEEN 1 AND 1024
            AND instr(park_reason, char(0)) = 0))
) STRICT;

CREATE INDEX operation_outbox_pending ON operation_outbox(acknowledged, sequence);

CREATE INDEX operation_outbox_by_operation
    ON operation_outbox(operation_id, operation_generation, sequence);

CREATE TABLE idempotent_calls (
    application_id BLOB NOT NULL CHECK(length(application_id) = 16),
    service TEXT NOT NULL
        CHECK(length(service) BETWEEN 1 AND 128 AND instr(service, char(0)) = 0),
    method TEXT NOT NULL
        CHECK(length(method) BETWEEN 1 AND 128 AND instr(method, char(0)) = 0),
    idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
    request_digest_sha256 BLOB NOT NULL CHECK(length(request_digest_sha256) = 32),
    operation_id BLOB NOT NULL CHECK(length(operation_id) = 16),
    operation_generation BLOB NOT NULL CHECK(length(operation_generation) = 8),
    receipt_id BLOB CHECK(receipt_id IS NULL OR length(receipt_id) = 16),
    -- Despite the historical internal column name, this stores only
    -- transport-independent stable service-result bytes.
    response_wire BLOB,
    PRIMARY KEY(application_id, service, method, idempotency_key),
    UNIQUE(operation_id, operation_generation),
    FOREIGN KEY(operation_id) REFERENCES operations(operation_id),
    CHECK((receipt_id IS NULL) = (response_wire IS NULL)),
    CHECK(response_wire IS NULL OR length(response_wire) <= 1048576)
) STRICT;

CREATE TRIGGER idempotent_result_is_immutable
BEFORE UPDATE OF receipt_id, response_wire ON idempotent_calls
WHEN OLD.receipt_id IS NOT NULL AND
     (NEW.receipt_id IS NOT OLD.receipt_id OR
      NEW.response_wire IS NOT OLD.response_wire)
BEGIN
    SELECT RAISE(ABORT, 'idempotent result is immutable');
END;

CREATE TABLE operation_dispatch_preparations (
    operation_id BLOB PRIMARY KEY NOT NULL CHECK(length(operation_id) = 16),
    operation_generation BLOB NOT NULL CHECK(length(operation_generation) = 8),
    owner_fiber_id BLOB NOT NULL CHECK(length(owner_fiber_id) = 16),
    owner_fiber_generation BLOB NOT NULL CHECK(length(owner_fiber_generation) = 8),
    cancellation_scope_id BLOB NOT NULL CHECK(length(cancellation_scope_id) = 16),
    cancellation_generation BLOB NOT NULL CHECK(length(cancellation_generation) = 8),
    callback_id BLOB NOT NULL CHECK(length(callback_id) = 16),
    cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
    preparation_receipt_id BLOB UNIQUE NOT NULL CHECK(length(preparation_receipt_id) = 16),
    FOREIGN KEY(operation_id) REFERENCES operations(operation_id)
) STRICT;

CREATE TABLE operation_dispatch_activation_receipts (
    activation_receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(activation_receipt_id) = 16),
    operation_id BLOB UNIQUE NOT NULL CHECK(length(operation_id) = 16),
    operation_generation BLOB NOT NULL CHECK(length(operation_generation) = 8),
    preparation_receipt_id BLOB UNIQUE NOT NULL CHECK(length(preparation_receipt_id) = 16),
    callback_id BLOB NOT NULL CHECK(length(callback_id) = 16),
    cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
    FOREIGN KEY(operation_id) REFERENCES operations(operation_id),
    FOREIGN KEY(preparation_receipt_id)
        REFERENCES operation_dispatch_preparations(preparation_receipt_id)
) STRICT;

CREATE TRIGGER operation_dispatch_preparations_immutable_update
BEFORE UPDATE ON operation_dispatch_preparations
BEGIN
    SELECT RAISE(ABORT, 'operation dispatch preparation is immutable');
END;

CREATE TRIGGER operation_dispatch_preparations_immutable_delete
BEFORE DELETE ON operation_dispatch_preparations
BEGIN
    SELECT RAISE(ABORT, 'operation dispatch preparation is immutable');
END;

CREATE TRIGGER operation_dispatch_activation_receipts_immutable_update
BEFORE UPDATE ON operation_dispatch_activation_receipts
BEGIN
    SELECT RAISE(ABORT, 'operation dispatch activation receipt is immutable');
END;

CREATE TRIGGER operation_dispatch_activation_receipts_immutable_delete
BEFORE DELETE ON operation_dispatch_activation_receipts
BEGIN
    SELECT RAISE(ABORT, 'operation dispatch activation receipt is immutable');
END;

CREATE TRIGGER operation_outbox_parking_is_one_way
BEFORE UPDATE OF parked_at_ms, park_reason ON operation_outbox
WHEN OLD.parked_at_ms IS NOT NULL
     OR (NEW.parked_at_ms IS NULL) <> (NEW.park_reason IS NULL)
BEGIN
    SELECT RAISE(ABORT, 'operation_outbox parking is one-way');
END;

CREATE INDEX operation_outbox_pending_unparked
   ON operation_outbox(sequence)
   WHERE acknowledged = 0 AND parked_at_ms IS NULL;

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
    owner_fiber_generation, callback_id, state_kind, receipt_id, acknowledged,
    parked_at_ms, park_reason
) VALUES (
    7, 0, X'11111111111111111111111111111111', X'0000000000000001',
    X'12121212121212121212121212121212', X'0000000000000001',
    X'14141414141414141414141414141414', 10,
    X'15151515151515151515151515151515', 0,
    6_000, 'v5 parked: poison head under the one-way schema'
);
