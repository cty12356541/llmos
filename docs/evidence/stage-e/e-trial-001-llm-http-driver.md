# E-TRIAL-001: Bounded Local HTTP/1.1 KeyPool LLM Driver Plane, 5D Token Budget Enforcement Shim, and NL Intent Guardrail Evidence (`W77-PRE-B` / `W77-PRE-D` / `W77-PRE-INT`)

- **Wave / Lane**: `W77-PRE` / Lane B (`W77-PRE-B`) + End-to-End Wiring (`W77-PRE-D`) & Wave Barrier Live Smoke (`W77-PRE-INT` - `E0` `E-TRIAL-BOOTSTRAP`)
- **Assurance Level**: `H4` (Deterministic Local Mock HTTP/1.1 + Durable `SQLite` `ResourceAuthority` Closed-Loop Integration Suite + Sanitized Local `127.0.0.1:8317` `glm-keypool` End-to-End Smoke Verification)
- **Status**: `DONE`
- **Base / Wave Commit Stack**: `f6b94f5` -> `1aba801` (`W77-PRE-B`) -> `a66e916` (`W77-PRE-D`) -> `e481bed` / `ccf119e` (`W77-PRE` CoT inclusive token conservation & truncation guardrail calibration)
- **Authority Specs**:
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) section 6, section 9.2, section 15.1, section 20 (`KABI-SVC-001`, `DRV-CRED-001`, `DRV-SHIM-001`, `DRV-PROTO-001`, `TCB-EGRESS-001`, `[NLOS-RESOURCE-001]`, `[BUD-INT-001]`, `[BUD-ARITH-001]`, `[TASK-PROVIDER-001]`, `[NL-AMBIG-001]`)
  - [ADR-0024 DriverProvider Payload Codec Freeze](../../management/adrs/0024-driver-provider-payload-codec-freeze.md)
  - [ADR-0027 Stage E SABI Long-Term Compatibility and Store Governance](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) section 2.2
  - [stage-e-progress.md](../../management/stage-e-progress.md) section E.2.2 (`E0`), section E.3.2 (`W77-PRE-B`)

---

## 1. Exclusive Write-Set Summary

