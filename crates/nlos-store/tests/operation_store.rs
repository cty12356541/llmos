use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Barrier;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_operation::{
    CompletionDecision, CompletionOutcome, OperationError, OperationSpec, OperationState,
};
use nlos_runtime::FiberHandle;
use nlos_store::{
    CancelRequestDecision, IdempotencyDecision, IdempotencyScope, OutboxKind, RegistrationDecision,
    SqliteOperationStore, StoreError,
};
use nlos_types::{
    ApplicationId, CallbackId, CancelEpoch, CancellationScopeId, ExecutionFiberId, Generation,
    IdempotencyKey, OperationId, ReceiptId,
};

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-store-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            suffix_path(&self.path, "-wal"),
            suffix_path(&self.path, "-shm"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database: {error}"),
            }
        }
    }
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn bytes(value: u8) -> [u8; 16] {
    [value; 16]
}

fn spec() -> OperationSpec {
    spec_with_operation(1)
}

fn spec_with_operation(value: u8) -> OperationSpec {
    OperationSpec {
        operation_id: OperationId::from_bytes(bytes(value)),
        generation: Generation::INITIAL,
        owner_fiber: FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes(bytes(2)),
            generation: Generation::INITIAL,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes(bytes(3)),
        cancellation_generation: Generation::INITIAL,
    }
}

fn idempotency_scope(method: &str) -> IdempotencyScope {
    IdempotencyScope {
        application_id: ApplicationId::from_bytes(bytes(0xa0)),
        service: "service-directory".to_owned(),
        method: method.to_owned(),
    }
}

#[test]
fn same_key_claim_is_atomic_and_request_digest_conflicts_fail_closed() {
    let database = TestDatabase::new("idempotency-claim");
    let store = SqliteOperationStore::open(&database.path).expect("open");
    let scope = idempotency_scope("resolve");
    let key = IdempotencyKey::from_bytes(bytes(0xb0));
    let digest = [0xc0; 32];

    let first = store
        .begin_idempotent_operation(&scope, key, digest, spec())
        .expect("claim key");
    assert!(matches!(first, IdempotencyDecision::Created(_)));
    assert_eq!(
        store
            .begin_idempotent_operation(&scope, key, digest, spec())
            .expect("replay claim"),
        IdempotencyDecision::PendingOrUncertain(first.operation())
    );
    assert_eq!(
        store
            .inspect_idempotent_operation(&scope, key)
            .expect("inspect original key"),
        Some(IdempotencyDecision::PendingOrUncertain(first.operation()))
    );
    assert!(matches!(
        store.begin_idempotent_operation(&scope, key, [0xc1; 32], spec()),
        Err(StoreError::IdempotencyConflict)
    ));

    let other_scope = idempotency_scope("negotiate");
    assert!(matches!(
        store
            .begin_idempotent_operation(&other_scope, key, digest, spec_with_operation(0x11),)
            .expect("same key in another endpoint scope"),
        IdempotencyDecision::Created(_)
    ));
}

#[test]
fn exact_result_replays_after_reopen_without_redispatch() {
    let database = TestDatabase::new("idempotency-result-reopen");
    let scope = idempotency_scope("resolve");
    let key = IdempotencyKey::from_bytes(bytes(0xb1));
    let digest = [0xc1; 32];
    let response = b"canonical response bytes";
    let expected;
    {
        let store = SqliteOperationStore::open(&database.path).expect("open");
        let operation = store
            .begin_idempotent_operation(&scope, key, digest, spec())
            .expect("claim")
            .operation();
        let ticket = store
            .dispatch(operation, CallbackId::from_bytes(bytes(4)))
            .expect("dispatch");
        expected = store
            .complete_idempotent_operation(
                ticket,
                CompletionOutcome::Completed {
                    receipt_id: ReceiptId::from_bytes(bytes(5)),
                },
                response,
            )
            .expect("complete and persist result");
    }

    let store = SqliteOperationStore::open(&database.path).expect("reopen");
    assert_eq!(
        store
            .begin_idempotent_operation(&scope, key, digest, spec())
            .expect("replay after restart"),
        IdempotencyDecision::Completed(expected)
    );
    assert_eq!(store.pending_outbox(10).expect("outbox").len(), 1);
}

