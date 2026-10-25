# Stage C Evidence — `C-LEASE-001`: Durable Three-Family Lease Store & Cross-Epoch Reconciliation Protocol

- **Evidence ID**: `C-LEASE-001`
- **Task / Attempt**: `W63-CLEASE-RECON` (Attempt 1, Wave `W63`, building on `W62-CLEASE-STORE`)
- **Authority Spec**: [`docs/design/06-架构设计总纲-v0.5.md`](../../design/06-架构设计总纲-v0.5.md) §12 (`§12.1`–`§12.6`)
- **Covered Requirements**:
  - `QuotaLease`: `[LEASE-GRANT-001]`, `[LEASE-SPEND-001]`, `[LEASE-REPORT-001]`, `[LEASE-CLOSE-001]`, `[LEASE-LOSS-001]`, `[LEASE-TTL-001]`, `[LEASE-PREACTIVE-001]`
  - `CapacityLease`: `[LEASE-CAPACITY-001]`, `[LEASE-CAPACITY-002]`
  - `ExclusiveDeviceLease`: `[LEASE-DEVICE-001]`
  - Cross-epoch reconciliation, fencing, migration & WAL-loss guards: `[LEASE-RECON-001]`, `[LEASE-FENCE-001]`, `[LEASE-MIGRATE-001]`, `[LEASE-WAL-001]`
- **Implementation Artifacts**:
  - [`crates/nlos-lease/src/lib.rs`](../../../crates/nlos-lease/src/lib.rs)
  - [`crates/nlos-lease/src/store.rs`](../../../crates/nlos-lease/src/store.rs)
  - [`crates/nlos-lease/src/reconcile.rs`](../../../crates/nlos-lease/src/reconcile.rs)
  - [`crates/nlos-lease/tests/durable_lease_store.rs`](../../../crates/nlos-lease/tests/durable_lease_store.rs)
  - [`crates/nlos-lease/tests/lease_reconciliation.rs`](../../../crates/nlos-lease/tests/lease_reconciliation.rs)

---

## 1. Executive Summary

`C-LEASE-001` closes the durable storage and cross-epoch reconciliation loop for all three `nlos-lease` resource lease families (`QuotaLease`, `CapacityLease`, and `ExclusiveDeviceLease`):

1. **Unified Durable `SQLite` (`WAL + FULL`) Lease Store (`SqliteLeaseStore`)**:
   - Persists pool headers, device head state, lease rows, attach/reset receipts, and reconciliation receipts across 11 schema-v1 tables guarded by 22 immutability and append-only SQLite triggers.
   - Enforces fail-closed rehydration on reopen: verifies schema version, exact table/trigger catalog counts, pool conservation equations, monotonic high-water marks, and cryptographic digest bindings.
2. **Three Typed Non-Interchangeable Reconciliation Receipts (`[LEASE-RECON-001]`, `[LEASE-FENCE-001]`)**:
   - `QuotaReconciliationReceipt` (with `FenceBarrierAck`): reconciles `QUARANTINED` / `FENCED` quota leases, enforcing `face_value == settled_spent + returned_to_available + remaining_quarantined`, monotonic `high_water_spent >= lease.spent`, and idempotent replay by `(lease_id, reconcile_seq)`.
   - `CapacityReconciliationReceipt` (with `FenceBarrierAck` + `CleanupAttestation`): reconciles `FENCED` / `QUARANTINED` / `RETURNING` capacity leases (`[LEASE-CAPACITY-002]`), verifying full `cleaned_amount == amount` and `holder_processes_terminated == true` before transitioning to `RETURNED` and restoring `pool_remaining` once.
   - `DeviceReconciliationReceipt` (with `FenceBarrierAck` + `DeviceResetReceipt`): reconciles `QUARANTINED` / `FENCING` / `RESETTING` exclusive device leases (`[LEASE-DEVICE-001]`), verifying `endpoint_fenced_old_token`, `queue_drained`, `vram_and_secrets_zeroized`, and `reset_generation == head.reset_generation` before returning the `DeviceLeaseHead` to `FREE` and bumping `reset_generation`.
3. **Cross-Node Lease Migration (`[LEASE-MIGRATE-001]`, `[LEASE-CAPACITY-002]`, `[LEASE-DEVICE-001]`)**:
   - `QuotaLeaseGrantor::migrate_lease`: enforces `drain (CLOSING/CLOSED) -> FenceBarrierAck -> close source -> mint new target lease`, explicitly rejecting direct migration of `QUARANTINED` (partitioned) or undrained `ACTIVE` leases.
   - `CapacityLeaseGrantor::migrate_lease`: enforces `fence source -> CapacityReconciliationReceipt (CleanupAttestation + FenceBarrierAck) -> return to pool -> mint target lease`.
   - `ExclusiveDeviceLeaseGrantor::migrate_lease`: enforces `bumped exclusivity_epoch -> fence source -> DeviceReconciliationReceipt -> return head to FREE with bumped reset_generation -> mint target lease`.
