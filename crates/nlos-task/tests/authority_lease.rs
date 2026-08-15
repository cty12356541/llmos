//! Acceptance tests for the schema-v27 durable `TaskAuthority` lease/term
//! primitive. The slice proves local `SQLite` fencing and restart readback; it
//! does not claim IPC peer authentication or full `PermitAdoption` semantics.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    AuthorityLeaseDecision, AuthorityLeaseRequest, MAX_AUTHORITY_LEASE_TTL_MS, SqliteTaskAuthority,
    TaskStoreError,
};
use nlos_types::{IdempotencyKey, ProcessId};
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
                "nlos-task-authority-lease-{name}-{}-{sequence}.sqlite3",
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
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn process(seed: u8) -> ProcessId {
    ProcessId::from_bytes([seed; 16])
}

fn request(holder: u8, key: u8, at_ms: i64, ttl_ms: i64) -> AuthorityLeaseRequest {
    AuthorityLeaseRequest {
        holder_id: process(holder),
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        requested_at_ms: at_ms,
        ttl_ms,
    }
}

fn record(decision: AuthorityLeaseDecision) -> nlos_task::AuthorityLeaseRecord {
    decision.record()
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the complete lease fence lifecycle.
fn lease_renewal_takeover_and_restart_fence_old_terms() {
    let database = TestDatabase::new("lifecycle");
    let authority = database.open();

    assert!(matches!(
        authority.inspect_authority_lease(),
        Err(TaskStoreError::AuthorityLeaseNotFound)
    ));
    let first_request = request(1, 0xa1, 100, 50);
    let first = record(
        authority
            .acquire_authority_lease(first_request)
            .expect("initial lease"),
    );
    assert_eq!(first.term, 1);
    assert_eq!(first.lease_epoch, 1);
    assert_eq!(first.expires_at_ms, 150);
    authority
        .validate_authority_lease(first, 149)
        .expect("initial lease is live");

    assert!(matches!(
        authority.acquire_authority_lease(request(2, 0xa2, 120, 50)),
        Err(TaskStoreError::AuthorityLeaseHeld)
    ));
    assert!(matches!(
        authority.acquire_authority_lease(request(1, 0xa1, 100, 51)),
        Err(TaskStoreError::InvalidAuthorityLease { .. })
    ));
    assert!(matches!(
        authority.acquire_authority_lease(first_request),
        Ok(AuthorityLeaseDecision::Replayed(replayed)) if replayed == first
    ));

    let renewed = record(
        authority
            .acquire_authority_lease(request(1, 0xa3, 120, 50))
            .expect("renew lease"),
    );
    assert_eq!(renewed.term, first.term, "renewal stays in the same term");
    assert_eq!(renewed.lease_epoch, 2);
    assert_ne!(renewed.fencing_token, first.fencing_token);
    assert!(matches!(
        authority.validate_authority_lease(first, 130),
        Err(TaskStoreError::AuthorityLeaseFenced)
    ));
    assert!(matches!(
        authority.validate_authority_lease(renewed, 170),
        Err(TaskStoreError::AuthorityLeaseExpired)
    ));

    let takeover = record(
        authority
            .acquire_authority_lease(request(2, 0xa4, 171, 100))
            .expect("expired lease takeover"),
    );
    assert_eq!(takeover.term, 2);
    assert_eq!(takeover.lease_epoch, 3);
    assert_ne!(takeover.fencing_token, renewed.fencing_token);
    assert!(matches!(
        authority.validate_authority_lease(renewed, 180),
        Err(TaskStoreError::AuthorityLeaseFenced)
    ));
    assert!(matches!(
        authority.acquire_authority_lease(first_request),
        Ok(AuthorityLeaseDecision::Replayed(replayed)) if replayed == first
    ));
    authority
        .validate_authority_lease(takeover, 270)
        .expect("takeover lease is live");

    let authority_id = takeover.authority_id;
    drop(authority);
    let reopened = database.open();
    assert_eq!(
        reopened.inspect_authority_lease().expect("lease replay"),
        takeover
    );
    assert_eq!(
        reopened
            .inspect_authority_lease()
            .expect("authority id")
            .authority_id,
        authority_id
    );

    let raw = Connection::open(&database.path).expect("raw connection");
    let history_count: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM task_authority_lease_history",
            [],
            |row| row.get(0),
        )
        .expect("history count");
    assert_eq!(
        history_count, 3,
        "acquire, renew, takeover are durable facts"
    );
    assert!(
        raw.execute(
            "UPDATE task_authority_lease_history SET transition_kind = 2
             WHERE authority_id = ?1 AND lease_epoch = ?2",
            rusqlite::params![
                authority_id.as_bytes().as_slice(),
                [0u8, 0, 0, 0, 0, 0, 0, 1].as_slice()
            ],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM task_authority_lease_history WHERE authority_id = ?1 AND lease_epoch = ?2",
            rusqlite::params![
                authority_id.as_bytes().as_slice(),
                [0u8, 0, 0, 0, 0, 0, 0, 1].as_slice()
            ],
        )
        .is_err()
    );
}

#[test]
fn lease_request_bounds_and_validation_fail_closed() {
    let database = TestDatabase::new("bounds");
    let authority = database.open();
    assert!(matches!(
        authority.acquire_authority_lease(request(1, 0xb1, -1, 10)),
        Err(TaskStoreError::InvalidAuthorityLease { .. })
    ));
    assert!(matches!(
        authority.acquire_authority_lease(request(1, 0xb2, 1, 0)),
        Err(TaskStoreError::InvalidAuthorityLease { .. })
    ));
    assert!(matches!(
        authority.acquire_authority_lease(request(1, 0xb3, 1, MAX_AUTHORITY_LEASE_TTL_MS + 1,)),
        Err(TaskStoreError::InvalidAuthorityLease { .. })
    ));
}
