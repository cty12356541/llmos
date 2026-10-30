# Stage E Evidence — `E-TRIAL-002`: Starter Productivity Application Bundle, Local Trial Desktop Builder & Cross-Platform Quickstart Guide

- **Evidence ID**: `E-TRIAL-002`
- **Task / Attempt**: `W77-PRE-C` (Attempt 1, Wave `W77-PRE`, Work Package `E0` `E-TRIAL-BOOTSTRAP`)
- **Base Read Snapshot**: `f6b94f5`
- **Evidence Ladder Tier**: `H3 -> H4` (lane-local deterministic authority workflow, `nlos-package` CLI conformance/lifecycle, and preflight script verification; wave-level integration belongs to `W77-PRE-INT`)
- **Authority Specs & Related Governance**:
  - [`docs/design/06-架构设计总纲-v0.5.md`](../../design/06-架构设计总纲-v0.5.md) (§9.1–§9.3 KABI/SABI Boundary, §15.1 User-Space Driver & `EnforcementShim`, §23.1–§23.2 Immutable Signed Packages & Lifecycle, §29 Deferred Boundaries)
  - [`docs/management/stage-e-progress.md`](../../management/stage-e-progress.md) (§E.1.3 Residual Risk Handover, §E.2.2 `E0` `E-TRIAL-BOOTSTRAP`, §E.3.2 `W77-PRE-C` Exclusive Write-Set)
  - [`docs/management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md`](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) (Stage E Architecture Decision Record)
  - [`docs/evidence/stage-c/c-app-payload-001-sample-cli-execution.md`](../stage-c/c-app-payload-001-sample-cli-execution.md) (Kernel payload-execution lane & `nlos-package` CLI baseline)
- **Deliverables in Exclusive Write-Set**:
  - [`examples/starter-assistant-app/Cargo.toml`](../../../examples/starter-assistant-app/Cargo.toml) & [`examples/starter-assistant-app/README.md`](../../../examples/starter-assistant-app/README.md)
  - [`examples/starter-assistant-app/src/main.rs`](../../../examples/starter-assistant-app/src/main.rs)
  - [`examples/starter-assistant-app/package/package.manifest`](../../../examples/starter-assistant-app/package/package.manifest) (`v1.0.0` developer tree + payloads + 2 serial `tasks/` templates)
  - [`examples/starter-assistant-app/package-v1.1.0/package.manifest`](../../../examples/starter-assistant-app/package-v1.1.0/package.manifest) (`v1.1.0` upgrade tree + payloads + 3 serial `tasks/` templates)
  - [`desktop/scripts/build-trial-bundle.ps1`](../../../desktop/scripts/build-trial-bundle.ps1) & [`desktop/scripts/build-trial-bundle.sh`](../../../desktop/scripts/build-trial-bundle.sh)
  - [`docs/developers/quickstart-trial.md`](../../developers/quickstart-trial.md)

---

## 1. Executive Summary

`E-TRIAL-002` delivers the application-bundle, local desktop trial packaging, and cross-platform developer documentation slice (`W77-PRE-C`) of Stage E `E0` (`E-TRIAL-BOOTSTRAP`):

