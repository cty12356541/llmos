# llmos — 自然语言操作系统

> **像 Linux 管理进程那样，管理 LLM Agent。**

[English](./README.md) | **中文**

llmos 是一个用 Rust 编写的微内核、分布式 Cell 联邦网络与自然语言工业工作站平台，将操作系统级的工程治理能力引入 LLM Agent 世界：能力令牌安全模型、六维资源预算守恒、持久化权威崩溃恢复、签名内容寻址制品、跨物理主机 mTLS Cell 联邦，以及五平面 `Z0–Z4` 可信 UI 工作站桌面。

---

## 为什么要做这个？

今天绝大多数 Agent 框架都假设了一个「友好的世界」：少量可信 Agent、没有硬性预算约束、不需要强隔离边界。这在原型演示里够用。

但面对多租户、长周期运行、不同信任等级的 Agent 并发协作，同时伴随严格的 Token/算力配额约束以及宿主或网络局部故障时，这套假设就会失效。

llmos 回答了一个底层系统问题：

> *如果 Agent 是进程，那操作系统是什么？*

| 多 Agent 系统规模化所需能力 | LangChain / AutoGen | Docker + K8s | **llmos** |
|---|---|---|---|
| 每个 Agent 独立的能力令牌 | No | No | Yes — Ed25519 签名、可衰减、可撤销 |
| Token / API 预算强制执行 | No | No | Yes — 6 维资源守恒模型 |
| 带持久化状态的崩溃恢复 | No | 容器重启 | Yes — 权威自有 SQLite WAL+FULL 持久库 |
| 签名的、内容寻址的产出物 | No | No | Yes — Artifact 存储 + Ed25519 收据 |
| Agent 与可信界面隔离 | No | 进程 / 命名空间 | Yes — Capability + Principal + Cell + Z4 可信 UI |
| 分布式联邦与进程迁移 | 外部基础设施 | K8s 集群 | Yes — 单机与跨机 mTLS Cell 联邦 + 8 步迁移协议 |

---

## 架构一览

```text
┌───────────────────────────────────────────────────────────┐
│   NL 工业工作站桌面（Stage D：Z0-Z4 Tauri 2 桌面壳）       │
│   L0-L3 渐进式披露 · 12 域显控同构 · Z4 实心安全确认屏     │
├───────────────────────────────────────────────────────────┤
│                 应用层 & 多语言 Agent SDK                  │
├─────────────────────────┬─────────────────────────────────┤
│   SABI（服务 API）      │  21 个服务族                     │
│   Task · Artifact       │  Semantic · Resource            │
│   Package · Search      │  Notify · Channel · Topic       │
├─────────────────────────┴─────────────────────────────────┤
│   KABI（内核 API）      │  27 个入口                       │
│   proc_spawn/exit       │  budget_query/transfer          │
│   io_submit/poll        │  meter_reserve/consume          │
├───────────────────────────────────────────────────────────┤
│            NLOS 微内核与分布式 Cell 织网                   │
│   Identity · Capability · Clock · Cell · Net · Consensus  │
│   （每个权威均由独立 SQLite WAL+FULL 持久化）              │
└───────────────────────────────────────────────────────────┘
             <-> 宿主操作系统（Windows / macOS / Linux）
```

**执行层级：**

```text
Application（应用）⊇ Process（进程）⊇ AgentInstance（Agent 实例）⊇ ExecutionFiber（执行纤程）
```

每个层级均拥有独立的持久化权威存储、单调 Generation fence 与重放安全的生命周期状态机。

---

## 核心设计决策

### 基于能力令牌的安全模型
每个受控操作都必须携带经 Ed25519 签名与 generation 校验的 Capability 令牌，而非全局环境 API Key。令牌可在层级间单向衰减（权限收窄）并随时撤销，Agent 无法自行提权；跨进程与桌面 GUI 控制统一汇聚至单一认证控制脊柱（[ADR-0011](docs/management/adrs/0011-ipc-principal-auth-signature-passthrough.md)）。

