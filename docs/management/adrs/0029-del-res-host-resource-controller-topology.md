# ADR-0029：DEL-RES 宿主 OS ResourceGroup 控制器拓扑、原子出生策略与 macOS 诚实降档架构决策

- **状态**：**PROPOSED**（DESIGN 草案 — 尚未获人类维护者批准；签认前 `Lane-RES-1..4` 不得创建 `crates/nlos-resource-controller` / `crates/nlos-host-shim-win32` 或修改 `nlos-resource` / `nlos-process`，现网 `COOPERATIVE_DOWNGRADE` / `GUEST_SANDBOX_ONLY` 分级维持不变）
- **日期**：2026-11-22
- **Owner**：DEL-RES 工作族（`nlos-resource` / `nlos-process` / `nlos-system-control` / `desktop`）
- **基线快照**：`3c3e9457cdd72f5371b74a4b59d916a78735f4cc`
- **关联 Requirement**：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) `ROAD-A-004`、`[RSM-SEPARATE-001]`、`[RSM-CONTROLLER-001/002]`、`[RSM-HIER-001/002]`、`[RSM-GROUP-001]`、`[RSM-RESERVE-001]`、`[RSM-ENFORCE-001]`、`[RSM-PRESSURE-001/002]`、`[RSM-RECLAIM-001/003]`、`[RSM-METER-002]`、`[RSM-ADMIT-002]`、`[PROC-SPAWN-001]`、`[PROFILE-IMPL-001]`、`[TCB-DOWNGRADE-001]`、`[BUD-TIER-001]`、`[BUD-LATE-001]`、`[EVID-SCOPE-001]`、未知边界 `U-2`
- **关联前序 ADR**：[ADR-0015](./0015-dual-layer-untrusted-execution-isolation.md)（双层隔离语义）、[ADR-0022](./0022-operator-root-capability-issuance.md)（准入与根能力签发）
- **关联设计与证据**：[del-res-resource-profile.md](../del-res-resource-profile.md)（12 维 x 3 平台画像 §3 全部 `DESIGN` 来源）、[b-res-profile-audit-20261121.md](../../evidence/stage-b/b-res-profile-audit-20261121.md)（`H3`，`188` macOS 测试）、[b-sandbox-001-isolation-comparison.md](../../evidence/stage-b/b-sandbox-001-isolation-comparison.md)（§2 D2 macOS `RLIMIT_AS` 否定实证）、[del-base-coverage-audit.md](../del-base-coverage-audit.md)
- **复审触发器**：见第 8 节

---

## 1. 上下文与阻塞全貌

### 1.1 现网事实（不可逾越的基线）

依据 [del-res-resource-profile.md §2](../del-res-resource-profile.md) 一手源码审计与实测（`b-res-profile-audit-20261121` `H3`）：

1. **宿主 OS 级执行器实装为零**：`nlos-resource::ResourceDemand::{cpu_shares, memory_mib, io_weight}` 与 `record_throttle_decision` 仅为 SQLite 声明与决策账本；三平台均无 `cgroup v2` / `Job Object` / `setrlimit` 宿主硬限执行器。`nlos-system-control` 的 `resource_throttle_executor.rs` / `working_set_reclaim_executor.rs` 同属账本/合作降档面。
2. **唯一的真实强制面在 Wasm 客体侧**：`nlos-wasm-sandbox`（`PageLimiter`、确定性 `fuel`、`EpochTicker`、`ToolHostLimits`、`killpg`）与 `nlos-net` mTLS 流控、服务边界帧长/水位自限构成现网 `GUEST_SANDBOX_ONLY` / 有界自限的全部实证。
3. **macOS 宿主原语否定实证**：`setrlimit(RLIMIT_AS)` 在 macOS arm64 恒 `EINVAL`、`RLIMIT_RSS` 无操作、XNU 无 cgroup/Job 对等容器原语——**任何 macOS「内存/IO 硬限」宣称均属伪造**。
4. **根 `Cargo.toml` `unsafe_code = "forbid"` 红线**：宿主控制器实现必须在不违反本红线的前提下分层放置 FFI。
5. **`U-2` 未闭合**：Windows 进程树 breakaway / 跨权限脱离仍是开放边界。

