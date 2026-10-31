# llmos — Natural Language Operating System

> **Managing LLM Agents the way Linux manages processes.**

**English** | [中文](./README.zh-CN.md)

llmos is a microkernel, distributed Cell fabric, and natural-language workstation platform written in Rust that brings operating system-grade management to LLM Agents: capability-based security, 6-dimensional resource budgets, crash recovery with durable authority stores, signed content-addressed artifacts, cross-host mTLS Cell federation, and a five-plane Trusted UI workstation desktop.

---

## Why does this exist?

Most Agent frameworks today assume a friendly environment: a handful of trusted agents, unconstrained budgets, and no hard isolation boundary. That works for prototypes.

It breaks down in multi-tenant, long-running environments where untrusted or semi-trusted agents operate under distinct privilege boundaries, strict token/compute budgets, and partial network or host failures.

llmos asks the foundational systems question:

> *If agents are processes, what is the operating system?*

| What multi-agent systems need | LangChain / AutoGen | Docker + K8s | **llmos** |
|---|---|---|---|
| Per-agent capability tokens | No | No | Yes — Ed25519 signed, attenuatable, revocable |
| Token / API budget enforcement | No | No | Yes — 6-dimensional resource conservation |
| Crash recovery with durable state | No | Container restart | Yes — authority-owned SQLite WAL+FULL stores |
| Signed, content-addressed outputs | No | No | Yes — Artifact store with Ed25519 receipts |
| Agent & UI isolation | No | Process / namespace | Yes — Capability + Principal + Cell + Z4 Trusted UI |
| Distributed federation | External infra | K8s cluster | Yes — single-host & cross-host mTLS Cells + 8-step migration |

---

## Architecture in one picture

```text
┌───────────────────────────────────────────────────────────┐
│   NL Workstation Desktop (Stage D: Z0-Z4 Tauri 2 Shell)   │
│   L0-L3 Progressive Disclosure · 12-Domain Control Parity │
├───────────────────────────────────────────────────────────┤
│                 Applications & Agent SDKs                 │
├─────────────────────────┬─────────────────────────────────┤
│   SABI (Service API)    │  21 service families            │
│   Task · Artifact       │  Semantic · Resource            │
│   Package · Search      │  Notify · Channel · Topic       │
├─────────────────────────┴─────────────────────────────────┤
│   KABI (Kernel API)     │  27 entries                     │
│   proc_spawn/exit       │  budget_query/transfer          │
│   io_submit/poll        │  meter_reserve/consume          │
├───────────────────────────────────────────────────────────┤
│            NLOS Microkernel & Distributed Fabric          │
│   Identity · Capability · Clock · Cell · Net · Consensus  │
│   (every authority backed by SQLite WAL+FULL)             │
└───────────────────────────────────────────────────────────┘
             <-> Host OS (Windows / macOS / Linux)
```

**Execution hierarchy:**

```text
Application ⊇ Process ⊇ AgentInstance ⊇ ExecutionFiber
```

Each level has its own durable authority store, monotonic generation fences, and replay-safe lifecycle state machine.

---

## Key design decisions

### Capability-based security
Every privileged operation requires an Ed25519-signed, generation-checked Capability token rather than an ambient API key. Tokens can be attenuated (narrowed) and revoked across the hierarchy; no agent can escalate its own permissions. Cross-process and GUI control flows traverse the single authenticated control spine ([ADR-0011](docs/management/adrs/0011-ipc-principal-auth-signature-passthrough.md)).

### Durable authority stores
Each subsystem (`Identity`, `Capability`, `Task`, `Process`, `Resource`, `Artifact`, `Semantic`, `Application`, `Plan`, `Cell`, `Lease`…) owns an independent SQLite database configured with `WAL + synchronous=FULL`. State transitions are idempotent and guarded by compare-and-swap generation fences so crashes do not corrupt authority state.

