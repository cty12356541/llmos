//! Integration tests for cross-host receiver-local monotonic liveness windows,
//! link clock-skew quarantine guards, and per-Cell monotonic checkpoint trails
//! (`ADR-0026` §2.3, `ADR-0019` Trigger 7 #1 & #2, `[MODEL-TIME-001]`).

use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_cell::{
    AuditWallTimestampHint, CellDirectory, CellEpoch, CellFence, CellFencingToken, CellIdentity,
    CheckpointDigest, CheckpointDigestVerification, CheckpointFact, CheckpointPrefix,
    ClockLivenessError, ControlPlaneLinkState, CrossHostHeartbeatDecision,
    CrossHostHeartbeatEnvelope, CrossHostHeartbeatWatermark, CrossHostLinkClockGuard,
    HeuristicPeerLiveness, LinkClockHealthState, LinkClockViolation, MonotonicCheckpointCoord,
    MonotonicCheckpointEntry, MonotonicRecvInstantMs, PartitionedActionKind,
    REASON_CLOCK_SKEW_RTT_EXCEEDED, REASON_LOCAL_MONOTONIC_REGRESSION,
    ReceiverMonotonicLivenessTracker,
};
use nlos_types::{Generation, SchedulerDomainId};

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nlos-cell-clock-liveness-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn cell(byte: u8) -> CellIdentity {
    CellIdentity::from_domain(SchedulerDomainId::from_bytes([byte; 16]))
}

fn nz(v: u64) -> NonZeroU64 {
    NonZeroU64::new(v).expect("non-zero")
}

#[test]
fn receiver_monotonic_liveness_ignores_remote_wall_hints_and_rejects_replay_and_regression() {
    let peer_a = cell(0xA1);
    let mut tracker = ReceiverMonotonicLivenessTracker::new(500, nz(50));

    // Heartbeat 1 arrives at receiver mono = 1_000ms, carrying a wildly future
    // sender wall-clock hint (`u64::MAX - 7`).
    let wm1 = CrossHostHeartbeatWatermark::new(Generation::INITIAL, CellEpoch::INITIAL, nz(1));
    let env1 = CrossHostHeartbeatEnvelope::new(
        peer_a,
        wm1,
        CellFencingToken::INITIAL,
        Some(40),
        Some(AuditWallTimestampHint::new(u64::MAX - 7)),
        "tcp://10.0.0.11:9443",
    )
    .expect("valid envelope 1");

    assert_eq!(
        tracker
            .observe_heartbeat(&env1, MonotonicRecvInstantMs::new(1_000))
            .expect("accept hb1"),
        CrossHostHeartbeatDecision::Accepted {
            watermark: wm1,
            received_at_mono: MonotonicRecvInstantMs::new(1_000),
        }
    );

    // Heartbeat 2 arrives at receiver mono = 1_200ms with a regressed sender
    // wall-clock hint (`0`). Receiver liveness MUST still succeed based strictly
    // on receiver-local monotonic arrival time.
    let wm2 = CrossHostHeartbeatWatermark::new(Generation::INITIAL, CellEpoch::INITIAL, nz(2));
    let env2 = CrossHostHeartbeatEnvelope::new(
        peer_a,
        wm2,
        CellFencingToken::INITIAL,
        Some(100), // 100 / 2 == 50 <= max_clock_skew_ms (50) -> exact boundary pass
        Some(AuditWallTimestampHint::new(0)),
        "tcp://10.0.0.11:9443",
    )
    .expect("valid envelope 2");

    tracker
        .observe_heartbeat(&env2, MonotonicRecvInstantMs::new(1_200))
        .expect("accept hb2 at exact 2*skew RTT boundary");

    assert_eq!(
        tracker
            .evaluate_peer(peer_a, MonotonicRecvInstantMs::new(1_600))
            .expect("eval at 1600"),
        Some(HeuristicPeerLiveness::HeuristicAlive { silence_ms: 400 })
    );

    // Replaying identical Heartbeat 2 at receiver mono = 1_650ms MUST be
    // DuplicateIgnored and MUST NOT refresh `last_received_at_mono` (still 1_200ms).
    assert_eq!(
        tracker
            .observe_heartbeat(&env2, MonotonicRecvInstantMs::new(1_650))
            .expect("duplicate hb2"),
        CrossHostHeartbeatDecision::DuplicateIgnored {
            watermark: wm2,
            preserved_received_at_mono: MonotonicRecvInstantMs::new(1_200),
        }
    );

    // Therefore at receiver mono = 1_701ms, silence is 1_701 - 1_200 = 501ms > 500ms!
    assert_eq!(
        tracker
            .evaluate_peer(peer_a, MonotonicRecvInstantMs::new(1_701))
            .expect("eval at 1701"),
        Some(HeuristicPeerLiveness::HeuristicSuspectSilence {
            silence_ms: 501,
            max_silence_ms: 500,
        })
    );
    assert_eq!(
        tracker
            .heuristic_live_peers(MonotonicRecvInstantMs::new(1_701))
            .expect("live peers"),
        Vec::new()
    );

    // Out-of-order replay of wm1 fails closed.
    assert!(matches!(
        tracker.observe_heartbeat(&env1, MonotonicRecvInstantMs::new(1_710)),
        Err(ClockLivenessError::StaleOrReplayedWatermark { .. })
    ));

    // Tampered digest fails closed.
    let wm3 = CrossHostHeartbeatWatermark::new(Generation::INITIAL, CellEpoch::INITIAL, nz(3));
    let tampered = CrossHostHeartbeatEnvelope::new(
        peer_a,
        wm3,
        CellFencingToken::INITIAL,
        Some(20),
        None,
        "tcp://10.0.0.11:9443",
    )
    .expect("env3")
    .with_digest([0xFF; 32]);
    assert!(matches!(
        tracker.observe_heartbeat(&tampered, MonotonicRecvInstantMs::new(1_720)),
        Err(ClockLivenessError::MalformedHeartbeatEnvelope(_))
    ));

    // Receiver monotonic regression fails closed.
    assert!(matches!(
        tracker.evaluate_peer(peer_a, MonotonicRecvInstantMs::new(1_719)),
        Err(ClockLivenessError::LocalMonotonicRegression {
            previous_mono_ms: 1_720,
            observed_mono_ms: 1_719,
        })
    ));
}

