//! Clock-trust acceptance tests for the authority-lease takeover judgement
//! (W23-005).
//!
//! `acquire_authority_lease_anchored` clamps the incumbent-lease liveness
//! judgement with a caller-supplied wall-clock observation: an incumbent is
//! judged dead only when expired at `min(requested_at_ms, clock_wall_ms)`,
//! so a self-reported timestamp can only make the lease look more alive,
//! never widen the window in which a live lease is taken over with
//! `term + 1`. `clock_wall_ms == 0` ("no observation available") fails
//! closed: the incumbent is treated as live. The historical
//! `acquire_authority_lease` entry keeps its exact legacy contract; those
//! semantics are pinned by `authority_lease.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    AuthorityLeaseDecision, AuthorityLeaseRequest, SqliteTaskAuthority, TaskStoreError,
};
use nlos_types::{IdempotencyKey, ProcessId};

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-task-lease-clock-trust-{name}-{}-{sequence}.sqlite3",
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

/// A self-reported request timestamp ten minutes ahead of the wall must not
/// turn a live incumbent lease into a takeover: the challenger is rejected
/// with `AuthorityLeaseHeld` and the incumbent lease bytes plus fencing
/// token stay exactly as issued.
#[test]
fn future_self_reported_time_cannot_take_over_live_lease() {
    let database = TestDatabase::new("future-request");
    let authority = database.open();
    let wall_ms = 120u64;
    let first = authority
        .acquire_authority_lease(request(0xa1, 0xb1, 100, 50))
        .expect("initial lease")
        .record();
    assert_eq!(first.expires_at_ms, 150, "lease is live at the wall");

    let malicious = request(0xa2, 0xb2, 120 + 600_000, 100);
    assert!(matches!(
        authority.acquire_authority_lease_anchored(malicious, wall_ms),
        Err(TaskStoreError::AuthorityLeaseHeld)
    ));

    assert_eq!(
        authority.inspect_authority_lease().expect("lease readback"),
        first,
        "incumbent lease bytes are untouched"
    );
    authority
        .validate_authority_lease(first, 149)
        .expect("incumbent fencing token still validates");
}

/// The anchored entry keeps the legacy takeover semantics when the request
/// time and the wall observation agree the incumbent is expired: term and
/// lease epoch advance and the old fencing token never validates again.
#[test]
fn anchored_takeover_of_expired_lease_keeps_legacy_semantics() {
    let database = TestDatabase::new("expired-takeover");
    let authority = database.open();
    let first = authority
        .acquire_authority_lease(request(0xa1, 0xb1, 100, 100))
        .expect("initial lease")
        .record();
    assert_eq!(first.expires_at_ms, 200);

    let decision = authority
        .acquire_authority_lease_anchored(request(0xa2, 0xb2, 201, 100), 201)
        .expect("expired lease takeover");
    assert!(matches!(decision, AuthorityLeaseDecision::TakenOver(_)));
    let takeover = decision.record();
    assert_eq!(takeover.term, 2);
    assert_eq!(takeover.lease_epoch, 2);
    assert_eq!(takeover.expires_at_ms, 301);
    assert_ne!(takeover.fencing_token, first.fencing_token);
    assert!(matches!(
        authority.validate_authority_lease(first, 250),
        Err(TaskStoreError::AuthorityLeaseFenced)
    ));
    authority
        .validate_authority_lease(takeover, 300)
        .expect("successor lease is live");
}

/// Pinned zero-anchor semantics: `clock_wall_ms == 0` declares "no wall
/// observation available" and fails closed. An initial acquisition (nothing
/// to fence) still succeeds and the incumbent holder still renews, but a
/// challenger can never prove an incumbent dead without an observation; it
/// must retry once it has one.
#[test]
fn zero_wall_observation_fails_closed_against_takeover() {
    let database = TestDatabase::new("zero-anchor");
    let authority = database.open();

    let first = authority
        .acquire_authority_lease_anchored(request(0xa1, 0xb1, 100, 100), 0)
        .expect("initial acquisition needs no anchor")
        .record();
    assert_eq!(first.term, 1);
    assert_eq!(first.expires_at_ms, 200);

    // Lease is genuinely dead at 250, but the challenger has no observation.
    assert!(matches!(
        authority.acquire_authority_lease_anchored(request(0xa2, 0xb2, 250, 100), 0),
        Err(TaskStoreError::AuthorityLeaseHeld)
    ));

    // The same challenger with a real observation takes over immediately.
    let takeover = authority
        .acquire_authority_lease_anchored(request(0xa2, 0xb2, 250, 100), 250)
        .expect("takeover once an observation exists")
        .record();
    assert_eq!(takeover.term, 2);
    assert_eq!(takeover.expires_at_ms, 350);

    // The incumbent itself still renews without an observation: no anchor
    // must not create a liveness deadlock for the current holder.
    let renewed = authority
        .acquire_authority_lease_anchored(request(0xa2, 0xb3, 300, 100), 0)
        .expect("incumbent renews without an observation")
        .record();
    assert_eq!(renewed.term, 2, "renewal never advances the term");
    assert_eq!(renewed.lease_epoch, 3);
    assert_eq!(renewed.expires_at_ms, 400);
    assert!(matches!(
        authority.validate_authority_lease(takeover, 360),
        Err(TaskStoreError::AuthorityLeaseFenced)
    ));
}

/// A request timestamp lagging behind the wall (normal clock jitter, or a
/// deliberately slow self-report) must not widen the takeover window: the
/// judgement takes the earlier of the two times, so the lease looks more
/// alive, never more dead, than the wall alone would show.
#[test]
fn lagging_self_reported_time_does_not_widen_takeover_window() {
    let database = TestDatabase::new("lagging-request");
    let authority = database.open();
    let first = authority
        .acquire_authority_lease(request(0xa1, 0xb1, 100, 100))
        .expect("initial lease")
        .record();
    assert_eq!(first.expires_at_ms, 200);

    // Wall says the lease is dead at 250, self-report lags at 100.
    assert!(matches!(
        authority.acquire_authority_lease_anchored(request(0xa2, 0xb2, 100, 100), 250),
        Err(TaskStoreError::AuthorityLeaseHeld)
    ));
    assert_eq!(
        authority.inspect_authority_lease().expect("lease readback"),
        first,
        "incumbent lease bytes are untouched"
    );

    // Corrected request time at the same wall: takeover proceeds.
    let takeover = authority
        .acquire_authority_lease_anchored(request(0xa2, 0xb2, 250, 100), 250)
        .expect("takeover once the request time agrees with the wall")
        .record();
    assert_eq!(takeover.term, 2);
}