### Resource conservation
Six resource demand families — `Fungible · CPU · Capacity · Rate · CumulativeMeter · Exclusive` — follow the `Quote → Reserve → Consume → Finalize` conservation lifecycle, extended across distributed Cells via durable `QuotaLease`, `CapacityLease`, and `ExclusiveDeviceLease` reconciliation.

### Signed evidence chain
Packages are cryptographically signed by their publisher; Artifacts and cross-domain event sets are content-addressed and bound to authority-signed receipts. Claims in this repository are tracked against machine-checked evidence levels (`H0–H8`) in `docs/management/claims.yaml`.

### Cell-based distribution & cross-host consensus
One OS process hosts one Cell. Cells federate both on a single machine ([ADR-0018](docs/management/adrs/0018-single-host-multiprocess-dual-cell-topology.md), [ADR-0019](docs/management/adrs/0019-cross-cell-commit-semantics-extension.md)) and across physical hosts via TLS 1.3 mTLS with 5-axis TLS Exporter `CellFence` handshakes, conservative clock-skew lease envelopes, 8-step process migration, and an embedded deterministic Raft control-plane quorum that keeps 0 Raft RPCs on the data plane and zero-config passthrough at `N = 1` ([ADR-0020](docs/management/adrs/0020-control-plane-single-writer-and-consensus-timing.md), [ADR-0026](docs/management/adrs/0026-cross-physical-host-transport-and-consensus-expansion.md)).

### Neutral industrial workstation & Trusted UI
The Tauri 2 desktop ([ADR-0025](docs/management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md)) isolates five compositor planes (`Z0–Z4`), exposes `L0–L3` progressive disclosure and 12-domain inspect/control symmetry (`[UI-PARITY-001]`), and enforces an unoccludable `Z4` solid security consent screen for high-impact or irreversible actions.

---

## Current status

| Stage | Scope | Status |
|---|---|---|
| **Stage A** | Bootstrap microkernel | `EXITED` |
| **Stage B** | Single-machine application platform ([`stage-b-progress.md`](docs/management/stage-b-progress.md)) | `EXITED` (2026-09-21) |
| **Stage C** | Distributed modern system: single-host multi-process Cells + `C-XHOST` cross-physical-host mTLS, clock-skew lease envelope & embedded Raft ([`stage-c-progress.md`](docs/management/stage-c-progress.md)) | `EXITED` (2026-10-29 `W76`) |
| **Stage D** | Complete NL desktop: five-plane `Z0–Z4` Tauri 2 industrial workstation, `L0–L3` progressive disclosure, 12-domain inspect/control symmetry & `Z4` security consent screen ([`stage-d-progress.md`](docs/management/stage-d-progress.md)) | `EXITED` (2026-10-29 `W76`) |
| **Stage E** | Compatibility & ecosystem: `SABI v1.0` long-term compatibility policy, out-of-core compatibility shim, store governance & `E0` out-of-the-box onboarding ([`stage-e-progress.md`](docs/management/stage-e-progress.md), [`ADR-0027`](docs/management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md)) | `IN_PROGRESS (E0–E5 Verified H4; ADR-0027 ACCEPTED; E6–E7 DESIGN)` (2026-10-31 `W79`) |

