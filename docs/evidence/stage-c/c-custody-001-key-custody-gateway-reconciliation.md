# Stage C Evidence — `C-CUSTODY-001`: Signing Key Custody Vault & Enforcement-Gateway Reconciliation Authority

- **Evidence ID**: `C-CUSTODY-001`
- **Task / Attempt**: `W64-CCUSTODY` (Attempt 1, Wave `W64`)
- **Handover Item**: Stage B → Stage C `#17 C-CUSTODY-GATEWAY` (`生产 signing key custody / enforcement-gateway reconciliation authority`)
- **Authority Specs & ADRs**:
  - [`docs/design/06-架构设计总纲-v0.5.md`](../../design/06-架构设计总纲-v0.5.md) §10.2 (`[BUD-CLOSE-001]`, `[BUD-UNCERTAIN-001]`, `[BUD-REBATE-001]`, `[BUD-REBATE-002]`, `[BUD-LATE-001]`)
  - [`docs/management/adrs/0017-resource-operation-cross-authority-prepare-finalize.md`](../../management/adrs/0017-resource-operation-cross-authority-prepare-finalize.md) (Constraint 4 & Review Trigger 2: `R-C` → `R-B` Bounded Settlement)
  - [`docs/management/adrs/0022-operator-root-capability-issuance.md`](../../management/adrs/0022-operator-root-capability-issuance.md) (Operator root capability signing custody)
- **Implementation Artifacts**:
  - [`crates/nlos-capability/src/custody.rs`](../../../crates/nlos-capability/src/custody.rs)
  - [`crates/nlos-capability/src/lib.rs`](../../../crates/nlos-capability/src/lib.rs)
  - [`crates/nlos-commit-coordinator/src/gateway_reconcile.rs`](../../../crates/nlos-commit-coordinator/src/gateway_reconcile.rs)
  - [`crates/nlos-commit-coordinator/src/lib.rs`](../../../crates/nlos-commit-coordinator/src/lib.rs)
  - [`crates/nlos-commit-coordinator/tests/custody_gateway_reconciliation.rs`](../../../crates/nlos-commit-coordinator/tests/custody_gateway_reconciliation.rs)

---

## 1. Executive Summary

`C-CUSTODY-001` closes Stage B → Stage C handover `#17 C-CUSTODY-GATEWAY` by introducing a non-exportable KEK-sealed signing key custody vault in `nlos-capability` and an authenticated enforcement-gateway reconciliation authority + bounded `R-B` settlement coordinator in `nlos-commit-coordinator`:

1. **Non-Exportable Signing Key Custody Vault (`SigningKeyCustodyVault`, `SqliteCustodyStore`)**:
   - Persists key slots, monotonic signing audit logs, rotation receipts, and compromise tombstones in `<root>/signing-key-custody.db` (`PRAGMA journal_mode = WAL`, `PRAGMA synchronous = FULL`, `STRICT` tables, `user_version = 1`), preserving `CapabilityAuthority`'s `capability-authority.db` (`SCHEMA_VERSION = 2`) untouched.
   - Seals 32-byte `Ed25519` seeds under a caller-supplied 32-byte `wrapping_kek` (verified on reopen via a salted `SHA-256` KEK commitment and per-slot stream authentication tag) and zeroizes ephemeral seed buffers after each operation; private signing material never leaves the vault via public types or APIs.
   - Emits an immutable `CustodySignAuditReceipt` with a strictly monotonic `sign_seq` (`1, 2, ...`) for every signature, including ADR-0022 root capability issuance (`issue_root_capability_under_custody`).
   - Enforces explicit key-epoch lifecycle states (`Active`, `RotatedVerifyOnly`, `CompromisedRevoked`, `Expired`): `rotate_key_epoch` / `rotate_key_epoch_with_identity` transitions `Active(E) -> RotatedVerifyOnly(E)` + `Active(E + 1)` atomically so rotated keys refuse new signing and live active-epoch verification while preserving historical verification; `revoke_compromised_key` / `revoke_compromised_key_with_identity` writes an immutable `CustodyRevocationTombstone` with `revoked_from_ms` that bars new signing and flags post-compromise signatures fail-closed.