### 持久化权威存储
每个核心子系统（`Identity`、`Capability`、`Task`、`Process`、`Resource`、`Artifact`、`Semantic`、`Application`、`Plan`、`Cell`、`Lease`……）都是独立的 SQLite 数据库，启用 `WAL + synchronous=FULL`。状态变迁具备幂等性与代际 CAS 防护，单进程崩溃不会破坏权威状态。

### 资源守恒模型
六类资源需求模型：`Fungible · CPU · Capacity · Rate · CumulativeMeter · Exclusive`，统一遵循 `Quote → Reserve → Consume → Finalize` 守恒生命周期，并通过持久化 `QuotaLease`、`CapacityLease` 与 `ExclusiveDeviceLease` 三族对账扩展至分布式多 Cell 拓扑。

### 签名证据链与诚实分级台账
每个应用包由发布者私钥签名；Agent 产出的每个 Artifact 与跨域事件集均采用内容寻址并绑定权威签发的不可变收据。仓库内所有能力声明均按 `H0–H8` 证据阶梯登记于机器可读台账 `docs/management/claims.yaml`，严守 `Claim <= Evidence` 铁律。

### 基于 Cell 的单机与跨物理主机分布式架构
一个 OS 进程承载一个 Cell。Cell 既支持单机多进程联邦（[ADR-0018](docs/management/adrs/0018-single-host-multiprocess-dual-cell-topology.md)、[ADR-0019](docs/management/adrs/0019-cross-cell-commit-semantics-extension.md)），也支持跨物理主机的 TLS 1.3 mTLS 传输与五轴 TLS Exporter `CellFence` 互认握手、跨时钟域保守偏斜租约包络、8 步跨机进程迁移，以及遵循 `[CONS-SCOPE-001]`（数据面 `0` Raft RPC、单机 `N = 1` 零开销直通）的嵌入式确定性 Raft 控制面多数派共识（[ADR-0020](docs/management/adrs/0020-control-plane-single-writer-and-consensus-timing.md)、[ADR-0026](docs/management/adrs/0026-cross-physical-host-transport-and-consensus-expansion.md)）。

### 中性工业工作站桌面与 Z4 可信 UI
Tauri 2 桌面（[ADR-0025](docs/management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md)）实行 `Z0–Z4` 五平面合成隔离、`L0–L3` 四级按需渐进式披露与 `[UI-PARITY-001]` 12 域显控同构，并通过不可被应用遮挡或伪造的 `Z4` 实心安全确认屏门控高风险及不可逆操作。

---

## 当前状态

| 阶段 | 范围描述 | 当前状态 |
|---|---|---|
| **Stage A** | 引导微内核 | `EXITED` |
| **Stage B** | 单机通用应用平台（[`stage-b-progress.md`](docs/management/stage-b-progress.md)） | `EXITED`（2026-09-21） |
| **Stage C** | 分布式现代系统：单机多进程 Cell + `C-XHOST` 跨物理主机 mTLS / 保守时钟偏斜租约 / 嵌入式 Raft 控制面（[`stage-c-progress.md`](docs/management/stage-c-progress.md)） | `EXITED`（2026-10-29 `W76`） |
| **Stage D** | 完整自然语言桌面：五平面 `Z0–Z4` Tauri 2 工业工作站、`L0–L3` 渐进式披露、12 域显控同构、`Z4` 安全确认屏（[`stage-d-progress.md`](docs/management/stage-d-progress.md)） | `EXITED`（2026-10-29 `W76`） |
| **Stage E** | 兼容制度与生态规模化：`SABI v1.0` 长期兼容、核外兼容垫片、商店治理与 `E0` 开箱试用引导（[`stage-e-progress.md`](docs/management/stage-e-progress.md)、[`ADR-0027`](docs/management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md)） | `IN_PROGRESS (E0–E5 Verified H4; ADR-0027 ACCEPTED; E6–E7 DESIGN)`（2026-10-31 `W79`） |

