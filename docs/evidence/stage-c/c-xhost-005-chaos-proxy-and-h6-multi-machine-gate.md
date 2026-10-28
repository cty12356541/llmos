# Evidence C-XHOST-005: Cross-Host Chaos Proxy, H6 Multi-Machine Gate & Scale Benchmark

- **Evidence ID**: `C-XHOST-005`
- **Stage / Wave / Lane**: Stage C / Wave W72 / Lane 4 (`W72-C-XHOST-GATE`, `C-XHOST-GATE`)
- **Date**: 2026-10-29
- **Status**: `VERIFIED` (`TESTED` across `nlos-net`, `nlos-slice-k`, `nlos-runtime-tokio`, and `scripts/xhost_cluster_harness.py`)
- **Requirements Covered**: `[ROAD-C-001]`, `[ROAD-C-002]`, `[CONS-SCOPE-001]`, `[CLOCK-BOUND-001]`, `[REMOTE-FAIL-001]`, `[DIST-FAIL-001]`, `[LEASE-LOSS-001]`, `[DIST-MIGRATE-001]`, `[DIST-MIGRATE-002]`, `[TCB-ENDPOINT-001]`

---

## 1. Scope & Write-Set Summary

| Artifact Path | Role | Verification Status |
|---|---|---|
| `crates/nlos-net/src/chaos_proxy.rs` (`lib.rs`) | Deterministic L4/L7 cross-host TCP chaos proxy, asymmetric 3-node triangle matrix, clock-skew guard, ciphertext corruption & mid-stream RST fault injectors | `7 passed` unit + `8 passed` (`poc_xhost_001`) |
| `crates/nlos-slice-k/tests/c_xhost_h6_multi_machine_gate.rs` | `[ROAD-C-001]` H6 multi-machine / multi-OS-process chaos gate (5 end-to-end tests: 4 default + 1 `--ignored` child-worker gate) | `4 passed` + `1 passed (--ignored)` |
| `scripts/xhost_cluster_harness.py` | 3-node multi-process cross-host topology & clock-bound invariant validator (`--self-test` / `--emit-topology`) | `VALIDATED` (exit `0`) |
| `crates/nlos-runtime-tokio/tests/c_xhost_bench_scale.rs` | `[ROAD-C-002]` Cross-host data-plane `0` Raft RPC isolation benchmark (1/2/4 Cell shards) & 5-fault matrix under concurrent load | `2 passed` |

---

## 2. `[ROAD-C-001]` Cross-Host Chaos Proxy & H6 Multi-Machine Gate (`c_xhost_h6_multi_machine_gate`)

### 2.1 Fault Primitives & Invariants Verified (`crates/nlos-net/src/chaos_proxy.rs`)
1. **Asymmetric Triangle Partition (`TriangleChaosMatrix` + `LinkDirection`)**:
   - Independently controls directional links (`A -> B` severed while `A -> C` and `C -> B` remain healthy).
   - Verifies `[CONS-SCOPE-001]` & `[REMOTE-FAIL-001]`: Raft control-plane quorum (`{A, C}`) elects a new term and fences old leader `B` even when `A <-> B` is asymmetrically partitioned.
2. **Bounded Clock-Skew Guard (`ChaosClockSkewController`, `[CLOCK-BOUND-001]`, `[LEASE-LOSS-001]`)**:
   - Enforces `ttl_grant > 2 * epsilon_max` and `ttl_holder <= ttl_grant - 2 * epsilon_max`.
   - When injected skew $|\Delta_{\text{clock}}| = 650\text{ ms} > \varepsilon_{\max} = 500\text{ ms}$, `RemoteLeaseClockGuard::validate_remote_lease_grant` fails closed (`ClockSkewBoundBreached`), quarantining remote authority leases and demoting the skewed node.
3. **8-Step Cross-Host Migration Under RTT Jitter & Pinned Rejection (`[DIST-MIGRATE-001]`, `[DIST-MIGRATE-002]`)**:
   - Executes the 8-step migration protocol over live TLS 1.3 (`rustls`, X25519 + Ed25519 mTLS + `0x4E4C4F53` wire framing) routed through `ChaosTcpProxy` with deterministic Xorshift64 RTT jitter (`base_rtt_ms + [0, jitter_ms]`).
   - Rejects `NonMigratablePinned` (`HostLocalDevice`, `HostBoundSecret`, `ProcessBoundHandle`) fail-closed before network transfer and fences stale source `cell_epoch` (`CellEpochFenced`).
4. **Mid-Stream TCP RST, Post-Handshake Ciphertext Corruption & Bounded-Loss Reconciliation (`[DIST-FAIL-001]`, `[TCB-ENDPOINT-001]`)**:
   - `OneShotPayloadResetRule`: injects mid-stream TCP close/RST after $N$ application C2S bytes; triggers `TransportState::Disconnected -> Reconnecting -> Active` with `advance_cell_epoch()`.
   - `CiphertextCorruptionRule`: flips a deterministic bit in post-handshake TLS application records; `rustls` AEAD verification rejects the tampered record immediately (`0` forged frames reach the endpoint handler).
   - Verifies `LossBounded { max_unflushed_entries }` window reconciliation (`UnflushedLossBoundExceeded` vs clean tail replay).
5. **Multi-Process Child Worker Chaos Gate (`road_c001_xhost_multi_process_child_worker_chaos_gate`)**:
   - Spawns a separate OS child process (`--exact road_c001_xhost_multi_process_child_worker_chaos_gate --nocapture`) hosting the target `CrossHostCellEndpoint` + `CellAuthority` (enforcing `ADR-0018` `[CELL-PROC-001]` single-Cell-per-process isolation), bridged through `ChaosTcpProxy`.

