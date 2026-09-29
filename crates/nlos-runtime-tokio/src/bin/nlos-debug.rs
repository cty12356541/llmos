//! `nlos-debug` — minimal debugger face (W33-G, X-3 后半；决策点 2 边界).
//!
//! A read-only shell over the durable replay/snapshot surfaces that
//! ADR-0009/0012 landed and W25's lifecycle consumes: no live-attach, no
//! mutation of any authority store. `snapshot inspect` renders the durable
//! fiber face (process bindings, fiber incarnation registry, B-path entry
//! snapshots) plus the wait registry of one store directory; `replay`
//! walks one binding's event stream through the REAL projection machinery
//! (`BindingEventProjection` + `ResumePlan::all_pending`) and reports what
//! the resume machinery would restore, step by step, without arming
//! anything; `recovery` renders the artifact/semantic/resource recovery
//! ledgers and open commit plans of the task authority.
//!
//! # Usage
//!
//! ```text
//! nlos-debug snapshot inspect <STORE>
//! nlos-debug replay <STORE> (--task <HEX32> | --binding <HEX32>)
//! nlos-debug recovery <STORE> [--now <MS>]
//! ```
//!
//! `<STORE>` is one directory holding the authority databases under their
//! canonical names (`channel-authority.db`, `wait-authority.db`,
//! `process-authority.db`, and `task.sqlite3` — `tasks.sqlite3` accepted as
//! the slice-k alias). Missing faces degrade explicitly per command; the
//! wait face (with its channel) is required by `replay`, the task face by
//! `replay` with `--task` and by `recovery`.
//!
//! # Read-only discipline
//!
//! The authorities expose no read-only open, so the debugger enforces its
//! zero-mutation contract in three layers:
//!
//! 1. **Preflight**: every database file is first opened through a raw
//!    `SQLITE_OPEN_READ_ONLY` connection that checks `user_version` against
//!    the pinned current schema versions below. A store at any other
//!    version is refused — the debugger can never be the writer that
//!    creates or migrates an authority database. The enumeration face
//!    (bindings per task, process/fiber/snapshot listings — surfaces the
//!    authorities do not expose) reads only through these read-only
//!    connections.
//! 2. **Detail reads** go through the authorities' own inspect/list APIs
//!    (opened only after preflight). On an existing, current-schema, WAL
//!    database the authority open path performs no durable write:
//!    `journal_mode=WAL` on an already-WAL database is a no-op read,
//!    `synchronous`/`foreign_keys`/`busy_timeout` are connection-local, and
//!    the migration chain is skipped at the current version.
//! 3. **Tripwire**: after rendering, every touched database's `user_version`
//!    is re-read through a fresh read-only connection; any drift fails the
//!    run. The bin tests additionally assert a full logical dump of every
//!    table is byte-identical before and after each command.
//!
//! # Exit codes
//!
//! `0` success · `1` usage · `2` malformed input (hex, `--now`) · `3` store
//! failure (missing store/faces, schema pin mismatch, authority read
//! failure) · `4` selector target not found (unknown task) · `5` read-only
//! tripwire fired (unreachable in normal operation).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use nlos_channel::ChannelAuthority;
use nlos_runtime_tokio::{
    BindingEventProjection, BindingReplay, BindingReplayEvent, ReplayAuthorities, ResumePlan,
};
use nlos_task::{
    ArtifactCommitPlanId, ResourceCommitPlanId, SemanticCommitPlanId, SqliteTaskAuthority,
    TaskRecord, TaskStoreError,
};
use nlos_types::TaskId;
use nlos_wait::{BindingId, WaitAuthority, WaitState};
use rusqlite::{Connection, OpenFlags};

const USAGE: &str = "usage: nlos-debug snapshot inspect <STORE> \
  | replay <STORE> (--task <HEX32> | --binding <HEX32>) \
  | recovery <STORE> [--now <MS>]";

/// Debug-side pins of the authorities' current `user_version` values. The
/// authorities keep these `pub(crate)`, so the pins are validated by
/// `schema_pins_match_fresh_authority_stores`, which fails the moment an
/// authority bumps its schema and forces the pin update. A store at any
/// other version is refused in preflight (never migrated by the debugger).
const WAIT_SCHEMA_VERSION: i64 = 1;
const CHANNEL_SCHEMA_VERSION: i64 = 3;
const PROCESS_SCHEMA_VERSION: i64 = 5;
const TASK_SCHEMA_VERSION: i64 = 44;

/// Cap for recovery plan/alert listings; the debugger renders everything
/// present, never samples.
const RECOVERY_LIST_LIMIT: usize = 10_000;

/// Typed CLI failure, mapped onto the documented exit codes.
#[derive(Debug)]
enum ToolError {
    /// Bad invocation (exit 1).
    Usage,
    /// Malformed input: hex selector, `--now` value (exit 2).
    Input(String),
    /// Store failure: missing store or required face, schema pin mismatch,
    /// authority read failure (exit 3).
    Store(String),
    /// Selector target not found: unknown task (exit 4).
    NotFound(String),
    /// The read-only tripwire fired: a store's schema version drifted
    /// during the run (exit 5; unreachable in normal operation).
    ReadOnlyViolated(String),
}

impl ToolError {
    fn input(context: &str, detail: &str) -> Self {
        Self::Input(format!("{context}: {detail}"))
    }

    fn store(context: &str, detail: &str) -> Self {
        Self::Store(format!("{context}: {detail}"))
    }

    fn exit_code(&self) -> u8 {
        match self {
            Self::Usage => 1,
            Self::Input(_) => 2,
            Self::Store(_) => 3,
            Self::NotFound(_) => 4,
            Self::ReadOnlyViolated(_) => 5,
        }
    }

    fn render(&self) -> String {
        match self {
            Self::Usage => "bad invocation".to_string(),
            Self::Input(text) | Self::Store(text) | Self::NotFound(text) => text.clone(),
            Self::ReadOnlyViolated(text) => format!("read-only violation: {text}"),
        }
    }
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match run_command(&arguments) {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(ToolError::Usage) => {
            eprintln!("{USAGE}");
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("nlos-debug: {}", error.render());
            ExitCode::from(error.exit_code())
        }
    }
}

fn run_command(arguments: &[String]) -> Result<String, ToolError> {
    let Some(operation) = arguments.first().cloned() else {
        return Err(ToolError::Usage);
    };
    match operation.as_str() {
        "snapshot" => snapshot_command(&arguments[1..]),
        "replay" => replay_command(&arguments[1..]),
        "recovery" => recovery_command(&arguments[1..]),
        _ => Err(ToolError::Usage),
    }
}

// ---------------------------------------------------------------------------
// read-only store probe
// ---------------------------------------------------------------------------

/// The probed authority faces of one store directory, with the schema
/// versions the preflight observed (the tripwire's baseline).
struct StoreFace {
    root: PathBuf,
    wait: Option<PathBuf>,
    channel: Option<PathBuf>,
    process: Option<PathBuf>,
    task: Option<PathBuf>,
    observed_versions: Vec<(PathBuf, i64)>,
}

impl StoreFace {
    fn task_path(&self) -> Option<&Path> {
        self.task.as_deref()
    }

    /// The read-only tripwire: every schema version the preflight observed
    /// must still hold after the command rendered its output.
    fn assert_untouched(&self) -> Result<(), ToolError> {
        for (path, observed) in &self.observed_versions {
            let connection = open_read_only(path)?;
            let current = user_version(&connection)?;
            if current != *observed {
                return Err(ToolError::ReadOnlyViolated(format!(
                    "{} drifted from schema version {observed} to {current} during the run",
                    path.display()
                )));
            }
        }
        Ok(())
    }
}

fn probe_store(root: &Path) -> Result<StoreFace, ToolError> {
    if !root.is_dir() {
        return Err(ToolError::store(
            "probe store",
            &format!("store directory not found: {}", root.display()),
        ));
    }
    let mut observed = Vec::new();
    let wait = probe_database(
        &root.join("wait-authority.db"),
        WAIT_SCHEMA_VERSION,
        &mut observed,
    )?;
    let channel = probe_database(
        &root.join("channel-authority.db"),
        CHANNEL_SCHEMA_VERSION,
        &mut observed,
    )?;
    let process = probe_database(
        &root.join("process-authority.db"),
        PROCESS_SCHEMA_VERSION,
        &mut observed,
    )?;
    let task = match probe_database(
        &root.join("task.sqlite3"),
        TASK_SCHEMA_VERSION,
        &mut observed,
    )? {
        Some(path) => Some(path),
        None => probe_database(
            &root.join("tasks.sqlite3"),
            TASK_SCHEMA_VERSION,
            &mut observed,
        )?,
    };
    if wait.is_none() && channel.is_none() && process.is_none() && task.is_none() {
        return Err(ToolError::store(
            "probe store",
            &format!("no nlos authority database under {}", root.display()),
        ));
    }
    Ok(StoreFace {
        root: root.to_path_buf(),
        wait,
        channel,
        process,
        task,
        observed_versions: observed,
    })
}