> **证据范围与诚实边界声明：** 阶段 B、阶段 C 与阶段 D 均在独立退出门终审（[W67](docs/evidence/stage-c/reviews/w67-stage-c-exit-gates.md)、[W73](docs/evidence/stage-c/reviews/w73-stage-c-xhost-exit-review.md)、[W74](docs/evidence/stage-d/reviews/w74-stage-d-exit-gates.md)）下以 `SATISFIED-WITH-BOUNDARIES`（`P0 = 0`）获批 `EXITED`。阶段 E 当前已完成先行开箱试用工作包 `E0`（`W77-PRE` / `M-E0.5`：`nlos-up` 一键引导、诚实标注 `PROVIDER_ISOLATION_UNVERIFIED` 的用户态 `nlos-driver-llm-http` 本地 KeyPool/HTTP 模型驱动、`starter-assistant-app` 样板包与桌面 KeyPool 桥接；[E-TRIAL-001](docs/evidence/stage-e/e-trial-001-llm-http-driver.md)、[E-TRIAL-002](docs/evidence/stage-e/e-trial-002-quickstart-and-starter-app.md)）、兼容治理首包 `E1`（`W77` / `M-E1`：`9/9` SABI 通道全量入冻、`N / N-1 / N-2` 版本兼容矩阵、`deprecation-manifest.yaml` 弃用遥测与五语言 `v1.0` 黄金不可变语料库；[E-SABI-001](docs/evidence/stage-e/e-sabi-001-version-matrix-and-deprecation.md)、[E-SABI-002](docs/evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md)）、核外兼容垫片 `E2`（`W78` / `M-E2`：无特权核外 `nlos-compat-shim`，支持 `N-1 / N-2 <-> N` SABI 与旧版 `minicbor <-> DriverProvider` 双向桥接及 `[DRV-SHIM-001]` 四层零秘密隔离；[E-SHIM-001](docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md)）、声明式迁移工具链 `E3`（`W78` / `M-E2`：`nlos-migrate` 清单/弃用静态检查器、`tools/sabi-compat-diff` 与 `[PKG-UPDATE-001]` 三态数据容器迁移回滚沙箱；[E-MIGRATE-001](docs/evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md)）、发布者信任链与跨生态选择器门控 `E4`（`W79` / `M-E3`：`nlos-publisher-trust` 三级 Ed25519 PKI、单调 `RevocationEpoch` 吊销账本、`ApplicationId` 防抢注归属与 `nlos-plan` 7 类生态 Typed Selector 不可变句柄解析 + G3 `WaitingAuthorization` 条件物化门，正式闭合 `U-7` / `U-8`；[E-TRUST-001](docs/evidence/stage-e/e-trust-001-publisher-pki-revocation-and-selectors.md)）以及商店治理目录与四门认证流水线 `E5`（`W79` / `M-E3`：`nlos-store-registry` OCI 内容寻址目录、不透明 `EntitlementToken` 幂等核销与三级 `RecallAdvisory` 撤回联动，连同 `nlos-certify` 四门自动化认证流水线与纯 Rust SPDX/锁文件供应链审计器，推进 `RISK-B-09` `open -> partial`；[E-STORE-001](docs/evidence/stage-e/e-store-001-oci-index-and-entitlement-ledger.md)、[E-CERTIFY-001](docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md)），保障等级均为 `H4`，且 [ADR-0027](docs/management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) 处于 `ACCEPTED`；下游双层 Wasm 强沙箱、解耦第三方市场样板与 `[ROAD-E-001]` 升级总门禁工作包 `E6`–`E7` 仍处于 `DESIGN` 规划态，尚未声称已实现。Layer-3+ 物理真机掉电台架（`U-4`）、`1M_LOGICAL` 裸金属跨机 WAN 集群实测（`U-1`）、远端失联主机外部物理副作用擦除证明（`U-5`）、第三方不可信负载三平台双层 Wasm 强沙箱生产接线（`U-9`）、`SABI v1.0` 长期冻结终审（`RISK-B-05`）、宿主 OS 商业安装器代码签名公证（`RISK-B-06`）与上游依赖周期复审（`RISK-B-09`）均按证据分级纪律如实登记于 [`docs/management/exit-unknown-risks.md`](docs/management/exit-unknown-risks.md)。

### Stage C 已验证工作包摘要（`C-core` + `C-XHOST`）