4. **Local Lease WAL Loss Quarantine (`[LEASE-WAL-001]`)**:
   - `fence_boot_generation_on_wal_loss` / `handle_node_wal_loss` across all three grantors requires `reconnected_boot_generation > lost_boot_generation` and moves all unsettled leases into `QUARANTINED` without returning funds/capacity/devices prematurely.
   - `QuotaLeaseGrantor::refuse_guessed_remaining_on_wal_loss` unconditionally rejects any attempt to guess `remaining` from `face_value` (`QuotaWalLossError::GuessedRemainingForbidden`).

---

## 2. Durable Schema & Trigger Invariants (`crates/nlos-lease/src/store.rs`)

`SqliteLeaseStore` initializes `PRAGMA journal_mode = WAL`, `PRAGMA synchronous = FULL`, and `PRAGMA foreign_keys = ON`, enforcing 11 tables and 22 triggers:

| Family / Layer | Table | Primary Key | Immutability / Append-Only Triggers |
| :--- | :--- | :--- | :--- |
| Schema Meta | `lease_store_meta` | `singleton_id = 1` | `lease_store_meta_no_update`, `lease_store_meta_no_delete` |
| `QuotaLease` | `quota_pool_state` | `singleton_id = 1` | `quota_pool_state_no_delete`, conservation check `available <= initial_available` |
| `QuotaLease` | `quota_leases` | `lease_id` | `quota_leases_no_delete`, `quota_leases_immutable_guard` (locks identity/epoch/token/face_value; forbids `spent`/`returned` regression; locks `CLOSED`/`CANCELLED`; requires a matching `quota_reconciliation_receipts` row to update a `QUARANTINED` row) |
| `QuotaLease` | `quota_reconciliation_receipts` | `(lease_id, reconcile_seq)` | `quota_reconcile_receipts_no_update`, `quota_reconcile_receipts_no_delete` |
| `CapacityLease` | `capacity_pool_state` | `singleton_id = 1` | `capacity_pool_state_no_delete`, conservation check `pool_remaining <= initial_pool` |
| `CapacityLease` | `capacity_leases` | `capacity_lease_id` | `capacity_leases_no_delete`, `capacity_leases_immutable_guard` (locks identity/epoch/token/amount; locks `RETURNED`; requires a matching `capacity_reconciliation_receipts` row to transition `QUARANTINED -> RETURNED`) |
| `CapacityLease` | `capacity_attach_receipts` | `capacity_lease_id` | `capacity_attach_receipts_no_update`, `capacity_attach_receipts_no_delete` |
| `CapacityLease` | `capacity_reconciliation_receipts` | `capacity_lease_id` | `capacity_reconcile_receipts_no_update`, `capacity_reconcile_receipts_no_delete` |
| `ExclusiveDeviceLease` | `device_heads` | `device_id` | `device_heads_no_delete`, `device_heads_monotonic_gen` (`NEW.reset_generation >= OLD.reset_generation`) |
| `ExclusiveDeviceLease` | `device_leases` | `device_lease_id` | `device_leases_no_delete`, `device_leases_immutable_guard` (locks identity/epoch/token/reset_generation; locks `RETURNED`; requires a matching `device_reconciliation_receipts` row to transition `QUARANTINED -> RETURNED`) |
| `ExclusiveDeviceLease` | `device_reset_receipts` | `receipt_id` | `device_reset_receipts_no_update`, `device_reset_receipts_no_delete` |
| `ExclusiveDeviceLease` | `device_reconciliation_receipts` | `device_lease_id` | `device_reconcile_receipts_no_update`, `device_reconcile_receipts_no_delete` |

*(Note: `lease_store_meta` + 10 domain tables = 11 tables total; 22 triggers total.)*

---

## 3. Three-Family Reconciliation & Migration Protocol (`crates/nlos-lease/src/reconcile.rs`)

### 3.1 `FenceBarrierAck` (`[LEASE-FENCE-001]`)
- Binds `(lease_id, fence_scope, epoch, fencing_token, gateway_id, observed_high_water, ack_digest)` with domain tag `NLOS-FENCE-ACK01`.
- Rejects zero `gateway_id`, tampered `ack_digest`, or any drift in `lease_id`, `fence_scope`, `epoch`, `fencing_token`, or `observed_high_water`.

### 3.2 `QuotaReconciliationReceipt` (`[LEASE-RECON-001]`)
- Enforces `settled_spent == high_water_spent`, `high_water_spent >= lease.spent`, `returned_to_available >= lease.returned`, and `face_value == settled_spent + returned_to_available + remaining_quarantined`.
- Supports multi-step partial reconciliation (`remaining_quarantined > 0` keeps state `Quarantined`; `remaining_quarantined == 0` transitions state to `Closed`) and idempotent replay by `(lease_id, reconcile_seq)`.

