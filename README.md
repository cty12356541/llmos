# llmos — Natural Language Operating System

> **Managing LLM Agents the way Linux manages processes.**

**English** | [中文](./README.zh-CN.md)

llmos is a microkernel and application platform written in Rust that brings operating system-grade management to LLM Agents: capability-based security, resource budgets, crash recovery, signed artifacts, and distributed federation — none of which exist in today's Agent frameworks.

---

## Why does this exist?

Every Agent framework today assumes a friendly world: a handful of trusted agents, unlimited resources, no need for isolation. That works for demos.

It doesn't work when you have **thousands of agents running in production**, each with different trust levels, resource constraints, and failure modes.

llmos asks the question nobody else is asking:

> *If agents are processes, what's the operating system?*

| What you need at scale | LangChain / AutoGen | Docker + K8s | **llmos** |
|---|---|---|---|
| Per-agent capability tokens | ✗ | ✗ | ✅ signed, attenuatable, revocable |
| Token / API budget enforcement | ✗ | ✗ | ✅ 6-dimensional resource conservation |
| Crash recovery with durable state | ✗ | container restart | ✅ authority-owned durable store + WAL |
| Signed, content-addressed outputs | ✗ | ✗ | ✅ Artifact store with Ed25519 receipts |
| Agent isolation | ✗ | process/namespace | ✅ Capability + Principal + Cell boundary |
| Distributed federation | external infra | K8s cluster | ✅ Cell-based with process migration |

---

## Architecture in one picture

```
┌─────────────────────────────────────────────────────┐
│                  NL Desktop  (Stage D)               │
├─────────────────────────────────────────────────────┤
│              Applications & Agent SDKs               │
├──────────────────────┬──────────────────────────────┤
│   SABI (Service API) │  21 service families          │
│   Task · Artifact    │  Semantic · Resource          │
│   Package · Search   │  Notify · Channel · Topic     │
├──────────────────────┴──────────────────────────────┤
│   KABI (Kernel API)  │  27 entries                   │
│   proc_spawn/exit    │  budget_query/transfer        │
│   io_submit/poll     │  meter_reserve/consume        │
├─────────────────────────────────────────────────────┤
│             NLOS Microkernel                         │
│   Identity · Capability · Clock · Cell               │
│   (every authority backed by SQLite WAL+FULL)        │
└─────────────────────────────────────────────────────┘
          ↕ Host OS (Windows / macOS / Linux)
```

**Execution hierarchy:**
```
Application ⊇ Process ⊇ AgentInstance ⊇ ExecutionFiber
```

Each level has its own durable authority store, generation fences, and lifecycle state machine.

---

## Key design decisions

### Capability-based security
Every operation requires a signed, generation-checked Capability token — not an API key. Tokens can be attenuated (narrowed) and revoked at any level of the hierarchy. No agent can escalate its own permissions.

### Durable authority stores
Each subsystem (Identity, Task, Process, Artifact, Semantic…) is an independent SQLite database with WAL + fsync. State transitions are idempotent and replay-safe. A process crash does not corrupt authority state.

### Resource conservation
Six resource demand types: `Fungible · CPU · Capacity · Rate · CumulativeMeter · Exclusive`. All follow the `Quote → Reserve → Consume → Finalize` lifecycle. Agents cannot exceed their budget; budgets are tracked with durable leases.

### Signed evidence chain
Every Package is signed by its developer. Every Artifact produced by an Agent is content-addressed and signed by the issuing authority. There is no "trust me" — there is only verifiable chain of custody.

### Cell-based distribution
One OS process = one Cell. Cells federate via signed event sets. Processes can migrate across Cells with an 8-step protocol. ADR-0019 defines cross-Cell commit semantics.

---

## Current status

| Stage | Description | Status |
|---|---|---|
| **Stage A** | Bootstrap microkernel | ✅ Done |
| **Stage B** | Single-machine application platform | ✅ Done (2026-09-21) — 1,536 tests passed |
| **Stage C** | Distributed modern system | 🔨 In Progress |
| **Stage D** | Complete NL desktop | 📋 Planned (parallel with C) |
| **Stage E** | Compatibility & ecosystem | 📋 Planned |

**Stage C work packages:**

| Package | Status |
|---|---|
| C-CELL — Cell authority & federation | 🟡 Partial |
| C-LEASE — Quota / Capacity / Device leases | 🟡 Partial |
| C-SYNC — Cross-library event & artifact sync | 🟡 Partial |
| C-SHARD — Cross-Cell sharding (Phase 0+1 verified) | 🟡 Partial |
| C-MIGRATE — Process migration protocol | 🟡 Partial |
| C-FANOUT — Distributed topic fanout | 🟡 Partial |
| C-SCHED — Distributed scheduler | 🟡 Partial |
| C-PARTITION — Partition tolerance | 🔴 Not started |
| C-BENCH — Scale benchmarks | 🔴 Not started |