#[test]
fn link_clock_guard_and_partition_gate_quarantine_on_odd_rtt_skew_and_monotonic_regression() {
    let local_cell = cell(0xB2);
    let fence = CellFence::present(
        local_cell,
        Generation::INITIAL,
        CellEpoch::INITIAL,
        CellFencingToken::INITIAL,
    );

    // Case 1: Odd RTT = 101ms against max_clock_skew_ms = 50ms -> 101 / 2.0 = 50.5 > 50 -> fail-closed!
    let mut guard = CrossHostLinkClockGuard::new_connected(fence, 1_000, nz(50));
    assert_eq!(
        guard.observe_sample(1_050, Some(100)),
        Ok(LinkClockHealthState::Healthy {
            max_clock_skew_ms: nz(50),
            last_mono_tick: 1_050,
            last_rtt_ms: Some(100),
        })
    );
    let skew_err = guard
        .observe_sample(1_100, Some(101))
        .expect_err("101ms RTT must violate max_clock_skew=50ms");
    assert!(matches!(
        skew_err,
        LinkClockViolation::ClockSkewRttExceeded {
            measured_rtt_ms: 101,
            quarantined_state: ControlPlaneLinkState::Partitioned {
                reason_code: REASON_CLOCK_SKEW_RTT_EXCEEDED,
                partitioned_since_tick: 1_100,
                ..
            },
            ..
        }
    ));
    assert!(guard.gate().state().is_partitioned());
    assert!(
        guard
            .gate()
            .admit_action(
                1_100,
                &fence,
                &PartitionedActionKind::RequestQuotaExpansionOrRenewal,
            )
            .is_err()
    );

    // Case 2: Local monotonic regression forces Connected -> Partitioned immediately.
    let mut reg_guard = CrossHostLinkClockGuard::new_connected(fence, 2_000, nz(50));
    let reg_err = reg_guard
        .observe_sample(1_999, Some(10))
        .expect_err("monotonic regression must quarantine link");
    assert!(matches!(
        reg_err,
        LinkClockViolation::LocalMonotonicRegression {
            previous_tick: 2_000,
            regressed_tick: 1_999,
            quarantined_state: ControlPlaneLinkState::Partitioned {
                reason_code: REASON_LOCAL_MONOTONIC_REGRESSION,
                partitioned_since_tick: 2_000,
                ..
            },
        }
    ));
    assert_eq!(
        reg_guard.health(),
        LinkClockHealthState::MonotonicRegressionQuarantined {
            previous_tick: 2_000,
            regressed_tick: 1_999,
        }
    );
    assert!(reg_guard.gate().state().is_partitioned());
}

