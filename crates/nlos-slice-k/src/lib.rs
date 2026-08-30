//! `nlos-slice-k` — the first longitudinal slice of Stage B, assembled
//! from the landed authorities only.
//!
//! The management baseline ([README §12](../../docs/management/README.md))
//! defines the first vertical slice as
//!
//! ```text
//! signed Package
//!   → install Application
//!   → create Task/TaskPlan
//!   → materialize Process/Fiber
//!   → async Driver Operation
//!   → Artifact + Receipt
//!   → cancel/crash recovery
//!   → CLI/NL inspect and control
//! ```
//!
//! and Issue 31 observes that only such a cross-crate slice can evidence
//! the P5 differences between an NLOS and a classic OS. This crate is that
//! slice's first integration proof. It invents **no** authority semantics:
//!
//! - [`SliceKRuntime`] only fixes sub-paths and opens the landed
//!   authorities together (`nlos-identity`, `nlos-artifact`,
//!   `nlos-application`, `nlos-task`, `nlos-clock`, `nlos-store`'s
//!   operation authority);
//! - [`package`] composes the landed sign/verify/install APIs
//!   (`ed25519-dalek` appears only on the producer side of the fixture,
//!   exactly like the `nlos-artifact` signature tests);
//! - [`fiber`] runs the landed `nlos-runtime-tokio` fiber contract over a
//!   job that composes the durable driver Operation (register → dispatch →
//!   complete), the permit-bound staged revision, and the commit plan;
//! - [`chain`] sequences the three scenarios (happy, cancel, crash
//!   recovery) through the landed `nlos-commit-coordinator` convergence;
//! - the demo bin prints one stable receipt line per step and an
//!   authority-sourced inspect view (the in-process stand-in for the CLI/NL
//!   control surface).
//!
//! Anything that could not be connected without inventing semantics is
//! recorded in the evidence document (`docs/evidence/stage-b/
//! b-slice-k-001-end-to-end.md`) as a known limitation or gap, not patched
//! over here.

mod chain;
mod error;
mod fiber;
mod package;
mod runtime;

pub use chain::{
    CancelFacts, HappyChain, RecoveryPrefix, run_cancel_path, run_happy_chain, run_recovery_prefix,
};
pub use error::{SliceKError, SliceKResult};
pub use fiber::{FiberOutcome, WriteFiberJob, spawn_write_fiber};
pub use package::{PublishedPackage, Publisher, fixture_bytes};
pub use runtime::{
    ChainInspect, ChainQuery, SliceKRuntime, initial_generation, seeded_key, short_hex,
};
