//! B-CLOCK-001: happy-path and fail-closed gate tests for the durable local
//! monotonic clock authority (`AuthorityClock`).
//!
//! Covers: first-call initialization, exactly-one advance per distinct key,
//! durable replay without re-advance (no double-jump), restart persistence
//! of the high-water (never regresses), unknown schema version rejection,
//! raw-regression guard baseline, and reading-space exhaustion fail-closed.

use nlos_clock::{AuthorityClock, AuthorityClockError, NowDecision, NowRequest, Reading};
use nlos_types::IdempotencyKey;
use rusqlite::Connection;

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn request(seed: u8) -> NowRequest {
    NowRequest {
        idempotency_key: key(seed),
    }
}

fn ticked(clock: &AuthorityClock, seed: u8) -> Reading {
    match clock.now(request(seed)).expect("now must tick") {
        NowDecision::Tick(reading) => reading,
        NowDecision::Replayed(reading) => {
            panic!("fresh key cannot replay, got {reading}")
        }
    }
}

fn replayed(clock: &AuthorityClock, seed: u8) -> Reading {
    match clock.now(request(seed)).expect("now must replay") {
        NowDecision::Replayed(reading) => reading,
        NowDecision::Tick(reading) => panic!("expected Replayed, got Tick {reading}"),
    }
}

struct TestRoot(std::path::PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("nlos-clock-test-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("create test root");
        Self(base)
    }

    fn base(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn clock_database(base: &std::path::Path) -> std::path::PathBuf {
    base.join("clock-authority.db")
}

fn raw_count(database: &std::path::Path, sql: &str) -> i64 {
    let connection = Connection::open(database).expect("open raw reader");
    connection
        .query_row(sql, [], |row| row.get(0))
        .expect("count rows")
}

fn assert_counts(base: &std::path::Path, expected: [i64; 2]) {
    for (table, want) in ["watermark", "tick_receipts"].iter().zip(expected) {
        assert_eq!(
            raw_count(
                &clock_database(base),
                &format!("SELECT COUNT(*) FROM {table}")
            ),
            want,
            "unexpected row count in {table}"
        );
    }
}

/// First call initializes reading 1; every distinct key advances exactly
/// one; same-key replays return the original reading without moving the
/// watermark (no double-jump); the watermark never regresses.
#[test]
fn now_initializes_once_and_advances_exactly_one_per_key() {
    let root = TestRoot::new("init-advance");
    let clock = AuthorityClock::open(root.base()).expect("open fresh clock");

    assert_eq!(
        clock.inspect().expect("fresh high-water"),
        Reading::from_u64(0),
        "a fresh store has issued no reading"
    );
    assert_eq!(ticked(&clock, 0x01), Reading::from_u64(1));
    assert_eq!(ticked(&clock, 0x02), Reading::from_u64(2));
    assert_eq!(ticked(&clock, 0x03), Reading::from_u64(3));

    // Same-key replays: the recorded original, watermark unmoved.
    assert_eq!(replayed(&clock, 0x02), Reading::from_u64(2));
    assert_eq!(replayed(&clock, 0x01), Reading::from_u64(1));
    assert_eq!(
        clock.inspect().expect("high-water after replays"),
        3_u64.into()
    );

    assert_counts(root.base(), [1, 3]);
}

/// The high-water survives restart; replays after reopen are byte-equal and
/// advance nothing; a fresh key continues exactly one above the durable
/// high-water (never below).
#[test]
fn reopen_preserves_high_water_and_replays_without_advancing() {
    let root = TestRoot::new("restart");
    let clock = AuthorityClock::open(root.base()).expect("open clock");
    assert_eq!(ticked(&clock, 0x01), Reading::from_u64(1));
    assert_eq!(ticked(&clock, 0x02), Reading::from_u64(2));
    drop(clock);

    let reopened = AuthorityClock::open(root.base()).expect("reopen clock");
    assert_eq!(
        reopened.inspect().expect("durable high-water"),
        Reading::from_u64(2),
        "the high-water must survive the restart"
    );
    assert_eq!(replayed(&reopened, 0x01), Reading::from_u64(1));
    assert_eq!(replayed(&reopened, 0x02), Reading::from_u64(2));
    assert_eq!(
        reopened.inspect().expect("replays advance nothing"),
        2_u64.into()
    );
    assert_eq!(ticked(&reopened, 0x03), Reading::from_u64(3));
    assert_counts(root.base(), [1, 3]);
}

/// An unknown stored schema version fails closed at open.
#[test]
fn unknown_schema_version_fails_closed() {
    let root = TestRoot::new("schema-version");
    {
        let connection = Connection::open(clock_database(root.base())).expect("create db");
        connection
            .pragma_update(None, "user_version", 7)
            .expect("stamp unknown version");
    }
    let Err(error) = AuthorityClock::open(root.base()) else {
        panic!("an unknown schema version must fail closed");
    };
    assert!(
        matches!(error, AuthorityClockError::SchemaVersionUnsupported(7)),
        "expected SchemaVersionUnsupported(7), got {error}"
    );
}

/// Raw regression is trigger-aborted even outside any injection, and a
/// reading at the top of the representable range fails closed with zero
/// durable state change.
#[test]
fn raw_regression_and_reading_exhaustion_fail_closed() {
    let root = TestRoot::new("exhaustion");
    let clock = AuthorityClock::open(root.base()).expect("open clock");
    assert_eq!(ticked(&clock, 0x01), Reading::from_u64(1));
    drop(clock);

    {
        let raw = Connection::open(clock_database(root.base())).expect("raw connection");
        // Decreasing updates abort (monotonic guard baseline).
        assert!(
            raw.execute("UPDATE watermark SET reading=0", []).is_err(),
            "the watermark can never move backwards"
        );
        // Climb to the top of the representable range (increases are
        // monotonic-legal raw updates).
        raw.execute("UPDATE watermark SET reading=9223372036854775807", [])
            .expect("climb to i64::MAX");
    }

    let reopened = AuthorityClock::open(root.base()).expect("reopen clock");
    let max_reading = Reading::from_u64(i64::MAX as u64);
    assert_eq!(reopened.inspect().expect("climbed high-water"), max_reading,);
    let error = reopened
        .now(request(0x41))
        .expect_err("a tick beyond i64::MAX must fail closed");
    assert!(
        matches!(error, AuthorityClockError::CorruptRecord(_)),
        "expected CorruptRecord at exhaustion, got {error}"
    );
    assert_eq!(
        reopened.inspect().expect("high-water after failed tick"),
        max_reading,
        "the failed tick left zero durable state"
    );
    assert_counts(root.base(), [1, 1]);
    assert_integrity(root.base());
}

fn assert_integrity(base: &std::path::Path) {
    let connection = Connection::open(clock_database(base)).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}
