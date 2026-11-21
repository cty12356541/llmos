# Stage D Evidence: `D-NOTIFY-ATTN-001` — `nlos-notify` `AttentionPolicyController` Sidecar Policy Engine & Adversarial Verification

- **Evidence ID**: `D-NOTIFY-ATTN-001`
- **Workpackage / Lane**: `DEL-RES-ATTN-CTRL` (Wave `W82-R4-IND`, Wave 1 independent lane; branch `feat/del-res-attn-ctrl`)
- **Baseline SHA**: `05f967451feaab9f99f84e397445ec3bffe03a6c`
- **Assurance Level**: `H4` (Host-Side Rust Unit & Integration/Adversarial Test Suite on `nlos-notify` Sidecar `SQLite` Ledger)
- **Status**: `VERIFIED_PARTIAL_LAYER` (`nlos-notify` policy/receipt engine verified; desktop Tauri `Z4` wiring & field soak pending)
- **Date**: `2026-11-22`
- **Authoritative Spec Alignment**:
  - `docs/design/06-架构设计总纲-v0.5.md`:
    - `[RSM-HUMAN-001]` (non-fungible human attention; quiet hours & rate policy; no credit/payment bypass)
    - `[RSM-ATTN-002]` (strict separation of ordinary notification vs. security consent queue & failure semantics)
    - `[TUI-CONSENT-001]` (untrusted applications forbidden from forging system security prompts)
    - `[DUI-NOTIFY-001]` (bound notification discipline & tamper-evident receipts)
  - `docs/management/del-res-resource-profile.md` §2 Item 12 (`人类注意力 Human Attention`)

---

## 1. Scope & Deliverables Summary

Implemented the `#![forbid(unsafe_code)]` durable human-attention policy controller (`AttentionPolicyController`) in [`crates/nlos-notify/src/attention.rs`](../../../crates/nlos-notify/src/attention.rs) with module/symbol exports in [`crates/nlos-notify/src/lib.rs`](../../../crates/nlos-notify/src/lib.rs) and full adversarial integration tests in [`crates/nlos-notify/tests/attention_policy.rs`](../../../crates/nlos-notify/tests/attention_policy.rs), using zero new Cargo dependencies (`rusqlite`, `sha2`, `nlos-types`).

### 1.1 Sidecar Persistence Isolation (`attention-policy.db` v1 vs. `notify-service.db` v2)

- `crates/nlos-notify/src/schema.rs` and `NotificationService` logic were left **untouched** (`notify-service.db` remains at `SCHEMA_VERSION = 2`).
- `AttentionPolicyController` persists policy revisions, immutable decision receipts, and active-window coalesce counters into an independent sidecar `SQLite` database `<root>/attention-policy.db` (`ATTENTION_POLICY_SCHEMA_VERSION = 1`).
- Startup enforces `PRAGMA journal_mode = WAL`, `PRAGMA synchronous = FULL`, and `PRAGMA foreign_keys = ON`, reading back all three values and failing closed with `AttentionPolicyError::DurabilityUnavailable` if unavailable.
- Schema tables and tamper-resistance triggers:
  - `attention_policy_config`: immutable versioned policy revisions (`BEFORE UPDATE` and `BEFORE DELETE` abort triggers).
  - `attention_decision_receipts`: immutable audit ledger (`BEFORE UPDATE` and `BEFORE DELETE` abort triggers) keyed by `receipt_id` and `idempotency_key`, foreign-keyed to `attention_policy_config(policy_generation)`.
  - `attention_coalesce_counters`: per-`(origin_key, coalesce_key, window_start_ms)` active-window counters foreign-keyed to `attention_decision_receipts(receipt_id)`, with triggers forbidding deletion and enforcing strictly monotonic `+1` `coalesced_count` increments without key mutation.

### 1.2 Core Invariants Enforced

1. **No Payment Bypass (`[RSM-HUMAN-001]`)**: Requests with `payment_priority_override_attempted == true` fail closed unconditionally across all origins and lanes, committing an immutable `REJECTED_PAYMENT_BYPASS` audit receipt before returning `Err(AttentionPolicyError::PaymentBypassForbidden)`.
2. **High-Risk Consent Origin Isolation (`[TUI-CONSENT-001]`, `[RSM-ATTN-002]`)**: `AttentionOrigin::Application(_)` requests targeting `ConsentPrompt` or `SystemCritical` fail closed unconditionally, committing an immutable `REJECTED_UNTRUSTED_ORIGIN` audit receipt before returning `Err(AttentionPolicyError::UntrustedConsentOrigin)`.
3. **Strict Dual-Lane Isolation & Zero Starvation (`[RSM-ATTN-002]`)**: Only `AttentionOrigin::TrustedSystemUi` requests for `ConsentPrompt` or `SystemCritical` route directly to `AttentionQueueTarget::PriorityConsentLane`, bypassing quiet hours, coalescing, and per-app/global ordinary rate quotas without consuming standard-lane quota.
4. **Quiet Hours, Coalescing & Tumbling-Window Quotas**: `Ordinary` and `BackgroundDigest` requests evaluate local quiet hours first (supporting daytime and cross-midnight ranges such as `22:00..07:00` with `utc_offset_minutes` in `-720..=840` and exact `resume_at_ms`), then collapse repeating `(origin, coalesce_key, active_window)` events when `allow_coalescing` is enabled, and otherwise enforce per-origin and global window limits with automatic window rollover recovery and deterministic `idempotency_key` replay / conflict detection across restarts.