> **Evidence & boundary discipline:** Stages B, C, and D exited under explicit `SATISFIED-WITH-BOUNDARIES` reviews ([W67](docs/evidence/stage-c/reviews/w67-stage-c-exit-gates.md), [W73](docs/evidence/stage-c/reviews/w73-stage-c-xhost-exit-review.md), [W74](docs/evidence/stage-d/reviews/w74-stage-d-exit-gates.md)) with `P0 = 0`. In Stage E, work packages `E0` (`W77-PRE` / `M-E0.5`: `nlos-up`, user-space `nlos-driver-llm-http` stamped `PROVIDER_ISOLATION_UNVERIFIED`, `starter-assistant-app`, and desktop KeyPool bridge; [E-TRIAL-001](docs/evidence/stage-e/e-trial-001-llm-http-driver.md), [E-TRIAL-002](docs/evidence/stage-e/e-trial-002-quickstart-and-starter-app.md)), `E1` (`W77` / `M-E1`: `9/9` SABI channels `frozen = true`, `N / N-1 / N-2` compatibility matrix, `deprecation-manifest.yaml` warning telemetry, and 5-language `v1.0` immutable golden wire corpus; [E-SABI-001](docs/evidence/stage-e/e-sabi-001-version-matrix-and-deprecation.md), [E-SABI-002](docs/evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md)), `E2` (`W78` / `M-E2`: out-of-core unprivileged `nlos-compat-shim` for `N-1 / N-2 <-> N` SABI & legacy `minicbor <-> DriverProvider` translation with `[DRV-SHIM-001]` 4-layer zero-secret isolation; [E-SHIM-001](docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md)), `E3` (`W78` / `M-E2`: `nlos-migrate` static manifest/deprecation checker, `tools/sabi-compat-diff`, and `[PKG-UPDATE-001]` 3-mode container migration & atomic rollback sandbox; [E-MIGRATE-001](docs/evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md)), `E4` (`W79` / `M-E3`: `nlos-publisher-trust` 3-tier Ed25519 PKI, monotonic `RevocationEpoch` CRL, `ApplicationId` anti-hijack ownership ledger, and `nlos-plan` 7-class ecosystem Typed Selector pinning + G3 `WaitingAuthorization` conditioned materialization gate closing `U-7` / `U-8`; [E-TRUST-001](docs/evidence/stage-e/e-trust-001-publisher-pki-revocation-and-selectors.md)), and `E5` (`W79` / `M-E3`: `nlos-store-registry` OCI CAS governance catalog, opaque `EntitlementToken` redemption & 3-tier `RecallAdvisory` revocation cascade, plus `nlos-certify` 4-gate automated certification pipeline & pure-Rust SPDX/lockfile supply-chain auditor advancing `RISK-B-09` `open -> partial`; [E-STORE-001](docs/evidence/stage-e/e-store-001-oci-index-and-entitlement-ledger.md), [E-CERTIFY-001](docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md)) are verified at `H4` with [ADR-0027](docs/management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) in `ACCEPTED`; downstream dual-layer Wasm sandbox, reference marketplace bundles, and `[ROAD-E-001]` upgrade gate work packages `E6`–`E7` remain in `DESIGN` planning with zero runtime claims. Hardware power-loss bench (`U-4`), `1M_LOGICAL` bare-metal WAN cluster (`U-1`), remote physical side-effect erasure (`U-5`), third-party Wasm sandbox production wiring (`U-9`), `SABI v1.0` permanent freeze sign-off (`RISK-B-05`), OS commercial installer notarization (`RISK-B-06`), and periodic upstream supply-chain review (`RISK-B-09`) remain explicitly bounded in [`docs/management/exit-unknown-risks.md`](docs/management/exit-unknown-risks.md).

### Stage C verified work packages (`C-core` + `C-XHOST`)

