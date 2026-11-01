//! Layer-2 in-process isolation engine: builds the Wasmtime `Store` with the
//! hard caps from [`crate::SandboxProfile`] and runs one untrusted guest to
//! a terminal outcome (`ADR-0015` decision 2/3/5).
//!
//! This module is `pub(crate)` on purpose: per `ADR-0015` the library never
//! loads an untrusted module into a caller's process. Only the dedicated
//! host binary ([`crate::host_bin`]) and this crate's own tests instantiate
//! this engine. Production callers reach it exclusively through the spawned
//! host process ([`crate::process`]).
//!
//! Hard caps assembled here:
//!
//! * **Linear memory**: [`PageLimiter`] enforces the profile page cap at
//!   64 KiB page granularity, both at instantiation (declared minimum) and
//!   at every `memory.grow` call site (denied growth observes `-1`).
//! * **Deterministic CPU**: fuel metering; exhaustion traps with
//!   `Trap::OutOfFuel`.
//! * **Wall clock**: epoch interruption driven by a dedicated ticker
//!   thread; deadline trips trap with `Trap::Interrupt`.
//! * **Tables/instances/memories counts**: tight deny-by-default ceilings.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use wasmtime::{Config, Engine, Module, ResourceLimiter, Store, Trap};

use crate::error::{SandboxError, TrapKind, sanitize_detail};
use crate::profile::{SandboxProfile, WASM_PAGE_BYTES};
use crate::wasi_allowlist::{GuestExited, GuestIoSink, GuestState, add_to_linker};

/// Hard cap on table elements per store (deny-by-default hardening).
const MAX_TABLE_ELEMENTS: usize = 1024;

/// Hard cap on instances per store (one guest module per run).
const MAX_INSTANCES: usize = 8;

/// Hard cap on linear memories per store (guests are single-memory).
const MAX_MEMORIES: usize = 2;

/// Linear-memory page limiter (`ADR-0015` decision 3: memory enforcement is
/// owned by the Wasm layer; host-process rlimits are not used).
#[derive(Debug)]
pub(crate) struct PageLimiter {
    max_memory_bytes: usize,
}

impl PageLimiter {
    /// Creates a limiter allowing at most `max_pages` 64 KiB pages.
    pub(crate) fn new(max_pages: u64) -> Self {
        let max_bytes = max_pages.saturating_mul(WASM_PAGE_BYTES);
        Self {
            max_memory_bytes: usize::try_from(max_bytes).unwrap_or(usize::MAX),
        }
    }
}

impl ResourceLimiter for PageLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= self.max_memory_bytes)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= MAX_TABLE_ELEMENTS)
    }

    fn instances(&self) -> usize {
        MAX_INSTANCES
    }

    fn tables(&self) -> usize {
        MAX_TABLE_ELEMENTS
    }

    fn memories(&self) -> usize {
        MAX_MEMORIES
    }
}

/// Background epoch ticker driving epoch interruption. Dropping the ticker
/// stops and joins the thread.
struct EpochTicker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EpochTicker {
    fn spawn(engine: Engine, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                engine.increment_epoch();
                std::thread::sleep(interval);
            }
            // One final increment so a deadline that became due during the
            // last sleep is still observed.
            engine.increment_epoch();
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _unused = handle.join();
        }
    }
}

/// Terminal outcome of one untrusted guest run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GuestOutcome {
    /// The entrypoint returned normally with the given code.
    Completed {
        /// Guest return value of the entrypoint.
        code: i32,
        /// Fuel remaining after the run (determinism observable).
        fuel_remaining: Option<u64>,
    },
    /// The guest requested `proc_exit` with the given code.
    Exited {
        /// Guest-requested exit code.
        code: i32,
        /// Fuel remaining after the run.
        fuel_remaining: Option<u64>,
    },
    /// The engine terminated the guest; see [`TrapKind`].
    Trapped {
        /// Classified trap kind.
        kind: TrapKind,
    },
}

