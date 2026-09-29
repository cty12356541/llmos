use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_resource::{
    CreateAccountRequest, CreateQuoteRequest, DemandDimension, RegisterDriverRequest,
    ReservationDecision, ReserveRequest, ResourceAuthority, ResourceAuthorityError, ResourceDemand,
};
use nlos_types::{CallId, IdempotencyKey, OperationId};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Root(PathBuf);
impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-resource-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn driver_request(seed: u8) -> RegisterDriverRequest {
    RegisterDriverRequest {
        profile_digest: [seed; 32],
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(1); 16]),
        created_at_ms: 1000,
    }
}
fn account_request(seed: u8, credit: u64) -> CreateAccountRequest {
    CreateAccountRequest {
        initial_credit: credit,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(2); 16]),
        created_at_ms: 1000,
    }
}
#[allow(clippy::large_types_passed_by_value)] // Keep fixture call sites compact and readable.
fn quote_request(
    seed: u8,
    d: nlos_resource::DriverRecord,
    upper: u64,
    capacity: ResourceDemand,
) -> CreateQuoteRequest {
    CreateQuoteRequest {
        driver_id: d.driver_id,
        driver_generation: d.generation,
        driver_fencing_token: d.fencing_token,
        operation_proposal_digest: [seed.wrapping_add(3); 32],
        pricing_version: [seed.wrapping_add(4); 32],
        upper_bound: upper,
        demand_capacity: capacity,
        valid_until_ms: 10_000,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(5); 16]),
        created_at_ms: 1000,
    }
}
#[allow(clippy::large_types_passed_by_value)] // Keep fixture call sites compact and readable.
fn reserve_request(
    seed: u8,
    a: nlos_resource::AccountRecord,
    q: nlos_resource::QuoteRecord,
    demand: ResourceDemand,
) -> ReserveRequest {
    ReserveRequest {
        account_id: a.account_id,
        quote_id: q.quote_id,
        call_id: CallId::from_bytes([seed.wrapping_add(6); 16]),
        operation_id: OperationId::from_bytes([seed.wrapping_add(7); 16]),
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(8); 16]),
        demand,
        reserved_at_ms: 2000,
    }
}

/// The pre-v6 `reservation_identity_immutable` trigger (the v1 thirteen
/// identity columns), used by fixtures to reproduce a database whose
/// reservations identity trigger predates the demand columns.
const PRE_DEMAND_IDENTITY_TRIGGER_SQL: &str =
    "DROP TRIGGER IF EXISTS reservation_identity_immutable;
     CREATE TRIGGER reservation_identity_immutable BEFORE UPDATE ON reservations
     WHEN NEW.reservation_id != OLD.reservation_id
       OR NEW.idempotency_key != OLD.idempotency_key
       OR NEW.account_id != OLD.account_id OR NEW.quote_id != OLD.quote_id
       OR NEW.call_id != OLD.call_id OR NEW.operation_id != OLD.operation_id
       OR NEW.driver_id != OLD.driver_id OR NEW.device_id != OLD.device_id
       OR NEW.driver_generation != OLD.driver_generation
       OR NEW.driver_fencing_token != OLD.driver_fencing_token
       OR NEW.upper_bound != OLD.upper_bound OR NEW.activation_token != OLD.activation_token
       OR NEW.created_at_ms != OLD.created_at_ms
     BEGIN SELECT RAISE(ABORT, 'reservation identity is immutable'); END;";

fn reservation_identity_update_fails(database: &Path, set_clause: &str) -> bool {
    let raw = Connection::open(database.join("resource-authority.db")).unwrap();
    raw.execute(&format!("UPDATE reservations SET {set_clause}"), [])
        .is_err()
}

#[test]
fn reserve_rejects_each_dimension_exceeding_quote_capacity_fail_closed() {
    let capacity = ResourceDemand {
        cpu_shares: 100,
        memory_mib: 1024,
        io_weight: 10,
    };
    for (n, dimension) in DemandDimension::ALL.iter().enumerate() {
        let seed = u8::try_from(n).unwrap() + 10;
        let root = Root::new("demand-over");
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let driver = authority
            .register_driver(driver_request(seed))
            .unwrap()
            .record();
        let account = authority
            .create_account(account_request(seed, 1000))
            .unwrap();
        let quote = authority
            .create_quote(quote_request(seed, driver, 100, capacity))
            .unwrap()
            .record();
        let mut demand = ResourceDemand::default();
        match dimension {
            DemandDimension::CpuShares => demand.cpu_shares = capacity.cpu_shares + 1,
            DemandDimension::MemoryMib => demand.memory_mib = capacity.memory_mib + 1,
            DemandDimension::IoWeight => demand.io_weight = capacity.io_weight + 1,
        }
        let attempt = authority.reserve(reserve_request(seed, account, quote, demand));
        let (violated, reported, bound) = match attempt {
            Err(ResourceAuthorityError::DemandExceedsCapacity {
                dimension,
                demand: reported,
                capacity: bound,
            }) => (dimension, reported, bound),
            other => panic!("expected DemandExceedsCapacity, got {other:?}"),
        };
        assert_eq!(violated, *dimension, "first violated dimension is reported");
        assert_eq!(reported, dimension.demand_of(demand));
        assert_eq!(bound, dimension.capacity_of(capacity));
        assert_eq!(
            authority
                .inspect_account(account.account_id)
                .unwrap()
                .available_credit,
            1000,
            "a rejected demand must not move credit"
        );
    }
}