---

## 2. Requirement & Test Verification Matrix

| Invariant / Behavior | Implementation Anchor | Verification Test(s) | Result |
| :--- | :--- | :--- | :--- |
| Sidecar `attention-policy.db` (`v1`) & `notify-service.db` (`v2`) coexistence + `WAL`/`FULL`/`foreign_keys` | `attention.rs::open_configured_connection`, `initialize_v1`, `verify_v1` | `schema_durability_and_coexistence_with_notify_service_v2` | **PASS** |
| Strict policy & request bounds validation (`0`, overflow, minute `< 1440`, offset `-720..=840`, bounded non-empty non-NUL `coalesce_key`) | `AttentionRatePolicy::validate`, `QuietHoursPolicy::validate`, `validate_request` | `rate_and_quiet_policy_validation_rejects_invalid_bounds` (unit)<br>`invalid_policies_fail_closed`<br>`malformed_requests_and_queries_fail_closed` | **PASS** |
| Per-app & global ordinary rate quotas + automatic tumbling-window rollover | `evaluate_standard_lane`, `query_window_delivery_counts`, `record_rate_limited` | `per_app_rate_limit_global_cap_and_window_rollover_recovery` | **PASS** |
| Same-app `(origin, coalesce_key, active_window)` coalescing increment, cross-app isolation, quota interaction & flag toggle | `try_coalesce_in_window`, `record_standard_delivery`, `attention_coalesce_counters` | `same_app_coalescing_increments_and_isolates_across_apps`<br>`coalescing_coexists_with_quota_and_resets_across_windows`<br>`disabling_allow_coalescing_enforces_quota_without_collapsing` | **PASS** |
| Quiet hours (cross-midnight `22:00..07:00` UTC+8 & daytime `13:00..15:00` UTC-5) + exact `resume_at_ms` + trusted priority bypass | `evaluate_quiet_hours`, `record_quiet_hours_deferral` | `quiet_hours_handles_cross_midnight_and_negative_utc_offsets` (unit)<br>`quiet_hours_cross_midnight_defers_ordinary_while_priority_bypasses`<br>`quiet_hours_daytime_negative_offset_defers_and_resumes_cleanly` | **PASS** |
| 500-message ordinary flood starvation resistance (`100%` immediate `PriorityConsentLane` admission) | `AttentionPolicyController::evaluate` | `flood_500_ordinary_notifications_never_starves_priority_consent_lane` | **PASS** |
| Payment bypass rejection & untrusted origin anti-spoofing with durable audit receipts | `AttentionPolicyController::evaluate` | `negative_security_payment_bypass_and_untrusted_origin_audits` | **PASS** |
| Deterministic `idempotency_key` replay across DB reopen & `IdempotencyConflict` | `compute_request_fingerprint`, `replay_existing_receipt` | `deterministic_idempotent_replay_across_restart_and_conflict_rejection` | **PASS** |
| `SQLite` `BEFORE UPDATE`/`BEFORE DELETE` immutability triggers & schema tamper detection | `INIT_V1_SQL` triggers, `verify_v1` | `sqlite_immutability_triggers_and_tamper_detection_fail_closed` | **PASS** |

---

## 3. Verification Gate Outputs

Executed on `2026-11-22` inside isolated worktree `wt-del-res-attn-ctrl` (`feat/del-res-attn-ctrl` based on `05f967451feaab9f99f84e397445ec3bffe03a6c`):

### 3.1 Formatting & Strict Clippy Gate

Commands:
```sh
cargo fmt -p nlos-notify -- --check
cargo clippy -p nlos-notify --all-targets -- -D warnings
```

Observed output (`exit code 0`):
```text
    Checking nlos-notify v0.1.0 (/Users/lipunima/.gemini/antigravity/brain/875ab7ba-845f-40de-8ce8-81186117c749/scratch/wt-del-res-attn-ctrl/crates/nlos-notify)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.15s
```

### 3.2 `nlos-notify` Full Test Suite (`42 passed; 0 failed`)

Command:
```sh
cargo test -p nlos-notify
```