### 2.2 Gate Execution Output
```text
$ cargo test -p nlos-net --no-fail-fast
running 7 tests (unit: chaos_proxy::tests) ... ok. 7 passed; 0 failed
running 8 tests (tests/poc_xhost_001.rs) ... ok. 8 passed; 0 failed

$ cargo test -p nlos-slice-k --test c_xhost_h6_multi_machine_gate --no-fail-fast
running 5 tests
test road_c001_xhost_multi_process_child_worker_chaos_gate ... ignored
test road_c001_xhost_asymmetric_triangle_partition_and_raft_quorum_fencing ... ok
test road_c001_xhost_clock_skew_bound_breach_and_remote_lease_quarantine ... ok
test road_c001_xhost_eight_step_migration_under_rtt_jitter_and_pinned_rejection ... ok
test road_c001_xhost_mid_stream_rst_ciphertext_corrupt_and_bounded_loss_reconciliation ... ok
test result: ok. 4 passed; 0 failed; 1 ignored; finished in 6.69s

$ cargo test -p nlos-slice-k --test c_xhost_h6_multi_machine_gate -- --ignored --test-threads=1
running 1 test
test road_c001_xhost_multi_process_child_worker_chaos_gate ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; finished in 3.00s
```

### 2.3 Cluster Harness Validation (`scripts/xhost_cluster_harness.py`)
```text
$ python scripts/xhost_cluster_harness.py --self-test
{
  "ClockBoundValidation": {
    "epsilon_max_ms": 500,
    "grantor_wait_floor_ms": 3500,
    "invariants_satisfied": true,
    "network_grace_ms": 500,
    "ttl_grant_ms": 3000,
    "ttl_holder_ms": 1750
  },
  "directed_links": 6,
  "nodes": ["cell-a", "cell-b", "cell-c"],
  "schema": "nlos.xhost.cluster_harness.v1",
  "status": "VALIDATED",
  "topology_sha256": "a22b65dda9e251be935d1c2577f116dfb99bd3fb6cf1f262fc7569ca5a9b7c4d"
}
```

---

## 3. `[ROAD-C-002]` Multi-Shard Scale & 5-Fault Matrix Benchmark (`c_xhost_bench_scale`)

### 3.1 Data-Plane `0` Raft RPC Isolation & Multi-Shard Scale (`[CONS-SCOPE-001]`)
Each Cell shard operates an independent SQLite `WAL + FULL` (`SqliteTaskAuthority` with `TASK_PROFILE_100K`). Critical-path task commits (`register_task -> register_attempt -> request_commit_permit_with_authority_lease -> finalize_commit_with_authority_lease`) execute with **`0` Raft RPCs**:

| Active Cell Shards | Total Task Commits | Critical-Path Raft RPCs | Aggregate Throughput (`commits/s`) | Latency `p50` (`µs`) | Latency `p95` (`µs`) | Latency `p99` (`µs`) |
|---:|---:|---:|---:|---:|---:|---:|
| `1` | `16` | `0` | `211.2 - 325.1` | `2,634 - 3,897` | `3,182 - 5,228` | `3,182 - 5,228` |
| `2` | `32` | `0` | `465.6 - 645.1` | `2,513 - 3,316` | `2,946 - 4,135` | `3,043 - 4,286` |
| `4` | `64` | `0` | `843.4 - 1,039.6` | `2,895 - 3,415` | `3,382 - 4,089` | `3,460 - 4,172` |

### 3.2 Five-Fault Matrix Under Concurrent Load (`road_c002_xhost_five_fault_matrix_under_concurrent_load`)
| Fault Scenario | Injected Condition | Observed Outcome |
|---|---|---|
| **Fault 1: Slow I/O Shard Isolation** | `25 ms` per-commit I/O delay on `Shard 0` while `Shards 1..3` run concurrently | Healthy shards complete with `p95 < 15 ms`; `0` cross-shard lock contention |
| **Fault 2: Lease Expiry & Stale Primary Fencing** | Term-1 lease (`ttl = 200 ms`) expires; premature takeover at `150 ms` & stale term-1 permit at `265 ms` | Premature takeover -> `AuthorityLeaseHeld`; stale term-1 -> `AuthorityLeaseFenced`; term-2 commits cleanly (`0` split-brain commits) |
| **Fault 3: Fanout Storm & Group Cancel** | `32` concurrent fanout requests against `cap=4, queue=4` (`WorkloadBulkheadCoordinator`) + `CancelMode::FailFast` | `24` excess requests shed deterministically (`RejectedQueueFull`); `SafetyTcbSystemReserve` retains `4/4` slots (`0` TCB encroachment); `GroupState::Cancelled` |
| **Fault 4: Outbox Idempotent Replay** | Duplicate `finalize_commit_v3` with identical `IdempotencyKey` + `OutboxBatchSpec` after simulated crash | Returns `FinalizeDecision::IdempotentReplay` with identical `commit_seq`; outbox entry count remains `1` (`0` duplicate side effects) |
| **Fault 5: Cell Saturation & Cross-Cell Offload** | `Cell 0` queue depth driven to `DomainPressureLevel::Saturated` (`32/32`) in `HierarchicalScheduler` | Admission offloads work to healthy `Cell 1` (`assigned_domain == Cell 1`) without blocking `Cell 0` |