/// Probes one database file: `None` when absent; `Some(path)` when present
/// and its `user_version` equals `expected`; a typed store failure when
/// present at any other version (the debugger never migrates).
fn probe_database(
    path: &Path,
    expected: i64,
    observed: &mut Vec<(PathBuf, i64)>,
) -> Result<Option<PathBuf>, ToolError> {
    let Ok(metadata) = std::fs::metadata(path) else {
        return Ok(None);
    };
    if !metadata.is_file() {
        return Err(ToolError::store(
            "probe store",
            &format!("{} is not a database file", path.display()),
        ));
    }
    let connection = open_read_only(path)?;
    let version = user_version(&connection)?;
    if version != expected {
        return Err(ToolError::store(
            "probe store",
            &format!(
                "{} is at schema version {version}, expected {expected}; \
                 the debugger never migrates an authority store",
                path.display()
            ),
        ));
    }
    observed.push((path.to_path_buf(), version));
    Ok(Some(path.to_path_buf()))
}

fn open_read_only(path: &Path) -> Result<Connection, ToolError> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|error| {
        ToolError::store("open read-only", &format!("{}: {error}", path.display()))
    })
}

fn user_version(connection: &Connection) -> Result<i64, ToolError> {
    connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| ToolError::store("read schema version", &error.to_string()))
}

// ---------------------------------------------------------------------------
// read-only enumeration (faces the authorities do not expose)
// ---------------------------------------------------------------------------

struct ProcessHeadRow {
    process_id: [u8; 16],
    generation: u64,
    lifecycle: i64,
    updated_at_ms: u64,
}

struct TerminalMarkerRow {
    process_id: [u8; 16],
    generation: u64,
    marked_at_ms: u64,
}

struct ProcessTaskRow {
    process_id: [u8; 16],
    generation: u64,
    task_id: [u8; 16],
    attempt_id: [u8; 16],
}

struct FiberHeadRow {
    process_id: [u8; 16],
    binding: [u8; 16],
    incarnation: u64,
    updated_at_ms: u64,
}

struct IncarnationRow {
    process_id: [u8; 16],
    binding: [u8; 16],
    incarnation: u64,
    created_at_ms: u64,
}

struct SnapshotRow {
    process_id: [u8; 16],
    binding: [u8; 16],
    digest: [u8; 32],
    written_by_incarnation: u64,
    written_at_ms: u64,
    input_len: u64,
}

fn blob16(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<[u8; 16]> {
    let value: Vec<u8> = row.get(index)?;
    value.try_into().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "expected a 16-byte id",
            )),
        )
    })
}

fn blob32(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<[u8; 32]> {
    let value: Vec<u8> = row.get(index)?;
    value.try_into().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "expected a 32-byte digest",
            )),
        )
    })
}

fn non_negative(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "expected a non-negative integer",
            )),
        )
    })
}

fn map_store(context: &'static str, error: &rusqlite::Error) -> ToolError {
    ToolError::store(context, &error.to_string())
}

