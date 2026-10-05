//! W49: the three lease families share one Cell authority inside one
//! [`CellHost`] (ADR-0021).
//!
//! Under the W46 by-value API exactly one grantor could exist per process,
//! so capacity and device could not assemble beside quota. With the shared
//! face ([`nlos_cell::CellAuthority::into_shared`]) all three co-hold one
//! authority. The closed loop under test: three grants coexist under one
//! fence; the capacity grant→return chain refunds the pool whole at the
//! assembly face; the device reset-gated return frees the head; and one
//! epoch advance links all three families — the quota lease quarantines
//! (`LEASE-LOSS-001`, no refund) while capacity and device reject stale
//! fences typed and complete their family-semantic returns under the new
//! fence.
//!
//! One `#[test]` so this file claims the process-scoped Cell exactly once
//! (the `nlos-cell` claim slot is per process and never released).

use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_cell::{CellEpoch, CellFence, FailureDetectorConfig};
use nlos_lease::{
    CapacityLeaseGrantError, CapacityLeaseState, DeviceResetReceipt,
    ExclusiveDeviceLeaseGrantError, ExclusiveDeviceLeaseState, QuotaLeaseState,
};
use nlos_slice_k::{CellHost, CellHostConfig, SliceKError};
use nlos_types::{
    CapacityLeaseId, DeviceId, ExclusiveDeviceLeaseId, QuotaLeaseId, ReceiptId, SchedulerDomainId,
};

struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-slice-k-cell-{name}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create temp root");
        Self { root }
    }

    fn root(&self) -> &std::path::Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove slice-k temp root: {error}"),
        }
    }
}

/// Escalation thresholds in durable-clock ticks (same shape as the W48
/// assembly test; this file exercises no detector lane, the config is
/// assembly input only).
fn detector_config() -> FailureDetectorConfig {
    FailureDetectorConfig::new(
        NonZeroU64::new(3).expect("suspect threshold"),
        NonZeroU64::new(5).expect("dead threshold"),
    )
    .expect("threshold ordering")
}

/// The ids of the three co-granted leases (one per family).
struct FamilyLeaseIds {
    quota: QuotaLeaseId,
    capacity: CapacityLeaseId,
    device: ExclusiveDeviceLeaseId,
}

/// Coexistence straight after assembly: three sub-ledgers over one claim,
/// with the inspect face reporting both new members.
fn assert_three_ledgers_assembled(host: &CellHost) {
    let inspect = host.inspect_assembly().expect("assembly inspect");
    assert_eq!(inspect.quota_available, 1000);
    assert_eq!(inspect.capacity_pool_remaining, 700);
    assert!(inspect.device_head_free);
    assert_eq!(inspect.epoch, CellEpoch::INITIAL);
}

/// One grant per family under the same opening fence — the coexistence the
/// by-value API structurally forbade. Every grant binds the same fence
/// axes: one shared authority, one claim, one trust domain.
fn assert_cogranted_under_one_fence(host: &mut CellHost) -> (CellFence, FamilyLeaseIds) {
    let fence_at_open = host.fence();
    let ids = FamilyLeaseIds {
        quota: QuotaLeaseId::from_bytes([0x71; 16]),
        capacity: CapacityLeaseId::from_bytes([0x72; 16]),
        device: ExclusiveDeviceLeaseId::from_bytes([0x73; 16]),
    };
    let quota_grant = host
        .grant_quota_lease(ids.quota, 100, None)
        .expect("quota grant");
    let capacity_grant = host
        .grant_capacity_lease(ids.capacity, 300)
        .expect("capacity grant");
    let device_grant = host.grant_device_lease(ids.device).expect("device grant");
    assert_eq!(host.quota_available(), 900);
    assert_eq!(host.capacity_pool_remaining(), 400);
    assert!(!host.device_head_free());
    assert_eq!(quota_grant.epoch(), fence_at_open.epoch());
    assert_eq!(quota_grant.fencing_token(), fence_at_open.fencing_token());
    assert_eq!(capacity_grant.capacity_epoch(), fence_at_open.epoch());
    assert_eq!(
        capacity_grant.fencing_token(),
        fence_at_open.fencing_token()
    );
    assert_eq!(device_grant.exclusivity_epoch(), fence_at_open.epoch());
    assert_eq!(device_grant.fencing_token(), fence_at_open.fencing_token());
    (fence_at_open, ids)
}

/// The capacity grant→return chain at the assembly face: begin does not
/// refund, ack refunds the whole amount once.
fn assert_capacity_grant_return_chain(host: &mut CellHost) {
    let second = CapacityLeaseId::from_bytes([0x74; 16]);
    let reserved = host
        .grant_capacity_lease(second, 80)
        .expect("second capacity grant");
    assert_eq!(reserved.state(), CapacityLeaseState::GlobalReserved);
    assert_eq!(host.capacity_pool_remaining(), 320);
    let returning = host.begin_capacity_return(second).expect("begin return");
    assert_eq!(returning.state(), CapacityLeaseState::Returning);
    assert_eq!(
        host.capacity_pool_remaining(),
        320,
        "RETURNING does not refund"
    );
    let returned = host.ack_capacity_return(second).expect("ack return");
    assert_eq!(returned.state(), CapacityLeaseState::Returned);
    assert_eq!(
        host.capacity_pool_remaining(),
        400,
        "RETURNED refunds the whole amount"
    );
}