1. [Cargo.toml](../../../crates/nlos-driver-llm-http/Cargo.toml) - Added `[[bin]] name = "nlos-llm-pool-cli"` using only existing workspace dependencies (`Cargo.lock` untouched).
2. [lib.rs](../../../crates/nlos-driver-llm-http/src/lib.rs) and [error.rs](../../../crates/nlos-driver-llm-http/src/error.rs) - `#![forbid(unsafe_code)]` public driver API and zero-credential-leak `LlmDriverError`.
3. [json.rs](../../../crates/nlos-driver-llm-http/src/json.rs) - Zero-dependency bounded RFC 8259 JSON parser and serializer (`MAX_JSON_DEPTH = 64`), supporting `UTF-8`, standard escapes, `\uXXXX` with `UTF-16` surrogate pair decoding, and exact 64-bit `u64`/`i64` integer preservation alongside finite `f64`.
4. [http.rs](../../../crates/nlos-driver-llm-http/src/http.rs) - Pure safe Rust `HTTP/1.1` client over `tokio::net::TcpStream` with `tokio::time::timeout` deadline enforcement, header `CR`/`LF` injection defense, case-insensitive response headers, and `Transfer-Encoding: chunked`, `Content-Length`, and EOF-close body framing.
5. [keypool.rs](../../../crates/nlos-driver-llm-http/src/keypool.rs) - `SecretCredential` (`Debug`/`Display` unconditionally emit `"<redacted-credential>"`), 4-tier `discover_local_keypool` (`override_path` -> `LLMOS_KEYPOOL_CONFIG` -> `LLMOS_LLM_BASE_URL` -> `$USERPROFILE`/`$HOME` `~/glm-keypool/config.json`), `glm-keypool` v2 `config.json` parser (`bigmodel.cn` automatic derivation to `/api/coding/paas/v4`, `0.0.0.0`/`::` -> `127.0.0.1` normalization, `allowModels` aggregation, key slot counting without plaintext retention), and in-process `DirectKeyPoolScheduler` (`sticky` `SHA-256` session affinity + `round-robin` + automatic `429`/`401`/`403`/`5xx` cooldown and failover replay).
6. [shim.rs](../../../crates/nlos-driver-llm-http/src/shim.rs) - Trusted `LlmEnforcementShim` implementing `probe_pool_status` (100% redacting `keys[*]` masks and owner names), `execute_metered_chat` (pre-dispatch fail-closed 5D `ResourceAuthority` budget gate, `Authorization: Bearer` + `x-pool-session` sticky prefix-cache header injection, OpenAI/BigModel inclusive `reasoning_tokens` normalization so `prompt_tokens + completion_tokens + reasoning_tokens == total_tokens` strictly holds alongside `UTF-8` byte fallback `>= 1`, 16-hex `SHA-256` `served_slot_fingerprint` redaction, post-call `consume_llm_tokens` durable `LlmTokenConsumptionReceipt`, and mandatory `PROVIDER_ISOLATION_UNVERIFIED` tagging), and `ensure_default_trial_llm_budget`.
7. [intent.rs](../../../crates/nlos-driver-llm-http/src/intent.rs) - `compile_nl_intent` / `compile_nl_intent_with_model` selecting `fast_intent_model` (`glm-5.3-flash`) with `DEFAULT_INTENT_MAX_TOKENS = 2048`, stripping Markdown ```` ```json ... ``` ```` code fences, recovering embedded JSON from `reasoning_content` when visible `content` is empty under chain-of-thought truncation, and enforcing `[NL-AMBIG-001]` mandatory `IRREVERSIBLE` + `needs_clarification = true` fail-safe escalation (preserving the already-charged `LlmTokenConsumptionReceipt`) whenever the raw query contains destructive keywords (`删除`, `卸载`, `清空`, `delete`, `wipe`, `uninstall`).
8. [nlos-llm-pool-cli.rs](../../../crates/nlos-driver-llm-http/src/bin/nlos-llm-pool-cli.rs) - Operational CLI supporting `status`, `chat`, and `intent` subcommands with optional `--root <STATE_DIR>` durable `ResourceAuthority` 5D token accounting and bounded `240`-character `reasoning_excerpt` diagnostics when small `--max-tokens` truncates inside `reasoning_content`.
9. [keypool_and_budget_gate.rs](../../../crates/nlos-driver-llm-http/tests/keypool_and_budget_gate.rs) - Deterministic closed-loop integration suite (`9` integration tests) over ephemeral `127.0.0.1:0` mock `HTTP/1.1` servers and temporary `SQLite` stores (zero host port `8317` or host user config dependencies).

---

## 2. Requirement and Invariant Matrix

