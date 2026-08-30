//! B-CLOCK-001 (wall domain): happy-path and fail-closed gate tests for
//! `AuthorityClock::wall_now` — the durable wall high-water (schema v2).
//!
//! Covers: bootstrap from the system clock, `max(durable, system)` advance,
//! non-density (same-millisecond keys share a reading), same-key durable
//! replay without consulting the source, restart persistence, simulated
//! system-clock rollback absorption (injected [`nlos_clock::WallSource`]),
//! wall-source refusal fail-closed with replay still served from durable
//! state, the v1→v2 additive upgrade path, wall-domain DDL guards, and
//! strict tick/wall domain isolation (`now` semantics untouched).

use std::sync::atomic::{AtomicU64, Ordering};

use nlos_clock::{
    AuthorityClock, AuthorityClockError, NowDecision, NowRequest, SystemWallSource,
    WallNowDecision, WallReading, WallSource,
};
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

fn wall_advanced(clock: &AuthorityClock, seed: u8) -> WallReading {
    match clock
        .wall_now(request(seed))
        .expect("wall_now must advance")
    {
        WallNowDecision::Advanced(reading) => reading,
        WallNowDecision::Replayed(reading) => {
            panic!("fresh key cannot replay, got {reading}")
        }
    }
}

fn wall_replayed(clock: &AuthorityClock, seed: u8) -> WallReading {
    match clock.wall_now(request(seed)).expect("wall_now must replay") {
        WallNowDecision::Replayed(reading) => reading,
        WallNowDecision::Advanced(reading) => panic!("expected Replayed, got Advanced {reading}"),
    }
}

/// Wall clock source with a test-controlled reading; `set` moves it
/// arbitrarily — including backwards past the durable watermark — which is
/// the minimal deterministic model of a system-clock rollback.  Cloning
/// shares the reading, so a test can drive the source while the authority
/// owns a clone of it.
#[derive(Clone)]
struct ManualWallSource(std::sync::Arc<AtomicU64>);

impl ManualWallSource {
    fn at(ms: u64) -> Self {
        Self(std::sync::Arc::new(AtomicU64::new(ms)))
    }

    fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::Relaxed);
    }
}

impl WallSource for ManualWallSource {
    fn now_ms(&self) -> Result<u64, AuthorityClockError> {
        Ok(self.0.load(Ordering::Relaxed))
    }
}

/// A wall source that can never provide a reading.
struct FailingWallSource;

impl WallSource for FailingWallSource {
    fn now_ms(&self) -> Result<u64, AuthorityClockError> {
        Err(AuthorityClockError::WallClockUnavailable)
    }
}

struct TestRoot(std::path::PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("nlos-clock-wall-{label}-{}", std::process::id()));
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

