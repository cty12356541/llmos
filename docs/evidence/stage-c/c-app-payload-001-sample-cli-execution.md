# Stage C Evidence — `C-APP-PAYLOAD-001`: Third-Party Sample Application Payload Execution & `nlos-package` CLI End-to-End Closure

- **Evidence ID**: `C-APP-PAYLOAD-001`
- **Task / Attempt**: `W65-CAPP-PAYLOAD` (Attempt 1, Wave `W65`; closing end-to-end over landed `W35-P2`, `W38-P2`, and `W49/W51`)
- **Handover Item**: Stage B → Stage C `#2 C-APP-PAYLOAD` (`载荷执行面 + nlos-package CLI install/run/update/uninstall + 第三方样板应用自身接线与独立 Evidence`)
- **Authority Specs & Related Evidence**:
  - [`docs/design/06-架构设计总纲-v0.5.md`](../../design/06-架构设计总纲-v0.5.md) (§9 Driver / Operation Lifecycle, §12 Semantic Authority, §13 Package & Application Authority)
  - [`docs/evidence/stage-b/b-application-007-third-party-sample.md`](../stage-b/b-application-007-third-party-sample.md) (`ROAD-B-001` constructive exit + `W35-P2` front slice)
  - [`docs/developers/packaging.md`](../../developers/packaging.md) (`nlos-package` CLI developer reference)
- **Implementation & Verification Artifacts**:
  - [`crates/nlos-slice-k/src/payload.rs`](../../../crates/nlos-slice-k/src/payload.rs) (`execute_application_payload`, `PayloadExecution`, `payload_execution_seed`)
  - [`crates/nlos-slice-k/src/semantic_writer.rs`](../../../crates/nlos-slice-k/src/semantic_writer.rs) (`SemanticWriter::record_payload_execution`)
  - [`crates/nlos-package/src/main.rs`](../../../crates/nlos-package/src/main.rs) (`install`, `run`, `update`, `uninstall` subcommands & exit-code contract `0..=7`)
  - [`examples/sample-app/src/main.rs`](../../../examples/sample-app/src/main.rs) (`cmd_run` payload execution wiring, `cmd_payload` inspection/replay subcommand, cross-generation driver tests)
  - [`examples/sample-app/lifecycle.sh`](../../../examples/sample-app/lifecycle.sh) (co-root `sample-app-driver` + `nlos-package` drill & standalone pure-CLI lifecycle drill)
  - [`examples/sample-app/README.md`](../../../examples/sample-app/README.md) (updated third-party developer workflow & honest scope)
  - [`crates/nlos-package/tests/sample_app_cli_lifecycle.rs`](../../../crates/nlos-package/tests/sample_app_cli_lifecycle.rs) (cross-platform CLI E2E integration suite over real `examples/sample-app/package` `v1.0.0` and `examples/sample-app/package-v1.1.0` `v1.1.0`)

---

## 1. Executive Summary

`C-APP-PAYLOAD-001` closes Stage B → Stage C handover `#2 C-APP-PAYLOAD` end-to-end across the kernel payload-execution lane, the `nlos-package` developer CLI, and the third-party sample application (`examples/sample-app`):

1. **Kernel Payload-Execution Lane & Semantic Assertion Bridge (`W35-P2` + `W49/W51`)**:
   - [`execute_application_payload`](../../../crates/nlos-slice-k/src/payload.rs) resolves an installed application's current `InstallationReceipt`, computes the manifest-declared executable entry's deterministic `ArtifactId` via `derive_artifact_id(package_id, package_version, entry_name)`, reads back the retention-gated head revision bytes from `ArtifactStore`, and drives one durable `nlos-driver-mock` `MockProvider` lifecycle (`register -> dispatch -> complete`) seeded by `SHA-256("llmos/slice-k/payload-execution-seed/v1" || payload_bytes)`.
   - Terminal completion bridges into `SemanticAuthority` through [`SemanticWriter::record_payload_execution`](../../../crates/nlos-slice-k/src/semantic_writer.rs), emitting a capability-authorized, replay-idempotent `SemanticAssertionReceipt` (`schema_id = "llmos/slice-k/payload-execution/v1"`).