| Requirement / Invariant ID | Verification Target | Evidence Location | Result |
| :--- | :--- | :--- | :--- |
| `KABI-SVC-001` & `DRV-PROTO-001` | Zero provider-specific syscalls added to kernel crates; `OpenAI`-compatible / `glm-keypool` HTTP/1.1 protocol encapsulated purely in user-space `nlos-driver-llm-http` | `crates/nlos-driver-llm-http/src/{http,shim}.rs` | PASS |
| `DRV-CRED-001` & `[PD-COMMIT-004]` | `SecretCredential` prints `"<redacted-credential>"` in `Debug`/`Display`; `/__pool__/status` strips `keys[*]` masks and human names; `x-pool-key` hashed to 16-hex `SHA-256` `served_slot_fingerprint`; zero real keys or private host paths in code/tests/docs | `crates/nlos-driver-llm-http/src/{keypool,shim}.rs`, `tests/keypool_and_budget_gate.rs` | PASS |
| `glm-keypool` v2 Endpoint Derivation | `upstreamBase` containing `bigmodel.cn` automatically derives `/api/coding/paas/v4` (avoiding `/api/paas/v4` `1113` billing mismatch while unifying `xai` group rewrite); `0.0.0.0`/`::` normalizes to `127.0.0.1`; `allowModels` selects `glm-5.3` and `glm-5.3-flash` | `crates/nlos-driver-llm-http/src/keypool.rs`, `tests/keypool_and_budget_gate.rs` | PASS |
| `[NLOS-RESOURCE-001]` / `[BUD-INT-001]` / `[BUD-ARITH-001]` | Pre-dispatch budget gate checks `remaining_api_calls() > 0 && remaining_total_tokens() > 0` and fails closed (`LlmDriverError::BudgetExhausted`) with zero network traffic when exhausted; post-call `consume_llm_tokens` commits exact `prompt_tokens`, `completion_tokens`, `reasoning_tokens`, and `api_calls = 1` | `crates/nlos-driver-llm-http/src/shim.rs`, `tests/keypool_and_budget_gate.rs` | PASS |
| `ADR-0024` `[TASK-PROVIDER-001]` | Every metered chat and compiled intent outcome carries `isolation_tag = "PROVIDER_ISOLATION_UNVERIFIED"` (`nlos_driver_mock::PROVIDER_ISOLATION_UNVERIFIED_TAG`) | `crates/nlos-driver-llm-http/src/{shim,intent}.rs`, `tests/keypool_and_budget_gate.rs` | PASS |
| `[NL-AMBIG-001]` Destructive Intent Guardrail | Destructive keywords (`删除`/`卸载`/`清空`/`delete`/`wipe`/`uninstall`) in raw query force `undo_class = IRREVERSIBLE` and `needs_clarification = true` even when model output claims `LOSSLESS` | `crates/nlos-driver-llm-http/src/intent.rs`, `tests/keypool_and_budget_gate.rs` | PASS |

---

## 3. Verification Gate Outputs

### 3.1 Formatting Gate

Command:

```text
cargo fmt -p nlos-driver-llm-http -- --check
```

Result: exit code `0` (zero formatting diffs).

### 3.2 Lint and Pedantic Clippy Gate

Command:

```text
cargo clippy --locked -p nlos-driver-llm-http --all-targets -- -D warnings
```

Result: exit code `0` (`lib`, `bin/nlos-llm-pool-cli`, unit tests, and integration tests compiled with zero warnings under `#![forbid(unsafe_code)]`, `clippy::all`, and `clippy::pedantic`).

### 3.3 Unit and Deterministic Closed-Loop Integration Test Gate

Command:

```text
cargo test --locked -p nlos-driver-llm-http
```

Output summary (paths normalized to `<WORKSPACE_ROOT>` per `[PD-COMMIT-004]`):

