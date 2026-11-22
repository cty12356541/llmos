# B-RES-PROFILE-001：12 维现代资源平面现网基线核验与降档画像

- **证据 ID**：`b-res-profile-audit-20261121`
- **工作项 / 车道**：`DEL-RES` 启动基线核验（Wave `W82-R3` / 车道 `W82-R3-DEL-RES`）
- **日期**：`2026-11-21`
- **保障等级**：`H3`（单机 macOS 单 crate 基线复验与一手源码审计；不含 Linux/Windows 宿主执行、不含跨机混沌或真实硬件台架；`H8 = 0`）
- **基线 SHA**：`9414000f9b19179ca0922d95bbf81e21eff137ab`（分支 `feat/w82-r3-del-res`）
- **输出画像**：[DEL-RES：12 维现代资源平面强制/降档画像与宿主控制器架构基线](../../management/del-res-resource-profile.md)
- **关联规范**：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §1.1 (`[NLOS-RESOURCE-001]`)、§6 (`[TCB-DOWNGRADE-001]` / `[TCB-EGRESS-001]`)、§10、§12、§16、§28 (`[ROAD-A-004]`)、[NLOS 总任务计划书](../../management/nlos-goal-delivery-plan.md) §5 `DEL-RES`

---

## 1. 环境与可复现信息

| 项 | 实测值 |
|---|---|
| 宿主机操作系统 | `macOS 26.6.2` (`BuildVersion 25G83`) |
| 内核与架构 | `Darwin Kernel Version 25.6.0` (`RELEASE_ARM64_T8142 arm64`) |
| Rust 工具链 | `rustc 1.99.0 (b940084d7 2026-09-28)`、`cargo 1.99.0 (5f94df478 2026-08-27)` |
| 根工作区策略 | [`Cargo.toml`](../../../Cargo.toml) (`edition = "2024"`, `rust-version = "1.97"`, `unsafe_code = "forbid"`, `clippy::all` + `clippy::pedantic = "warn"`) |
| 执行目录 | 隔离工作树 `feat/w82-r3-del-res` @ `9414000f9b19179ca0922d95bbf81e21eff137ab` |
| 未运行宿主平台 | Linux、Windows（本轮仅在单机 macOS 实跑目标 crate 复验与全仓源码审计） |

---

## 2. 实跑测试基线输出（2026-11-21，macOS）

在隔离工作树内按默认 debug profile 实际串行执行以下三条基线测试命令，全部退出码为 `0`：

```zsh
cargo test -p nlos-resource
cargo test -p nlos-lease
cargo test -p nlos-wasm-sandbox
```

### 2.1 `cargo test -p nlos-resource`：**61 passed / 0 failed / 0 ignored**（10 个集成测试二进制 + 0 单测 + 0 doc-test）

| 测试目标 | 通过 | 失败 | 忽略 | 实测覆盖核心断言 |
|---|---:|---:|---:|---|
| `src/lib.rs` (unittests) | 0 | 0 | 0 | — |
| `tests/activation_consume_finalize_restart.rs` | 1 | 0 | 0 | 激活→消费→终结前缀重开幂等重放 |
| `tests/cost_receipt.rs` | 3 | 0 | 0 | owner 派生聚合 `ResourceCostReceipt` 终态门控与零高水位闭合 |
| `tests/cost_receipt_fault_injection.rs` | 4 | 0 | 0 | `ENOSPC` / `EIO` / `PowerLoss` 注入后 cost receipt 恢复收敛 |
| `tests/demand_admission.rs` | 7 | 0 | 0 | `ResourceDemand` 三维（`cpu_shares`/`memory_mib`/`io_weight`）超限拒绝、v5→v6 迁移与 DDL 不可变触发器 |
| `tests/demand_throttle.rs` | 4 | 0 | 0 | `throttle_demand` 百分比钳位（不扩宽）与 Quote 容量回读 |
| `tests/finalize_fault_injection.rs` | 7 | 0 | 0 | `kill -9` / `ENOSPC` / `EIO` / 撕裂尾写入下结算原子性 |
| `tests/finalize_refund.rs` | 7 | 0 | 0 | `FinalizationReceipt` 双分录退款、Driver 轮换围栏、`Quarantined` 收敛与 v5 迁移 |
| `tests/llm_token_budget.rs` | 7 | 0 | 0 | v8 5 维 LLM 预算 checked 算术溢出、逐维耗尽零写入、多步累计消费与 `OpenMetrics` 导出 |
| `tests/resource_authority.rs` | 9 | 0 | 0 | Driver 注册/轮换、`AVAILABLE -> RESERVATION` 守恒、单调高水位、`Quarantine` 冻结、`endpoint_proofs` 覆盖 |
| `tests/throttle_decision_persistence.rs` | 12 | 0 | 0 | v7 不可变节流决策链、`EffectiveDemandRecord` 重放合并与时间戳/内容冲突拒绝 |
| `Doc-tests nlos_resource` | 0 | 0 | 0 | — |
| **合计** | **61** | **0** | **0** | **exit code 0** |

