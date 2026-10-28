# D-ROAD-001 Evidence: Stage D Non-Developer Multi-Application Workstation End-to-End Acceptance Gate (`d-road-001`)

- **Evidence ID**: `D-ROAD-001` (`d-road-001-non-developer-multi-app-e2e-gate`)
- **Wave / Task / Attempt**: `W74` / `W74-D-ROADD001` (`[ROAD-D-001]` Stage D Exit Gate Verification, Attempt 1)
- **Date**: `2026-10-29`
- **Verification Status**: **`PARTIAL_PASS`**
- **Evidence Level**: **`H4`** (Deterministic Multi-Subsystem Workstation Integration & Negative-Bypass E2E Suite)
- **Base Commit**: `40f9967a67a981b385e415353a09cf5518a81633`
- **Normative & Design Authorities**:
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) (`§28.4 [ROAD-D-001]`, `§24.1–§24.3`, `§25.1–§25.4`)
  - [ADR-0025 Stage D Hybrid Desktop Compositor and Trusted UI Layering](../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md) (Decisions 1–4, Gates `D-GATE-001..006`)
  - [2026-10-26-stage-d-desktop-visual-and-ux-design.md](../../superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md) (`W68-R1` / `W68-R2` Neutral Industrial Workstation & `L0–L3` Progressive Disclosure)
  - [stage-d-progress.md](../../management/stage-d-progress.md) (`§D.1.1`, `§D.3.1`, `§D.5.2 W74-D-ROADD001`, `§D.5.5`)

---

## 1. Delivered Write-Set (`WriteSet(W74-D-ROADD001)`)

1. [d_road_001_non_dev_multi_app_e2e.rs](../../../desktop/src-tauri/tests/d_road_001_non_dev_multi_app_e2e.rs)
   - Implements the 6-test end-to-end non-developer multi-application workstation acceptance gate exercising `WorkstationHostShell` (`CompositorState`), `OmnibarEngine` (`OmnibarState`), `TrustedUiAuthority` / `TrustedUiState`, `PlanEditorState`, `NotifySearchOfflineState`, `SettingsKeychainState` (`ControlCenterState`), `A11yI18nState`, and `llmos_desktop_lib::ipc` wire builders in a single cohesive operator journey.
2. [d-road-001-non-developer-multi-app-e2e-gate.md](./d-road-001-non-developer-multi-app-e2e-gate.md)
   - Canonical Stage D exit gate evidence report, requirement coverage matrix, verbatim verification receipts, and residual boundary declaration.

---

## 2. Non-Developer Multi-Application E2E Journey & Test Matrix

