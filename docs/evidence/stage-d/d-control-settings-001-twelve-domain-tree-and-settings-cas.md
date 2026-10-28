# D-CONTROL-SETTINGS-A11Y Slice 1 Evidence: 12-Domain System Control Center, 10-Layer Ownership Tree & 4-Layer Settings CAS (`d-control-settings-001`)

- **Wave / Task / Attempt**: `W72` / `W72-D-SETTINGS-CTRL` (`D-CONTROL-SETTINGS-A11Y` Slice 1, Attempt 1)
- **Road / ADR / Spec Authorities**:
  - `docs/design/06-架构设计总纲-v0.5.md` (`[DUI-CTRL-001]`, `[DUI-SETTINGS-001]`, `[DUI-ASSOC-001]`, `[DUI-SESSION-001]`, `[TASK-AGENT-001]`, `[TASK-ISO-005]`, `[UI-PARITY-001]`, `[SCHED-STEAL-001]`, `[LEASE-TTL-001]`, `[CTRL-SAFETY-001]`, `[CTRL-RECOVERY-001]`, `[CONS-CTRL-001]`, `[RCPT-UNDO-001]`, `[TASK-GRAPH-002]`, `[TASK-COMMIT-001]`, `[TASK-EFFECT-003]`)
  - `docs/management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md` (Decisions 1–4, Gates `D-GATE-001..006`)
  - `docs/superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md` (`§0.1 ANTI-AI-01..07`, `§1.1–§1.3`, `§2A.1–§2A.4`, `§3.0`, `§3.7`, `§4.1–§4.3`)
  - `docs/management/stage-d-progress.md` (`§D.3.1 #8 D-CONTROL-SETTINGS-A11Y`, `§D.5.2 W72-D-SETTINGS-CTRL`)

---

## 1. Delivered Write-Set (`WriteSet(W72-D-SETTINGS-CTRL)`)

### 1.1 Frontend 12-Domain `SystemControlCenter` (`desktop/src/views/control-center/**`)