| 工作包 | 状态 | 已验证范围摘要 |
|---|---|---|
| `C-CELL` — Cell 权威与联邦装配 | `Verified` | 本地七件套装配、联邦目录/迁移意图/对账检查点、`PartitionAutonomyGate` 断连自治门 |
| `C-LEASE` — 配额 / 容量 / 独占设备 Lease | `Verified` | 三族 `SQLite WAL+FULL` 持久状态机、围栏屏障与非互换对账回执 |
| `C-SYNC` — 跨库事件与制品同步 | `Verified` | 签名事件集合并、签名向量检查点、内容寻址 `ArtifactTransferPackage` 与同步感知 GC |
| `C-SHARD` — TaskAuthority 分片与接管 | `Verified` | Schema v47 三 Record 对齐、六类端点接管屏障、生产级接管驱动绑定 |
| `C-FANOUT` — 层级扇出与四域舱壁 | `Verified` | 有界层级扇出树、Merkle 结果根、6 维准入背压、4 域舱壁与 TCB 系统保留水位 |
| `C-MIGRATE` — 进程迁移协议 | `Verified` | 严格有序 8 步迁移状态机、跨 `CellHost` 双 OS 进程端到端租约与能力重绑守恒 |
| `C-SCHED` — 三层分布式调度器 | `Verified` | 稳态零全局锁 Worker 热路径、有界 DRR、租约安全工作窃取与反向背压卸载 |
| `C-PARTITION` — 分区容错与重连收敛 | `Verified` | 四阶段重连收敛编排、`PartitionBoundedLossProof` 精确额度守恒证明 |
| `C-BENCH` — 单机多分片分布式基准 | `Verified` | `1->8` 进程 Cell `4.44x` 扩展（`--release`）、`10K/100K_LOGICAL` `WAL+FULL`、5 类故障负载验证 |
| `C-XHOST`（`001`–`005`）— 跨物理主机扩展（[ADR-0026](docs/management/adrs/0026-cross-physical-host-transport-and-consensus-expansion.md)） | `Verified` | `nlos-net` TLS 1.3 mTLS + 五轴 `CellFence`、跨时钟域保守偏斜租约、`nlos-consensus` 嵌入式 Raft 控制面（`N=1` 零开销直通）、跨主机流式同步与迁移、`ChaosTcpProxy` `H6` 多机混沌门禁 |

### Stage D 已验证工作包摘要（`D-core` + `[ROAD-D-001]`）

| 工作包 | 状态 | 已验证范围摘要 |
|---|---|---|
| `D-TOKENS-KIT` — 工业工作站视觉令牌与原语套件 | `Verified` | `Graphite` / `Paper` 100% 实心双主题（零毛玻璃/零发光/零 Emoji）与六大工业级 SOTA 可视化原语 |
| `D-IPC-ENUM` — 12 域分页枚举与显控同构 | `Verified` | `IPC-D1..D8` 有界游标枚举（`limit <= 64`）、显式 `ControlScope`、爆炸半径预览与法定不可篡改禁区 |
| `D-COMPOSITOR-WM` — 五平面 `Z0–Z4` 合成器与窗口管理 | `Verified` | 五模式平铺优先窗口管理器、`36px` 底部紧凑任务栏、`L0–L3` 渐进式披露 Store 与代际防伪输入路由 |
| `D-OMNIBAR-MULTIMODAL` — 按需意图面板与多模态 | `Verified` | `Ctrl+Space` `Z3` 命令/意图面板、`TypedIntent` 预览、`[NL-AMBIG-001]` 歧义澄清门、语音流与内容寻址附件 |
| `D-TRUSTED-UI` — `Z4` 安全确认屏、选择器与剪贴板门 | `Verified` | UAC/Polkit 级实心 `Z4` 安全确认屏（`350ms` 防劫持、`IRREVERSIBLE` 显式披露）、`ChooserGrant` 衰减选择器、污点感知剪贴板/拖拽 |
| `D-TASK-SPACE-DAG` — `L1–L3` 任务空间与 DAG 编辑器 | `Verified` | `L1` 16 态承诺表、`L2` 单行按需归因抽屉、`L3` 正交 `TaskPlan` DAG 编辑器与 `TaskAttempt` 竞速泳道 |
| `D-NOTIFY-SEARCH-OFFLINE` — 通知、Spotlight 搜索与离线自治 | `Verified` | 一次性 SHA-256 `ActionToken` 通知核销、`Ctrl+K` `0x1F` 非权威搜索守卫、`[DUI-OFFLINE-001]` 本地断网自治 |
| `D-CONTROL-SETTINGS-A11Y` 与 `[ROAD-D-001]` — 控制中心、设置、无障碍与终验门 | `Verified` | 12 域 `SystemControlCenter`（108 项 Tauri 命令）、四层 `SettingsDomain` CAS、`KeychainReference`、`WCAG 2.2 AA`、`zh-CN / en-US` 双语包、1,000 轮零泄漏战役与非开发者多应用端到端门禁 |

