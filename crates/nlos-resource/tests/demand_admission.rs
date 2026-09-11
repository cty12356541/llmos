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
    // Strip the v6 demand columns to reproduce a legacy v5-shaped database.
    {
        let raw = Connection::open(root.path().join("resource-authority.db")).unwrap();
        raw.execute_batch(
            "ALTER TABLE quotes DROP COLUMN capacity_cpu_shares;
             ALTER TABLE quotes DROP COLUMN capacity_memory_mib;
             ALTER TABLE quotes DROP COLUMN capacity_io_weight;
             ALTER TABLE reservations DROP COLUMN demand_cpu_shares;
             ALTER TABLE reservations DROP COLUMN demand_memory_mib;
             ALTER TABLE reservations DROP COLUMN demand_io_weight;
             PRAGMA user_version = 5;",
        )
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
        raw.execute_batch(
            "ALTER TABLE reservations DROP COLUMN demand_io_weight;
             PRAGMA user_version = 5;",
        )
        .unwrap();
    }
    assert!(matches!(
        ResourceAuthority::open(root.path()),
        Err(ResourceAuthorityError::CorruptRecord(
            "partial resource demand schema"
        ))
    ));
}