1. [ownership-tree.ts](file:///d:/llmos/desktop/src/views/control-center/ownership-tree.ts)
   - Implements the 12-Domain `SystemControlCenter` (`system-device`, `workspace`, `application`, `task`, `task-group`, `task-node`, `task-attempt`, `execution-unit`, `resource-group`, `topic-channel`, `operation`, `cell`) and the System Informer 10-layer parent-child ownership hierarchy (`L1_SYSTEM_DEVICE` through `L10_EXECUTION_FIBER`, depths `0..9`).
   - Automatically detects and propagates `SHARED_THREAD_LIKE` fault-domain badges (`[TASK-AGENT-001]`, `[TASK-ISO-005]`) when multiple `AgentInstance` or `ExecutionFiber` units share a host OS `ProcessId` (`PID 4820`).
   - Integrates CNCF Headlamp capability + state-machine action pruning (`computeAllowedVerbs`), the 5+1 statutory non-tamperable locks (`renderStatutoryLockBadgeHtml`), Nomad triple-watermark bars (`renderBulletMicroBarHtml`), and `O(visible_rows)` cursor-windowed tree slicing (`windowOwnershipTreeRows`).
2. [cell-lease-panel.ts](file:///d:/llmos/desktop/src/views/control-center/cell-lease-panel.ts)
   - Inspects Stage C `Cell` connectivity posture (`LOCAL_SINGLE_NODE`, `HEALTHY_CLUSTER_MEMBER`, `DEGRADED_HEARTBEAT`, `PARTITIONED`, `FENCED_READ_ONLY`, `QUARANTINED`, `OFFLINE`), `PartitionAutonomyGate` (`OPEN`, `LOCAL_AUTONOMY_ONLY`, `FENCED_READ_ONLY`, `QUARANTINED_HOLD`), and the 3 Stage C lease families (`EXECUTION_LEASE` / `Quota`, `RESOURCE_SUBPOOL_LEASE` / `Capacity`, `CAPABILITY_EPOCH_LEASE` / `ExclusiveDevice`).
   - Implements `evaluateProcessMigration` (`[SCHED-STEAL-001]`, `[LEASE-TTL-001]`) enforcing CAS `expectedEpoch`, blocking migration into `PARTITIONED / FENCED_READ_ONLY / QUARANTINED / OFFLINE` target cells, and blocking migration out of a `QUARANTINED` cell without both `closureReceiptHex` and `fenceReceiptHex` (`QUARANTINED_LEASE_FENCE`).
3. [blast-radius-modal.ts](file:///d:/llmos/desktop/src/views/control-center/blast-radius-modal.ts)
   - Implements Terraform-style `+ / ~ / - / =` blast-radius diff preview rows (`buildBlastRadiusRowsForVerb`), the 5-second (`5000ms`) two-step arming countdown with `350ms` clickjack guard (`[CTRL-SAFETY-001]`), mandatory `TopicPurge` data-loss acknowledgment and **Data-Loss `Receipt`** generation (`§2A.3`), and `[CTRL-RECOVERY-001]` `ControlCommandReconciler` integration (`BLIND_RETRY_FORBIDDEN_ON_UNKNOWN` until status is reconciled).
4. [index.ts](file:///d:/llmos/desktop/src/views/control-center/index.ts)
   - Provides the `360px` bottom-right `SystemControlCenter` tray flyout (`renderSystemControlCenterFlyoutHtml`), full workstation `TaskManagerSurface` & `ResourceMonitorSurface` (`renderControlCenterWorkstationHtml`), and stateful controller (`createSystemControlCenterController`).
5. [control-center.css](file:///d:/llmos/desktop/src/views/control-center/control-center.css)
   - 100% solid `Graphite/Paper` workstation tokens (`--bg-elevated`, `--bg-canvas`, `--bg-surface`), strict `{0px, 2px, 4px, 6px}` `border-radius` discipline, zero `backdrop-filter`, zero glow.

### 1.2 Frontend 4-Layer Settings CAS, Associations & Keychain Vault (`desktop/src/views/settings/**`)

1. [settings-cas-view.ts](file:///d:/llmos/desktop/src/views/settings/settings-cas-view.ts)
   - Implements the 4-layer `SettingsDomain` (`system / user / workspace / application`) effective inheritance resolver (`application > workspace > user > system`), CAS compare-exchange (`expectedDomainRevision` returning `CAS_CONFLICT` on stale revision), system-policy lock protection (`SYSTEM_POLICY_LOCKED`), and monotonic `watchSettingsDomain` live synchronization (including live theme switching between `graphite-workstation` and `paper-workstation`).
2. [associations-view.ts](file:///d:/llmos/desktop/src/views/settings/associations-view.ts)
   - Implements user-revocable `ContentTypeAssociation` management (`[DUI-ASSOC-001]`) where package installations can only submit unapproved proposals (`submitPackageProposal`) and can **never** silently overwrite `handlerApplicationId` or `handlerInstallationId`, plus explicit user CAS binding and one-click revocation back to the system chooser.
3. [keychain-view.ts](file:///d:/llmos/desktop/src/views/settings/keychain-view.ts)
   - Implements the zero-secret `KeychainReference` vault (`[DUI-SETTINGS-001]`, `[DUI-SESSION-001]`): keeps raw secret material strictly in a private closure map, projects only `KeychainReferenceViewItem` metadata (`keychainRefId`, `publicFingerprintHex`, `authorizedApplicationIds`, `hardwareBacked`, `revision`, `revoked`), supports scoped `acquireUseByReferenceHandle`, one-click revocation, and immediate handle cache wipe on `lockSessionAndWipeHandles()`.
4. [index.ts](file:///d:/llmos/desktop/src/views/settings/index.ts) & [settings.css](file:///d:/llmos/desktop/src/views/settings/settings.css)
   - Unified Settings & Vault controller + 100% solid `Graphite/Paper` stylesheet (`{0px, 2px, 4px, 6px}` radii).

### 1.3 Rust Tauri Backend & Integration Tests (`desktop/src-tauri/**`)

1. [settings_keychain.rs](file:///d:/llmos/desktop/src-tauri/src/settings_keychain.rs)
   - Implements `SettingsKeychainState` and core + `#[tauri::command]` operations for 12-domain control center projection, 10-layer System Informer tree with `SHARED_THREAD_LIKE` propagation, Headlamp action pruning & 5+1 statutory lock validation, Stage C `Cell` 3-family lease inspection & `request_process_migration_core`, Terraform `+ / ~ / - / =` blast-radius preview, 5s two-step arming + `DataLossReceiptDto`, `ControlCommandId` idempotent reconciliation, 4-layer Settings CAS + watch log, anti-hijack `ContentTypeAssociation`, and zero-secret `KeychainReference` vault with `PrivateVaultSecretMaterial` (`<REDACTED_TCB_VAULT_SECRET>`).
2. [control_center_settings_cas_side.rs](file:///d:/llmos/desktop/src-tauri/tests/control_center_settings_cas_side.rs)
   - End-to-end Rust integration and negative-bypass test suite verifying all 6 statutory locks, process migration CAS & quarantine fences, two-step arming & `DataLossReceiptDto`, `BLIND_RETRY_FORBIDDEN_ON_UNKNOWN` reconciliation, 4-layer Settings CAS conflicts, package association anti-hijack, and zero-secret serialization proofs.
3. [control-center-settings.test.ts](file:///d:/llmos/desktop/src/views/control-center/__tests__/control-center-settings.test.ts)
   - Deterministic TypeScript verification suite covering all 12 domains, 10 layers, `SHARED_THREAD_LIKE`, 5+1 statutory locks, Stage C migration, blast-radius 5s arming & Data-Loss Receipt, `ControlCommandId` recovery, 4-layer Settings CAS + watch, `ContentTypeAssociation` anti-hijack, `KeychainReference` zero-secret leak proof, `L0/L1/L2` zero-jargon audit (`zh-CN` & `en-US`), and static/rendered `ANTI-AI-01..07` compliance.

---

## 2. Verification Receipts

1. **Frontend Unit & Invariant Suite (`npx tsx --test`)**:
   - Command: `npx tsx --test src/views/control-center/__tests__/control-center-settings.test.ts`
   - Result: **7 passed, 0 failed** (`pass 7, fail 0`).
2. **Rust Integration & Negative-Bypass Suite (`cargo test`)**:
   - Command: `cargo test --manifest-path desktop/src-tauri/Cargo.toml --features dev-fixture --test control_center_settings_cas_side -- --nocapture`
   - Result: **7 passed, 0 failed**.
3. **Rust Clippy & Format Gates (`cargo clippy -- -D warnings`, `cargo fmt -- --check`)**:
   - Zero warnings, zero formatting diffs.
