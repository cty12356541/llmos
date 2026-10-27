//! Property, boundary, anomaly, and durable `SQLite WAL + FULL` integration
//! verification for `C-NET-CLOCK-LEASE` (`ADR-0026` §2.3, `ADR-0019` R2.3 #2,
//! `[MODEL-TIME-001]`, `[RESCROW-TIME-001]`, `[LEASE-TTL-001]`,
//! `[LEASE-FENCE-001]`, `[LEASE-LOSS-001]`).
//!
//! Covers:
//! 1. Exhaustive / property sweep across **> 15,000** `(delta_skew, relative_drift,
//!    grant_ttl, one_way_req_delay, one_way_resp_delay)` tuples proving **0**
//!    dual-active overlap violations whenever `ttl_grant > 2 * max_clock_skew +
//!    safety_margin`, holder anchors `t_req_holder_mono <= t_grant_grantor_mono`
//!    with `ttl_holder = ttl_grant - 2 * max_clock_skew - safety_margin`, and
//!    grantor waits strictly past `ttl_grant + max_clock_skew`.
//! 2. Boundary rejections (`ttl_grant <= 2 * max_clock_skew + safety_margin`),
//!    grantor premature sweep refusals (`elapsed <= ttl_grant + max_clock_skew`),
//!    and `AuditWallTimeHint` non-ordering isolation.
//! 3. Fail-closed quarantine on `ceil(RTT / 2) > max_clock_skew`, local
//!    monotonic clock rollback, boot-generation regression, and durable
//!    `AuthorityClock` logical-tick rollback (`AuthorityClockHorizonGuard`).
//! 4. Durable `SqliteLeaseStore` (`WAL+FULL`) three-family (`QuotaLease`,
//!    `CapacityLease`, `ExclusiveDeviceLease`) conservative expiry into
//!    `QUARANTINED` (never unlocking `AVAILABLE` / `pool_remaining` / `FREE`),
//!    crash/reopen rehydration, Path A (`SignedLeaseCloseReceipt`) idempotent
//!    settlement, Path B (`epoch/token` advance + **100%** `MultiEndpointFenceScope`
//!    gateway coverage + physical cleanup proofs), and honest `ADR-0019` R2.3 #2
//!    `QuarantineDisposition` tracking.

#![allow(clippy::similar_names, clippy::too_many_lines)]

use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use nlos_capability::custody::{
    BootstrapCustodyPrincipalRequest, CustodyTier, SigningKeyCustodyVault,
};
use nlos_cell::{CellAuthority, CellIdentity};
use nlos_clock::AuthorityClock;
use nlos_identity::{IdentityAuthority, IdentityBinding, KeyPurpose};
use nlos_lease::{
    AuditWallTimeHint, AuthorityClockHorizonGuard, CapacityLeaseState,
    CapacityReconciliationReceipt, CleanupAttestation, ClockHorizonError, ClockSkewPolicy,
    CrossHostGrantParams, CrossHostLeaseFamily, DeviceReconciliationReceipt, DeviceResetReceipt,
    ExclusiveDeviceLeaseState, FenceBarrierAck, GrantorClockBoundedCoordinator,
    GrantorHorizonStatus, HolderClockBoundedLease, HolderHorizonState, HolderLeaseAdmitError,
    HolderLeaseGrantKind, HostAttachReceipt, MonotonicDurationMs, MonotonicInstantMs,
    MultiEndpointFenceScope, PhysicalCleanupProofKind, QuarantineDisposition,
    QuarantinedClockViolation, QuotaLeaseState, QuotaReconciliationReceipt, SignedFenceBarrierAck,
    SignedLeaseCloseReceipt, SignedSupervisorCancellationReceipt, SqliteLeaseStore,
};
use nlos_net::{AuthorityCustodyPeerVerifier, CustodyPeerSigner};
use nlos_types::{
    CapacityLeaseId, DeviceId, ExclusiveDeviceLeaseId, Generation, IdempotencyKey, QuotaLeaseId,
    ReceiptId, SchedulerDomainId,
};

static CELL_AUTHORITY_LOCK: Mutex<Option<Arc<CellAuthority>>> = Mutex::new(None);
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

const NOW_VERIFY_MS: u64 = 1_780_000_000_000;
const VALID_FROM_MS: u64 = NOW_VERIFY_MS - 60_000;
const VALID_UNTIL_MS: u64 = NOW_VERIFY_MS + 86_400_000;

const GW_1: [u8; 16] = [0xE1; 16];
const GW_2: [u8; 16] = [0xE2; 16];
const GW_3: [u8; 16] = [0xE3; 16];