### 2.2 `cargo test -p nlos-lease`：**30 passed / 0 failed / 0 ignored**（14 个集成测试二进制 + 0 单测 + 0 doc-test）

| 测试目标 | 通过 | 失败 | 忽略 | 实测覆盖核心断言 |
|---|---:|---:|---:|---|
| `src/lib.rs` (unittests) | 0 | 0 | 0 | — |
| `tests/capacity_activate.rs` | 1 | 0 | 0 | `CapacityLease` `HostAttachReceipt` 激活转移 |
| `tests/capacity_grant.rs` | 1 | 0 | 0 | `CapacityLeaseGrantor` 池扣减与 `CellAuthority::admit` |
| `tests/capacity_prepare.rs` | 1 | 0 | 0 | `TARGET_PREPARED` 未 ACK 前不退还池容量 |
| `tests/capacity_return.rs` | 1 | 0 | 0 | pre-active 归还在 ACK 时一次性退回 |
| `tests/clock_skew_horizon_properties.rs` | 4 | 0 | 0 | 15,000 组保守时钟偏斜包络扫描零重叠、`QuarantineDisposition` 三族再水化 |
| `tests/device_grant.rs` | 1 | 0 | 0 | `ExclusiveDeviceLeaseGrantor` `FREE` 排他认领 |
| `tests/device_reset.rs` | 1 | 0 | 0 | `DeviceResetReceipt` 解锁 `DeviceLeaseHead` |
| `tests/durable_lease_store.rs` | 1 | 0 | 0 | `SqliteLeaseStore` WAL+FULL 11 表 / 22 触发器三族持久化不变量 |
| `tests/lease_reconciliation.rs` | 11 | 0 | 0 | `FenceBarrierAck` / `CleanupAttestation` / `DeviceReconciliationReceipt` 跨节点迁移与 WAL 丢失隔离 |
| `tests/quota_grant.rs` | 1 | 0 | 0 | `QuotaLeaseGrantor` 预付扣池 |
| `tests/quota_ledger.rs` | 1 | 0 | 0 | 单调高水位、`CLOSING -> CLOSED` 与 pre-active `CANCELLED` |
| `tests/quota_loss.rs` | 1 | 0 | 0 | epoch 推进后未对清额度强制转 `QUARANTINED` |
| `tests/quota_ttl.rs` | 1 | 0 | 0 | 逻辑 TTL 到期转 `FENCED` 并单次结算 |
| `tests/remote_lease_mtls_protocol.rs` | 4 | 0 | 0 | TLS 1.3 mTLS 五轴跨单元租约、`SingleHostDesktop` 零 socket 与 RTT 偏斜隔离 |
| `Doc-tests nlos_lease` | 0 | 0 | 0 | — |
| **合计** | **30** | **0** | **0** | **exit code 0** |

### 2.3 `cargo test -p nlos-wasm-sandbox`：**97 passed / 0 failed / 0 ignored**（1 库单测 + 1 bin 单测 + 4 集成测试二进制 + 0 doc-test）

| 测试目标 | 通过 | 失败 | 忽略 | 实测覆盖核心断言 |
|---|---:|---:|---:|---|
| `src/lib.rs` (lib unittests) | 48 | 0 | 0 | `SandboxProfile` fail-closed 校验、`PageLimiter`/fuel/epoch、`wasi_allowlist` 词法与 `#[cfg(unix)]` 符号链接逃逸、`ToolOperationStore` 同步故障、`host_bin` argv 校验 |
| `src/bin/nlos-wasm-sandbox-host.rs` | 0 | 0 | 0 | — |
| `tests/escape_matrix.rs` | 16 | 0 | 0 | `env_clear()` 零环境继承、显式 argv、零 socket 拒绝、`PageLimiter`、`fuel`、epoch、只读/硬链接/符号链接拒绝、父目录替换防重定位、墙钟强杀 |
| `tests/tool_journal.rs` | 7 | 0 | 0 | 工具日志崩溃恢复、并发 `begin` CAS 与 `EffectUnknown` 保留 |
| `tests/tool_pipes.rs` | 1 | 0 | 0 | 1.64 MiB 双管道并发排空无死锁 |
| `tests/tool_service.rs` | 25 | 0 | 0 | 共享 writer 全局 `16`/每主体 `4` 活宿主配额 (`ToolHostQuota`)、跨实例取消达执行宿主、孤儿进程 watchdog 自灭、256 KiB/60 KiB/8 MiB 界、65,536 B 编码界与 `measured_spawn_and_run` |
| `Doc-tests nlos_wasm_sandbox` | 0 | 0 | 0 | — |
| **合计** | **97** | **0** | **0** | **exit code 0** |

