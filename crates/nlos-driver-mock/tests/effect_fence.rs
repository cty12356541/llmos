//! Speculative side-effect fence (W30-C, ROAD-B-003 sixth gate): effects
//! speculatively attempted during a degraded/uncertain window are fenced by
//! the durable generation and cancel-epoch machinery, never by optimism.
//!
//! * a speculative dispatch carrying a stale generation is typed-rejected
//!   with zero durable trace — no preparation, no activation, no outbox
//!   evidence — so no un-fenced effect boundary ever opens;
//! * a cancel that advances the epoch during the degraded window wins over
//!   the provider's late speculative completion: the completion is
//!   canonicalized for reconciliation only (the wake stays fenced) and the
//!   callback identity fence still holds afterwards;
//! * across a provider restart inside the fence window the terminal state
//!   commits exactly once — the replayed completion is a duplicate and a
//!   different seed stays a `CallbackIdentityConflict`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_driver_mock::cache::ProviderCache;
use nlos_driver_mock::provider::{
    CompleteProviderOperation, DispatchProviderOperation, ProviderError, ProviderFaultMode,
};
use nlos_operation::{OperationHandle, OperationSpec, OperationState};
use nlos_runtime::FiberHandle;
use nlos_store::{CancelRequestDecision, OutboxKind, SqliteOperationStore};
use nlos_types::{
    CallbackId, CancelEpoch, CancellationScopeId, ExecutionFiberId, Generation, OperationId,
    ReceiptId,
};

struct Root(PathBuf);

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

impl Root {
    fn new(label: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = Self(std::env::temp_dir().join(format!(
            "nlos-driver-mock-fence-{label}-{}-{sequence}",
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
        fiber_id: ExecutionFiberId::from_bytes([0x81; 16]),
        generation: Generation::INITIAL,
    }
}

fn spec(seed: u8) -> OperationSpec {
    OperationSpec {
        operation_id: operation_id(seed),
        generation: Generation::INITIAL,
        owner_fiber: owner_fiber(),
        cancellation_scope_id: CancellationScopeId::from_bytes([0x82; 16]),
        cancellation_generation: Generation::INITIAL,
    }
}

fn cache(root: &Root, sub: &str) -> ProviderCache {
    ProviderCache::new(Arc::new(nlos_driver_mock::MockProvider::new(Arc::new(
        SqliteOperationStore::open(root.path().join(sub)).unwrap(),
    ))))
}

fn outbox_for(store: &SqliteOperationStore, operation: OperationHandle) -> Vec<OutboxKind> {
    store
        .pending_outbox(64)
        .unwrap()
        .into_iter()
        .filter(|entry| entry.operation == operation)
        .map(|entry| entry.kind)
        .collect()
}

#[test]
fn stale_generation_speculative_dispatch_is_typed_rejected_without_effect_commit() {
    let root = Root::new("stale");
    let cache = cache(&root, "ops");
    let provider = cache.provider();
    let registered = cache.register(spec(0x91)).unwrap();
    let stale = OperationHandle {
        operation_id: registered.handle.operation_id,
        generation: Generation::new(std::num::NonZeroU64::new(2).unwrap()),
    };

    provider.arm_fault(ProviderFaultMode::FailProviderRpc);
    assert!(matches!(
        cache.dispatch(DispatchProviderOperation {
            handle: stale,
            callback_id: callback_id(0x92),
        }),
        Err(ProviderError::Unreachable)
    ));
    provider.disarm_fault();

    // The provider returned; the retried speculative dispatch still carries
    // a stale generation and must be typed-rejected.
    assert!(matches!(
        cache.dispatch(DispatchProviderOperation {
            handle: stale,
            callback_id: callback_id(0x92),
        }),
        Err(ProviderError::Store(nlos_store::StoreError::Operation(
            nlos_operation::OperationError::InvalidGeneration
        )))
    ));

    let store = provider.store();
    assert_eq!(
        store.inspect(registered.handle).unwrap().state,
        OperationState::Registered,
        "the rejected speculative dispatch must not transition the operation"
    );
    assert!(
        outbox_for(store, registered.handle).is_empty(),
        "no effect evidence may exist for a rejected speculative dispatch"
    );
    // The activation proof loads the durable preparation first, so its typed
    // absence proves the speculative attempt left no preparation row behind.
    assert!(matches!(
        store.inspect_activation_proof(registered.handle),
        Err(nlos_store::StoreError::DispatchPreparationNotFound)
    ));

    // The fence rejected the stale handle, not the operation: the correct
    // generation still opens exactly one dispatch boundary.
    let dispatched = cache
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x92),
        })
        .unwrap();
    assert!(!dispatched.replayed);
    assert_eq!(dispatched.ticket.operation, registered.handle);
}