| # | Test Function (`d_road_001_non_dev_multi_app_e2e.rs`) | Exercised Subsystems | Non-Developer Journey & Security / Conformance Invariants Verified | Result |
|---|---|---|---|---|
| 1 | `non_dev_multi_app_launch_tiling_and_bilingual_zero_jargon_surface` | `WorkstationHostShell`, `ApplicationLauncher`, `DesktopCompositor`, `A11yI18nState` | Launches 3 workstation apps (`app://notes.workspace`, `app://sheets.finance`, `app://slides.briefing`) by `(ApplicationId, InstallationId)` while rejecting `PackageId` and transient `ProcessId` (`[DUI-LAUNCH-001]`); verifies `Z0 < Z1 < Z2 < Z3 < Z4` monotonic Z-order, `4px` tiled `Split2` (`TwoColumnSplit`), `Grid4` (`Quadrants`), and `ThreePaneWorkstation` layouts with `0px` tiled radius, `30px` `WindowFrame`, and `36px` `Z3` taskbar (`[DUI-WINDOW-001]`); switches `zh-CN <-> en-US` bundles atomically (`switch_generation 1 -> 2 -> 3`); proves `0` internal kernel jargon (`CausalCone`, `LeaseEpoch`, `TypedIntent`, `0x1F`, `TaskAttempt`, `ExecutionFiber`, `receipt_hex`, etc.) and `0` emojis across all `L0/L1/L2` strings; verifies `20/20` `WCAG 2.2 AA` contrast pairs (`>= 4.5:1` text, `>= 3.0:1` UI/focus ring) and `Z0–Z4` ARIA landmarks (`26px` min hit target). | **PASS** |
| 2 | `natural_language_cross_app_goal_ambiguity_clarification_and_z4_consent` | `OmnibarEngine`, `WorkstationHostShell`, `TrustedUiAuthority`, `A11yI18nState` | Verifies `0px` permanent top footprint and `Ctrl+Space` `Z3` (`z-index: 460`, `4px` radius) invocation; records `InputSource::Voice` transcript (`RequestingMicPermission -> Capturing -> Transcribing -> Committed`) + attaches `ChooserGrant` file chip (stripping `C:\Users\Operator\Documents\q4-notes-draft.md` to `"q4-notes-draft.md"`); triggers `[NL-AMBIG-001]` on ambiguous cross-app goal (`MultipleMatchingTargets`, `HighRiskIrreversibleEffect`, `CrossApplicationTransfer`, `BudgetEscalation`), hard-blocking unclarified submission with zero execution (`!z4_modal_lock`); resolves candidate into a 64-hex `clarification_digest_hex` preview and hands off to `Z4` `TrustedConsentOverlay` (`z-index: 900`, `rgba(10,11,13,0.78)` solid scrim, `0` blur, `32x20` Gerono seal, 4-row KV spec table, `Z4` keyboard focus trap); rejects `< 350ms` clickjacking attempts (`ClickjackingArmingActive`) and approves at `>= 350ms` bound to `session_generation`. | **PASS** |
| 3 | `l1_commitment_progression_l2_attribution_and_l3_dag_override_reauth` | `PlanEditorState`, `NotifySearchOfflineState`, `A11yI18nState` | Confirms all 3 historical `TASK_SPACE_GAPS` are closed (`CLOSED_TASK_SPACE_GAPS_COUNT == 3`) and `O(visible_rows) <= 64` cursor clamping holds; verifies `L1` 16-state task progression in plain bilingual copy; inspects `L3` (`Ctrl+Shift+E`) `TaskPlan` DAG with `[PLAN-LAZY-001]` unexpanded super-node (`+18 待物化步骤 · 0 进程` / `+18 Unmaterialized Steps · 0 Processes`, `MetadataOnly`) and expands 8 steps on demand with `0` pre-allocated processes; verifies Statutory Lock #3 rejecting edits on `Completed`/`Active` nodes (`STATUTORY_LOCK_NODE_COMPLETED_OR_ACTIVE`); executes Path A (`PauseNode` + `ReorderNodes` -> bounded `ControlCommand`s without `Z4` re-auth, `plan_revision 3 -> 4`) and Path B (`AddNode` + `ReplaceNode` -> `RecompileAndZ4Reauthorize` with new `candidate_plan_digest_hex`, blocking missing `Z4` grant and committing with valid `Z4ReauthorizationGrantDto`); verifies Statutory Lock #4 (`STATUTORY_LOCK_ATTEMPT_NO_MANUAL_COMMIT` on `TaskAttempt` race lanes); exercises `[TASK-REOPEN-001]` `RetryNewAttempt` (`task-03-attempt-3`) and `RefineNewGeneration` (`task-04-gen3`) preserving prior history. | **PASS** |
| 4 | `notification_action_token_redemption_and_spotlight_0x1f_zero_write_fence` | `NotifySearchOfflineState`, `SettingsKeychainState`, `OmnibarEngine`, `WorkstationHostShell` | Pulls `Z3` notification feed and locates `AWAITING_APPROVAL` item with inline `ActionToken`; proves cross-app spoofing (`ActionTokenAppMismatch`), cross-task spoofing (`ActionTokenTaskMismatch`), and expired token (`ActionTokenExpired`) fail closed; redeems one-shot `ActionToken` (`resulting_task_state == Running`) and proves immediate replay fails closed (`ActionTokenAlreadyConsumed`); queries `Ctrl+K` Spotlight (`0x1F` `NON_AUTHORITATIVE`) and proves all 5 forbidden control gates (`CapabilityGrant`, `ResourceLedgerDeduction`, `TaskCommit`, `ApplicationUpdate`, `IrreversibleAction`) fail closed with `SearchResultNonAuthoritative0x1f` even when caller supplies tampered mask `0x00`, plus `ControlCenter` (`SEARCH_0X1F_READ_ONLY`) and `Omnibar` (`NonAuthoritativeSearchHitForbidden { restriction_bits: 0x1F }`) hard blocks; jumps back to authoritative `Z1/Z2` surface and re-verifies against live store generation (`verified == true`). | **PASS** |
| 5 | `chooser_grant_save_taint_clipboard_escalation_and_settings_cas_undo` | `TrustedUiAuthority`, `TrustedUiState`, `AppState`, `SettingsKeychainState` | Opens `ChooserMode::Save` (`ExportSave`), blocks path traversal (`..\..\Windows\System32\evil.dll` -> `ChooserInvalidExportName`), confirms `"cross-app-briefing-q4.pdf"` after `>= 350ms` with `0` raw host path tokens (`C:\`, `Users`, `Documents`, `/home/`) in serialized `ChooserGrantForAppDto`, and exercises grant; publishes `CREDENTIALS + CONFIDENTIAL` tainted clipboard from `app://sheets.finance` and requests paste into `app://slides.briefing`, triggering `EscalatedToZ4Consent`, resetting `350ms` arming timer on focus regain, approving in `Z4` with high-contrast ack, and releasing payload; verifies `ContentTypeAssociation` package proposal never hijacks active default handler (`[DUI-ASSOC-001]`), binds with CAS revision, and revokes in one click; updates 4-tier `SettingsDomain` (`User` layer `appearance.theme`) with CAS (`revision 20 -> 21`), drains watch event, and rejects stale revision `20` (`CAS_CONFLICT`); verifies `[RCPT-UNDO-001]` (`IRREVERSIBLE_NO_UNDO` / `IrreversibleStatutoryLockViolation`). | **PASS** |
| 6 | `offline_partition_autonomy_six_local_surfaces_and_reconciliation_recovery` | `NotifySearchOfflineState`, `SettingsKeychainState`, `PlanEditorState`, `OmnibarEngine`, `WorkstationHostShell`, `ipc` | Switches connectivity into `Partitioned` and `LocalSingleNode`; proves all 6 local workstation surfaces (`Launcher`, `LocalArtifacts`, `TaskHistoryAndLocalTasks`, `Settings`, `LocalSearch`, `NotificationHistory`) remain 100% interactive (`AvailableLocalAutonomy` with signed `LocalAutonomyProofBadge`) and executes live local operations across `Task Space`, `Omnibar`, `Spotlight`, `SystemControlCenter`, `Settings`, and `ApplicationView`; proves remote actions degrade to `WaitingNetwork` / `UnavailableRemoteOffline` and `QUARANTINED` cell lease rejects local TTL unlock (`QuarantinedLeaseFenceLocked`); simulates transport `UNKNOWN` on armed `TopicPurge` (`UnknownNeedsReconcile` + `DataLossReceiptDto`), blocks blind retry (`BLIND_RETRY_FORBIDDEN_ON_UNKNOWN`), verifies `QueryControlCommandStatus` & monotonic `PollDesktopWatermarkEvents` (`100 -> 107 -> 114`) builders, reconciles to `AppliedIdempotent`, and restores `Connected` mode. | **PASS** |

---

## 3. Requirement ID Coverage Matrix

| Requirement ID (Spec / Stage D Alias) | Normative Invariant Summary | Exercised In Test(s) | Status |
|---|---|---|---|
| `[ROAD-D-001]` | Non-developer continuously uses multiple Applications and background Tasks without understanding Agent orchestration or encountering internal kernel jargon at `L0/L1/L2` | Tests 1–6 | **`PARTIAL_PASS` (`H4`)** |
| `[UI-PARITY-001]` | 12-domain bounded `List* + Inspect*` (`limit <= 64`) and typed `ControlCommand` symmetry + 5+1 statutory non-tamperable locks | Tests 3, 4, 5, 6 | **`PARTIAL_PASS` (`H4`)** |
| `[DUI-COMPOSITOR-001]` / `[DUI-LAUNCH-001]` / `[DUI-SESSION-001]` | 5-plane `Z0–Z4` workstation compositor, `36px` taskbar, strict `(ApplicationId, InstallationId)` launch identity (rejecting `PackageId`/`ProcessId`), session generation fencing | Tests 1, 2 | **`PARTIAL_PASS` (`H4`)** |
| `[DUI-WND-001]` / `[DUI-WINDOW-001]` / `[DUI-INPUT-001]` | `4px` tiled workstation layouts (`Split2`/`Grid4`/`ThreePane`), `30px` `WindowFrame`, `0px` tiled radius, trusted input origin routing, `WCAG 2.2 AA` & ARIA tree | Tests 1, 2, 5 | **`PARTIAL_PASS` (`H4`)** |
| `[DUI-OMNIBAR-001]` / `[NL-COMPILE-001]` / `[NL-PROPOSE-001]` / `[NL-BIND-001]` | On-demand `Ctrl+Space` `Z3` Command & Intent Palette (`0px` top footprint), `InputSource::Voice`, content-addressed/ChooserGrant attachment chips, `L1/L2` zero-jargon preview | Tests 2, 4, 6 | **`PARTIAL_PASS` (`H4`)** |
| `[NL-AMBIG-001]` | High-risk / multi-target / cross-app / budget ambiguity triggers structured clarification table and hard-blocks execution prior to explicit selection | Test 2 | **`PARTIAL_PASS` (`H4`)** |
| `[TUI-CONSENT-001]` / `[RCPT-UNDO-001]` | Unoccludable `Z4` (`z-index: 900`) solid security screen, `350ms` clickjacking arming gate, `32x20` Gerono seal, 4-row KV spec table, `IRREVERSIBLE` zero-fake-undo guard | Tests 2, 5, 6 | **`PARTIAL_PASS` (`H4`)** |
| `[TUI-CHOOSER-001]` / `[DUI-CHOOSER-001]` | `Open / Save / Share` chooser returns scoped `ChooserGrant` capability handles with zero raw host path or unselected sibling disclosure | Tests 2, 5 | **`PARTIAL_PASS` (`H4`)** |
| `[DUI-CLIP-001]` | Clipboard & drag-drop transfers bind `content_type`, source `Application`, `taint`, `expiry`, and human `user_gesture`; tainted cross-app reads escalate to `Z4` | Test 5 | **`PARTIAL_PASS` (`H4`)** |
| `[DUI-TASK-001]` / `[TASK-SPACE-001]` / `[CTRL-PLAN-001]` / `[PLAN-CHANGE-001]` / `[PLAN-OVERRIDE-001]` | `L1 -> L2 -> L3` progressive disclosure `Task Space`, Path A bounded `ControlCommand` vs Path B `AdmissionPlan` recompile + `Z4` diff re-authorization, Statutory Locks #3/#4 | Test 3 | **`PARTIAL_PASS` (`H4`)** |
| `[PLAN-LAZY-001]` | Unexpanded `TaskPlan` subgraph super-node (`+N 待物化步骤 · 0 进程`) remains `MetadataOnly` with `0` allocated processes until expanded on demand | Test 3 | **`PARTIAL_PASS` (`H4`)** |
| `[TASK-REOPEN-001]` | Terminal failed/cancelled tasks reopen via `RetryNewAttempt` or `RefineNewGeneration` while preserving prior attempts and history | Test 3 | **`PARTIAL_PASS` (`H4`)** |
| `[DUI-NOTIFY-001]` / `[TASK-NOTIFY-001]` | 16-state task lifecycle notifications with one-shot `ActionToken` bound to `(NotificationId, ApplicationId, PrincipalId, TaskId)` rejecting replay and cross-app spoofing | Tests 3, 4 | **`PARTIAL_PASS` (`H4`)** |
| `[DUI-SEARCH-001]` / `[CONS-CTRL-001]` | `Ctrl+K` Spotlight carries `0x1F` `NON_AUTHORITATIVE` mask, blocks all 5 mutation gates fail-closed, and requires authoritative jump re-verification | Tests 4, 6 | **`PARTIAL_PASS` (`H4`)** |
| `[DUI-OFFLINE-001]` / `[CTRL-RECOVERY-001]` | 6 local workstation surfaces remain 100% interactive during partition while remote actions show `WAITING_NETWORK` / `UNAVAILABLE`; `ControlCommandId` reconciles `UNKNOWN` without blind retry | Test 6 | **`PARTIAL_PASS` (`H4`)** |
| `[DUI-SETTINGS-001]` / `[DUI-ASSOC-001]` / `[TUI-EXT-001]` | 4-tier `SettingsDomain` (`system/user/workspace/application`) CAS + watch, anti-hijack user-revocable `ContentTypeAssociation`, versioned `zh-CN / en-US` bundles | Tests 1, 5, 6 | **`PARTIAL_PASS` (`H4`)** |

---

## 4. Verification Outputs

### 4.1 Formatting & Clippy Lint Gate

- **Command**:
  ```powershell
  rustfmt --edition 2024 desktop/src-tauri/tests/d_road_001_non_dev_multi_app_e2e.rs
  cargo clippy --manifest-path desktop/src-tauri/Cargo.toml --test d_road_001_non_dev_multi_app_e2e -- -D warnings
  ```
- **Result**: **Exit code `0` (`0` warnings, `0` formatting diffs)**.

### 4.2 `[ROAD-D-001]` Non-Developer Multi-App E2E Integration Suite

- **Command**:
  ```powershell
  cargo test --manifest-path desktop/src-tauri/Cargo.toml --test d_road_001_non_dev_multi_app_e2e
  ```
- **Verbatim Output**:
  ```text
  running 6 tests
  test natural_language_cross_app_goal_ambiguity_clarification_and_z4_consent ... ok
  test chooser_grant_save_taint_clipboard_escalation_and_settings_cas_undo ... ok
  test notification_action_token_redemption_and_spotlight_0x1f_zero_write_fence ... ok
  test offline_partition_autonomy_six_local_surfaces_and_reconciliation_recovery ... ok
  test non_dev_multi_app_launch_tiling_and_bilingual_zero_jargon_surface ... ok
  test l1_commitment_progression_l2_attribution_and_l3_dag_override_reauth ... ok

  test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
  ```

---

## 5. Honest Residual Boundaries (`PARTIAL_PASS`, `H4`)

1. **In-Process Deterministic Workstation State Integration (`H4`)**: `d_road_001_non_dev_multi_app_e2e.rs` verifies the complete Stage D non-developer multi-application journey across all 8 `llmos_desktop` subsystems in-process (`H4`), complementing the cross-platform computer-use GUI campaign and 1,000-cycle soak test in [D-GUI-CAMPAIGN-001](./d-gui-campaign-001-cross-platform-workstation-campaign.md).
2. **Stage E Ecosystem & Packaging Boundaries**: OS-native installer code-signing/notarization (`Authenticode` / macOS Notarization), third-party Publisher Store governance (`ROAD-E-001`), and arbitrary untrusted guest Wasm/ELF GUI rendering sandboxes remain explicitly scoped to Stage E per [stage-d-progress.md §D.1.2](../../management/stage-d-progress.md).
3. **Barrier Integration**: Shared machine ledgers (`claims.yaml`, `risks.yaml`, `evidence-index.yaml`) and `stage-d-progress.md` status transitions are reserved for the `W74-INT` single barrier integrator after `W74-SCHEMA-V12` and `W74-D-EXIT-REVIEW` complete.
