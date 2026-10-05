# Cell 七件套装配接线图（W47-L2 产出，2026-10-05）

> 依据 v0.5 §26.1"蜂窝式权威"：每 Cell 本地七件。首批两件（failure detector、capability/name cache）已实现于 `nlos-cell`（`0f7d375`/`e3f877c`）；本图是其余五件的装配施工图。
>
> **共同形态结论**：五件都不搬进 `nlos-cell`——Cell = 装配体（slice-k 或其 Stage-C 后继 daemon crate）按值持有各权威；`nlos-cell` 保持 identity/epoch/fencing + 内存态两件。
>
> **关键装配不变量**：`CellAuthority` 不可克隆且 `nlos-lease` 按值持有它，故 epoch 推进必须由**单一持有者**发起，并向快照消费者（`FailureDetector` / `CapabilityNameCache` 均收 `&CellFence`）广播 `on_epoch_advanced`。

| 件 | 现有机械 | 装配形态 | 依赖边变化 | ADR-0018 关系 |
|---|---|---|---|---|
| Process supervisor | `nlos-process`：`ProcessSupervisor`（spawn/suspend/resume/kill）+ `SupervisorPidRegistry`（世代栅栏 pid 映射）+ `PlatformKillAdapter`（Posix/Windows/Stub）；`ProcessAuthority` 持久 kill 回执 | 直接按值组合（`SliceKRuntime` 已持有 `ProcessAuthority`），无需新 trait；心跳来源 = supervisor spawn/exit 观察 → `MonitoredSubject::Process` | 装配 crate → nlos-process（已有）、→ nlos-cell（新增）；**nlos-cell 不依赖 nlos-process**（避免层级倒置） | 每 Cell 进程一套 supervisor；杀第二 Cell 进程 = 文档化分区注入手段 |
| Resource lease 子账本 | `nlos-lease` 三族（W46 完备），已按值持有 `CellAuthority` 且 admit 委托 `CellAuthority::admit`；`advance_epoch_and_quarantine` 实现 `LEASE-LOSS-001` | 直接依赖（边已存在）；组装者 claim 后传入；epoch 推进经 grantor 后取新 fence 广播给两件新组件 | 无新增（`nlos-lease → nlos-cell` 已有） | 控制面最小面 = 单写者 + lease/fencing（决定 4，无共识） |
| Driver gateway | `nlos-driver-mock`（`MockProvider`+`ProviderCache` 降级/恢复 + ADR-0011 认证 IPC 面）+ `nlos-operation`/`nlos-store` durable prepare→activate（§15 enforcement shim 半边） | 组合为 Cell 本地 gateway；如需与 provider 解耦，在**装配 crate** 定义 port trait（仿 `PlatformKillAdapter` trait+adapter 模式），不在 nlos-cell 定义 | 装配 → nlos-driver-mock/nlos-operation/nlos-ipc（均已有）；未来 `DRV-BOUND-001` ENFORCED_DISTRIBUTED 的 lease-epoch 校验是 nlos-driver-mock ↔ nlos-cell 的唯一潜在直接边（或经装配 trait） | 每 Cell 独立 gateway；掐认证 socket = `Unreachable` 注入 |
| durable event/outbox | `nlos-outbox` 同步消费核心（`OutboxSource`/`WakeSink`/`ReconcileSink`，at-least-once）+ `nlos-store` SQLite 面 + slice-k `OutboxPump` 异步泵 + `nlos-semantic` admission outbox | **trait 借用已是其设计**（`OutboxSource` 即 port）；每 Cell 一个 `<cell_root>` 数据目录一条泵 | 装配 → nlos-outbox/nlos-store/nlos-semantic（已有）；跨 Cell 投递不属此件（归 ADR-0013 扩展点/共识 ADR，明示不做） | per-Cell durable root |
| Artifact cache | `nlos-artifact` `ArtifactStore` 的 `cache_entries` 表（digest 引用计数保护、`cache/` 与 `artifacts/` 独立保留域、`CTX-NOTDATA-001`） | 直接依赖；`ArtifactStore::open(<cell_root>/artifacts)` 每 Cell 一实例（`SliceKRuntime` 现状即可）；无需 trait，跨 Cell 同步（`DIST-ART-001`）到来才需 port | 无新增 | per-Cell cache；跨 Cell 同步后置到双 Cell Evidence 之后 |

**首批两件的 epoch 语义**（实现于 `crates/nlos-cell/src/{failure_detector,name_cache}.rs`，测试 `tests/{failure_detector,name_cache}.rs`）：
- failure detector（`DIST-FAIL-001`）：怀疑不跨 epoch 晋升 Dead；心跳证据不跨 epoch 重计时；Dead 判决不可变，对象以新 incarnation 重入（防旧实例复活，配 `DIST-NAME-001`）。
- capability/name cache：失效高水位单调只升（`CAP-REVOKE-001`）；条目世代栅栏不可越过；epoch 推进旧条目一律不可见但世代高水位跨边界存活；miss 一律 `None`（`NS-NOENT-001` 不泄露存在性）。
