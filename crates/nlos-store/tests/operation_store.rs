use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Barrier;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_operation::{
    CompletionDecision, CompletionOutcome, OperationError, OperationSpec, OperationState,
};
use nlos_runtime::FiberHandle;
use nlos_store::{OutboxKind, RegistrationDecision, SqliteOperationStore, StoreError};
use nlos_types::{
    CallbackId, CancellationScopeId, ExecutionFiberId, Generation, OperationId, ReceiptId,
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