**三包实测总计**：**188 passed / 0 failed / 0 ignored**（均在 2026-11-21 于本机 macOS arm64 实际执行完毕）。

---

## 3. 一手源码与基线证据核验结论

经直接核验目标 crate 源码与仓库历史实测文档，确认以下工程事实：

1. **账本与控制器分离现状（`nlos-resource` + `nlos-system-control`）**：
   - [`crates/nlos-resource/src/schema.rs`](../../../crates/nlos-resource/src/schema.rs)（`SCHEMA_VERSION = 8`）与 [`lib.rs`](../../../crates/nlos-resource/src/lib.rs) 完整实现了费用预留/消耗/隔离/双重记账结算、三维 `ResourceDemand` 准入谓词、不可变节流决策日志和 5 维 `LlmTokenLimits` 账本。
   - [`crates/nlos-system-control/src/resource_throttle_executor.rs`](../../../crates/nlos-system-control/src/resource_throttle_executor.rs) 与 [`working_set_reclaim_executor.rs`](../../../crates/nlos-system-control/src/working_set_reclaim_executor.rs) 仅分别驱动 SQLite `record_throttle_decision` 与 `nlos-task` 的合成 `evicted_units` 计数。
   - **全仓检索核实**：整个 `crates/` 目录下**零** `cgroup v2`、**零** Win32 `JobObject`、**零** `setrlimit`/`RLIMIT_*` 强制控制器实现代码（仅在 [`crates/nlos-runtime-tokio/src/distributed_scheduler.rs`](../../../crates/nlos-runtime-tokio/src/distributed_scheduler.rs) 存在用户态 `ResourceGroupQueuePolicy` 并发槽/WFQ 队列和 `ResourceGroupId` 标识符引用）。
2. **三族租约与物理隔离边界（`nlos-lease`）**：
   - [`crates/nlos-lease/src/lib.rs`](../../../crates/nlos-lease/src/lib.rs)、[`reconcile.rs`](../../../crates/nlos-lease/src/reconcile.rs) 与 [`clock_horizon.rs`](../../../crates/nlos-lease/src/clock_horizon.rs) 已具备三族租约持久化与非互换对账回执，且 `QuarantineDisposition` 类型显式区隔 `LedgerQuarantinedFenceOnly` 与 `PhysicallyCleanedVerified`。
   - 物理 GPU/NPU 设备驱动、VRAM 清零与能源/热控制代码全仓为空（`UNSUPPORTED`，归 `DEL-EXT`）。
3. **进程隔离与多平台差距（`nlos-process` + `b-sandbox-001-isolation-comparison`）**：
   - [`crates/nlos-process/src/platform_kill.rs`](../../../crates/nlos-process/src/platform_kill.rs)、[`supervisor.rs`](../../../crates/nlos-process/src/supervisor.rs) 与 [`managed.rs`](../../../crates/nlos-process/src/managed.rs) 实现了 Unix `process_group(0)` + `killpg`（含 Darwin `EPERM` 空僵尸组识别）与 Windows `taskkill /F /T` 整树终止，但受根 `Cargo.toml` `unsafe_code = "forbid"` 约束未接入 Windows Job Object API，保留 `U-2` 跨权限与 Job breakaway 残域。
   - [b-sandbox-001-isolation-comparison.md](./b-sandbox-001-isolation-comparison.md) §2 D2 已实测证明 macOS `setrlimit(RLIMIT_AS)` 返回 `EINVAL`（`RLIMIT_RSS` 为 no-op），故 macOS 原生宿主进程内存限制必须保持 `UNSUPPORTED` / `COOPERATIVE_DOWNGRADE`。