#[test]
fn crash_window_remains_uncertain_and_never_reissues_dispatch_authority() {
    let database = TestDatabase::new("idempotency-uncertain-reopen");
    let scope = idempotency_scope("resolve");
    let key = IdempotencyKey::from_bytes(bytes(0xb2));
    let digest = [0xc2; 32];
    let operation;
    {
        let store = SqliteOperationStore::open(&database.path).expect("open");
        operation = store
            .begin_idempotent_operation(&scope, key, digest, spec())
            .expect("claim")
            .operation();
        store
            .dispatch(operation, CallbackId::from_bytes(bytes(4)))
            .expect("dispatch before simulated crash");
    }

    let store = SqliteOperationStore::open(&database.path).expect("reopen");
    assert_eq!(
        store
            .begin_idempotent_operation(&scope, key, digest, spec())
            .expect("query original key"),
        IdempotencyDecision::PendingOrUncertain(operation)
    );
    assert_eq!(
        store.inspect(operation).expect("inspect").state,
        OperationState::Dispatched
    );
}

#[test]
fn completed_result_is_immutable_even_for_duplicate_callback() {
    let database = TestDatabase::new("idempotency-immutable");
    let store = SqliteOperationStore::open(&database.path).expect("open");
    let scope = idempotency_scope("resolve");
    let operation = store
        .begin_idempotent_operation(
            &scope,
            IdempotencyKey::from_bytes(bytes(0xb3)),
            [0xc3; 32],
            spec(),
        )
        .expect("claim")
        .operation();
    let ticket = store
        .dispatch(operation, CallbackId::from_bytes(bytes(4)))
        .expect("dispatch");
    let outcome = CompletionOutcome::Completed {
        receipt_id: ReceiptId::from_bytes(bytes(5)),
    };
    let first = store
        .complete_idempotent_operation(ticket, outcome, b"result-v1")
        .expect("first completion");
    assert_eq!(
        store
            .complete_idempotent_operation(ticket, outcome, b"result-v1")
            .expect("exact duplicate"),
        first
    );
    assert!(matches!(
        store.complete_idempotent_operation(ticket, outcome, b"result-v2"),
        Err(StoreError::IdempotencyConflict)
    ));
    assert_eq!(store.pending_outbox(10).expect("outbox").len(), 1);
}

#[test]
fn idempotent_cancel_before_dispatch_commits_no_effect_result_atomically() {
    let database = TestDatabase::new("idempotency-cancel-before-dispatch");
    let scope = idempotency_scope("deadline-before-dispatch");
    let key = IdempotencyKey::from_bytes(bytes(0xb5));
    let digest = [0xc5; 32];
    let receipt = ReceiptId::from_bytes(bytes(6));
    let expected;
    let operation;
    {
        let store = SqliteOperationStore::open(&database.path).expect("open");
        operation = store
            .begin_idempotent_operation(&scope, key, digest, spec())
            .expect("claim")
            .operation();
        expected = store
            .cancel_idempotent_before_dispatch(operation, receipt, b"deadline")
            .expect("cancel and persist no-effect result");
        assert_eq!(
            store.inspect(operation).expect("inspect").state,
            OperationState::CancelledBeforeEffect {
                receipt_id: receipt,
            }
        );
        assert!(matches!(
            store.dispatch(operation, CallbackId::from_bytes(bytes(4))),
            Err(StoreError::Operation(OperationError::InvalidState))
        ));
        let pending = store.pending_outbox(10).expect("outbox");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, OutboxKind::WakeFiber);
        assert_eq!(pending[0].callback_id, None);
    }

    let store = SqliteOperationStore::open(&database.path).expect("reopen");
    assert_eq!(
        store
            .begin_idempotent_operation(&scope, key, digest, spec())
            .expect("replay claim"),
        IdempotencyDecision::Completed(expected.clone())
    );
    assert_eq!(
        store
            .cancel_idempotent_before_dispatch(operation, receipt, b"deadline")
            .expect("exact cancel retry"),
        expected
    );
    assert!(matches!(
        store.cancel_idempotent_before_dispatch(operation, receipt, b"different"),
        Err(StoreError::IdempotencyConflict)
    ));
}