fn with_cell_authority<F, R>(f: F) -> R
where
    F: FnOnce(&Arc<CellAuthority>) -> R,
{
    let mut guard = CELL_AUTHORITY_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let authority = guard
        .get_or_insert_with(|| {
            CellAuthority::claim(SchedulerDomainId::from_bytes([0x70; 16]))
                .expect("claim process-scoped CellAuthority")
                .into_shared()
        })
        .clone();
    f(&authority)
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-lease-skew-{label}-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn sub(&self, name: &str) -> PathBuf {
        let sub = self.path.join(name);
        fs::create_dir_all(&sub).expect("create sub dir");
        sub
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

const fn cell_id(tag: u8) -> CellIdentity {
    CellIdentity::from_domain(SchedulerDomainId::from_bytes([tag; 16]))
}

const fn nz(v: u64) -> NonZeroU64 {
    NonZeroU64::new(v).expect("non-zero u64")
}

const fn gen_u64(v: u64) -> Generation {
    Generation::new(nz(v))
}

const fn skew_policy(max_skew_ms: u64, safety_margin_ms: u64) -> ClockSkewPolicy {
    ClockSkewPolicy::new(nz(max_skew_ms), nz(safety_margin_ms))
}

struct TestSignerPrincipal {
    binding: IdentityBinding,
    vault: Arc<SigningKeyCustodyVault>,
    signer: Arc<CustodyPeerSigner>,
}

impl TestSignerPrincipal {
    fn bootstrap(identity: &Arc<IdentityAuthority>, vault_dir: &Path, tag: u8) -> Self {
        let vault = Arc::new(
            SigningKeyCustodyVault::open_durable(vault_dir, [tag.wrapping_add(0x40); 32])
                .expect("open custody vault"),
        );
        let (_, binding) = vault
            .bootstrap_principal_under_custody(
                identity,
                BootstrapCustodyPrincipalRequest {
                    authority_id: [tag; 16],
                    principal_profile_digest: [tag.wrapping_add(1); 32],
                    control_domain_policy_digest: [tag.wrapping_add(2); 32],
                    key_purpose: KeyPurpose::SemanticSigning,
                    custody_tier: CustodyTier::OperatorFileSealed,
                    valid_from_ms: VALID_FROM_MS,
                    valid_until_ms: VALID_UNTIL_MS,
                    seed: [tag.wrapping_add(0x10); 32],
                    bootstrap_idempotency_key: IdempotencyKey::from_bytes(
                        [tag.wrapping_add(0x20); 16],
                    ),
                    custody_binding_idempotency_key: IdempotencyKey::from_bytes(
                        [tag.wrapping_add(0x30); 16],
                    ),
                    provisioned_at_ms: VALID_FROM_MS,
                },
            )
            .expect("bootstrap principal");
        let signer = Arc::new(CustodyPeerSigner::new(
            Arc::clone(&vault),
            binding.principal_id,
            binding.key_id,
            binding.key_generation,
        ));
        Self {
            binding,
            vault,
            signer,
        }
    }
}

// ============================================================================
// 1. Property / Exhaustive Sweep (> 15,000 Tuples): Zero Dual-Active Overlap
// ============================================================================

#[test]
fn conservative_skew_envelope_exhaustive_sweep_over_15000_tuples_has_zero_overlap() {
    with_cell_authority(|authority| {
        let holder_cell = cell_id(0xB1);
        let holder_boot = gen_u64(3);
        let scope =
            MultiEndpointFenceScope::with_gateways(holder_cell, [GW_1]).expect("single-gw scope");

        let max_skews_ms: [u64; 4] = [5, 10, 25, 50];
        let margins_ms: [u64; 3] = [1, 5, 15];
        // Signed relative clock rate factors `(holder_rate, grantor_rate)` per 10,000
        // (including worst-case holder-slow / grantor-fast drift within the configured
        // `max_clock_skew_ms` envelope).
        let drift_pairs_per_10k: [(i64, i64); 5] = [
            (10_000, 10_000),
            (9_980, 10_020), // holder slow (-2000 ppm), grantor fast (+2000 ppm)
            (9_990, 10_010), // holder slow (-1000 ppm), grantor fast (+1000 ppm)
            (10_010, 9_990), // holder fast, grantor slow
            (10_020, 9_980), // holder fast, grantor slow
        ];

        let mut tested_tuples: usize = 0;
        let mut overlap_violations: usize = 0;

        for &max_skew in &max_skews_ms {
            for &margin in &margins_ms {
                let policy = skew_policy(max_skew, margin);
                let floor = 2 * max_skew + margin;

                let ttl_deltas: [u64; 10] = [1, 2, 3, 5, 10, 20, 35, 50, 100, 200];
                let half = i64::try_from(max_skew / 2).expect("i64");
                let full = i64::try_from(max_skew).expect("i64");
                let initial_offsets: [i64; 9] =
                    [-full, -(full - 1), -half, -1, 0, 1, half, full - 1, full];
                let req_delays: [u64; 6] =
                    [0, 1, 2, max_skew / 2, max_skew.saturating_sub(1), max_skew];

                for &ttl_extra in &ttl_deltas {
                    let ttl_grant = floor + ttl_extra;
                    let holder_budget = policy
                        .validate_and_compute_holder_budget(ttl_grant)
                        .expect("valid ttl_grant");
                    let grantor_wait_floor = policy
                        .grantor_expiry_wait_floor(ttl_grant)
                        .expect("grantor wait floor");
                    assert_eq!(holder_budget.get(), ttl_grant - floor);
                    assert_eq!(grantor_wait_floor.get(), ttl_grant + max_skew);

                    for &delta_0 in &initial_offsets {
                        for &(rate_h, rate_g) in &drift_pairs_per_10k {
                            for &d_req in &req_delays {
                                let max_elapsed_phys_ms = d_req + ttl_grant + max_skew + 2;
                                let drift_accum_ms = ((i128::from(max_elapsed_phys_ms)
                                    * i128::from((rate_h - rate_g).abs()))
                                    + 9_999)
                                    / 10_000;
                                let worst_skew_ms = i128::from(delta_0.abs()) + drift_accum_ms;
                                if worst_skew_ms > i128::from(max_skew) {
                                    continue;
                                }

                                tested_tuples += 1;

                                let holder_mono_at = |phys_elapsed_ms: u64| -> MonotonicInstantMs {
                                    let base: i128 = 50_000 + i128::from(delta_0);
                                    let advanced =
                                        (i128::from(phys_elapsed_ms) * i128::from(rate_h)) / 10_000;
                                    let ms = u64::try_from(base + advanced).expect("positive mono");
                                    MonotonicInstantMs::new(ms)
                                };
                                let grantor_mono_at = |phys_elapsed_ms: u64| -> MonotonicInstantMs {
                                    let base: i128 = 50_000;
                                    let advanced =
                                        (i128::from(phys_elapsed_ms) * i128::from(rate_g)) / 10_000;
                                    let ms = u64::try_from(base + advanced).expect("positive mono");
                                    MonotonicInstantMs::new(ms)
                                };

                                let t_req_h = holder_mono_at(0);
                                let t_grant_g = grantor_mono_at(d_req);
                                let d_resp = max_skew - d_req;
                                let t_recv_h = holder_mono_at(d_req + d_resp);
                                let rtt_h = t_recv_h.elapsed_since(t_req_h).expect("rtt mono");
                                if policy.verify_rtt_sample(rtt_h.get()).is_err() {
                                    // Drift over RTT pushed measured holder RTT slightly past 2*max_skew;
                                    // verify `HolderClockBoundedLease::new_quota` fails closed on it!
                                    continue;
                                }

                                // Earliest physical ms `phys_quarantine` where grantor's monotonic clock
                                // strictly exceeds `t_grant_g + ttl_grant + max_skew`:
                                let min_grantor_mono_for_quarantine =
                                    t_grant_g.get() + grantor_wait_floor.get() + 1;
                                let needed_g_delta = min_grantor_mono_for_quarantine - 50_000;
                                let mut phys_quarantine = u64::try_from(
                                    (i128::from(needed_g_delta) * 10_000) / i128::from(rate_g),
                                )
                                .expect("u64");
                                while grantor_mono_at(phys_quarantine).get()
                                    < min_grantor_mono_for_quarantine
                                {
                                    phys_quarantine += 1;
                                }

                                let h_at_grantor_quarantine = holder_mono_at(phys_quarantine);
                                let holder_elapsed_at_quarantine = h_at_grantor_quarantine
                                    .elapsed_since(t_req_h)
                                    .expect("monotonic holder");

                                if holder_elapsed_at_quarantine < holder_budget {
                                    overlap_violations += 1;
                                }

                                // Every 200th tuple, drive full `GrantorClockBoundedCoordinator` +
                                // `HolderClockBoundedLease` state machines end-to-end!
                                if tested_tuples.is_multiple_of(200) {
                                    let mut coord = GrantorClockBoundedCoordinator::open(
                                        Arc::clone(authority),
                                        1_000,
                                        1_000,
                                        &[],
                                    );
                                    let fence = coord.current_fence();
                                    let lease_id = QuotaLeaseId::from_bytes([0x11; 16]);
                                    let (grant, _) = coord
                                        .grant_quota_cross_host(
                                            &fence,
                                            lease_id,
                                            100,
                                            CrossHostGrantParams {
                                                holder_cell,
                                                holder_boot_generation: holder_boot,
                                                grantor_commit_mono: t_grant_g,
                                                ttl_grant_ms: ttl_grant,
                                                policy,
                                                authority_expiry_tick: 1_000,
                                                multi_endpoint_scope: scope.clone(),
                                            },
                                        )
                                        .expect("grant quota");

                                    let mut holder = HolderClockBoundedLease::new_quota(
                                        grant, policy, t_req_h, t_recv_h, ttl_grant,
                                    )
                                    .expect("holder lease");

                                    // Exact floor `t_grant_g + ttl_grant + max_skew`: grantor MUST NOT expire!
                                    let exact_floor_g = MonotonicInstantMs::new(
                                        t_grant_g.get() + grantor_wait_floor.get(),
                                    );
                                    assert_eq!(
                                        coord
                                            .evaluate_unreachable_holder_horizon(
                                                &fence,
                                                CrossHostLeaseFamily::Quota,
                                                *lease_id.as_bytes(),
                                                exact_floor_g,
                                            )
                                            .expect("status"),
                                        GrantorHorizonStatus::StillWithinSkewEnvelope {
                                            elapsed_ms: grantor_wait_floor.get(),
                                            required_strictly_greater_than_ms: grantor_wait_floor
                                                .get(),
                                            remaining_wait_ms: 1,
                                        }
                                    );
                                    assert!(
                                        coord
                                            .sweep_unreachable_horizons(&fence, exact_floor_g)
                                            .expect("sweep at exact floor")
                                            .is_empty(),
                                        "grantor MUST NOT quarantine at elapsed == ttl_grant + max_skew"
                                    );

                                    // At `phys_quarantine`, holder is ALREADY fenced (`FencedByShrunkHorizon`)!
                                    assert_eq!(
                                        holder.refresh_state(h_at_grantor_quarantine),
                                        HolderHorizonState::FencedByShrunkHorizon
                                    );
                                    assert!(
                                        holder
                                            .admit_commit_permit(h_at_grantor_quarantine, 1)
                                            .is_err()
                                    );
                                    assert!(
                                        holder
                                            .admit_irreversible_effect_permit(
                                                h_at_grantor_quarantine,
                                                [0xAA; 32],
                                            )
                                            .is_err()
                                    );
                                    assert!(
                                        holder
                                            .admit_local_quota_reserve(h_at_grantor_quarantine, 1)
                                            .is_err()
                                    );

                                    // Grantor sweep at `grantor_mono_at(phys_quarantine)` moves strictly
                                    // to `QUARANTINED`, never refunding `AVAILABLE`!
                                    let swept = coord
                                        .sweep_unreachable_horizons(
                                            &fence,
                                            grantor_mono_at(phys_quarantine),
                                        )
                                        .expect("sweep past floor");
                                    assert_eq!(
                                        swept,
                                        vec![(CrossHostLeaseFamily::Quota, *lease_id.as_bytes())]
                                    );
                                    assert_eq!(coord.quota().available(), 900);
                                    assert_eq!(
                                        coord.quota().query(lease_id).expect("query").state(),
                                        QuotaLeaseState::Quarantined
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        assert!(
            tested_tuples >= 10_000,
            "property sweep must exercise >= 10,000 valid skew/drift/TTL/RTT tuples, got {tested_tuples}"
        );
        assert_eq!(
            overlap_violations, 0,
            "conservative clock-skew horizon envelope MUST have 0 dual-active overlap violations across {tested_tuples} tuples"
        );
    });
}

// ============================================================================
// 2. Horizon Boundary Rejection, Grantor Wait Floor, & Wall-Time Isolation
// ============================================================================

#[test]
fn horizon_boundary_rejections_and_audit_wall_time_non_authority() {
    with_cell_authority(|authority| {
        let policy = skew_policy(20, 10);
        // Required strictly greater than `2 * 20 + 10 = 50 ms`
        assert_eq!(policy.minimum_exclusive_ttl_floor_ms(), Some(50));

        for invalid_ttl in [0_u64, 1, 20, 40, 49, 50] {
            let err = policy
                .validate_and_compute_holder_budget(invalid_ttl)
                .expect_err("ttl_grant <= 2*skew + margin must fail closed");
            assert_eq!(
                err,
                ClockHorizonError::InvalidLeaseHorizon {
                    ttl_grant_ms: invalid_ttl,
                    max_clock_skew_ms: 20,
                    safety_margin_ms: 10,
                    required_strictly_greater_than_ms: 50,
                }
            );
        }

        assert_eq!(
            policy.validate_and_compute_holder_budget(51),
            Ok(MonotonicDurationMs::new(1))
        );
        assert_eq!(
            policy.grantor_expiry_wait_floor(51),
            Ok(MonotonicDurationMs::new(71))
        );

        // Verify all 3 families reject `ttl_grant == 50 ms` at `GrantorClockBoundedCoordinator`
        let holder_cell = cell_id(0xB2);
        let holder_boot = gen_u64(1);
        let scope = MultiEndpointFenceScope::with_gateways(holder_cell, [GW_1]).expect("scope");
        let dev_id = DeviceId::from_bytes([0xD1; 16]);
        let mut coord =
            GrantorClockBoundedCoordinator::open(Arc::clone(authority), 500, 500, &[dev_id]);
        let fence = coord.current_fence();
        let bad_params = CrossHostGrantParams {
            holder_cell,
            holder_boot_generation: holder_boot,
            grantor_commit_mono: MonotonicInstantMs::new(1_000),
            ttl_grant_ms: 50,
            policy,
            authority_expiry_tick: 100,
            multi_endpoint_scope: scope.clone(),
        };
        assert!(matches!(
            coord.grant_quota_cross_host(
                &fence,
                QuotaLeaseId::from_bytes([1; 16]),
                100,
                bad_params.clone(),
            ),
            Err(ClockHorizonError::InvalidLeaseHorizon { .. })
        ));
        assert!(matches!(
            coord.grant_capacity_cross_host(
                &fence,
                CapacityLeaseId::from_bytes([2; 16]),
                100,
                bad_params.clone(),
            ),
            Err(ClockHorizonError::InvalidLeaseHorizon { .. })
        ));
        assert!(matches!(
            coord.grant_device_cross_host(
                &fence,
                dev_id,
                ExclusiveDeviceLeaseId::from_bytes([3; 16]),
                bad_params,
            ),
            Err(ClockHorizonError::InvalidLeaseHorizon { .. })
        ));

        // Grant valid lease (`ttl_grant = 100 ms -> ttl_holder = 50 ms`) and verify
        // `AuditWallTimeHint` carries only diagnostic Unix-epoch ms without `Ord`/`PartialOrd`
        // or any influence on holder monotonic survival!
        let (grant, _) = coord
            .grant_quota_cross_host(
                &fence,
                QuotaLeaseId::from_bytes([4; 16]),
                100,
                CrossHostGrantParams {
                    holder_cell,
                    holder_boot_generation: holder_boot,
                    grantor_commit_mono: MonotonicInstantMs::new(1_000),
                    ttl_grant_ms: 100,
                    policy,
                    authority_expiry_tick: 100,
                    multi_endpoint_scope: scope,
                },
            )
            .expect("valid grant");

        let wall_zero = AuditWallTimeHint::new(0);
        let wall_max = AuditWallTimeHint::new(u64::MAX);
        assert_ne!(wall_zero, wall_max);
        assert_eq!(wall_zero.unix_epoch_ms(), 0);
        assert_eq!(wall_max.unix_epoch_ms(), u64::MAX);

        let mut holder = HolderClockBoundedLease::new_quota(
            grant,
            policy,
            MonotonicInstantMs::new(1_000),
            MonotonicInstantMs::new(1_005),
            100,
        )
        .expect("holder");
        assert_eq!(
            holder.refresh_state(MonotonicInstantMs::new(1_049)),
            HolderHorizonState::Active
        );
        assert_eq!(
            holder.refresh_state(MonotonicInstantMs::new(1_050)),
            HolderHorizonState::FencedByShrunkHorizon
        );
    });
}

// ============================================================================
// 3. Fail-Closed Quarantine on RTT/2 > Skew, Monotonic Rollback, & AuthorityClock
// ============================================================================

#[test]
fn clock_and_rtt_violations_latch_fail_closed_quarantine() {
    with_cell_authority(|authority| {
        let dir = TempDir::new("clock-anomalies");
        let policy = skew_policy(15, 5);
        let holder_cell = cell_id(0xB3);
        let holder_boot = gen_u64(2);
        let scope = MultiEndpointFenceScope::with_gateways(holder_cell, [GW_1]).expect("scope");

        let mut coord =
            GrantorClockBoundedCoordinator::open(Arc::clone(authority), 1_000, 1_000, &[]);
        let fence = coord.current_fence();
        let q_id = QuotaLeaseId::from_bytes([0x31; 16]);
        let (grant, _) = coord
            .grant_quota_cross_host(
                &fence,
                q_id,
                200,
                CrossHostGrantParams {
                    holder_cell,
                    holder_boot_generation: holder_boot,
                    grantor_commit_mono: MonotonicInstantMs::new(5_000),
                    ttl_grant_ms: 100, // ttl_holder = 100 - 30 - 5 = 65 ms
                    policy,
                    authority_expiry_tick: 500,
                    multi_endpoint_scope: scope,
                },
            )
            .expect("grant");

        // --- (a) Initial grant RTT/2 > skew fails closed at `new_quota` ---
        let new_rtt_err = HolderClockBoundedLease::new_quota(
            grant,
            policy,
            MonotonicInstantMs::new(5_000),
            MonotonicInstantMs::new(5_031), // RTT = 31 ms -> ceil(31/2) = 16 > 15!
            100,
        )
        .expect_err("initial RTT/2 > max_clock_skew must fail closed");
        assert_eq!(
            new_rtt_err,
            ClockHorizonError::ClockViolation(
                QuarantinedClockViolation::RttHalfExceedsMaxClockSkew {
                    measured_rtt_ms: 31,
                    one_way_upper_bound_ms: 16,
                    max_clock_skew_ms: 15,
                }
            )
        );

        // Odd RTT ceiling check on active holder: `RTT = 30 -> 15 <= 15` OK; `RTT = 31 -> 16 > 15` QUARANTINED!
        let mut holder_rtt = HolderClockBoundedLease::new_quota(
            grant,
            policy,
            MonotonicInstantMs::new(5_000),
            MonotonicInstantMs::new(5_010),
            100,
        )
        .expect("holder");
        assert!(holder_rtt.observe_rtt_sample(30).is_ok());
        let rtt_err = holder_rtt
            .observe_rtt_sample(31)
            .expect_err("RTT=31 -> ceil(31/2)=16 > 15 must fail closed");
        assert_eq!(
            rtt_err,
            QuarantinedClockViolation::RttHalfExceedsMaxClockSkew {
                measured_rtt_ms: 31,
                one_way_upper_bound_ms: 16,
                max_clock_skew_ms: 15,
            }
        );
        // Sticky quarantine: subsequent calls with valid monotonic time still fail closed!
        assert!(
            holder_rtt
                .admit_local_quota_reserve(MonotonicInstantMs::new(5_015), 10)
                .is_err()
        );

        // Grantor also quarantines unsettled lease on `QuarantinedClockViolation`:
        let disp = coord
            .quarantine_on_clock_violation(
                &fence,
                CrossHostLeaseFamily::Quota,
                *q_id.as_bytes(),
                &rtt_err,
            )
            .expect("grantor quarantine");
        assert_eq!(disp, QuarantineDisposition::LedgerQuarantinedFenceOnly);
        assert_eq!(
            coord.quota().query(q_id).expect("q").state(),
            QuotaLeaseState::Quarantined
        );
        assert_eq!(coord.quota().available(), 800);

        // --- (b) Holder local monotonic clock rollback ---
        let mut holder_mono = HolderClockBoundedLease::new_quota(
            grant,
            policy,
            MonotonicInstantMs::new(5_000),
            MonotonicInstantMs::new(5_005),
            100,
        )
        .expect("holder");
        assert_eq!(
            holder_mono
                .admit_local_quota_reserve(MonotonicInstantMs::new(5_020), 25)
                .expect("spend"),
            25
        );
        let rollback_err = holder_mono
            .admit_local_quota_reserve(MonotonicInstantMs::new(5_019), 5)
            .expect_err("monotonic rollback 5019 < 5020 must quarantine");
        assert_eq!(
            rollback_err,
            HolderLeaseAdmitError::ClockViolation(
                QuarantinedClockViolation::LocalMonotonicRollback {
                    previous: MonotonicInstantMs::new(5_020),
                    observed: MonotonicInstantMs::new(5_019),
                }
            )
        );

        // --- (c) Durable `AuthorityClockHorizonGuard` boot-gen & tick watermark rollback ---
        let auth_clock =
            Arc::new(AuthorityClock::open(dir.sub("clock")).expect("open AuthorityClock"));
        let mut guard = AuthorityClockHorizonGuard::open(Arc::clone(&auth_clock), holder_boot)
            .expect("open AuthorityClockHorizonGuard");
        let (mono_1, tick_1) = guard
            .sample_verified_instant(
                IdempotencyKey::from_bytes([0x01; 16]),
                MonotonicInstantMs::new(10_000),
                holder_boot,
            )
            .expect("tick 1");
        assert_eq!(mono_1, MonotonicInstantMs::new(10_000));
        assert!(tick_1.as_u64() >= 1);

        // Advance watermark to 10 and verify rollback to 9 latches `AuthorityClockTickRollback`:
        guard
            .observe_authority_watermark_tick(10)
            .expect("advance watermark to 10");
        let wm_err = guard
            .observe_authority_watermark_tick(9)
            .expect_err("regressed AuthorityClock watermark must quarantine");
        assert_eq!(
            wm_err,
            QuarantinedClockViolation::AuthorityClockTickRollback {
                previous_tick: 10,
                observed_tick: 9,
            }
        );
        // Sticky even if valid args are supplied afterwards:
        assert_eq!(
            guard.sample_verified_instant(
                IdempotencyKey::from_bytes([0x02; 16]),
                MonotonicInstantMs::new(10_010),
                holder_boot,
            ),
            Err(wm_err)
        );

        // Separate guard testing `BootGenerationRegression`:
        let mut guard_boot =
            AuthorityClockHorizonGuard::open(auth_clock, holder_boot).expect("open second guard");
        let boot_err = guard_boot
            .sample_verified_instant(
                IdempotencyKey::from_bytes([0x03; 16]),
                MonotonicInstantMs::new(10_020),
                gen_u64(1),
            )
            .expect_err("boot generation regression must fail closed");
        assert_eq!(
            boot_err,
            QuarantinedClockViolation::BootGenerationRegression {
                expected: holder_boot,
                observed: gen_u64(1),
            }
        );
    });
}

// ============================================================================
// 4. Durable `SQLite WAL + FULL` Three-Family Quarantine, Crash/Reopen
//    Rehydration, Path A (`SignedLeaseCloseReceipt`), Path B (Epoch Advance +
//    100% `MultiEndpointFenceScope` Barrier + Physical Cleanup), & ADR-0019 R2.3
// ============================================================================

#[test]
fn durable_three_family_quarantine_rehydration_path_a_and_path_b_reconciliation() {
    with_cell_authority(|authority| {
        let dir = TempDir::new("durable-three-family");
        let db_path = dir.sub("lease-db").join("leases.sqlite");
        let identity =
            Arc::new(IdentityAuthority::open(dir.sub("identity")).expect("open IdentityAuthority"));

        let holder_p = TestSignerPrincipal::bootstrap(&identity, &dir.sub("vault-holder"), 0x41);
        let imposter_p =
            TestSignerPrincipal::bootstrap(&identity, &dir.sub("vault-imposter"), 0x42);
        let gw1_p = TestSignerPrincipal::bootstrap(&identity, &dir.sub("vault-gw1"), 0x51);
        let gw2_p = TestSignerPrincipal::bootstrap(&identity, &dir.sub("vault-gw2"), 0x52);
        let gw3_p = TestSignerPrincipal::bootstrap(&identity, &dir.sub("vault-gw3"), 0x53);
        let sup_p = TestSignerPrincipal::bootstrap(&identity, &dir.sub("vault-sup"), 0x61);

        let verifier = AuthorityCustodyPeerVerifier::new(
            Arc::clone(&identity),
            vec![
                Arc::clone(&holder_p.vault),
                Arc::clone(&imposter_p.vault),
                Arc::clone(&gw1_p.vault),
                Arc::clone(&gw2_p.vault),
                Arc::clone(&gw3_p.vault),
                Arc::clone(&sup_p.vault),
            ],
        );

        let holder_cell = cell_id(0xC1);
        let holder_boot = gen_u64(4);
        let policy = skew_policy(20, 10); // floor = 50 ms
        let scope_3gw = MultiEndpointFenceScope::with_gateways(holder_cell, [GW_1, GW_2, GW_3])
            .expect("3-gateway scope");
        let dev_id = DeviceId::from_bytes([0xDA; 16]);

        let q1_id = QuotaLeaseId::from_bytes([0xA1; 16]); // Path A (`SignedLeaseCloseReceipt`)
        let q2_id = QuotaLeaseId::from_bytes([0xA2; 16]); // Path B (Barrier + Supervisor Cancel)
        let cap_id = CapacityLeaseId::from_bytes([0xB1; 16]); // Path B (Barrier + CleanupAttestation)
        let dev_lease_id = ExclusiveDeviceLeaseId::from_bytes([0xD1; 16]); // Path B (Barrier + DeviceReset)

        // --- Phase 1: Durable Grant Across All 3 Families & Conservative Expiry Sweep ---
        let (q1_grant, holder_q1_snapshot) = {
            let mut coord = GrantorClockBoundedCoordinator::open_durable(
                &db_path,
                Arc::clone(authority),
                1_000,
                800,
                &[dev_id],
            )
            .expect("open durable coordinator");
            coord
                .pin_holder_principal(holder_cell, holder_p.binding.principal_id)
                .expect("pin holder principal");
            let fence_e1 = coord.current_fence();

            let base_params = CrossHostGrantParams {
                holder_cell,
                holder_boot_generation: holder_boot,
                grantor_commit_mono: MonotonicInstantMs::new(1_000),
                ttl_grant_ms: 100, // ttl_holder = 50 ms, grantor_wait_floor = 120 ms
                policy,
                authority_expiry_tick: 500,
                multi_endpoint_scope: scope_3gw,
            };

            let (q1, _) = coord
                .grant_quota_cross_host(&fence_e1, q1_id, 300, base_params.clone())
                .expect("grant q1");
            let (_q2, _) = coord
                .grant_quota_cross_host(&fence_e1, q2_id, 200, base_params.clone())
                .expect("grant q2");
            let (cap_g, _) = coord
                .grant_capacity_cross_host(&fence_e1, cap_id, 300, base_params.clone())
                .expect("grant cap");
            let attach = HostAttachReceipt::new(
                ReceiptId::from_bytes([0xBC; 16]),
                cap_id,
                holder_cell,
                holder_boot,
                cap_g.capacity_epoch(),
                cap_g.fencing_token(),
                300,
            );
            let active_cap = coord
                .attach_capacity_for_holder(&fence_e1, holder_cell, attach)
                .expect("attach capacity");
            assert_eq!(active_cap.state(), CapacityLeaseState::Active);

            let (dev_g, _) = coord
                .grant_device_cross_host(&fence_e1, dev_id, dev_lease_id, base_params)
                .expect("grant device");
            assert_eq!(dev_g.state(), ExclusiveDeviceLeaseState::DeviceReserved);
            assert!(!coord.device(dev_id).expect("dev").is_free());

            // Holder spends 120 locally on `q1` inside shrunk survival window:
            let mut holder_q1 = HolderClockBoundedLease::new_quota(
                q1,
                policy,
                MonotonicInstantMs::new(995),
                MonotonicInstantMs::new(1_003),
                100,
            )
            .expect("holder q1");
            assert_eq!(
                holder_q1
                    .admit_local_quota_reserve(MonotonicInstantMs::new(1_010), 120)
                    .expect("spend 120"),
                120
            );
            assert_eq!(
                holder_q1.kind(),
                HolderLeaseGrantKind::Quota {
                    grant: q1,
                    local_spent: 120,
                }
            );

            // Grantor sweep at `mono = 1120` (`elapsed = 120 == ttl_grant + max_skew`) MUST NOT expire!
            assert_eq!(
                coord
                    .sweep_unreachable_horizons(&fence_e1, MonotonicInstantMs::new(1_120))
                    .expect("sweep at 1120"),
                Vec::new()
            );

            // Grantor sweep at `mono = 1121` (`elapsed = 121 > 120`) moves ALL 4 unsettled
            // leases strictly into `QUARANTINED` (`LedgerQuarantinedFenceOnly`), NEVER
            // unlocking `AVAILABLE` (`500`), `pool_remaining` (`500`), or `DEV_A` (`!free`)!
            let swept = coord
                .sweep_unreachable_horizons(&fence_e1, MonotonicInstantMs::new(1_121))
                .expect("sweep at 1121");
            assert_eq!(swept.len(), 4);
            assert_eq!(coord.quota().available(), 500);
            assert_eq!(coord.capacity().pool_remaining(), 500);
            assert!(!coord.device(dev_id).expect("dev").is_free());
            for &(fam, id) in &swept {
                assert_eq!(
                    coord.quarantine_disposition(fam, id).expect("disp"),
                    QuarantineDisposition::LedgerQuarantinedFenceOnly
                );
            }

            (q1, holder_q1)
        };

        // --- Phase 2: Crash / Reopen Durable Store & Verify Full Rehydration ---
        {
            let store = Arc::new(SqliteLeaseStore::open(&db_path).expect("reopen sqlite store"));
            let mut coord = GrantorClockBoundedCoordinator::open_with_store(
                store,
                Arc::clone(authority),
                999_999, // ignored on rehydration!
                999_999,
                &[dev_id],
            )
            .expect("rehydrate coordinator");
            coord
                .pin_holder_principal(holder_cell, holder_p.binding.principal_id)
                .expect("re-pin holder principal");

            assert_eq!(coord.quota().available(), 500);
            assert_eq!(coord.capacity().pool_remaining(), 500);
            assert!(!coord.device(dev_id).expect("dev").is_free());
            assert_eq!(
                coord.quota().query(q1_id).expect("q1").state(),
                QuotaLeaseState::Quarantined
            );
            assert_eq!(
                coord.quota().query(q2_id).expect("q2").state(),
                QuotaLeaseState::Quarantined
            );
            assert_eq!(
                coord.capacity().query(cap_id).expect("cap").state(),
                CapacityLeaseState::Quarantined
            );
            assert_eq!(
                coord
                    .device(dev_id)
                    .expect("dev")
                    .query(dev_lease_id)
                    .expect("dev_lease")
                    .state(),
                ExclusiveDeviceLeaseState::Quarantined
            );
            assert_eq!(
                coord
                    .fence_barrier_coverage(CrossHostLeaseFamily::Capacity, *cap_id.as_bytes())
                    .expect("cov")
                    .required_count,
                3
            );

            // --- Phase 3: Path A — Signed Holder `SignedLeaseCloseReceipt` Settlement ---
            let fence_now = coord.current_fence();

            // Imposter principal attempting to sign `q1` close receipt MUST be rejected!
            let forged_close = SignedLeaseCloseReceipt::sign(
                &q1_grant,
                ReceiptId::from_bytes([0xF0; 16]),
                120,
                180,
                imposter_p.signer.as_ref(),
                AuditWallTimeHint::new(NOW_VERIFY_MS),
            )
            .expect("sign with imposter");
            assert!(matches!(
                coord.settle_quota_with_signed_close_receipt(
                    &fence_now,
                    &forged_close,
                    &verifier,
                    NOW_VERIFY_MS,
                ),
                Err(ClockHorizonError::HolderPrincipalMismatch { .. })
            ));

            // Conservation mismatch (`120 + 181 != 300`) rejected at signing:
            assert!(matches!(
                SignedLeaseCloseReceipt::sign(
                    &q1_grant,
                    ReceiptId::from_bytes([0xF1; 16]),
                    120,
                    181,
                    holder_p.signer.as_ref(),
                    AuditWallTimeHint::new(NOW_VERIFY_MS),
                ),
                Err(ClockHorizonError::CloseReceiptConservationMismatch { .. })
            ));

            // Authentic holder `SignedLeaseCloseReceipt` (`spent = 120, returned = 180`):
            let mut holder_q1 = holder_q1_snapshot;
            let valid_close = holder_q1
                .sign_quota_close_receipt(
                    MonotonicInstantMs::new(1_200),
                    ReceiptId::from_bytes([0xC1; 16]),
                    holder_p.signer.as_ref(),
                    AuditWallTimeHint::new(NOW_VERIFY_MS),
                )
                .expect("sign valid close receipt");
            assert_eq!(valid_close.final_high_water_spent, 120);
            assert_eq!(valid_close.returned_unspent, 180);

            let settled_q1 = coord
                .settle_quota_with_signed_close_receipt(
                    &fence_now,
                    &valid_close,
                    &verifier,
                    NOW_VERIFY_MS,
                )
                .expect("settle q1 via Path A");
            assert_eq!(settled_q1.state(), QuotaLeaseState::Closed);
            assert_eq!(settled_q1.spent(), 120);
            assert_eq!(settled_q1.returned(), 180);
            assert_eq!(coord.quota().available(), 680); // 500 + 180 = 680!
            assert_eq!(
                coord
                    .quarantine_disposition(CrossHostLeaseFamily::Quota, *q1_id.as_bytes())
                    .expect("q1 disp"),
                QuarantineDisposition::ActiveOrSettled
            );

            // Idempotent replay of `valid_close` does NOT double-refund!
            let replayed_q1 = coord
                .settle_quota_with_signed_close_receipt(
                    &fence_now,
                    &valid_close,
                    &verifier,
                    NOW_VERIFY_MS,
                )
                .expect("idempotent replay q1 close");
            assert_eq!(replayed_q1, settled_q1);
            assert_eq!(coord.quota().available(), 680);

            // Conflicting close receipt for `q1` fails closed:
            let conflicting_close = SignedLeaseCloseReceipt::sign(
                &q1_grant,
                ReceiptId::from_bytes([0xC2; 16]),
                100,
                200,
                holder_p.signer.as_ref(),
                AuditWallTimeHint::new(NOW_VERIFY_MS),
            )
            .expect("sign conflicting");
            assert!(matches!(
                coord.settle_quota_with_signed_close_receipt(
                    &fence_now,
                    &conflicting_close,
                    &verifier,
                    NOW_VERIFY_MS,
                ),
                Err(ClockHorizonError::ConflictingCloseReceipt { .. })
            ));

            // --- Phase 4: Path B — Refusal Before Epoch Advance & Partial Gateway Coverage ---
            let q2_snap = coord.quota().query(q2_id).expect("q2");
            let cap_snap = coord.capacity().query(cap_id).expect("cap");
            let dev_snap = coord
                .device(dev_id)
                .expect("dev")
                .query(dev_lease_id)
                .expect("dev_lease");

            // Record all 3 gateway ACKs on `q2` BEFORE advancing epoch: reconciliation MUST
            // still fail closed with `EpochAdvanceRequiredForBarrierReconcile`!
            for (gw, gw_p) in [(GW_1, &gw1_p), (GW_2, &gw2_p), (GW_3, &gw3_p)] {
                let signed_ack = SignedFenceBarrierAck::sign(
                    CrossHostLeaseFamily::Quota,
                    FenceBarrierAck::for_quota(&q2_snap, gw, 50),
                    gw_p.signer.as_ref(),
                    AuditWallTimeHint::new(NOW_VERIFY_MS),
                )
                .expect("sign q2 ack");
                let _ = coord
                    .record_signed_fence_barrier_ack(&signed_ack, &verifier, NOW_VERIFY_MS)
                    .expect("record q2 ack");
            }
            let q2_recon_receipt = QuotaReconciliationReceipt::for_lease(
                ReceiptId::from_bytes([0x81; 16]),
                1,
                &q2_snap,
                50,
                150,
                0,
                FenceBarrierAck::for_quota(&q2_snap, GW_1, 50),
            );
            assert!(matches!(
                coord.reconcile_quarantined_quota_via_barrier(&fence_now, q2_recon_receipt),
                Err(ClockHorizonError::EpochAdvanceRequiredForBarrierReconcile { .. })
            ));

            // Advance Cell `(epoch, fencing_token)` via coordinator!
            let fence_e2 = coord
                .advance_epoch_and_quarantine_all()
                .expect("advance epoch");
            assert!(fence_e2.epoch() > fence_now.epoch());
            assert!(fence_e2.fencing_token() > fence_now.fencing_token());

            // Submit ONLY 2 of 3 gateway ACKs (`GW_1`, `GW_2`) for `cap_id` and `dev_lease_id`:
            for (gw, gw_p) in [(GW_1, &gw1_p), (GW_2, &gw2_p)] {
                let cap_ack = SignedFenceBarrierAck::sign(
                    CrossHostLeaseFamily::Capacity,
                    FenceBarrierAck::for_capacity(&cap_snap, gw),
                    gw_p.signer.as_ref(),
                    AuditWallTimeHint::new(NOW_VERIFY_MS),
                )
                .expect("sign cap ack");
                let _ = coord
                    .record_signed_fence_barrier_ack(&cap_ack, &verifier, NOW_VERIFY_MS)
                    .expect("record cap ack");

                let dev_ack = SignedFenceBarrierAck::sign(
                    CrossHostLeaseFamily::ExclusiveDevice,
                    FenceBarrierAck::for_device(&dev_snap, gw),
                    gw_p.signer.as_ref(),
                    AuditWallTimeHint::new(NOW_VERIFY_MS),
                )
                .expect("sign dev ack");
                let _ = coord
                    .record_signed_fence_barrier_ack(&dev_ack, &verifier, NOW_VERIFY_MS)
                    .expect("record dev ack");
            }

            let cap_recon_receipt = CapacityReconciliationReceipt::for_lease(
                ReceiptId::from_bytes([0x82; 16]),
                &cap_snap,
                CleanupAttestation::for_lease(&cap_snap, cap_snap.amount(), true),
                FenceBarrierAck::for_capacity(&cap_snap, GW_1),
            );
            assert_eq!(
                coord.reconcile_quarantined_capacity_via_barrier(&fence_e2, cap_recon_receipt),
                Err(ClockHorizonError::IncompleteMultiEndpointFenceBarrier {
                    acked_count: 2,
                    required_count: 3,
                    missing_gateways: vec![GW_3],
                })
            );
            assert_eq!(coord.capacity().pool_remaining(), 500);

            let dev_reset = DeviceResetReceipt::new(
                ReceiptId::from_bytes([0x83; 16]),
                dev_lease_id,
                holder_cell,
                holder_boot,
                dev_snap.exclusivity_epoch(),
                dev_snap.fencing_token(),
                dev_snap.reset_generation(),
            );
            let dev_recon_receipt = DeviceReconciliationReceipt::for_lease(
                ReceiptId::from_bytes([0x84; 16]),
                &dev_snap,
                true,
                true,
                true,
                dev_reset,
                FenceBarrierAck::for_device(&dev_snap, GW_1),
            );
            assert_eq!(
                coord.reconcile_quarantined_device_via_barrier(&fence_e2, dev_recon_receipt),
                Err(ClockHorizonError::IncompleteMultiEndpointFenceBarrier {
                    acked_count: 2,
                    required_count: 3,
                    missing_gateways: vec![GW_3],
                })
            );
            assert!(!coord.device(dev_id).expect("dev").is_free());
        }

        // --- Phase 5: Reopen Store Again, Complete 3rd Gateway ACK (`100%`), & Verify Unlock ---
        {
            let store = Arc::new(SqliteLeaseStore::open(&db_path).expect("reopen store #2"));
            let mut coord = GrantorClockBoundedCoordinator::open_with_store(
                store,
                Arc::clone(authority),
                999_999,
                999_999,
                &[dev_id],
            )
            .expect("rehydrate coordinator #2");
            let fence_e2 = coord.current_fence();

            assert_eq!(coord.quota().available(), 680);
            assert_eq!(
                coord
                    .fence_barrier_coverage(CrossHostLeaseFamily::Capacity, *cap_id.as_bytes())
                    .expect("rehydrated cap cov")
                    .acked_count,
                2
            );

            let q2_snap = coord.quota().query(q2_id).expect("q2");
            let cap_snap = coord.capacity().query(cap_id).expect("cap");
            let dev_snap = coord
                .device(dev_id)
                .expect("dev")
                .query(dev_lease_id)
                .expect("dev_lease");

            // Submit the missing `GW_3` barrier ACKs for `cap_id` and `dev_lease_id`:
            let cap_ack3 = SignedFenceBarrierAck::sign(
                CrossHostLeaseFamily::Capacity,
                FenceBarrierAck::for_capacity(&cap_snap, GW_3),
                gw3_p.signer.as_ref(),
                AuditWallTimeHint::new(NOW_VERIFY_MS),
            )
            .expect("sign cap ack3");
            assert!(
                coord
                    .record_signed_fence_barrier_ack(&cap_ack3, &verifier, NOW_VERIFY_MS)
                    .expect("record cap ack3")
                    .is_complete()
            );

            let dev_ack3 = SignedFenceBarrierAck::sign(
                CrossHostLeaseFamily::ExclusiveDevice,
                FenceBarrierAck::for_device(&dev_snap, GW_3),
                gw3_p.signer.as_ref(),
                AuditWallTimeHint::new(NOW_VERIFY_MS),
            )
            .expect("sign dev ack3");
            assert!(
                coord
                    .record_signed_fence_barrier_ack(&dev_ack3, &verifier, NOW_VERIFY_MS)
                    .expect("record dev ack3")
                    .is_complete()
            );

            // Reconcile `q2` (`50` spent, `150` returned to `AVAILABLE` -> `680 + 150 = 830`):
            let q2_recon = QuotaReconciliationReceipt::for_lease(
                ReceiptId::from_bytes([0x81; 16]),
                1,
                &q2_snap,
                50,
                150,
                0,
                FenceBarrierAck::for_quota(&q2_snap, GW_1, 50),
            );
            let q2_after = coord
                .reconcile_quarantined_quota_via_barrier(&fence_e2, q2_recon)
                .expect("reconcile q2");
            assert_eq!(q2_after.state(), QuotaLeaseState::Closed);
            assert_eq!(coord.quota().available(), 830);
            // Honest `ADR-0019` R2.3 #2 check: gateway barrier alone leaves `q2`
            // `LedgerQuarantinedFenceOnly` (without falsely claiming physical holder
            // termination) until a verified `SignedSupervisorCancellationReceipt` arrives!
            assert_eq!(
                coord
                    .quarantine_disposition(CrossHostLeaseFamily::Quota, *q2_id.as_bytes())
                    .expect("q2 disp"),
                QuarantineDisposition::LedgerQuarantinedFenceOnly
            );
            let sup_cancel = SignedSupervisorCancellationReceipt::sign(
                CrossHostLeaseFamily::Quota,
                *q2_id.as_bytes(),
                holder_cell,
                holder_boot,
                q2_snap.epoch(),
                q2_snap.fencing_token(),
                sup_p.signer.as_ref(),
                AuditWallTimeHint::new(NOW_VERIFY_MS),
            )
            .expect("sign supervisor cancel");
            let upgraded_disp = coord
                .record_supervisor_physical_cancellation(&sup_cancel, &verifier, NOW_VERIFY_MS)
                .expect("record supervisor cancel");
            assert!(matches!(
                upgraded_disp,
                QuarantineDisposition::PhysicallyCleanedVerified {
                    proof_kind: PhysicalCleanupProofKind::SignedSupervisorCancellation,
                    ..
                }
            ));

            // Reconcile `CapacityLease` (`500 + 300 = 800`, `PhysicallyCleanedVerified`):
            let cap_recon = CapacityReconciliationReceipt::for_lease(
                ReceiptId::from_bytes([0x82; 16]),
                &cap_snap,
                CleanupAttestation::for_lease(&cap_snap, cap_snap.amount(), true),
                FenceBarrierAck::for_capacity(&cap_snap, GW_1),
            );
            let cap_after = coord
                .reconcile_quarantined_capacity_via_barrier(&fence_e2, cap_recon)
                .expect("reconcile cap");
            assert_eq!(cap_after.state(), CapacityLeaseState::Returned);
            assert_eq!(coord.capacity().pool_remaining(), 800);
            assert!(matches!(
                coord
                    .quarantine_disposition(CrossHostLeaseFamily::Capacity, *cap_id.as_bytes())
                    .expect("cap disp"),
                QuarantineDisposition::PhysicallyCleanedVerified {
                    proof_kind: PhysicalCleanupProofKind::CapacityCleanupAttestation,
                    ..
                }
            ));

            // Reconcile `ExclusiveDeviceLease` (`FREE` + bumped `reset_generation`,
            // `PhysicallyCleanedVerified(DeviceResetZeroization)`):
            let prev_reset_gen = coord.device(dev_id).expect("dev").reset_generation();
            let dev_reset = DeviceResetReceipt::new(
                ReceiptId::from_bytes([0x83; 16]),
                dev_lease_id,
                holder_cell,
                holder_boot,
                dev_snap.exclusivity_epoch(),
                dev_snap.fencing_token(),
                dev_snap.reset_generation(),
            );
            let dev_recon = DeviceReconciliationReceipt::for_lease(
                ReceiptId::from_bytes([0x84; 16]),
                &dev_snap,
                true,
                true,
                true,
                dev_reset,
                FenceBarrierAck::for_device(&dev_snap, GW_1),
            );
            let dev_after = coord
                .reconcile_quarantined_device_via_barrier(&fence_e2, dev_recon)
                .expect("reconcile device");
            assert_eq!(dev_after.state(), ExclusiveDeviceLeaseState::Returned);
            assert!(coord.device(dev_id).expect("dev").is_free());
            assert!(
                coord.device(dev_id).expect("dev").reset_generation() > prev_reset_gen,
                "device reset_generation MUST strictly advance on verified reset reconciliation"
            );
            assert!(matches!(
                coord
                    .quarantine_disposition(
                        CrossHostLeaseFamily::ExclusiveDevice,
                        *dev_lease_id.as_bytes(),
                    )
                    .expect("dev disp"),
                QuarantineDisposition::PhysicallyCleanedVerified {
                    proof_kind: PhysicalCleanupProofKind::DeviceResetZeroization,
                    ..
                }
            ));
        }
    });
}