4. **双层 Wasm 沙箱强制范围（`nlos-wasm-sandbox`）**：
   - [`isolate.rs`](../../../crates/nlos-wasm-sandbox/src/isolate.rs)、[`wasi_allowlist.rs`](../../../crates/nlos-wasm-sandbox/src/wasi_allowlist.rs)、[`tool_host_protocol.rs`](../../../crates/nlos-wasm-sandbox/src/tool_host_protocol.rs)、[`process.rs`](../../../crates/nlos-wasm-sandbox/src/process.rs) 与 [`tool_service.rs`](../../../crates/nlos-wasm-sandbox/src/tool_service.rs) 严格强制 Wasm 客体 64 KiB 线性内存页上限、fuel、epoch、零 socket 导入、单次调用 8 MiB 文件写/4096 创建尝试/256 KiB stdout/60 KiB stderr 及共享 writer 活宿主配额 `16`/`4`。该强保障严格限定于 Wasm 沙箱边界内（`GUEST_SANDBOX_ONLY`）。
5. **LLM 网关 `OBSERVED` 边界（`nlos-driver-llm-http`）**：
   - [`llm_gateway.rs`](../../../crates/nlos-driver-llm-http/src/llm_gateway.rs) 与 [`shim.rs`](../../../crates/nlos-driver-llm-http/src/shim.rs) 在调用前校验全祖先五维包络并强制标注 `PROVIDER_ISOLATION_UNVERIFIED_TAG`；源码明确界定其保障档位为 `OBSERVED`（provider failover、不可观察费用及外部直连均可产生高于内部账本的真实费用）。
6. **消息容量、调度与注意力支撑面**：
   - [`nlos-schema/src/lib.rs`](../../../crates/nlos-schema/src/lib.rs)、[`nlos-channel/src/lib.rs`](../../../crates/nlos-channel/src/lib.rs)、[`nlos-topic/src/lib.rs`](../../../crates/nlos-topic/src/lib.rs)、[`nlos-capability/src/blob_model.rs`](../../../crates/nlos-capability/src/blob_model.rs) 与 [`nlos-net/src/frame.rs`](../../../crates/nlos-net/src/frame.rs) 实现了受控路径内的消息容量硬边界。
   - [`desktop/src-tauri/src/trusted_ui.rs`](../../../desktop/src-tauri/src/trusted_ui.rs) 与 [`nlos-notify/src/lib.rs`](../../../crates/nlos-notify/src/lib.rs) 落地了 `Z4` 防遮挡与一次性动作令牌，但系统级注意力速率限流、静默时段与队列隔离尚未落地。

---

## 4. 显式未运行项与不可跨界声明清单

1. **未实现组件（严格 `DESIGN / NOT_IMPLEMENTED`）**：Linux `cgroup v2` controller shim、Windows `Job Object` controller shim（含禁止 `JOB_OBJECT_LIMIT_BREAKAWAY_OK` 双端测试）、跨平台 `SystemStorageReservePool` (`ENOSPC` 救援池)、macOS `HostCapabilities` 自动降档透传链、以及系统级通知注意力速率/静默策略控制器，目前均**仅有架构设计，无任何实现代码**。
2. **物理硬件与外部资源归 `DEL-EXT`**：物理 GPU/NPU 显存隔离与硬件复位清零、能源 `mJ`/`mW`/DVFS 与热策略、层 3 真实硬件掉电（PDU/IPMI/STONITH）与介质校准、原生安装包签名公证均归属 `DEL-EXT`，本单机软件核验不可替代。
3. **本轮未运行平台/套件**：本轮未在 Linux 与 Windows 宿主机运行 tests（Linux cgroup/openat2 与 Windows `platform_kill`/NTFS junction 留待对应宿主或三平台 CI 执行）；本轮亦未运行 `nlos-driver-llm-http`、`nlos-process` 或全 `--workspace` 测试。
4. **生产证据状态**：`H8 = 0`。

---

## 5. 写集与并发隔离声明

- 本次核验与建档严格在隔离工作树 `feat/w82-r3-del-res`（基线 `9414000f9b19179ca0922d95bbf81e21eff137ab`）内执行。
- 独占写集严格仅包含：
  1. [`docs/management/del-res-resource-profile.md`](../../management/del-res-resource-profile.md)
  2. [`docs/evidence/stage-b/b-res-profile-audit-20261121.md`](./b-res-profile-audit-20261121.md)
- 未修改任何 Rust 源码、`Cargo.toml`、`Cargo.lock`、`claims.yaml`、`evidence-index.yaml`、`README.md`、主工作区或 `.agentdash/` 台账。