| Package | Status | Verified scope summary |
|---|---|---|
| `C-CELL` — Cell authority & federation | `Verified` | 7-piece local Cell assembly, federation directory/intent/checkpoint, `PartitionAutonomyGate` |
| `C-LEASE` — Quota / Capacity / Device leases | `Verified` | 3-family durable `SQLite WAL+FULL` state machines, fencing & reconciliation receipts |
| `C-SYNC` — Event & artifact sync | `Verified` | Signed event-set merge, vector checkpoints, content-addressed `ArtifactTransferPackage` |
| `C-SHARD` — TaskAuthority sharding | `Verified` | Schema v47 3-record sharding, 6-endpoint takeover barrier, production takeover binding |
| `C-FANOUT` — Hierarchical fanout & bulkhead | `Verified` | Bounded fanout tree, Merkle result root, 6-dim admission backpressure, 4-domain bulkhead |
| `C-MIGRATE` — Process migration protocol | `Verified` | Ordered 8-step migration state machine, cross-`CellHost` dual-process conservation |
| `C-SCHED` — Three-tier scheduler | `Verified` | Lock-free worker hot path, bounded DRR, lease-safe work stealing & reverse backpressure |
| `C-PARTITION` — Partition tolerance | `Verified` | 4-stage reconnection convergence, `PartitionBoundedLossProof` conservation accounting |
| `C-BENCH` — Single-host sharded benchmarks | `Verified` | 4.44x scaling (1 to 8 process Cells, `--release`), `10K/100K_LOGICAL` `WAL+FULL`, 5 fault classes |
| `C-XHOST` (`001`–`005`) — Cross-physical-host fabric ([ADR-0026](docs/management/adrs/0026-cross-physical-host-transport-and-consensus-expansion.md)) | `Verified` | `nlos-net` TLS 1.3 mTLS + 5-axis `CellFence`, conservative clock-skew lease envelope, `nlos-consensus` embedded Raft control plane (`N=1` passthrough), streaming cross-host migration, `ChaosTcpProxy` `H6` multi-machine gates |

### Stage D verified work packages (`D-core` + `[ROAD-D-001]`)

| Package | Status | Verified scope summary |
|---|---|---|
| `D-TOKENS-KIT` — Workstation tokens & SOTA UI kit | `Verified` | Solid `Graphite` / `Paper` themes (zero blur/glow/emoji), Nomad/Temporal/Headlamp/Terraform primitives |
| `D-IPC-ENUM` — 12-domain enumeration & symmetry | `Verified` | `IPC-D1..D8` bounded cursor pagination (`limit <= 64`), typed `ControlScope`, blast-radius preview, statutory immutable locks |
| `D-COMPOSITOR-WM` — Five-plane `Z0–Z4` compositor & WM | `Verified` | 5-mode tiling-first window manager, `36px` bottom taskbar, `L0–L3` disclosure store, generation-fenced input routing |
| `D-OMNIBAR-MULTIMODAL` — On-demand intent palette | `Verified` | `Ctrl+Space` `Z3` command/intent palette, `TypedIntent` preview, `[NL-AMBIG-001]` clarification gate, voice & attachment tags |
| `D-TRUSTED-UI` — `Z4` consent, chooser & clipboard gate | `Verified` | UAC/Polkit-grade `Z4` solid security screen (`350ms` anti-hijack, `IRREVERSIBLE` notice), `ChooserGrant`, taint-aware clipboard/drag-drop |
| `D-TASK-SPACE-DAG` — `L1–L3` Task Space & DAG editor | `Verified` | `L1` 16-state commitment table, `L2` single-row attribution drawer, `L3` orthogonal `TaskPlan` DAG editor & `TaskAttempt` lanes |
| `D-NOTIFY-SEARCH-OFFLINE` — Notifications, Spotlight & offline | `Verified` | One-time SHA-256 `ActionToken` notifications, `Ctrl+K` `0x1F` non-authoritative search, `[DUI-OFFLINE-001]` local offline autonomy |
| `D-CONTROL-SETTINGS-A11Y` & `[ROAD-D-001]` — Control Center, settings, a11y & E2E gate | `Verified` | 12-domain `SystemControlCenter` (108 Tauri commands), 4-tier `SettingsDomain` CAS, `KeychainReference`, `WCAG 2.2 AA`, `zh-CN / en-US` packs, 1,000-round zero-leak campaign & non-developer multi-app E2E gate |

---

## Repository layout