fn assert_wall_counts(base: &std::path::Path, expected: [i64; 2]) {
    for (table, want) in ["wall_watermark", "wall_receipts"].iter().zip(expected) {
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

/// The first wall call bootstraps the durable watermark from the source;
/// later calls issue `max(durable, source)`; a rolled-back source is
/// absorbed at the durable watermark (never regresses); keys within one
/// millisecond share a reading (non-dense); replays are byte-equal and move
/// nothing; the tick domain stays byte-for-byte independent.
#[test]
fn wall_now_bootstraps_absorbs_rollback_and_stays_non_dense() {
    let root = TestRoot::new("bootstrap-rollback");
    let source = ManualWallSource::at(1_000);
    let clock = AuthorityClock::open_with_wall_source(root.base(), source.clone())
        .expect("open fresh clock");

    assert_eq!(
        clock.inspect_wall().expect("fresh wall high-water"),
        WallReading::from_u64(0),
        "a fresh store has issued no wall reading"
    );

    // Bootstrap: the first reading is the source value itself.
    assert_eq!(wall_advanced(&clock, 0x01), WallReading::from_u64(1_000));
    // Advance: source moved past the watermark.
    source.set(2_500);
    assert_eq!(wall_advanced(&clock, 0x02), WallReading::from_u64(2_500));
    // Non-dense: a fresh key within the same millisecond shares the reading.
    assert_eq!(wall_advanced(&clock, 0x03), WallReading::from_u64(2_500));

    // Rollback: the source steps below the durable watermark; fresh keys
    // still read the durable watermark, never the regressed clock.
    source.set(2_400);
    assert_eq!(
        wall_advanced(&clock, 0x04),
        WallReading::from_u64(2_500),
        "a system-clock rollback must not pull the wall reading down"
    );

    // Same-key replays: byte-equal originals, watermark unmoved.
    assert_eq!(wall_replayed(&clock, 0x01), WallReading::from_u64(1_000));
    assert_eq!(wall_replayed(&clock, 0x02), WallReading::from_u64(2_500));
    assert_eq!(wall_replayed(&clock, 0x02), WallReading::from_u64(2_500));
    assert_eq!(
        clock.inspect_wall().expect("wall high-water after replays"),
        WallReading::from_u64(2_500)
    );

    // Domain isolation: the logical tick domain is untouched by wall state.
    assert_eq!(
        clock.inspect().expect("tick high-water"),
        nlos_clock::Reading::from_u64(0),
        "wall readings must not move the tick watermark"
    );
    let ticked = match clock.now(request(0xA1)).expect("now must tick") {
        NowDecision::Tick(reading) => reading,
        NowDecision::Replayed(reading) => panic!("fresh tick key cannot replay, got {reading}"),
    };
    assert_eq!(ticked, nlos_clock::Reading::from_u64(1), "ticks stay dense");
    assert_eq!(
        wall_advanced(&clock, 0x05),
        WallReading::from_u64(2_500),
        "ticks must not move the wall watermark"
    );
    assert_wall_counts(root.base(), [1, 5]);
}

/// Wall state survives restart; replays after reopen are byte-equal without
/// consulting the source; a rolled-back source after restart cannot pull the
/// watermark down; a source past the watermark advances it.
#[test]
fn wall_now_survives_restart_and_never_regresses() {
    let root = TestRoot::new("restart");
    let source = ManualWallSource::at(5_000);
    {
        let clock =
            AuthorityClock::open_with_wall_source(root.base(), source.clone()).expect("open clock");
        assert_eq!(wall_advanced(&clock, 0x01), WallReading::from_u64(5_000));
        source.set(7_250);
        assert_eq!(wall_advanced(&clock, 0x02), WallReading::from_u64(7_250));
    }

    // Reopen with the source rolled far back (simulated clock rollback
    // across a restart).
    let reopened = AuthorityClock::open_with_wall_source(root.base(), ManualWallSource::at(1))
        .expect("reopen");
    assert_eq!(
        reopened.inspect_wall().expect("durable wall high-water"),
        WallReading::from_u64(7_250),
        "the wall watermark must survive the restart"
    );
    assert_eq!(wall_replayed(&reopened, 0x01), WallReading::from_u64(5_000));
    assert_eq!(wall_replayed(&reopened, 0x02), WallReading::from_u64(7_250));
    assert_eq!(
        wall_advanced(&reopened, 0x03),
        WallReading::from_u64(7_250),
        "fresh keys read at least the durable watermark after rollback"
    );
    assert_eq!(
        reopened.inspect_wall().expect("watermark unmoved"),
        WallReading::from_u64(7_250)
    );
    assert_wall_counts(root.base(), [1, 3]);

    // A source that has moved past the watermark advances it.
    let advanced = AuthorityClock::open_with_wall_source(root.base(), ManualWallSource::at(9_000))
        .expect("reopen");
    assert_eq!(wall_advanced(&advanced, 0x04), WallReading::from_u64(9_000));
    assert_wall_counts(root.base(), [1, 4]);
}

/// The default `open` wires the system clock; a wall reading lands within
/// the before/after observation window and is never below it.
#[test]
fn wall_now_defaults_to_system_source_within_observation_window() {
    let root = TestRoot::new("system-source");
    let clock = AuthorityClock::open(root.base()).expect("open clock");
    let before = SystemWallSource.now_ms().expect("system source before");
    let reading = wall_advanced(&clock, 0x01);
    let after = SystemWallSource.now_ms().expect("system source after");
    assert!(
        (before..=after).contains(&reading.as_u64()),
        "reading {reading} outside observed window [{before}, {after}]"
    );
    assert_eq!(
        clock.inspect_wall().expect("durable wall high-water"),
        reading,
        "the bootstrap reading is the durable watermark"
    );
    assert_wall_counts(root.base(), [1, 1]);
}

/// A refusing wall source fails closed with zero durable state; an
/// already-issued key still replays from durable state without the source;
/// a working source on the same store then issues normally.
#[test]
fn wall_source_failure_fails_closed_and_replay_survives() {
    let root = TestRoot::new("source-failure");
    let source = ManualWallSource::at(3_000);
    {
        let clock = AuthorityClock::open_with_wall_source(root.base(), source).expect("open clock");
        assert_eq!(wall_advanced(&clock, 0x01), WallReading::from_u64(3_000));
    }
    let broken = AuthorityClock::open_with_wall_source(root.base(), FailingWallSource)
        .expect("reopen clock");

    let error = broken
        .wall_now(request(0x02))
        .expect_err("a refusing wall source must fail closed");
    assert!(
        matches!(error, AuthorityClockError::WallClockUnavailable),
        "expected WallClockUnavailable, got {error}"
    );
    assert_eq!(
        broken.inspect_wall().expect("watermark after refusal"),
        WallReading::from_u64(3_000),
        "the refusal left zero durable state"
    );
    assert_wall_counts(root.base(), [1, 1]);

    // Replay needs no system clock: the durable receipt answers alone.
    assert_eq!(
        wall_replayed(&broken, 0x01),
        WallReading::from_u64(3_000),
        "replays are served from durable state, never from the source"
    );
    assert_wall_counts(root.base(), [1, 1]);

    // A healthy source on the same store issues normally above the watermark.
    let healthy = AuthorityClock::open_with_wall_source(root.base(), ManualWallSource::at(4_500))
        .expect("reopen clock");
    assert_eq!(wall_advanced(&healthy, 0x02), WallReading::from_u64(4_500));
    assert_wall_counts(root.base(), [1, 2]);
}

/// A v1 store (tick domain only) upgrades additively on reopen: the wall
/// domain is created seeded at 0, the tick domain's committed state is
/// untouched, and both domains then operate normally.
#[test]
fn v1_store_upgrades_additively_and_keeps_tick_state() {
    let root = TestRoot::new("v1-upgrade");
    {
        let clock = AuthorityClock::open(root.base()).expect("open v2 clock");
        let bootstrapped = wall_advanced(&clock, 0x01);
        assert!(
            bootstrapped.as_u64() > 0,
            "bootstrap reading is the system clock"
        );
    }
    {
        // Strip the wall domain and stamp the store back to v1: the exact
        // durable shape a pre-v2 binary leaves behind.
        let raw = Connection::open(clock_database(root.base())).expect("raw connection");
        raw.execute("DROP TABLE wall_receipts", [])
            .expect("drop wall receipts");
        raw.execute("DROP TABLE wall_watermark", [])
            .expect("drop wall watermark");
        raw.pragma_update(None, "user_version", 1)
            .expect("stamp v1");
    }

    let reopened = AuthorityClock::open(root.base()).expect("reopen upgrades to v2");
    assert_eq!(
        reopened.inspect_wall().expect("wall domain re-seeded"),
        WallReading::from_u64(0)
    );
    assert_eq!(
        reopened.inspect().expect("tick state untouched by upgrade"),
        nlos_clock::Reading::from_u64(0)
    );
    let ticked = match reopened.now(request(0xA1)).expect("now must tick") {
        NowDecision::Tick(reading) => reading,
        NowDecision::Replayed(reading) => panic!("fresh tick key cannot replay, got {reading}"),
    };
    assert_eq!(ticked, nlos_clock::Reading::from_u64(1));
    let advanced = match reopened.wall_now(request(0x02)).expect("wall must advance") {
        WallNowDecision::Advanced(reading) => reading,
        WallNowDecision::Replayed(reading) => panic!("fresh wall key cannot replay, got {reading}"),
    };
    assert!(
        advanced.as_u64() > 0,
        "bootstrap reading is the system clock"
    );
    assert_wall_counts(root.base(), [1, 1]);
}

/// The wall-domain DDL guards abort raw tampering exactly like the tick
/// domain's: no decrease, no second row, no delete, frozen singleton,
/// immutable and undeletable receipts bounded by the watermark.
#[test]
fn wall_domain_ddl_guards_fail_closed() {
    let root = TestRoot::new("wall-guards");
    let source = ManualWallSource::at(10_000);
    let clock = AuthorityClock::open_with_wall_source(root.base(), source).expect("open clock");
    assert_eq!(wall_advanced(&clock, 0x01), WallReading::from_u64(10_000));
    drop(clock);

    let raw = Connection::open(clock_database(root.base())).expect("raw connection");
    assert!(
        raw.execute("UPDATE wall_watermark SET reading_ms=9_999", [])
            .is_err(),
        "the wall watermark can never move backwards"
    );
    assert!(
        raw.execute("UPDATE wall_watermark SET singleton=2", [])
            .is_err(),
        "the wall watermark singleton is trigger-frozen"
    );
    assert!(
        raw.execute(
            "INSERT INTO wall_watermark (singleton, reading_ms) VALUES (1, 99)",
            []
        )
        .is_err(),
        "no second wall watermark row can be inserted"
    );
    assert!(raw.execute("DELETE FROM wall_watermark", []).is_err());
    assert!(
        raw.execute("UPDATE wall_receipts SET reading_ms=99", [])
            .is_err(),
        "a wall receipt can never be rewritten"
    );
    assert!(raw.execute("DELETE FROM wall_receipts", []).is_err());
    assert!(
        raw.execute(
            "INSERT INTO wall_receipts (idempotency_key, reading_ms)
             VALUES (x'BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB', 10_001)",
            []
        )
        .is_err(),
        "wall receipts are watermark-bounded"
    );
    assert_wall_counts(root.base(), [1, 1]);
}