#[test]
fn multi_dimension_demand_within_capacity_binds_replays_and_survives_restart() {
    let root = Root::new("demand-bind");
    let capacity = ResourceDemand {
        cpu_shares: 100,
        memory_mib: 1024,
        io_weight: 10,
    };
    let first = {
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let driver = authority
            .register_driver(driver_request(20))
            .unwrap()
            .record();
        let account = authority.create_account(account_request(20, 1000)).unwrap();
        let quote = authority
            .create_quote(quote_request(20, driver, 100, capacity))
            .unwrap()
            .record();
        let demand = ResourceDemand {
            cpu_shares: 64,
            memory_mib: 512,
            io_weight: 5,
        };
        let request = reserve_request(20, account, quote, demand);
        let first = authority.reserve(request).unwrap();
        assert!(matches!(first, ReservationDecision::Reserved(_)));
        let replay = authority.reserve(request).unwrap();
        assert!(matches!(replay, ReservationDecision::Replayed(_)));
        assert_eq!(first.record(), replay.record());
        assert_eq!(
            authority
                .inspect_account(account.account_id)
                .unwrap()
                .available_credit,
            900,
            "the single-credit hold is unchanged by demand admission"
        );
        first.record()
    };
    let reopened = ResourceAuthority::open(root.path()).unwrap();
    let stored = reopened.inspect_reservation(first.reservation_id).unwrap();
    assert_eq!(stored, first, "demand round-trips through the durable row");
    assert_eq!(
        stored.demand,
        ResourceDemand {
            cpu_shares: 64,
            memory_mib: 512,
            io_weight: 5,
        }
    );
    assert_eq!(stored.state, nlos_resource::ReservationState::Reserved);

    // A demand exactly equal to the per-dimension capacity is admissible.
    let boundary_seed = 21;
    let driver = reopened
        .register_driver(driver_request(boundary_seed))
        .unwrap()
        .record();
    let account = reopened
        .create_account(account_request(boundary_seed, 1000))
        .unwrap();
    let quote = reopened
        .create_quote(quote_request(boundary_seed, driver, 100, capacity))
        .unwrap()
        .record();
    let boundary = reopened
        .reserve(reserve_request(boundary_seed, account, quote, capacity))
        .unwrap();
    assert!(matches!(boundary, ReservationDecision::Reserved(_)));
    assert_eq!(boundary.record().demand, capacity);
}

#[test]
fn reserve_replay_rejects_demand_conflict_fail_closed() {
    let root = Root::new("demand-replay");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let driver = authority
        .register_driver(driver_request(30))
        .unwrap()
        .record();
    let account = authority.create_account(account_request(30, 1000)).unwrap();
    let capacity = ResourceDemand {
        cpu_shares: 10,
        memory_mib: 128,
        io_weight: 5,
    };
    let quote = authority
        .create_quote(quote_request(30, driver, 100, capacity))
        .unwrap()
        .record();
    let request = reserve_request(30, account, quote, ResourceDemand::default());
    let reserved = match authority.reserve(request) {
        Ok(ReservationDecision::Reserved(r)) => r,
        other => panic!("expected Reserved, got {other:?}"),
    };
    let stored = authority
        .inspect_reservation(reserved.reservation_id)
        .unwrap();
    let mut conflict = request;
    conflict.demand = capacity;
    assert!(matches!(
        authority.reserve(conflict),
        Err(ResourceAuthorityError::IdempotencyConflict)
    ));
    assert_eq!(
        authority
            .inspect_reservation(stored.reservation_id)
            .unwrap(),
        stored,
        "the conflicting replay leaves the durable reservation untouched"
    );
}

