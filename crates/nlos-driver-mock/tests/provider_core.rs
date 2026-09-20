//! Deterministic fake provider core: register/dispatch/complete full chain
//! against the durable `SqliteOperationStore` prepare→activate boundary, with
//! exact restart replay and the fail-closed negative transitions. Runs on
//! every platform; the authenticated IPC face is covered separately.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_driver_mock::provider::{
    CompleteProviderOperation, DispatchProviderOperation, MockProvider, ProviderError,
    derive_provider_outcome,
};
use nlos_operation::{CompletionOutcome, OperationHandle, OperationSpec};
use nlos_runtime::FiberHandle;
use nlos_store::{RegistrationDecision, SqliteOperationStore};
use nlos_types::{
    CallbackId, CancellationScopeId, ExecutionFiberId, Generation, OperationId, ReceiptId,
};

struct Root(PathBuf);

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

impl Root {
    fn new(label: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = Self(std::env::temp_dir().join(format!(
            "nlos-driver-mock-core-{label}-{}-{sequence}",
            std::process::id()
        )));
        fs::create_dir_all(root.path()).unwrap();
        root
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn operation_id(seed: u8) -> OperationId {
    OperationId::from_bytes([seed; 16])
}

fn callback_id(seed: u8) -> CallbackId {
    CallbackId::from_bytes([seed; 16])
}

fn owner_fiber() -> FiberHandle {
    FiberHandle {
        fiber_id: ExecutionFiberId::from_bytes([0x21; 16]),
        generation: Generation::INITIAL,
    }
}

fn spec(seed: u8) -> OperationSpec {
    OperationSpec {
        operation_id: operation_id(seed),
        generation: Generation::INITIAL,
        owner_fiber: owner_fiber(),
        cancellation_scope_id: CancellationScopeId::from_bytes([0x22; 16]),
        cancellation_generation: Generation::INITIAL,
    }
}

fn provider(root: &Root, sub: &str) -> MockProvider {
    MockProvider::new(Arc::new(
        SqliteOperationStore::open(root.path().join(sub)).unwrap(),
    ))
}

#[test]
fn register_dispatch_complete_full_chain_is_durable() {
    let root = Root::new("chain");
    let provider = provider(&root, "ops");

    let registered = provider.register(spec(0x31)).unwrap();
    assert!(!registered.replayed);
    assert_eq!(registered.handle.operation_id, operation_id(0x31));
    assert_ne!(
        registered.admission_receipt_id,
        ReceiptId::from_bytes([0; 16]),
        "authority-derived admission receipt must be non-zero"
    );

    let dispatch = provider
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x41),
        })
        .unwrap();
    assert!(!dispatch.replayed);
    assert_eq!(dispatch.ticket.callback_id, callback_id(0x41));
    assert_eq!(dispatch.ticket.operation, registered.handle);
    assert_ne!(
        dispatch.preparation_receipt_id,
        dispatch.activation_receipt_id
    );

    let seed = [0x77; 32];
    let completion = provider
        .complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x41),
            seed,
        })
        .unwrap();
    assert!(!completion.replayed);
    let expected = derive_provider_outcome(registered.handle, callback_id(0x41), &seed);
    assert_eq!(
        completion.outcome, expected,
        "terminal outcome must be the deterministic seed-derived outcome"
    );
    assert!(completion.state.is_terminal());

    let snapshot = provider.store().inspect(registered.handle).unwrap();
    assert_eq!(snapshot.state, completion.state);
}

#[test]
fn restart_replays_in_flight_dispatch_exactly() {
    let root = Root::new("inflight");
    let database = root.path().join("ops");
    let first = MockProvider::new(Arc::new(SqliteOperationStore::open(&database).unwrap()));
    let registered = first.register(spec(0x32)).unwrap();
    let dispatched = first
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x42),
        })
        .unwrap();
    assert!(!dispatched.replayed);

    // Provider process drop: store and provider are gone; only the durable
    // database survives.
    drop(first);

    let second = MockProvider::new(Arc::new(SqliteOperationStore::open(&database).unwrap()));
    let replayed = second
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x42),
        })
        .unwrap();
    assert!(replayed.replayed, "restart must replay, not re-issue");
    assert_eq!(replayed.ticket, dispatched.ticket);
    assert_eq!(
        replayed.preparation_receipt_id, dispatched.preparation_receipt_id,
        "preparation receipt must be stable across restart"
    );
    assert_eq!(
        replayed.activation_receipt_id, dispatched.activation_receipt_id,
        "activation receipt must be stable across restart"
    );

    let seed = [0x88; 32];
    let completion = second
        .complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x42),
            seed,
        })
        .unwrap();
    assert!(!completion.replayed);
    assert!(completion.state.is_terminal());
}