### 3.3 `CapacityReconciliationReceipt` & `CleanupAttestation` (`[LEASE-CAPACITY-002]`, `[LEASE-RECON-001]`)
- `CleanupAttestation` binds `(capacity_lease_id, target_node, target_node_boot_generation, capacity_epoch, fencing_token, cleaned_amount, holder_processes_terminated, attestation_digest)` with domain tag `NLOS-CLEAN-ATT01`.
- Requires `cleaned_amount == lease.amount` and `holder_processes_terminated == true` in addition to a verified `FenceBarrierAck` before moving `Fenced` / `Quarantined` / `Returning` -> `Returned` and crediting `pool_remaining += lease.amount`.

### 3.4 `DeviceReconciliationReceipt` (`[LEASE-DEVICE-001]`, `[LEASE-RECON-001]`)
- Binds `DeviceResetReceipt` + `FenceBarrierAck` + explicit hardware reset flags (`endpoint_fenced_old_token`, `queue_drained`, `vram_and_secrets_zeroized`).
- Rejects reconciliation if any reset flag is `false`, if `reset_generation != head.reset_generation`, or if `FenceBarrierAck` / `DeviceResetReceipt` fails verification.
- On commit, transitions `Quarantined` / `Fencing` / `Resetting` -> `Returned`, sets `head.free = true`, and bumps `head.reset_generation` by `+1`.

---

## 4. Verification Commands & Test Results

Executed in `d:\llmos`:

```powershell
cargo fmt -p nlos-lease -- --check
cargo clippy -p nlos-lease --all-targets -- -D warnings
cargo test -p nlos-lease --no-fail-fast
```

### Summary of Test Suites (`22` passed, `0` failed across `12` test binaries)

| Test Binary | Tests | Verified Invariants |
| :--- | :---: | :--- |
| `tests/lease_reconciliation.rs` | 11 | Full & partial `QuotaLease` reconciliation conservation + idempotent replay; barrier digest & high-water regression rejection; `CapacityLease` `FenceBarrierAck` + `CleanupAttestation` pool restoration & fail-closed rejection; `ExclusiveDeviceLease` reset-flag/generation/barrier reconciliation; unified durable SQLite persistence & reopen for all 3 reconciliation receipt families; cross-node migration for all 3 families (including partition migration refusal & epoch bump enforcement); WAL-loss boot generation fencing & refusal of guessed remaining balances |
| `tests/durable_lease_store.rs` | 1 (5 sub-scenarios) | `SQLite WAL + FULL` durability, reopen recovery, SQLite immutability triggers, and corruption/tamper fail-closed rejection across all 3 lease families |
| `tests/quota_grant.rs` | 1 | `[LEASE-GRANT-001]` prepaid deduction & authority fence admit |
| `tests/quota_ledger.rs` | 1 | `[LEASE-SPEND-001]`, `[LEASE-CLOSE-001]`, `[LEASE-PREACTIVE-001]` |
| `tests/quota_loss.rs` | 1 | `[LEASE-LOSS-001]` epoch advance quarantines unsettled leases without refund |
| `tests/quota_ttl.rs` | 1 | `[LEASE-TTL-001]` logical deadline sweep into `FENCED` and `settle_fenced` |
| `tests/capacity_grant.rs` | 1 | `[LEASE-CAPACITY-001]` pool deduction & fence admit |
| `tests/capacity_prepare.rs` | 1 | `GLOBAL_RESERVED -> TARGET_PREPARED -> RETURNING` |
| `tests/capacity_activate.rs` | 1 | `HostAttachReceipt` (`TARGET_PREPARED -> ACTIVE`) |
| `tests/capacity_return.rs` | 1 | `[LEASE-PREACTIVE-001]` single refund on `ack_return` |
| `tests/device_grant.rs` | 1 | `[LEASE-DEVICE-001]` `FREE -> DEVICE_RESERVED` head claim |
| `tests/device_reset.rs` | 1 | `[LEASE-DEVICE-001]` `RESETTING -> RETURNED` via `DeviceResetReceipt` |

---

## 5. Honest Boundaries & Remaining Stage C Scope

- **Implemented & Verified (`C-LEASE-001`)**:
  - Single-Cell in-memory and durable `SQLite` (`WAL + FULL`) three-family lease grantors, append-only receipt logs, immutability triggers, cross-epoch reconciliation receipts (`QuotaReconciliationReceipt`, `CapacityReconciliationReceipt`, `DeviceReconciliationReceipt`), `FenceBarrierAck`, `CleanupAttestation`, cross-node migration helpers, and local lease WAL-loss quarantine guards.
- **Not Claimed Here (Deferred to Downstream Stage C Slices)**:
  - Multi-node network transport / Raft consensus replication of `SqliteLeaseStore` across remote Cell authority replicas.
  - `C-LEASE-002`: `200`-node fault injection benchmark (`QuotaLease` / `CapacityLease` / `ExclusiveDeviceLease` under partition, crash, clock drift, and delayed ACK with `over_allocation_events == 0`).
