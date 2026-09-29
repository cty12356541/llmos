//! Provider cache degradation (W30-C, ROAD-B-003 fifth gate): when the
//! provider RPC boundary is unreachable, the shared provider cache degrades
//! typed-fail-closed — every consumer sees a typed `Unreachable`, the durable
//! operation authority keeps every row in its pre-degradation state, and the
//! recovery after the provider returns converges through the W30-B replay
//! semantics (exact dispatch replay, idempotent completion) with no double
//! effect.
//!
//! The unreachable condition is injected deterministically through the
//! provider's fault knob (`ProviderFaultMode::FailProviderRpc`, the
//! `nlos-store-fault` arm/disarm pattern lifted to the provider RPC
//! boundary); the knob is in-memory only, so a provider restart (store
//! reopen, the W30-B convention) ends an injected degradation window.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_driver_mock::cache::{CacheHealth, DegradationCause, ProviderCache};
use nlos_driver_mock::provider::{
    CompleteProviderOperation, DispatchProviderOperation, MockProvider, ProviderError,
    ProviderFaultMode,
};
use nlos_operation::OperationSpec;
use nlos_runtime::FiberHandle;
use nlos_store::SqliteOperationStore;
use nlos_types::{CallbackId, CancellationScopeId, ExecutionFiberId, Generation, OperationId};

struct Root(PathBuf);

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

impl Root {
    fn new(label: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = Self(std::env::temp_dir().join(format!(
            "nlos-driver-mock-degrade-{label}-{}-{sequence}",
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
        fiber_id: ExecutionFiberId::from_bytes([0x71; 16]),
        generation: Generation::INITIAL,
    }
}

fn spec(seed: u8) -> OperationSpec {
    OperationSpec {
        operation_id: operation_id(seed),
        generation: Generation::INITIAL,
        owner_fiber: owner_fiber(),
        cancellation_scope_id: CancellationScopeId::from_bytes([0x72; 16]),
        cancellation_generation: Generation::INITIAL,
    }
}

fn cache(root: &Root, sub: &str) -> ProviderCache {
    ProviderCache::new(Arc::new(MockProvider::new(Arc::new(
        SqliteOperationStore::open(root.path().join(sub)).unwrap(),
    ))))
}

fn assert_degraded(health: CacheHealth) {
    assert_eq!(
        health,
        CacheHealth::Degraded {
            cause: DegradationCause::ProviderUnreachable
        },
        "an unreachable provider RPC must degrade the shared cache"
    );
}

#[test]
fn degraded_provider_fails_typed_and_preserves_durable_rows() {
    let root = Root::new("rows");
    let cache = cache(&root, "ops");
    let provider = cache.provider();
    let registered = cache.register(spec(0x61)).unwrap();
    let before = provider.store().inspect(registered.handle).unwrap();

    provider.arm_fault(ProviderFaultMode::FailProviderRpc);
    let observed = provider.rpcs_observed();

    assert!(matches!(
        cache.register(spec(0x62)),
        Err(ProviderError::Unreachable)
    ));
    assert_degraded(cache.health());
    assert!(matches!(
        cache.dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x63),
        }),
        Err(ProviderError::Unreachable)
    ));
    assert!(matches!(
        cache.complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x63),
            seed: [0x64; 32],
        }),
        Err(ProviderError::Unreachable)
    ));
    assert_eq!(
        provider.rpcs_observed(),
        observed + 3,
        "every faulted RPC attempt is observable"
    );

    // The durable authority keeps every row in its pre-degradation state:
    // the registered operation is untouched and the never-arriving
    // registration left no row at all.
    let after = provider.store().inspect(registered.handle).unwrap();
    assert_eq!(after, before);
    assert!(matches!(
        provider.store().inspect(nlos_operation::OperationHandle {
            operation_id: operation_id(0x62),
            generation: Generation::INITIAL,
        }),
        Err(nlos_store::StoreError::Operation(
            nlos_operation::OperationError::InvalidGeneration
        ))
    ));
    assert!(
        provider.store().pending_outbox(16).unwrap().is_empty(),
        "degradation must not manufacture wake or reconciliation evidence"
    );
}