---

## Repository layout

```
crates/          36 Rust library/binary crates
  nlos-types/      Nominal identity types (zero deps)
  nlos-identity/   Principal & key authority (Ed25519)
  nlos-capability/ Capability issue, attenuation, revocation
  nlos-task/       Task authority (CommitPermit, EffectPermit, recovery)
  nlos-process/    Process / AgentInstance lifecycle
  nlos-artifact/   Content-addressed artifact store
  nlos-semantic/   Semantic assertion admission authority
  nlos-cell/       Cell authority & federation (Stage C)
  nlos-lease/      Distributed lease authority (Stage C)
  nlos-slice-k/    End-to-end integration slice (Package→Task→Fiber→Artifact)
  nlos-package/    CLI: keygen · build · verify · install · run
  ... (27 more)

desktop/         Tauri desktop application (TypeScript + Rust)
docs/
  design/          Architecture spec (v0.5, 278 KB, 200+ requirement IDs)
  discussions/     36 design discussions (philosophy → distributed kernel)
  management/
    adrs/          23 Architecture Decision Records
    evidence-index.yaml   150+ evidence entries (H0–H8 levels)
    claims.yaml    Machine-readable claims registry
sdk/             Python · TypeScript · Go · C# SDKs
schema/          Protobuf + CDDL schemas + golden test vectors
experiments/     8 Python/Rust design experiments
fuzz/            3 fuzz targets with seed corpora
```

---

## Building

**Prerequisites:** Rust (toolchain pinned via `rust-toolchain.toml`), Git.

```bash
git clone https://github.com/cty12356541/llmos
cd llmos

# Build the entire workspace
cargo build --workspace

# Run all tests
cargo test --workspace

# Run the package CLI
cargo run -p nlos-package -- --help
```

**Platform support:** Windows, macOS, Linux (CI runs all three).

---

## Engineering practices

- **`unsafe_code = "forbid"`** workspace-wide (IPC FFI and fault-injection shim are the only opt-outs)
- **Evidence-driven development**: every claim has a unique ID, evidence level (H0–H8), and machine-readable registry entry
- **Wave-based progress tracking**: each development wave has explicit read-set, write-set, and acceptance criteria
- **CI matrix**: Ubuntu · Windows · macOS × Rust + Node.js 24 + Python 3.13 + Go + .NET
- **Specialised CI**: power-loss simulation, takeover-control external crash, takeover-control long soak, fuzz smoke

---

## Contributing

The project is early in its open-contribution journey. The best starting points:

- Browse [GitHub Discussions](../../discussions) for architecture questions and RFCs
- Look for issues labelled [`good first issue`](../../labels/good%20first%20issue) and [`help wanted`](../../labels/help%20wanted)
- Read [`AGENTS.md`](AGENTS.md) for the mandatory agent work rules (applies to both human contributors and AI coding assistants)
- Read the [progressive disclosure rules](docs/management/project-knowledge-progressive-disclosure.md) before touching design documents

**Good entry points by interest:**

| Interest | Where to start |
|---|---|
| Rust / systems | `nlos-types`, `nlos-store`, `nlos-clock` (clean, well-tested foundations) |
| Security / crypto | `nlos-identity`, `nlos-capability`, `nlos-canonical` |
| Distributed systems | `nlos-cell`, `nlos-lease` (Stage C, active development) |
| CLI / tooling | `nlos-package` (binary crate, user-facing) |
| TypeScript / desktop | `desktop/` (Tauri app) |
| Documentation | Any crate missing a `description` in `Cargo.toml`, or the SDK READMEs |

---

## Design documents

| Document | Description |
|---|---|
| [`docs/design/06-架构设计总纲-v0.5.md`](docs/design/06-架构设计总纲-v0.5.md) | **Current canonical architecture spec** (278 KB) |
| [`docs/discussions/00-讨论索引.md`](docs/discussions/00-讨论索引.md) | Index of all 36 design discussions |
| [`docs/management/adrs/`](docs/management/adrs/) | 23 Architecture Decision Records |
| [`docs/management/stage-c-progress.md`](docs/management/stage-c-progress.md) | Stage C progress log |
| [`AGENTS.md`](AGENTS.md) | Mandatory rules for all agents working in this repo |

---

## License

Apache-2.0 — see [LICENSE](LICENSE).