2. **Complete `nlos-package` Developer Lifecycle CLI (`W35-P2` + `W38-P2`)**:
   - `nlos-package` exposes the complete developer and operator command surface over one persistent `--root <DIR>`: `keygen`, `build`, `conformance`, `verify`, `install`, `run`, `update [--compat same-major|same-minor]`, and `uninstall`, with deterministic domain-separated idempotency keys and typed exit codes `0..=7`.
3. **Third-Party Sample Application Wiring & Cross-Platform E2E Verification (`W65-CAPP-PAYLOAD`)**:
   - [`examples/sample-app/src/main.rs`](../../../examples/sample-app/src/main.rs) wires `execute_application_payload(&runtime, package_id, "sample-driver")` directly into `sample-app-driver run` (executing the materialized `package/app.bin` bytes before Task/Attempt/Process/Fiber staging and verifying `payload_digest == ContentDigest::of_bytes(app.bin)`) and adds `sample-app-driver payload <state_root> <package_id_hex> [entry_name]` for generation-2 (`package-v1.1.0/app.bin`) execution and idempotent replay verification.
   - [`examples/sample-app/lifecycle.sh`](../../../examples/sample-app/lifecycle.sh) exercises both co-root interoperability (`sample-app-driver` + `nlos-package` sharing `$STATE`, verifying exact generation-1 replay and distinct generation-2 operation/digest after W29-E staged update) and a standalone pure-CLI `$CLI_STATE` lifecycle (`install -> run -> negative gate -> update -> run -> uninstall -> post-uninstall refusal`).
   - [`crates/nlos-package/tests/sample_app_cli_lifecycle.rs`](../../../crates/nlos-package/tests/sample_app_cli_lifecycle.rs) drives `env!("CARGO_BIN_EXE_nlos-package")` headlessly on both Unix and Windows over isolated copies of the real `examples/sample-app/package` (`v1.0.0`) and `examples/sample-app/package-v1.1.0` (`v1.1.0`) trees.

---

## 2. Authority Dataflow & CLI Exit-Code Contract

### 2.1 End-to-End Lifecycle Chain Over `examples/sample-app`

| Lifecycle Stage | CLI / Driver Surface | Durable Authorities Exercised | Key Verified Facts |
| :--- | :--- | :--- | :--- |
| Keygen & Build (`v1.0.0`, `v1.1.0`) | `nlos-package keygen` / `build` / `conformance` | Stateless `.nlospkg` v2 encoder & conformance checker (`20` rules) | Package ID `9a1c7d3e5f0b2a4c6d8e0f1a2b3c4d5e`, `entries 2 tasks 2` (`sample-driver` executable `app.bin` + `sample-config` data `assets/config.bin`), byte-reproducible output |
| Verify & Install (`gen 1`) | `nlos-package verify` + `install` / `sample-app-driver install` | `IdentityAuthority`, `ArtifactStore`, `ApplicationAuthority`, `AuthorityClock` | Full gate verification in throwaway store before target materialization, orphan-blob GC pass, 7-item digest-bound `InstallationReceipt` (`generation = 1`, `executables sample-driver`) |
| Execute Payload (`gen 1`) | `nlos-package run <pkg> sample-driver` / `sample-app-driver run` | `ApplicationAuthority`, `ArtifactStore`, `SqliteOperationStore` (`MockProvider`), `CapabilityAuthority`, `SemanticAuthority` | Reads `v1.0.0` `app.bin`, records `admission` / `preparation` / `activation` + terminal `CompletionOutcome` receipts and `SemanticAssertionReceipt`; cross-tool re-execution on same `--root` returns `decision replayed` with identical operation & receipt IDs |
| Same-Major Update (`gen 2`) | `nlos-package update --compat same-major` / `sample-app-driver update` | `IdentityAuthority`, `ArtifactStore`, `ApplicationAuthority` (`MigrationRunner` in driver) | Verifies `v1.1.0`, enforces `CompatibilityWindow::SameMajor`, advances `current_installation_generation` from `1` to `2` |
| Execute Updated Payload (`gen 2`) | `nlos-package run <pkg> sample-driver` / `sample-app-driver payload` | Same authorities under generation-2 identity derivation | Derives fresh `(operation_id, callback_id)` from `(application_id, generation=2, "sample-driver")`, reads updated `v1.1.0` `app.bin` digest, leaves generation-1 receipts intact |
| Uninstall & Post-Uninstall Gate | `nlos-package uninstall` / `sample-app-driver uninstall` | `ApplicationAuthority` (+ W27-D activity gate & W30-D teardown chain in driver) | Transitions application to terminal `Uninstalled` (`generation = 2`); subsequent `nlos-package run` refuses fail-closed with exit code `7` (`driver` exits `2`) |