```text
crates/              44 Rust workspace library/binary crates (root Cargo.toml)
  nlos-types/           Nominal identity types (zero dependencies)
  nlos-identity/        Principal & key authority (Ed25519)
  nlos-capability/      Capability issuance, attenuation, revocation & KEK custody
  nlos-task/            Task authority (CommitPermit, EffectPermit, sharded takeover)
  nlos-process/         Process / AgentInstance lifecycle & 8-step migration
  nlos-artifact/        Content-addressed artifact store & sync policy
  nlos-semantic/        Semantic assertion admission & signed vector checkpoints
  nlos-cell/            Cell authority, 7-piece local assembly & partition gate (Stage C)
  nlos-lease/           Durable Quota / Capacity / ExclusiveDevice lease authority (Stage C)
  nlos-net/             Cross-host TLS 1.3 mTLS transport & 5-axis CellFence (Stage C C-XHOST)
  nlos-consensus/       Embedded deterministic Raft control-plane quorum (Stage C C-XHOST)
  nlos-system-control/  Resident system-control-daemon & system-control-cli (12-domain spine)
  nlos-slice-k/         End-to-end integration slice (Package → Task → Fiber → Artifact)
  nlos-package/         CLI: keygen · build · conformance · verify · install · run · update · uninstall
  nlos-driver-llm-http/ User-space KeyPool/OpenAI HTTP/1.1 LLM driver & 5D LlmEnforcementShim (Stage E E0)
  nlos-up/              One-command cross-platform trial bootstrapper, KeyPool CLI & desktop launcher (Stage E E0)
  nlos-compat-shim/     Out-of-core unprivileged SABI epoch & DriverProvider protocol shim (Stage E E2)
  nlos-migrate/         Static contract/deprecation checker & 3-mode migration rollback sandbox (Stage E E3)
  nlos-publisher-trust/ 3-tier Ed25519 publisher PKI, RevocationEpoch CRL & AppId ownership (Stage E E4)
  nlos-store-registry/  OCI CAS store catalog, entitlement redemption & 3-tier recall ledger (Stage E E5)
  nlos-certify/         4-gate store certification pipeline & pure-Rust SPDX/lockfile auditor (Stage E E5)
  ... (23 more workspace crates)

desktop/             Five-plane Z0-Z4 Tauri 2 workstation desktop (TypeScript + independent Rust workspace)
examples/
  sample-app/            Sample signed NLOS application payload & lifecycle fixture (independent package)
  starter-assistant-app/ Reference productivity bundle (v1.0.0/v1.1.0 TaskPlan + 5D LlmTokenBudget + Artifact lineage)
docs/
  design/            Canonical architecture specification (v0.5, 278 KB, 200+ requirement IDs)
  discussions/       36 design discussion documents (index + 35 numbered topics)
  management/
    adrs/            27 Architecture Decision Records (ADR-0001..0027, plus template)
    evidence-index.yaml  197 evidence entries (H0-H8 assurance levels)
    claims.yaml      116 machine-readable claims (ledger-schema v1.3: 71 Stage B + 35 Stage C + 10 Stage D)
sdk/                 Python · TypeScript · Go · C# SABI SDKs
schema/              Protobuf + CDDL schemas + golden wire test vectors
experiments/         8 Python/Rust design experiments
fuzz/                3 cargo-fuzz targets with seed corpora (excluded from root workspace)
```

---

## Building & Quick Trial

**Prerequisites:** Rust (pinned via `rust-toolchain.toml`, MSRV `1.97`), Git. Optional for desktop/SDK trials: Node.js `>= 20` (Tauri 2 desktop), Python `>= 3.11` (`nlos-sdk`).

**Platform support:** Windows, macOS, and Linux (verified across all three in CI).

### 0. One-Command Bootstrap & Local KeyPool Model Trial (`nlos-up`)

If you run a local `~/glm-keypool` gateway (default `http://127.0.0.1:8317/api/coding/paas/v4`, where `nlos-driver-llm-http` locks both `default_model` and `fast_intent_model` directly to **`glm-5.3`** and skips `*-flash` models by default) or any OpenAI-compatible / Ollama endpoint (`LLMOS_LLM_BASE_URL`), `nlos-up` initializes a trial state root, meters every call against `ResourceAuthority` 5D `LlmTokenBudget` with full credential redaction, and launches the daemon or `Z0–Z4` desktop in one command:

```powershell
# Probe local ~/glm-keypool (or OpenAI-compatible endpoint) status with full credential redaction
cargo run -p nlos-up -- pool-status

# Bootstrap trial state root & run metered chat directly against automatic glm-5.3 (commits durable ResourceAuthority 5D Token budget receipt)
cargo run -p nlos-up -- chat --prompt "用一句话介绍 NLOS 微内核与能力令牌"

# Compile plain-language intent into structured JSON (built-in [NL-AMBIG-001] destructive escalation guardrail)
cargo run -p nlos-up -- intent --query "查看当前工作区正在运行的任务与剩余模型额度"

# Launch the cross-platform resident control plane (Windows Named Pipe / Unix Socket) or industrial workstation desktop
cargo run -p nlos-up -- daemon --seed-sample-app
cargo run -p nlos-up -- desktop --seed-sample-app
```

**Two immediate ways to view the `Z0–Z4` workstation GUI:**

1. **Full Native Tauri Workstation Window (live resident control plane + local `glm-keypool` `glm-5.3`):** run `cargo run -p nlos-up -- desktop --seed-sample-app` (on Windows GNU hosts with `C:\msys64\ucrt64` installed, `nlos-up` automatically injects `windres`/`gcc` linker flags and opens the `1180x800` `llmos 任务管理器` native window with zero manual setup; press `Ctrl+Space` or click the bottom `36px` taskbar command slot to open the `Z3` command palette pre-warmed against `glm-5.3` and try "Model Assist").
2. **Instant Browser UI Preview (`[DUI-OFFLINE-001]` zero-compile UI layout preview):** run `cd desktop && npm run dev` and open `http://localhost:1420` in your browser to inspect the five-plane `Z0–Z4` `Graphite / Paper` solid industrial workstation layout, bottom `36px` taskbar, `L0–L3` progressive disclosure toggles, `Z1` Task Space, `Ctrl+Space` `Z3` command palette (deterministic local fallback mode), and `Z4` solid security consent screen with zero Rust compilation wait.

See the full step-by-step walkthrough in [`docs/developers/quickstart-trial.md`](docs/developers/quickstart-trial.md) and the multi-step `TaskPlan` + `Artifact` productivity bundle in [`examples/starter-assistant-app/README.md`](examples/starter-assistant-app/README.md).

### 1. Build, test, and run the package CLI

```bash
git clone https://github.com/cty12356541/llmos
cd llmos

# Build all 44 root workspace crates
cargo build --workspace

# Run the root workspace test suite
cargo test --workspace

# Explore the signed package lifecycle CLI
cargo run -p nlos-package -- --help
```

### 2. Start the cross-platform resident control plane (`system-control-daemon`)

`system-control-daemon` assembles all local authorities under `--root` and binds two local IPC endpoints simultaneously — an ADR-0011 authenticated endpoint for the desktop shell and a plain endpoint for `system-control-cli` — using **Named Pipes on Windows** (`\\.\pipe\llmos-system-control-{auth|plain}-<digest>`) and **owner-only Unix domain sockets on macOS/Linux** (`<STATE_DIR>/system-control-{auth|plain}.sock`).

Key files contain one line of 64 hex characters (32-byte Ed25519 seed; on Unix, files must carry `0600` permissions or startup fails closed):

**Windows (PowerShell):**

```powershell
$StateDir = Join-Path $env:TEMP "llmos-trial-state"
New-Item -ItemType Directory -Force -Path $StateDir | Out-Null
$KeyFile = Join-Path $StateDir "trial-ed25519.key"
"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" | Set-Content -NoNewline -Encoding ascii $KeyFile

cargo run -p nlos-system-control --features daemon --bin system-control-daemon -- `
  --root $StateDir `
  --identity-key-file $KeyFile `
  --operator-key-file $KeyFile