#[test]
fn cancel_control_is_epoch_fenced_idempotent_and_does_not_rewrite_completion() {
    let database = TestDatabase::new("cancel-control");
    let store = SqliteOperationStore::open(&database.path).expect("open");
    let receipt = ReceiptId::from_bytes(bytes(0x61));

    let registered = store.register(spec()).expect("register").handle();
    let first = store
        .request_cancel_idempotent(registered, CancelEpoch::INITIAL, receipt)
        .expect("first cancel");
    assert!(matches!(first, CancelRequestDecision::Applied(_)));
    assert_eq!(first.snapshot().cancel_epoch, CancelEpoch::new(1));
    assert!(matches!(
        first.snapshot().state,
        OperationState::CancelledBeforeEffect { receipt_id } if receipt_id == receipt
    ));
    let replay = store
        .request_cancel_idempotent(registered, CancelEpoch::INITIAL, receipt)
        .expect("exact retry");
    assert!(matches!(replay, CancelRequestDecision::Replayed(_)));
    assert_eq!(store.pending_outbox(10).expect("outbox").len(), 1);
    assert!(matches!(
        store
            .request_cancel_idempotent(registered, CancelEpoch::new(1), receipt)
            .expect("terminal cancel remains a read-only result"),
        CancelRequestDecision::AlreadyTerminal(_)
    ));
    assert_eq!(
        store
            .pending_outbox(10)
            .expect("outbox remains single")
            .len(),
        1
    );

    let completed = store
        .register(spec_with_operation(0x12))
        .expect("register completed operation")
        .handle();
    let ticket = store
        .dispatch(completed, CallbackId::from_bytes(bytes(0x13)))
        .expect("dispatch");
    store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(bytes(0x14)),
            },
        )
        .expect("complete");
    let late = store
        .request_cancel_idempotent(completed, CancelEpoch::INITIAL, receipt)
        .expect("late cancel reads winner");
    assert!(matches!(late, CancelRequestDecision::AlreadyTerminal(_)));
    assert!(matches!(
        late.snapshot().state,
        OperationState::Completed { .. }
    ));
}

#[test]
fn dispatched_cancel_control_advances_epoch_once() {
    let database = TestDatabase::new("cancel-control-dispatched");
    let store = SqliteOperationStore::open(&database.path).expect("open");
    let operation = store.register(spec()).expect("register").handle();
    store
        .dispatch(operation, CallbackId::from_bytes(bytes(0x21)))
        .expect("dispatch");

    let first = store
        .request_cancel_idempotent(
            operation,
            CancelEpoch::INITIAL,
            ReceiptId::from_bytes(bytes(0x22)),
        )
        .expect("cancel dispatched operation");
    assert!(matches!(first, CancelRequestDecision::Applied(_)));
    assert_eq!(first.snapshot().state, OperationState::CancelRequested);
    assert_eq!(first.snapshot().cancel_epoch, CancelEpoch::new(1));
    let replay = store
        .request_cancel_idempotent(
            operation,
            CancelEpoch::INITIAL,
            ReceiptId::from_bytes(bytes(0x22)),
        )
        .expect("retry cancel");
    assert!(matches!(replay, CancelRequestDecision::Replayed(_)));
    assert!(store.pending_outbox(10).expect("outbox").is_empty());
}

#[test]
fn idempotency_scope_and_result_size_are_bounded_before_writing() {
    let database = TestDatabase::new("idempotency-bounds");
    let store = SqliteOperationStore::open(&database.path).expect("open");
    let invalid_scope = IdempotencyScope {
        application_id: ApplicationId::from_bytes(bytes(0xa0)),
        service: String::new(),
        method: "resolve".to_owned(),
    };
    assert!(matches!(
        store.begin_idempotent_operation(
            &invalid_scope,
            IdempotencyKey::from_bytes(bytes(0xb4)),
            [0xc4; 32],
            spec(),
        ),
        Err(StoreError::InvalidIdempotencyScope)
    ));

    let operation = store
        .begin_idempotent_operation(
            &idempotency_scope("resolve"),
            IdempotencyKey::from_bytes(bytes(0xb4)),
            [0xc4; 32],
            spec(),
        )
        .expect("claim valid scope")
        .operation();
    let ticket = store
        .dispatch(operation, CallbackId::from_bytes(bytes(4)))
        .expect("dispatch");
    let oversized_result = vec![0; 1024 * 1024 + 1];
    assert!(matches!(
        store.complete_idempotent_operation(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(bytes(5)),
            },
            &oversized_result,
        ),
        Err(StoreError::DurableResultTooLarge { .. })
    ));
    assert_eq!(
        store
            .inspect(operation)
            .expect("oversize rolled back")
            .state,
        OperationState::Dispatched
    );
}