/// One epoch advance links the three families: the quota lease quarantines
/// (`LEASE-LOSS-001`, no refund); capacity and device reject new grants
/// under the pre-advance fence typed; and their old-epoch leases complete
/// their family-semantic returns under the new fence — capacity whole, the
/// device through its reset receipt.
fn assert_epoch_links_three_families(
    host: &mut CellHost,
    fence_at_open: CellFence,
    ids: &FamilyLeaseIds,
) {
    let advance = host.advance_cell_epoch().expect("epoch advance");
    let fence_after = advance.fence;
    let epoch_two = CellEpoch::INITIAL.checked_next().expect("epoch two");
    assert_eq!(fence_after.epoch(), epoch_two);

    // Quota: quarantine, no refund.
    let quarantined = host.query_quota_lease(ids.quota).expect("quota query");
    assert_eq!(quarantined.state(), QuotaLeaseState::Quarantined);
    assert_eq!(host.quota_available(), 900, "quarantine never refunds");

    // Capacity and device: new grants under the pre-advance fence are
    // stale-rejected typed — the shared authority advanced underneath.
    let stale_capacity = host.grant_capacity_lease_against(
        &fence_at_open,
        CapacityLeaseId::from_bytes([0x75; 16]),
        10,
    );
    assert!(matches!(
        stale_capacity,
        Err(SliceKError::CapacityLeaseGrant(
            CapacityLeaseGrantError::StaleEpoch { .. }
        ))
    ));
    let stale_device = host.grant_device_lease_against(
        &fence_at_open,
        ExclusiveDeviceLeaseId::from_bytes([0x76; 16]),
    );
    assert!(matches!(
        stale_device,
        Err(SliceKError::DeviceLeaseGrant(
            ExclusiveDeviceLeaseGrantError::StaleEpoch { .. }
        ))
    ));

    // Capacity has no quarantine face: the old-epoch lease returns whole
    // under the new fence.
    let old_capacity_returning = host
        .begin_capacity_return(ids.capacity)
        .expect("old-epoch capacity return");
    assert_eq!(
        old_capacity_returning.state(),
        CapacityLeaseState::Returning
    );
    let old_capacity_returned = host
        .ack_capacity_return(ids.capacity)
        .expect("old-epoch capacity ack");
    assert_eq!(old_capacity_returned.state(), CapacityLeaseState::Returned);
    assert_eq!(host.capacity_pool_remaining(), 700);

    // Device: the reset-gated return completes under the new fence — the
    // receipt binds the lease's own issue-time axes (head fence included).
    let device_grant = host.query_device_lease(ids.device).expect("device query");
    let resetting = host
        .declare_device_reset(ids.device)
        .expect("declare reset");
    assert_eq!(resetting.state(), ExclusiveDeviceLeaseState::Resetting);
    let receipt = DeviceResetReceipt::new(
        ReceiptId::from_bytes([0x87; 16]),
        device_grant.device_lease_id(),
        device_grant.holder_node(),
        device_grant.holder_node_boot_generation(),
        device_grant.exclusivity_epoch(),
        device_grant.fencing_token(),
        device_grant.reset_generation(),
    );
    let device_returned = host
        .submit_device_reset_receipt(receipt)
        .expect("reset receipt");
    assert_eq!(device_returned.state(), ExclusiveDeviceLeaseState::Returned);
    assert!(host.device_head_free());

    // The freed head serves the new epoch: a fresh device lease binds the
    // post-advance fence, and the inspect face reports both new members.
    let fresh = host
        .grant_device_lease(ExclusiveDeviceLeaseId::from_bytes([0x77; 16]))
        .expect("fresh device grant");
    assert_eq!(fresh.exclusivity_epoch(), epoch_two);
    assert_eq!(fresh.fencing_token(), fence_after.fencing_token());
    assert!(!host.device_head_free());

    let post = host.inspect_assembly().expect("post-advance inspect");
    assert_eq!(post.epoch, epoch_two);
    assert_eq!(post.capacity_pool_remaining, 700);
    assert!(!post.device_head_free);
    assert!(
        post.report_lines()
            .iter()
            .any(|line| line == "lease_capacity_pool=700"),
        "report lines must carry the capacity member: {:?}",
        post.report_lines()
    );
    assert!(
        post.report_lines()
            .iter()
            .any(|line| line == "lease_device_head=reserved"),
        "report lines must carry the device member: {:?}",
        post.report_lines()
    );
}

#[test]
fn three_lease_families_share_one_authority_and_link_on_epoch_advance() {
    let temp = TempDir::new("w49");
    let domain = SchedulerDomainId::from_bytes([0x4a; 16]);
    let device = DeviceId::from_bytes([0x7a; 16]);
    let mut host = CellHost::open(
        temp.root(),
        CellHostConfig::new(domain, 1000, 700, device, detector_config()),
    )
    .expect("assemble cell host with three lease families");

    assert_three_ledgers_assembled(&host);
    let (fence_at_open, ids) = assert_cogranted_under_one_fence(&mut host);
    assert_capacity_grant_return_chain(&mut host);
    assert_epoch_links_three_families(&mut host, fence_at_open, &ids);
}