fn process_heads(connection: &Connection) -> Result<Vec<ProcessHeadRow>, ToolError> {
    let mut statement = connection
        .prepare(
            "SELECT process_id, current_generation, lifecycle_state, updated_at_ms
             FROM process_heads ORDER BY process_id",
        )
        .map_err(|error| map_store("enumerate process heads", &error))?;
    let rows = statement
        .query_map([], |row| {
            Ok(ProcessHeadRow {
                process_id: blob16(row, 0)?,
                generation: non_negative(row, 1)?,
                lifecycle: row.get(2)?,
                updated_at_ms: non_negative(row, 3)?,
            })
        })
        .map_err(|error| map_store("enumerate process heads", &error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| map_store("enumerate process heads", &error))
}

fn terminal_markers(connection: &Connection) -> Result<Vec<TerminalMarkerRow>, ToolError> {
    let mut statement = connection
        .prepare(
            "SELECT process_id, process_generation, marked_at_ms
             FROM process_terminal_markers ORDER BY process_id, process_generation",
        )
        .map_err(|error| map_store("enumerate terminal markers", &error))?;
    let rows = statement
        .query_map([], |row| {
            Ok(TerminalMarkerRow {
                process_id: blob16(row, 0)?,
                generation: non_negative(row, 1)?,
                marked_at_ms: non_negative(row, 2)?,
            })
        })
        .map_err(|error| map_store("enumerate terminal markers", &error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| map_store("enumerate terminal markers", &error))
}

fn process_task_rows(connection: &Connection) -> Result<Vec<ProcessTaskRow>, ToolError> {
    let mut statement = connection
        .prepare(
            "SELECT process_id, process_generation, task_id, task_attempt_id
             FROM process_bindings ORDER BY process_id, process_generation",
        )
        .map_err(|error| map_store("enumerate process task bindings", &error))?;
    let rows = statement
        .query_map([], |row| {
            Ok(ProcessTaskRow {
                process_id: blob16(row, 0)?,
                generation: non_negative(row, 1)?,
                task_id: blob16(row, 2)?,
                attempt_id: blob16(row, 3)?,
            })
        })
        .map_err(|error| map_store("enumerate process task bindings", &error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| map_store("enumerate process task bindings", &error))
}

fn fiber_heads(connection: &Connection) -> Result<Vec<FiberHeadRow>, ToolError> {
    let mut statement = connection
        .prepare(
            "SELECT process_id, binding_id, current_incarnation, updated_at_ms
             FROM fiber_incarnation_heads ORDER BY process_id, binding_id",
        )
        .map_err(|error| map_store("enumerate fiber heads", &error))?;
    let rows = statement
        .query_map([], |row| {
            Ok(FiberHeadRow {
                process_id: blob16(row, 0)?,
                binding: blob16(row, 1)?,
                incarnation: non_negative(row, 2)?,
                updated_at_ms: non_negative(row, 3)?,
            })
        })
        .map_err(|error| map_store("enumerate fiber heads", &error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| map_store("enumerate fiber heads", &error))
}

fn incarnations(connection: &Connection) -> Result<Vec<IncarnationRow>, ToolError> {
    let mut statement = connection
        .prepare(
            "SELECT process_id, binding_id, incarnation_generation, created_at_ms
             FROM fiber_incarnations
             ORDER BY process_id, binding_id, incarnation_generation",
        )
        .map_err(|error| map_store("enumerate fiber incarnations", &error))?;
    let rows = statement
        .query_map([], |row| {
            Ok(IncarnationRow {
                process_id: blob16(row, 0)?,
                binding: blob16(row, 1)?,
                incarnation: non_negative(row, 2)?,
                created_at_ms: non_negative(row, 3)?,
            })
        })
        .map_err(|error| map_store("enumerate fiber incarnations", &error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| map_store("enumerate fiber incarnations", &error))
}

fn entry_snapshots(connection: &Connection) -> Result<Vec<SnapshotRow>, ToolError> {
    let mut statement = connection
        .prepare(
            "SELECT process_id, binding_id, input_digest, written_by_incarnation,
                    written_at_ms, length(handler_input)
             FROM fiber_entry_snapshots ORDER BY process_id, binding_id",
        )
        .map_err(|error| map_store("enumerate entry snapshots", &error))?;
    let rows = statement
        .query_map([], |row| {
            Ok(SnapshotRow {
                process_id: blob16(row, 0)?,
                binding: blob16(row, 1)?,
                digest: blob32(row, 2)?,
                written_by_incarnation: non_negative(row, 3)?,
                written_at_ms: non_negative(row, 4)?,
                input_len: non_negative(row, 5)?,
            })
        })
        .map_err(|error| map_store("enumerate entry snapshots", &error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| map_store("enumerate entry snapshots", &error))
}

/// Distinct fiber bindings that registered effects for `task_id`, straight
/// from the registration rows (the task authority exposes no by-task
/// listing).
fn effect_bindings_for_task(
    connection: &Connection,
    task_id: TaskId,
) -> Result<Vec<[u8; 16]>, ToolError> {
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT binding_id FROM effect_fiber_registrations
             WHERE task_id = ?1 ORDER BY binding_id",
        )
        .map_err(|error| map_store("enumerate task effect bindings", &error))?;
    let rows = statement
        .query_map([task_id.as_bytes().as_slice()], |row| blob16(row, 0))
        .map_err(|error| map_store("enumerate task effect bindings", &error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| map_store("enumerate task effect bindings", &error))
}

/// Distinct processes bound to `task_id` (process-side path from a task to
/// its fiber bindings).
fn processes_for_task(
    connection: &Connection,
    task_id: TaskId,
) -> Result<Vec<[u8; 16]>, ToolError> {
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT process_id FROM process_bindings
             WHERE task_id = ?1 ORDER BY process_id",
        )
        .map_err(|error| map_store("enumerate task processes", &error))?;
    let rows = statement
        .query_map([task_id.as_bytes().as_slice()], |row| blob16(row, 0))
        .map_err(|error| map_store("enumerate task processes", &error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| map_store("enumerate task processes", &error))
}

fn fiber_head_bindings(
    connection: &Connection,
    process_id: [u8; 16],
) -> Result<Vec<[u8; 16]>, ToolError> {
    let mut statement = connection
        .prepare(
            "SELECT binding_id FROM fiber_incarnation_heads
             WHERE process_id = ?1 ORDER BY binding_id",
        )
        .map_err(|error| map_store("enumerate process fibers", &error))?;
    let rows = statement
        .query_map([process_id.as_slice()], |row| blob16(row, 0))
        .map_err(|error| map_store("enumerate process fibers", &error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| map_store("enumerate process fibers", &error))
}

// ---------------------------------------------------------------------------
// rendering helpers
// ---------------------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn hex_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0_u8; N];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

fn lifecycle_name(code: i64) -> &'static str {
    match code {
        0 => "active",
        1 => "terminated",
        2 => "crashed",
        _ => "unknown",
    }
}

fn debug_lower(value: impl std::fmt::Debug) -> String {
    format!("{value:?}").to_ascii_lowercase()
}

fn optional_ms(value: Option<i64>) -> String {
    value.map_or_else(|| "-".to_string(), |ms| ms.to_string())
}

fn open_channel_and_wait(
    face: &StoreFace,
) -> Result<(Arc<ChannelAuthority>, WaitAuthority), ToolError> {
    if face.channel.is_none() {
        return Err(ToolError::store(
            "open wait registry",
            "wait authority store present but its channel authority store is missing",
        ));
    }
    let channel = Arc::new(
        ChannelAuthority::open(&face.root)
            .map_err(|error| ToolError::store("open channel authority", &error.to_string()))?,
    );
    let wait = WaitAuthority::open(&face.root, Arc::clone(&channel))
        .map_err(|error| ToolError::store("open wait authority", &error.to_string()))?;
    Ok((channel, wait))
}

// ---------------------------------------------------------------------------
// snapshot inspect
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)] // One renderer, one deterministic output shape.
fn snapshot_command(arguments: &[String]) -> Result<String, ToolError> {
    let Some((operation, rest)) = arguments.split_first() else {
        return Err(ToolError::Usage);
    };
    if operation != "inspect" {
        return Err(ToolError::Usage);
    }
    let mut store = None;
    parse_flags(rest, &mut |_flag, _value| false, &mut |positional| {
        if store.is_none() {
            store = Some(positional.to_string());
            true
        } else {
            false
        }
    })?;
    let Some(store) = store else {
        return Err(ToolError::Usage);
    };
    let face = probe_store(Path::new(&store))?;
    let mut out = String::new();
    let _ = writeln!(out, "store {}", face.root.display());
    render_face(
        &mut out,
        "wait-authority.db",
        "wait",
        face.wait.as_deref(),
        WAIT_SCHEMA_VERSION,
    );
    render_face(
        &mut out,
        "channel-authority.db",
        "channel",
        face.channel.as_deref(),
        CHANNEL_SCHEMA_VERSION,
    );
    render_face(
        &mut out,
        "process-authority.db",
        "process",
        face.process.as_deref(),
        PROCESS_SCHEMA_VERSION,
    );
    match &face.task {
        Some(path) => {
            let _ = writeln!(
                out,
                "face {} present schema={}",
                path.file_name().map_or_else(
                    || "?".to_string(),
                    |name| name.to_string_lossy().into_owned()
                ),
                TASK_SCHEMA_VERSION
            );
        }
        None => {
            let _ = writeln!(out, "face task absent");
        }
    }

    if face.wait.is_some() {
        let (_channel, wait) = open_channel_and_wait(&face)?;
        let waits = wait
            .list_waits(None)
            .map_err(|error| ToolError::store("list waits", &error.to_string()))?;
        let _ = writeln!(out, "wait-registry waits={}", waits.len());
        for record in &waits {
            let _ = writeln!(
                out,
                "  wait id={} binding={} channel={} target={} state={} registered_at={} woken_at={} woken_up_to={} cancelled_at={}",
                hex(record.wait_id.as_bytes()),
                hex(record.binding.as_bytes()),
                hex(record.channel_id.as_bytes()),
                record.target_sequence,
                record.state,
                record.registered_at_ms,
                record.woken_at_ms,
                record.woken_up_to_sequence,
                record.cancelled_at_ms
            );
        }
    } else {
        let _ = writeln!(out, "wait-registry absent");
    }

    if let Some(connection) = face.process.as_deref().map(open_read_only).transpose()? {
        let heads = process_heads(&connection)?;
        let markers = terminal_markers(&connection)?;
        let tasks = process_task_rows(&connection)?;
        let fibers = fiber_heads(&connection)?;
        let history = incarnations(&connection)?;
        let snapshots = entry_snapshots(&connection)?;
        let _ = writeln!(out, "process-registry processes={}", heads.len());
        for head in &heads {
            if head.lifecycle == 0 {
                let _ = writeln!(
                    out,
                    "  process id={} generation={} lifecycle=active updated_at={}",
                    hex(&head.process_id),
                    head.generation,
                    head.updated_at_ms
                );
                continue;
            }
            match markers
                .iter()
                .find(|marker| marker.process_id == head.process_id)
            {
                Some(marker) => {
                    let _ = writeln!(
                        out,
                        "  process id={} generation={} lifecycle={} at_generation={} marked_at={}",
                        hex(&head.process_id),
                        head.generation,
                        lifecycle_name(head.lifecycle),
                        marker.generation,
                        marker.marked_at_ms
                    );
                }
                None => {
                    let _ = writeln!(
                        out,
                        "  process id={} generation={} lifecycle={}",
                        hex(&head.process_id),
                        head.generation,
                        lifecycle_name(head.lifecycle)
                    );
                }
            }
        }
        for task in &tasks {
            let _ = writeln!(
                out,
                "  process-task process={} generation={} task={} attempt={}",
                hex(&task.process_id),
                task.generation,
                hex(&task.task_id),
                hex(&task.attempt_id)
            );
        }
        let _ = writeln!(out, "fiber-registry fibers={}", fibers.len());
        for fiber in &fibers {
            let _ = writeln!(
                out,
                "  fiber binding={} process={} incarnation={} updated_at={}",
                hex(&fiber.binding),
                hex(&fiber.process_id),
                fiber.incarnation,
                fiber.updated_at_ms
            );
        }
        for row in &history {
            let _ = writeln!(
                out,
                "  fiber-incarnation binding={} process={} incarnation={} created_at={}",
                hex(&row.binding),
                hex(&row.process_id),
                row.incarnation,
                row.created_at_ms
            );
        }
        let _ = writeln!(out, "entry-snapshots count={}", snapshots.len());
        for snapshot in &snapshots {
            let _ = writeln!(
                out,
                "  entry-snapshot process={} binding={} incarnation={} digest={} input_len={} written_at={}",
                hex(&snapshot.process_id),
                hex(&snapshot.binding),
                snapshot.written_by_incarnation,
                hex(&snapshot.digest),
                snapshot.input_len,
                snapshot.written_at_ms
            );
        }
    } else {
        let _ = writeln!(out, "process-registry absent");
        let _ = writeln!(out, "fiber-registry absent");
        let _ = writeln!(out, "entry-snapshots absent");
    }
    face.assert_untouched()?;
    Ok(out)
}

fn render_face(out: &mut String, file: &str, name: &str, path: Option<&Path>, schema: i64) {
    if path.is_some() {
        let _ = writeln!(out, "face {file} present schema={schema}");
    } else {
        let _ = writeln!(out, "face {name} absent");
    }
}

// ---------------------------------------------------------------------------
// replay
// ---------------------------------------------------------------------------

enum Selector {
    Task(TaskId),
    Binding([u8; 16]),
}

#[allow(clippy::too_many_lines)] // One renderer, one deterministic output shape.
fn replay_command(arguments: &[String]) -> Result<String, ToolError> {
    let mut store = None;
    let mut task = None;
    let mut binding = None;
    parse_flags(
        arguments,
        &mut |flag, value| match flag {
            "task" => {
                task = Some(value.to_string());
                true
            }
            "binding" => {
                binding = Some(value.to_string());
                true
            }
            _ => false,
        },
        &mut |positional| {
            if store.is_none() {
                store = Some(positional.to_string());
                true
            } else {
                false
            }
        },
    )?;
    let Some(store) = store else {
        return Err(ToolError::Usage);
    };
    let selector = match (task, binding) {
        (Some(task), None) => {
            let bytes = hex_array::<16>(&task)
                .ok_or_else(|| ToolError::input("replay", "--task must be exactly 32 hex chars"))?;
            Selector::Task(TaskId::from_bytes(bytes))
        }
        (None, Some(binding)) => {
            let bytes = hex_array::<16>(&binding).ok_or_else(|| {
                ToolError::input("replay", "--binding must be exactly 32 hex chars")
            })?;
            Selector::Binding(bytes)
        }
        (Some(_), Some(_)) => {
            return Err(ToolError::input(
                "replay",
                "--task and --binding are mutually exclusive",
            ));
        }
        (None, None) => return Err(ToolError::Usage),
    };
    let face = probe_store(Path::new(&store))?;
    render_replay(&face, &selector)
}

fn render_replay(face: &StoreFace, selector: &Selector) -> Result<String, ToolError> {
    if face.wait.is_none() {
        return Err(ToolError::store(
            "replay",
            "the wait authority store is required by replay",
        ));
    }
    let mut out = String::new();
    let _ = writeln!(out, "replay store={}", face.root.display());

    let task_connection = face.task_path().map(open_read_only).transpose()?;
    let process_connection = face.process.as_deref().map(open_read_only).transpose()?;

    let bindings = match selector {
        Selector::Binding(binding) => vec![*binding],
        Selector::Task(task_id) => {
            let Some(task_path) = face.task_path() else {
                return Err(ToolError::store(
                    "replay",
                    "the task authority store is required by --task",
                ));
            };
            let task_authority = SqliteTaskAuthority::open(task_path)
                .map_err(|error| ToolError::store("open task authority", &error.to_string()))?;
            let record = task_authority
                .inspect_task(*task_id)
                .map_err(|error| match error {
                    TaskStoreError::TaskNotFound => ToolError::NotFound(format!(
                        "task {} is not registered",
                        hex(task_id.as_bytes())
                    )),
                    other => ToolError::store("inspect task", &other.to_string()),
                })?;
            render_task_header(&mut out, &record);
            let task_read = task_connection.as_ref().expect("task face open");
            let mut bindings = effect_bindings_for_task(task_read, *task_id)?;
            if let Some(connection) = process_connection.as_ref() {
                for process_id in processes_for_task(connection, *task_id)? {
                    for binding in fiber_head_bindings(connection, process_id)? {
                        bindings.push(binding);
                    }
                }
            }
            bindings.sort_unstable();
            bindings.dedup();
            bindings
        }
    };

    let task_authority = face
        .task_path()
        .map(SqliteTaskAuthority::open)
        .transpose()
        .map_err(|error| ToolError::store("open task authority", &error.to_string()))?;
    let snapshots = match process_connection.as_ref() {
        Some(connection) => entry_snapshots(connection)?,
        None => Vec::new(),
    };

    let (channel, wait) = open_channel_and_wait(face)?;
    for binding in bindings {
        render_binding_walkthrough(
            &mut out,
            channel.as_ref(),
            &wait,
            task_authority.as_ref(),
            binding,
            &snapshots,
        )?;
    }
    face.assert_untouched()?;
    Ok(out)
}

fn render_task_header(out: &mut String, record: &TaskRecord) {
    let _ = writeln!(
        out,
        "task id={} state={} generation={} head_commit_seq={} permit={}",
        hex(record.task_id.as_bytes()),
        debug_lower(record.state),
        record.task_generation.get(),
        record.head_commit_seq,
        record
            .active_permit
            .as_ref()
            .map_or_else(|| "none".to_string(), |id| hex(id.as_bytes()))
    );
}

#[allow(clippy::too_many_lines)] // One renderer, one deterministic output shape.
fn render_binding_walkthrough(
    out: &mut String,
    channel: &ChannelAuthority,
    wait: &WaitAuthority,
    task: Option<&SqliteTaskAuthority>,
    binding: [u8; 16],
    snapshots: &[SnapshotRow],
) -> Result<(), ToolError> {
    let sources = ReplayAuthorities {
        channel: Some(channel),
        task,
        process: None,
    };
    let replay = BindingEventProjection::project(wait, sources, BindingId::from_bytes(binding))
        .map_err(|error| ToolError::store("project binding replay", &error.to_string()))?;
    let _ = writeln!(
        out,
        "binding {} events={}",
        hex(&binding),
        replay.events.len()
    );
    let plan = ResumePlan::all_pending(&replay);
    render_replay_events(out, &replay);
    let rearms = plan
        .rearm_wait_ids
        .iter()
        .map(|wait_id| hex(wait_id.as_bytes()))
        .collect::<Vec<_>>()
        .join(",");
    let _ = writeln!(out, "  resume-plan path=A rearm=[{rearms}]");
    let matches: Vec<&SnapshotRow> = snapshots
        .iter()
        .filter(|snapshot| snapshot.binding == binding)
        .collect();
    if matches.is_empty() {
        let _ = writeln!(out, "  resume-plan path=B entry-snapshot absent");
    } else {
        for snapshot in matches {
            let _ = writeln!(
                out,
                "  resume-plan path=B entry-snapshot process={} incarnation={} digest={} input_len={}",
                hex(&snapshot.process_id),
                snapshot.written_by_incarnation,
                hex(&snapshot.digest),
                snapshot.input_len
            );
        }
    }
    Ok(())
}

fn render_replay_events(out: &mut String, replay: &BindingReplay) {
    for (index, event) in replay.events.iter().enumerate() {
        match event {
            BindingReplayEvent::Wait(event) => {
                let action = match event.record.state {
                    WaitState::Pending => "would-rearm",
                    WaitState::Woken => "already-woken",
                    WaitState::Cancelled => "cancelled",
                };
                let _ = writeln!(
                    out,
                    "  event {} kind=wait at={} wait={} channel={} target={} state={} action={}",
                    index + 1,
                    event.record.registered_at_ms,
                    hex(event.record.wait_id.as_bytes()),
                    hex(event.record.channel_id.as_bytes()),
                    event.record.target_sequence,
                    event.record.state,
                    action
                );
            }
            BindingReplayEvent::Effect(event) => {
                let _ = writeln!(
                    out,
                    "  event {} kind=effect at={} registration={} task={} slot_seq={} slot_state={} effect_receipt={} action=report-only",
                    index + 1,
                    u64::try_from(event.registration.registered_at_ms).unwrap_or(u64::MAX),
                    hex(event.registration.registration_id.as_bytes()),
                    hex(event.registration.task_id.as_bytes()),
                    event.registration.effect_seq,
                    debug_lower(event.registration.slot_state),
                    event
                        .registration
                        .effect_receipt_id
                        .as_ref()
                        .map_or_else(|| "-".to_string(), |id| hex(id.as_bytes()))
                );
            }
            BindingReplayEvent::QueueConsumed(event) => {
                let _ = writeln!(
                    out,
                    "  event {} kind=queue at={} registration={} channel={} sequence={} action=report-only",
                    index + 1,
                    event.registration.registered_at_ms,
                    hex(event.registration.registration_id.as_bytes()),
                    hex(event.registration.channel_id.as_bytes()),
                    event.registration.sequence
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// recovery
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)] // Three mirror domains in one renderer.
fn recovery_command(arguments: &[String]) -> Result<String, ToolError> {
    let mut store = None;
    let mut now = None;
    parse_flags(
        arguments,
        &mut |flag, value| match flag {
            "now" => {
                now = Some(value.to_string());
                true
            }
            _ => false,
        },
        &mut |positional| {
            if store.is_none() {
                store = Some(positional.to_string());
                true
            } else {
                false
            }
        },
    )?;
    let Some(store) = store else {
        return Err(ToolError::Usage);
    };
    // `i64::MAX` is the deterministic default: every non-finalized plan is
    // listed regardless of retry backoff ("open plans", not "due now").
    let now_ms = match now {
        Some(text) => text.parse::<i64>().map_err(|_| {
            ToolError::input("recovery", "--now must be a decimal millisecond timestamp")
        })?,
        None => i64::MAX,
    };
    let face = probe_store(Path::new(&store))?;
    let Some(task_path) = face.task_path() else {
        return Err(ToolError::store(
            "recovery",
            "the task authority store is required by recovery",
        ));
    };
    let task = SqliteTaskAuthority::open(task_path)
        .map_err(|error| ToolError::store("open task authority", &error.to_string()))?;
    let mut out = String::new();
    let _ = writeln!(out, "recovery store={}", face.root.display());
    let _ = writeln!(out, "now {now_ms}");

    let artifact_summary = task
        .summarize_artifact_recovery()
        .map_err(|error| ToolError::store("summarize artifact recovery", &error.to_string()))?;
    let _ = writeln!(
        out,
        "artifact retrying={} escalated={} unacknowledged={} resolved={}",
        artifact_summary.retrying,
        artifact_summary.escalated,
        artifact_summary.unacknowledged_escalated,
        artifact_summary.resolved
    );
    for plan in task
        .list_due_artifact_commit_plans(RECOVERY_LIST_LIMIT, now_ms)
        .map_err(|error| ToolError::store("list artifact plans", &error.to_string()))?
    {
        let _ = writeln!(
            out,
            "  artifact-plan id={} task={} state={} created_at={} updated_at={}",
            hex(plan.plan_id.as_bytes()),
            hex(plan.task_id.as_bytes()),
            debug_lower(plan.state),
            plan.created_at_ms,
            plan.updated_at_ms
        );
        render_artifact_recovery(&mut out, &task, plan.plan_id)?;
    }
    for alert in task
        .list_artifact_recovery_alerts(RECOVERY_LIST_LIMIT)
        .map_err(|error| ToolError::store("list artifact alerts", &error.to_string()))?
    {
        let _ = writeln!(
            out,
            "  artifact-alert plan={} state={} consecutive={} total={} source={} next_retry={} acknowledged={}",
            hex(alert.recovery.plan_id.as_bytes()),
            debug_lower(alert.recovery.state),
            alert.recovery.consecutive_failures,
            alert.recovery.total_failures,
            debug_lower(alert.recovery.last_source),
            optional_ms(alert.recovery.next_retry_at_ms),
            if alert.acknowledgement.is_some() {
                "yes"
            } else {
                "no"
            }
        );
    }

    let semantic_summary = task
        .summarize_semantic_recovery()
        .map_err(|error| ToolError::store("summarize semantic recovery", &error.to_string()))?;
    let _ = writeln!(
        out,
        "semantic retrying={} escalated={} unacknowledged={} resolved={}",
        semantic_summary.retrying,
        semantic_summary.escalated,
        semantic_summary.unacknowledged_escalated,
        semantic_summary.resolved
    );
    for plan in task
        .list_due_semantic_commit_plans(RECOVERY_LIST_LIMIT, now_ms)
        .map_err(|error| ToolError::store("list semantic plans", &error.to_string()))?
    {
        let _ = writeln!(
            out,
            "  semantic-plan id={} task={} state={} created_at={} updated_at={}",
            hex(plan.plan_id.as_bytes()),
            hex(plan.task_id.as_bytes()),
            debug_lower(plan.state),
            plan.created_at_ms,
            plan.updated_at_ms
        );
        render_semantic_recovery(&mut out, &task, plan.plan_id)?;
    }
    for alert in task
        .list_semantic_recovery_alerts()
        .map_err(|error| ToolError::store("list semantic alerts", &error.to_string()))?
    {
        let _ = writeln!(
            out,
            "  semantic-alert plan={} state={} consecutive={} total={} source={} next_retry={} acknowledged={}",
            hex(alert.recovery.plan_id.as_bytes()),
            debug_lower(alert.recovery.state),
            alert.recovery.consecutive_failures,
            alert.recovery.total_failures,
            debug_lower(alert.recovery.last_source),
            optional_ms(alert.recovery.next_retry_at_ms),
            if alert.acknowledgement.is_some() {
                "yes"
            } else {
                "no"
            }
        );
    }

    let resource_summary = task
        .summarize_resource_recovery()
        .map_err(|error| ToolError::store("summarize resource recovery", &error.to_string()))?;
    let _ = writeln!(
        out,
        "resource retrying={} escalated={} unacknowledged={} resolved={}",
        resource_summary.retrying,
        resource_summary.escalated,
        resource_summary.unacknowledged_escalated,
        resource_summary.resolved
    );
    for plan in task
        .list_due_resource_commit_plans(RECOVERY_LIST_LIMIT, now_ms)
        .map_err(|error| ToolError::store("list resource plans", &error.to_string()))?
    {
        let _ = writeln!(
            out,
            "  resource-plan id={} task={} state={} created_at={} updated_at={}",
            hex(plan.plan_id.as_bytes()),
            hex(plan.task_id.as_bytes()),
            debug_lower(plan.state),
            plan.created_at_ms,
            plan.updated_at_ms
        );
        render_resource_recovery(&mut out, &task, plan.plan_id)?;
    }
    for alert in task
        .list_resource_recovery_alerts()
        .map_err(|error| ToolError::store("list resource alerts", &error.to_string()))?
    {
        let _ = writeln!(
            out,
            "  resource-alert plan={} state={} consecutive={} total={} source={} next_retry={} acknowledged={}",
            hex(alert.recovery.plan_id.as_bytes()),
            debug_lower(alert.recovery.state),
            alert.recovery.consecutive_failures,
            alert.recovery.total_failures,
            debug_lower(alert.recovery.last_source),
            optional_ms(alert.recovery.next_retry_at_ms),
            if alert.acknowledgement.is_some() {
                "yes"
            } else {
                "no"
            }
        );
    }
    face.assert_untouched()?;
    Ok(out)
}

fn render_artifact_recovery(
    out: &mut String,
    task: &SqliteTaskAuthority,
    plan_id: ArtifactCommitPlanId,
) -> Result<(), ToolError> {
    let Some(record) = task
        .inspect_artifact_recovery(plan_id)
        .map_err(|error| ToolError::store("inspect artifact recovery", &error.to_string()))?
    else {
        return Ok(());
    };
    let _ = writeln!(
        out,
        "  artifact-recovery plan={} state={} consecutive={} total={} source={} next_retry={} escalated_at={} resolved_at={}",
        hex(plan_id.as_bytes()),
        debug_lower(record.state),
        record.consecutive_failures,
        record.total_failures,
        debug_lower(record.last_source),
        optional_ms(record.next_retry_at_ms),
        optional_ms(record.escalated_at_ms),
        optional_ms(record.resolved_at_ms)
    );
    Ok(())
}

fn render_semantic_recovery(
    out: &mut String,
    task: &SqliteTaskAuthority,
    plan_id: SemanticCommitPlanId,
) -> Result<(), ToolError> {
    let Some(record) = task
        .inspect_semantic_recovery(plan_id)
        .map_err(|error| ToolError::store("inspect semantic recovery", &error.to_string()))?
    else {
        return Ok(());
    };
    let _ = writeln!(
        out,
        "  semantic-recovery plan={} state={} consecutive={} total={} source={} next_retry={} escalated_at={} resolved_at={}",
        hex(plan_id.as_bytes()),
        debug_lower(record.state),
        record.consecutive_failures,
        record.total_failures,
        debug_lower(record.last_source),
        optional_ms(record.next_retry_at_ms),
        optional_ms(record.escalated_at_ms),
        optional_ms(record.resolved_at_ms)
    );
    Ok(())
}

fn render_resource_recovery(
    out: &mut String,
    task: &SqliteTaskAuthority,
    plan_id: ResourceCommitPlanId,
) -> Result<(), ToolError> {
    let Some(record) = task
        .inspect_resource_recovery(plan_id)
        .map_err(|error| ToolError::store("inspect resource recovery", &error.to_string()))?
    else {
        return Ok(());
    };
    let _ = writeln!(
        out,
        "  resource-recovery plan={} state={} consecutive={} total={} source={} next_retry={} escalated_at={} resolved_at={}",
        hex(plan_id.as_bytes()),
        debug_lower(record.state),
        record.consecutive_failures,
        record.total_failures,
        debug_lower(record.last_source),
        optional_ms(record.next_retry_at_ms),
        optional_ms(record.escalated_at_ms),
        optional_ms(record.resolved_at_ms)
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// argument parsing (house discipline)
// ---------------------------------------------------------------------------

fn parse_flags(
    arguments: &[String],
    on_flag: &mut dyn FnMut(&str, &str) -> bool,
    on_positional: &mut dyn FnMut(&str) -> bool,
) -> Result<(), ToolError> {
    let mut index = 0;
    while index < arguments.len() {
        let token = arguments[index].as_str();
        if let Some(flag) = token.strip_prefix("--") {
            let Some(value) = arguments.get(index + 1) else {
                return Err(ToolError::Usage);
            };
            if value.starts_with("--") || !on_flag(flag, value) {
                return Err(ToolError::Usage);
            }
            index += 2;
        } else if !on_positional(token) {
            return Err(ToolError::Usage);
        } else {
            index += 1;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{ToolError, hex_array, run_command};
    use std::fmt::Write as _;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use nlos_channel::{
        ChannelAuthority, ChannelDecision, CreateChannelRequest, EnqueueDecision, EnqueueRequest,
        RegisterQueueConsumptionRequest,
    };
    use nlos_process::{
        CreateIsolationDomainRequest, FiberEntrySnapshotDecision, IsolationDomainDecision,
        ProcessAuthority, ProcessBindingDecision, ProcessTerminalDecision,
        RegisterDelegatedProcessRequest, RegisterFiberIncarnationRequest,
        WriteFiberEntrySnapshotRequest,
    };
    use nlos_task::{
        ArtifactPublicationExpectation, ArtifactRecoveryFailureRequest,
        ArtifactRecoveryFailureSource, ArtifactRecoveryState, AttemptSpec, Authorities,
        LogicalEffectDescriptor, PermitDecision, PermitRequest, PlanArtifactCommitRequest,
        PlannedEffect, RegisterEffectBindingRequest, SnapshotBundle, SqliteTaskAuthority, TaskSpec,
        artifact_publication_plan_root, empty_effect_history_root,
    };
    use nlos_types::{
        CancellationScopeId, Generation, IdempotencyKey, TaskAttemptId, TaskId, TaskSnapshotId,
    };
    use nlos_wait::{
        BindingId, CancelDecision, CancelWaitRequest, NotifyCommitsRequest, RegisterDecision,
        RegisterWaitRequest, WaitAuthority,
    };
    use rusqlite::{Connection, OpenFlags};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        root: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "nlos-debug-{tag}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&root).expect("create temp root");
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct Fixture {
        root: TempDir,
        first_process: [u8; 16],
    }

    fn key(seed: u8) -> IdempotencyKey {
        IdempotencyKey::from_bytes([seed; 16])
    }

    fn binding(seed: u8) -> [u8; 16] {
        [seed; 16]
    }

    fn hex_of(bytes: &[u8]) -> String {
        let mut text = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            let _ = write!(text, "{byte:02x}");
        }
        text
    }

    /// The W33-G fixture store: one channel with two enqueued entries; the
    /// wait registry holds one woken, one pending and one cancelled wait for
    /// binding 1 plus a foreign pending wait for binding 2; the task
    /// authority registers task A's effect on binding 1 and an escalated
    /// artifact recovery ledger for task R; the process authority binds two
    /// active processes for task A (fibers 1 and 3), records binding 1's
    /// entry snapshot under the first process, and crashes a third process
    /// for an unrelated task. Returns the store root and the first
    /// process's authority-derived id (process ids are never caller-chosen).
    #[allow(clippy::too_many_lines)] // One fixture assembling four authorities end to end.
    fn build_fixture(tag: &str) -> Fixture {
        let root = TempDir::new(tag);
        let channel = ChannelAuthority::open(root.path()).expect("open channel authority");
        let channel_record = match channel
            .create_channel(CreateChannelRequest {
                capacity_bytes: 4_096,
                policy_digest: [0x44; 32],
                idempotency_key: key(201),
                created_at_ms: 900,
            })
            .expect("create channel")
        {
            ChannelDecision::Created(record) => record,
            ChannelDecision::Replayed(_) => panic!("fresh create cannot replay"),
        };
        let channel_id = channel_record.channel_id;
        for seed in [2_u8, 3_u8] {
            let head = channel.inspect_channel(channel_id).expect("channel head");
            match channel
                .enqueue(EnqueueRequest {
                    channel_id,
                    expected_generation: head.generation,
                    expected_fencing_token: head.fencing_token,
                    payload: vec![seed; 8],
                    idempotency_key: key(seed),
                    enqueued_at_ms: 1_250 + u64::from(seed),
                })
                .expect("enqueue")
            {
                EnqueueDecision::Enqueued(_) => {}
                EnqueueDecision::Replayed(_) => panic!("fresh enqueue cannot replay"),
            }
        }
        channel
            .register_queue_consumption(RegisterQueueConsumptionRequest {
                channel_id,
                sequence: 1,
                binding: nlos_types::ExecutionFiberId::from_bytes(binding(1)),
                fiber_generation: Generation::INITIAL,
                idempotency_key: key(4),
                registered_at_ms: 1_500,
            })
            .expect("register consumption");

        let wait = WaitAuthority::open(root.path(), Arc::new(channel)).expect("open wait");
        let wait_request = |target: u64, at: u64, seed: u8| RegisterWaitRequest {
            binding: BindingId::from_bytes(binding(1)),
            channel_id,
            target_sequence: target,
            idempotency_key: key(seed),
            registered_at_ms: at,
        };
        let woken = match wait
            .register_wait(wait_request(1, 1_050, 11))
            .expect("register woken")
        {
            RegisterDecision::Registered(record) => record,
            RegisterDecision::Replayed(_) => panic!("fresh register cannot replay"),
        };
        match wait
            .register_wait(wait_request(5, 1_100, 12))
            .expect("register pending")
        {
            RegisterDecision::Registered(_) => {}
            RegisterDecision::Replayed(_) => panic!("fresh register cannot replay"),
        }
        let cancelled = match wait
            .register_wait(wait_request(9, 1_150, 13))
            .expect("register cancelled")
        {
            RegisterDecision::Registered(record) => record,
            RegisterDecision::Replayed(_) => panic!("fresh register cannot replay"),
        };
        match wait
            .register_wait(RegisterWaitRequest {
                binding: BindingId::from_bytes(binding(2)),
                channel_id,
                target_sequence: 5,
                idempotency_key: key(14),
                registered_at_ms: 1_200,
            })
            .expect("register foreign wait")
        {
            RegisterDecision::Registered(_) => {}
            RegisterDecision::Replayed(_) => panic!("fresh register cannot replay"),
        }
        let wake = wait
            .notify_commits(NotifyCommitsRequest {
                channel_id,
                up_to_sequence: 1,
                notified_at_ms: 2_000,
                idempotency_key: key(205),
            })
            .expect("notify commits");
        assert_eq!(wake.woken.len(), 1);
        assert_eq!(wake.woken[0].wait_id, woken.wait_id);
        assert_eq!(wake.woken[0].state, nlos_wait::WaitState::Woken);
        match wait
            .cancel_wait(CancelWaitRequest {
                wait_id: cancelled.wait_id,
                cancelled_at_ms: 2_100,
                idempotency_key: key(206),
            })
            .expect("cancel wait")
        {
            CancelDecision::Cancelled(_) => {}
            CancelDecision::Replayed(_) => panic!("fresh cancel cannot replay"),
        }
        drop(wait);

        let task = SqliteTaskAuthority::open(root.path().join("task.sqlite3"))
            .expect("open task authority");
        let task_a = TaskId::from_bytes([0x21; 16]);
        task.register_task(TaskSpec {
            task_id: task_a,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
            application_id: None,
            plan_revision: None,
        })
        .expect("register task A");
        let attempt_a = TaskAttemptId::from_bytes([0x22; 16]);
        task.register_attempt(AttemptSpec {
            task_id: task_a,
            attempt_id: attempt_a,
            attempt_generation: Generation::INITIAL,
            snapshot: SnapshotBundle {
                snapshot_id: TaskSnapshotId::from_bytes([0x23; 16]),
                snapshot_digest: [0x24; 32],
                expected_head_commit_seq: 0,
                effect_history_root: empty_effect_history_root(),
                retry_fence_epoch: 0,
            },
            cancellation_scope_id: CancellationScopeId::from_bytes([0x25; 16]),
            cancellation_generation: Generation::INITIAL,
            idempotency_key: key(26),
            registered_at_ms: 1_050,
        })
        .expect("register attempt A");
        let permit_a = match task
            .request_commit_permit_with_authorities_struct(
                Authorities::default(),
                PermitRequest {
                    task_id: task_a,
                    attempt_id: attempt_a,
                    attempt_generation: Generation::INITIAL,
                    write_set_root: [0x27; 32],
                    planned_effects: vec![PlannedEffect {
                        descriptor: LogicalEffectDescriptor {
                            task_id: task_a,
                            task_generation: Generation::INITIAL,
                            intent_spec_id: [0x28; 32],
                            stable_action_slot: 1,
                            target_authority_object_id: [0x29; 32],
                            effect_class: 7,
                            idempotency_scope: 3,
                        },
                        required: false,
                        required_condition_digest: None,
                        success_criteria_digest: [0x2a; 32],
                        action_proposal_digest: [0x2b; 32],
                    }],
                    idempotency_key: key(30),
                    valid_until_ms: 99_999,
                    requested_at_ms: 1_100,
                },
            )
            .expect("request permit A")
        {
            PermitDecision::Issued(record) => record,
            other => panic!("expected issued permit, got {other:?}"),
        };
        task.register_effect_binding(RegisterEffectBindingRequest {
            task_id: task_a,
            attempt_id: attempt_a,
            attempt_generation: Generation::INITIAL,
            permit_id: permit_a.permit_id,
            permit_epoch: permit_a.permit_epoch,
            effect_seq: 0,
            binding: nlos_types::ExecutionFiberId::from_bytes(binding(1)),
            fiber_generation: Generation::INITIAL,
            idempotency_key: key(31),
            registered_at_ms: 1_400,
        })
        .expect("register effect binding");

        let plan_artifact = |task_id: [u8; 16], attempt_id: [u8; 16], seed: u8| {
            let task_id = TaskId::from_bytes(task_id);
            let attempt_id = TaskAttemptId::from_bytes(attempt_id);
            task.register_task(TaskSpec {
                task_id,
                task_generation: Generation::INITIAL,
                registered_at_ms: 1_000,
                application_id: None,
                plan_revision: None,
            })
            .expect("register plan task");
            task.register_attempt(AttemptSpec {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                snapshot: SnapshotBundle {
                    snapshot_id: TaskSnapshotId::from_bytes([seed; 16]),
                    snapshot_digest: [seed.wrapping_add(1); 32],
                    expected_head_commit_seq: 0,
                    effect_history_root: empty_effect_history_root(),
                    retry_fence_epoch: 0,
                },
                cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(2); 16]),
                cancellation_generation: Generation::INITIAL,
                idempotency_key: key(seed.wrapping_add(3)),
                registered_at_ms: 2_000,
            })
            .expect("register plan attempt");
            let expectation = ArtifactPublicationExpectation {
                staging_id: [seed.wrapping_add(4); 16],
                artifact_id: nlos_types::ArtifactId::from_bytes([seed.wrapping_add(5); 16]),
                target_revision: 1,
                digest: [seed.wrapping_add(6); 32],
                size_bytes: 128,
            };
            let permit = match task
                .request_commit_permit_with_authorities_struct(
                    Authorities::default(),
                    PermitRequest {
                        task_id,
                        attempt_id,
                        attempt_generation: Generation::INITIAL,
                        write_set_root: artifact_publication_plan_root(&[expectation])
                            .expect("plan root"),
                        planned_effects: Vec::new(),
                        idempotency_key: key(seed.wrapping_add(7)),
                        valid_until_ms: 99_999,
                        requested_at_ms: 3_000,
                    },
                )
                .expect("request plan permit")
            {
                PermitDecision::Issued(record) => record,
                other => panic!("expected issued permit, got {other:?}"),
            };
            let plan = task
                .plan_artifact_commit(PlanArtifactCommitRequest {
                    task_id,
                    attempt_id,
                    attempt_generation: Generation::INITIAL,
                    permit_id: permit.permit_id,
                    expectations: vec![expectation],
                    idempotency_key: key(seed.wrapping_add(8)),
                    planned_at_ms: 4_000,
                })
                .expect("plan artifact commit");
            plan.record().clone()
        };

        // The escalated ledger: three failures under threshold 3 escalate
        // task R's plan (alert unacknowledged).
        let escalated_plan = plan_artifact([0x31; 16], [0x32; 16], 0x51);
        let mut total = 0_u64;
        let mut observed = 5_000_i64;
        for source in [
            ArtifactRecoveryFailureSource::ArtifactAuthority,
            ArtifactRecoveryFailureSource::TaskAuthority,
        ] {
            let record = task
                .record_artifact_recovery_failure(ArtifactRecoveryFailureRequest {
                    plan_id: escalated_plan.plan_id,
                    expected_total_failures: total,
                    source,
                    observed_at_ms: observed,
                    base_delay_ms: 100,
                    max_delay_ms: 1_000,
                    escalation_threshold: 3,
                })
                .expect("record recovery failure");
            assert_eq!(record.state, ArtifactRecoveryState::Retrying);
            total = record.total_failures;
            observed = record.next_retry_at_ms.expect("retry while retrying");
        }
        let escalated = task
            .record_artifact_recovery_failure(ArtifactRecoveryFailureRequest {
                plan_id: escalated_plan.plan_id,
                expected_total_failures: total,
                source: ArtifactRecoveryFailureSource::Coordinator,
                observed_at_ms: observed,
                base_delay_ms: 100,
                max_delay_ms: 1_000,
                escalation_threshold: 3,
            })
            .expect("escalate");
        assert_eq!(escalated.state, ArtifactRecoveryState::Escalated);

        // The retrying ledger: one failure keeps task R2's plan retrying
        // with a concrete next-retry time (the due-plan surface).
        let retrying_plan = plan_artifact([0x41; 16], [0x42; 16], 0x61);
        let retrying = task
            .record_artifact_recovery_failure(ArtifactRecoveryFailureRequest {
                plan_id: retrying_plan.plan_id,
                expected_total_failures: 0,
                source: ArtifactRecoveryFailureSource::TaskAuthority,
                observed_at_ms: 5_000,
                base_delay_ms: 100,
                max_delay_ms: 1_000,
                escalation_threshold: 3,
            })
            .expect("record retrying failure");
        assert_eq!(retrying.state, ArtifactRecoveryState::Retrying);
        assert!(retrying.next_retry_at_ms.is_some());
        drop(task);

        let process = ProcessAuthority::open(root.path()).expect("open process authority");
        let domain = match process
            .create_isolation_domain(CreateIsolationDomainRequest {
                policy_digest: [0x51; 32],
                idempotency_key: key(52),
                created_at_ms: 900,
            })
            .expect("create domain")
        {
            IsolationDomainDecision::Created(record) => record,
            other @ IsolationDomainDecision::Replayed(_) => {
                panic!("expected created domain, got {other:?}")
            }
        };
        let register_process = |task_id: [u8; 16], attempt: [u8; 16], seed: u8| match process
            .register_delegated_process(RegisterDelegatedProcessRequest {
                task_id: TaskId::from_bytes(task_id),
                task_attempt_id: TaskAttemptId::from_bytes(attempt),
                attempt_generation: Generation::INITIAL,
                isolation_domain_id: nlos_types::IsolationDomainId::from_bytes(
                    domain.isolation_domain_id.into_bytes(),
                ),
                isolation_domain_generation: domain.generation,
                isolation_domain_fencing_token: domain.fencing_token,
                idempotency_key: key(seed),
                created_at_ms: 950,
            })
            .expect("register process")
        {
            ProcessBindingDecision::Registered(record) => record,
            other @ ProcessBindingDecision::Replayed(_) => {
                panic!("expected registered process, got {other:?}")
            }
        };
        let first = register_process([0x21; 16], [0x22; 16], 53);
        let second = register_process([0x21; 16], [0x62; 16], 63);
        let third = register_process([0x99; 16], [0x92; 16], 73);
        let register_incarnation =
            |process_id, generation, token, fiber: [u8; 16], seed: u8| match process
                .register_fiber_incarnation(RegisterFiberIncarnationRequest {
                    process_id,
                    expected_process_generation: generation,
                    expected_process_fencing_token: token,
                    binding: nlos_types::ExecutionFiberId::from_bytes(fiber),
                    idempotency_key: key(seed),
                    registered_at_ms: 990,
                })
                .expect("register incarnation")
            {
                nlos_process::FiberIncarnationDecision::Registered(record) => record,
                other @ nlos_process::FiberIncarnationDecision::Replayed(_) => {
                    panic!("expected registered incarnation, got {other:?}")
                }
            };
        let first_incarnation = register_incarnation(
            first.process_id,
            first.process_generation,
            first.process_fencing_token,
            binding(1),
            54,
        );
        register_incarnation(
            second.process_id,
            second.process_generation,
            second.process_fencing_token,
            binding(3),
            64,
        );
        match process
            .write_fiber_entry_snapshot(WriteFiberEntrySnapshotRequest {
                process_id: first.process_id,
                binding: nlos_types::ExecutionFiberId::from_bytes(binding(1)),
                expected_incarnation_generation: first_incarnation.incarnation_generation,
                handler_input: b"handler-entry-input".to_vec(),
                written_at_ms: 2_500,
            })
            .expect("write entry snapshot")
        {
            FiberEntrySnapshotDecision::Written(_) => {}
            FiberEntrySnapshotDecision::Replayed(_) => panic!("fresh write cannot replay"),
        }
        match process
            .propagate_crash(nlos_process::PropagateCrashRequest {
                process_id: third.process_id,
                expected_process_generation: third.process_generation,
                expected_process_fencing_token: third.process_fencing_token,
                idempotency_key: key(74),
                marked_at_ms: 2_600,
            })
            .expect("propagate crash")
        {
            ProcessTerminalDecision::Marked(_) => {}
            ProcessTerminalDecision::Replayed(_) => panic!("fresh crash cannot replay"),
        }
        drop(process);
        let first_process = *first.process_id.as_bytes();
        Fixture {
            root,
            first_process,
        }
    }

    fn dump_all(root: &Path) -> String {
        let mut dump = String::new();
        let mut names: Vec<PathBuf> = fs::read_dir(root)
            .expect("read store dir")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension().is_some_and(|extension| extension == "db")
                    || path
                        .extension()
                        .is_some_and(|extension| extension == "sqlite3")
            })
            .collect();
        names.sort_unstable();
        for path in names {
            let _ = writeln!(
                dump,
                "== {} ==",
                path.file_name().unwrap().to_string_lossy()
            );
            let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("open read-only for dump");
            let tables: Vec<String> = connection
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type='table'
                     AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )
                .expect("list tables")
                .query_map([], |row| row.get(0))
                .expect("list tables")
                .collect::<Result<Vec<_>, _>>()
                .expect("list tables");
            for table in tables {
                let _ = writeln!(dump, "-- {table} --");
                let mut statement = connection
                    .prepare(&format!("SELECT rowid, * FROM {table} ORDER BY rowid"))
                    .expect("select table");
                let columns = statement.column_count();
                let mut rows = statement.query([]).expect("select table");
                while let Some(row) = rows.next().expect("select table") {
                    for column in 0..columns {
                        match row.get_ref(column).expect("column value") {
                            rusqlite::types::ValueRef::Null => dump.push_str("null;"),
                            rusqlite::types::ValueRef::Integer(value) => {
                                let _ = write!(dump, "{value};");
                            }
                            rusqlite::types::ValueRef::Real(value) => {
                                let _ = write!(dump, "{value};");
                            }
                            rusqlite::types::ValueRef::Text(value) => {
                                let _ = write!(dump, "t{};", hex_of(value));
                            }
                            rusqlite::types::ValueRef::Blob(value) => {
                                let _ = write!(dump, "b{};", hex_of(value));
                            }
                        }
                    }
                    dump.push('\n');
                }
            }
        }
        dump
    }

    fn run(arguments: &[&str]) -> Result<String, ToolError> {
        let owned: Vec<String> = arguments.iter().map(ToString::to_string).collect();
        run_command(&owned)
    }

    fn exit_of(result: &Result<String, ToolError>) -> u8 {
        match result {
            Ok(_) => 0,
            Err(error) => error.exit_code(),
        }
    }

    fn occurrences(text: &str, needle: &str) -> usize {
        text.matches(needle).count()
    }

    #[test]
    fn hex_selector_parsing_accepts_exact_hex_and_rejects_shapes() {
        assert_eq!(
            hex_array::<16>("0102030405060708090a0b0c0d0e0f10"),
            Some([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
        );
        assert_eq!(hex_array::<16>("0102"), None);
        assert_eq!(hex_array::<16>("zz0102030405060708090a0b0c0d0e0f10"), None);
        assert_eq!(hex_array::<16>(""), None);
    }

    #[test]
    fn snapshot_inspect_renders_expected_durable_state() {
        let fixture = build_fixture("snapshot");
        let output = run(&["snapshot", "inspect", fixture.root.path().to_str().unwrap()])
            .expect("snapshot inspect renders");
        assert_eq!(occurrences(&output, "state=pending"), 2);
        assert_eq!(occurrences(&output, "state=woken"), 1);
        assert_eq!(occurrences(&output, "state=cancelled"), 1);
        assert!(output.contains("wait-registry waits=4"));
        assert!(output.contains("process-registry processes=3"));
        assert!(output.contains("lifecycle=crashed at_generation=1 marked_at=2600"));
        assert_eq!(occurrences(&output, "lifecycle=active"), 2);
        assert!(output.contains(&format!("fiber binding={} ", hex_of(&binding(1)))));
        assert!(output.contains(&format!("fiber binding={} ", hex_of(&binding(3)))));
        assert!(output.contains(&format!(
            "entry-snapshot process={} binding={} incarnation=1 digest=",
            hex_of(&fixture.first_process),
            hex_of(&binding(1))
        )));
        assert!(output.contains("input_len=19 written_at=2500"));
        assert!(output.contains("face task.sqlite3 present schema=44"));
    }

    #[test]
    fn replay_walkthrough_uses_real_projection_and_is_deterministic() {
        let fixture = build_fixture("replay");
        let binding_hex = hex_of(&binding(1));
        let arguments = [
            "replay",
            fixture.root.path().to_str().unwrap(),
            "--binding",
            binding_hex.as_str(),
        ];
        let first = run(&arguments).expect("first walkthrough renders");
        let second = run(&arguments).expect("second walkthrough renders");
        assert_eq!(first, second, "replay output must be deterministic");

        assert!(first.contains("binding 01010101010101010101010101010101 events=5"));
        let order: Vec<(usize, &str)> = ["at=1050", "at=1100", "at=1150", "at=1400", "at=1500"]
            .iter()
            .map(|needle| {
                (
                    first.find(needle).expect("event timestamp present"),
                    *needle,
                )
            })
            .collect();
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(order, sorted, "events must render registration-ordered");

        assert_eq!(occurrences(&first, "kind=wait"), 3);
        assert_eq!(occurrences(&first, "kind=effect"), 1);
        assert_eq!(occurrences(&first, "kind=queue"), 1);
        assert_eq!(occurrences(&first, "action=already-woken"), 1);
        assert_eq!(occurrences(&first, "action=would-rearm"), 1);
        assert_eq!(occurrences(&first, "action=cancelled"), 1);
        assert_eq!(occurrences(&first, "action=report-only"), 2);
        assert_eq!(occurrences(&first, "slot_state=planned"), 1);
        let pending_line = first
            .lines()
            .find(|line| line.contains("action=would-rearm"))
            .expect("pending wait line");
        let wait_id = pending_line
            .split("wait=")
            .nth(1)
            .expect("wait id in line")
            .split(' ')
            .next()
            .expect("wait id token")
            .to_string();
        assert!(first.contains(&format!("resume-plan path=A rearm=[{wait_id}]")));
        assert!(first.contains(&format!(
            "resume-plan path=B entry-snapshot process={} incarnation=1",
            hex_of(&fixture.first_process)
        )));

        let foreign = run(&[
            "replay",
            fixture.root.path().to_str().unwrap(),
            "--binding",
            &hex_of(&binding(2)),
        ])
        .expect("foreign walkthrough renders");
        assert!(foreign.contains("binding 02020202020202020202020202020202 events=1"));
        assert!(foreign.contains("resume-plan path=B entry-snapshot absent"));
    }

    #[test]
    fn replay_task_selector_resolves_bindings_across_authorities() {
        let fixture = build_fixture("replay-task");
        let output = run(&[
            "replay",
            fixture.root.path().to_str().unwrap(),
            "--task",
            &hex_of(&[0x21; 16]),
        ])
        .expect("task walkthrough renders");
        assert!(output.contains("task id=21212121212121212121212121212121 state=active"));
        assert!(output.contains(&format!("binding {} ", hex_of(&binding(1)))));
        assert!(output.contains(&format!("binding {} ", hex_of(&binding(3)))));

        let unknown = run(&[
            "replay",
            fixture.root.path().to_str().unwrap(),
            "--task",
            &hex_of(&[0xee; 16]),
        ]);
        assert_eq!(exit_of(&unknown), 4);
    }

    #[test]
    fn recovery_renders_three_domains_and_ledger_states() {
        let fixture = build_fixture("recovery");
        let output =
            run(&["recovery", fixture.root.path().to_str().unwrap()]).expect("recovery renders");
        assert!(output.contains("artifact retrying=1 escalated=1 unacknowledged=1 resolved=0"));
        assert!(output.contains("semantic retrying=0 escalated=0 unacknowledged=0 resolved=0"));
        assert!(output.contains("resource retrying=0 escalated=0 unacknowledged=0 resolved=0"));
        assert!(output.contains("artifact-plan id="));
        assert_eq!(occurrences(&output, "artifact-recovery plan="), 1);
        assert!(output.contains("state=retrying"));
        assert!(output.contains("next_retry=5"));
        assert!(output.contains("artifact-alert plan="));
        assert!(output.contains("state=escalated"));
        assert!(output.contains("source=coordinator"));
        assert!(output.contains("acknowledged=no"));
        assert!(!output.contains("acknowledged=yes"));
    }

    #[test]
    fn exit_codes_are_typed() {
        let fixture = build_fixture("exits");
        let root = fixture.root.path().to_str().unwrap();
        assert_eq!(exit_of(&run(&[])), 1);
        assert_eq!(exit_of(&run(&["snap", root])), 1);
        assert_eq!(exit_of(&run(&["replay", root])), 1);
        assert_eq!(exit_of(&run(&["replay", root, "--binding", "0102"])), 2);
        assert_eq!(
            exit_of(&run(&[
                "replay",
                root,
                "--binding",
                &hex_of(&binding(1)),
                "--now",
                "5"
            ])),
            1,
            "unknown flag is usage"
        );
        assert_eq!(
            exit_of(&run(&["snapshot", "inspect", "/nonexistent-store"])),
            3
        );
        let empty = TempDir::new("empty");
        assert_eq!(
            exit_of(&run(&[
                "snapshot",
                "inspect",
                empty.path().to_str().unwrap()
            ])),
            3
        );
        assert_eq!(exit_of(&run(&["recovery", root, "--now", "x"])), 2);

        let mismatch = build_fixture("schema-pin");
        let wait_db = mismatch.root.path().join("wait-authority.db");
        let connection = Connection::open(&wait_db).expect("open wait db read-write");
        connection
            .pragma_update(None, "user_version", 99)
            .expect("tamper version");
        drop(connection);
        assert_eq!(
            exit_of(&run(&[
                "snapshot",
                "inspect",
                mismatch.root.path().to_str().unwrap()
            ])),
            3
        );
    }

    #[test]
    fn debugger_leaves_every_authority_store_logically_unchanged() {
        let fixture = build_fixture("readonly");
        let root = fixture.root.path().to_str().unwrap();
        let before = dump_all(fixture.root.path());
        run(&["snapshot", "inspect", root]).expect("snapshot renders");
        run(&["replay", root, "--task", &hex_of(&[0x21; 16])]).expect("replay renders");
        run(&["recovery", root, "--now", "6000"]).expect("recovery renders");
        let after = dump_all(fixture.root.path());
        assert_eq!(
            before, after,
            "every command must leave the store untouched"
        );
    }

    #[test]
    #[ignore = "manual smoke: materializes the fixture store for running the real nlos-debug binary"]
    fn leave_fixture_store_for_manual_smoke() {
        let fixture = build_fixture("smoke");
        let destination = PathBuf::from("nlos-debug-smoke-store");
        let _ = fs::remove_dir_all(&destination);
        fs::create_dir_all(&destination).expect("create smoke store dir");
        for entry in fs::read_dir(fixture.root.path()).expect("read fixture") {
            let path = entry.expect("fixture entry").path();
            if path.is_file() {
                let name = path.file_name().expect("fixture file name");
                fs::copy(&path, destination.join(name)).expect("copy fixture file");
            }
        }
        println!(
            "smoke store at {}",
            destination.canonicalize().unwrap_or(destination).display()
        );
    }

    #[test]
    fn schema_pins_match_fresh_authority_stores() {
        let root = TempDir::new("pins");
        let channel = ChannelAuthority::open(root.path()).expect("open channel");
        let wait = WaitAuthority::open(root.path(), Arc::new(channel)).expect("open wait");
        drop(wait);
        let process = ProcessAuthority::open(root.path()).expect("open process");
        drop(process);
        let task = SqliteTaskAuthority::open(root.path().join("task.sqlite3")).expect("open task");
        drop(task);
        let version = |name: &str| {
            let connection = Connection::open_with_flags(
                root.path().join(name),
                OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .expect("open read-only");
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<usize, i64>(0))
                .expect("user_version")
        };
        assert_eq!(version("wait-authority.db"), super::WAIT_SCHEMA_VERSION);
        assert_eq!(
            version("channel-authority.db"),
            super::CHANNEL_SCHEMA_VERSION
        );
        assert_eq!(
            version("process-authority.db"),
            super::PROCESS_SCHEMA_VERSION
        );
        assert_eq!(version("task.sqlite3"), super::TASK_SCHEMA_VERSION);
    }
}
