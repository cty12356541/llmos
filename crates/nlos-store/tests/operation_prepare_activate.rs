use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_operation::{OperationError, OperationSpec, OperationState};
use nlos_runtime::FiberHandle;
use nlos_store::{
    OperationActivationDecision, OperationDispatchPreparation, OperationPrepareDecision,
    SqliteOperationStore, StoreError,
};
use nlos_types::{CallbackId, CancellationScopeId, ExecutionFiberId, Generation, OperationId};

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
    OperationSpec {
        operation_id: OperationId::from_bytes(bytes(1)),
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
fn operation_prepare_activate_replays_owner_facts_after_restart() {
    let database = TestDatabase::new("operation-prepare-activate");
    let handle;
    let preparation;
    let activation;
    {
        let store = SqliteOperationStore::open(&database.path).expect("open");
        handle = store.register(spec()).expect("register").handle();
        let callback_id = CallbackId::from_bytes(bytes(4));

        preparation = match store
            .prepare_dispatch(handle, callback_id)
            .expect("prepare dispatch")
        {
            OperationPrepareDecision::Prepared(preparation) => preparation,
            OperationPrepareDecision::Replayed(_) => panic!("first prepare was replayed"),
        };
        assert_eq!(preparation.operation, handle);
        assert_eq!(preparation.owner_fiber, spec().owner_fiber);
        assert_eq!(preparation.cancel_epoch.get(), 0);
        assert_ne!(preparation.preparation_receipt_id.into_bytes(), [0; 16]);
        assert_eq!(
            store
                .inspect(handle)
                .expect("inspect prepared operation")
                .state,
            OperationState::Registered
        );

        assert!(matches!(
            store.dispatch(handle, CallbackId::from_bytes(bytes(5))),
            Err(StoreError::Operation(OperationError::InvalidState))
        ));

        activation = match store
            .activate_dispatch(preparation)
            .expect("activate dispatch")
        {
            OperationActivationDecision::Activated(activation) => activation,
            OperationActivationDecision::Replayed(_) => panic!("first activation was replayed"),
        };
        assert_eq!(activation.preparation, preparation);
        assert_eq!(activation.ticket.callback_id, callback_id);
        assert_eq!(activation.ticket.operation, handle);
        assert_eq!(activation.ticket.owner_fiber, preparation.owner_fiber);
        assert_eq!(activation.ticket.cancel_epoch, preparation.cancel_epoch);
        assert_ne!(activation.activation_receipt_id.into_bytes(), [0; 16]);
        assert_eq!(
            store
                .inspect(handle)
                .expect("inspect activated operation")
                .state,
            OperationState::Dispatched
        );
    }

    let store = SqliteOperationStore::open(&database.path).expect("reopen");
    assert_eq!(
        store
            .prepare_dispatch(handle, preparation.callback_id)
            .expect("prepare replay after restart"),
        OperationPrepareDecision::Replayed(preparation)
    );
    assert_eq!(
        store
            .activate_dispatch(preparation)
            .expect("activation replay after restart"),
        OperationActivationDecision::Replayed(activation)
    );
    assert_eq!(
        store
            .inspect(handle)
            .expect("inspect replayed operation")
            .state,
        OperationState::Dispatched
    );
    store
        .request_cancel(handle, nlos_types::ReceiptId::from_bytes(bytes(10)))
        .expect("cancel activated operation");
    assert_eq!(
        store
            .activate_dispatch(preparation)
            .expect("activation replay after later cancel"),
        OperationActivationDecision::Replayed(activation)
    );
    assert_eq!(
        store
            .prepare_dispatch(handle, preparation.callback_id)
            .expect("preparation replay after later cancel"),
        OperationPrepareDecision::Replayed(preparation)
    );
}

#[test]
fn operation_prepare_conflicts_and_cancel_fences_activation() {
    let database = TestDatabase::new("operation-prepare-conflict");
    let store = SqliteOperationStore::open(&database.path).expect("open");
    let handle = store.register(spec()).expect("register").handle();
    let preparation = match store
        .prepare_dispatch(handle, CallbackId::from_bytes(bytes(6)))
        .expect("prepare dispatch")
    {
        OperationPrepareDecision::Prepared(preparation) => preparation,
        OperationPrepareDecision::Replayed(_) => panic!("first prepare was replayed"),
    };

    assert!(matches!(
        store.prepare_dispatch(handle, CallbackId::from_bytes(bytes(7))),
        Err(StoreError::DispatchPreparationConflict)
    ));

    let mut forged = preparation;
    forged.callback_id = CallbackId::from_bytes(bytes(8));
    assert!(matches!(
        store.activate_dispatch(forged),
        Err(StoreError::DispatchPreparationConflict)
    ));

    let stale = nlos_operation::OperationHandle {
        operation_id: handle.operation_id,
        generation: handle.generation.checked_next().expect("next generation"),
    };
    let mut stale_preparation: OperationDispatchPreparation = preparation;
    stale_preparation.operation = stale;
    assert!(matches!(
        store.activate_dispatch(stale_preparation),
        Err(StoreError::Operation(OperationError::InvalidGeneration))
    ));

    store
        .request_cancel(handle, nlos_types::ReceiptId::from_bytes(bytes(9)))
        .expect("cancel prepared operation");
    assert!(matches!(
        store.activate_dispatch(preparation),
        Err(StoreError::Operation(OperationError::InvalidState))
    ));
    assert_eq!(
        store
            .inspect(handle)
            .expect("inspect cancelled operation")
            .state,
        OperationState::CancelledBeforeEffect {
            receipt_id: nlos_types::ReceiptId::from_bytes(bytes(9))
        }
    );
}