1. **Out-of-the-Box Reference Productivity Application (`examples/starter-assistant-app/`)**:
   - Structured as a standalone Cargo `[workspace]` (`unsafe_code = "forbid"`, `clippy::all = "warn"`, `clippy::pedantic = "warn"`) consuming solely public authority crates (`nlos-application`, `nlos-artifact`, `nlos-identity`, `nlos-plan`, `nlos-resource`, `nlos-slice-k`, `nlos-task`, `nlos-types`) without modifying root `Cargo.toml` or `Cargo.lock` and without touching any private kernel API.
   - Ships two `nlos-package`-conformant developer trees under `package-id = 4e1077c1a5b24d8e9f0123456789abcd`:
     - `package/` (`v1.0.0`): executable entry `assistant-driver` (`assistant.bin`), data entry `assistant-profile` (`assets/assistant-profile.txt`), and a 2-node serial `TaskPlan` template DAG (`draft-plan -> synthesize-artifact`).
     - `package-v1.1.0/` (`v1.1.0`): preserves the five declaration digests of execution-frozen upstream nodes (`draft-plan`, `synthesize-artifact`) in accordance with `[PLAN-DAG-001]` G1 while extending the DAG with a 3rd audit node (`verify-brief`) and updated `v1.1.0` payload bytes.
   - Executes and verifies the full productivity authority loop: signed package conformance & verification -> templated installation (`ApplicationAuthority` generation 1 + `SqlitePlanAuthority` `TaskPlan` revision 1) -> `ResourceAuthority` five-dimension `LlmTokenBudget` creation and per-step `consume_llm_tokens` charging (`LlmTokenConsumptionReceipt` seq 1..=3, OpenMetrics exposition check) -> `[PLAN-LAZY-001]` dependency fence enforcement -> 3-revision content-addressed `ArtifactStore` brief publication with causal `ProvenanceSourceTriple` lineage -> `CompatibilityWindow::SameMajor` `v1.0.0 -> v1.1.0` hot upgrade & `TaskPlan` revision 2 extension -> clean uninstall and post-uninstall fail-closed refusal.
2. **Controllable Local Trial Desktop Bundle Scripts (`desktop/scripts/build-trial-bundle.{ps1,sh}`)**:
   - Provides dual-platform scripts supporting fast `-CheckOnly` / `--check-only` toolchain and manifest preflight verification (`node >= 20`, `npm`, `cargo`, `rustc`, `desktop/package.json`, `desktop/src-tauri/tauri.conf.json`, `desktop/src-tauri/Cargo.toml`, icons) as well as full unsigned local trial bundling (`npm ci`/`npm install` -> `npm run build` -> `npx tauri build --no-sign --config <temp_override_json>`), leaving tracked `desktop/src-tauri/tauri.conf.json` (`bundle.active = false`) untouched.
   - Emits an explicit honest boundary banner on stdout distinguishing local unsigned trial bundles from `E-deferred` / `RISK-B-06` Apple Developer ID and Microsoft EV hardware-token commercial CA notarization.
3. **Cross-Platform Developer Quickstart (`docs/developers/quickstart-trial.md`)**:
   - Documents side-by-side Windows PowerShell and macOS/Linux POSIX Shell workflows for: (1) `nlos-up daemon` / `desktop` bootstrap (`~/.llmos` / `$env:USERPROFILE\.llmos`); (2) local `~/glm-keypool` (`127.0.0.1:8317/api/coding/paas/v4`) auto-discovery and `nlos-llm-pool-cli status / chat / intent` direct testing with `<YOUR_EGRESS_KEY>` placeholder; (3) `starter-assistant-app` signed packaging, conformance checking, installation, and multi-step `TaskPlan` + `Artifact` execution; and (4) `build-trial-bundle` usage and `E-deferred` notarization boundaries.

---

## 2. Normative Requirement Mapping