---

## 仓库结构

```text
crates/              44 个根 workspace Rust 库/二进制 crate（根 Cargo.toml）
  nlos-types/           标称身份类型（零第三方依赖）
  nlos-identity/        Principal 与密钥权威（Ed25519）
  nlos-capability/      Capability 颁发、衰减、撤销与 KEK 托管签名库
  nlos-task/            Task 权威（CommitPermit、EffectPermit、分片接管）
  nlos-process/         Process / AgentInstance 生命周期与 8 步迁移状态机
  nlos-artifact/        内容寻址制品存储与四轴同步策略
  nlos-semantic/        语义断言准入权威与签名向量检查点
  nlos-cell/            Cell 权威、本地七件套装配与分区自治门（Stage C）
  nlos-lease/           持久化 Quota / Capacity / ExclusiveDevice Lease 权威（Stage C）
  nlos-net/             跨主机 TLS 1.3 mTLS 传输与五轴 CellFence 握手（Stage C C-XHOST）
  nlos-consensus/       嵌入式确定性 Raft 控制面多数派共识（Stage C C-XHOST）
  nlos-system-control/  常驻 system-control-daemon 与 system-control-cli（12 域控制脊柱）
  nlos-slice-k/         端到端集成切片（Package → Task → Fiber → Artifact）
  nlos-package/         CLI：keygen · build · conformance · verify · install · run · update · uninstall
  nlos-driver-llm-http/ 用户态本地 KeyPool/OpenAI HTTP/1.1 模型驱动与五维预算垫片（Stage E E0）
  nlos-up/              一键跨平台试用引导器、模型池 CLI 与桌面启动器（Stage E E0）
  nlos-compat-shim/     核外无特权 SABI 跨纪元与 DriverProvider 协议转换垫片（Stage E E2）
  nlos-migrate/         声明式契约/弃用静态检查器与三态数据迁移回滚沙箱（Stage E E3）
  nlos-publisher-trust/ 三级 Ed25519 发布者 PKI、RevocationEpoch 吊销与 AppId 归属账本（Stage E E4）
  nlos-store-registry/  OCI CAS 商店治理目录、许可令牌核销与三级恶意包撤回账本（Stage E E5）
  nlos-certify/         四门自动化上架认证流水线与纯 Rust SPDX/锁文件供应链审计器（Stage E E5）
  ...（其余 23 个根 workspace crate）

desktop/             五平面 Z0-Z4 Tauri 2 工业工作站桌面（TypeScript + 独立 Rust workspace）
examples/
  sample-app/            签名样板 NLOS 应用载荷与全生命周期夹具（独立 Cargo 包）
  starter-assistant-app/ 参考生产力应用包（v1.0.0/v1.1.0 TaskPlan + 五维 LlmTokenBudget + Artifact 因果血缘）
docs/
  design/            规范性架构总纲（v0.5，278 KB，200+ 需求 ID）
  discussions/       36 份设计讨论文档（总索引 + 35 篇专题讨论）
  management/
    adrs/            27 份架构决策记录（ADR-0001..0027，另含模板）
    evidence-index.yaml  197 条证据索引（覆盖 H0-H8 证据等级）
    claims.yaml      116 条机器可读声明（ledger-schema v1.3：71 条 Stage B + 35 条 Stage C + 10 条 Stage D）
sdk/                 Python · TypeScript · Go · C# 四语言 SABI SDK
schema/              Protobuf + CDDL 契约定义 + 黄金线协议测试向量
experiments/         8 个 Python/Rust 早期架构实验
fuzz/                3 个 cargo-fuzz 目标与种子语料库（独立于根 workspace exclude）
```