#[test]
fn legacy_v5_rows_migrate_with_zero_default_demand_and_legacy_path_still_binds() {
    let root = Root::new("demand-migration");
    let (reservation_id, account_id) = {
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let driver = authority
            .register_driver(driver_request(40))
            .unwrap()
            .record();
        let account = authority.create_account(account_request(40, 1000)).unwrap();
        let capacity = ResourceDemand {
            cpu_shares: 10,
            memory_mib: 128,
            io_weight: 5,
        };
        let quote = authority
            .create_quote(quote_request(40, driver, 100, capacity))
            .unwrap()
            .record();
        let demand = ResourceDemand {
            cpu_shares: 4,
            memory_mib: 64,
            io_weight: 2,
        };
        let reserved = authority
            .reserve(reserve_request(40, account, quote, demand))
            .unwrap();
        assert!(matches!(reserved, ReservationDecision::Reserved(_)));
        (reserved.record().reservation_id, account.account_id)
    };
    // Strip the v6 demand columns to reproduce a legacy v5-shaped database
    // (its identity trigger predates the demand columns).
    {
        let raw = Connection::open(root.path().join("resource-authority.db")).unwrap();
        raw.execute_batch(&format!(
            "{PRE_DEMAND_IDENTITY_TRIGGER_SQL}
             ALTER TABLE quotes DROP COLUMN capacity_cpu_shares;
             ALTER TABLE quotes DROP COLUMN capacity_memory_mib;
             ALTER TABLE quotes DROP COLUMN capacity_io_weight;
             ALTER TABLE reservations DROP COLUMN demand_cpu_shares;
             ALTER TABLE reservations DROP COLUMN demand_memory_mib;
             ALTER TABLE reservations DROP COLUMN demand_io_weight;
             PRAGMA user_version = 5;"
        ))
        .unwrap();
    }
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let migrated = authority.inspect_reservation(reservation_id).unwrap();
    assert_eq!(
        migrated.demand,
        ResourceDemand::default(),
        "legacy v5 rows read back as the zero (single-credit) demand profile"
    );
    assert_eq!(migrated.upper_bound, 100);
    // The v6 migration rebuilds the identity trigger over the surviving
    // rows: the migrated row's zero demand is now DDL-immutable.
    assert!(
        reservation_identity_update_fails(root.path(), "demand_cpu_shares = 1"),
        "the rebuilt trigger must cover the demand columns for legacy rows"
    );

    // The legacy zero-demand path still binds: zero demand never exceeds
    // zero capacity, so pre-demand callers keep their exact behavior.
    let driver = authority
        .register_driver(driver_request(41))
        .unwrap()
        .record();
    let account = authority.create_account(account_request(41, 500)).unwrap();
    let quote = authority
        .create_quote(quote_request(41, driver, 100, ResourceDemand::default()))
        .unwrap()
        .record();
    let legacy_reserve = authority
        .reserve(reserve_request(
            41,
            account,
            quote,
            ResourceDemand::default(),
        ))
        .unwrap();
    assert!(matches!(legacy_reserve, ReservationDecision::Reserved(_)));
    assert_eq!(
        authority
            .inspect_account(account_id)
            .unwrap()
            .available_credit,
        900,
        "the migrated reservation keeps its original hold"
    );
}

#[test]
fn partial_demand_schema_fails_closed() {
    let root = Root::new("demand-migration-partial");
    {
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let driver = authority
            .register_driver(driver_request(42))
            .unwrap()
            .record();
        let account = authority.create_account(account_request(42, 1000)).unwrap();
        let quote = authority
            .create_quote(quote_request(42, driver, 100, ResourceDemand::default()))
            .unwrap()
            .record();
        authority
            .reserve(reserve_request(
                42,
                account,
                quote,
                ResourceDemand::default(),
            ))
            .unwrap();
    }
    {
        let raw = Connection::open(root.path().join("resource-authority.db")).unwrap();
        raw.execute_batch(&format!(
            "{PRE_DEMAND_IDENTITY_TRIGGER_SQL}
             ALTER TABLE reservations DROP COLUMN demand_io_weight;
             PRAGMA user_version = 5;"
        ))
        .unwrap();
    }
    assert!(matches!(
        ResourceAuthority::open(root.path()),
        Err(ResourceAuthorityError::CorruptRecord(
            "partial resource demand schema"
        ))
    ));
}

