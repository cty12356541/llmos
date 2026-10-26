# llmos — 自然语言操作系统

> **像 Linux 管理进程那样，管理 LLM Agent。**

llmos 是一个用 Rust 编写的微内核与应用平台，将操作系统级别的管理能力带入 LLM Agent 世界：能力令牌安全模型、资源预算、崩溃恢复、签名制品，以及分布式 Cell 联邦——这些在现有的 Agent 框架中统统不存在。

[English](./README.md) | **中文**

---

## 为什么要做这个？

今天所有的 Agent 框架都假设了一个「友好的世界」：少量可信 Agent、资源充裕、不需要隔离。这在 Demo 里够用。

但当**数千个 Agent 在生产环境中同时运行**，彼此信任级别不同、资源约束各异、故障模式千变万化时，这套假设就彻底崩塌了。

llmos 提出了一个别人都没问过的问题：

> *如果 Agent 是进程，那操作系统是什么？*

| 规模化后你需要的 | LangChain / AutoGen | Docker + K8s | **llmos** |
|---|---|---|---|
| 每个 Agent 独立的能力令牌 | ✗ | ✗ | ✅ Ed25519 签名，可衰减，可撤销 |
| Token / API 调用预算强制执行 | ✗ | ✗ | ✅ 6 维资源守恒模型 |
| 带持久化状态的崩溃恢复 | ✗ | 容器重启 | ✅ 权威持久化存储 + WAL |
| 签名的、内容寻址的产出物 | ✗ | ✗ | ✅ Artifact 存储 + Ed25519 收据 |
| Agent 隔离 | ✗ | 进程/命名空间 | ✅ Capability + Principal + Cell 边界 |
| 分布式联邦 | 外部基础设施 | K8s 集群 | ✅ Cell 联邦 + 进程迁移 |

---

## 架构一览

```
┌─────────────────────────────────────────────────────┐
│                  NL 桌面（Stage D）                  │
├─────────────────────────────────────────────────────┤
│              应用层 & Agent SDK                      │
├──────────────────────┬──────────────────────────────┤
│   SABI（服务 API）   │  21 个服务族                  │
│   Task · Artifact    │  Semantic · Resource          │
│   Package · Search   │  Notify · Channel · Topic     │
├──────────────────────┴──────────────────────────────┤
│   KABI（内核 API）   │  27 个入口                    │
│   proc_spawn/exit    │  budget_query/transfer        │
│   io_submit/poll     │  meter_reserve/consume        │
├─────────────────────────────────────────────────────┤
│             NLOS 微内核                              │
│   Identity · Capability · Clock · Cell               │
│   （每个权威均由 SQLite WAL+FULL 持久化）            │
└─────────────────────────────────────────────────────┘
          ↕ 宿主操作系统（Windows / macOS / Linux）
```

**执行层级：**
```
Application（应用）⊇ Process（进程）⊇ AgentInstance（Agent 实例）⊇ ExecutionFiber（执行纤程）
```

每个层级都有独立的持久化权威存储、Generation fence 和生命周期状态机。

---

## 核心设计决策

### 基于能力的安全模型
每个操作都需要一个经过 Ed25519 签名、generation 检查的 Capability 令牌——而不是 API Key。令牌可以被衰减（权限收窄）并在层级的任何位置撤销。Agent 无法自行提升权限。

### 持久化权威存储
每个子系统（Identity、Task、Process、Artifact、Semantic……）都是独立的 SQLite 数据库，使用 WAL + fsync。状态转换具有幂等性和重放安全性。进程崩溃不会破坏权威状态。

### 资源守恒模型
六种资源需求类型：`Fungible · CPU · Capacity · Rate · CumulativeMeter · Exclusive`。全部遵循 `Quote → Reserve → Consume → Finalize` 生命周期。Agent 不能超过预算；预算通过持久化 Lease 跟踪。

### 签名证据链
每个 Package 由开发者签名。Agent 产生的每个 Artifact 都经过内容寻址，并由颁发权威签名。没有「相信我」——只有可验证的监管链。

### 基于 Cell 的分布式架构
一个 OS 进程 = 一个 Cell。Cell 通过签名事件集合并进行联邦。进程可以通过 8 步协议跨 Cell 迁移。ADR-0019 定义了跨 Cell 的提交语义。

---

## 当前状态

| 阶段 | 描述 | 状态 |
|---|---|---|
| **Stage A** | 引导微内核 | ✅ 完成 |
| **Stage B** | 单机应用平台 | ✅ 完成（2026-09-21）— 1,536 个测试全部通过 |
| **Stage C** | 分布式现代系统 | ✅ 技术完成 — 1,922+ 测试通过，等待正式退出批准 |
| **Stage D** | 完整 NL 桌面 | 📋 规划中（与 C 并行） |
| **Stage E** | 兼容性与生态 | 📋 规划中 |

**Stage C 工作包（全部在 [ADR-0018](docs/management/adrs/0018-single-host-multiprocess-dual-cell-topology.md) 单主机多进程拓扑下验证）：**