---

## 构建与本地快速试用

**前置条件：** Rust（工具链版本由 `rust-toolchain.toml` 固定，MSRV `1.97`）、Git。桌面壳与多语言 SDK 本地试用可选安装：Node.js `>= 20`（Tauri 2 桌面）、Python `>= 3.11`（`nlos-sdk`）。

**平台支持：** Windows、macOS、Linux（CI 全矩阵覆盖三大平台）。

### 0. 一键开箱引导与直连本地模型 Key 池（`nlos-up`）

若本机已运行 `~/glm-keypool` 号池网关（默认 `http://127.0.0.1:8317/api/coding/paas/v4`，`nlos-driver-llm-http` 会将 `default_model` 与 `fast_intent_model` 统一直接锁定为 **`glm-5.3`** 并默认跳过 `*-flash` 模型）或配置了任意 OpenAI-compatible / 本地 Ollama 端点（`LLMOS_LLM_BASE_URL`），可通过 `nlos-up` 一键完成试用状态根初始化、`ResourceAuthority` 五维 `LlmTokenBudget` 计量对话/意图拆解（全链路凭据脱敏）以及常驻控制面或工业工作站桌面拉起：

```powershell
# 探测本地 ~/glm-keypool（或 OpenAI-compatible 端点）状态（全链路凭据脱敏）
cargo run -p nlos-up -- pool-status

# 一键初始化试用状态根并直连默认 glm-5.3 对话（自动落库 ResourceAuthority 五维 Token 预算回执）
cargo run -p nlos-up -- chat --prompt "用一句话介绍 NLOS 微内核与能力令牌"

# 自然语言意图结构化拆解（内置 [NL-AMBIG-001] 破坏性动作不可逆升级护栏）
cargo run -p nlos-up -- intent --query "查看当前工作区正在运行的任务与剩余模型额度"

# 一键拉起跨平台常驻控制面（Windows Named Pipe / Unix Socket）或工业工作站桌面
cargo run -p nlos-up -- daemon --seed-sample-app
cargo run -p nlos-up -- desktop --seed-sample-app
```

**立即查看 `Z0–Z4` 工业工作站 GUI 的两种方式：**

1. **原生 Tauri 桌面工作站窗口（连接真实常驻控制面 + 本机 `glm-keypool` `glm-5.3`）**：运行 `cargo run -p nlos-up -- desktop --seed-sample-app`（Windows GNU 环境若安装了 `C:\msys64\ucrt64` 会由 `nlos-up` 自动注入 `windres`/`gcc` 链接参数，免手工配置直接弹出 `1180x800` `llmos 任务管理器` 窗口；按 `Ctrl+Space` 或点击底部 `36px` 任务栏命令槽即可打开已直连 `glm-5.3` 的 `Z3` 命令面板并体验“模型辅助拆解”）。
2. **浏览器即时前端预览（`[DUI-OFFLINE-001]` 免编译秒开 UI 布局）**：运行 `cd desktop && npm run dev`，在浏览器访问 `http://localhost:1420` 即可零 Rust 编译等待即时查看五平面 `Z0–Z4` `Graphite / Paper` 实心工业工作站布局、底部 `36px` 任务栏、`L0–L3` 渐进式披露切换、`Z1` 任务空间、`Ctrl+Space` `Z3` 命令面板（本地确定性回退态）与 `Z4` 实心安全确认屏。

完整跨平台操作步骤与诚实公证边界见 [`docs/developers/quickstart-trial.md`](docs/developers/quickstart-trial.md)，多步 `TaskPlan` + `Artifact` 因果血缘生产力样板见 [`examples/starter-assistant-app/README.md`](examples/starter-assistant-app/README.md)。

### 1. 构建工作区、运行测试与包管理 CLI

```bash
git clone https://github.com/cty12356541/llmos
cd llmos

# 构建根工作区全部 44 个 crate
cargo build --workspace

# 运行根工作区全部测试
cargo test --workspace

# 查看签名包生命周期 CLI 帮助
cargo run -p nlos-package -- --help
```

### 2. 启动跨平台常驻控制面守护进程（`system-control-daemon`）