/// The declared demand of a fresh v6 database is part of the
/// DDL-enforced immutable Reservation identity: out-of-band SQL cannot
/// rewrite it, while the mutable bookkeeping columns (the usage
/// high-water) keep updating.
#[test]
fn declared_demand_is_ddl_immutable_on_fresh_v6_databases() {
    let root = Root::new("demand-immutable");
    let reservation_id = {
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let driver = authority
            .register_driver(driver_request(43))
            .unwrap()
            .record();
        let account = authority.create_account(account_request(43, 1000)).unwrap();
        let capacity = ResourceDemand {
            cpu_shares: 100,
            memory_mib: 1024,
            io_weight: 10,
        };
        let quote = authority
            .create_quote(quote_request(43, driver, 100, capacity))
            .unwrap()
            .record();
        let demand = ResourceDemand {
            cpu_shares: 64,
            memory_mib: 512,
            io_weight: 5,
        };
        authority
            .reserve(reserve_request(43, account, quote, demand))
            .unwrap()
            .record()
            .reservation_id
    };

    for set_clause in [
        "demand_cpu_shares = demand_cpu_shares + 1",
        "demand_memory_mib = 0",
        "demand_io_weight = 7",
    ] {
        assert!(
            reservation_identity_update_fails(root.path(), set_clause),
            "out-of-band UPDATE ({set_clause}) must be DDL-rejected"
        );
    }
    // The quote capacity columns are covered by the whole-table quote
    // immutability trigger.
    let raw = Connection::open(root.path().join("resource-authority.db")).unwrap();
    assert!(
        raw.execute("UPDATE quotes SET capacity_cpu_shares = 1", [])
            .is_err()
    );
    // Mutable bookkeeping columns are not over-blocked by the identity
    // trigger (the consume path updates exactly these columns).
    assert_eq!(
        raw.execute(
            "UPDATE reservations SET usage_high_water_seq = 3, usage_high_water = 70
             WHERE reservation_id = ?1",
            rusqlite::params![reservation_id.as_bytes().as_slice()],
        )
        .unwrap(),
        1
    );
}

/// Existing-data upgrade: a v6 database migrated before the demand columns
/// joined the identity trigger keeps its rows, and the first reopen after
/// the fix rebuilds the trigger in place over the surviving rows.
#[test]
fn pre_demand_v6_identity_trigger_rebuilds_over_surviving_rows() {
    let root = Root::new("demand-trigger-rebuild");
    let seeded = {
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let driver = authority
            .register_driver(driver_request(44))
            .unwrap()
            .record();
        let account = authority.create_account(account_request(44, 1000)).unwrap();
        let capacity = ResourceDemand {
            cpu_shares: 100,
            memory_mib: 1024,
            io_weight: 10,
        };
        let quote = authority
            .create_quote(quote_request(44, driver, 100, capacity))
            .unwrap()
            .record();
        let demand = ResourceDemand {
            cpu_shares: 64,
            memory_mib: 512,
            io_weight: 5,
        };
        authority
            .reserve(reserve_request(44, account, quote, demand))
            .unwrap()
            .record()
    };

    // Reproduce the pre-fix database: columns present, identity trigger
    // predating them (the defect state — a demand rewrite is accepted). The
    // fixture must also re-stamp v6: the database the fix targeted carries
    // user_version 6, and the dispatch only re-enters migrate_v6 from there.
    {
        let raw = Connection::open(root.path().join("resource-authority.db")).unwrap();
        raw.execute_batch(&format!(
            "{PRE_DEMAND_IDENTITY_TRIGGER_SQL}
             PRAGMA user_version = 6;"
        ))
        .unwrap();
        assert!(
            !reservation_identity_update_fails(root.path(), "demand_cpu_shares = 65"),
            "fixture setup: the pre-demand trigger must not cover the demand columns"
        );
        // Undo the probe rewrite so the upgrade asserts over pristine data.
        assert_eq!(
            raw.execute(
                "UPDATE reservations SET demand_cpu_shares = ?1 WHERE reservation_id = ?2",
                rusqlite::params![
                    i64::try_from(seeded.demand.cpu_shares).unwrap(),
                    seeded.reservation_id.as_bytes().as_slice()
                ],
            )
            .unwrap(),
            1
        );
    }

    // First reopen: the trigger-only upgrade runs over the surviving rows.
    let authority = ResourceAuthority::open(root.path()).unwrap();
    assert_eq!(
        authority
            .inspect_reservation(seeded.reservation_id)
            .unwrap(),
        seeded,
        "the trigger rebuild leaves the surviving row byte-identical"
    );
    assert!(
        reservation_identity_update_fails(root.path(), "demand_cpu_shares = 65"),
        "after the upgrade the declared demand is DDL-immutable"
    );
    assert!(
        reservation_identity_update_fails(root.path(), "demand_io_weight = 9"),
        "every demand column is covered"
    );

    // Second reopen takes the complete-state fast path (idempotent re-entry)
    // and the protection stays enforced.
    drop(authority);
    let reopened = ResourceAuthority::open(root.path()).unwrap();
    assert_eq!(
        reopened
            .inspect_reservation(seeded.reservation_id)
            .unwrap()
            .demand,
        seeded.demand
    );
    assert!(reservation_identity_update_fails(
        root.path(),
        "demand_memory_mib = 1"
    ));
}