fn classify_call_error(error: &wasmtime::Error) -> Option<GuestOutcome> {
    if let Some(exit) = error.downcast_ref::<GuestExited>() {
        return Some(GuestOutcome::Exited {
            code: exit.code,
            fuel_remaining: None,
        });
    }
    if let Some(trap) = error.downcast_ref::<Trap>() {
        return Some(GuestOutcome::Trapped {
            kind: match trap {
                Trap::OutOfFuel => TrapKind::OutOfFuel,
                Trap::Interrupt => TrapKind::EpochInterrupt,
                Trap::UnreachableCodeReached => TrapKind::UnreachableCodeReached,
                other => TrapKind::Other {
                    detail: sanitize_detail(&other.to_string()),
                },
            },
        });
    }
    None
}

/// Runs one untrusted guest module under the profile's hard caps and
/// allowlist surface, returning the terminal outcome plus the I/O sink for
/// inspection.
///
/// # Errors
///
/// Returns [`SandboxError::GuestModuleRejected`] for invalid WAT/wasm,
/// [`SandboxError::GuestInstantiationRejected`] for rejected instantiation
/// (unknown imports — for example socket imports — or memory minimums over
/// the page cap), and the remaining typed variants for entrypoint and
/// engine failures.
pub(crate) fn run_guest(
    profile: &SandboxProfile,
    wat_or_wasm: &[u8],
    entrypoint: &str,
    sink: Box<dyn GuestIoSink>,
) -> Result<(GuestOutcome, Box<dyn GuestIoSink>), SandboxError> {
    profile.validate().map_err(SandboxError::Profile)?;

    let mut config = Config::new();
    config.consume_fuel(true).epoch_interruption(true);
    let engine = Engine::new(&config).map_err(|e| SandboxError::GuestEngineFailure {
        detail: sanitize_detail(&e.to_string()),
    })?;
    let module =
        Module::new(&engine, wat_or_wasm).map_err(|e| SandboxError::GuestModuleRejected {
            detail: sanitize_detail(&e.to_string()),
        })?;

    let mut store = Store::new(&engine, GuestState::new(profile, sink));
    store
        .set_fuel(profile.fuel())
        .map_err(|e| SandboxError::GuestEngineFailure {
            detail: sanitize_detail(&e.to_string()),
        })?;
    store.limiter(|state: &mut GuestState| &mut state.limiter);

    let mut linker = wasmtime::Linker::new(&engine);
    add_to_linker(&mut linker).map_err(|e| SandboxError::GuestEngineFailure {
        detail: sanitize_detail(&e.to_string()),
    })?;

    let _ticker = if profile.epoch_deadline_ticks() > 0 {
        store.set_epoch_deadline(profile.epoch_deadline_ticks());
        Some(EpochTicker::spawn(
            engine.clone(),
            Duration::from_millis(profile.epoch_tick_interval_ms()),
        ))
    } else {
        // Wasmtime traps immediately when epoch interruption is enabled but
        // no deadline was set; an unconfigured profile means "no epoch
        // fence", so arm a deadline that can never be reached.
        store.set_epoch_deadline(u64::MAX);
        None
    };

    let instance = linker.instantiate(&mut store, &module).map_err(|e| {
        SandboxError::GuestInstantiationRejected {
            detail: sanitize_detail(&e.to_string()),
        }
    })?;
    let Some(raw_func) = instance.get_func(&mut store, entrypoint) else {
        return Err(SandboxError::EntrypointMissing {
            name: entrypoint.to_string(),
        });
    };
    let func = raw_func.typed::<(), i32>(&mut store).map_err(|_| {
        SandboxError::EntrypointSignatureInvalid {
            name: entrypoint.to_string(),
        }
    })?;

    let call_result = func.call(&mut store, ());
    let fuel_remaining = store.get_fuel().ok();
    let outcome = match call_result {
        Ok(code) => GuestOutcome::Completed {
            code,
            fuel_remaining,
        },
        Err(error) => match classify_call_error(&error) {
            Some(GuestOutcome::Exited { code, .. }) => GuestOutcome::Exited {
                code,
                fuel_remaining,
            },
            Some(GuestOutcome::Trapped { kind }) => GuestOutcome::Trapped { kind },
            _ => GuestOutcome::Trapped {
                kind: TrapKind::Other {
                    detail: sanitize_detail(&error.to_string()),
                },
            },
        },
    };
    let sink = store.into_data().into_sink();
    Ok((outcome, sink))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::PreopenGrant;
    use crate::wasi_allowlist::VecIoSink;

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nlos-e6a-isolate-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn run_wat(profile: &SandboxProfile, wat: &str) -> Result<GuestOutcome, SandboxError> {
        run_guest(
            profile,
            wat.as_bytes(),
            "main",
            Box::new(VecIoSink::default()),
        )
        .map(|(outcome, _sink)| outcome)
    }

    #[test]
    fn fuel_exhaustion_is_typed_out_of_fuel() {
        let wat = r#"
            (module
                (memory (export "memory") 1)
                (func (export "main") (result i32)
                    (local $i i64)
                    (loop $l
                        (local.set $i (i64.add (local.get $i) (i64.const 1)))
                        (br_if $l (i64.lt_u (local.get $i) (i64.const 1000000000))))
                    (i32.const 0)))
        "#;
        let profile = SandboxProfile::deny_all().with_fuel(50_000);
        let outcome = run_wat(&profile, wat).expect("run completes");
        assert_eq!(
            outcome,
            GuestOutcome::Trapped {
                kind: TrapKind::OutOfFuel
            }
        );
    }

    #[test]
    fn fuel_consumption_is_deterministic_across_identical_runs() {
        let wat = r#"
            (module
                (memory (export "memory") 1)
                (func (export "main") (result i32)
                    (local $i i64)
                    (loop $l
                        (local.set $i (i64.add (local.get $i) (i64.const 1)))
                        (br_if $l (i64.lt_u (local.get $i) (i64.const 20000))))
                    (i32.const 0)))
        "#;
        let profile = SandboxProfile::deny_all().with_fuel(1_000_000);
        let run1 = run_wat(&profile, wat).expect("first run");
        let run2 = run_wat(&profile, wat).expect("second run");
        let GuestOutcome::Completed {
            fuel_remaining: fuel1,
            ..
        } = run1
        else {
            panic!("first run must complete");
        };
        let GuestOutcome::Completed {
            fuel_remaining: fuel2,
            ..
        } = run2
        else {
            panic!("second run must complete");
        };
        assert_eq!(fuel1, fuel2, "identical runs must consume identical fuel");
        let fuel1 = fuel1.expect("fuel observable");
        assert!(fuel1 < 1_000_000, "some fuel must have been consumed");
    }

    #[test]
    fn epoch_deadline_interrupts_before_fuel_backstop() {
        let wat = r#"
            (module
                (memory (export "memory") 1)
                (func (export "main") (result i32)
                    (loop $l (br $l))
                    (i32.const 0)))
        "#;
        // Huge-but-finite fuel backstop: if epoch interruption were broken,
        // the run would still terminate with OutOfFuel instead of hanging,
        // and the assertion below would fail rather than the test hang.
        let profile = SandboxProfile::deny_all()
            .with_fuel(100_000_000_000)
            .with_epoch_deadline(2, 1);
        let outcome = run_wat(&profile, wat).expect("run terminates");
        assert_eq!(
            outcome,
            GuestOutcome::Trapped {
                kind: TrapKind::EpochInterrupt
            }
        );
    }

    #[test]
    fn memory_grow_beyond_page_cap_observes_minus_one() {
        let wat = r#"
            (module
                (memory (export "memory") 1)
                (func (export "main") (result i32)
                    (local $prev i32)
                    (local.set $prev (memory.grow (i32.const 100)))
                    (if (i32.eq (local.get $prev) (i32.const -1))
                        (then (return (i32.const 42))))
                    (local.get $prev)))
        "#;
        let profile = SandboxProfile::deny_all().with_max_memory_pages(4);
        let outcome = run_wat(&profile, wat).expect("run completes");
        match outcome {
            GuestOutcome::Completed { code, .. } => {
                assert_eq!(code, 42, "memory.grow must observe -1 under the cap");
            }
            other => panic!("expected completion, got {other:?}"),
        }
    }

    #[test]
    fn memory_grow_within_page_cap_succeeds() {
        let wat = r#"
            (module
                (memory (export "memory") 1)
                (func (export "main") (result i32)
                    (local $prev i32)
                    (local.set $prev (memory.grow (i32.const 2)))
                    (if (i32.eq (local.get $prev) (i32.const -1))
                        (then (return (i32.const 42))))
                    (local.get $prev)))
        "#;
        let profile = SandboxProfile::deny_all().with_max_memory_pages(4);
        let outcome = run_wat(&profile, wat).expect("run completes");
        match outcome {
            GuestOutcome::Completed { code, .. } => assert_eq!(code, 1, "prev page count"),
            other => panic!("expected completion, got {other:?}"),
        }
    }

    #[test]
    fn declared_memory_over_cap_is_rejected_at_instantiation() {
        let wat = r#"
            (module
                (memory (export "memory") 8)
                (func (export "main") (result i32) (i32.const 0)))
        "#;
        let profile = SandboxProfile::deny_all().with_max_memory_pages(4);
        let err = run_wat(&profile, wat).expect_err("instantiation must be rejected");
        match err {
            SandboxError::GuestInstantiationRejected { detail } => {
                assert_ne!(detail.len(), 0);
            }
            other => panic!("expected instantiation rejection, got {other:?}"),
        }
    }

    #[test]
    fn guest_trap_unreachable_is_typed() {
        let wat = r#"
            (module
                (memory (export "memory") 1)
                (func (export "main") (result i32) unreachable))
        "#;
        let outcome = run_wat(&SandboxProfile::deny_all(), wat).expect("run completes");
        assert_eq!(
            outcome,
            GuestOutcome::Trapped {
                kind: TrapKind::UnreachableCodeReached
            }
        );
    }

    #[test]
    fn proc_exit_maps_to_exited_with_requested_code() {
        let wat = r#"
            (module
                (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
                (memory (export "memory") 1)
                (func (export "main") (result i32)
                    (call $proc_exit (i32.const 7))
                    (i32.const 0)))
        "#;
        let outcome = run_wat(&SandboxProfile::deny_all(), wat).expect("run completes");
        match outcome {
            GuestOutcome::Exited { code, .. } => assert_eq!(code, 7),
            other => panic!("expected guest exit, got {other:?}"),
        }
    }

    #[test]
    fn guest_environment_is_empty_and_wild_environ_get_is_safe() {
        let wat = r#"
            (module
                (import "wasi_snapshot_preview1" "environ_sizes_get"
                    (func $env_sizes (param i32 i32) (result i32)))
                (import "wasi_snapshot_preview1" "environ_get"
                    (func $env_get (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "main") (result i32)
                    (if (i32.ne (call $env_sizes (i32.const 100) (i32.const 104)) (i32.const 0))
                        (then (return (i32.const 100))))
                    (if (i32.ne (i32.load (i32.const 100)) (i32.const 0))
                        (then (return (i32.const 101))))
                    (if (i32.ne (i32.load (i32.const 104)) (i32.const 0))
                        (then (return (i32.const 102))))
                    ;; zero entries: even wild pointers must stay SUCCESS
                    (if (i32.ne (call $env_get (i32.const -1) (i32.const -1)) (i32.const 0))
                        (then (return (i32.const 103))))
                    (i32.const 0)))
        "#;
        let outcome = run_wat(&SandboxProfile::deny_all(), wat).expect("run completes");
        match outcome {
            GuestOutcome::Completed { code, .. } => assert_eq!(code, 0),
            other => panic!("expected completion, got {other:?}"),
        }
    }

    #[test]
    fn guest_arguments_are_exactly_the_declared_argv() {
        let wat = r#"
            (module
                (import "wasi_snapshot_preview1" "args_sizes_get"
                    (func $args_sizes (param i32 i32) (result i32)))
                (import "wasi_snapshot_preview1" "args_get"
                    (func $args_get (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "main") (result i32)
                    (if (i32.ne (call $args_sizes (i32.const 100) (i32.const 104)) (i32.const 0))
                        (then (return (i32.const 100))))
                    (if (i32.ne (i32.load (i32.const 100)) (i32.const 2))
                        (then (return (i32.const 101))))
                    ;; "alpha\0" (6) + "beta\0" (5) = 11 bytes
                    (if (i32.ne (i32.load (i32.const 104)) (i32.const 11))
                        (then (return (i32.const 102))))
                    (if (i32.ne (call $args_get (i32.const 200) (i32.const 4096)) (i32.const 0))
                        (then (return (i32.const 103))))
                    (if (i32.ne (i32.load (i32.const 200)) (i32.const 0))
                        (then (return (i32.const 104))))
                    (if (i32.ne (i32.load (i32.const 204)) (i32.const 6))
                        (then (return (i32.const 105))))
                    (if (i32.ne (i32.load8_u (i32.const 4096)) (i32.const 97))
                        (then (return (i32.const 106))))
                    (if (i32.ne (i32.load8_u (i32.const 4100)) (i32.const 97))
                        (then (return (i32.const 107))))
                    (if (i32.ne (i32.load8_u (i32.const 4101)) (i32.const 0))
                        (then (return (i32.const 108))))
                    (if (i32.ne (i32.load8_u (i32.const 4102)) (i32.const 98))
                        (then (return (i32.const 109))))
                    (i32.const 0)))
        "#;
        let profile = SandboxProfile::deny_all().with_guest_args(["alpha", "beta"]);
        let outcome = run_wat(&profile, wat).expect("run completes");
        match outcome {
            GuestOutcome::Completed { code, .. } => assert_eq!(code, 0),
            other => panic!("expected completion, got {other:?}"),
        }
    }

    #[test]
    fn guest_stdout_is_captured_by_the_sink() {
        let wat = r#"
            (module
                (import "wasi_snapshot_preview1" "fd_write"
                    (func $fd_write (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 1024) "e6a-ok\n")
                (func (export "main") (result i32)
                    (i32.store (i32.const 2048) (i32.const 1024))
                    (i32.store (i32.const 2052) (i32.const 7))
                    (if (i32.ne (call $fd_write (i32.const 1) (i32.const 2048) (i32.const 1) (i32.const 2056)) (i32.const 0))
                        (then (return (i32.const 100))))
                    (i32.const 0)))
        "#;
        let (outcome, sink) = run_guest(
            &SandboxProfile::deny_all(),
            wat.as_bytes(),
            "main",
            Box::new(VecIoSink::default()),
        )
        .expect("run completes");
        match outcome {
            GuestOutcome::Completed { code, .. } => assert_eq!(code, 0),
            other => panic!("expected completion, got {other:?}"),
        }
        let mut vec_sink = sink;
        let captured = vec_sink
            .as_any()
            .downcast_mut::<VecIoSink>()
            .expect("vec sink");
        assert_eq!(captured.stdout, b"e6a-ok\n");
    }

    #[test]
    fn absent_socket_import_rejects_instantiation() {
        let wat = r#"
            (module
                (import "wasi_snapshot_preview1" "sock_open"
                    (func $sock_open (param i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "main") (result i32)
                    (call $sock_open (i32.const 2) (i32.const 1) (i32.const 512))))
        "#;
        let err = run_wat(&SandboxProfile::deny_all(), wat).expect_err("must be rejected");
        match err {
            SandboxError::GuestInstantiationRejected { detail } => {
                assert!(detail.contains("sock_open"), "detail was {detail}");
            }
            other => panic!("expected instantiation rejection, got {other:?}"),
        }
    }

    #[test]
    fn missing_entrypoint_is_a_typed_error() {
        let wat = r#"
            (module
                (memory (export "memory") 1)
                (func (export "other") (result i32) (i32.const 0)))
        "#;
        let err = run_wat(&SandboxProfile::deny_all(), wat).expect_err("must be rejected");
        assert!(matches!(
            err,
            SandboxError::EntrypointMissing { name } if name == "main"
        ));
    }

    #[test]
    fn wrong_entrypoint_signature_is_a_typed_error() {
        let wat = r#"
            (module
                (memory (export "memory") 1)
                (func (export "main") (param i32) (result i32) (local.get 0)))
        "#;
        let err = run_wat(&SandboxProfile::deny_all(), wat).expect_err("must be rejected");
        assert!(matches!(
            err,
            SandboxError::EntrypointSignatureInvalid { .. }
        ));
    }

    #[test]
    fn invalid_wat_is_rejected_at_compile() {
        let err = run_wat(&SandboxProfile::deny_all(), "(module (bogus").expect_err("must fail");
        assert!(matches!(err, SandboxError::GuestModuleRejected { .. }));
    }

    #[test]
    fn deny_all_profile_has_no_preopen_fds_path_open_returns_badf() {
        let wat = r#"
            (module
                (import "wasi_snapshot_preview1" "path_open"
                    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 1024) "any.txt")
                (func (export "main") (result i32)
                    (call $path_open (i32.const 3) (i32.const 0) (i32.const 1024) (i32.const 7)
                        (i32.const 0) (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 2048))))
        "#;
        let outcome = run_wat(&SandboxProfile::deny_all(), wat).expect("run completes");
        match outcome {
            GuestOutcome::Completed { code, .. } => assert_eq!(code, 8, "EBADF for fd 3"),
            other => panic!("expected completion, got {other:?}"),
        }
    }

    #[test]
    fn preopen_grant_allows_in_mount_read_and_fences_traversal() {
        let root = scratch_dir("preopen");
        std::fs::write(root.join("allowed.txt"), b"nlos-e6").expect("seed file");
        // A sibling outside the granted root must not be reachable.
        let outside = root
            .parent()
            .expect("parent")
            .join("e6a-outside-marker.txt");
        std::fs::write(&outside, b"hostile").expect("outside marker");

        let grant = PreopenGrant::new("/workspace", &root, false).expect("grant");
        let profile = SandboxProfile::deny_all()
            .with_preopen(grant)
            .with_fuel(5_000_000);

        let read_wat = r#"
            (module
                (import "wasi_snapshot_preview1" "path_open"
                    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
                (import "wasi_snapshot_preview1" "fd_read"
                    (func $fd_read (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 1024) "allowed.txt")
                (func (export "main") (result i32)
                    (local $errno i32) (local $fd i32)
                    (local.set $errno (call $path_open (i32.const 3) (i32.const 0)
                        (i32.const 1024) (i32.const 11) (i32.const 0)
                        (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 2048)))
                    (if (i32.ne (local.get $errno) (i32.const 0))
                        (then (return (i32.add (local.get $errno) (i32.const 1000)))))
                    (local.set $fd (i32.load (i32.const 2048)))
                    (i32.store (i32.const 3072) (i32.const 4096))
                    (i32.store (i32.const 3076) (i32.const 64))
                    (local.set $errno (call $fd_read (local.get $fd) (i32.const 3072) (i32.const 1) (i32.const 3078)))
                    (if (i32.ne (local.get $errno) (i32.const 0))
                        (then (return (i32.add (local.get $errno) (i32.const 2000)))))
                    (i32.load (i32.const 3078))))
        "#;
        let outcome = run_wat(&profile, read_wat).expect("run completes");
        match outcome {
            GuestOutcome::Completed { code, .. } => assert_eq!(code, 7, "read exactly 7 bytes"),
            other => panic!("expected completion, got {other:?}"),
        }

        let escape_wat = r#"
            (module
                (import "wasi_snapshot_preview1" "path_open"
                    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 1024) "../../../e6a-outside-marker.txt")
                (func (export "main") (result i32)
                    (call $path_open (i32.const 3) (i32.const 0)
                        (i32.const 1024) (i32.const 31) (i32.const 0)
                        (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 2048))))
        "#;
        let outcome = run_wat(&profile, escape_wat).expect("run completes");
        match outcome {
            GuestOutcome::Completed { code, .. } => {
                assert_eq!(code, 76, "ENOTCAPABLE for traversal escape");
            }
            other => panic!("expected completion, got {other:?}"),
        }
    }

    #[test]
    fn readonly_preopen_denies_write_intent_open() {
        let root = scratch_dir("readonly");
        let grant = PreopenGrant::new("/workspace", &root, false).expect("grant");
        let profile = SandboxProfile::deny_all()
            .with_preopen(grant)
            .with_fuel(5_000_000);
        let wat = r#"
            (module
                (import "wasi_snapshot_preview1" "path_open"
                    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 1024) "new.txt")
                (func (export "main") (result i32)
                    ;; oflags CREAT (0x1) + fd_write right (1 << 6)
                    (call $path_open (i32.const 3) (i32.const 0)
                        (i32.const 1024) (i32.const 7) (i32.const 1)
                        (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 2048))))
        "#;
        let outcome = run_wat(&profile, wat).expect("run completes");
        match outcome {
            GuestOutcome::Completed { code, .. } => {
                assert_eq!(code, 76, "ENOTCAPABLE for write on read-only grant");
            }
            other => panic!("expected completion, got {other:?}"),
        }
        assert!(
            !root.join("new.txt").exists(),
            "no file may be created through a read-only grant"
        );
    }

    #[test]
    fn writable_preopen_allows_create_and_write() {
        let root = scratch_dir("writable");
        let grant = PreopenGrant::new("/workspace", &root, true).expect("grant");
        let profile = SandboxProfile::deny_all()
            .with_preopen(grant)
            .with_fuel(5_000_000);
        let wat = r#"
            (module
                (import "wasi_snapshot_preview1" "path_open"
                    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
                (import "wasi_snapshot_preview1" "fd_write"
                    (func $fd_write (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 1024) "made.txt")
                (data (i32.const 2048) "e6w")
                (func (export "main") (result i32)
                    (local $errno i32) (local $fd i32)
                    (local.set $errno (call $path_open (i32.const 3) (i32.const 0)
                        (i32.const 1024) (i32.const 8) (i32.const 1)
                        (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 2096)))
                    (if (i32.ne (local.get $errno) (i32.const 0)) (then (return (local.get $errno))))
                    (local.set $fd (i32.load (i32.const 2096)))
                    (i32.store (i32.const 2112) (i32.const 2048))
                    (i32.store (i32.const 2116) (i32.const 3))
                    (local.set $errno (call $fd_write (local.get $fd) (i32.const 2112) (i32.const 1) (i32.const 2120)))
                    (if (i32.ne (local.get $errno) (i32.const 0)) (then (return (i32.add (local.get $errno) (i32.const 1000)))))
                    (if (i32.ne (i32.load (i32.const 2120)) (i32.const 3)) (then (return (i32.const 2000))))
                    (i32.const 0)))
        "#;
        let outcome = run_wat(&profile, wat).expect("run completes");
        match outcome {
            GuestOutcome::Completed { code, .. } => assert_eq!(code, 0),
            other => panic!("expected completion, got {other:?}"),
        }
        let content = std::fs::read(root.join("made.txt")).expect("created file");
        assert_eq!(content, b"e6w");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_outside_root_is_denied() {
        let root = scratch_dir("symlink");
        let outside_target = root
            .parent()
            .expect("parent")
            .join("e6a-symlink-target.txt");
        std::fs::write(&outside_target, b"hostile").expect("outside target");
        std::os::unix::fs::symlink(&outside_target, root.join("innocent.txt")).expect("symlink");
        let grant = PreopenGrant::new("/workspace", &root, false).expect("grant");
        let profile = SandboxProfile::deny_all()
            .with_preopen(grant)
            .with_fuel(5_000_000);
        let wat = r#"
            (module
                (import "wasi_snapshot_preview1" "path_open"
                    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 1024) "innocent.txt")
                (func (export "main") (result i32)
                    (call $path_open (i32.const 3) (i32.const 0)
                        (i32.const 1024) (i32.const 12) (i32.const 0)
                        (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 2048))))
        "#;
        let outcome = run_wat(&profile, wat).expect("run completes");
        match outcome {
            GuestOutcome::Completed { code, .. } => {
                assert_eq!(code, 76, "ENOTCAPABLE for symlink escape");
            }
            other => panic!("expected completion, got {other:?}"),
        }
    }
}