`system-control-daemon` 在 `--root` 状态目录下装配全套本地持久化权威，并同时绑定两个本地 IPC 端点——供桌面壳使用的 ADR-0011 认证端点（`auth_socket`）与供 `system-control-cli` 使用的明文端点（`plain_socket`）：**Windows 下自动派生 Named Pipe**（`\\.\pipe\llmos-system-control-{auth|plain}-<digest>`），**macOS / Linux 下自动创建仅属主可访问的 Unix Domain Socket**（`<STATE_DIR>/system-control-{auth|plain}.sock`）。

密钥文件包含单行 64 位十六进制字符（32 字节 Ed25519 种子；Unix 平台要求文件权限严格为 `0600`，否则启动时按 fail-closed 拒绝）：

**Windows（PowerShell）：**

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

**macOS / Linux（POSIX Shell）：**

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

守护进程就绪后会输出一行标准就绪摘要：

```text
READY service=system_control root=... auth_socket=<AUTH_ENDPOINT> plain_socket=<PLAIN_ENDPOINT> principal=<PRINCIPAL_HEX32> operator_root_receipt=...
```

### 3. 使用 `system-control-cli` 或 Python SDK 交互巡检

打开第二个终端，传入 `READY` 行打印的 `<PLAIN_ENDPOINT>`：

```bash
# 巡检恢复工作线程与权威健康态势
cargo run -p nlos-system-control --bin system-control-cli -- "<PLAIN_ENDPOINT>" inspect-health

# 有界游标分页枚举任务列表（limit <= 64）
cargo run -p nlos-system-control --bin system-control-cli -- "<PLAIN_ENDPOINT>" list-tasks 16
```

如需本地体验 Python SABI 客户端（详见 [`sdk/python/README.md`](sdk/python/README.md)）：

```bash
pip install -e sdk/python
```

（`nlos_sdk` 在 macOS/Linux 上连接 Unix Domain Socket，在 Windows 上通过默认 `asyncio.WindowsProactorEventLoopPolicy()` 原生连接 Named Pipe。）

### 4. 启动 Stage D 工业工作站桌面（`desktop/`）

在 `desktop/` 目录下将守护进程打印的 `<AUTH_ENDPOINT>`、`<PRINCIPAL_HEX32>` 与密钥文件路径配置到环境变量后启动 Tauri 2 桌面（详细说明见 [`desktop/README.md`](desktop/README.md)）：

**Windows（PowerShell）：**

```powershell
cd desktop
$env:LLMOS_DESKTOP_SOCKET = "<AUTH_ENDPOINT>"      # 例如 \\.\pipe\llmos-system-control-auth-...
$env:LLMOS_DESKTOP_PRINCIPAL = "<PRINCIPAL_HEX32>"
$env:LLMOS_DESKTOP_KEY_FILE = "<HEX64_KEY_FILE>"
npm install
npm run tauri dev
```

**macOS / Linux：**

```bash
cd desktop
export LLMOS_DESKTOP_SOCKET="<AUTH_ENDPOINT>"      # 例如 /tmp/.../system-control-auth.sock
export LLMOS_DESKTOP_PRINCIPAL="<PRINCIPAL_HEX32>"
export LLMOS_DESKTOP_KEY_FILE="<HEX64_KEY_FILE>"
npm install
npm run tauri dev
```

> **提示（`nlos-up desktop` 与跨平台 `dev_server`）：** `cargo run -p nlos-up -- desktop` 与 `desktop/` 内置开发夹具 `npm run dev:server`（`desktop/src-tauri/examples/dev_server.rs`）现已同时支持 **Windows Named Pipe** 与 **macOS / Linux Unix Domain Socket**。

---

## 工程实践

- **`unsafe_code = "forbid"`** 全工作区强制生效（仅底层 OS IPC FFI 与故障注入 shim 拥有严格受限的局部例外）
- **证据驱动台账（`ledger-schema v1.3`）**：每条 Claim 绑定唯一规范 Requirement ID、证据等级（`H0–H8`）、适用范围限制与双向索引机械校验（`python scripts/lint_claims.py`）
- **依赖感知波次编排**：多车道任务在隔离 Git Worktree 中按严格互斥写集与 CAS 版本纪律并行推进，由波屏障单一集成器合流验收
- **跨平台全矩阵 CI**：Ubuntu · Windows · macOS × Rust（固定工具链 + MSRV `1.97`）+ Node.js 24 + Python 3.13 + Go + .NET
- **专项常设验证门**：Layer-2 `dm-flakey` 块层掉电模拟、接管控制外部强杀长稳浸泡、三平台 `--release` 规模探针与 `cargo-fuzz` 烟雾测试

