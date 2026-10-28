# Stage D Evidence: `D-NOTIFY-SEARCH-001` — Notification Center, Cryptographic One-Shot `ActionToken`, `Ctrl+K` Spotlight `0x1F` Read-Only Lock & `[DUI-OFFLINE-001]` Local Offline Autonomy

- **Evidence ID**: `D-NOTIFY-SEARCH-001`
- **Workpackage**: `D-NOTIFY-SEARCH-OFFLINE` (`W72-D-NOTIFY-SEARCH`, Wave `W72` Lane 2)
- **Assurance Level**: `H4` (Host-Side Rust Integration Suite + Frontend Deterministic Verification Suite + Static `ANTI-AI-01..07` & `L0..L3` Zero-Jargon Audit)
- **Status**: `VERIFIED`
- **Date**: `2026-10-28`
- **Authoritative Spec Alignment**:
  - `docs/design/06-架构设计总纲-v0.5.md` §16.3.1, §16.3.4 (`IPC-D4` `QueryIndex` / `InspectSearchScope`, `IPC-D7` `ListCells` / `InspectCellLeases`, `IPC-D8` `ListNotifications` / `InspectNotification`), §26.1–§26.4, §27.1–§27.3
  - `docs/management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md` Decision 1 (`Z3 = 400` flyout & command/search surfaces), Decision 2 (`L0..L3` progressive disclosure & action pruning), Decision 3 (Nomad `72-96px × 6px` solid triple-watermark micro-bar, zero `backdrop-filter`), Decision 4 (Statutory Lock #1 `0x1F` read-only search & Statutory Lock #3 `QUARANTINED_LEASE_FENCE`)
  - `docs/superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md` §0.1 (`ANTI-AI-01..07`), §1.1–§1.3, §2A.1–§2A.4, §3.5, §3.6, §4.1–§4.3

---

## 1. Scope & Deliverables Summary

This evidence package verifies the end-to-end implementation of Workpackage **`D-NOTIFY-SEARCH-OFFLINE` (`W72-D-NOTIFY-SEARCH`)** across the Tauri Rust host (`desktop/src-tauri`) and the TypeScript/CSS desktop workstation shell (`desktop/src`):

1. **Notification Center (`Z3 = 400`) & Cryptographic One-Shot `ActionToken` (`IPC-D8`)**:
   - Implemented in [`desktop/src-tauri/src/notify_search_offline.rs`](../../../desktop/src-tauri/src/notify_search_offline.rs) and [`desktop/src/views/notifications/`](../../../desktop/src/views/notifications/index.ts).
   - Covers all 16 canonical task states (`PROPOSED..CANCELLED` / `DRAFT..ROLLED_BACK`) with descending cursor pagination (`limit <= 64`), unread filtering, category filtering, `inspect_notification_item`, and `dismiss_notification_item` (advancing `ack_watermark_sequence` and synchronizing the `WorkstationCompositor` unread badge).
   - Binds each inline `ActionToken` cryptographically (`SHA-256` domain-separated commitment `LLMOS_STAGE_D_NOTIFY_ACTION_TOKEN_COMMIT_V1`) to `(NotificationId, ApplicationId, TaskId, PrincipalId, action_id, expiry_mono_ms)`.
   - Enforces atomic one-shot redemption (`redeem_notification_action_token`): once redeemed, `redeem_action_token` is immediately pruned from `allowed_verbs`, and any subsequent replay, expired token, cross-application call, cross-principal call, task mismatch, or forged commitment fails closed with deterministic error codes (`ACTION_TOKEN_ALREADY_CONSUMED`, `ACTION_TOKEN_EXPIRED`, `ACTION_TOKEN_APP_MISMATCH`, `ACTION_TOKEN_PRINCIPAL_MISMATCH`, `ACTION_TOKEN_TASK_MISMATCH`, `ACTION_TOKEN_INVALID_PROOF`).

2. **`Ctrl+K` Global Spotlight Search & Statutory Lock #1 (`0x1F` Read-Only Non-Authoritative Guarantee, `IPC-D4` + `IPC-D8`)**:
   - Implemented in [`desktop/src-tauri/src/notify_search_offline.rs`](../../../desktop/src-tauri/src/notify_search_offline.rs) and [`desktop/src/views/spotlight/`](../../../desktop/src/views/spotlight/index.ts).
   - Queries local and federated multi-Cell `SemanticVectorCheckpoint` snapshots across `TASK`, `ARTIFACT`, `APP`, `SETTING`, and `NOTIFICATION` categories.
   - Enforces `authority_grade = "NON_AUTHORITATIVE"` and `restrictions_bitmask = 0x1F` (`31`) on both the response envelope and every candidate item, restricting candidate verbs strictly to `["inspect", "jump_to_authority"]`.
   - Enforces Statutory Lock #1 (`attempt_mutation_from_search_hit`): all 5 forbidden control-plane mutation gates (`CAPABILITY_GRANT` `0x01`, `RESOURCE_LEDGER_DEDUCTION` `0x02`, `TASK_COMMIT` `0x04`, `APPLICATION_UPDATE` `0x08`, `IRREVERSIBLE_ACTION` `0x10`) unconditionally fail closed with `SEARCH_RESULT_NON_AUTHORITATIVE_0X1F`, even when a caller supplies a tampered `caller_supplied_mask = 0x00`.
   - Enforces authoritative jump re-verification (`verify_spotlight_candidate_authority`): clicking a search candidate re-verifies the target against its authoritative store before opening the target view or unlocking authoritative verbs, failing closed with `STALE_GENERATION` if the authoritative generation advanced or `AUTHORITY_RECORD_RETRACTED` if the record was retracted.

3. **Offline Autonomy (`[DUI-OFFLINE-001]`) & Cluster Posture Tray (`IPC-D7`)**:
   - Implemented in [`desktop/src-tauri/src/notify_search_offline.rs`](../../../desktop/src-tauri/src/notify_search_offline.rs) and [`desktop/src/views/offline-cluster/`](../../../desktop/src/views/offline-cluster/index.ts).
   - Supports all 4 `PartitionAutonomyGate` connectivity modes (`CONNECTED`, `PARTITIONED`, `RECONNECTING`, `LOCAL_SINGLE_NODE`) with Nomad `72-96px × 6px` solid triple-watermark lease micro-bars.
   - Verifies `[DUI-OFFLINE-001]`: in both `PARTITIONED` and `LOCAL_SINGLE_NODE` modes, all 6 core local workstation surfaces (`Launcher`, `LocalArtifacts`, `TaskHistoryAndLocalTasks`, `Settings`, `LocalSearch`, `NotificationHistory`) remain 100% interactive (`interactive = true`, `AVAILABLE_LOCAL_AUTONOMY`) with a signed `LocalAutonomyProofBadge`.
   - Remote-dependent actions (`requires_remote_provider = true` or targeting an unreachable remote worker node) deterministically degrade to `WAITING_NETWORK` (in `PARTITIONED`) or `UNAVAILABLE_REMOTE_OFFLINE` (in `LOCAL_SINGLE_NODE`) with zero-jargon `L1/L2` copy and `L3` epoch/cell diagnostics.
   - Enforces Statutory Lock #3 (`QUARANTINED_LEASE_FENCE`): attempting to unlock a `QUARANTINED` node lease solely by local TTL expiration (`attempt_unlock_quarantined_lease_by_ttl`) fails closed with `QUARANTINED_LEASE_FENCE_LOCKED`, and action dispatch to a quarantined node is blocked (`BLOCKED_QUARANTINED_FENCE`).

---

## 2. Requirement & Invariant Verification Matrix

| Requirement / Invariant | Implementation Artifact | Verification Test | Result |
| :--- | :--- | :--- | :--- |
| **16-State Notification Feed & Cursor Pagination (`IPC-D8`)** | `desktop/src-tauri/src/notify_search_offline.rs`<br>`desktop/src/views/notifications/notification-center.ts` | `test_notification_feed_covers_all_16_canonical_task_states_and_cursor_pagination`<br>`testNotification16StatesAndPagination` | **PASS** |
| **One-Shot `ActionToken` Redemption & Replay/Forgery Rejection** | `desktop/src-tauri/src/notify_search_offline.rs`<br>`desktop/src/views/notifications/notification-center.ts` | `test_action_token_one_shot_redemption_and_replay_rejection`<br>`test_action_token_rejects_expired_cross_app_cross_principal_and_forged_commitment`<br>`testActionTokenRedemptionAndNegativeSecurityGates` | **PASS** |
| **Statutory Lock #1: Spotlight `0x1F` Read-Only Non-Authoritative Lock** | `desktop/src-tauri/src/notify_search_offline.rs`<br>`desktop/src/views/spotlight/spotlight-view.ts` | `test_spotlight_0x1f_statutory_lock_blocks_all_five_mutation_gates_even_with_tampered_mask`<br>`testSpotlight0x1fStatutoryLockAndAuthorityReverification` | **PASS** |
| **Authoritative Jump Re-Verification (`STALE_GENERATION` / `AUTHORITY_RECORD_RETRACTED`)** | `desktop/src-tauri/src/notify_search_offline.rs`<br>`desktop/src/views/spotlight/spotlight-view.ts` | `test_spotlight_authoritative_jump_reverification_and_retracted_or_stale_rejection`<br>`testSpotlight0x1fStatutoryLockAndAuthorityReverification` | **PASS** |
| **`[DUI-OFFLINE-001]` Local 100% Offline Autonomy & `LocalAutonomyProof`** | `desktop/src-tauri/src/notify_search_offline.rs`<br>`desktop/src/views/offline-cluster/cluster-posture-tray.ts` | `test_offline_autonomy_dui_offline_001_local_100_percent_interactive_and_remote_degradation`<br>`testOfflineAutonomyDuiOffline001AndQuarantinedLeaseFence` | **PASS** |
| **Statutory Lock #3: `QUARANTINED_LEASE_FENCE` Rejects Local TTL Unlock** | `desktop/src-tauri/src/notify_search_offline.rs`<br>`desktop/src/views/offline-cluster/cluster-posture-tray.ts` | `test_quarantined_cell_lease_fence_rejects_local_ttl_unlock_and_blocks_dispatch`<br>`testOfflineAutonomyDuiOffline001AndQuarantinedLeaseFence` | **PASS** |
| **`L0/L1/L2` Zero Internal Jargon vs `L3` Engineering Diagnostics (`zh-CN` & `en-US`)** | All 3 views + Rust DTOs | `test_progressive_disclosure_l1_l2_zero_jargon_and_l3_diagnostic_completeness`<br>`testProgressiveDisclosureZeroJargonInL1L2AndCompleteDiagnosticsInL3` | **PASS** |
| **`ANTI-AI-01..07` Solid Surfaces, Zero Blur, Zero Emojis, `{0,2,4,6}px` Radii** | `notifications.css`, `spotlight.css`, `offline-cluster.css`, `.ts` views | `testAntiAiSmellStaticSourceAndCssAudit` | **PASS** |

---

## 3. Verification Gate Outputs

### 3.1 Frontend Deterministic Test Suite (`npx tsx --test`)

Command:
```powershell
cd d:\llmos\desktop; npx tsx --test src/views/notifications/__tests__/notify-search-offline.test.ts
```

Output:
```text
PASS: W72-D-NOTIFY-SEARCH deterministic verification & ANTI-AI-01..07 suite (all assertions green).
✔ src\views\notifications\__tests__\notify-search-offline.test.ts (182.3606ms)
ℹ tests 1
ℹ suites 0
ℹ pass 1
ℹ fail 0
```

### 3.2 Host-Side Rust Integration Test Suite (`notify_token_search_offline_side`)

Command:
```powershell
$env:CARGO_TARGET_DIR='d:\llmos\target\w72-notify'; cargo test --manifest-path desktop/src-tauri/Cargo.toml --test notify_token_search_offline_side --features dev-fixture
```

Output:
```text
running 7 tests
test test_action_token_one_shot_redemption_and_replay_rejection ... ok
test test_action_token_rejects_expired_cross_app_cross_principal_and_forged_commitment ... ok
test test_notification_feed_covers_all_16_canonical_task_states_and_cursor_pagination ... ok
test test_offline_autonomy_dui_offline_001_local_100_percent_interactive_and_remote_degradation ... ok
test test_progressive_disclosure_l1_l2_zero_jargon_and_l3_diagnostic_completeness ... ok
test test_quarantined_cell_lease_fence_rejects_local_ttl_unlock_and_blocks_dispatch ... ok
test test_spotlight_0x1f_statutory_lock_blocks_all_five_mutation_gates_even_with_tampered_mask ... ok
test test_spotlight_authoritative_jump_reverification_and_retracted_or_stale_rejection ... ok

test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

---

## 4. Residual Boundaries & Wave `W72-INT` Hand-Off

- All 9 `#[tauri::command]` handlers (`list_notification_feed`, `redeem_notification_action_token`, `dismiss_notification_item`, `query_spotlight_candidates`, `verify_spotlight_candidate_authority`, `attempt_mutation_from_search_hit`, `inspect_offline_cluster_posture`, `evaluate_offline_action_availability`, `set_cluster_connectivity_mode`) and `NotifySearchOfflineState` are exported in `desktop/src-tauri/src/notify_search_offline.rs`.
- Per the Wave `W72` exclusive write-set boundary (`∩ = ∅`), registration of `NotifySearchOfflineState` in `tauri::Builder::manage(...)` and `tauri::generate_handler![...]` inside `desktop/src-tauri/src/lib.rs`, as well as top-level stylesheet imports in `desktop/src/main.ts` / `desktop/src/style.css`, are reserved for the downstream single-writer integration barrier (`W72-INT`).
