//! Deterministic fake provider driver plane (W30-B + W30-C).
//!
//! This crate is the minimal honest provider face for ROAD-B-003: a
//! deterministic fake provider whose every operation lands in the durable
//! `SqliteOperationStore` prepare→activate boundary (ADR-0002,
//! B-OP-FENCE-002) — never in a private side table — and whose terminal
//! outcomes are derived from caller-supplied seeds alone: no wall clock, no
//! randomness.
//!
//! Two faces share one core (`provider::MockProvider`):
//!
//! * the in-process handle: call the core directly, or through the shared
//!   [`cache::ProviderCache`] (the consumer-side entry whose degradation
//!   state is ROAD-B-003's fifth gate);
//! * the typed IPC face (`ipc`): SABI envelope handling, served only
//!   through the `authenticated` ADR-0011 principal challenge-response entry
//!   (Unix). There is deliberately no plaintext IPC shortcut.
//!
//! The provider owns no canonical state. Registration identity is the
//! `OperationSpec` itself; dispatch replays under the durable preparation/
//! activation receipts; completion replays under the callback identity
//! fence. Restarting the provider (dropping and reopening the store) replays
//! in-flight dispatches exactly, by operation identity.
//!
//! Degradation and the speculative side-effect fence (W30-C, ROAD-B-003's
//! last two gates):
//!
//! * the provider RPC boundary carries a fault-injection gate
//!   (`provider::ProviderFaultMode`, the `nlos-store-fault` arm/disarm
//!   pattern): armed, every RPC fails typed `ProviderError::Unreachable`
//!   before touching the durable authority, and over IPC the failure is the
//!   bounded `HostLost` + `RetrySameIdempotencyKey` envelope;
//! * the shared [`cache::ProviderCache`] degrades on unreachable RPCs and
//!   recovers observationally on the next provider answer, so the recovery
//!   converges through the durable replay semantics with no cache-side
//!   invalidation protocol;
//! * speculative effects are fenced by the durable machinery, never by
//!   optimism: a stale generation is typed-rejected with zero durable trace,
//!   and a cancel epoch advanced during a degraded window wins over the
//!   provider's late speculative completion (reconciliation-only commit,
//!   wake fenced, exactly-once across restarts).

#[cfg(unix)]
pub mod authenticated;
pub mod cache;
pub mod codec;
pub mod ipc;
pub mod provider;

pub use cache::{CacheHealth, DegradationCause, ProviderCache};
pub use provider::{
    CompleteProviderOperation, CompleteProviderOutcome, DispatchProviderOperation,
    DispatchProviderOutcome, MockProvider, ProviderError, ProviderFaultMode,
    RegisterProviderOutcome, derive_provider_outcome,
};