Observed output (`exit code 0`):
```text
   Compiling nlos-notify v0.1.0 (/Users/lipunima/.gemini/antigravity/brain/875ab7ba-845f-40de-8ce8-81186117c749/scratch/wt-del-res-attn-ctrl/crates/nlos-notify)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.99s
     Running unittests src/lib.rs (target/debug/deps/nlos_notify-81eb24c245a5a970)

running 4 tests
test attention::tests::quiet_hours_handles_cross_midnight_and_negative_utc_offsets ... ok
test task_notify::tests::all_sixteen_task_transition_tags_roundtrip_and_classify_terminality ... ok
test task_notify::tests::task_envelope_fixed_wire_encoding_is_deterministic ... ok
test attention::tests::rate_and_quiet_policy_validation_rejects_invalid_bounds ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/attention_policy.rs (target/debug/deps/attention_policy-fc60089d9f38115f)

running 13 tests
test invalid_policies_fail_closed ... ok
test malformed_requests_and_queries_fail_closed ... ok
test disabling_allow_coalescing_enforces_quota_without_collapsing ... ok
test quiet_hours_daytime_negative_offset_defers_and_resumes_cleanly ... ok
test same_app_coalescing_increments_and_isolates_across_apps ... ok
test coalescing_coexists_with_quota_and_resets_across_windows ... ok
test per_app_rate_limit_global_cap_and_window_rollover_recovery ... ok
test quiet_hours_cross_midnight_defers_ordinary_while_priority_bypasses ... ok
test deterministic_idempotent_replay_across_restart_and_conflict_rejection ... ok
test negative_security_payment_bypass_and_untrusted_origin_audits ... ok
test sqlite_immutability_triggers_and_tamper_detection_fail_closed ... ok
test schema_durability_and_coexistence_with_notify_service_v2 ... ok
test flood_500_ordinary_notifications_never_starves_priority_consent_lane ... ok

test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.16s

     Running tests/c_notify_task_action_token.rs (target/debug/deps/c_notify_task_action_token-9c438bd3839b1a3c)

running 13 tests
test malformed_and_truncated_task_notification_payloads_fail_closed ... ok
test issue_action_token_verifies_subscription_bounds_and_idempotency ... ok
test cross_application_spoof_is_rejected_and_leaves_token_unconsumed_for_owner ... ok
test one_shot_token_blocks_second_redeem_and_sqlite_triggers_block_rollback ... ok
test expired_action_token_is_rejected_and_exact_deadline_succeeds ... ok
test offline_history_monotonic_retention_pruning_and_sqlite_triggers ... ok
test offline_history_cursor_pagination_and_conflict_bounds ... ok
test background_task_transitions_publish_wake_and_poll_without_shadow_fanout ... ok
test pruned_offline_history_survives_restart_and_replayed_publish_converges ... ok
test action_token_happy_path_persists_across_reopen_without_storing_raw_secret ... ok
test revoked_action_token_cannot_be_redeemed_or_resurrected ... ok
test wrong_bindings_invalid_proof_and_unknown_token_fail_closed ... ok
test schema_v1_migrates_in_place_to_v2_preserving_existing_subscriptions ... ok

test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.34s

     Running tests/notify_service.rs (target/debug/deps/notify_service-8fe9efcf0880c0cf)

running 12 tests
test cancel_routes_through_topic_authority_and_removes_the_reference ... ok
test typed_failures_for_ack_sequence_bounds ... ok
test publish_replay_reenqueues_nothing_and_rereports_the_original_wake ... ok
test list_reports_live_authority_state_through_the_thin_reference ... ok
test no_shadow_fanout_delivery_reads_only_authority_state ... ok
test subscribe_wait_publish_deliver_ack_full_chain ... ok
test stale_stored_token_fails_closed_then_self_heals_on_resubscribe ... ok
test publish_notify_flips_only_covered_waits ... ok
test typed_failures_for_unknown_topic_and_unknown_notification ... ok
test typed_failures_for_delivery_wait_registration ... ok
test subscription_reference_records_replay_across_restart ... ok
test list_surfaces_dangling_references_when_the_authority_diverged ... ok

test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.31s

   Doc-tests nlos_notify

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

---

## 4. Honest Boundary Statement & Remaining Integration Gaps

1. **Scope Boundary (`nlos-notify` Authoritative Policy Engine Only)**: This deliverable provides the authoritative `nlos-notify` policy evaluation engine, `SQLite` sidecar ledger (`attention-policy.db`), immutable decision receipts, and active-window coalesce counters at assurance level **`H4`**.
2. **Desktop `Z4` & Daemon Integration Pending (`DEL-UX` / Wave Integration)**: End-to-end wiring into the desktop Tauri `Z4` Trusted UI / notification center frontend-backend (`desktop/src-tauri` & `desktop/src`) and `SystemControlDaemon` (`nlos-system-control`) has **not** yet been performed in this lane.
3. **Human Long-Duration Field Soak Pending (`DEL-FIELD`, `H8 = 0`)**: No multi-day human workstation soak or cross-platform desktop UI field verification has been executed (`H8 = 0`).
4. **Shared Index Hand-Off**: Per the exclusive 4-file write-set of lane `DEL-RES-ATTN-CTRL`, updates to `docs/evidence/evidence-index.yaml`, `docs/management/del-res-resource-profile.md`, and `claims.yaml` are reserved for the downstream wave integrator barrier.