#[test]
#[allow(clippy::too_many_lines)]
fn monotonic_checkpoint_trail_orders_without_wall_clock_and_rejects_gaps_and_tampering() {
    let temp = TempDir::new("mchk");
    let directory = CellDirectory::open(temp.path()).expect("open directory");
    let cell_a = cell(0xC3);

    let prefix_1 = CheckpointPrefix::from_entries([b"quota:lease-1=spent:20".as_slice()]);
    let digest_1 = CheckpointDigest::from_prefix(
        Generation::INITIAL,
        CellEpoch::INITIAL,
        CellFencingToken::INITIAL,
        &prefix_1,
    );
    let fact_1 = CheckpointFact::new(
        Generation::INITIAL,
        CellEpoch::INITIAL,
        CellFencingToken::INITIAL,
        &digest_1.to_text(),
    )
    .expect("fact 1");
    assert_eq!(
        fact_1.verify_digest(&prefix_1),
        CheckpointDigestVerification::Verified
    );

    // Deliberately stamp entry 1 with a huge wall hint and entry 2 with a tiny
    // wall hint to prove ordering ignores wall time completely.
    let coord_1 =
        MonotonicCheckpointCoord::new(cell_a, Generation::INITIAL, CellEpoch::INITIAL, nz(1));
    let entry_1 = MonotonicCheckpointEntry::new(
        coord_1,
        fact_1,
        [0u8; 32],
        Some(AuditWallTimestampHint::new(9_999_999_999_999)),
    )
    .expect("entry 1");
    directory
        .record_monotonic_checkpoint(&entry_1)
        .expect("persist entry 1");
    // Idempotent replay succeeds.
    directory
        .record_monotonic_checkpoint(&entry_1)
        .expect("idempotent replay entry 1");

    // Sequence gap (jumping directly to seq 3 in the same epoch) MUST fail closed.
    let fact_gap = CheckpointFact::new(
        Generation::INITIAL,
        CellEpoch::INITIAL,
        CellFencingToken::INITIAL,
        &digest_1.to_text(),
    )
    .expect("fact gap");
    let gap_entry = MonotonicCheckpointEntry::new(
        MonotonicCheckpointCoord::new(cell_a, Generation::INITIAL, CellEpoch::INITIAL, nz(3)),
        fact_gap,
        entry_1.entry_chain_digest(),
        None,
    )
    .expect("gap entry");
    assert!(matches!(
        directory.record_monotonic_checkpoint(&gap_entry),
        Err(ClockLivenessError::CheckpointOrderViolation { .. })
    ));

    // Contiguous seq 2 in Epoch 1, then seq 1 in Epoch 2.
    let fact_2 = CheckpointFact::new(
        Generation::INITIAL,
        CellEpoch::INITIAL,
        CellFencingToken::INITIAL,
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .expect("fact 2");
    let entry_2 = MonotonicCheckpointEntry::new(
        MonotonicCheckpointCoord::new(cell_a, Generation::INITIAL, CellEpoch::INITIAL, nz(2)),
        fact_2,
        entry_1.entry_chain_digest(),
        Some(AuditWallTimestampHint::new(1)),
    )
    .expect("entry 2");
    directory
        .record_monotonic_checkpoint(&entry_2)
        .expect("persist entry 2");

    let epoch_2 = CellEpoch::from_u64(2).expect("epoch 2");
    let token_2 = CellFencingToken::from_u64(2).expect("token 2");
    let fact_3 = CheckpointFact::new(
        Generation::INITIAL,
        epoch_2,
        token_2,
        "sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
    )
    .expect("fact 3");
    let entry_3 = MonotonicCheckpointEntry::new(
        MonotonicCheckpointCoord::new(cell_a, Generation::INITIAL, epoch_2, nz(1)),
        fact_3,
        entry_2.entry_chain_digest(),
        Some(AuditWallTimestampHint::new(0)),
    )
    .expect("entry 3");
    directory
        .record_monotonic_checkpoint(&entry_3)
        .expect("persist entry 3");

    let trail = directory
        .read_monotonic_checkpoint_trail(cell_a)
        .expect("read verified trail");
    let entries = trail.entries(cell_a);
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0], entry_1);
    assert_eq!(entries[1], entry_2);
    assert_eq!(entries[2], entry_3);

    // Tamper with one on-disk `.mchk` file and verify `read_monotonic_checkpoint_trail` fails closed.
    let mchk_dir = temp.path().join("monotonic-checkpoints");
    let first_file = fs::read_dir(&mchk_dir)
        .expect("read mchk dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|s| s.to_str()) == Some("mchk"))
        .expect("at least one mchk file");
    let original = fs::read_to_string(&first_file).expect("read mchk");
    let tampered_text = original.replace("token=1", "token=99");
    fs::write(&first_file, tampered_text).expect("tamper mchk");

    assert!(matches!(
        directory.read_monotonic_checkpoint_trail(cell_a),
        Err(ClockLivenessError::CheckpointChainTampered { .. })
    ));
}