2. **Enforcement-Gateway Reconciliation & Bounded `R-B` Settlement (`EnforcementGatewayReconciliationAuthority`, `verify_and_apply_gateway_reconciliation`)**:
   - Signs canonical domain-separated `GatewayReconciliationAttestation` payloads (`EffectClosed { final_seq, final_usage }`, `ConfirmedNoEffect`, `StillUncertainQuarantine` × `Provisional` / `FinalNonSupersedeable`) exclusively through `SigningKeyCustodyVault::sign_digest_under_custody`.
   - Enforces dual verification against both `SigningKeyCustodyVault::verify_active_epoch_signature` and `IdentityAuthority::verify_semantic_authority_signature` before any ledger or owner state mutation.
   - Implements `[BUD-REBATE-001]` / `[BUD-REBATE-002]`: `Provisional` attestations record `ProjectedReconciliationAudit` (`projected_seq`, `projected_usage`, `projected_rebate`) only, never moving credit, unfreezing `Quarantined` reservations, or converging Task commit plans.
   - Implements `[BUD-UNCERTAIN-001]`: `StillUncertainQuarantine` transitions `Active -> Quarantined` (or preserves `Quarantined`) and keeps credit + Task plans frozen; invalid, expired, rotated-verify-only, compromised, wrong-gateway, or stale-`fencing_token` attestations fail closed with zero fund movement.
   - Implements ADR-0017 `R-B` Bounded Settlement (`[BUD-CLOSE-001]`, `[BUD-LATE-001]`): `FinalNonSupersedeable` + `EffectClosed` / `ConfirmedNoEffect` validates monotonic `(final_seq, final_usage)` against high-water and `upper_bound`, settles `ResourceAuthority::finalize_reservation` with `effect_closed_proof_digest` bound to the verified gateway signature, converges `SqliteTaskAuthority::converge_resource_commit_plan`, replays exact retries idempotently, and permanently locks the reservation against post-final supersession.

---

## 2. Durable Custody Schema & Trigger Invariants (`crates/nlos-capability/src/custody.rs`)

`SqliteCustodyStore::open` initializes `<root>/signing-key-custody.db` with `WAL + FULL` and `foreign_keys = ON` over 5 `STRICT` tables and 10 triggers:

| Table | Primary Key | Protected State & `SQLite` Triggers |
| :--- | :--- | :--- |
| `custody_vault_meta` | `singleton_id = 1` | Stores `vault_salt`, `kek_commitment`, and `last_sign_seq`; guarded by `custody_vault_meta_monotonic_seq` (forbids KEK commitment mutation or `last_sign_seq` regression) and `custody_vault_meta_no_delete` |
| `custody_key_slots` | `(key_id, rotation_epoch)` | Stores public slot binding, `CustodyTier`, `CustodyKeyState`, validity/rotation/revocation timestamps, and `(wrap_nonce, wrapped_seed, wrap_auth_tag)`; guarded by `custody_key_slots_no_delete` and `custody_key_slots_compromised_terminal` (`CompromisedRevoked` is irreversible) |
| `custody_sign_audit_log` | `sign_seq` | Stores immutable `CustodySignAuditReceipt` rows; guarded by `custody_sign_audit_log_no_update` and `custody_sign_audit_log_no_delete` |
| `custody_rotation_receipts` | `receipt_id` | Links `(previous_key_id, previous_epoch)` to `(active_key_id, previous_epoch + 1)`; guarded by `custody_rotation_receipts_no_update` and `custody_rotation_receipts_no_delete` |
| `custody_revocation_tombstones` | `tombstone_id` | Records `revoked_from_ms`, `reason_digest`, and `recorded_at_ms` per `(key_id, rotation_epoch)`; guarded by `custody_revocation_tombstones_no_update` and `custody_revocation_tombstones_no_delete` |

---

## 3. Verification Commands & Test Matrix

Executed in `d:\llmos` on base commit `3a91098a7557a987128b29a4eaf8490d1277a65b`:

```powershell
cargo fmt -p nlos-capability -p nlos-commit-coordinator -- --check
cargo clippy -p nlos-capability -p nlos-commit-coordinator --no-deps --all-targets -- -D warnings
cargo test -p nlos-capability -p nlos-commit-coordinator --no-fail-fast
```

- `cargo fmt -p nlos-capability -p nlos-commit-coordinator -- --check` → **exit code `0`**
- `cargo clippy -p nlos-capability -p nlos-commit-coordinator --no-deps --all-targets -- -D warnings` → **exit code `0`** (zero warnings)
- `cargo test -p nlos-capability -p nlos-commit-coordinator --no-fail-fast` → **exit code `0`** (**`110` passed, `0` failed** across `19` test binaries: `35` in `nlos-capability` + `75` in `nlos-commit-coordinator`)

### New `C-CUSTODY-001` Tests (`12` new tests: `2` unit + `10` integration)