| 工作包 | 状态 | 亮点 |
|---|---|---|
| C-CELL — Cell 权威与联邦 | ✅ 已验证 | 七件套装配，分区自治门 |
| C-LEASE — 配额 / 容量 / 设备 Lease | ✅ 已验证 | 三族持久状态机，对账回执 |
| C-SYNC — 跨库事件与制品同步 | ✅ 已验证 | 签名事件集合并，向量检查点，制品传输 |
| C-SHARD — TaskAuthority 分片 | ✅ 已验证 | Phase 0+1 完成，Schema v47，生产接管绑定 |
| C-FANOUT — 层级扇出与舱壁 | ✅ 已验证 | Merkle 结果根，6 维准入，4 域舱壁 |
| C-MIGRATE — 进程迁移协议 | ✅ 已验证 | 8 步协议，跨 CellHost 双进程端到端 |
| C-SCHED — 三层调度器 | ✅ 已验证 | 零全局锁热路径，DRR，工作窃取 |
| C-PARTITION — 分区容错 | ✅ 已验证 | 有界损耗证明，四阶段重连收敛 |
| C-BENCH — 分布式基准测试 | ✅ 已验证 | 4.44x 线性扩展 (1→8 Cell)，p50=84µs @100K，5 类故障覆盖 |

---

## 仓库结构

```
crates/          35 个 Rust 库/二进制 crate
  nlos-types/      标称身份类型（零依赖）
  nlos-identity/   Principal 与密钥权威（Ed25519）
  nlos-capability/ Capability 颁发、衰减、撤销
  nlos-task/       Task 权威（CommitPermit、EffectPermit、恢复）
  nlos-process/    Process / AgentInstance 生命周期
  nlos-artifact/   内容寻址制品存储
  nlos-semantic/   语义断言准入权威
  nlos-cell/       Cell 权威与联邦（Stage C）
  nlos-lease/      分布式 Lease 权威（Stage C）
  nlos-slice-k/    端到端集成切片（Package→Task→Fiber→Artifact）
  nlos-package/    CLI：keygen · build · verify · install · run
  ...（还有 24 个）

desktop/         Tauri 桌面应用（TypeScript + Rust）
docs/
  design/          架构规范（v0.5，278 KB，200+ 需求 ID）
  discussions/     36 篇设计讨论（哲学 → 分布式内核）
  management/
    adrs/          24 份架构决策记录（ADR）
    evidence-index.yaml   169 条证据条目（H0–H8 等级）
    claims.yaml    101 条机器可读 Claim（71 条 Stage B + 30 条 Stage C）
sdk/             Python · TypeScript · Go · C# SDK
schema/          Protobuf + CDDL schema + 金色测试向量
experiments/     8 个 Python/Rust 设计实验
fuzz/            3 个 fuzz 目标及种子语料库
```

---

## 构建

**前置条件：** Rust（工具链通过 `rust-toolchain.toml` 固定版本）、Git。

```bash
git clone https://github.com/cty12356541/llmos
cd llmos

# 构建整个工作区
cargo build --workspace

# 运行所有测试
cargo test --workspace

# 运行 package CLI
cargo run -p nlos-package -- --help
```

**平台支持：** Windows、macOS、Linux（CI 在三个平台上运行）。

---

## 工程实践

- **`unsafe_code = "forbid"`** 全工作区强制（IPC FFI 和故障注入 shim 是唯一例外）
- **证据驱动开发**：每个 Claim 有唯一 ID、证据等级（H0–H8）和机器可读的注册表条目
- **波次制进度跟踪**：每个开发波次有明确的读集、写集和验收条件
- **CI 矩阵**：Ubuntu · Windows · macOS × Rust + Node.js 24 + Python 3.13 + Go + .NET
- **专项 CI**：电力中断模拟、接管控制外部崩溃、接管控制长浸泡测试、fuzz 烟雾测试

---

## 参与贡献

项目正处于开放贡献的早期阶段。最佳起点：

- 浏览 [GitHub Discussions](../../discussions) 中的架构问题和 RFC
- 查找标有 [`good first issue`](../../labels/good%20first%20issue) 和 [`help wanted`](../../labels/help%20wanted) 的 Issue
- 在修改任何设计文档之前，请阅读 [`AGENTS.md`](AGENTS.md)（对人类贡献者和 AI 编码助手均适用）
- 阅读[渐进式披露规则](docs/management/project-knowledge-progressive-disclosure.md)

**按兴趣分类的入口：**

| 兴趣方向 | 建议起点 |
|---|---|
| Rust / 系统编程 | `nlos-types`、`nlos-store`、`nlos-clock`（干净、测试完备的基础层） |
| 安全 / 密码学 | `nlos-identity`、`nlos-capability`、`nlos-canonical` |
| 分布式系统 | `nlos-cell`、`nlos-lease`（Stage C，活跃开发中） |
| CLI / 工具链 | `nlos-package`（面向用户的二进制 crate） |
| TypeScript / 桌面 | `desktop/`（Tauri 应用） |
| 文档 | 任何 `Cargo.toml` 缺少 `description` 的 crate，或 SDK README |

---

## 设计文档

| 文档 | 说明 |
|---|---|
| [`docs/design/06-架构设计总纲-v0.5.md`](docs/design/06-架构设计总纲-v0.5.md) | **当前规范性架构基线**（278 KB） |
| [`docs/discussions/00-讨论索引.md`](docs/discussions/00-讨论索引.md) | 36 篇设计讨论索引 |
| [`docs/management/adrs/`](docs/management/adrs/) | 23 份架构决策记录 |
| [`docs/management/stage-c-progress.md`](docs/management/stage-c-progress.md) | Stage C 进度日志 |
| [`AGENTS.md`](AGENTS.md) | 仓库内所有 Agent 的强制工作规则 |

---

## 许可证

Apache-2.0 — 详见 [LICENSE](LICENSE)。
