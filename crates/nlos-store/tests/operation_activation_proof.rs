use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_operation::{OperationSpec, OperationState};
use nlos_runtime::FiberHandle;
use nlos_store::{
    OperationActivationDecision, OperationPrepareDecision, SqliteOperationStore, StoreError,
};
use nlos_types::{CallbackId, CancellationScopeId, ExecutionFiberId, Generation, OperationId};

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new() -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-store-operation-activation-proof-{}-{sequence}.sqlite3",
                std::process::id()
            )),
        }
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

fn operation_spec() -> OperationSpec {
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
fn activation_proof_requires_owner_activation_and_replays_after_restart() {
    let database = TestDatabase::new();
    let handle;
    let preparation;
    let expected_proof;
    {
        let store = SqliteOperationStore::open(&database.path).expect("open");
        handle = store.register(operation_spec()).expect("register").handle();

        assert!(matches!(
            store.inspect_activation_proof(handle),
            Err(StoreError::DispatchPreparationNotFound)
        ));
        preparation = match store
            .prepare_dispatch(handle, CallbackId::from_bytes(bytes(4)))
            .expect("prepare")
        {
            OperationPrepareDecision::Prepared(preparation) => preparation,
            OperationPrepareDecision::Replayed(_) => panic!("first prepare replayed"),
        };
        assert!(matches!(
            store.inspect_activation_proof(handle),
            Err(StoreError::OperationNotActivated)
        ));
        assert_eq!(
            store.inspect(handle).expect("prepared inspect").state,
            OperationState::Registered
        );

        let activation = match store.activate_dispatch(preparation).expect("activate") {
            OperationActivationDecision::Activated(activation) => activation,
            OperationActivationDecision::Replayed(_) => panic!("first activation replayed"),
        };
        expected_proof = store.inspect_activation_proof(handle).expect("proof");
        assert_eq!(expected_proof.operation, handle);
        assert_eq!(
            expected_proof.preparation_receipt_id,
            preparation.preparation_receipt_id
        );
        assert_eq!(
            expected_proof.activation_receipt_id,
            activation.activation_receipt_id
        );
        assert_eq!(expected_proof.callback_id, preparation.callback_id);
        assert_eq!(expected_proof.cancel_epoch, preparation.cancel_epoch);
        assert_eq!(
            store.inspect(handle).expect("activated inspect").state,
            OperationState::Dispatched
        );
    }

    let store = SqliteOperationStore::open(&database.path).expect("reopen");
    assert_eq!(
        store
            .inspect_activation_proof(handle)
            .expect("replay proof"),
        expected_proof
    );
    store
        .request_cancel(handle, nlos_types::ReceiptId::from_bytes(bytes(9)))
        .expect("cancel");
    assert_eq!(
        store
            .inspect_activation_proof(handle)
            .expect("terminal activation remains durable"),
        expected_proof
    );
    assert!(matches!(
        store.inspect_activation_proof(nlos_operation::OperationHandle {
            operation_id: handle.operation_id,
            generation: handle.generation.checked_next().expect("next generation"),
        }),
        Err(StoreError::Operation(
            nlos_operation::OperationError::InvalidGeneration
        ))
    ));
}