### 2.2 Verified `nlos-package` Exit-Code Mapping

| Exit Code | Variant | Trigger Conditions Verified by Tests |
| :---: | :--- | :--- |
| `0` | `SUCCESS` | Clean `keygen`, `build`, `conformance`, `verify`, `install`, `run`, `update`, `uninstall` (fresh or idempotent `replayed`) |
| `1` | `Usage` | Missing required positional arguments or unknown flags/subcommands |
| `2` | `Input` | Malformed hex identifiers, invalid `--compat` window token, corrupt `.nlospkg` framing |
| `3` | `Signature` | Invalid `Ed25519` package signature, unknown signer principal, or revoked key |
| `4` | `Binding` | Tampered entry payload bytes during `verify`/`install`/`update` (leaves zero tampered payload bytes in `--root`), or `run` naming an unknown/undeclared entry or raw file path (`assets/config.bin`, `app.bin`, `missing-entry` → `ArtifactError::ArtifactNotFound`) |
| `5` | `Internal` | Unrecoverable filesystem or SQLite authority open/I/O error |
| `6` | `Conformance` | Static `.nlospkg` conformance rule violations |
| `7` | `Install` / `Update` / `Uninstall` / `Run` | Application authority or payload-state refusal: unknown or `uninstalled` package on `run`, `HeadState::Empty` unmaterialized entry artifact on `run`, `1.0.0 -> 1.1.0` under `--compat same-minor`, or `1.1.0 -> 2.0.0` under `--compat same-major` |

---

## 3. Verification Commands & Test Matrix

Executed in `d:\llmos` on base commit `5ac73b6333515efbd28b9fa94a4c9eb27df2cf18`:

```powershell
cargo fmt -p nlos-package -- --check
cargo fmt --manifest-path examples/sample-app/Cargo.toml -- --check
cargo clippy -p nlos-package --all-targets -- -D warnings
cargo clippy --manifest-path examples/sample-app/Cargo.toml --all-targets -- -D warnings
cargo test -p nlos-package --no-fail-fast
cargo test --manifest-path examples/sample-app/Cargo.toml --no-fail-fast
```

- `cargo fmt -p nlos-package -- --check` → **exit code `0`**
- `cargo fmt --manifest-path examples/sample-app/Cargo.toml -- --check` → **exit code `0`**
- `cargo clippy -p nlos-package --all-targets -- -D warnings` → **exit code `0`** (zero warnings)
- `cargo clippy --manifest-path examples/sample-app/Cargo.toml --all-targets -- -D warnings` → **exit code `0`** (zero warnings)
- `cargo test -p nlos-package --no-fail-fast` → **exit code `0`** (**`47` passed, `0` failed** across `8` test binaries: `5` unit + `4` `install_payload_execution` + `20` `package_conformance` + `8` `package_sdk_cli` + `2` `run_payload_execution` + `4` `sample_app_cli_lifecycle` + `2` `uninstall_payload_execution` + `2` `update_payload_execution`)
- `cargo test --manifest-path examples/sample-app/Cargo.toml --no-fail-fast` → **exit code `0`** (**`3` passed, `0` failed** in `sample-app-driver`)

### New `W65-CAPP-PAYLOAD` Tests (`7` new tests: `4` CLI E2E + `3` `sample-app-driver`)