#[test]
fn recovery_after_provider_returns_replays_and_converges() {
    let root = Root::new("recover");
    let cache = cache(&root, "ops");
    let provider = cache.provider();
    let registered = cache.register(spec(0x65)).unwrap();
    let dispatched = cache
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x66),
        })
        .unwrap();
    assert!(!dispatched.replayed);

    provider.arm_fault(ProviderFaultMode::FailProviderRpc);
    assert!(matches!(
        cache.complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x66),
            seed: [0x67; 32],
        }),
        Err(ProviderError::Unreachable)
    ));
    assert_eq!(
        provider.store().inspect(registered.handle).unwrap().state,
        nlos_operation::OperationState::Dispatched,
        "the degraded window leaves the in-flight dispatch untouched"
    );

    provider.disarm_fault();
    let replayed = cache
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x66),
        })
        .unwrap();
    assert!(replayed.replayed, "recovery must replay, not re-issue");
    assert_eq!(replayed.ticket, dispatched.ticket);
    assert_eq!(
        replayed.preparation_receipt_id,
        dispatched.preparation_receipt_id
    );
    assert_eq!(
        replayed.activation_receipt_id,
        dispatched.activation_receipt_id
    );

    let completion = cache
        .complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x66),
            seed: [0x67; 32],
        })
        .unwrap();
    assert!(!completion.replayed);
    assert!(completion.state.is_terminal());
    assert_eq!(cache.health(), CacheHealth::Healthy);

    let replayed_completion = cache
        .complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x66),
            seed: [0x67; 32],
        })
        .unwrap();
    assert!(replayed_completion.replayed);
    assert_eq!(replayed_completion.state, completion.state);
}

#[test]
fn degradation_window_survives_provider_restart_and_converges() {
    let root = Root::new("restart");
    let database = root.path().join("ops");
    let first = cache(&root, "ops");
    let registered = first.register(spec(0x68)).unwrap();
    let dispatched = first
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x69),
        })
        .unwrap();

    first
        .provider()
        .arm_fault(ProviderFaultMode::FailProviderRpc);
    assert!(matches!(
        first.complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x69),
            seed: [0x6A; 32],
        }),
        Err(ProviderError::Unreachable)
    ));

    // Provider process drop: the injected fault is in-memory and dies with
    // the process; only the durable database survives.
    drop(first);

    let second = ProviderCache::new(Arc::new(MockProvider::new(Arc::new(
        SqliteOperationStore::open(&database).unwrap(),
    ))));
    let replayed = second
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x69),
        })
        .unwrap();
    assert!(replayed.replayed);
    assert_eq!(replayed.ticket, dispatched.ticket);
    assert_eq!(
        replayed.activation_receipt_id,
        dispatched.activation_receipt_id
    );

    let completion = second
        .complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x69),
            seed: [0x6A; 32],
        })
        .unwrap();
    assert!(!completion.replayed);
    assert!(completion.state.is_terminal());
    assert_eq!(second.health(), CacheHealth::Healthy);
}

#[test]
fn shared_cache_degradation_is_visible_to_every_consumer() {
    let root = Root::new("shared");
    let cache = Arc::new(ProviderCache::new(Arc::new(MockProvider::new(Arc::new(
        SqliteOperationStore::open(root.path().join("ops")).unwrap(),
    )))));
    let consumer_two = Arc::clone(&cache);
    let registered = cache.register(spec(0x6B)).unwrap();

    cache
        .provider()
        .arm_fault(ProviderFaultMode::FailProviderRpc);
    assert!(matches!(
        cache.dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x6C),
        }),
        Err(ProviderError::Unreachable)
    ));
    assert_degraded(consumer_two.health());
    assert!(matches!(
        consumer_two.register(spec(0x6D)),
        Err(ProviderError::Unreachable)
    ));
    assert_degraded(cache.health());

    cache.provider().disarm_fault();
    let dispatched = consumer_two
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x6C),
        })
        .unwrap();
    assert!(!dispatched.replayed);
    assert_eq!(
        cache.health(),
        CacheHealth::Healthy,
        "one consumer's recovered RPC restores the shared entry for everyone"
    );
}

#[test]
fn typed_authority_rejection_does_not_degrade_the_cache() {
    let root = Root::new("answer");
    let cache = cache(&root, "ops");
    let provider = cache.provider();
    let registered = cache.register(spec(0x6E)).unwrap();
    cache
        .dispatch(DispatchProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x6F),
        })
        .unwrap();

    // A typed authority rejection is a provider answer, not unreachability:
    // the cache entry must stay healthy.
    assert!(matches!(
        cache.complete(CompleteProviderOperation {
            handle: registered.handle,
            callback_id: callback_id(0x70),
            seed: [0x71; 32],
        }),
        Err(ProviderError::Store(nlos_store::StoreError::Operation(
            nlos_operation::OperationError::InvalidGeneration
        )))
    ));
    assert_eq!(cache.health(), CacheHealth::Healthy);
    assert_eq!(
        provider.store().inspect(registered.handle).unwrap().state,
        nlos_operation::OperationState::Dispatched
    );
}