#[test]
fn restart_replays_completed_operation_idempotently() {
    let root = Root::new("terminal");
    let database = root.path().join("ops");
    let request = || CompleteProviderOperation {
        handle: nlos_operation::OperationHandle {
            operation_id: operation_id(0x33),
            generation: Generation::INITIAL,
        },
        callback_id: callback_id(0x43),
        seed: [0x99; 32],
    };
    let first = MockProvider::new(Arc::new(SqliteOperationStore::open(&database).unwrap()));
    let registered = first.register(spec(0x33)).unwrap();
    first
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x43),
        })
        .unwrap();
    let completion = first.complete(request()).unwrap();
    drop(first);

    let second = MockProvider::new(Arc::new(SqliteOperationStore::open(&database).unwrap()));
    let replayed_register = second.register(spec(0x33)).unwrap();
    assert!(replayed_register.replayed);
    let replayed_dispatch = second
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x43),
        })
        .unwrap();
    assert!(replayed_dispatch.replayed);
    let replayed_completion = second.complete(request()).unwrap();
    assert!(replayed_completion.replayed);
    assert_eq!(replayed_completion.outcome, completion.outcome);
    assert_eq!(replayed_completion.state, completion.state);
}

#[test]
fn outcome_derivation_is_seed_deterministic() {
    let handle = OperationHandle {
        operation_id: operation_id(0x34),
        generation: Generation::INITIAL,
    };
    let seed = [0xAB; 32];
    assert_eq!(
        derive_provider_outcome(handle, callback_id(0x44), &seed),
        derive_provider_outcome(handle, callback_id(0x44), &seed)
    );
    let receipt_of = |outcome: CompletionOutcome| match outcome {
        CompletionOutcome::Completed { receipt_id }
        | CompletionOutcome::Failed { receipt_id }
        | CompletionOutcome::PartialEffect { receipt_id }
        | CompletionOutcome::EffectUnknown { receipt_id }
        | CompletionOutcome::CancelledBeforeEffect { receipt_id } => receipt_id,
    };
    let receipt = receipt_of(derive_provider_outcome(handle, callback_id(0x44), &seed));
    let other_seed = [0xAC; 32];
    let other_receipt = receipt_of(derive_provider_outcome(
        handle,
        callback_id(0x44),
        &other_seed,
    ));
    assert_ne!(receipt, other_receipt);
}

#[test]
fn register_conflicting_spec_reuse_is_rejected() {
    let root = Root::new("conflict");
    let provider = provider(&root, "ops");
    provider.register(spec(0x35)).unwrap();
    let mut conflicting = spec(0x35);
    conflicting.owner_fiber = FiberHandle {
        fiber_id: ExecutionFiberId::from_bytes([0x99; 16]),
        generation: Generation::INITIAL,
    };
    assert!(matches!(
        provider.register(conflicting),
        Err(ProviderError::Store(nlos_store::StoreError::Operation(
            nlos_operation::OperationError::DuplicateOperation
        )))
    ));
}

#[test]
fn complete_before_dispatch_is_rejected() {
    let root = Root::new("undispatched");
    let provider = provider(&root, "ops");
    let registered = provider.register(spec(0x36)).unwrap();
    assert!(matches!(
        provider.complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x46),
            seed: [0x11; 32],
        }),
        // W30-C: the completion ticket is reconstructed from the durable
        // dispatch activation, so an undispatched operation fails on the
        // missing activation proof (still a typed fail-closed rejection
        // with zero partial state).
        Err(ProviderError::Store(
            nlos_store::StoreError::DispatchPreparationNotFound
        ))
    ));
}

#[test]
fn complete_with_forged_callback_is_rejected() {
    let root = Root::new("forged");
    let provider = provider(&root, "ops");
    let registered = provider.register(spec(0x37)).unwrap();
    provider
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x47),
        })
        .unwrap();
    assert!(matches!(
        provider.complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x5E),
            seed: [0x12; 32],
        }),
        Err(ProviderError::Store(nlos_store::StoreError::Operation(
            nlos_operation::OperationError::InvalidGeneration
        )))
    ));
}

#[test]
fn complete_with_different_seed_after_terminal_is_callback_conflict() {
    let root = Root::new("reseed");
    let provider = provider(&root, "ops");
    let registered = provider.register(spec(0x38)).unwrap();
    provider
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x48),
        })
        .unwrap();
    provider
        .complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x48),
            seed: [0x13; 32],
        })
        .unwrap();
    assert!(matches!(
        provider.complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x48),
            seed: [0x14; 32],
        }),
        Err(ProviderError::Store(nlos_store::StoreError::Operation(
            nlos_operation::OperationError::CallbackIdentityConflict
        )))
    ));
}

#[test]
fn register_exact_replay_reports_existing_without_transition() {
    let root = Root::new("exact");
    let provider = provider(&root, "ops");
    let first = provider.register(spec(0x39)).unwrap();
    let second = provider.register(spec(0x39)).unwrap();
    assert!(second.replayed);
    assert_eq!(first.handle, second.handle);
    assert_eq!(
        first.admission_receipt_id, second.admission_receipt_id,
        "admission receipt is derived from the immutable registration row"
    );
    assert!(matches!(
        provider.store().register(spec(0x39)),
        Ok(RegistrationDecision::Existing(_))
    ));
}
