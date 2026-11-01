//! Cross-platform dual-layer Wasm strong sandbox plane for untrusted
//! third-party community packages (`Stage E` `E6`, `ADR-0027`;
//! `[TCB-BYPASS-001]`, `[TCB-EGRESS-001]`, `[TCB-DOWNGRADE-001]`,
//! `[DRV-PROTO-001]`, `[DRV-LIFE-001]`, `[TUI-CONSENT-001]`).
//!
//! This crate operationalizes [ADR-0015](../../../docs/management/adrs/0015-dual-layer-untrusted-execution-isolation.md)
//! ("independent host process + in-process Wasmtime") as the mandatory
//! runtime base for `COMMUNITY_SANDBOXED` and otherwise-untrusted
//! third-party components, closing gap `U-9` (`RISK-E-04`):
//!
//! * **Layer 1 — independent host process** ([`process`]): the library
//!   never loads an untrusted module into the caller's process. Production
//!   callers spawn the dedicated [`host_bin`] binary (or an equivalent
//!   carrier built from it) through [`process::build_isolated_host_command`],
//!   which constructs a `std::process::Command` with an **empty environment**
//!   (`.env_clear()`), **explicit argv only**, and piped/null stdio. Handle
//!   and descriptor inheritance semantics follow the Rust `std` defaults on
//!   each platform and are documented in [`process`] together with the
//!   remaining platform-specific boundaries. [`process::run_isolated_host`]
//!   adds a wall-clock deadline with a hard process kill as the terminal
//!   last resort.
//! * **Layer 2 — in-process Wasmtime hard caps** ([`isolate`], used only by
//!   the dedicated host binary and by this crate's own tests): a strict
//!   hand-written `wasi_snapshot_preview1` **allowlist surface**
//!   ([`wasi_allowlist`]) — preopened-directory set empty by default, no
//!   network or socket imports at all, zero inherited environment, explicit
//!   guest argv only — plus a `ResourceLimiter` linear-memory page cap,
//!   deterministic CPU **fuel** quota, and **epoch** interruption
//!   (`Trap::OutOfFuel` / `Trap::Interrupt`).
//!
//! The escape-adversarial negative test matrix (path traversal, absolute
//! path escape, read-only preopen write attempts, absent socket imports,
//! memory over-allocation, fuel exhaustion, epoch interruption, zero host
//! environment leakage, wall-clock kill) lives in this crate's unit tests
//! and in `tests/escape_matrix.rs`, which drives the full dual-layer path
//! through real child processes. Evidence:
//! `docs/evidence/stage-e/e-sandbox-001-cross-platform-wasm-process-isolation.md`.

#![forbid(unsafe_code)]

pub mod error;
pub mod host_bin;
pub mod process;
pub mod profile;

mod isolate;
mod wasi_allowlist;

pub use error::{HostOutcome, HostReport, SandboxError, TrapKind};
pub use profile::{PreopenGrant, SandboxProfile};
