# DEL-RES：12 维现代资源平面强制/降档画像与宿主控制器架构基线

> **状态**：**`IN_PROGRESS`（启动基线 / 现网代码审计与 `DESIGN` 规范基线；`2026-11-22` 增补第 12 维 `nlos-notify` 边车策略引擎实证）** — 2026-11-21 / 2026-11-22。
> 对应 [NLOS 总任务计划书](./nlos-goal-delivery-plan.md#del-res现代资源与可控执行的生产边界承接-bc) §5 `DEL-RES`，波次车道 `W82-R3 / W82-R3-DEL-RES`（隔离工作分支 `feat/w82-r3-del-res`，基线 SHA `9414000f9b19179ca0922d95bbf81e21eff137ab`）及 `W82-R4-IND / DEL-RES-ATTN-CTRL`（第 12 维人类注意力边车控制器增补）。
> **配对核验依据**：[B-RES-PROFILE-001：12 维现代资源平面现网基线核验与降档画像](../evidence/stage-b/b-res-profile-audit-20261121.md)（`b-res-profile-audit-20261121`，`H3`）、[D-NOTIFY-ATTN-001：nlos-notify AttentionPolicyController 边车策略引擎与对抗验证](../evidence/stage-d/d-notify-attention-policy-20261122.md)（`d-notify-attention-policy-20261122`，`H4`）。
>
> **诚实边界声明**：依据 [v0.5 §0.2/§0.3](../design/06-架构设计总纲-v0.5.md#02-规范理由理论和证据分离) 与 [AGENTS.md 规则 7](../../AGENTS.md)，本文第 2 节仅陈述经一手源码阅读与单机测试核验的现网事实（含 `W82-R4-IND` 合入的 `nlos-notify` 第 12 维边车控制器 `H4` 实证，桌面 Tauri `Z4`/UI 与通知分发总线真实连线 `DEL-UX` 及真实人类试用 `DEL-FIELD` `H8 = 0` 仍未完成，故整机/UI 端维持受限分级），第 3–4 节标注为 **`DESIGN`（未实现）** 的宿主 `ResourceGroup` 控制器与验收测试矩阵**尚未落地任何 OS 级生产代码**，不得标为已实现或已通过验收。本文件仅由波次集成者按独占写集同步更新，未触碰 `Cargo.lock` 或源码实现。

---

## 1. 分级标签定义与 `[TCB-DOWNGRADE-001]` / `[ROAD-A-004]` 红线

### 1.1 五级约束强制分级标签

为消除「软件账本记账」「Wasm 客体沙箱限额」与「宿主操作系统硬隔离」之间的概念混用，本画像统一使用以下五类互斥分级标签评估每个（资源维度 × 平台）单元：

| 分级标签 | 精确语义与判定条件 | 典型适用情形 |
|---|---|---|
| `STRICT_ENFORCED` | 存在不可绕过的系统/宿主强制点（`[RSM-ENFORCE-001]`），能在受控边界内同步阻断越界、触发压力/隔离并产出关联 Receipt，不依赖不可信调用者自律 | 受控服务边界内消息帧长与存储水位自限、`nlos-net` mTLS 传输流控、未来经实测的 Linux `cgroup v2` / Windows `Job Object` 硬限 |
| `GUEST_SANDBOX_ONLY` | 仅在 `nlos-wasm-sandbox`（独立 `env_clear()` 子进程 + Wasmtime L2 白名单/Limiter/Fuel/Epoch）内对不可信 Wasm 客体强制生效；对原生二进制进程、宿主侧或跨进程共享均不构成强制 | Wasm 64 KiB 线性内存页硬限、`fuel`/epoch、零 socket WASI 白名单、单次调用 8 MiB 文件写上限 |
| `METERED_OBSERVED` | 可信服务端或驱动垫片对消费做单调整数计量、预留守恒、配额检查或 `OpenMetrics` 观测导出（`[RSM-METER-001]`），但**无法物理阻止**外部端点真实计费超出、旁路直连或底层物理硬件耗尽 | `ResourceAuthority` v8 5 维 LLM 预算、`llm_gateway::Gateway`（明确声明 `OBSERVED`）、模型上下文 Token 上限与影子包络 |
| `COOPERATIVE_DOWNGRADE` | 仅有声明性需求谓词、用户态调度器队列权重、账本决策记录或部分托管边界，**缺失**不可绕过的 OS/内核级控制器绑定或存在未封堵宿主旁路（`[NLOS-HOST-002]`）；必须按 `[TCB-DOWNGRADE-001]` 显式降档透传至 Receipt 与 UI | `ResourceDemand` 三维字段与 `record_throttle_decision`、宿主未沙箱原生进程、macOS 宿主内存/I/O 限制、系统级通知注意力流控 |
| `UNSUPPORTED` | 全仓零控制器实现，或宿主 OS / 硬件平台根本不提供可用强制原语（`[RSM-CONTROLLER-001]` 禁止在 `OperationRegistry` 广告） | 物理 GPU/NPU 显存隔离与固件清零、能源/功耗/热节流（`mJ`/`mW`/DVFS）、macOS 宿主 `RLIMIT_AS`（实测 `EINVAL`） |

### 1.2 必须守住的四条规范红线

1. **`[MODEL-RESOURCE-001]` / `[RSM-SEPARATE-001]` / `[ROAD-A-004]`（账本与控制器严格分离）**：[`nlos-resource`](../../crates/nlos-resource/src/lib.rs) 中的 `ResourceAccount`、`ReservationRecord`、`LlmTokenBudgetRecord` 及其 5 维预算只承担可转移额度与不可变核算；**严禁**以账户余额或 LLM Token 预算冒充宿主 OS `ResourceGroup` 对 CPU、内存、存储或 I/O 的调度与容量硬限。
2. **`[TCB-DOWNGRADE-001]` / `[NLOS-HOST-002]`（未中介路径强制可见降档）**：任何无法阻止宿主旁路或缺失 OS 硬件 controller 的维度，其 `GuaranteeTier` / `EnforcementProfile` **必须**在 `AdmissionDecision`、执行 `Receipt` 及桌面 `Z4` / Resource Monitor 中降档为 `COOPERATIVE_DOWNGRADE` 或 `GUEST_SANDBOX_ONLY`；请求 `STRICT` 时必须 fail-closed 拒绝，严禁静默降级（`[BUD-TIER-001]`）。
3. **`[RSM-ENFORCE-001]`（观测与声明不得冒充强制）**：仅具备 `ResourceDemand::{cpu_shares, memory_mib, io_weight}` 声明对比、`WorkingSetReclaimOutcome::evicted_units` 合成计数或 `OpenMetrics` 导出的路径，在未绑定宿主强制执行点前最高只能标记 `COOPERATIVE_DOWNGRADE` 或 `METERED_OBSERVED`。
4. **`[EVID-SCOPE-001]`（跨平台与生产边界诚实）**：当前全仓 `H8 = 0`（无生产现场长稳证据）；macOS 单机实测与 Wasmtime 沙箱结果不得外推为 Linux `cgroup v2`、Windows `Job Object` 或真实硬件设备的已验证能力。

---

## 2. 12 类现代资源维度 × 3 平台（Linux / macOS / Windows）分级约束画像

### 2.1 12 × 3 分级约束总览矩阵

> 记号约定：当同一维度在「Wasm 客体 / 受控 SABI-Service 边界」与「宿主 OS `ResourceGroup` / 全局物理边界」之间存在分层差异时，矩阵显式双标两层真实等级（`客体或局部边界 / 宿主或全局边界`），绝不以局部强边界掩盖全局缺口。

| # | 规范资源维度（[v0.5 §10.0](../design/06-架构设计总纲-v0.5.md#100-现代系统方法与-nlos-资源平面)） | Linux | macOS (arm64/x86_64) | Windows (x86_64) | 关键源码 crate / 模块锚点 | 已有 Evidence ID（最高等级） | `[TCB-DOWNGRADE-001]` / `[ROAD-A-004]` 红线与核心缺口 |
|---|---|---|---|---|---|---|---|
| 1 | **CPU** | `GUEST_SANDBOX_ONLY`（Wasm `fuel`/epoch + `killpg`） / `COOPERATIVE_DOWNGRADE`（宿主 `ResourceGroup` 无 `cgroup v2`） | `GUEST_SANDBOX_ONLY`（Wasm `fuel`/epoch + `killpg`） / `COOPERATIVE_DOWNGRADE`（宿主无 `cpu.max` 对等原语） | `GUEST_SANDBOX_ONLY`（Wasm `fuel`/epoch + `TerminateProcess`） / `COOPERATIVE_DOWNGRADE`（无 Job CPU rate + 无 safe suspend） | [`nlos-wasm-sandbox/src/isolate.rs`](../../crates/nlos-wasm-sandbox/src/isolate.rs), [`process.rs`](../../crates/nlos-wasm-sandbox/src/process.rs), [`nlos-process/src/platform_kill.rs`](../../crates/nlos-process/src/platform_kill.rs), [`supervisor.rs`](../../crates/nlos-process/src/supervisor.rs), [`managed.rs`](../../crates/nlos-process/src/managed.rs), [`nlos-resource/src/lib.rs`](../../crates/nlos-resource/src/lib.rs), [`nlos-runtime-tokio/src/distributed_scheduler.rs`](../../crates/nlos-runtime-tokio/src/distributed_scheduler.rs) | `e-sandbox-001-cross-platform-wasm-process-isolation` (`H4`), `w82-b3-tool-service-20261116` (`H4`), `b-sandbox-001-isolation-comparison` (`POC`), `b-process-003-crash-propagation` (`H3`), `c-sched-001-three-tier-scheduler-domain` (`H3`) | `ResourceDemand::cpu_shares` 与用户态 WFQ/DRR 不强制宿主核时间；缺 `cgroup v2 cpu.max` / Job CPU Rate Control；Windows 无 safe suspend 且留存 `U-2` 跨权限/breakaway 残域 |
| 2 | **内存 (Memory)** | `GUEST_SANDBOX_ONLY`（Wasm `PageLimiter`） / `COOPERATIVE_DOWNGRADE`（宿主无 `cgroup v2 memory.max`） | `GUEST_SANDBOX_ONLY`（Wasm `PageLimiter`） / `UNSUPPORTED`（宿主 `RLIMIT_AS` 实测 `EINVAL`） | `GUEST_SANDBOX_ONLY`（Wasm `PageLimiter`） / `COOPERATIVE_DOWNGRADE`（宿主无 Job Memory 硬限） | [`nlos-wasm-sandbox/src/isolate.rs`](../../crates/nlos-wasm-sandbox/src/isolate.rs), [`profile.rs`](../../crates/nlos-wasm-sandbox/src/profile.rs), [`nlos-resource/src/lib.rs`](../../crates/nlos-resource/src/lib.rs), [`nlos-task/src/pressure.rs`](../../crates/nlos-task/src/pressure.rs), [`nlos-system-control/src/working_set_reclaim_executor.rs`](../../crates/nlos-system-control/src/working_set_reclaim_executor.rs) | `b-sandbox-001-isolation-comparison` (`POC`), `e-sandbox-001-cross-platform-wasm-process-isolation` (`H4`), `w82-b3-tool-service-20261116` (`H4`), `b-res-profile-audit-20261121` (`H3`) | macOS 禁止用 `setrlimit(RLIMIT_AS)` 充当内存配额；`memory_mib` 与合成 `evicted_units` 不等于 OS 物理内存/OOM 控制器；原生进程内存无宿主硬限 |
| 3 | **存储 (Storage)** | `GUEST_SANDBOX_ONLY`（沙箱 8 MiB/4096 次） + `STRICT_ENFORCED`（服务 journal 自限） / `COOPERATIVE_DOWNGRADE`（全局 FS quota） | `GUEST_SANDBOX_ONLY`（沙箱 8 MiB/4096 次） + `STRICT_ENFORCED`（服务 journal 自限） / `COOPERATIVE_DOWNGRADE`（全局 FS quota） | `GUEST_SANDBOX_ONLY`（沙箱 8 MiB/4096 次） + `STRICT_ENFORCED`（服务 journal 自限） / `COOPERATIVE_DOWNGRADE`（全局 FS quota） | [`nlos-wasm-sandbox/src/tool_host_protocol.rs`](../../crates/nlos-wasm-sandbox/src/tool_host_protocol.rs), [`wasi_allowlist.rs`](../../crates/nlos-wasm-sandbox/src/wasi_allowlist.rs), [`tool_operation_store.rs`](../../crates/nlos-wasm-sandbox/src/tool_operation_store.rs), [`nlos-capability/src/blob_ledger.rs`](../../crates/nlos-capability/src/blob_ledger.rs), [`nlos-artifact/src/lib.rs`](../../crates/nlos-artifact/src/lib.rs), [`nlos-driver-llm-http/src/llm_gateway.rs`](../../crates/nlos-driver-llm-http/src/llm_gateway.rs) | `w82-b3-tool-service-20261116` (`H4`), `w82-b4-blob-service-20261116` (`H4`), `b-artifact-001-content-addressed-store` (`H3`), `b-artifact-005-retention-policy` (`H3`), `b-store-fs-semantics-001-apfs-calibration` (`POC`) | 工具沙箱 8 MiB 单次写与单一服务 journal 上限不等于工作区累计磁盘配额；缺跨组 FS quota 与 `[RSM-RESERVE-001]` `ENOSPC` 救援保留池；层 3 物理断电未测（`U-4`） |
| 4 | **I/O** | `GUEST_SANDBOX_ONLY`（FD/IOV/stdio 界） / `COOPERATIVE_DOWNGRADE`（宿主无 `io.max`） | `GUEST_SANDBOX_ONLY`（FD/IOV/stdio 界） / `UNSUPPORTED`（macOS 无每组块 I/O 带宽硬限原语） | `GUEST_SANDBOX_ONLY`（FD/IOV/stdio 界） / `COOPERATIVE_DOWNGRADE`（无 Job I/O 计数与 Storage QoS） | [`nlos-resource/src/lib.rs`](../../crates/nlos-resource/src/lib.rs), [`schema.rs`](../../crates/nlos-resource/src/schema.rs), [`nlos-system-control/src/resource_throttle_executor.rs`](../../crates/nlos-system-control/src/resource_throttle_executor.rs), [`nlos-wasm-sandbox/src/wasi_allowlist.rs`](../../crates/nlos-wasm-sandbox/src/wasi_allowlist.rs), [`tool_process.rs`](../../crates/nlos-wasm-sandbox/src/tool_process.rs) | `b-resource-001-driver-reservation-binding-authority` (`H3`), `w82-b3-tool-service-20261116` (`H4`), `b-res-profile-audit-20261121` (`H3`) | `ResourceDemand::io_weight` 与 `record_throttle_decision` 只写 SQLite 决策账本，不驱动内核块/文件 I/O 节流；Wasm `MAX_OPEN_FDS=256`/`MAX_IOV_TOTAL=4 MiB` 仅限客体 |
| 5 | **网络 (Network)** | `GUEST_SANDBOX_ONLY`（Wasm 零 socket） + `STRICT_ENFORCED`（`nlos-net` mTLS mux） / `COOPERATIVE_DOWNGRADE`（宿主全局 egress/QoS） | `GUEST_SANDBOX_ONLY`（Wasm 零 socket） + `STRICT_ENFORCED`（`nlos-net` mTLS mux） / `COOPERATIVE_DOWNGRADE`（宿主全局 egress/QoS） | `GUEST_SANDBOX_ONLY`（Wasm 零 socket） + `STRICT_ENFORCED`（`nlos-net` mTLS mux） / `COOPERATIVE_DOWNGRADE`（宿主全局 egress/QoS） | [`nlos-wasm-sandbox/src/wasi_allowlist.rs`](../../crates/nlos-wasm-sandbox/src/wasi_allowlist.rs), [`nlos-net/src/lib.rs`](../../crates/nlos-net/src/lib.rs), [`profile.rs`](../../crates/nlos-net/src/profile.rs), [`mux.rs`](../../crates/nlos-net/src/mux.rs), [`nlos-driver-llm-http/src/http.rs`](../../crates/nlos-driver-llm-http/src/http.rs) | `e-sandbox-001-cross-platform-wasm-process-isolation` (`H4`), `c-xhost-001-mtls-transport-and-fence-handshake` (`H5`), `e-trial-001-llm-http-driver` (`H4`) | 仅 Wasm 客体与 `nlos-net` 受控链路被中介；缺 Linux netns / Windows WFP / macOS pf 宿主级默认拒绝防火墙与带宽整形，原生进程直连外网不可防旁路（`[TCB-EGRESS-001]`） |
| 6 | **加速器 (Accelerator)** | `UNSUPPORTED`（物理驱动/VRAM/清零） / `COOPERATIVE_DOWNGRADE`（`ExclusiveDeviceLease` 仅软件账本） | `UNSUPPORTED`（物理驱动/VRAM/清零） / `COOPERATIVE_DOWNGRADE`（`ExclusiveDeviceLease` 仅软件账本） | `UNSUPPORTED`（物理驱动/VRAM/清零） / `COOPERATIVE_DOWNGRADE`（`ExclusiveDeviceLease` 仅软件账本） | [`nlos-lease/src/lib.rs`](../../crates/nlos-lease/src/lib.rs), [`reconcile.rs`](../../crates/nlos-lease/src/reconcile.rs), [`clock_horizon.rs`](../../crates/nlos-lease/src/clock_horizon.rs) | `c-lease-001-durable-three-family-reconciliation` (`H3`), `c-xhost-002-conservative-clock-skew-and-remote-lease` (`H5`), `b-res-profile-audit-20261121` (`H3`) | 全仓无 GPU/NPU/MIG/Metal/DirectML 驱动与硬件复位执行器；`QuarantineDisposition::LedgerQuarantinedFenceOnly` 不得冒充显存/硬件物理擦除（`U-5`，物理台架归 `DEL-EXT`） |
| 7 | **能源 (Energy)** | `UNSUPPORTED` | `UNSUPPORTED` | `UNSUPPORTED` | 无实现锚点（仅规范 [v0.5 §10.0](../design/06-架构设计总纲-v0.5.md#100-现代系统方法与-nlos-资源平面)） | `b-res-profile-audit-20261121` (`H3` 代码核验零实现) | 无 `energy_mJ`/`power_mW`/DVFS/电池状态或热降频控制器；严禁以超时或 CPU 限制冒充能源预算，物理硬件测量归 `DEL-EXT` |
| 8 | **费用 (Cost)** | `METERED_OBSERVED` | `METERED_OBSERVED` | `METERED_OBSERVED` | [`nlos-resource/src/lib.rs`](../../crates/nlos-resource/src/lib.rs), [`schema.rs`](../../crates/nlos-resource/src/schema.rs), [`nlos-driver-llm-http/src/llm_gateway.rs`](../../crates/nlos-driver-llm-http/src/llm_gateway.rs), [`shim.rs`](../../crates/nlos-driver-llm-http/src/shim.rs), [`nlos-lease/src/lib.rs`](../../crates/nlos-lease/src/lib.rs) | `b-resource-001-driver-reservation-binding-authority`..`b-resource-006-cost-receipt-aggregate` (`H3`), `c-lease-001-durable-three-family-reconciliation` (`H3`), `c-custody-001-key-custody-gateway-reconciliation` (`H3`), `c-partition-001-partition-autonomy-bounded-loss` (`H6`), `e-trial-001-llm-http-driver` (`H4`), `w82-b2-llm-gateway-20261116` (`H4`) | 内部双分录账本与 `QuotaLease` 守恒成立，但外部商业 LLM 仍标 `OBSERVED` + `PROVIDER_ISOLATION_UNVERIFIED`；failover/不可观察响应/宿外 API 直调仍可超出内部预算；严禁冒充物理 `ResourceGroup` |
| 9 | **模型上下文 (Model Context)** | `METERED_OBSERVED` | `METERED_OBSERVED` | `METERED_OBSERVED` | [`nlos-resource/src/lib.rs`](../../crates/nlos-resource/src/lib.rs), [`nlos-driver-llm-http/src/gateway_pricing.rs`](../../crates/nlos-driver-llm-http/src/gateway_pricing.rs), [`llm_gateway.rs`](../../crates/nlos-driver-llm-http/src/llm_gateway.rs), [`nlos-task/src/pressure.rs`](../../crates/nlos-task/src/pressure.rs), [`nlos-system-control/src/working_set_reclaim_executor.rs`](../../crates/nlos-system-control/src/working_set_reclaim_executor.rs), [`nlos-artifact/src/lib.rs`](../../crates/nlos-artifact/src/lib.rs) | `w82-b2-llm-gateway-20261116` (`H4`), `e-trial-001-llm-http-driver` (`H4`), `b-task-scale-001` (`H3`), `b-artifact-001-content-addressed-store` (`H3`) | 仅对经 `Gateway` 的请求强制 Prompt/Completion/Total token 门限与 `16,384` 输出帽；无真实 KV-cache/RAG 内存驻留控制器，`WorkingSetReclaimOutcome::evicted_units` 为合成计数占位 |
| 10 | **消息容量 (Message Capacity)** | `STRICT_ENFORCED`（SABI/Channel/Topic/Blob/Net Mux） / `COOPERATIVE_DOWNGRADE`（宿外原生 IPC） | `STRICT_ENFORCED`（SABI/Channel/Topic/Blob/Net Mux） / `COOPERATIVE_DOWNGRADE`（宿外原生 IPC） | `STRICT_ENFORCED`（SABI/Channel/Topic/Blob/Net Mux） / `COOPERATIVE_DOWNGRADE`（宿外原生 IPC） | [`nlos-schema/src/lib.rs`](../../crates/nlos-schema/src/lib.rs), [`nlos-channel/src/lib.rs`](../../crates/nlos-channel/src/lib.rs), [`nlos-topic/src/lib.rs`](../../crates/nlos-topic/src/lib.rs), [`nlos-capability/src/blob_model.rs`](../../crates/nlos-capability/src/blob_model.rs), [`nlos-net/src/frame.rs`](../../crates/nlos-net/src/frame.rs) | `b-channel-001-endpoint-authority` (`H3`), `b-topic-001-topic-service-single-log-fanout` (`H3`), `w82-a-verify-20261116` (`H4`), `w82-b-contract-20261116` (`H4`), `w82-b4-blob-service-20261116` (`H4`) | 12 通道 SABI 单帧界（`64 KiB` 等）、`Channel` `capacity_bytes`、`Topic` `retained_bytes`/`max_recipients`/`cascade_depth` 严格执行；非受控原生进程自建 OS 管道/共享内存不在此界内 |
| 11 | **速率/配额 (Rate/Quota)** | `STRICT_ENFORCED`（单权威服务并发槽/窗口） / `COOPERATIVE_DOWNGRADE`（跨进程全局速率与系统配额） | `STRICT_ENFORCED`（单权威服务并发槽/窗口） / `COOPERATIVE_DOWNGRADE`（跨进程全局速率与系统配额） | `STRICT_ENFORCED`（单权威服务并发槽/窗口） / `COOPERATIVE_DOWNGRADE`（跨进程全局速率与系统配额） | [`nlos-driver-llm-http/src/llm_gateway.rs`](../../crates/nlos-driver-llm-http/src/llm_gateway.rs), [`nlos-wasm-sandbox/src/tool_service.rs`](../../crates/nlos-wasm-sandbox/src/tool_service.rs), [`nlos-capability/src/blob_model.rs`](../../crates/nlos-capability/src/blob_model.rs), [`nlos-lease/src/lib.rs`](../../crates/nlos-lease/src/lib.rs), [`nlos-runtime-tokio/src/distributed_scheduler.rs`](../../crates/nlos-runtime-tokio/src/distributed_scheduler.rs) | `w82-b2-llm-gateway-20261116` (`H4`), `w82-b3-tool-service-20261116` (`H4`), `w82-b4-blob-service-20261116` (`H4`), `c-lease-001-durable-three-family-reconciliation` (`H3`), `c-sched-001-three-tier-scheduler-domain` (`H3`) | 各服务各自强制并发槽与 60s 窗口速率（如 `ToolHostQuota` 16/4、`GatewayLimits` 32/4、`BlobConfig` 读/铸 60s 窗），但 `W82-INT` daemon 挂载与跨 OS 进程全局协调待闭环，且不得与信用混算 |
| 12 | **人类注意力 (Human Attention)** | `STRICT_ENFORCED`（`nlos-notify` 边车策略与 `Z4` 防遮挡） / `COOPERATIVE_DOWNGRADE`（桌面 `Z4`/UI 与分发总线未连线，`H8=0`） | `STRICT_ENFORCED`（`nlos-notify` 边车策略与 `Z4` 防遮挡） / `COOPERATIVE_DOWNGRADE`（桌面 `Z4`/UI 与分发总线未连线，`H8=0`） | `STRICT_ENFORCED`（`nlos-notify` 边车策略与 `Z4` 防遮挡） / `COOPERATIVE_DOWNGRADE`（桌面 `Z4`/UI 与分发总线未连线，`H8=0`） | [`desktop/src-tauri/src/trusted_ui.rs`](../../desktop/src-tauri/src/trusted_ui.rs), [`desktop/src-tauri/src/settings_keychain.rs`](../../desktop/src-tauri/src/settings_keychain.rs), [`nlos-notify/src/lib.rs`](../../crates/nlos-notify/src/lib.rs), [`nlos-notify/src/attention.rs`](../../crates/nlos-notify/src/attention.rs), [`task_notify.rs`](../../crates/nlos-notify/src/task_notify.rs) | `d-notify-attention-policy-20261122` (`H4`), `d-trusted-ui-001-z4-consent-chooser-and-taint-clipboard` (`H4`), `d-compositor-001-five-plane-tiling-wm-and-session` (`H4`), `d-notify-search-001-action-token-spotlight-and-offline` (`H4`), `b-notify-001-minimal-service` (`H3`) | `nlos-notify` 侧已实证 `AttentionPolicyController`（`attention-policy.db` v1 边车：`WAL+FULL` 不可变审计、双队列隔离、跨午夜静默时段、同类聚合、单应用/全局频控、付费绕过/伪造 `ConsentPrompt` 拒接）与 `Z4` 防点击劫持；桌面端 Tauri `Z4`/UI 与通知分发总线真实连线（`DEL-UX`）及真实人类试用试点（`DEL-FIELD`, `H8 = 0`）仍未完成，整机/UI 端维持受限分级 |

---

### 2.2 12 维现网深度核验明细

#### 1. CPU
- **规范目标**：`[NLOS-RESOURCE-001]`、`[ROAD-A-004]`、`[FIBER-METER-001]`、`[RSM-SCHED-001]`、`[RSM-METER-002]`。
- **现网实证事实**：
  - **Wasm 沙箱层（三平台 `GUEST_SANDBOX_ONLY`）**：[`crates/nlos-wasm-sandbox/src/isolate.rs`](../../crates/nlos-wasm-sandbox/src/isolate.rs) 通过 `Config::consume_fuel(true)` 与 `Store::set_fuel` 提供确定性字节码算力配额（耗尽触发 `TrapKind::OutOfFuel`），通过 `EpochTicker` 后台线程与 `epoch_interruption(true)` 提供墙钟切片中断（触发 `TrapKind::EpochInterrupt`）；[`crates/nlos-wasm-sandbox/src/process.rs`](../../crates/nlos-wasm-sandbox/src/process.rs) 与 [`tool_process.rs`](../../crates/nlos-wasm-sandbox/src/tool_process.rs) 在父进程侧维护墙钟死线（`MAX_WALL_MS = 30_000`），超时即执行 OS 硬杀（Unix `SIGKILL` / Windows `TerminateProcess`）。
  - **进程与纤程控制层（`COOPERATIVE_DOWNGRADE`）**：[`crates/nlos-process/src/platform_kill.rs`](../../crates/nlos-process/src/platform_kill.rs) 与 [`managed.rs`](../../crates/nlos-process/src/managed.rs) 在 Unix 使用 `CommandExt::process_group(0)` + `killpg(SIGTERM/SIGKILL)`（含 Darwin `killpg` 对全僵尸空组返回 `EPERM` 映射为 `AlreadyTerminated` 的实证处理），在 [`supervisor.rs`](../../crates/nlos-process/src/supervisor.rs) 支持 Unix `SIGSTOP`/`SIGCONT` 挂起恢复；Windows 仅支持 `taskkill /F /T` 整树强杀，无安全 `suspend`/`resume`（显式返回 `UnsupportedOnPlatform`），且存在 `U-2` 跨权限/Job breakaway 残域。[`crates/nlos-runtime-tokio/src/distributed_scheduler.rs`](../../crates/nlos-runtime-tokio/src/distributed_scheduler.rs) 实现用户态 `Global -> Cell -> Worker` WFQ/DRR 调度与 `parent_hard_quota_in_flight` 并发槽控制，[`crates/nlos-runtime-tokio/src/lib.rs`](../../crates/nlos-runtime-tokio/src/lib.rs) 通过 `UsageAccumulator` 分维计量 `Running`/`WaitingExternal`/`BackpressureWait`/`Suspended` 时长（`[FIBER-METER-001]`）。
  - **核心缺口**：[`crates/nlos-resource/src/lib.rs`](../../crates/nlos-resource/src/lib.rs) 中 `ResourceDemand::cpu_shares` 与 `record_throttle_decision` 仅为 SQLite 声明与决策记录；**三平台均未实现宿主 OS 级 CPU 带宽/权重/核数控制器**（Linux 缺 `cgroup v2 cpu.max`/`cpu.weight`，Windows 缺 `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION`，macOS 缺乏宿主组 CPU 配额接口）。
- **红线**：严禁将 Wasm `fuel`、用户态 WFQ 权重或 `ResourceDemand::cpu_shares` 宣称为原生进程的 OS CPU 硬限。

#### 2. 内存 (Memory)
- **规范目标**：`[NLOS-RESOURCE-001]`、`[ROAD-A-004]`、`[RSM-OOM-001]`、`[RSM-RECLAIM-001]`。
- **现网实证事实**：
  - **Wasm 沙箱层（三平台 `GUEST_SANDBOX_ONLY`）**：[`crates/nlos-wasm-sandbox/src/isolate.rs`](../../crates/nlos-wasm-sandbox/src/isolate.rs) 实现 `PageLimiter`（`wasmtime::ResourceLimiter`），在模块实例化与每次 `memory.grow` 同步强制 64 KiB 页上限（[`profile.rs`](../../crates/nlos-wasm-sandbox/src/profile.rs) 默认 `DEFAULT_MEMORY_PAGES = 16` 即 1 MiB，最大 `MAX_WASM_MEMORY_PAGES = 65_536` 即 4 GiB；同步截断 `MAX_TABLE_ELEMENTS = 1024`、`MAX_INSTANCES = 8`、`MAX_MEMORIES = 2`，工具模式下强制单线性内存 `1`）。
  - **macOS 宿主原语否定实证（`UNSUPPORTED`）**：[b-sandbox-001-isolation-comparison](../evidence/stage-b/b-sandbox-001-isolation-comparison.md) §2 D2 已在 macOS arm64 实测 `setrlimit(RLIMIT_AS, ...)` 无论 64 MB 还是 2 GB 均返回 `EINVAL`，子进程无阻碍分配至 256 MB/4 GB，`RLIMIT_RSS` 亦为历史 no-op；macOS 无内核级 cgroup/Task 内存硬限对等接口。
  - **Linux/Windows 宿主与 Task 工作集层（`COOPERATIVE_DOWNGRADE`）**：[`crates/nlos-resource/src/lib.rs`](../../crates/nlos-resource/src/lib.rs) 中 `ResourceDemand::memory_mib` 仅作 Quote/Reservation 准入数值比较；[`crates/nlos-task/src/pressure.rs`](../../crates/nlos-task/src/pressure.rs) 与 [`crates/nlos-system-control/src/working_set_reclaim_executor.rs`](../../crates/nlos-system-control/src/working_set_reclaim_executor.rs) 仅统计 `CommitPermit` 计数并产出**合成占位计数** `WorkingSetReclaimOutcome::evicted_units`。三平台均未实现宿主级物理内存限额、PSI/压力事件驱动回收与真实 OOM Receipt 联动。
- **红线**：macOS 禁止任何 `RLIMIT_AS` 假强制方案；合成 `evicted_units` 与 `memory_mib` 账本字段不得冒充物理内存隔离。

#### 3. 存储 (Storage)
- **规范目标**：`[ROAD-A-004]`、`[RSM-ADMIT-001]`、`[RSM-RESERVE-001]`、`[ART-LOCAL-001]`。
- **现网实证事实**：
  - **沙箱单次写界（`GUEST_SANDBOX_ONLY`）**：[`crates/nlos-wasm-sandbox/src/tool_host_protocol.rs`](../../crates/nlos-wasm-sandbox/src/tool_host_protocol.rs) 固定 `MAX_FILE_WRITE = 8 MiB`、`MAX_WASM_BYTES = 16 MiB`；[`wasi_allowlist.rs`](../../crates/nlos-wasm-sandbox/src/wasi_allowlist.rs) 限制每次运行最多 `creation_attempts_remaining = 4096` 次创建、关闭可写描述符 `fd_seek` 与 `oflags::TRUNC`、通过词法规范化 + `canonicalize` 父目录 + `FollowSymlinks::No` + `nlink() == 1` 阻断只读逃逸与链接攻击（注意 Windows `writable_grant_refuses_a_symbolic_link_as_file` 在非提权账户曾命中 `os_error=1314`，后在 CI 覆盖）。
  - **受控服务审计日志自限（`STRICT_ENFORCED` 局部边界）**：[`crates/nlos-capability/src/blob_ledger.rs`](../../crates/nlos-capability/src/blob_ledger.rs) 的 `BlobLedger::guard_storage` 在每次写入前以主库页数 + WAL + B-tree 最坏分裂膨胀量检查 `BlobConfig::{audit_bytes, audit_rows}`，防止无界膨胀；[`crates/nlos-driver-llm-http/src/llm_gateway.rs`](../../crates/nlos-driver-llm-http/src/llm_gateway.rs) 与 [`crates/nlos-wasm-sandbox/src/tool_operation_store.rs`](../../crates/nlos-wasm-sandbox/src/tool_operation_store.rs) 各自强制 192 MiB/196,608 条准入水位与 256 MiB/262,144 条硬上限。
  - **宿主工作区磁盘配额与系统救援池（`COOPERATIVE_DOWNGRADE`）**：[`crates/nlos-artifact/src/lib.rs`](../../crates/nlos-artifact/src/lib.rs) 实现 `artifacts/` 与 `cache/` 双目录隔离、`collect_orphan_blobs` 保守 GC 与 `set_retention` 过期只读拒绝，但**尚无跨调用累计工作区磁盘配额、OS 文件系统 quota controller、`[RSM-RESERVE-001]` 系统关键元数据 `ENOSPC` 救援池**，且层 3 真实物理断电测试（`U-4`）尚未运行（仅有 [`nlos-store-fault`](../../crates/nlos-store-fault/src/lib.rs) `FaultCode::Full` 进程内模拟与层 1/2 测试）。
- **红线**：单次调用 `8 MiB` 不阻止工具被多次调用填满磁盘；`BlobLedger` 自限不代表宿主文件系统拥有 per-`ResourceGroup` 磁盘硬配额。

#### 4. I/O
- **规范目标**：`[ROAD-A-004]`、`[RSM-CONTROLLER-001]`、`v0.5 §10.0`。
- **现网实证事实**：
  - **沙箱句柄与管道流界（`GUEST_SANDBOX_ONLY`）**：[`crates/nlos-wasm-sandbox/src/wasi_allowlist.rs`](../../crates/nlos-wasm-sandbox/src/wasi_allowlist.rs) 限制 `MAX_OPEN_FDS = 256`、`MAX_IOV_TOTAL = 4 MiB`；[`tool_host_protocol.rs`](../../crates/nlos-wasm-sandbox/src/tool_host_protocol.rs) 限制 `MAX_STDOUT = 256 KiB`、`MAX_GUEST_STDERR = 60 KiB`；[`tool_process.rs`](../../crates/nlos-wasm-sandbox/src/tool_process.rs) 双线程异步排空管道防止管道满阻塞死锁。
  - **宿主块/文件 I/O 调度控制（Linux/Windows `COOPERATIVE_DOWNGRADE`；macOS `UNSUPPORTED`）**：[`crates/nlos-resource/src/lib.rs`](../../crates/nlos-resource/src/lib.rs) 与 [`crates/nlos-system-control/src/resource_throttle_executor.rs`](../../crates/nlos-system-control/src/resource_throttle_executor.rs) 支持将 `ResourceDemand::io_weight` 写入不可变 SQLite `reservation_throttle_decisions` 表，但只作账本记录；三平台均无驱动内核 `cgroup v2 io.max` 或存储 QoS 的块/文件 I/O 带宽/IOPS 控制器（macOS 在内核层面无每 `ResourceGroup` 块 I/O 带宽硬限原语）。
- **红线**：`record_throttle_decision` 成功返 Receipt 只证明账本状态迁移，不代表 OS 底层 I/O 吞吐已被限速。

#### 5. 网络 (Network)
- **规范目标**：`[TCB-EGRESS-001]`、`[NLOS-RESOURCE-001]`、`[DIST-NAME-001]`。
- **现网实证事实**：
  - **Wasm 沙箱（三平台 `GUEST_SANDBOX_ONLY`）**：[`crates/nlos-wasm-sandbox/src/wasi_allowlist.rs`](../../crates/nlos-wasm-sandbox/src/wasi_allowlist.rs) 在单一注册点 `add_to_linker` 中**完全不注册任何 WASI socket 导入**（`sock_open`/`sock_accept`/`sock_recv`/`sock_send`/`sock_shutdown` 均不存在），任何试图导入网络原语的模块在实例化时即 fail-closed 拒绝。
  - **跨主机集群通道（局部 `STRICT_ENFORCED`）**：[`crates/nlos-net/src/lib.rs`](../../crates/nlos-net/src/lib.rs)、[`profile.rs`](../../crates/nlos-net/src/profile.rs)、[`frame.rs`](../../crates/nlos-net/src/frame.rs) 与 [`mux.rs`](../../crates/nlos-net/src/mux.rs) 在 `ClusterProfile::SingleHostDesktop` 下强制绑定 **0** 个 socket；跨机模式下强制 TLS 1.3 mTLS 五轴 Exporter 认证（`0-RTT` 禁用）与 `ControlRpc` 优先于 `<= 64 KiB` `BulkChunk` 双流信用背压。
  - **LLM HTTP 客户端与宿主全局 egress（`METERED_OBSERVED` / `COOPERATIVE_DOWNGRADE`）**：[`crates/nlos-driver-llm-http/src/http.rs`](../../crates/nlos-driver-llm-http/src/http.rs) 封装纯用户态有界 `HTTP/1.1`（`64 KiB` header / `16 MiB` body / `60_000 ms` timeout），但**三平台均未实现宿主 OS 级默认拒绝防火墙 / network namespace / WFP / pf 中介以及带宽 QoS**，因此无法阻止非沙箱原生进程或恶意第三方二进制绕过 gateway 直连公网/metadata/loopback。
- **红线**：依 `[TCB-EGRESS-001]` 与 `[NLOS-HOST-002]`，在宿主级默认拒绝 egress 围栏未对原生进程生效前，凡运行非 Wasm 原生工具/进程的部署均属 `COOPERATIVE_DOWNGRADE`。

#### 6. 加速器 (Accelerator)
- **规范目标**：`[NLOS-RESOURCE-001]`、`[LEASE-DEVICE-001]`、`[RSM-DIST-002]`、`[RSM-RECLAIM-003]`。
- **现网实证事实**：
  - **控制面租约契约（`COOPERATIVE_DOWNGRADE`）**：[`crates/nlos-lease/src/lib.rs`](../../crates/nlos-lease/src/lib.rs) 与 [`reconcile.rs`](../../crates/nlos-lease/src/reconcile.rs) 已实现 `ExclusiveDeviceLeaseGrantor`（`DeviceLeaseHead` 单占用 CAS 与 monotonic `reset_generation`）、`DeviceResetReceipt` 与 `DeviceReconciliationReceipt`（字段显式包含 `endpoint_fenced_old_token`、`queue_drained`、`vram_and_secrets_zeroized`）；[`clock_horizon.rs`](../../crates/nlos-lease/src/clock_horizon.rs) 通过 `QuarantineDisposition` 严格区分 `LedgerQuarantinedFenceOnly` 与 `PhysicallyCleanedVerified`。
  - **物理驱动与硬件执行层（`UNSUPPORTED`）**：经全仓库代码检索，**无任何 GPU/NPU/CUDA/ROCm/Metal/DirectML/IOMMU/MIG 设备驱动、显存配额强杀或物理清零执行器**。
- **红线**：`DeviceReconciliationReceipt` 的布尔/摘要校验目前依赖外部注入证据；在 `DEL-EXT` 接入真实硬件驱动与复位台架前，加速器维度对外一律标 `UNSUPPORTED`（`U-5` / `RISK-B-11`）。

#### 7. 能源 (Energy)
- **规范目标**：`[RSM-ENERGY-001]`、`[RSM-METER-002]`。
- **现网实证事实**：全仓 `crates/` 与 `desktop/` **零实现代码**——无 `energy_mJ`、`power_mW`、热节流事件读取、`DVFS` 策略绑定或电池状态挂钩调度器；三平台一致为 **`UNSUPPORTED`**。
- **红线**：`OperationRegistry` 不得广告能源 controller；能源/功耗/热降频闭环及物理测功设备依赖 `DEL-EXT` 后续立项。

#### 8. 费用 (Cost)
- **规范目标**：`[BUD-INT-001]`、`[BUD-ALG-001]`、`[BUD-ARITH-001]`、`[BUD-CONS-001/002]`、`[BUD-TIER-001..003]`。
- **现网实证事实（三平台统一 `METERED_OBSERVED`）**：
  - **强软件账本层**：[`crates/nlos-resource/src/lib.rs`](../../crates/nlos-resource/src/lib.rs) 与 [`schema.rs`](../../crates/nlos-resource/src/schema.rs) v1–v8 实现 SQLite WAL+FULL 原子账本、checked 整数运算、单调 consumption 高水位、fail-closed `Quarantined` 冻结及双分录 `FinalizationReceipt` 退款守恒；[`crates/nlos-lease/src/lib.rs`](../../crates/nlos-lease/src/lib.rs) 实现预付 `QuotaLease` 跨 Cell 守恒与 `FenceBarrierAck` 对账。
  - **LLM 网关垫片（显式 `OBSERVED` 边界）**：[`crates/nlos-driver-llm-http/src/llm_gateway.rs`](../../crates/nlos-driver-llm-http/src/llm_gateway.rs) 先在本地日志持久化 `PREPARED` 报价与 `SETTLEMENT_INTENT`，校验全部祖先包络后调用 `ResourceAuthority::consume_llm_tokens`，并将不可信外部自报归入隔离的 `advisory` 区域；[`shim.rs`](../../crates/nlos-driver-llm-http/src/shim.rs) 强制注入 `PROVIDER_ISOLATION_UNVERIFIED_TAG`。但如 `llm_gateway.rs` 源码注释及 [W82-B2 Evidence](../evidence/stage-e/w82-b2-llm-gateway-20261116.md) 明示：provider failover 重试（最多 8 次底层尝试）、流式/断流不可观察用量及未经过网关的外部真实计费仍可高于账本记录。
- **红线**：当前费用维度最高仅为 `OBSERVED`（`BOUNDED_RISK` / `STRICT` 外部结算与商业 provider H5 对账未完成），且**绝不能**把费用账户余额解释为物理计算容量或 OS 隔离边界。

#### 9. 模型上下文 (Model Context)
- **规范目标**：`[NLOS-RESOURCE-001]`、`[CTX-NOTDATA-001]`、`[RSM-RECLAIM-001]`、`[RSM-METER-002]`。
- **现网实证事实（三平台统一 `METERED_OBSERVED`）**：
  - **Token 窗口准入**：`ResourceAuthority` v8 `LlmTokenLimits` 与 `Gateway` 服务端按 `UTF-8` 字节 + turn framing 估算 prompt 上界，并由 [`gateway_pricing.rs`](../../crates/nlos-driver-llm-http/src/gateway_pricing.rs) 固定 `MAX_MODEL_OUTPUT_TOKENS = 16_384` 及推理 token 账目布局（`NoReasoning` / `InclusiveReasoning` / `SeparateReasoning`）。
  - **Working-set 回收占位**：[`crates/nlos-task/src/pressure.rs`](../../crates/nlos-task/src/pressure.rs) 与 [`crates/nlos-system-control/src/working_set_reclaim_executor.rs`](../../crates/nlos-system-control/src/working_set_reclaim_executor.rs) 对 `CommitPermit` 活跃数判定软阈值并执行 `TASK_DEFAULT_RECLAIM_POLICY` 第一阶段，但模块注释明确写明 `evicted_units` 为**合成占位计数**，尚未连接真实 Context Residency / KV-cache / embedding 内存驱逐控制器。
- **红线**：Token 配额只约束经网关的模型调用；真实 Context 常驻内存回收属于 `DEL-RES` 与 `DEL-DATA` 待建能力。

#### 10. 消息容量 (Message Capacity)
- **规范目标**：`[RSM-FANOUT-001]`、`[COMPAT-WIRE-001]`、`v0.5 §10.0`。
- **现网实证事实（受控协议面 `STRICT_ENFORCED` / 宿外原生 IPC `COOPERATIVE_DOWNGRADE`）**：
  - [`crates/nlos-schema/src/lib.rs`](../../crates/nlos-schema/src/lib.rs) 在 12 个 SABI 通道上强制单帧编码载荷硬界（`MAX_ENVELOPE_BYTES = 1 MiB`、业务通道 `64 KiB`/`16 KiB`/`4 KiB`、`MAX_BLOB_REFERENCE_CHUNK_BYTES = 65_408`、`MAX_SYSTEM_CONTROL_ENUM_LIMIT = 64`）。
  - [`crates/nlos-channel/src/lib.rs`](../../crates/nlos-channel/src/lib.rs) 强制队列 `capacity_bytes` 准入与消费/裁剪单调水位；[`crates/nlos-topic/src/lib.rs`](../../crates/nlos-topic/src/lib.rs) 强制 `max_recipients`、`delivery_attempts`（超限转 `QUARANTINED` 隔离慢消费者）、`cascade_depth` 与 `retained_bytes`/`retention_ms` 发布前背压拒入。
  - [`crates/nlos-capability/src/blob_model.rs`](../../crates/nlos-capability/src/blob_model.rs) 限制单块 `65_408` B、单清册最多 `4_096` 块；[`crates/nlos-net/src/frame.rs`](../../crates/nlos-net/src/frame.rs) 限制 `BulkChunk <= 64 KiB` 与信用窗口流控。
  - **边界**：对未接入 NLOS 通道而直接走操作系统原生 pipe/Unix socket/shared memory 的非沙箱进程，无消息容量强制。

#### 11. 速率/配额 (Rate/Quota)
- **规范目标**：`[RSM-ALG-001]`、`[SCHED-QUEUE-001]`、`[DIST-BULKHEAD-001]`。
- **现网实证事实（服务内 `STRICT_ENFORCED` / 全局与跨进程 `COOPERATIVE_DOWNGRADE`）**：
  - `GatewayLimits`（[`llm_gateway.rs`](../../crates/nlos-driver-llm-http/src/llm_gateway.rs)）：在途调用全局 `32` / 每主体 `4`、60s 窗 `256` 咨询行、每父 `16` 活跃子账户、全局 `4096` 账户、`max_api_calls` 硬截断。
  - `ToolServiceLimits` + `ToolHostQuota`（[`tool_service.rs`](../../crates/nlos-wasm-sandbox/src/tool_service.rs)）：共享 writer 下跨实例强守全局 `16` / 每主体 `4` 活宿主进程（超限返回 `NoHostPermit`）。
  - `BlobConfig`（[`blob_model.rs`](../../crates/nlos-capability/src/blob_model.rs)）：60s 滑动窗 per-principal/global mint 与 read 速率、并发活跃引用数及最大读次。
  - `ResourceGroupQueuePolicy`（[`nlos-runtime/src/scheduler_domain.rs`](../../crates/nlos-runtime/src/scheduler_domain.rs) / [`distributed_scheduler.rs`](../../crates/nlos-runtime-tokio/src/distributed_scheduler.rs)）：`max_queue_depth` 与 `parent_hard_quota_in_flight` 原子 CAS 限流。
  - `QuotaLeaseGrantor` / `CapacityLeaseGrantor`（[`nlos-lease/src/lib.rs`](../../crates/nlos-lease/src/lib.rs)）：预付额度租约与逻辑 TTL 围栏。
  - **缺口**：`W82-INT` 尚未完成三服务向 `SystemControlDaemon` 全量挂载；且进程内 `Arc<ToolHostQuota>` 仅在单一权威 writer 进程内强制，若宿主外另有独立进程直接拉起程序则属 `COOPERATIVE_DOWNGRADE`。

#### 12. 人类注意力 (Human Attention)
- **规范目标**：`[RSM-HUMAN-001]`、`[RSM-ATTN-002]`、`[TUI-CONSENT-001]`、`[DUI-NOTIFY-001]`。
- **现网实证事实（`nlos-notify` 边车策略引擎已达 `STRICT_ENFORCED`；桌面总线与整机/UI 端维持 `COOPERATIVE_DOWNGRADE`）**：
  - **桌面安全屏与绑定令牌底线已实证**：[`desktop/src-tauri/src/trusted_ui.rs`](../../desktop/src-tauri/src/trusted_ui.rs) 实现 `Z4` 实心安全确认屏（`Z4_SECURITY_Z_INDEX = 900` 禁止 `Z2` 应用遮挡或伪造、`350 ms` 防点击劫持武装窗、`Z4_IRREVERSIBLE_CRIMSON_HEX` 不可篡改警示）；[`crates/nlos-notify/src/lib.rs`](../../crates/nlos-notify/src/lib.rs) 与 [`task_notify.rs`](../../crates/nlos-notify/src/task_notify.rs) 实现一次性绑定动作令牌（`notify_action_tokens`）与离线分页元数据。
  - **`nlos-notify` 边车注意力策略控制器已落地并验证（`W82-R4-IND`）**：[`crates/nlos-notify/src/attention.rs`](../../crates/nlos-notify/src/attention.rs) 实现 `#![forbid(unsafe_code)]` `AttentionPolicyController` 并独立落盘于 sidecar `attention-policy.db` v1（`PRAGMA journal_mode = WAL`、`PRAGMA synchronous = FULL`、`PRAGMA foreign_keys = ON`，原 `notify-service.db` v2 零改动），经 [`crates/nlos-notify/tests/attention_policy.rs`](../../crates/nlos-notify/tests/attention_policy.rs)（`nlos-notify` 全套件 `42 passed; 0 failed`：[D-NOTIFY-ATTN-001](../evidence/stage-d/d-notify-attention-policy-20261122.md)，`H4`）实证：
    1. **双队列严格隔离与零饥饿**：仅 `AttentionOrigin::TrustedSystemUi` 发起的 `ConsentPrompt`/`SystemCritical` 进入 `PriorityConsentLane` 并绕过静默时段/聚合/普通频控，在 `500` 条普通通知洪泛下保持 `100%` 即时放行且不挤占普通队列配额；
    2. **静默时段与同类聚合/频控**：支持白昼与跨午夜静默时段（如 `22:00..07:00`，`utc_offset_minutes` `-720..=840`）并计算精确 `resume_at_ms`；支持 `(origin, coalesce_key, active_window)` 同类通知窗口聚合（`attention_coalesce_counters`）及单应用与全局滚动窗口速率限制；
    3. **付费绕过拒接、伪造防线与不可变审计回执**：`payment_priority_override_attempted == true` 一律提交不可变 `REJECTED_PAYMENT_BYPASS` 审计回执并 fail-closed 返回 `PaymentBypassForbidden`；非受信 `AttentionOrigin::Application(_)` 请求 `ConsentPrompt` 或 `SystemCritical` 一律落盘 `REJECTED_UNTRUSTED_ORIGIN` 并返回 `UntrustedConsentOrigin`；`attention_policy_config` / `attention_decision_receipts` / `attention_coalesce_counters` 配 `SQLite` `BEFORE UPDATE`/`BEFORE DELETE` 触发器防篡改与幂等键跨重启重放。
  - **诚实未闭环缺口（整机/UI 端维持受限分级）**：上述 `AttentionPolicyController` 目前仅为 `nlos-notify` 侧边车策略引擎，**桌面端 Tauri `Z4` / UI（`desktop/src-tauri` 与 `desktop/src`）及 `SystemControlDaemon` 实时通知分发总线真实连线（`DEL-UX`）与真实人类长稳使用试点（`DEL-FIELD`, `H8 = 0`）仍未完成**，故整机/UI 端仍标为 `COOPERATIVE_DOWNGRADE`。
- **红线**：不得用支付 credit 换取跳过 `Z4` 确认或静默模式；未完成 `DEL-UX` 全总线连线及 `DEL-FIELD` 真实人类试点（`H8 = 0`）前，严禁将整机端宣称为端到端生产 `STRICT_ENFORCED`。

---

## 3. `ROAD-A-004` 宿主 OS `ResourceGroup` 控制器 Shim 设计（`DESIGN`）与后续实施写集

> **阶段声明**：本节全部结构与接口设计均为 **`DESIGN`（待实现方案）**，尚未进入代码库，不改变现网 `COOPERATIVE_DOWNGRADE` / `GUEST_SANDBOX_ONLY` 分级状态。

### 3.1 总体架构：账本与宿主控制器解耦、原子出生与强制降档闭环

依据 `[RSM-SEPARATE-001]`、`[RSM-CONTROLLER-001/002]` 与 `[PROC-SPAWN-001]`，宿主资源控制器层位于 `nlos-resource`（不可变费用/配额账本）**之外**的独立控制适配层（建议新建 `crates/nlos-resource-controller`），与 `nlos-process::ProcessSupervisor`、`nlos-task` 与 `nlos-system-control` 通过有类型 Receipt 与生命周期状态机联动：

```text
  AdmissionPlan ([RSM-ADMIT-002])
     ├── ReservationProposal ──▶ nlos-resource::ResourceAuthority (信用/5D token，不管 OS 隔离)
     └── AllocationProposal  ──▶ nlos-resource-controller::ResourceGroupAuthority
                                     │
                                     ├── HostCapabilities::probe() -> PlatformProfile
                                     │     ├─ Linux:   cgroup v2 + PSI + FS/Disk quota
                                     │     ├─ Windows: Win32 Job Object + CompletionPort + Disk check
                                     │     └─ macOS:   WasmPage/Fuel/Epoch + Advisory Telemetry + 强制降档标记
                                     │
                                     ▼
                         ControllerBinding (binding_epoch, host_boot_generation, GuaranteeTier)
                           DETACHED → ATTACHING → ACTIVE ↔ PRESSURED → FROZEN/RECLAIMING
                                       → FENCING → CLEANING → DETACHED | QUARANTINED
                                     │
                                     ├── 绑定 nlos-process::ProcessSupervisor::spawn_supervised_grouped
                                     └── 输出 PressureEvent / OomReceipt / GuaranteeDowngrade → Receipt & Desktop Z4
```

**核心设计约束（面向仓库现实的 3 项关键裁定）**：
1. **严格遵守根 `Cargo.toml` `unsafe_code = "forbid"` 红线**：
   - 根 [`Cargo.toml`](../../Cargo.toml) L59–60 声明 `[workspace.lints.rust] unsafe_code = "forbid"`。
   - **Linux `cgroup v2`** 主路径通过读写 `/sys/fs/cgroup/...` 虚拟文件系统与 `poll` 接口实现，主体逻辑可 **100% 保持零 `unsafe`**。仅当需解决「`fork()` 后到写入 `cgroup.procs` 前窗口逃逸」并采用 Linux `CLONE_INTO_CGROUP`（`clone3`）时，需对比「安全方案 A：受控同步管道门控（子进程入口阻塞读 readiness pipe，父进程写入 `cgroup.procs` 成功后关闭写端放行，零 `unsafe` 但不防已加载第三方二进制的动态库构造函数）」与「隔离方案 B：独立微 crate 经 ADR 审批开启极窄 `unsafe` 调用 `clone3(CLONE_INTO_CGROUP)`」。
   - **Windows `Job Object`** 必须调用 Win32 FFI（`windows-sys` 中的 `Win32_System_JobObjects`、`Win32_System_IO`、`Win32_System_Threading`）。依 [项目管理机制 §7](./README.md#7-评审与决策机制)，任何新增 `unsafe` FFI crate（建议命名为 `crates/nlos-host-shim-win32`，与核心 `nlos-resource`、`nlos-process` 物理隔离）必须在实施前提交独立 ADR、安全审查与边界测试，且**禁止**业务策略进入 `unsafe` 块。
2. **禁止迁组逃逸（`[RSM-GROUP-001]`）**：每个受管宿主进程在每类 advertised hard controller 树中恰好隶属一个叶级 `ResourceGroup`；迁组必须原子推进 `binding_epoch` 并关闭旧组计量高水位回调，未拿到新老两组确认前保持 `FENCING` 禁止新准入。
3. **启动降级即拒或明示（`[PROFILE-IMPL-001]` / `[TCB-DOWNGRADE-001]`）**：服务启动时由 `HostCapabilities::probe()` 实测宿主支持矩阵（如探测 `/sys/fs/cgroup/cgroup.controllers` 是否委托 `cpu memory io pids`、探测 Windows Job 权限、识别 macOS 无宿主内存硬限）。若实测能力低于部署 Manifest 声明且契约要求 `STRICT_ENFORCED`，立即 fail-closed 拒启；若允许降档，强制写入 `GuaranteeTier::{GuestSandboxOnly, CooperativeDowngrade}` 与结构化 `DowngradeReason` 到全部 Receipt。

---

### 3.2 Linux `cgroup v2` Shim 设计（`DESIGN`）

| 控制子面 | 宿主机内核接口（`/sys/fs/cgroup/<slice>/<rg_id>/`） | NLOS 对象与不变量映射 | 故障 / 超限 / Receipt 联动 |
|---|---|---|---|
| **层级组织** | `nlos.slice/system_reserve/`（TCB 独立保留，`[RSM-RESERVE-001]`）与 `nlos.slice/tenants/<tenant>/<app>/<rg_id>/`；根开启 `cgroup.subtree_control = "+cpu +memory +io +pids"` | 严格映射 `[RSM-HIER-001/002]` 父子树；子组 `memory.max` 与 `cpu.max` 构造时校验不超过祖先可分配余量 | 若宿主未挂载 cgroup v2 unified hierarchy 或无委托写权限，probe 返回 `CgroupUnavailable` 并强制降档为 `COOPERATIVE_DOWNGRADE` |
| **CPU 硬限与权重** | `cpu.max`（`<quota_us> <period_us>`，如 `50000 100000` = 50% 单核）、`cpu.weight`（`1..10000`，映射 `ResourceContract::scheduling_class`） | 对应 `CpuDemand::{quota_ns, period_ns}` 与 WFQ 权重；读取 `cpu.stat` (`usage_usec`, `user_usec`, `system_usec`, `nr_throttled`, `throttled_usec`) 分维核算 | `cpu.stat` 运行时间与调度器等待/provider 等待分维上报（`[RSM-METER-002]`）；严禁将 blocked wall time 算入 `cpu_ns` |
| **内存软/硬限与 OOM** | `memory.high`（软回收水位）、`memory.max`（硬天花板）、`memory.swap.max = 0`（防 swap 延迟穿透）、`memory.current` / `memory.peak` / `memory.events` | `CapacityDemand` 激活前由 `ControllerBinding` 写入；父级预留确保 `sum(child memory.max) + system_reserve <= host_allocatable` | 跨越 `memory.high` 或 PSI 触发 `PressureEvent`；触发 `memory.events` `oom`/`oom_kill` 时立即捕获受影响组、生成不可变 `OomVictimReceipt` 并迁移组态至 `QUARANTINED` 直到进程清理完结 |
| **块/文件 I/O** | `io.max`（`<major>:<minor> rbps=... wbps=... riops=... wiops=...`）与 `io.weight`；读取 `io.stat`（`rbytes`/`wbytes`/`rios`/`wios`） | 按主工作区/数据卷所在块设备号绑定；`RecordThrottleDecisionRequest` 执行时同步按比例下调 `io.max` 与 `cpu.max` | 块设备为虚设备或 overlayfs 无底层设备号映射时，显式记录 `IoDeviceUnmapped` 降档原因而非静默忽略 |
| **进程数与整组冻结/强杀** | `pids.max`（有界 fanout 防 fork 炸弹）；`cgroup.freeze` (`1`/`0`)；`cgroup.kill` (`1`，Linux >= 5.14) | 与 `nlos-process::ProcessSupervisor` 联动：`pause`→`cgroup.freeze=1`、`resume`→`0`、`kill_force`→`cgroup.kill=1` 并在 `cgroup.events` (`populated=0`) 后确认排空 | 解决孙进程脱离单 PID / pgid 逃逸问题；`populated=0` 是释放 `ControllerBinding` 进入 `DETACHED` 的前提（`[RSM-RECLAIM-003]`） |
| **PSI 压力感知** | 监听 `memory.pressure`、`cpu.pressure`、`io.pressure`（`some`/`full` 窗口阈值通过 `poll`/`epoll` 事件驱动触发） | 转换输出为带单调序号与高水位的 `PressureEvent`（`[RSM-PRESSURE-001/002]`），逆向通知 `nlos-runtime-tokio` 收缩 `materialization_window` 并按 `[RSM-RECLAIM-001]` 执行四阶段回收 | 不得只给 Agent 注入自然语言提示；达到 `CRITICAL` 超时无 ACK 自动转 `FORCE_PENDING → FENCING → cgroup.kill` |

---

### 3.3 Windows `Job Object` Shim 设计（`DESIGN`，承接 `U-2` / `RISK-B-10`）

Windows 平台围绕 Win32 Job Object（建议独立安全审查的 FFI crate `crates/nlos-host-shim-win32`）构建宿主执行控制器，**直接闭环 `U-2`（Job Object breakaway 脱离进程树）**：

1. **进程树绑定与强制防逃逸（`U-2` 核心闭环）**：
   - 创建命名或匿名受控 Job：`CreateJobObjectW(NULL, NULL)`，并将句柄封装于带 RAII `Drop`（`CloseHandle`）与 `binding_epoch` 的安全 Rust 句柄中。
   - 构造 `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`：
     - **强制开启**：`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`（控制器崩溃或显式释放句柄时内核连坐终止组内全部后代进程）、`JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION`。
     - **显式永久禁止**：**严禁设置 `JOB_OBJECT_LIMIT_BREAKAWAY_OK` 与 `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK`**；在 `SetInformationJobObject` 写入前与 `QueryInformationJobObject` 读回核验阶段双端断言 `LimitFlags & (JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK) == 0`，使组内任何后代进程调用 `CreateProcessW(..., CREATE_BREAKAWAY_FROM_JOB)` 被 Win32 内核以 `ERROR_ACCESS_DENIED` 拒绝。
   - **原子出生（`CREATE_SUSPENDED` 模式）**：生成子进程时传入 `CREATE_SUSPENDED`（或先加入父级不可逃逸 Job 再 `AssignProcessToJobObject`），在确认 `AssignProcessToJobObject` 成功返回后才 `ResumeThread`，消除 `spawn` 与 `Assign` 之间产生脱离孤儿的竞态窗口。
   - **显式保留的 `U-2` 外部权限边界**：若子进程通过操作系统 UAC 提权服务、Task Scheduler 或独立管理员会话启动高完整性级别进程，Windows 安全模型允许跨会话脱离非提权 Job，该情形在失败回执中诚实标注 `CrossPrivilegeElevationUnsupported`，绝不宣称任意跨权限必杀。
2. **内存、CPU 与进程数限制**：
   - **内存**：设置 `JOB_OBJECT_LIMIT_JOB_MEMORY`（整组提交内存上限 `JobMemoryLimit`）与 `JOB_OBJECT_LIMIT_PROCESS_MEMORY`（单进程上限 `ProcessMemoryLimit`）；周期通过 `QueryInformationJobObject(JobObjectExtendedLimitInformation)` 读取 `PeakJobMemoryUsed` 与 `PeakProcessMemoryUsed` 上报 `ResourceGroupControllerView`。
   - **CPU**：设置 `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION`（启用 `JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP`，`CpuRate` 以万分之一核粒度配置，映射 `CpuDemand`）。
   - **进程数**：设置 `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` 限制并发进程树分支上限，防止 fork 炸弹。
3. **I/O 计量与原生限制分界（诚实限定）**：
   - 通过 `QueryInformationJobObject(JobObjectBasicAndIoAccountingInformation)` 读取权威内核 `IoInfo`（`ReadOperationCount`、`WriteOperationCount`、`ReadTransferCount`、`WriteTransferCount`），实现**真实 OS 级 I/O 计量（`METERED_OBSERVED`）**。
   - **诚实声明**：基础 Win32 Job Object 原生不包含跨卷通用磁盘 IOPS/字节硬限（Windows `JobObjectNetRateControlInformation` 仅管网络出站 DSCP/带宽，磁盘硬限需 Hyper-V/Storage QoS 或微过滤驱动）。因此 Windows 磁盘块 I/O 硬限在未接入 Storage QoS 驱动前，明确标为 `COOPERATIVE_DOWNGRADE`（仅具备内核级 I/O 真实计量与超限事件处置）。
4. **Completion Port 事件联动与 Receipt**：
   - 通过 `JobObjectAssociateCompletionPortInformation` 将 Job 绑定到专用 I/O Completion Port 监听线程，捕获 `JOB_OBJECT_MSG_JOB_MEMORY_LIMIT`、`JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT`、`JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO` 与 `JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS`；收到超限消息即生成带 `(resource_group_id, binding_epoch)` 的 `PressureEvent` / `OomLimitReceipt`，并在 `FORCE_PENDING` 时调用 `TerminateJobObject(job, exit_code)`，待 `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO` 到达后才允许归还容量。

---

### 3.4 macOS 诚实降档与受限强制设计（`DESIGN`）

基于 [b-sandbox-001-isolation-comparison](../evidence/stage-b/b-sandbox-001-isolation-comparison.md) §2 D2 已验证的决定性事实（macOS arm64 上 `setrlimit(RLIMIT_AS)` 恒返回 `EINVAL`，`RLIMIT_RSS` 为无操作，且 XNU 无对等 cgroup v2/Job Object 容器原语），macOS Shim 设计明确分为两条边界：

1. **受强约束的 Wasmtime 客体面（`GUEST_SANDBOX_ONLY`）**：
   - 保持并继续加固 `nlos-wasm-sandbox`：`PageLimiter` 64 KiB 页硬限、确定性 `fuel`、`EpochTicker` 中断（校准 timer coalescing 有效周期约 1.5 ms）、`ToolHostLimits`（`8 MiB` 写 / `256 KiB` stdout / `60 KiB` stderr / `4096` 次文件创建）、`env_clear()` 与 `process_group(0)` + `killpg`（已正确处理 Darwin `EPERM` 空僵尸组）。
2. **宿主原生进程与 OS 限制面（禁止伪硬限，仅开放真实观测 + 强制降档标记）**：
   - **明确禁止**：禁止调用 `setrlimit(RLIMIT_AS)` 或以 `RLIMIT_RSS` 伪造内存配额。
   - **真实遥测（`METERED_OBSERVED`）**：仅使用安全的进程资源探测（如 `proc_pid_rusage` / `getrusage` 的 `ri_phys_footprint`、`ri_diskio_bytesread/written`、`ri_user_time`/`system_time` 与 macOS `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE` 系统压力事件）产出观测 high-water 与 `PressureEvent`（仅作软告警与调度器背压输入，不伪称硬限）。
   - **强制 `[TCB-DOWNGRADE-001]` 链路透传**：
     - macOS `HostCapabilities::probe()` 固定返回 `host_memory_hard_cap = Unsupported`、`host_io_bw_hard_cap = Unsupported`、`host_cpu_bw_hard_cap = CooperativeOnly`。
     - 若工作负载以原生进程运行（非 `nlos-wasm-sandbox`），`AdmissionDecision` 与执行 `Receipt` **必须**携带 `GuaranteeTier::CooperativeDowngrade` 及非空 `DowngradeReason::MacOsHostNoGroupQuota`；若以 Wasm 工具运行，则携带 `GuaranteeTier::GuestSandboxOnly`（标明「仅限 Wasm 线性内存/Fuel/单次写，宿主机原生内存无硬限」）。
     - **桌面 `Z4` 与 Resource Monitor 映射**：[`desktop/src-tauri/src/trusted_ui.rs`](../../desktop/src-tauri/src/trusted_ui.rs) 的 `Z4ResourceCeilingSpec` 与 `ControlCenterDomainKind::ResourceGroup` 面板消费该结构化降档枚举，在授权前强制展示「当前 macOS 宿主不对原生进程强制内存/I/O 硬限」警示条；若 `ResourceContract::requested_guarantee == Strict`，Admission 必须 fail-closed 拒绝并返回 `TypedDenial::HostCapabilityInsufficient`（`[BUD-TIER-001]`）。

---

### 3.5 存储控制器与 `[RSM-RESERVE-001]` 系统救援池（跨平台 `DESIGN`）

针对 `ROAD-A-004` 第四类本地资源（Storage）与 `[RSM-RESERVE-001]`，设计双层存储控制器：

1. **系统救援保留池（`SystemStorageReservePool`）**：
   - 在节点初始化时对 `ResourceAuthority`、`TaskAuthority`、`IdentityAuthority`、`LeaseStore` 与审计 WAL 所在挂载卷预检可用空间并设定硬保留地板（`system_reserve_bytes`，默认不低于 `256 MiB` 或卷可用空间配置地板）。
   - 任何普通 Application/Task 写入或 `ArtifactStore::put_revision` / `stage_revision` 准入前检查 `volume_free_bytes - requested_upper_bound >= system_reserve_bytes`；触及地板即在准入前返回 `StoragePressure::SystemReserveProtected`，保证 `ENOSPC` 发生时内核权威与恢复提交依然有可用磁盘空间，绝不通过删除权威恢复日志腾挪空间。
2. **工作区与每组存储限额**：
   - **Linux**：支持项目配额（XFS/ext4 `FS_IOC_FSSETXATTR` project quota）或基于 `statvfs` + 目录树会计账本（在无 root/非 project-quota FS 上显式降档标注 `ApparentQuotaOnly`）。
   - **Windows**：查询卷磁盘可用空间（`GetDiskFreeSpaceExW`）与每工作区目录字节累加预算（NTFS 每用户/卷配额需管理员权限，无权限时降档标注）。
   - **macOS**：`statvfs` 卷余量 + APFS 容器/卷级配额探测（非提权环境下单目录硬限不可用，沿用 `ArtifactStore` / `BlobLedger::guard_storage` 有界账本与 `ToolHostLimits` 8 MiB 逐次写硬限，并附带 `COOPERATIVE_DOWNGRADE` 标记）。

---

### 3.6 `DEL-RES` 后续实施写集规划（按波次隔离，严禁越权）

后续实际编码实施拆分为 5 条写集互斥的车道，遵循 [项目并行与提交协议 §6.1](./project-knowledge-progressive-disclosure.md#61-子-agent-编排与依赖感知并行)：

| 实施车道 | 阻塞关系 | 独占写集（Write-Set） | 关键交付与退出门 |
|---|---|---|---|
| **Lane-RES-1：控制器核心契约与台账** | 无（先行） | `crates/nlos-resource-controller/`（新建 crate，待 ADR + 根 `Cargo.toml` 由 integrator 挂载）、`crates/nlos-resource/src/{lib.rs, schema.rs}`（仅追加 `HostControllerProof` 只读引用字段，不混写 `ResourceAccount`） | `HostResourceController` trait、`ResourceControllerManifest`、`ControllerBinding` 状态机、`PressureEvent`、`ReclaimRequest`、`HostCapabilities::probe()`、`GuaranteeTier` 降档枚举 |
| **Lane-RES-2：Linux `cgroup v2` 控制器实现** | blocked-by Lane-RES-1 | `crates/nlos-resource-controller/src/linux_cgroup.rs`、`crates/nlos-resource-controller/tests/linux_cgroup_enforcement.rs`、`crates/nlos-process/src/supervisor.rs`（仅增加 `attach_controller_hook`） | 真实 Linux cgroup v2 `cpu.max`/`memory.high`/`memory.max`/`io.max`/`pids.max`/`cgroup.kill` + PSI trigger + OOM Receipt（Linux runner 实测） |
| **Lane-RES-3：Windows `Job Object` 控制器实现（闭环 `U-2`）** | blocked-by Lane-RES-1（与 Lane-RES-2 并行） | `crates/nlos-host-shim-win32/`（独立经 ADR 审批 FFI crate）、`crates/nlos-resource-controller/src/windows_job.rs`、`crates/nlos-process/tests/platform_kill.rs` | `JOB_OBJECT_LIMIT_JOB_MEMORY`/`PROCESS_MEMORY`/CPU rate/I/O 计数/`KILL_ON_JOB_CLOSE` + **禁止 `BREAKAWAY_OK` 双端校验与跨树对抗测试**（Windows runner 实测） |
| **Lane-RES-4：macOS 降档透传、存储救援池与 UI 映射** | blocked-by Lane-RES-1（与 Lane-RES-2/3 并行） | `crates/nlos-resource-controller/src/{macos_probe.rs, storage_reserve.rs}`、`crates/nlos-system-control/src/control_stage_d.rs`、`desktop/src-tauri/src/{trusted_ui.rs, settings_keychain.rs}` | `SystemStorageReservePool` (`ENOSPC` 救援)、macOS `proc_pid_rusage` 观测 + `GuaranteeTier` 降档标记进入 Receipt 与桌面 `Z4`/Resource Monitor（`Z4` 禁伪造回归） |
| **Lane-RES-INT：集成接线与 `claims.yaml` 同步** | blocked-by Lane-RES-1..4 | 根 `Cargo.toml`/`Cargo.lock`、`crates/nlos-system-control/src/bin/system-control-daemon.rs`、`docs/management/{claims.yaml, risks.yaml, evidence-index.yaml, stage-b-progress.md}` | 端到端 `AdmissionPlan` → `ProcessSupervisor` → 宿主控制器 → `Receipt`；重跑全量安全/故障门并更新 `ROAD-A-004` / `RISK-B-03` / `RISK-B-10` / `U-2` |

---

## 4. 验收测试矩阵草案（`DESIGN` / 拟定准出门）

依据 [NLOS 总任务计划书](./nlos-goal-delivery-plan.md) §5 `DEL-RES` 与 [v0.5 §10 / §12 / §28](../design/06-架构设计总纲-v0.5.md)，后续 `DEL-RES` 验收必须覆盖以下六大类正负路径，所有断言必须绑定具体错误变体、Receipt 字段与操作系统级实测结果，不得用 mock 或单一 `is_err()` 替代。

| 测试域 | 测试 ID 草案 | 目标平台 | 正向 / 负向 / 安全断言要求 | 依赖与分界（本地/CI vs `DEL-EXT`） |
|---|---|---|---|---|
| **1. 资源竞争（Contention）** | `RES-CONTEND-01` 多组 CPU/内存并发争抢与 `SYSTEM_CRITICAL` 隔离 | Linux / Windows（macOS 测 Wasm + 降档） | 两个 `BATCH` / `BEST_EFFORT` 组满载争抢 CPU 与内存时，`SYSTEM_CRITICAL`（`SystemControlDaemon`/`ResourceAuthority`）保留组延迟与内存驻留不受饿死；Wasm `ToolHostQuota` 跨实例并发槽 `16/4` 拒绝超额调用返回 `NoHostPermit` 且零新增 journal 垃圾帧 | 纯软件 + CI runner 可跑 |
| **1. 资源竞争（Contention）** | `RES-CONTEND-02` 兄弟故障隔离 | Linux / Windows / macOS | 组 A 内存泄漏/CPU 死循环/写满 8 MiB/fd 耗尽（`MAX_OPEN_FDS=256`）被 OOM/Epoch/Trap/Kill 终止后，同父树下健康兄弟组 B 全程存活且配额计数无泄漏 | 纯软件 + CI runner 可跑 |
| **2. 压力 / 回收 / OOM & ENOSPC** | `RES-PRESSURE-01` Linux cgroup v2 PSI 梯度回收到硬 OOM | Linux（需要委托 cgroup v2 runner） | 内存越过 `memory.high` → 发出带 monotonic seq 的 `PressureEvent` → 按 `[RSM-RECLAIM-001]` 先触发 `RebuildableCache` 清理 → 拒绝合作继续分配触达 `memory.max` → 内核 OOM kill (`memory.events` `oom_kill >= 1`) → 生成 `OomVictimReceipt` 并将绑定状态迁入 `QUARANTINED` 直至确认排空 | 需 Linux cgroup v2 CI/容器委托权限 |
| **2. 压力 / 回收 / OOM & ENOSPC** | `RES-PRESSURE-02` Windows Job Memory 超限终止与 `CompletionPort` 回执 | Windows | Job 进程总分配逼近 `JOB_OBJECT_LIMIT_JOB_MEMORY` 时，`CompletionPort` 捕获 `JOB_OBJECT_MSG_JOB_MEMORY_LIMIT`，控制器 `TerminateJobObject` 并等待 `ACTIVE_PROCESS_ZERO` 后生成 `OomLimitReceipt`，禁止在清零完成前重放容量 | Windows CI runner 可跑 |
| **2. 压力 / 回收 / OOM & ENOSPC** | `RES-PRESSURE-03` macOS 降档诚实性与 Wasm `PageLimiter` | macOS | `HostCapabilities::probe()` 验证不调用 `RLIMIT_AS`；请求 `Strict` 原生进程被 fail-closed 拒绝；允许降档时 `ExecutionReceipt` 与 `Z4` DTO 显式携带 `CooperativeDowngrade`；同时 Wasm guest `memory.grow` 越过页 cap 仍同步返回 `-1` | macOS CI runner 可跑 |
| **2. 压力 / 回收 / OOM & ENOSPC** | `RES-STORAGE-01` `ENOSPC` 系统救援保留池保护（`[RSM-RESERVE-001]`） | Linux / macOS / Windows | 填满普通工作区可用空间逼近 `system_reserve_bytes`（含 `nlos-store-fault` `FaultCode::Full` 故障注入与真实小容量环回分区/配额盘），验证普通 App 写被提前拒绝而 `ResourceAuthority` / `TaskAuthority` / `IdentityAuthority` 的取消、Quarantine 与恢复 Receipt 提交仍 100% 成功 | 环回镜像/小盘在 Linux/macOS CI 可跑；真实硬件介质掉电归 `DEL-EXT` (`U-4`) |
| **3. 父子配额守恒** | `RES-HIER-01` 多级 `ResourceGroup` 守恒与迁组围栏（`[RSM-HIER-002]` / `[RSM-GROUP-001]`） | Linux / Windows | 验证 `sum(child PREPARED + ACTIVE strict) + parent_local <= parent_effective_allocatable`；子组申请超出父组余量在 `AdmissionPlan` 阶段被原子 `ABORT`；运行中进程反复迁组时，旧组 `binding_epoch` 立即围栏并阻止旧回调计数泄漏或超配 | 纯软件 + Linux/Windows runner |
| **3. 父子配额守恒** | `RES-HIER-02` `ResourceAccount` 与 `ResourceGroup` 隔离边界（`[MODEL-RESOURCE-001]` / `[ROAD-A-004]`） | 全平台 | 验证向 `ResourceAccount` 充入大量 credit 或上调 `LlmTokenLimits` **不会**自动放大物理 CPU/内存/I/O 配额；反之宿主 `ResourceGroup` 降额/OOM 强杀 **不会**篡改用户 `ResourceAccount` 已扣款与预留余额，仅经标准结算/退款状态机收敛 | 纯软件可跑 |
| **4. 取消 / 迟到回调 / 防逃逸** | `RES-CANCEL-01` 回收超时强杀与迟到消费回调围栏（`[RSM-RECLAIM-003]` / `[BUD-LATE-001]`） | 全平台 | `ReclaimRequest` 发送后模拟进程阻塞忽略 `ACKNOWLEDGED` 超出 `force_deadline` → 强制进入 `FORCE_PENDING → FENCING → cgroup.kill / TerminateJobObject`；同时注入迟到的 `consume` / `usage` 回调与过期 `binding_epoch` 更新，全部被权威拒绝并保持既有终态 | 纯软件 + 真实子进程可跑 |
| **4. 取消 / 迟到回调 / 防逃逸** | `RES-WIN-BREAKAWAY-01` Windows Job Object 防 breakaway 闭环与跨权限诚实边界（`U-2`） | Windows | 在受控 Job 内运行恶意子进程尝试 `CreateProcessW(..., CREATE_BREAKAWAY_FROM_JOB)` → 内核返回 `ERROR_ACCESS_DENIED`；强杀父进程或关闭 Job 句柄后验证含孙进程在内的整树 100% 终止（兄弟 Job 零误杀）；另置跨权限/提权失败夹具验证返回 `CrossPrivilegeElevationUnsupported` 而非伪造成功 | Windows CI runner 可跑 |
| **5. 设备租约失联隔离** | `RES-DEV-LOSS-01` `ExclusiveDeviceLease` / `CapacityLease` 失联隔离与围栏非互换性（`[LEASE-DEVICE-001]` / `U-5`） | 全平台 | 持有者进程崩溃、节点心跳超时或本地 WAL 丢失后：`DeviceLeaseHead` 保持 `QUARANTINED`、`QuarantineDisposition` 保持 `LedgerQuarantinedFenceOnly`；旧 `fencing_token` 访问模拟设备端点被拒；仅持 `QuotaReconciliationReceipt` 无法解锁设备，缺 `DeviceResetReceipt` (`vram_and_secrets_zeroized = true`) 时拒绝重发给新租约 | `nlos-lease` 已有软件协议底座；真机复位见下行 `DEL-EXT` |
| **6. 与 `DEL-EXT` 物理硬件门的分界** | `RES-EXT-BOUNDARY` 物理硬件/外部资源门（非本仓自动化测试可替代） | 物理真机台架 | **明确划归 [`DEL-EXT`](./nlos-goal-delivery-plan.md#del-ext外部资源长期承诺与维护接续横切-cde) 验收，严禁软件 mock 冒充**：<br>1) **GPU/NPU 真实物理隔离与显存清零**：真实 CUDA/ROCm/Metal/NPU 设备重置、MIG 分区、VRAM 残留读检测与硬件复位证明；<br>2) **真实能源与热节流**：真机功率计/OS 能源 API（`powermetrics`/RAPL/Win32 Battery/Thermal）实测 `energy_mJ` 与热降频触发已授权 fallback；<br>3) **层 3 物理断电与集群 STONITH**：智能 PDU / IPMI 断电台架对 `SQLite WAL+FULL` 与 `SystemStorageReservePool` 的掉电恢复；<br>4) **原生签名与 `H8` 现场运行**：macOS 公证/Windows EV 签名包（跨权限/entitlement 行为）与真实用户长期运行档案 | **全部归属 `DEL-EXT` / `DEL-FIELD`，必须人类与物理硬件台架签字取证** |

---

## 5. 参考权威索引

- 规范基线：[06-架构设计总纲-v0.5.md](../design/06-架构设计总纲-v0.5.md)（§1.1、§6、§8、§10、§12、§16、§28 `ROAD-A-004`、§30）
- 交付计划与覆盖审计：[NLOS 总任务计划书](./nlos-goal-delivery-plan.md)、[DEL-BASE 覆盖审计报告](./del-base-coverage-audit.md)
- 风险与未知项：[risks.yaml](./risks.yaml)（`RISK-B-03`、`RISK-B-10`、`RISK-B-11`、`RISK-E-04`）、[exit-unknown-risks.md](./exit-unknown-risks.md)（`U-2`、`U-4`、`U-5`、`U-9`、`U-10`）
- 已有 ADR：[ADR-0015 双层不可信执行隔离](./adrs/0015-dual-layer-untrusted-execution-isolation.md)、[ADR-0017 资源-操作跨权威预备终结](./adrs/0017-resource-operation-cross-authority-prepare-finalize.md)、[ADR-0024 驱动提供方编码冻结](./adrs/0024-driver-provider-payload-codec-freeze.md)、[ADR-0025 Stage D 桌面合成与可信 UI](./adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md)
- 配对核验报告：[B-RES-PROFILE-001：12 维现代资源平面现网基线核验与降档画像](../evidence/stage-b/b-res-profile-audit-20261121.md)、[D-NOTIFY-ATTN-001：nlos-notify AttentionPolicyController 边车策略引擎与对抗验证](../evidence/stage-d/d-notify-attention-policy-20261122.md)