#[test]
fn cancel_epoch_wins_over_late_speculative_completion() {
    let root = Root::new("epoch");
    let cache = cache(&root, "ops");
    let provider = cache.provider();
    let registered = cache.register(spec(0x93)).unwrap();
    let dispatched = cache
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x94),
        })
        .unwrap();
    assert_eq!(dispatched.ticket.cancel_epoch, CancelEpoch::INITIAL);

    provider.arm_fault(ProviderFaultMode::FailProviderRpc);
    // The owner cancels through the operation authority during the degraded
    // window (the owner path is not a provider RPC).
    match provider
        .store()
        .request_cancel_idempotent(
            registered.handle,
            CancelEpoch::INITIAL,
            ReceiptId::from_bytes([0x95; 16]),
        )
        .unwrap()
    {
        CancelRequestDecision::Applied(snapshot) => {
            assert_eq!(snapshot.state, OperationState::CancelRequested);
            assert_eq!(snapshot.cancel_epoch, CancelEpoch::new(1));
        }
        other => panic!("owner cancel during the window must apply, got {other:?}"),
    }

    // A speculative re-dispatch cannot open a second effect boundary once
    // the epoch moved: a different callback identity is a typed conflict,
    // and the original callback only ever replays the fenced ticket.
    provider.disarm_fault();
    assert!(matches!(
        cache.dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x95),
        }),
        Err(ProviderError::Store(
            nlos_store::StoreError::DispatchPreparationConflict
        ))
    ));
    let replayed_dispatch = cache
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x94),
        })
        .unwrap();
    assert!(replayed_dispatch.replayed);
    assert_eq!(replayed_dispatch.ticket, dispatched.ticket);
    assert_eq!(
        provider.store().inspect(registered.handle).unwrap().state,
        OperationState::CancelRequested,
        "a replayed dispatch never re-opens the effect boundary"
    );

    // The provider's late speculative completion arrives with the window
    // already closed by the cancel: it is accepted only as fenced evidence
    // (reconciliation-only), never as a wake-permitted commit.
    let completion = cache
        .complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x94),
            seed: [0x96; 32],
        })
        .unwrap();
    assert!(!completion.replayed);
    assert!(completion.state.is_terminal());
    assert_eq!(
        outbox_for(provider.store(), registered.handle),
        vec![OutboxKind::ReconcileEffect],
        "cancel epoch wins: the late completion wakes nobody"
    );

    let replayed = cache
        .complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x94),
            seed: [0x96; 32],
        })
        .unwrap();
    assert!(replayed.replayed);
    assert_eq!(replayed.state, completion.state);
    assert!(matches!(
        cache.complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x94),
            seed: [0x97; 32],
        }),
        Err(ProviderError::Store(nlos_store::StoreError::Operation(
            nlos_operation::OperationError::CallbackIdentityConflict
        )))
    ));
    assert_eq!(
        outbox_for(provider.store(), registered.handle),
        vec![OutboxKind::ReconcileEffect],
        "the fenced commit is exactly-once"
    );
}

#[test]
fn fence_window_restart_commits_exactly_once() {
    let root = Root::new("window");
    let database = root.path().join("ops");
    let first = cache(&root, "ops");
    let registered = first.register(spec(0x98)).unwrap();
    let dispatched = first
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x99),
        })
        .unwrap();

    first
        .provider()
        .arm_fault(ProviderFaultMode::FailProviderRpc);
    assert!(matches!(
        first.complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x99),
            seed: [0x9A; 32],
        }),
        Err(ProviderError::Unreachable)
    ));
    owner_cancel(first.provider(), registered.handle);
    drop(first);

    // Provider restart inside the fence window: the fault dies with the
    // process, the durable boundary survives.
    let second = ProviderCache::new(Arc::new(nlos_driver_mock::MockProvider::new(Arc::new(
        SqliteOperationStore::open(&database).unwrap(),
    ))));
    let replayed = second
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x99),
        })
        .unwrap();
    assert!(replayed.replayed);
    assert_eq!(replayed.ticket, dispatched.ticket);

    let completion = second
        .complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x99),
            seed: [0x9A; 32],
        })
        .unwrap();
    assert!(!completion.replayed);
    assert!(completion.state.is_terminal());
    assert_eq!(
        outbox_for(second.provider().store(), registered.handle),
        vec![OutboxKind::ReconcileEffect]
    );
    drop(second);

    let third = ProviderCache::new(Arc::new(nlos_driver_mock::MockProvider::new(Arc::new(
        SqliteOperationStore::open(&database).unwrap(),
    ))));
    let replayed_completion = third
        .complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x99),
            seed: [0x9A; 32],
        })
        .unwrap();
    assert!(replayed_completion.replayed);
    assert_eq!(replayed_completion.state, completion.state);
    assert_eq!(
        outbox_for(third.provider().store(), registered.handle),
        vec![OutboxKind::ReconcileEffect],
        "a second restart must not commit the fenced effect again"
    );
    assert!(matches!(
        third.complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x99),
            seed: [0x9B; 32],
        }),
        Err(ProviderError::Store(nlos_store::StoreError::Operation(
            nlos_operation::OperationError::CallbackIdentityConflict
        )))
    ));
}

/// Owner-side cancel through the operation authority during a degraded
/// window (the owner path is not a provider RPC).
fn owner_cancel(provider: &nlos_driver_mock::MockProvider, handle: OperationHandle) {
    match provider
        .store()
        .request_cancel_idempotent(
            handle,
            CancelEpoch::INITIAL,
            ReceiptId::from_bytes([0xA5; 16]),
        )
        .unwrap()
    {
        CancelRequestDecision::Applied(snapshot) => {
            assert_eq!(snapshot.state, OperationState::CancelRequested);
        }
        other => panic!("owner cancel during the window must apply, got {other:?}"),
    }
}
