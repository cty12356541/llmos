//! Shared provider cache with degradation state (W30-C, ROAD-B-003 fifth
//! gate).
//!
//! [`ProviderCache`] is the consumer-side holder of one shared provider
//! handle: every consumer clones the same `Arc<ProviderCache>`, so one
//! degradation is visible to all of them. The cache never caches operation
//! state — only health — so a degraded window cannot manufacture stale
//! success evidence, and the recovery after the provider returns converges
//! through the provider's durable replay semantics without any cache-side
//! invalidation protocol.
//!
//! Health transitions are observational, not fail-fast: every call still
//! attempts the provider RPC (a deterministic mock must report the
//! provider's current truth, not a cached verdict), [`ProviderError::Unreachable`]
//! degrades the entry, and any provider answer — success or typed authority
//! rejection — proves reachability and restores [`CacheHealth::Healthy`].
//! Circuit-breaker hysteresis is production policy, not a semantic floor,
//! and is deliberately absent.

use std::sync::MutexGuard;
use std::sync::{Arc, Mutex};

use nlos_operation::OperationSpec;

use crate::provider::{
    CompleteProviderOperation, CompleteProviderOutcome, DispatchProviderOperation,
    DispatchProviderOutcome, MockProvider, ProviderError, RegisterProviderOutcome,
};

/// Why a provider cache entry is degraded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DegradationCause {
    /// The provider RPC boundary reported the provider unreachable.
    ProviderUnreachable,
}

/// Last-observed health of one shared cache entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheHealth {
    /// The last observed provider RPC answered.
    Healthy,
    /// The last observed provider RPC was unreachable.
    Degraded { cause: DegradationCause },
}

/// The shared provider cache entry. Cloning the `Arc` shares the health
/// state; the provider handle itself is shared unchanged.
pub struct ProviderCache {
    provider: Arc<MockProvider>,
    health: Mutex<CacheHealth>,
}

impl ProviderCache {
    /// Wraps one shared provider handle with a healthy cache entry.
    #[must_use]
    pub fn new(provider: Arc<MockProvider>) -> Self {
        Self {
            provider,
            health: Mutex::new(CacheHealth::Healthy),
        }
    }

    /// The shared provider behind this entry.
    #[must_use]
    pub fn provider(&self) -> &MockProvider {
        &self.provider
    }

    /// The last observed health of the entry.
    #[must_use]
    pub fn health(&self) -> CacheHealth {
        self.lock_health().to_owned()
    }

    /// Registers one provider operation through the shared entry.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Unreachable`] (degrading the entry) or the
    /// provider's typed rejection.
    pub fn register(&self, spec: OperationSpec) -> Result<RegisterProviderOutcome, ProviderError> {
        self.observe(self.provider.register(spec))
    }

    /// Dispatches one provider operation through the shared entry.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Unreachable`] (degrading the entry) or the
    /// provider's typed rejection.
    pub fn dispatch(
        &self,
        request: DispatchProviderOperation,
    ) -> Result<DispatchProviderOutcome, ProviderError> {
        self.observe(self.provider.dispatch(request))
    }

    /// Completes one provider operation through the shared entry.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Unreachable`] (degrading the entry) or the
    /// provider's typed rejection.
    pub fn complete(
        &self,
        request: CompleteProviderOperation,
    ) -> Result<CompleteProviderOutcome, ProviderError> {
        self.observe(self.provider.complete(request))
    }

    fn observe<T>(&self, outcome: Result<T, ProviderError>) -> Result<T, ProviderError> {
        let observed = match &outcome {
            Ok(_) | Err(ProviderError::Store(_)) => CacheHealth::Healthy,
            Err(ProviderError::Unreachable) => CacheHealth::Degraded {
                cause: DegradationCause::ProviderUnreachable,
            },
        };
        *self.lock_health() = observed;
        outcome
    }

    fn lock_health(&self) -> MutexGuard<'_, CacheHealth> {
        // A poisoned health lock keeps the last written verdict observable;
        // health is advisory state and never blocks the typed provider path.
        self.health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