| Suite / Test | Verified Invariants |
| :--- | :--- |
| `nlos_capability::custody::tests::rfc8032_engine_matches_ed25519_dalek_byte_for_byte` | Pure safe-Rust RFC 8032 `Ed25519` key derivation, signing, and strict verification match `ed25519_dalek::SigningKey` byte-for-byte across multiple seeds and message lengths, rejecting tampered signatures |
| `nlos_capability::custody::tests::custody_vault_issues_adr0022_root_capability_without_exporting_private_key` | ADR-0022 operator & holder principal bootstrap under custody + signed root capability issuance via `CapabilityAuthority::issue_root_signed` with `sign_seq = 1` |
| `custody_vault_durable_sqlite_wal_full_reopen_kek_commitment_and_schema_isolation` | `SQLite WAL + FULL` persistence across close/reopen, ciphertext seed wrapping (`wrapped_seed != raw_seed`), wrong-KEK rejection (`WrappingKeyMismatch`), and `CapabilityAuthority` `user_version == 2` isolation |
| `custody_signing_emits_strictly_monotonic_audit_seq_and_adr0022_root_capability` | Strictly monotonic `sign_seq = 1..=4` across ADR-0022 root issuance, gateway signing, and vault reopen; `SQLite` trigger aborts `DELETE` on `custody_sign_audit_log` |
| `custody_key_epoch_rotation_bars_old_signing_and_preserves_historical_verification` | `Active(1) -> RotatedVerifyOnly(1)` + `Active(2)` synchronized with `IdentityAuthority`: epoch `1` refuses new signing and live active verification, verifies pre-rotation historical receipts, and rejects post-rotation backdated timestamps |
| `custody_compromised_revocation_tombstone_refuses_signing_and_flags_post_compromise_proofs` | `CompromisedRevoked` tombstone with `revoked_from_ms` refuses new signing and live verification, flags `signed_at_ms >= revoked_from_ms` fail-closed, and distinguishes pre-compromise historical audit records |
| `provisional_gateway_attestation_projects_rebate_without_moving_funds_or_unfreezing` | `[BUD-REBATE-001]` / `[BUD-REBATE-002]`: `Provisional` `v1 -> v2` supersession updates `ProjectedReconciliationAudit` only; `available_credit`, `Quarantined` state, and Task `ResourceCommitPlan` (`NotDue`) remain untouched |
| `final_effect_closed_attestation_settles_quarantined_reservation_converges_task_and_replays_idempotently` | ADR-0017 `R-B` Bounded Settlement (`[BUD-CLOSE-001]`): `FinalNonSupersedeable` `EffectClosed` settles `Quarantined -> Finalized` with exact refund, converges stuck Task `ResourceCommitPlan -> Finalized`, and replays idempotently without double refund |
| `final_confirmed_no_effect_settles_zero_usage_and_rejects_nonzero_high_water` | `FinalNonSupersedeable` `ConfirmedNoEffect` refunds 100% of zero-usage `Quarantined` hold + converges Task plan, while failing closed (`BelowHighWaterMark`) whenever `usage_high_water > 0` |
| `supersession_conflicts_stale_versions_post_final_and_below_high_water_fail_closed` | Rejects below-high-water usage, above-`upper_bound` usage, conflicting same-version attestations, stale lower-version attestations, and any supersession after `FinalNonSupersedeable` |
| `still_uncertain_quarantine_freezes_active_reservation_and_keeps_quarantined_funds_frozen` | `[BUD-UNCERTAIN-001]`: rejects `(StillUncertainQuarantine, FinalNonSupersedeable)`, transitions `Active -> Quarantined` on `Provisional` uncertain attestation, and keeps credit + Task plan frozen |
| `compromised_expired_rotated_wrong_gateway_and_stale_fence_attestations_fail_closed` | Wrong `gateway_id`, stale `fencing_token`, expired key, `RotatedVerifyOnly` key epoch, and `CompromisedRevoked` key all fail closed and keep `Quarantined` funds frozen |

---

## 4. Honest Boundaries & Remaining Stage C Scope

- **Implemented & Verified (`C-CUSTODY-001`)**:
  - Local `SQLite WAL + FULL` KEK-wrapped signing key custody vault (`OperatorFileSealed`, `PlatformKeychainSealed`, `HardwareHsmAttested` slot tiers), monotonic `CustodySignAuditReceipt` ledger, ADR-0022 custody-backed root capability issuance, key-epoch rotation (`Active -> RotatedVerifyOnly`), compromise tombstones (`CompromisedRevoked`), signed `GatewayReconciliationAttestation` protocol, supersession ledger (`GatewayReconciliationLedger`), and bounded `R-B` settlement across `ResourceAuthority` and `SqliteTaskAuthority`.
- **Not Claimed Here (External Hardware / Transport Boundaries)**:
  - Vendor-specific PKCS#11 / CloudHSM / TPM C-ABI FFI drivers and OS-native keychain syscalls (`DPAPI` / macOS Security.framework / Linux Secret Service): `SigningKeyCustodyVault` enforces the KEK-sealed non-exportable custody boundary, tier metadata, and audit/lifecycle state machine in pure safe Rust (`#![forbid(unsafe_code)]`), ready to wrap platform/HSM KEK unsealing providers at deployment wiring boundaries.
  - Multi-node RPC transport delivery of `GatewayReconciliationAttestation` frames from remote driver gateway processes and durable coordinator-side persistence of `GatewayReconciliationLedger` across coordinator process restarts (owner `ResourceAuthority` `FinalizationReceipt` / `QuarantineReceipt` and `SqliteTaskAuthority` `ResourceCommitPlanRecord` already persist durably in `SQLite WAL + FULL`).