```text
running 4 tests (src/lib.rs)
test http::tests::parses_valid_http_urls_and_rejects_https_or_crlf ... ok
test json::tests::rejects_lone_surrogates_control_chars_and_leading_zeros ... ok
test tests::scaffold_crate_name_matches_manifest ... ok
test json::tests::round_trips_utf8_escapes_surrogate_pairs_and_numbers ... ok
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

running 0 tests (src/bin/nlos-llm-pool-cli.rs)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

running 9 tests (tests/keypool_and_budget_gate.rs)
test glm_keypool_v2_config_derivation_and_credential_redaction ... ok
test discover_local_keypool_four_tier_priority_order ... ok
test pool_status_probe_parses_chunked_transfer_and_redacts_individual_key_labels ... ok
test compile_nl_intent_extracts_fenced_json_and_escalates_destructive_queries ... ok
test execute_metered_chat_charges_sqlite_resource_authority_and_fails_closed_when_exhausted ... ok
test direct_keypool_scheduler_sticky_affinity_and_429_cooldown_failover ... ok
test cli_binary_executes_metered_chat_end_to_end_with_redacted_output ... ok
test inclusive_reasoning_tokens_conserves_total_and_cli_reports_cot_truncation ... ok
test compile_nl_intent_recovers_or_fails_safe_on_cot_truncated_empty_content ... ok
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

Doc-tests nlos_driver_llm_http
running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Total passing tests in `nlos-driver-llm-http`: `13` (`4` unit + `9` closed-loop integration + `0` doc-tests), `0` failed, zero new external third-party dependencies.

## 3.4 W77-PRE-D End-to-End Wiring and W77-PRE-INT Live Local KeyPool Smoke Verification

### 3.4.1 Cross-Crate CLI, Tauri 5D Budget Bridge, and Omnibar Assist Verification (`W77-PRE-D`)

1. **`crates/nlos-up` One-Command KeyPool Subcommands (`pool-status | chat | intent`)**:
   - Wired [crates/nlos-up/src/lib.rs](../../../crates/nlos-up/src/lib.rs) and [crates/nlos-up/src/main.rs](../../../crates/nlos-up/src/main.rs) to bind `<STATE_DIR>/trial-account.txt` and `<STATE_DIR>/resource` (`ResourceAuthority` v8 five-dimension `LlmTokenBudget`) directly to `LlmEnforcementShim`.
   - Verification: `cargo test --locked -p nlos-up` (`11/11` passed) and `cargo clippy --locked -p nlos-up --all-targets -- -D warnings` (exit code `0`, `ccf119e`).
2. **Desktop Tauri 2 Host KeyPool Bridge (`desktop/src-tauri/src/keypool_bridge.rs`)**:
   - Registered `llm_keypool_inspect_status`, `llm_keypool_assist_intent`, and `llm_keypool_chat` in [desktop/src-tauri/src/keypool_bridge.rs](../../../desktop/src-tauri/src/keypool_bridge.rs) and [desktop/src-tauri/src/lib.rs](../../../desktop/src-tauri/src/lib.rs), executing blocking `SQLite` + local HTTP calls off the UI main thread, enforcing `[DUI-OFFLINE-001]` deterministic local fallback (`LOCAL_DETERMINISTIC` / `LOCAL_FALLBACK`) when unconfigured or offline, and enforcing `[NL-AMBIG-001]` `IRREVERSIBLE` escalation across both live and fallback paths.
   - Verification: `cargo test --manifest-path desktop/src-tauri/Cargo.toml --test keypool_bridge_side` (`4/4` passed) and `cargo test --manifest-path desktop/src-tauri/Cargo.toml --lib --features dev-fixture` (`21/21` passed).
3. **Desktop `Z3` Omnibar Status Bar and Non-Authoritative Model Assist Card (`desktop/src/views/omnibar`)**:
   - Wired solid `4px`-radius local model pool status bar (`本地模型池 · 在线` / `Local Model Pool · Online` vs `离线回退` / `Local Fallback`) and non-authoritative `模型辅助拆解` / `Model Assist` breakdown card in [palette-view.ts](../../../desktop/src/views/omnibar/palette-view.ts), preserving `L1/L2` zero internal architecture jargon and gating `PROVIDER_ISOLATION_UNVERIFIED` + redacted `served_slot_fingerprint` strictly behind `L3` disclosure.
   - Verification: `omnibar-multimodal.test.ts` Suites `1..10` (`726` assertions passed) and `tsc && vite build` exit code `0`.

### 3.4.2 Sanitized 5-Step Live Local `glm-keypool` (`127.0.0.1:8317`) Smoke Receipt (`W77-PRE-INT`)

Executed against a live local `http://127.0.0.1:8317` `glm-keypool` instance (`glm-5.3` / `glm-5.3-flash`, `40/44` active key slots) using an isolated trial state root (`<TRIAL_ROOT>`), confirming monotonic `ResourceAuthority` 5D `LlmTokenConsumptionReceipt` accounting (`sequence: 1 -> 2 -> 3`, `cumulative_total_tokens: 668 -> 986 -> 1714`) and 100% redaction of raw keys, slot nicknames, and host user directory paths:

| Step | Executed Command | Verified Sanitized Output & Durable Receipt Summary |
| :--- | :--- | :--- |
| **1** | `cargo run -p nlos-up -- pool-status --require-live` | `NLOS_UP_POOL_STATUS configured=true reachable=true`; `discovery_source = user-home:~/glm-keypool/config.json`; `openai_base_url = http://127.0.0.1:8317/api/coding/paas/v4`; `credential_present = true [REDACTED]`; `pool_keys = 40/44`; `isolation_tag = PROVIDER_ISOLATION_UNVERIFIED` |
| **2** | `cargo run -p nlos-up -- chat --root <TRIAL_ROOT> --model glm-5.3-flash --max-tokens 1024 --prompt "..."` | Live Chinese summary of NLOS microkernel & Capability security returned; `served_slot_fingerprint = 2d5a1c732e7e3d86` (16-hex `SHA-256` redacted); `prompt_tokens=30 + completion_tokens=60 + reasoning_tokens=578 = total_tokens=668`; durable receipt `sequence=1`, `cumulative_total_tokens=668`, `cumulative_api_calls=1` |
| **3** | `cargo run -p nlos-up -- intent --root <TRIAL_ROOT> --query "查看当前工作区正在运行的任务与剩余模型额度"` | Read-only structured intent compiled: `target_domain="workspace"`, `undo_class="LOSSLESS"`, `needs_clarification=false`, `safety_escalated=false`; `prompt_tokens=131 + completion_tokens=71 + reasoning_tokens=116 = total_tokens=318`; durable receipt `sequence=2`, `cumulative_total_tokens=986`, `cumulative_api_calls=2` |
| **4** | `cargo run -p nlos-up -- intent --root <TRIAL_ROOT> --query "卸载全部第三方应用并清空缓存"` | Destructive query guardrail triggered (`[NL-AMBIG-001]`): `target_domain="application"`, `undo_class="IRREVERSIBLE"`, `needs_clarification=true`, `safety_escalated=true`; `prompt_tokens=129 + completion_tokens=92 + reasoning_tokens=507 = total_tokens=728`; durable receipt `sequence=3`, `cumulative_total_tokens=1714`, `cumulative_api_calls=3` |
| **5** | `cargo run -p nlos-driver-llm-http --bin nlos-llm-pool-cli -- status` | `discovery_source: user-home:~/glm-keypool/config.json`; `online_pool_status: REACHABLE`; `egress_auth: configured (<redacted-credential>)` |

---

## 4. Residual Scope and Honest Boundaries

- **Unverified External/Local HTTP Provider Isolation (`ADR-0024` / `[TASK-PROVIDER-001]`)**: External and local HTTP LLM endpoints (including `127.0.0.1:8317` `glm-keypool`, Ollama, and OpenAI-compatible gateways) do not provide hardware enclave attestation or cryptographic zero-data-retention proofs; every status, chat, and compiled intent response honestly stamps `PROVIDER_ISOLATION_UNVERIFIED`.
- **`W77-PRE` (`E0`) Wave Closure vs. Stage E Mainline (`E1`..`E7`)**: Root workspace integration across `crates/nlos-up`, `crates/nlos-driver-llm-http`, `desktop/src-tauri/src/keypool_bridge.rs`, `desktop/src/views/omnibar`, and `examples/starter-assistant-app` ([E-TRIAL-002](./e-trial-002-quickstart-and-starter-app.md)) is complete at wave barrier `W77-PRE-INT` (`M-E0.5` `DONE` at `H4`). Stage E core compatibility and store governance work packages (`E1`..`E7`, `W77`..`W81`) remain `NOT_STARTED` (`DESIGN`), and commercial OS installer notarization remains explicitly bounded under `E-deferred` / `RISK-B-06`.
