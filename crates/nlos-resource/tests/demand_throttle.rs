use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_resource::{
    CreateAccountRequest, CreateQuoteRequest, DemandDimension, RegisterDriverRequest,
    ReserveRequest, ResourceAuthority, ResourceDemand, throttle_demand,
};
use nlos_types::{CallId, IdempotencyKey, OperationId};

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
fn throttled_to_percent_scales_every_dimension_saturating() {
    let demand = ResourceDemand {
        cpu_shares: 64,
        memory_mib: 512,
        io_weight: 5,
    };
    assert_eq!(
        demand.throttled_to_percent(100),
        demand,
        "100 percent is the identity"
    );
    assert_eq!(
        demand.throttled_to_percent(50),
        ResourceDemand {
            cpu_shares: 32,
            memory_mib: 256,
            io_weight: 2,
        },
        "integer division truncates per dimension"
    );
    assert_eq!(
        demand.throttled_to_percent(0),
        ResourceDemand::default(),
        "zero percent collapses the demand"
    );
    let huge = ResourceDemand {
        cpu_shares: u64::MAX,
        memory_mib: u64::MAX,
        io_weight: u64::MAX,
    };
    assert_eq!(
        huge.throttled_to_percent(100),
        huge,
        "the saturating scale never wraps at the 64-bit bound"
    );
}

#[test]
fn throttle_demand_reports_before_after_and_admission() {
    let capacity = ResourceDemand {
        cpu_shares: 100,
        memory_mib: 1024,
        io_weight: 10,
    };
    let admitted = ResourceDemand {
        cpu_shares: 80,
        memory_mib: 900,
        io_weight: 9,
    };
    let decision = throttle_demand(admitted, capacity, 25);
    assert_eq!(decision.throttle_percent, 25);
    assert_eq!(decision.demand_before, admitted);
    assert_eq!(
        decision.demand_after,
        ResourceDemand {
            cpu_shares: 20,
            memory_mib: 225,
            io_weight: 2,
        }
    );
    assert_eq!(decision.capacity, capacity);
    assert_eq!(decision.first_exceedance, None);

    // A demand that already exceeded capacity keeps its first violated
    // dimension report after the (narrower) throttle.
    let over = ResourceDemand {
        cpu_shares: 101,
        memory_mib: 1024,
        io_weight: 10,
    };
    let still_over = throttle_demand(over, capacity, 100);
    assert_eq!(
        still_over.first_exceedance,
        Some((DemandDimension::CpuShares, 101, 100)),
        "the fixed-order first-violation report matches exceedance_of"
    );
}

#[test]
fn inspect_quote_returns_the_declared_demand_capacity() {
    let root = Root::new("throttle-inspect-quote");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let driver = authority
        .register_driver(driver_request(50))
        .unwrap()
        .record();
    let account = authority.create_account(account_request(50, 1000)).unwrap();
    let capacity = ResourceDemand {
        cpu_shares: 100,
        memory_mib: 1024,
        io_weight: 10,
    };
    let quote = authority
        .create_quote(quote_request(50, driver, 100, capacity))
        .unwrap()
        .record();
    let reserved = authority
        .reserve(reserve_request(
            50,
            account,
            quote,
            ResourceDemand {
                cpu_shares: 64,
                memory_mib: 512,
                io_weight: 5,
            },
        ))
        .unwrap()
        .record();

    let stored_quote = authority.inspect_quote(quote.quote_id).unwrap();
    assert_eq!(stored_quote, quote);
    assert_eq!(stored_quote.demand_capacity, capacity);

    // The reservation-to-quote readback pair is exactly what the W29-D
    // throttle executor drives: current demand from the reservation row,
    // admission capacity from the quote row.
    let stored_reservation = authority
        .inspect_reservation(reserved.reservation_id)
        .unwrap();
    assert_eq!(stored_reservation.demand, reserved.demand);
    let decision = throttle_demand(stored_reservation.demand, stored_quote.demand_capacity, 50);
    assert_eq!(decision.demand_after.cpu_shares, 32);
    assert_eq!(decision.first_exceedance, None);

    assert!(matches!(
        authority.inspect_quote(nlos_types::QuoteId::from_bytes([0xFF; 16])),
        Err(nlos_resource::ResourceAuthorityError::QuoteNotFound)
    ));
}