```

**macOS / Linux (POSIX shell):**

```bash
STATE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/llmos-trial-state.XXXXXX")"
KEY_FILE="$STATE_DIR/trial-ed25519.key"
printf '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n' > "$KEY_FILE"
chmod 600 "$KEY_FILE"

cargo run -p nlos-system-control --features daemon --bin system-control-daemon -- \
  --root "$STATE_DIR" \
  --identity-key-file "$KEY_FILE" \
  --operator-key-file "$KEY_FILE"
```

Once ready, the daemon prints one probe line:

```text
READY service=system_control root=... auth_socket=<AUTH_ENDPOINT> plain_socket=<PLAIN_ENDPOINT> principal=<PRINCIPAL_HEX32> operator_root_receipt=...
```

### 3. Inspect via `system-control-cli` or Python SDK

Open a second terminal and pass `<PLAIN_ENDPOINT>` from the `READY` line:

```bash
# Check recovery worker & authority health
cargo run -p nlos-system-control --bin system-control-cli -- "<PLAIN_ENDPOINT>" inspect-health

# Enumerate tasks with bounded cursor pagination (limit <= 64)
cargo run -p nlos-system-control --bin system-control-cli -- "<PLAIN_ENDPOINT>" list-tasks 16
```

To try the Python SABI client (`sdk/python`, see [`sdk/python/README.md`](sdk/python/README.md)):

```bash
pip install -e sdk/python
```

(`nlos_sdk` supports Unix domain sockets on macOS/Linux and Named Pipes via `asyncio.WindowsProactorEventLoopPolicy()` on Windows.)

### 4. Launch the Stage D workstation desktop (`desktop/`)

Point the Tauri 2 workstation shell at `<AUTH_ENDPOINT>`, `<PRINCIPAL_HEX32>`, and `<HEX64_KEY_FILE>` emitted by `system-control-daemon` (details in [`desktop/README.md`](desktop/README.md)):

**Windows (PowerShell):**

```powershell
cd desktop
$env:LLMOS_DESKTOP_SOCKET = "<AUTH_ENDPOINT>"      # e.g. \\.\pipe\llmos-system-control-auth-...
$env:LLMOS_DESKTOP_PRINCIPAL = "<PRINCIPAL_HEX32>"
$env:LLMOS_DESKTOP_KEY_FILE = "<HEX64_KEY_FILE>"
npm install
npm run tauri dev
```

**macOS / Linux:**

```bash
cd desktop
export LLMOS_DESKTOP_SOCKET="<AUTH_ENDPOINT>"      # e.g. /tmp/.../system-control-auth.sock
export LLMOS_DESKTOP_PRINCIPAL="<PRINCIPAL_HEX32>"
export LLMOS_DESKTOP_KEY_FILE="<HEX64_KEY_FILE>"
npm install
npm run tauri dev
```

> **Tip (`nlos-up desktop` & cross-platform `dev_server`):** Both `cargo run -p nlos-up -- desktop` and the standalone demo fixture `npm run dev:server` (`desktop/src-tauri/examples/dev_server.rs`) support **Windows Named Pipes** as well as **macOS/Linux Unix domain sockets** out of the box.

---

## Engineering practices

- **`unsafe_code = "forbid"`** workspace-wide (OS IPC FFI and the fault-injection shim are the only narrowly scoped opt-outs)
- **Evidence-driven claims (`ledger-schema v1.3`)**: every claim maps to a normative Requirement ID, assurance tier (`H0–H8`), explicit scope/limitations, and bidirectional evidence lint (`python scripts/lint_claims.py`)
- **Dependency-aware wave orchestration**: multi-lane tasks execute in isolated worktrees with disjoint write-sets, CAS revision discipline, and single-integrator barrier merges
- **Cross-platform CI matrix**: Ubuntu · Windows · macOS × Rust (pinned toolchain + MSRV `1.97`) + Node.js 24 + Python 3.13 + Go + .NET
- **Specialised standing gates**: Layer-2 `dm-flakey` power-loss simulation, external crash takeover soak, cross-platform `--release` scale probes, and `cargo-fuzz` smoke suites

---

## Contributing

The best starting points for contributors and coding agents:

- Browse [GitHub Discussions](../../discussions) for architecture questions and RFCs
- Look for issues labelled [`good first issue`](../../labels/good%20first%20issue) and [`help wanted`](../../labels/help%20wanted)
- Read [`AGENTS.md`](AGENTS.md) for the mandatory repository rules (applies equally to human contributors and AI agents)
- Read the [progressive disclosure & CRUD rules](docs/management/project-knowledge-progressive-disclosure.md) before modifying design or governance documents

**Entry points by interest:**

| Interest | Where to start |
|---|---|
| Rust / durable systems | `nlos-types`, `nlos-store`, `nlos-clock` (zero/minimal-dep foundations) |
| Security / cryptography | `nlos-identity`, `nlos-capability`, `nlos-canonical` |
| Distributed systems | `nlos-cell`, `nlos-lease`, `nlos-net`, `nlos-consensus` (Stage C `EXITED`) |
| Control plane & CLI | `nlos-system-control`, `nlos-package` (`system-control-daemon`, `system-control-cli`, package tooling) |
| TypeScript / Tauri 2 desktop | `desktop/` (Stage D `EXITED`, five-plane `Z0–Z4` workstation shell) |
| Compatibility, store governance & SDKs | `nlos-up`, `nlos-driver-llm-http`, `nlos-compat-shim`, `nlos-migrate`, `nlos-publisher-trust`, `nlos-store-registry`, `nlos-certify`, `nlos-schema`, `sdk/` (Stage E `IN_PROGRESS`: `E0`–`E5` Verified `H4`, `ADR-0027` `ACCEPTED`, `E6`–`E7` `DESIGN`; see [`stage-e-progress.md`](docs/management/stage-e-progress.md)) |

---

## Design documents

| Document | Description |
|---|---|
| [`docs/design/06-架构设计总纲-v0.5.md`](docs/design/06-架构设计总纲-v0.5.md) | **Current canonical architecture specification** (`v0.5`, 278 KB) |
| [`docs/discussions/00-讨论索引.md`](docs/discussions/00-讨论索引.md) | Index of all 36 design discussion documents |
| [`docs/management/README.md`](docs/management/README.md) | Project management mechanism & stage governance routing |
| [`docs/management/adrs/`](docs/management/adrs/) | **27** Architecture Decision Records (`ADR-0001..0027`) |
| [`docs/management/stage-b-progress.md`](docs/management/stage-b-progress.md) | Stage B authoritative progress ledger (`EXITED`) |
| [`docs/management/stage-c-progress.md`](docs/management/stage-c-progress.md) | Stage C authoritative progress ledger (`EXITED`) |
| [`docs/management/stage-d-progress.md`](docs/management/stage-d-progress.md) | Stage D authoritative progress ledger (`EXITED`) |
| [`docs/management/stage-e-progress.md`](docs/management/stage-e-progress.md) | Stage E authoritative progress ledger (`IN_PROGRESS`: `E0`–`E5` Verified `H4`; `ADR-0027` `ACCEPTED`; `E6`–`E7` `DESIGN`) |
| [`docs/management/exit-unknown-risks.md`](docs/management/exit-unknown-risks.md) | Stage B / C / D exit unknown & residual risks ledger (`P0 = 0`) |
| [`AGENTS.md`](AGENTS.md) | Mandatory work rules for all agents and contributors in this repo |

---

## License

Apache-2.0 — see [LICENSE](LICENSE).