| Binary / Test | Verified Invariants |
| :--- | :--- |
| `sample_app_cli_lifecycle::full_sample_app_cli_lifecycle_executes_v1_and_v1_1_payloads_and_refuses_after_uninstall` | Full CLI chain (`keygen -> build v1.0.0 -> conformance -> verify -> install -> run [gen 1] -> build v1.1.0 -> conformance -> update --compat same-major -> run [gen 2] -> uninstall -> post-uninstall run exit 7`) over real `examples/sample-app` trees + `SliceKRuntime` readback of digests, provider outcomes, and terminal `Uninstalled` state |
| `sample_app_cli_lifecycle::non_executable_asset_paths_and_unmaterialized_entries_fail_closed` | Raw asset paths (`assets/config.bin`, `app.bin`) and undeclared names (`missing-entry`) fail closed with exit `4` (`entry artifact unknown`), and an unmaterialized `HeadState::Empty` entry artifact fails closed with exit `7` (`payload execution state refusal`) |
| `sample_app_cli_lifecycle::tampered_payload_exits_four_and_incompatible_version_windows_exit_seven` | Corrupting `app.bin` bytes inside signed `v1.0.0` `.nlospkg` fails `install` with exit `4` leaving zero tampered payload bytes in `--root`; `1.0.0 -> 1.1.0` under `--compat same-minor` and `1.1.0 -> 2.0.0` under `--compat same-major` both fail closed with exit `7` leaving active generation untouched |
| `sample_app_cli_lifecycle::deterministic_builds_and_idempotent_cli_replays_across_all_lifecycle_verbs` | Rebuilding `v1.0.0` / `v1.1.0` produces byte-identical `.nlospkg` archives; re-running `verify`, `install`, `run`, `update`, and `uninstall` returns `REPLAYED` / `decision replayed` with bit-identical receipt, operation, and outcome identifiers |
| `sample_app_driver::tests::hex_and_driver_key_derivations_stay_deterministic_and_domain_separated` | Fixed-width `HEX32` parsing/formatting and domain-separated `llmos/sample-app/driver-key/v1` idempotency key derivation |
| `sample_app_driver::tests::stand_in_pid_resolution_enforces_explicit_pid_contract` | Explicit OS stand-in PID contract for `cmd_uninstall` across zero vs. active process bindings |
| `sample_app_driver::tests::driver_lifecycle_executes_real_sample_app_payload_across_generations` | Co-root `nlos-package` (`keygen`/`build`/`verify`) + `sample-app-driver` (`cmd_install` -> `cmd_run` [gen 1 payload + fiber commit] -> `cmd_payload` replay -> `assets/config.bin` refusal -> `cmd_update` [W29-E migration to `v1.1.0`] -> `cmd_payload` [gen 2 fresh execution] -> `cmd_uninstall` -> post-uninstall `cmd_run`/`cmd_payload` fail-closed) |

---

## 4. Honest Boundaries & Non-Claims

- **Deterministic Driver-Face Payload Lane vs. Guest Sandbox VM**:
  `execute_application_payload` (invoked by both `nlos-package run` and `sample-app-driver run`/`payload`) executes the materialized `app.bin` bytes through the kernel's durable `Operation` lifecycle (`nlos-driver-mock` `register -> dispatch -> complete`), content-digest verification, and `SemanticWriter` assertion bridge. It does **not** claim to be a guest Wasm, ELF, or native OS sandbox instruction VM executing arbitrary guest machine code.
- **Post-Verify Entry Role Persistence**:
  `PackageVerificationReceipt` and `InstallationReceipt` durably bind the signed `manifest_digest` rather than persisting a separate post-verify `PackageEntryRole` table ([`crates/nlos-slice-k/src/payload.rs`](../../../crates/nlos-slice-k/src/payload.rs)). `nlos-package install` and `update` parse the `.nlospkg` envelope to display `executables sample-driver`; at run time, `execute_application_payload` resolves `derive_artifact_id(package_id, package_version, entry_name)`, failing closed on raw asset paths (`assets/config.bin`, `app.bin`), undeclared entry names, unmaterialized artifacts, or uninstalled applications.
- **Cross-Platform OS Stand-In Teardown**:
  In `sample-app-driver`, the `background-service` stand-in spawns a real `sleep 600` child process terminated by `PosixPlatformKillAdapter` (`SIGTERM`) on Unix (`#[cfg(unix)]`), and exercises `NoopPlatformKillAdapter` on non-Unix hosts (`#[cfg(not(unix))]`, e.g. Windows) so automated tests execute deterministically across platforms without self-signaling.
- **Signing Key Custody**:
  `nlos-package keygen` / `build` / `verify` uses developer `.devkey` files for local packaging and cryptographic consistency checks; non-exportable KEK-sealed production signing key custody is governed separately by [`C-CUSTODY-001`](./c-custody-001-key-custody-gateway-reconciliation.md) (`#17 C-CUSTODY-GATEWAY`).
