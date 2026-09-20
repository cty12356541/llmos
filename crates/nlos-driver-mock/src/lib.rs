//! Deterministic fake provider driver plane (W30-B, B3-4 first half).
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
//! * the in-process handle: call the core directly;
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
//! Provider cache degradation and the speculative side-effect fence are
//! W30-C and are not present here.

pub mod provider;

#[cfg(unix)]
pub mod authenticated;
pub mod codec;
pub mod ipc;

pub use provider::{
    CompleteProviderOperation, CompleteProviderOutcome, DispatchProviderOperation,
    DispatchProviderOutcome, MockProvider, RegisterProviderOutcome, derive_provider_outcome,
};
