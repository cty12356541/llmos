//! W26-001 schema acceptance: the v42 migration lands the Semantic
//! recovery ledger table group — one recovery row per semantic commit
//! plan plus immutable alert receipts — additively on top of v41, and
//! the open path re-runs the guard idempotently on every reopen.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::SqliteTaskAuthority;
use rusqlite::Connection;

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-task-semantic-recovery-{name}-{}-{sequence}.sqlite3",
                std::process::id()
            )),
        }
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).expect("open task authority")
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            suffix_path(&self.path, "-wal"),
            suffix_path(&self.path, "-shm"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database: {error}"),
            }
        }
    }
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.to_path_buf().into_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[test]
fn schema_v42_creates_semantic_recovery_tables_idempotently() {
    let database = TestDatabase::new("v42");
    // Opening a fresh database runs the full chain to the current
    // version: both new tables exist and user_version = 42.
    drop(database.open());
    let raw = Connection::open(&database.path).expect("open raw connection");
    let count: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table'
             AND name IN ('task_semantic_recovery',
                          'task_semantic_recovery_alert_receipts')",
            [],
            |row| row.get(0),
        )
        .expect("count semantic recovery tables");
    assert_eq!(count, 2);
    let version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read user_version");
    assert_eq!(version, 42);
    drop(raw);

    // Reopening the same root re-runs the migration guard idempotently.
    drop(database.open());
    let raw = Connection::open(&database.path).expect("reopen raw connection");
    let version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("reread user_version");
    assert_eq!(version, 42);
}