#[test]
fn exact_registration_is_idempotent_but_conflicting_reuse_is_rejected() {
    let database = TestDatabase::new("registration");
    let store = SqliteOperationStore::open(&database.path).expect("open");
    let first = store.register(spec()).expect("register");
    let second = store.register(spec()).expect("register duplicate");
    assert!(matches!(first, RegistrationDecision::Created(_)));
    assert_eq!(second, RegistrationDecision::Existing(first.handle()));

    let mut conflicting = spec();
    conflicting.owner_fiber.generation = Generation::INITIAL.checked_next().expect("generation");
    assert!(matches!(
        store.register(conflicting),
        Err(StoreError::Operation(OperationError::DuplicateOperation))
    ));
}

#[test]
fn completion_and_wake_outbox_survive_reopen_and_duplicate_callback() {
    let database = TestDatabase::new("reopen");
    let ticket;
    let handle;
    {
        let store = SqliteOperationStore::open(&database.path).expect("open");
        handle = store.register(spec()).expect("register").handle();
        ticket = store
            .dispatch(handle, CallbackId::from_bytes(bytes(4)))
            .expect("dispatch");
        assert_eq!(
            store
                .complete(
                    ticket,
                    CompletionOutcome::Completed {
                        receipt_id: ReceiptId::from_bytes(bytes(5)),
                    },
                )
                .expect("complete"),
            CompletionDecision::CanonicalizedAndWake {
                state: OperationState::Completed {
                    receipt_id: ReceiptId::from_bytes(bytes(5)),
                },
            }
        );
    }

    let store = SqliteOperationStore::open(&database.path).expect("reopen");
    assert_eq!(
        store.inspect(handle).expect("inspect").state,
        OperationState::Completed {
            receipt_id: ReceiptId::from_bytes(bytes(5)),
        }
    );
    let pending = store.pending_outbox(10).expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, OutboxKind::WakeFiber);
    assert_eq!(pending[0].callback_id, Some(ticket.callback_id));

    assert!(matches!(
        store.complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(bytes(5)),
            },
        ),
        Ok(CompletionDecision::Duplicate { .. })
    ));
    assert_eq!(store.pending_outbox(10).expect("pending").len(), 1);

    store
        .acknowledge_outbox(pending[0].sequence)
        .expect("acknowledge");
    store
        .acknowledge_outbox(pending[0].sequence)
        .expect("idempotent acknowledge");
    assert!(store.pending_outbox(10).expect("pending").is_empty());
}

#[test]
fn late_callback_is_durably_routed_to_reconciliation() {
    let database = TestDatabase::new("late");
    let store = SqliteOperationStore::open(&database.path).expect("open");
    let handle = store.register(spec()).expect("register").handle();
    let ticket = store
        .dispatch(handle, CallbackId::from_bytes(bytes(4)))
        .expect("dispatch");
    store
        .request_cancel(handle, ReceiptId::from_bytes(bytes(6)))
        .expect("cancel");

    let decision = store
        .complete(
            ticket,
            CompletionOutcome::PartialEffect {
                receipt_id: ReceiptId::from_bytes(bytes(7)),
            },
        )
        .expect("complete");
    assert!(matches!(
        decision,
        CompletionDecision::CanonicalizedForReconciliation { .. }
    ));
    let pending = store.pending_outbox(10).expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, OutboxKind::ReconcileEffect);
}