| Requirement ID | Normative Invariant Exercised | Concrete Verification in `W77-PRE-C` |
|---|---|---|
| `PKG-IMM-001` | Content-addressed immutable signed package with zero baked mutable state or secrets | `package/` (`v1.0.0`) and `package-v1.1.0/` (`v1.1.0`) pass `check_package_file` and `nlos-package conformance` (`CONFORMANT`), byte-identical rebuild determinism, and mid-file byte-flip tampering rejection (`starter_package_trees_pass_conformance_and_detect_tampering`) |
| `PKG-MULTI-001` | Multi-entry + multi-node task-templated package compilation & digest binding | Both package trees bundle `executable` + `data` entries alongside 2 (`v1.0.0`) and 3 (`v1.1.0`) chained `task` templates compiled via `compile_task_templates` into `SqlitePlanAuthority` revisions 1 and 2 |
| `PKG-UPDATE-001` | Explicit compatibility-window enforcement and execution-frozen plan continuity across updates | `v1.0.0 -> v1.1.0` succeeds under `CompatibilityWindow::SameMajor` (advancing `installation_generation` `1 -> 2` and `TaskPlan` `revision` `1 -> 2`), while `CompatibilityWindow::SameMinor` and frozen-node digest mutation fail closed with zero state drift (`narrow_compat_window_and_frozen_node_rewrite_fail_closed`) |
| `PKG-UNINSTALL-001` | Terminal uninstall receipt commitment and post-uninstall execution refusal | `uninstall_application` transitions status to `ApplicationStatus::Uninstalled` at generation 2; subsequent `execute_application_payload` fails closed |
| `KABI-SVC-001` / `DRV-CRED-001` / `DRV-SHIM-001` | Microkernel remains free of vendor LLM SDKs; credentials stay isolated in user-space `EnforcementShim` coupled with `ResourceAuthority` token budgets | `starter-assistant-app` accounts five-dimension `LlmTokenBudget` (`prompt`, `completion`, `total`, `api_calls`, `cost_microcredits`) through `ResourceAuthority::consume_llm_tokens` and rejects over-budget requests (`llm_token_budget_idempotent_replay_and_exhaustion_fail_closed`); `quickstart-trial.md` documents user-space `nlos-driver-llm-http` / `~/glm-keypool` boundary with zero embedded keys |
| `SABI-RECEIPT-001` | Every state-changing authority step produces an immutable, replay-idempotent receipt and causal provenance | Verifies verification receipts, installation receipts, payload operation receipts, `LlmTokenConsumptionReceipt` (seq 1..=3), `TaskPlan` revision chain (`chain_len = 2`), and `ArtifactStore` `ProvenanceSourceTriple` receipts across revisions 1..=3 |
| `NLOS-DEFER-001` | Deferred commercial release notarization is explicitly separated rather than conflated with internal package signatures | `build-trial-bundle.{ps1,sh}` and `quickstart-trial.md` explicitly declare the `E-deferred` / `RISK-B-06` commercial CA notarization boundary |

---

## 3. Verification Gates & Reproducible Results

All commands below were executed inside the isolated worktree on branch `feat/w77pre-c` (base `f6b94f5`; host user directory prefixes in raw console excerpts are sanitized to `<WORKTREE_ROOT>` and `<TEMP_DIR>` per `[PD-COMMIT-004]`).

### 3.1 Formatting, Strict Pedantic Clippy, Unit/E2E Tests & Workflow Execution

```powershell
cargo fmt --manifest-path examples/starter-assistant-app/Cargo.toml -- --check
cargo clippy --manifest-path examples/starter-assistant-app/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path examples/starter-assistant-app/Cargo.toml
cargo run --manifest-path examples/starter-assistant-app/Cargo.toml
```

- `cargo fmt --manifest-path examples/starter-assistant-app/Cargo.toml -- --check` -> **exit code `0`**
- `cargo clippy --manifest-path examples/starter-assistant-app/Cargo.toml --all-targets -- -D warnings` -> **exit code `0`** (`0` warnings under `unsafe_code = "forbid"`, `clippy::all = "warn"`, `clippy::pedantic = "warn"`)
- `cargo test --manifest-path examples/starter-assistant-app/Cargo.toml` -> **exit code `0` (`5 passed; 0 failed`)**:
  1. `tests::cli_argument_parser_accepts_default_and_explicit_root_only` ... `ok`
  2. `tests::starter_package_trees_pass_conformance_and_detect_tampering` ... `ok`
  3. `tests::llm_token_budget_idempotent_replay_and_exhaustion_fail_closed` ... `ok`
  4. `tests::narrow_compat_window_and_frozen_node_rewrite_fail_closed` ... `ok`
  5. `tests::end_to_end_productivity_workflow_commits_all_receipts_and_uninstalls_cleanly` ... `ok`
- `cargo run --manifest-path examples/starter-assistant-app/Cargo.toml` -> **exit code `0`**, deterministic receipt trace:

```text
[starter-assistant] PACKAGES_CONFORMANT v1_bytes=1329 v2_bytes=1617
[starter-assistant] INSTALL_V1 application=81d19ff8940b6124 package=4e1077c1a5b24d8e generation=1 plan=d85172748ca826d5 plan_rev=1 declared_nodes=2
[starter-assistant] PAYLOAD_V1 entry=assistant-driver generation=1 operation=60169f8f954f5000 digest=303be47176461fee
[starter-assistant] BUDGET_INIT account=585dcc2ac690e7a6 generation=1 max_total_tokens=16384 max_api_calls=8 max_cost_uc=50000
[starter-assistant] PLAN_GATE_BLOCKED node=e8975e5635214e9d unresolved_deps=1
[starter-assistant] LLM_RECEIPT seq=1 id=871086766a4e121f total_tokens=940 cumulative_tokens=940 cumulative_calls=1 cumulative_cost_uc=3200
[starter-assistant] ARTIFACT_REV artifact=333e075c78b2f128 revision=1 bytes=280 digest=d9dc9fdc058434ee provenance_receipt=492486d1cb39f295
[starter-assistant] LLM_RECEIPT seq=2 id=77e276469644fb46 total_tokens=1690 cumulative_tokens=2630 cumulative_calls=2 cumulative_cost_uc=8600
[starter-assistant] ARTIFACT_REV artifact=333e075c78b2f128 revision=2 bytes=511 digest=1f6d01b072659b90 provenance_receipt=e0b883034a27b46e
[starter-assistant] UPDATE_V1_1 generation=2 payload_operation=d1f05a4dd25b37da payload_digest=1b77fb4593a4c274
[starter-assistant] PLAN_REV2 plan=d85172748ca826d5 revision=2 chain_len=2 declared_nodes=3
[starter-assistant] LLM_RECEIPT seq=3 id=704d75eb242e4fc5 total_tokens=720 cumulative_tokens=3350 cumulative_calls=3 cumulative_cost_uc=11200
[starter-assistant] ARTIFACT_REV artifact=333e075c78b2f128 revision=3 bytes=765 digest=52680e6709759972 provenance_receipt=186cf06d8aa99434
[starter-assistant] UNINSTALL application=81d19ff8940b6124 generation=2 status=uninstalled remaining_tokens=13034 remaining_calls=5
[starter-assistant] SUMMARY package=4e1077c1a5b24d8e application=81d19ff8940b6124 plan=d85172748ca826d5 brief_artifact=333e075c78b2f128 account=585dcc2ac690e7a6 v1_receipt=5ecc7aa2c946b2ab v2_receipt=e42af127a5e0f7da v1_payload_digest=303be47176461fee v2_payload_digest=1b77fb4593a4c274 llm_receipts=3 consumed_tokens=3350 final_brief_digest=52680e6709759972 uninstall_generation=2
```

### 3.2 `nlos-package` CLI Build, Conformance & Full Lifecycle Interoperability

Executed via [`crates/nlos-package/src/main.rs`](../../../crates/nlos-package/src/main.rs) into an isolated `<TEMP_DIR>` directory (immediately deleted after verification so zero binary `.nlospkg` or `.devkey` artifacts remain in the repository):

```powershell
cargo run --locked -p nlos-package -- keygen --seed 4747474747474747474747474747474747474747474747474747474747474747 --out "<TEMP_DIR>\starter.devkey"
cargo run --locked -p nlos-package -- build examples/starter-assistant-app/package --key "<TEMP_DIR>\starter.devkey" --out "<TEMP_DIR>\starter-v1.0.0.nlospkg"
cargo run --locked -p nlos-package -- conformance "<TEMP_DIR>\starter-v1.0.0.nlospkg"
cargo run --locked -p nlos-package -- build examples/starter-assistant-app/package-v1.1.0 --key "<TEMP_DIR>\starter.devkey" --out "<TEMP_DIR>\starter-v1.1.0.nlospkg"
cargo run --locked -p nlos-package -- conformance "<TEMP_DIR>\starter-v1.1.0.nlospkg"
cargo run --locked -p nlos-package -- install "<TEMP_DIR>\starter-v1.0.0.nlospkg" --root "<TEMP_DIR>\cli-state"
cargo run --locked -p nlos-package -- run 4e1077c1a5b24d8e9f0123456789abcd assistant-driver --root "<TEMP_DIR>\cli-state"
cargo run --locked -p nlos-package -- update "<TEMP_DIR>\starter-v1.1.0.nlospkg" --root "<TEMP_DIR>\cli-state" --compat same-major
cargo run --locked -p nlos-package -- run 4e1077c1a5b24d8e9f0123456789abcd assistant-driver --root "<TEMP_DIR>\cli-state"
cargo run --locked -p nlos-package -- uninstall 4e1077c1a5b24d8e9f0123456789abcd --root "<TEMP_DIR>\cli-state"
```