---

## 参与贡献

面向人类贡献者与 AI 编码 Agent 的推荐入口：

- 浏览 [GitHub Discussions](../../discussions) 中的架构议题与 RFC
- 查找带有 [`good first issue`](../../labels/good%20first%20issue) 与 [`help wanted`](../../labels/help%20wanted) 标签的 Issue
- 修改代码或文档前，请先阅读 [`AGENTS.md`](AGENTS.md) 强制工作规则
- 修改设计规范或管理台账前，请先阅读[项目知识渐进式披露与自动 CRUD 规则](docs/management/project-knowledge-progressive-disclosure.md)

**按兴趣方向分类的起点：**

| 兴趣方向 | 建议起点 |
|---|---|
| Rust / 持久化系统底座 | `nlos-types`、`nlos-store`、`nlos-clock`（零依赖/极简依赖核心基础层） |
| 安全 / 密码学 | `nlos-identity`、`nlos-capability`、`nlos-canonical` |
| 分布式系统与共识 | `nlos-cell`、`nlos-lease`、`nlos-net`、`nlos-consensus`（Stage C 已 `EXITED`） |
| 控制面守护进程与 CLI | `nlos-system-control`、`nlos-package`（`system-control-daemon`、`system-control-cli` 与包工具链） |
| TypeScript / Tauri 2 桌面 | `desktop/`（Stage D 已 `EXITED`，五平面 `Z0–Z4` 工业工作站壳） |
| 开箱引导、长期兼容、商店治理与多语言 SDK | `nlos-up`、`nlos-driver-llm-http`、`nlos-compat-shim`、`nlos-migrate`、`nlos-publisher-trust`、`nlos-store-registry`、`nlos-certify`、`nlos-schema`、`sdk/`（Stage E `IN_PROGRESS`：`E0`–`E5` 已验证 `H4`，`ADR-0027` `ACCEPTED`，`E6`–`E7` 为 `DESIGN`；详见 [`stage-e-progress.md`](docs/management/stage-e-progress.md)） |

---

## 设计文档

| 文档 | 说明 |
|---|---|
| [`docs/design/06-架构设计总纲-v0.5.md`](docs/design/06-架构设计总纲-v0.5.md) | **当前规范性架构基线**（`v0.5`，278 KB） |
| [`docs/discussions/00-讨论索引.md`](docs/discussions/00-讨论索引.md) | 全部 36 份设计讨论总索引 |
| [`docs/management/README.md`](docs/management/README.md) | 项目管理机制与各阶段治理总路由 |
| [`docs/management/adrs/`](docs/management/adrs/) | **27** 份架构决策记录（`ADR-0001..0027`） |
| [`docs/management/stage-b-progress.md`](docs/management/stage-b-progress.md) | Stage B 权威进度台账（`EXITED`） |
| [`docs/management/stage-c-progress.md`](docs/management/stage-c-progress.md) | Stage C 权威进度台账（`EXITED`） |
| [`docs/management/stage-d-progress.md`](docs/management/stage-d-progress.md) | Stage D 权威进度台账（`EXITED`） |
| [`docs/management/stage-e-progress.md`](docs/management/stage-e-progress.md) | Stage E 权威进度台账（`IN_PROGRESS`：`E0`–`E5` Verified `H4`；`ADR-0027` `ACCEPTED`；`E6`–`E7` `DESIGN`） |
| [`docs/management/exit-unknown-risks.md`](docs/management/exit-unknown-risks.md) | Stage B / C / D 退出未知与残余风险清单（`P0 = 0`） |
| [`AGENTS.md`](AGENTS.md) | 仓库内所有 Agent 与贡献者的强制工作规则 |

---

## 许可证

Apache-2.0 — 详见 [LICENSE](LICENSE)。