#[test]
fn cancel_before_dispatch_commits_terminal_state_and_wake_together() {
    let database = TestDatabase::new("cancel-before-dispatch");
    let store = SqliteOperationStore::open(&database.path).expect("open");
    let handle = store.register(spec()).expect("register").handle();
    let snapshot = store
        .request_cancel(handle, ReceiptId::from_bytes(bytes(6)))
        .expect("cancel");

    assert!(matches!(
        snapshot.state,
        OperationState::CancelledBeforeEffect { .. }
    ));
    let pending = store.pending_outbox(10).expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, OutboxKind::WakeFiber);
    assert_eq!(pending[0].callback_id, None);
}

#[test]
fn forged_callback_rolls_back_without_outbox_or_state_change() {
    let database = TestDatabase::new("forged-callback");
    let store = SqliteOperationStore::open(&database.path).expect("open");
    let handle = store.register(spec()).expect("register").handle();
    let mut ticket = store
        .dispatch(handle, CallbackId::from_bytes(bytes(4)))
        .expect("dispatch");
    ticket.callback_id = CallbackId::from_bytes(bytes(8));

    assert!(matches!(
        store.complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(bytes(5)),
            },
        ),
        Err(StoreError::Operation(OperationError::InvalidGeneration))
    ));
    assert_eq!(
        store.inspect(handle).expect("inspect").state,
        OperationState::Dispatched
    );
    assert!(store.pending_outbox(10).expect("pending").is_empty());
}

#[test]
fn acknowledged_completion_survives_abrupt_process_exit() {
    let database = TestDatabase::new("abrupt-exit");
    let status = Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_writer_helper", "--nocapture"])
        .env("NLOS_CRASH_WRITER_DATABASE", &database.path)
        .status()
        .expect("run crash writer");
    assert_eq!(status.code(), Some(97));

    let store = SqliteOperationStore::open(&database.path).expect("reopen after crash");
    let handle = store
        .register(spec())
        .expect("idempotent register")
        .handle();
    assert_eq!(
        store.inspect(handle).expect("inspect").state,
        OperationState::Completed {
            receipt_id: ReceiptId::from_bytes(bytes(5)),
        }
    );
    assert_eq!(store.pending_outbox(10).expect("pending").len(), 1);
}

#[test]
fn crash_writer_helper() {
    let Ok(path) = std::env::var("NLOS_CRASH_WRITER_DATABASE") else {
        return;
    };
    let store = SqliteOperationStore::open(path).expect("open crash writer");
    let handle = store.register(spec()).expect("register").handle();
    let ticket = store
        .dispatch(handle, CallbackId::from_bytes(bytes(4)))
        .expect("dispatch");
    store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(bytes(5)),
            },
        )
        .expect("durable completion acknowledgement");
    std::process::exit(97);
}

#[test]
fn durable_cancel_and_completion_race_is_serialized() {
    let database = TestDatabase::new("cancel-complete-race");
    let store = SqliteOperationStore::open(&database.path).expect("open");

    for operation_byte in 20..84 {
        let handle = store
            .register(spec_with_operation(operation_byte))
            .expect("register")
            .handle();
        let ticket = store
            .dispatch(handle, CallbackId::from_bytes(bytes(operation_byte + 80)))
            .expect("dispatch");
        let barrier = Barrier::new(3);

        std::thread::scope(|scope| {
            let cancel = scope.spawn(|| {
                barrier.wait();
                store.request_cancel(handle, ReceiptId::from_bytes(bytes(6)))
            });
            let complete = scope.spawn(|| {
                barrier.wait();
                store.complete(
                    ticket,
                    CompletionOutcome::Completed {
                        receipt_id: ReceiptId::from_bytes(bytes(5)),
                    },
                )
            });
            barrier.wait();

            let cancel_result = cancel.join().expect("cancel thread");
            let completion_result = complete.join().expect("completion thread");
            match (cancel_result, completion_result) {
                (Ok(snapshot), Ok(CompletionDecision::CanonicalizedForReconciliation { .. })) => {
                    assert_eq!(snapshot.state, OperationState::CancelRequested);
                }
                (
                    Err(StoreError::Operation(OperationError::InvalidState)),
                    Ok(CompletionDecision::CanonicalizedAndWake { .. }),
                ) => {}
                unexpected => panic!("non-serializable durable race: {unexpected:?}"),
            }
        });
    }
}