### 1.2 本 ADR 的决策范围

`del-res-resource-profile.md §3` 已给出完整 `DESIGN`，本 ADR 不重复设计内容，只裁定落地所必须的四个分歧点（决策 1–4）。签认后 `Lane-RES-1` 解除阻塞，`Lane-RES-2/3/4` 按写集并行，`Lane-RES-INT` 收口。

**显式不在本 ADR 范围**：`crates/nlos-host-shim-win32`（Win32 Job Object FFI）与 `clone3(CLONE_INTO_CGROUP)` 均为 `unsafe` FFI 载体，依 [README §7](../README.md#7-评审与决策机制) 须各自提交独立 ADR 方可实施；本 ADR 只预留其挂载位置与禁止事项，不构成其开工许可。

---

## 2. 决策 1：宿主控制器 crate 拓扑（账本/控制器解耦落点）

### 2.1 候选方案

- **方案 1A（推荐）：新建 `crates/nlos-resource-controller` 承载契约与全部宿主控制器实现**
  - 契约面：`HostResourceController` trait、`ResourceControllerManifest`、`ControllerBinding` 状态机（`DETACHED → ATTACHING → ACTIVE ↔ PRESSURED → FROZEN/RECLAIMING → FENCING → CLEANING → DETACHED | QUARANTINED`）、`PressureEvent`、`ReclaimRequest`、`HostCapabilities::probe()`、`GuaranteeTier` 与 `DowngradeReason` 结构化枚举、`HostControllerProof`。
  - 依赖方向单向：`nlos-resource-controller` 依赖 `nlos-resource`（账本只读引用）与 `nlos-process`（仅经 `attach_controller_hook` 扩展点）；反向依赖为零。
  - `crates/nlos-resource/src/{lib.rs, schema.rs}` 仅允许追加 `HostControllerProof` 只读引用字段（不混写 `ResourceAccount` 账本表），兑现 `[RSM-SEPARATE-001]`「账本不管 OS 隔离」。
  - crate 根维持 `#![forbid(unsafe_code)]`；Linux cgroup v2 实现经 `/sys/fs/cgroup` 虚拟文件读写 + `poll` 达成零 `unsafe`。
- **方案 1B（否决）：控制器并入 `nlos-resource` 或 `nlos-process` 既有 crate**
  - 并入 `nlos-resource` 使不可变账本与易变宿主适配同 crate 同审计面，违反 `[RSM-SEPARATE-001]` 并扩大 TCB 扰动半径；并入 `nlos-process` 则让 supervisor 携带平台资源语义，破坏其作为跨阶段复用组件的边界，且 Windows/Linux 平台模块会拉入与 `nlos-process` 无关的依赖面。

| 评估维度 | 1A（新建 crate） | 1B（并入既有 crate） |
|---|---|---|
| `[RSM-SEPARATE-001]` 契合 | 账本/控制器物理分层，账本表零混入 | 账本与宿主适配同居，边界稀释 |
| 不安全代码隔离 | `forbid(unsafe_code)` 整 crate 成立；FFI 另立 shim crate | FFI 若入 crate 将被迫局部放宽 lint 或混排 |
| 演进/回退成本 | 新 crate 可整体摘除回退 | 回退需从账本 crate 剥离代码 |

### 2.2 决策结论

**拟采纳 1A**。新建 `crates/nlos-resource-controller`（根 `Cargo.toml` / `Cargo.lock` 挂载由 `Lane-RES-INT` 单一 integrator 串行执行）；`nlos-resource` 仅追加只读 `HostControllerProof` 引用字段；Windows Job FFI 与 `clone3` 路径另置独立 crate 并各需独立 ADR。

---

## 3. 决策 2：原子出生策略（spawn→入组窗口防逃逸）

### 3.1 候选方案

- **方案 2A（推荐）：受控同步管道门控（readiness pipe gating），零 `unsafe`**
  - 子进程入口阻塞读 readiness pipe；父进程写入 `cgroup.procs`（Linux）/`AssignProcessToJobObject`（Windows）成功后关闭写端放行。
  - 满足 `[PROC-SPAWN-001]` 主窗口闭合：自 spawn 至入组确认间子进程用户 `main` 不执行。
  - **诚实残余窗口**：无法阻止已加载第三方二进制在 `main` 前运行的动态库构造函数/`.init` 段逃逸入组前执行——该窗口计入残余风险并在 Receipt 留痕，不宣称绝对原子。
- **方案 2B（延后）：`clone3(CLONE_INTO_CGROUP)` 内核态原子出生（Linux ≥ 5.7）**
  - 出生即入组，窗口内核级闭合；但 `clone3` 须 `unsafe` FFI，依红线须另置微 crate 并独立 ADR + 安全审查，工期与审查面显著大于 2A。

| 评估维度 | 2A（管道门控） | 2B（clone3） |
|---|---|---|
| `unsafe` 红线 | 零 `unsafe` 可落地 | 强制 `unsafe` + 独立 crate + 独立 ADR |
| 逃逸窗口 | 闭 `main` 前窗口；留动态库 ctor 残余（明示） | 内核级闭合 |
| 与既有 supervisor 兼容 | `attach_controller_hook` 扩展点即可 | 需改造 spawn 路径，跨平台分支发散 |

### 3.2 决策结论

**拟采纳 2A**（默认全部平台）；2B 留作后续独立 ADR 议题（触发器见 §8.3-2）。Windows 侧采用等价 `CREATE_SUSPENDED + AssignProcessToJobObject + ResumeThread` 原子出生，并执行 `KILL_ON_JOB_CLOSE`/`DIE_ON_UNHANDLED_EXCEPTION` 强制开启与 `BREAKAWAY_OK`/`SILENT_BREAKAWAY_OK` **双端断言永禁**（写入前与读回核验两处），跨权限 UAC/任务计划脱离按 `CrossPrivilegeElevationUnsupported` 诚实标注（`U-2` 残余边界）。

---

## 4. 决策 3：macOS 降档契约（诚实降档，禁止伪硬限）

基于 `b-sandbox-001-isolation-comparison` §2 D2 已验证事实（`RLIMIT_AS` 恒 `EINVAL`、`RLIMIT_RSS` no-op、无容器原语），拟裁定单一方案：

1. **禁止项**：禁止调用 `setrlimit(RLIMIT_AS)`/`RLIMIT_RSS` 伪造内存配额；禁止把合成 `evicted_units`、目录账本或 `cpu_shares` 记录外宣为 OS 硬限。
2. **probe 固定返回值**：macOS `HostCapabilities::probe()` 返回 `host_memory_hard_cap = Unsupported`、`host_io_bw_hard_cap = Unsupported`、`host_cpu_bw_hard_cap = CooperativeOnly`。
3. **Receipt 强制透传**：原生进程负载携带 `GuaranteeTier::CooperativeDowngrade` + `DowngradeReason::MacOsHostNoGroupQuota`；Wasm 工具负载携带 `GuaranteeTier::GuestSandboxOnly`；`ResourceContract::requested_guarantee == Strict` 时准入 fail-closed 返回 `TypedDenial::HostCapabilityInsufficient`（`[BUD-TIER-001]`）。
4. **真实观测面（`METERED_OBSERVED`）**：仅允许 `proc_pid_rusage`/`getrusage`（`ri_phys_footprint`、`ri_diskio_*`、`ri_user/system_time`）与 `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE` 产出 high-water 与 `PressureEvent`（软告警/背压输入，非硬限）。
5. **桌面 `Z4` 映射**：`trusted_ui.rs` 的 `Z4ResourceCeilingSpec` 与 `ControlCenterDomainKind::ResourceGroup` 面板必须消费结构化 `DowngradeReason` 展示「macOS 宿主不对原生进程强制内存/I/O 硬限」警示条；严禁 Z4 伪造回归（归入 `Lane-RES-4` 验收门）。

**决策结论**：**拟采纳（单一方案）**。无备选——任何「macOS 宿主硬限」方案均无内核原语支撑，伪造将违反 `[EVID-SCOPE-001]`。

---

## 5. 决策 4：`SystemStorageReservePool` 救援池放置与跨平台降级表

1. **放置**：`SystemStorageReservePool` 实现于 `nlos-resource-controller`（`storage_reserve.rs`，`Lane-RES-4` 写集），被 `ArtifactStore::put_revision`/`stage_revision` 准入调用；`system_reserve_bytes` 默认 ≥ 256 MiB，触及地板准入前返回 `StoragePressure::SystemReserveProtected`，绝不删权威恢复日志腾空间（`[RSM-RESERVE-001]`）。
2. **三平台降级表（诚实标注）**：
   - **Linux**：XFS/ext4 project quota（`FS_IOC_FSSETXATTR`）→ 否则 `statvfs` + 目录树账本标注 `ApparentQuotaOnly`。
   - **Windows**：`GetDiskFreeSpaceExW` 余量 + 每工作区目录字节累加预算；NTFS 每用户/卷配额无管理员权限时标注 `CooperativeDowngrade`。
   - **macOS**：`statvfs` 卷余量 + APFS 配额探测；沿用 `ArtifactStore`/`BlobLedger::guard_storage` 有界账本与 `ToolHostLimits` 8 MiB 逐次写硬限，附带 `CooperativeDowngrade`。
3. **决策结论**：**拟采纳**。救援池与组级存储限额统一走「真实原语 → 明示降档 → 结构回执」三段式，与其他维一致。

---

## 6. 维护者签认矩阵

维护者可逐项复核；若全部按推荐采纳，回复 **`批准 ADR-0029: 1A/2A/3/4`** 即授权 `Lane-RES-1` 开工（或显式指定替代组合）：

| 决策 | 议题 | 推荐 | 备选 | 核心不变式与影响 | 解锁 |
|---|---|---|---|---|---|
| 1 | 控制器 crate 拓扑 | **1A** 新建 `nlos-resource-controller` | 1B 并入既有 crate（否决） | `[RSM-SEPARATE-001]` 账本/控制器分层；新 crate `forbid(unsafe_code)` | Lane-RES-1 契约面 |
| 2 | 原子出生 | **2A** 管道门控零 `unsafe` | 2B `clone3`（延后独立 ADR） | `[PROC-SPAWN-001]` 主窗口闭合；动态库 ctor 残余明示 | Lane-RES-1/2/3 spawn 接线 |
| 3 | macOS 降档 | **3** 诚实降档单一方案 | 无 | 禁 `RLIMIT_AS`/`RLIMIT_RSS` 伪造；`GuaranteeTier` + `DowngradeReason` 全链路透传；Z4 警示 | Lane-RES-4 macOS 面 |
| 4 | 存储救援池 | **4** `nlos-resource-controller::storage_reserve` + 三平台降级表 | 无 | `[RSM-RESERVE-001]` 救援地板不可挪用；无配额原语平台明示 `ApparentQuotaOnly`/`CooperativeDowngrade` | Lane-RES-4 存储面 |

---

## 7. 实施写集规划（与 `del-res-resource-profile.md §3.6` 一致，签认后生效）

```text
[前置屏障] 维护者批准 ADR-0029
        │
        ▼
Lane-RES-1 契约与台账（无阻塞，先行）
        │
        ├──────────────┬──────────────┐
        ▼              ▼              ▼
   Lane-RES-2     Lane-RES-3      Lane-RES-4
   Linux cgroup   Windows Job*    macOS 降档+存储救援+Z4
   （需委托 runner） （需独立 FFI ADR 前置）  （本机可验）
        └──────────────┴──────────────┘
                       ▼
            Lane-RES-INT 集成接线 + 台账同步
```

*Lane-RES-3 的 `nlos-host-shim-win32` FFI crate 须在 Lane-RES-1 契约定稿后另提交独立 ADR 方可开工；其间可先交付 `windows_job.rs` 的 trait 实现骨架（不含 FFI 调用，`#![forbid(unsafe_code)]` 成立）。

| 车道 | 独占写集 | 关键退出门 |
|---|---|---|
| Lane-RES-1 | `crates/nlos-resource-controller/**`（新 crate）、`crates/nlos-resource/src/{lib.rs,schema.rs}`（仅 `HostControllerProof` 追加） | trait/状态机/manifest/probe/tier 枚举定稿 + 单测；`cargo test -p nlos-resource-controller -p nlos-resource` 全绿 |
| Lane-RES-2 | `…/src/linux_cgroup.rs`、`tests/linux_cgroup_enforcement.rs`、`nlos-process/src/supervisor.rs`（仅 `attach_controller_hook`） | Linux runner 实测 `cpu.max`/`memory.*`/`io.max`/`pids.max`/`cgroup.kill`/PSI + OOM Receipt |
| Lane-RES-3 | `crates/nlos-host-shim-win32/**`（独立 ADR 后）、`…/src/windows_job.rs`、`nlos-process/tests/platform_kill.rs` | Windows runner 实测 Job 限制/IOCP 回执 + `BREAKAWAY_OK` 双端断言对抗 |
| Lane-RES-4 | `…/src/{macos_probe.rs,storage_reserve.rs}`、`nlos-system-control/src/control_stage_d.rs`、`desktop/src-tauri/src/{trusted_ui.rs,settings_keychain.rs}` | macOS probe 固定返回 + `SystemStorageReservePool` ENOSPC 救援 + Z4 禁伪造回归 |
| Lane-RES-INT | 根 `Cargo.toml`/`Cargo.lock`、`system-control-daemon.rs`、`docs/management/{claims,risks,evidence-index,stage-b-progress}` | 端到端 `AdmissionPlan → Supervisor → 宿主控制器 → Receipt`；全量安全/故障门重跑；`ROAD-A-004`/`U-2`/`RISK-B-03/10` 台账更新 |

**强制验收门禁**：`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、受影响 crate 全量、`RES-PRESSURE-01/02`、`RES-CANCEL-01` 对抗矩阵、`git diff --check`、`lint_claims.py`、`verify_stage_e_ledger_coverage.py --mode strict` 全绿；`#[forbid(unsafe_code)]` 在 `nlos-resource-controller` 编译期成立（CI 断言）。

---

## 8. 后果、退出策略与复审触发器

### 8.1 正面收益与显式接受的代价

- **收益**：在不动摇账本 TCB、不违 `unsafe_code = "forbid"` 的前提下，获得三平台宿主控制器的一致契约面与诚实降档闭环；`U-2` 首次获得可验证闭合路径；`ROAD-A-004` 自 `DESIGN` 进入可实施状态。
- **代价（显式接受）**：
  - 新增 crate 与 supervisor 扩展点带来集成面（由 Lane-RES-INT 单一 integrator 收口）；
  - 2A 残余动态库 ctor 窗口（明示、计数为残余风险而非闭合 `PROC-SPAWN-001` 全案）；
  - `nlos-host-shim-win32` 与 `clone3` 各需一份后续独立 ADR，Lane-RES-3 实质开工晚于 Lane-RES-2。

### 8.2 退出策略

- 控制器层为独立 crate，可按 `DEPRECATED → SUNSET` 整体摘除回退至现网 `COOPERATIVE_DOWNGRADE`；`nlos-resource` 的 `HostControllerProof` 引用字段为只增字段，回退后保留不碍兼容。
- 2B/FFI crate 若后续获批，为追加式实现，不推翻 2A 契约面。

### 8.3 复审触发器

1. **`clone3(CLONE_INTO_CGROUP)` 立项需求实证**（如动态库 ctor 窗口被证明为真实逃逸路径）→ 触发决策 2 复审与 `clone3` shim ADR。
2. **`nlos-host-shim-win32` 实施前** → 强制提交独立 FFI ADR（本 ADR 不构成其开工许可）。
3. **Linux cgroup v2 委托权限在 CI 不可得** → 触发 `RES-PRESSURE-01` 验收路径复审（外部 runner / 特权容器方案）。
4. **出现第四类宿主资源（GPU/NPU/网络带宽）硬限诉求** → 触发 12 维画像与控制器清单复审。