Verified outputs (all **exit code `0`**):

- `starter-v1.0.0.nlospkg`: `entries 2 tasks 2`, `manifest_digest 47fd420e3ca52ce3ff8b8cdc0d32ad6954ac0e4997d694ec5cdeb7f599d4ca7d` -> `CONFORMANT`
- `starter-v1.1.0.nlospkg`: `entries 2 tasks 3`, `manifest_digest 3e103dd99870f618139512a6083737d09d3d726d8d451848d3f831d815ab682c` -> `CONFORMANT`
- Cross-tool determinism: `nlos-package` CLI derived the exact same `application = 81d19ff8940b6124272dcca8cd964bae`, generation-1 `operation = 60169f8f954f50005bd5b03f60c2b2fa` (`digest = 303be47176461fee...`), and generation-2 `operation = d1f05a4dd25b37dac411f0548c2f8b8a` (`digest = 1b77fb4593a4c274...`) as `starter-assistant-app`.

### 3.3 Local Trial Bundle Preflight & Documentation Hygiene Gates

```powershell
powershell -ExecutionPolicy Bypass -File desktop/scripts/build-trial-bundle.ps1 -CheckOnly
sh desktop/scripts/build-trial-bundle.sh --check-only
```

- `build-trial-bundle.ps1 -CheckOnly` & `build-trial-bundle.sh --check-only` -> **exit code `0`**, verified `llmos-task-manager (0.1.0)`, `node v24.16.0`, `npm 11.13.0`, `cargo 1.99.0`, `rustc 1.99.0`, `@tauri-apps/cli ^2.11.5`, printed `CHECK_ONLY_OK` and the `E-deferred / RISK-B-06` honest boundary statement.
- Python hygiene & link integrity scan across all newly authored `.md`, `.ps1`, `.sh`, and `.rs` files -> **exit code `0`** (`Emoji = 0`, `100%` of local relative Markdown links exist on disk, `0` private host paths or secrets).

---

## 4. Honest Boundaries & Non-Extrapolation Guardrails

1. **Commercial OS Installer Code Signing & Notarization (`E-deferred` / `RISK-B-06`)**: `desktop/scripts/build-trial-bundle.{ps1,sh}` runs `-CheckOnly` preflight in automated lane verification and invokes `npx tauri build --no-sign` during full local trial builds. Resulting bundles are strictly for local developer trial on the current workstation and are **not** signed or notarized with Apple Developer ID or Microsoft EV hardware-token commercial CA certificates.
2. **Deterministic In-Process Budget Accounting vs. Live HTTP Provider Plane**: `starter-assistant-app` runs deterministically offline against `ResourceAuthority` five-dimension `LlmTokenBudget` so automated tests and CI remain 100% reproducible without external network keys; live local/remote HTTP model execution (`~/glm-keypool` at `127.0.0.1:8317/api/coding/paas/v4`, Ollama, or cloud providers) is owned by sibling lane `W77-PRE-B` (`crates/nlos-driver-llm-http`, `E-TRIAL-001`), and one-command daemon/desktop bootstrapping is owned by sibling lane `W77-PRE-A` (`crates/nlos-up`). Cross-lane integration verification is performed at wave barrier `W77-PRE-INT`.
3. **Untrusted Third-Party Strong Sandbox (`U-9`)**: Payload execution in `starter-assistant-app` exercises digest-bound durable `Operation` receipts (`execute_application_payload`), not cross-platform `Process + Wasmtime` untrusted WASI sandboxing (`crates/nlos-wasm-sandbox`, scheduled for Wave `W80-E6A-WASM-SANDBOX`).
