# ADR-0027：阶段 E SABI v1.0 长期兼容制度、核外兼容垫片与生态商店治理架构（Stage E 立项）

- **状态**：**`CANDIDATE`**（2026-10-29 `W76` 立项，2026-10-30 `W76.1` 增补第 7 通道 `PrincipalHandshake` 9/9 入冻收口路径与 `E0` 用户态真实 LLM 开箱引导边界；2026-10-30 `W77-PRE` 已按本 ADR `KABI-SVC-001` / `DRV-CRED-001` / `DRV-SHIM-001` 边界完成先行工作包 `E0` `H4` 实证，核心兼容与商店治理主线 `E1`..`E7` 仍为 `DESIGN`；待 `W77` 9/9 通道入冻 + 五语言黄金线协议语料库与 `W78` 核外兼容垫片原型验证通过后晋升 `ACCEPTED`，待 `W81` `[ROAD-E-001]` 跨主系统版本零重编译回归终审通过后晋升 `VERIFIED`）
- **日期**：2026-10-29
- **Owner**：Stage E 兼容制度与生态规模化工作族（先行开箱试用包 `E0` + 核心主线 `E1`..`E7`；唯一编排与进度权威入口：[stage-e-progress.md](../stage-e-progress.md)）
- **关联 Requirement**：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §28.5 `ROAD-E-001`；§9 `COMPAT-VER-001`、`COMPAT-VER-002`、`COMPAT-DEPRECATE-001`、`KABI-IDEM-001`、`KABI-HANDLE-001`、`KABI-SVC-001`、`KABI-ERR-001`、`KABI-UNCERTAIN-001`、`SABI-DISC-001`、`SABI-COMMON-001`、`SABI-RECEIPT-001`、`SABI-AUTH-001`、`SABI-TRANSPORT-001`、`SABI-CONFORM-001`；§15.1 `DRV-NEGO-001`、`DRV-BOUND-001`、`DRV-ACTIVE-001`、`DRV-CRED-001`、`DRV-PROTO-001`、`DRV-LIFE-001`、`DRV-SHIM-001`、`DRV-DISC-001`；§23.1–§23.2 `PKG-SEP-001`、`PKG-IMM-001`、`PKG-SEED-001`、`PKG-UPDATE-001`、`PKG-UNINSTALL-001`、`PKG-OCI-001`、`PKG-MULTI-001`、`PKG-APPID-001`；§6 `TCB-BYPASS-001`、`TCB-EGRESS-001`、`TCB-DOWNGRADE-001`；§7 `CAP-UNFORGE-001`、`CAP-ATTEN-001`、`CAP-REVOKE-001`；§29.2 `NLOS-DEFER-001`；§30 `EVID-SCOPE-001`、`EVID-NONLINEAR-001`、`EVID-NEG-001`、`EVID-REPRO-001`、`EVID-GEN-001`
- **关联前序 ADR**：[ADR-0001](./0001-stage-b-core-language-and-runtime.md)、[ADR-0003](./0003-stage-b-idl-and-canonical-encoding.md)、[ADR-0010](./0010-signed-capability-commands.md)、[ADR-0011](./0011-ipc-principal-auth-signature-passthrough.md)、[ADR-0013](./0013-cross-authority-verify-then-commit-contract.md)、[ADR-0014](./0014-schema-channel-freeze-v1-beta.md)、[ADR-0015](./0015-dual-layer-untrusted-execution-isolation.md)、[ADR-0022](./0022-operator-root-capability-issuance.md)、[ADR-0024](./0024-driver-provider-payload-codec-freeze.md)、[ADR-0025](./0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md)、[ADR-0026](./0026-cross-physical-host-transport-and-consensus-expansion.md)
- **关联工作包与风险项**：阶段 E 八大核心工作包 `E0` + `E1`..`E7`（定义见 [stage-e-progress.md](../stage-e-progress.md) §E.2）；承接 [exit-unknown-risks.md](../exit-unknown-risks.md) 与 [risks.yaml](../risks.yaml) 中的 `RISK-B-05`（`premature-format-freeze`：含第 7 通道 `PrincipalHandshake` 入冻补齐 9/9 通道冻结及 `SABI v1-beta -> v1.0` 正式长期冻结）、`RISK-B-09`（`third-party-supply-chain`）、`U-7` / `U-8`（跨生态 Typed Selector 解析与授权门）、`U-9`（第三方不可信负载双层强沙箱隔离）与 `U-10` 本地开箱真实 LLM HTTP 用户态驱动试用衔接，并预登记阶段 E 专项风险 `RISK-E-01`..`RISK-E-05`
- **决策来源**：2026-10-29（`W76`）维护者正式批复阶段 C（[stage-c-progress.md](../stage-c-progress.md)）与阶段 D（[stage-d-progress.md](../stage-d-progress.md)）收口退出并开启阶段 E；2026-10-30（`W76.1`）维护者指令将剩余两项协议契约冻结收口动作（`PrincipalHandshake` 第 7 通道入冻 + `SABI v1-beta -> v1.0` 长期冻结）及先行本地开箱真实试用工作包 `E0`（`W77-PRE`）正式排入阶段 E 计划
- **证据**：**`E0` 先行试用边界已验证 `H4`**（[E-TRIAL-001](../../evidence/stage-e/e-trial-001-llm-http-driver.md)、[E-TRIAL-002](../../evidence/stage-e/e-trial-002-quickstart-and-starter-app.md)）；**四大核心兼容与商店治理主线 `E1`..`E7` 仍为 `DESIGN`**（在 `W77`–`W81` 对应验证门禁通过前严禁宣称 `E1`..`E7` 已实现）
- **复审触发器**：见文末第 6 节

---

## 1. 上下文与阶段交接契机（Context & Stage C/D Exit Transition）

### 1.1 前序阶段收官事实与阶段 E 开启背景

按照 [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §28 的路线拓扑，NLOS 已依次完成阶段 B（单机通用应用平台，`W34-D` `EXITED`），并在 `W35–W75` 双轨收口了阶段 C（分布式现代系统：[W67 单机退出评审](../../evidence/stage-c/reviews/w67-stage-c-exit-gates.md) + [W73 跨主机退出终审](../../evidence/stage-c/reviews/w73-stage-c-xhost-exit-review.md)，最高保障等级 `H6`）与阶段 D（完整自然语言桌面：[D-ROAD-001](../../evidence/stage-d/d-road-001-non-developer-multi-app-e2e-gate.md) + [W74 阶段 D 退出终审](../../evidence/stage-d/reviews/w74-stage-d-exit-gates.md)，最高保障等级 `H4`）。在维护者于 `W76` 批准阶段 C 与阶段 D 退出后，系统正式进入路线图收官阶段——**阶段 E：兼容制度与生态规模化（§28.5）**。

在代码与协议现状上，仓库已具备进入阶段 E 的关键地基，但距离真正的第三方解耦生态仍有四道结构性鸿沟：

1. **线协议尚有 1 个通道未入冻且全表面仍处于 `v1-beta` 预冻结与同仓同步编译态**：当前 [crates/nlos-schema](../../../crates/nlos-schema) `REGISTRY` 共 9 个已注册 SABI 通道中，已有 **8 个为 `frozen: true`**——其中 `nlos.sabi.Envelope`、`nlos.sabi.ServiceDirectory`、`nlos.sabi.OperationControl`、`nlos.sabi.SystemControl`、`nlos.sabi.TakeoverControl`、`nlos.sabi.WaitControl` 6 个核心通道依 [ADR-0014](./0014-schema-channel-freeze-v1-beta.md) 确立 `v1-beta` additive 冻结，第 8 个通道 `nlos.sabi.DriverProvider` v1.0 依 [ADR-0024](./0024-driver-provider-payload-codec-freeze.md) 冻结，第 9 个通道 `nlos.sabi.ChannelControl` v1.0 依 `W73` `L-002`（[W73-DOGFOOD-001](../../evidence/stage-c/w73-qq-agent-dogfood-closure.md)）冻结；**唯一仍处于 `frozen: false` 的是第 7 通道 `nlos.sabi.PrincipalHandshake`（`SABI_PRINCIPAL_HANDSHAKE_V1`）**。同时，`sdk/` 虽已具备 TypeScript、Python、Go、C# 探针（[B-SCHEMA-002](../../evidence/stage-b/b-schema-002-cross-language-generation.md)），但系统尚未补齐 `PrincipalHandshake` 的五语言握手黄金向量以达成 9/9 通道全量入冻，尚未建立跨历史版本快照锁定的黄金线协议语料库（Golden Wire Corpus），也未实现机器可读的 `ACTIVE -> DEPRECATED -> SUNSET` 弃用生命周期与运行时协商告警（`COMPAT-DEPRECATE-001`），因此尚未完成从 `ADR-0014` `v1-beta` 向 `SABI v1.0` 正式长期冻结的晋升（`RISK-B-05`）。
2. **缺乏核外版本化兼容垫片（Out-of-Core Compatibility Shim）**：当前 `nlos-driver-mock` 仅在单 crate 内保留了 `minicbor` legacy 编解码模块（[C-PROVIDER-001](../../evidence/stage-c/c-provider-001-real-process-provider-codec.md)）；系统尚无通用的核外版本化 SABI Adapter Cell 与 Driver Protocol Shim 动态挂载机制，一旦主系统推进至 `N` 版本而第三方包停留在 `N-1` / `N-2` 契约，若无核外隔离垫片，兼容分支将不可避免地倒灌污染核心权威模块。
3. **本地包管理尚未上升为多级信任商店与供应链治理体系**：`crates/nlos-package` 与 `examples/sample-app` 已跑通本地签名包构建、校验、安装、运行、更新与卸载（[C-APP-PAYLOAD-001](../../evidence/stage-c/c-app-payload-001-sample-cli-execution.md)），`nlos-capability::custody` 已具备 KEK 托管签名库（[C-CUSTODY-001](../../evidence/stage-c/c-custody-001-key-custody-gateway-reconciliation.md)）；但仍缺少第三方 Publisher 证书链、单调吊销纪元（`RevocationEpoch`）、`ApplicationId` 跨发布者防抢注账本（`PKG-APPID-001`）、基于 OCI 内容寻址层的商店元数据注册表（`PKG-OCI-001`）以及自动化上架认证流水线。
4. **缺少跨主系统升级的零重编译不破坏实证门禁（`[ROAD-E-001]`）**：阶段 B/C/D 的测试均在单一 Git commit 快照下同源编译宿主与样板应用，尚未在自动化门禁中证明「针对 `N-1` 版本编译并冻结签名的第三方 Application 与用户态 Driver 包，在宿主主系统原地升级至 `N` 后无需重新编译或重签即可完整恢复与运行」。

### 1.2 与前序权威 ADR 的精确演进关系（零历史改写）

本 ADR 在既有架构契约预留的扩展点上演进，不推翻、不追溯篡改任何前序已验证结论：

| 前序 ADR | 既有核心裁定 | 本 ADR（`ADR-0027`）在阶段 E 的承接与扩展 |
|---|---|---|
| [ADR-0001](./0001-stage-b-core-language-and-runtime.md) | Rust 核心工作区 + Tauri 2 桌面宿主 + Wasmtime 不可信执行基线 | 保持宿主技术栈不变；第三方生态包通过语言无关的 SABI 线协议与 WASI/进程接口接入，不强制第三方使用 Rust |
| [ADR-0013](./0013-cross-authority-verify-then-commit-contract.md) | 跨权威 `verify-then-commit + 有界收敛` 与持久化回执语义 | 商店包安装、跨版本数据容器迁移与吊销隔离严格复用 `verify-then-commit` 与幂等恢复契约，禁止静默半安装态 |
| [ADR-0014](./0014-schema-channel-freeze-v1-beta.md) | 6 个核心 SABI 通道冻结为 `v1-beta`，仅允许 additive 扩列；独立 ADR 负责 `v1.0` 晋升 | 定义从 `v1-beta` 迈向 `SABI v1.0` 长期稳定承诺的制度化门禁、跨代黄金语料快照锁与 `COMPAT-DEPRECATE-001` 弃用状态机 |
| [ADR-0015](./0015-dual-layer-untrusted-execution-isolation.md) | 互不信任代码采用「独立宿主 Process + 进程内 Wasmtime」双层隔离；未完全中介路径强制按 `TCB-DOWNGRADE-001` 降档 | 将双层隔离固化为商店 `COMMUNITY_SANDBOXED` 与未授信第三方组件的强制执行底座，并在三平台补齐逃逸负向测试以闭合 `U-9` |
| [ADR-0022](./0022-operator-root-capability-issuance.md) | `nlos-system-control` 启动期经受控 operator 密钥显式签发根能力，缺省 fail-closed | 将单机 operator 根密钥模型分层扩展为离线平台根 CA -> 商店中继签发器 -> 发布者证书链，保留本地 operator 覆盖否决权 |
| [ADR-0024](./0024-driver-provider-payload-codec-freeze.md) | 冻结第 8 通道 `nlos.sabi.DriverProvider` v1.0，保留 `minicbor` legacy 垫片并校验 `ProviderIsolationAttestationWire` | 将单点 `minicbor` 垫片提炼为通用核外 `nlos-compat-shim` 架构，严格分离可信 `EnforcementShim` 与不可信协议转换/供应商适配器 |
| [ADR-0025](./0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md) | 五平面 `Z0–Z4` 工业工作站合成器、`L0–L3` 渐进式披露、`[UI-PARITY-001]` 与 `Z4` UAC/Polkit 安全确认屏 | 商店浏览与第三方包安装/升级确认统一接入 `Z2/Z3` 标准表面与 `Z4` 实心安全确认屏（展示发布者信任等级、权限目的与不可逆迁移警告） |
| [ADR-0026](./0026-cross-physical-host-transport-and-consensus-expansion.md) | 跨物理主机 mTLS 五轴认证传输、保守时钟偏斜租约与嵌入式 Raft 控制面多数派日志 | 多节点集群中的发布者吊销纪元（`RevocationEpoch`）与包信任策略通过控制面元数据同步，离线节点遵循保守新鲜度窗口自治 |

---

## 2. 阶段 E 四大架构支柱（Four Architectural Pillars）

### 2.1 支柱一：SABI v1.0 长期稳定契约与双层兼容演进制度（`COMPAT-*` / `SABI-*`）

依据 [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §9，阶段 E 严格区分三类契约边界，拒绝“把内核内部实现细节与第三方生态契约混为一谈”：

1. **KABI 与 SABI 双层解耦演进模型**：
   - **KABI 层（微内核、可信系统服务与受控 `EnforcementShim` 边界）**：严格守住 27-entry 候选基础面与 3-entry `RESOURCE_ESCROW` 扩展边界，严禁为特定模型、工具或商店业务新增 provider-specific syscall（`KABI-SVC-001`），强制携带 128-bit `IdempotencyKey`（`KABI-IDEM-001`）、`slot + generation` 防复活句柄（`KABI-HANDLE-001`）与精确类型化错误（`KABI-ERR-001`、`KABI-UNCERTAIN-001`）。KABI 允许随主系统内核版本在可信系统组件之间同步演进，不向第三方 Application 承诺裸 syscall 编号级二进制固化。
   - **SABI v1.0 层（第三方 Application 与用户态 Driver 长期稳定编程表面）**：覆盖 `SurfaceProfile=APPLICATION_PLATFORM` 与 `SurfaceProfile=DESKTOP` 规定的核心服务族（`SABI-CONFORM-001`）及 `nlos.sabi.DriverProvider` 驱动线协议。所有第三方包必须通过 `ServiceDirectory.resolve` / `negotiate` 动态发现服务并协商版本与 feature 集（`SABI-DISC-001`），完整继承 capability、异步 `Operation`、`deadline`、`cancel`、`idempotency`、部分失败、不可变 `Receipt` 与授权摘要绑定语义（`SABI-COMMON-001`、`SABI-RECEIPT-001`、`SABI-AUTH-001`、`SABI-TRANSPORT-001`）。

2. **主系统 `N` 对历史包 `N-1` / `N-2` 的前向与后向兼容矩阵（`COMPAT-VER-001`、`COMPAT-VER-002`）**：
   每个 SABI envelope、Driver 协商帧与 `llmos.package` Manifest 必须显式包含 `(schema_name, major, minor, critical_extensions, noncritical_extensions)`。兼容裁决规则如下表所示：

   | 宿主主系统版本 | 第三方 Application / Driver 包目标纪元 | 线协议扩展特征 | 宿主系统确定性裁决行为 |
   |---|---|---|---|
   | 主系统 `N`（`SABI v1.n`） | 当前纪元 `N`（`v1.n`） | 已知 `major=1, minor=n` | 原生直通执行，零垫片开销 |
   | 主系统 `N`（`SABI v1.n`） | 历史已认证纪元 `N-1` 或 `N-2` | 同一 `major=1`，旧 `minor` 或已登记弃用字段 | **保证零重编译兼容运行**：additive 兼容调用直通；涉及已演进 wire 形态者由宿主按包声明的 `target_sabi_epoch` 自动挂载核外 `nlos-compat-shim` 转换 |
   | 主系统 `N`（`SABI v1.n`） | 更高次版本包 `N+k`（`major=1, minor > n`） | 仅携带未知 `noncritical_extensions` | 按 `COMPAT-VER-002` 允许安全忽略未知非关键扩展，但必须在转发、持久化与回执摘要路径保留原始未知字段的 round-trip 字节语义 |
   | 主系统 `N`（`SABI v1.n`） | 更高次版本包或异构包 | 携带未知 `major != 1` 或未知 `critical_extensions` / 未知关键 flag | **强制 Fail-Closed 拒绝**：立即返回 `E_ABI_VERSION` 或类型化 `UnsupportedCriticalExtension` 错误，严禁静默截断字段后继续产生外部副作用 |
   | 主系统 `N`（`SABI v1.n`） | 超出滚动承诺窗口的已日落纪元（`< N-2` 且状态为 `SUNSET`） | 依赖已移除 `SUNSET` 接口 | 安装/启动门明确拒绝并给出 `nlos-migrate` 升级指引，除非管理员显式安装独立的归档兼容包 |

3. **三阶段弃用生命周期制度（`ACTIVE -> DEPRECATED -> SUNSET`，`COMPAT-DEPRECATE-001`）**：
   任何已进入 `SABI v1.0` 稳定面的字段、错误码、服务方法或 Driver 扩展，严禁直接物理删除或原号篡改语义，必须经历以下受控状态跃迁：
   - **`ACTIVE`**：标准受支持接口，纳入五语言 SDK 默认生成面与上架一致性套件；
   - **`DEPRECATED`**：在机器可读弃用清单（`schema/nlos/sabi/v1/deprecation-manifest.yaml`）中登记 `deprecated_in_epoch`、`earliest_sunset_epoch`、`replacement_symbol`、`migration_rule_id` 与 conformance 向量；**最短预告窗口不少于两个主系统发布纪元（`earliest_sunset_epoch >= deprecated_in_epoch + 2`）**；处于 `DEPRECATED` 窗口内的接口保持 100% 功能与字节兼容，但在 `ServiceDirectory.negotiate`、`DRV-NEGO-001` 及控制面遥测中发出结构化 `DeprecationWarningReceipt`（不阻断业务执行）；
   - **`SUNSET`**：跨越最短双周期预告且提供自动化 `nlos-migrate` 规则后，主系统核心注册表将该字段号/方法名永久标为 `RESERVED`（严禁复用历史编号给新语义），新上架包在 `nlos-certify` 阶段拒绝引用 `SUNSET` 符号。

4. **跨版本黄金线协议语料库（Golden Wire Corpus）锁定**：
   在 `schema/golden/epochs/` 下按发布纪元建立只读锁定的二进制黄金向量目录（如 `v1.0/`、`v1.1/`），一旦某个纪元封板，CI 机械门严禁任何后续提交修改历史纪元目录下的 `.hex` / `.bin` 向量文件；Rust、TypeScript、Python、Go、C# 五套编解码器必须同时通过当前纪元与全部未日落历史纪元向量的双向解码与确定性重编码校验。

5. **剩余两项协议契约冻结收口路径（`PrincipalHandshake` 第 7 通道入冻 + `SABI v1-beta -> v1.0` 正式长期冻结 `RISK-B-05`）**：
   - **第一步（`W77` `W77-E1A-SABI-REGISTRY` + `W77-E1B-GOLDEN-SDKS`：达成 9/9 SABI 通道全量入冻）**：针对 `crates/nlos-schema` `REGISTRY` 中唯一仍为 `frozen: false` 的第 7 通道 `nlos.sabi.PrincipalHandshake`（`SABI_PRINCIPAL_HANDSHAKE_V1`），完成 peer attestation 扩展槽审查与 Rust/TypeScript/Python/Go/C# 五语言握手黄金向量钉死，将其翻转为 `frozen: true`，与既有 8 个已入冻通道（`Envelope`、`ServiceDirectory`、`OperationControl`、`SystemControl`、`TakeoverControl`、`WaitControl`、`DriverProvider`、`ChannelControl`）汇合达成 **9/9 SABI 通道全量入冻**；
   - **第二步（`W77 -> W78 -> W81` 决策点 `E2`：晋升 `SABI v1.0` 正式长期冻结并闭合 `RISK-B-05`）**：通道级 9/9 入冻后仍维持 `ADR-0014` `v1-beta` / `v1.0-rc` 审慎护栏，依次经 `W77` 五语言跨纪元黄金语料库 + 弃用清单、`W78` 核外无特权 `nlos-compat-shim` 转换及 `W81` `[ROAD-E-001]` `N-1 -> N` 零重编译跨主系统升级门禁实证并获维护者显式签字后，正式将 SABI 全表面自 `ADR-0014` `v1-beta` 晋升为 `SABI v1.0` 长期冻结承诺并闭合 `RISK-B-05`。

---

### 2.2 支柱二：核外兼容垫片（Compatibility Shim）与声明式迁移工具链（`DRV-SHIM-001` / SDK Migration）

1. **核心控制面零历史分支污染铁律（Out-of-Core Shim Isolation）**：
   - **硬禁令**：严禁在 `crates/nlos-cell`、`crates/nlos-task`、`crates/nlos-capability`、`crates/nlos-resource`、`crates/nlos-consensus` 或 `crates/nlos-system-control` 内部散落针对历史第三方包版本的 `if pkg_epoch == ...` 协议兼容分支。核心权威组件始终只维护当前 canonical schema 的清晰状态机与最小 TCB 表面。
   - **动态按纪元挂载核外垫片（`crates/nlos-compat-shim`）**：第三方包在 `llmos.package` Manifest 的 `package.compatibility` 段显式声明 `target_sabi_epoch` 与 `driver_abi_epoch`。当 `ApplicationAuthority` 启动历史包或 `Device/Driver` 子系统执行 `DRV-NEGO-001` 协商时，若检测到 `target_sabi_epoch` 属于受支持的 `N-1` / `N-2` 历史纪元且存在协议形态差异，宿主在独立用户态受限隔离域中动态挂载对应的 **SABI Adapter Cell** 或 **Driver Protocol Shim**。

2. **严格遵守 `[DRV-SHIM-001]` 的可信执行垫片与不可信适配器分离**：
   核外兼容架构严格实行双边界切分：
   ```text
   [Core Authorities (Current SABI v1.N)]
            ^
            |  Canonical Frozen Wire + ADR-0011 Auth
            v
   [Trusted EnforcementShim (TCB Edge: Credential / Capability / Reservation / EffectFence)]
            ^
            |  Mediated Structured Frame (Zero Raw Secret)
            v
   [Versioned Protocol Compat Shim (nlos-compat-shim: Epoch N-1/N-2 <-> N Schema Translation)]
            ^
            |  Historical SABI / DriverProvider Wire Epoch
            v
   [Untrusted / Third-Party ProviderAdapter or Legacy Application Process]
   ```
   - **可信 `EnforcementShim`（驻留受控 TCB 边缘）**：独占托管外部 provider 原始凭据（`DRV-CRED-001`），强制校验 `Capability`、`Operation`-bound `Reservation` 一次性激活令牌（`DRV-ACTIVE-001`）、`deadline`、`ActivityContext`、`Process incarnation` 与分布式 `fencing_token`（`DRV-BOUND-001`），并在驱动崩溃后将所有未终结 `Operation` 强制置为 `UNCERTAIN` 直至对账完成（`DRV-LIFE-001`）；
   - **版本转换垫片（`nlos-compat-shim`）**：仅执行纯确定性、无特权的线协议字段映射、默认值补齐与错误码降级翻译；垫片进程**不持有任何根密钥或外部凭据，不能铸造 Capability、不能绕过 Reservation 扣费、也不能伪造已签名的 `UsageReceipt` 或 `ProviderIsolationAttestationWire`**。

   - **先行开箱试用工作包 `E0`（`W77-PRE`）对微内核边界的严格遵守（已于 `2026-10-30` 落地 `H4` 验证）**：`W77-PRE` 交付的一键引导器（`crates/nlos-up`）、用户态 HTTP/1.1 模型驱动（`crates/nlos-driver-llm-http`，4 层自动发现本机 `~/glm-keypool` `/api/coding/paas/v4` 及 OpenAI-compatible / 本地 Ollama 端点）、参考生产力样板包（`examples/starter-assistant-app`）与桌面端 `keypool_bridge` 已于 `2026-10-30` 完整落在此用户态 `LlmEnforcementShim` 边界之上并通过 `H4` 验证（[E-TRIAL-001](../../evidence/stage-e/e-trial-001-llm-http-driver.md)、[E-TRIAL-002](../../evidence/stage-e/e-trial-002-quickstart-and-starter-app.md)）——由 `LlmEnforcementShim` 独占托管脱敏凭据、联动 `nlos-resource` v8 五维 `LlmTokenBudget` 守恒回执并诚实标注 `PROVIDER_ISOLATION_UNVERIFIED`，**零厂商专有 HTTP 模型协议进入微内核、零新增 provider-specific KABI**（`KABI-SVC-001`、`DRV-CRED-001`、`DRV-SHIM-001`）；同时核心兼容与商店治理主线 `E1`..`E7` 仍保持 `DESIGN`。

3. **声明式契约与数据迁移辅助工具链（`crates/nlos-migrate`）**：
   面向生态开发者提供离线 CLI 与 CI 检查器 `nlos-migrate`：
   - 扫描第三方仓库的 `llmos.package` Manifest、protobuf/SDK 依赖声明与 `EffectDescriptor` 定义，对照 `deprecation-manifest.yaml` 输出精确到字段与方法的弃用告警、兼容窗口剩余周期及结构化升级补丁建议；
   - 校验包更新时的数据容器迁移策略声明（`PKG-UPDATE-001`）：强制区分 `BACKWARD_COMPATIBLE_WINDOW`（新旧二进制可共享读写窗口）、`SNAPSHOT_ASSISTED_ROLLBACK`（升级前自动创建内容寻址容器快照，健康检查失败时原子回滚二进制与数据快照）与 `IRREVERSIBLE`（强制触发预备份与桌面 `Z4` Trusted UI 显式风险确认，物理禁止将不可逆迁移伪装为可静默自动回滚）。

---

### 2.3 支柱三：Application / Driver Store 发布者治理、信任根轮换与认证流水线（`PKG-*`）

依据 [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §23，OCI 镜像/制品层仅承担内容寻址传输与存储，绝不等同于应用商店（`PKG-OCI-001`）。阶段 E 建立完整的商店治理平面：

1. **三级发布者信任域与隔离策略映射**：

   | 信任等级 | 签名锚点与准入条件 | 允许请求的权限上界 | 强制运行沙箱档位（`ADR-0015`） | 桌面 `Z4` 授权体验（`ADR-0025`） |
   |---|---|---|---|---|
   | `PLATFORM_CORE` | 离线平台根 CA 签发的一方系统组件证书 + 可复现构建摘要 | 系统服务与受控 `EnforcementShim` 权限 | 受控宿主进程 + 最小权限配置 | 系统内置受信组件标识，更新走控制面原子事务 |
   | `CERTIFIED_PUBLISHER` | 经商店身份核验并完成 `nlos-certify` 全套四门认证的第三方发布者叶证书 | 按 Manifest 声明并经最小权限审计的 scoped SABI / Device 权限（零原始凭据） | 默认双层沙箱（独立宿主 Process + Wasmtime）；声明受控原生适配器者须通过平台隔离门审查 | 标准 `Z4` 四行键值规格表展示真实 Publisher、权限目的、预算上界与可逆等级 |
   | `COMMUNITY_SANDBOXED` | 开发者自签名或未经中央商店背书的社区分发包（显式开启外部源策略时可用） | 严格禁止系统控制面、`SYSTEM_CRITICAL` 资源与高危外发通道 | **强制封死在「独立宿主 Process + Wasmtime WASI 白名单」双层沙箱**；未完全中介能力一律按 `TCB-DOWNGRADE-001` 降档 | `Z4` 醒目展示 `UNVERIFIED_PUBLISHER` 警示徽标与降档说明，须用户二次显式确认 |

2. **Ed25519 分层证书链、防抢注身份账本与单调吊销纪元（`PKG-IMM-001`、`PKG-APPID-001`）**：
   - **三层密码学信任链**：`Offline Platform Root Key`（离线冷存储） -> `Store Intermediate Signing Key & Timestamp/Snapshot Key`（由 `SigningKeyCustodyVault` KEK 保护，支持定期纪元轮换） -> `Publisher Leaf Certificate`（绑定唯一 `PublisherId`、公钥代次与有效窗口）；
   - **`ApplicationId` 唯一归属与三元身份严格分离（`PKG-APPID-001`、`PKG-SEP-001`）**：商店注册账本强制将 `ApplicationId` 持久绑定至首次合法注册的 `PublisherId` 公钥策略集；任何其他发布者提交声明相同 `ApplicationId` 的包一律以 `ApplicationIdHijackAttempt` 拒绝；安装侧严格保持 `PackageId`（不可变内容分发包）、`ApplicationId`（稳定逻辑应用身份）与 `InstallationId`（绑定特定 Workspace/Device 作用域的实例及独立派生 `Application Principal`）三者物理不可互换；
   - **防回滚元数据与单调吊销纪元（CRL / `RevocationEpoch`）**：借鉴 TUF 防冻结/防回滚思想，商店索引与吊销列表携带严格单调递增的 `revocation_epoch` 与签发有效期。当发现恶意包、密钥泄露或严重安全缺陷时，商店发布签名 `RecallAdvisory`（分 `WARN_NEW_INSTALL`、`BLOCK_LAUNCH_AND_REVOKE_CAP`、`QUARANTINE_INSTALLATION` 三级处置）；客户端与集群控制面拒绝任何 `revocation_epoch` 倒退的旧索引重放攻击。

3. **OCI 内容寻址分发契约与商店治理索引（`PKG-OCI-001`、`PKG-MULTI-001`）**：
   - 底层制品采用不可变内容寻址层（SHA-256 digest 锁定），单包原生支持承载多组件复合应用（`executableComponents` + `drivers` + `uiSurfaceComponents` + `backgroundServiceComponents` + 只读 `seedTemplates`，`PKG-MULTI-001`、`PKG-SEED-001`），严禁将可变用户记忆或明文密钥烘焙入包（`PKG-IMM-001`）；
   - `crates/nlos-store-registry` 在 OCI 分发层之上维护结构化商店治理目录：发布者身份档案、渠道轨道（`stable` / `candidate` / `recalled`）、权限透明度摘要（请求的 `Capability` 与对应 `purpose` 声明）、许可与配额合同模板（`entitlement`）以及签名認證回执（`StoreCertificationReceipt`）。

4. **自动化上架认证流水线（`crates/nlos-certify` 四道硬门）**：
   任何第三方 Application 或 Driver 包晋升为 `CERTIFIED_PUBLISHER` 渠道前，必须全部通过可离线复现的 `nlos-certify` 自动化检验并生成不可变签名回执：
   - **门 1（协议与效应一致性门）**：验证 `SABI-CONFORM-001`、`DRV-NEGO-001`、`EffectDescriptor` 四分类（`READ_ONLY` / `REVERSIBLE` / `COMPENSATABLE` / `IRREVERSIBLE`）完整性、128-bit 幂等键透传以及收到 `E_UNCERTAIN` 时严禁换新 key 盲目重做的规范性行为（`KABI-UNCERTAIN-001`）；
   - **门 2（最小权限与供应链纯净度门）**：静态与动态扫描包内资产，拒绝未声明网络外连（`TCB-EGRESS-001`）、拒绝请求宿主裸文件系统路径（`DUI-CHOOSER-001`）、拒绝包内夹带非只读可变状态或硬编码密钥（`PKG-IMM-001`）、拒绝静默篡改默认文件类型关联（`DUI-ASSOC-001`）；
   - **门 3（事务化更新、健康检查与原子回滚门，`PKG-UPDATE-001`）**：在沙箱台架真实演练旧版向新版升级、注入启动后 `health` 检查失败（`DRV-HEALTH-001`）与崩溃故障，实证二进制与受控容器快照 100% 原子回滚至旧可用代际且零账务泄漏；
   - **门 4（零残留卸载审计门，`PKG-UNINSTALL-001`）**：演练完整卸载流程，验证后台服务彻底终止、已授予 `Capability` 句柄立即吊销（`CAP-REVOKE-001`）、私有缓存清理、共享 `Artifact` 引用计数安全保持，且默认保留 `UserData` 容器或仅在经可信确认后销毁。

---

### 2.4 支柱四：第三方 Driver / Application 生态市场与主系统跨版本不破坏门禁（`[ROAD-E-001]`）

为使 `[ROAD-E-001]`（「系统能力增长主要来自第三方生态，升级无需生态整体重写」）成为机械可证伪的工程门禁而非口号，阶段 E 建立以下双闭环：

1. **解耦构建的第三方参考生态样板矩阵**：
   在主内核 crate 之外建立模拟外部独立发布者工作流的参考生态包集合（包含至少 2 个多组件第三方 Application 与 2 个用户态第三方 Driver 适配器，覆盖 MCP 工具桥、OpenAI-compatible 推理网关或本地受限工具适配器 `DRV-PROTO-001`）。样板包仅允许依赖公开导出的 SABI SDK、标准 `llmos.package` 清单与用户态 Driver 协议，**物理禁止直接 `path` 依赖 `nlos-cell`、`nlos-task` 等内核私有 crate**。

2. **`[ROAD-E-001]` 跨主系统版本（`N-1 -> N`）零重编译升级回归总门禁**：
   构建确定性跨版本升级验收台架（目标证据等级：`H7` 安全与兼容 conformance）：
   - **步骤 A（`N-1` 产物冻结）**：针对 `SABI v1.0`（模拟 `N-1` 纪元）编译第三方参考 Application 与用户态 Driver 包，完成发布者签名与 `nlos-certify` 认证，产出字节锁定的 `.nlos-pkg` 归档制品与 SHA-256 清单；
   - **步骤 B（`N-1` 宿主建态）**：在 `N-1` 宿主配置下完成包验签安装、`Z4` 授权、应用启动、Driver 绑定、持久 `Artifact` 与 `UserData` 写入、后台 `Task` 推进及优雅检查点落盘；
   - **步骤 C（主系统 `N-1 -> N` 原地升级）**：将宿主控制面、`nlos-cell`、`nlos-system-control`、桌面合成壳及 SABI 注册表升级至演进后的 `N` 版本（包含模拟的 KABI 内部重构、新增 additive SABI 字段及至少一项进入 `DEPRECATED` 窗口需经 `nlos-compat-shim` 桥接的历史接口），**全程严禁对步骤 A 冻结的 `.nlos-pkg` 第三方包做任何重新编译、重新打包或重新签名**；
   - **步骤 D（`N` 宿主全链路无损验证）**：在升级后的 `N` 主系统上验证既有 `InstallationId` 与 `UserData` 100% 自动恢复、历史包通过 `ServiceDirectory` 与 `nlos-compat-shim` 正常完成 SABI IPC 调用与 `Receipt` 核验、用户态 Driver 通过热插拔/重绑与崩溃后 `UNCERTAIN -> reconcile` 恢复、收到预期的 `DeprecationWarningReceipt` 遥测告警、并最终干净通过包更新/回滚与零残留卸载。

---

## 3. 备选方案对比（Alternatives Considered）

| 候选架构方案 | 核心主张 | 评估结论与取舍依据 |
|---|---|---|
| **方案 A（采纳）：双层契约解耦 + 核外版本化垫片（`nlos-compat-shim`）+ OCI 之上独立密码学商店治理（`nlos-store-registry` / `nlos-certify`）+ `N-1 -> N` 零重编译门禁** | KABI 保持最小受控内部边界，SABI v1.0 承担长期第三方兼容承诺；历史协议差异由核外无特权垫片隔离转换；OCI 仅作内容寻址层，商店治理独立承担发布者证书链、吊销纪元与四门认证 | **采纳**：100% 契合 `v0.5` §9 / §15 / §23 / §28.5 全部规范性条款，兼顾内核持续演进能力、最小 TCB 安全性与第三方生态长期投资保护 |
| **方案 B（否决）：直接冻结底层 KABI syscall 编号作为第三方二进制 ABI，并在核心权威 crate 内部塞入历史版本 `if/else` 分支** | 省去独立兼容垫片层，让核心控制面直接兼容所有历史请求格式 | **否决**：一方面违反 `v0.5 §9.2`（KABI 为微内核与可信服务边界，第三方应用编程表面是 SABI），过早锁死微内核演进空间；另一方面历史兼容解析逻辑驻留核心权威会持续扩大 TCB 攻击面并诱发跨代状态机回归漏洞 |
| **方案 C（否决）：将普通 OCI 镜像仓库或单文件 Prompt 模板目录直接当作 Application Store** | 仅复用 OCI tag 推送与拉取，不建设独立发布者身份账本、单调吊销纪元或自动化上架认证门 | **否决**：直接违反 `[PKG-OCI-001]`（「OCI 不得等同于应用商店」）、`[PKG-MULTI-001]` 与 `[PKG-APPID-001]`；无法防御 `latest` 标签投毒、元数据冻结回滚攻击、跨发布者 `ApplicationId` 抢注与卸载权限残留 |
| **方案 D（否决）：维持源码级同仓锁步升级（每次主系统升级要求第三方源码重编译适配）** | 仅保证源码级近似兼容或提供手工迁移文档，不维持线协议二进制向后兼容 | **否决**：直接违背阶段 E 唯一退出门 `[ROAD-E-001]`（「升级无需生态整体重写」），在生态规模扩大后会导致升级碎片化与第三方生态停滞 |

---

## 4. 显式非目标与安全诚实边界（Explicit Non-Goals & Security Boundaries）

严格遵循 [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §29 与 `AGENTS.md` 第 7 条诚实分级铁律，本 ADR 明确划定以下四条红线：

1. **不在操作系统内核或 TCB 内构建商业计费收银台、法币清分税务系统或广告竞价后台（`v0.5 §29.1`、`§29.2`）**：商店治理（`nlos-store-registry`）只负责密码学发布者身份、权限透明度、不可变许可证摘要（`entitlement`）、自动化质量/安全认证回执与恶意包撤回机制；商业定价策略、跨组织法币财务清算与市场商业运营规则严禁硬编码进内核。
2. **绝不引入内核态（Ring-0）第三方驱动，也不替代宿主 OS 硬件内核（`v0.5 §29.1`、`DRV-SHIM-001`）**：NLOS 的全部第三方 Driver 均为运行在用户态隔离域中的 `ProviderAdapter`（如模型推理网关、MCP/A2A 桥接器、受控工具/设备代理），一律经可信 `EnforcementShim` 中介调用，不存在第三方代码进入宿主硬件内核态的路径。
3. **在未闭合 `U-9` 前绝不宣称任意不可信原生机器码可免沙箱安全裸跑（`ADR-0015`、`TCB-BYPASS-001`、`TCB-DOWNGRADE-001`）**：未经可信审计的社区/第三方不可信负载必须运行在「独立宿主 Process + Wasmtime 能力白名单」双层沙箱内；任何因特定外部工具桥接需使用宿主原生子进程的场景，若所在平台尚未具备经实测验证的 OS 级强沙箱与内存硬限额，必须按 `[TCB-DOWNGRADE-001]` 在回执与桌面 `Z4` 界面显式降档标注，严禁静默冒充 `STRICT` 隔离。
4. **不在本立项波次冒充 `SABI v1.0` 已完成永久冻结或商店已投产（`RISK-B-05`）**：本 ADR 在 `W76` 处于 `CANDIDATE` 状态、证据等级 `DESIGN`；唯有在 `W77–W81` 跑通五语言黄金语料、核外兼容垫片、供应链 `H7` 对抗矩阵与 `[ROAD-E-001]` 跨主系统升级门禁并经维护者显式批准后，方可宣布阶段 E 退出与 `SABI v1.0` 长期兼容承诺生效。

---

## 5. 后果、风险缓解与退出策略（Consequences, Risk Mitigation & Exit Strategy）

### 5.1 正面收益与工程代价

- **正面收益**：
  - 将微内核内部演进自由度（KABI）与第三方生态长期稳定性（SABI v1.0 + `llmos.package` + `DriverProvider`）在架构上彻底解耦；
  - 通过核外无特权 `nlos-compat-shim` 守住核心权威 crate 的代码整洁度与最小 TCB 边界；
  - 通过分层证书链、防抢注 `ApplicationId` 账本、单调 `RevocationEpoch` 与 `nlos-certify` 四门流水线，从制度上收敛 `RISK-B-09`（第三方供应链风险）、`U-7`/`U-8`（生态 Typed Selector 锁定）与 `U-9`（不可信负载隔离）。
- **工程代价（显式接受）**：
  - 需额外维护 `schema/golden/epochs/` 历史快照语料与 `nlos-compat-shim` 协议转换测试矩阵；
  - 经过 `nlos-compat-shim` 桥接的 `N-1` / `N-2` 历史包相比原生 `N` 包会增加一次本地 IPC/转换跳数（原生 `N` 包保持零额外跳数开销）。

### 5.2 核心风险与缓解映射

| 风险关联 | 失效模式描述 | 架构级缓解机制 |
|---|---|---|
| `RISK-B-05` / `RISK-E-01` | SABI v1.0 某字段语义考虑不周即冻结，或后续 additive 修改破坏隐式边界不变量 | 五语言跨纪元 Golden Wire Corpus 字节锁 + `COMPAT-VER-002` critical/non-critical 显式分流 + `E2` 决策门在 `W81` 全链路验证后再最终锁定 |
| `RISK-E-02` | 兼容垫片（`nlos-compat-shim`）出现逻辑漏洞，被恶意旧版包利用以绕过权限或预算 | 严格执行 `[DRV-SHIM-001]`：转换垫片置于可信 `EnforcementShim` 之外的无特权沙箱侧，所有 Capability、Reservation 激活令牌与凭据代填仍在核心/可信侧独立验真 |
| `RISK-B-09` / `RISK-E-03` | 商店索引遭受中间人冻结/回滚攻击、发布者私钥泄露或同名应用抢注投毒 | 离线根 CA + KEK 托管中继密钥 + 单调递增 `revocation_epoch` 与过期窗口校验 + `(PublisherId, ApplicationId)` 首次注册密码学钉死 + `RecallAdvisory` 三级联动吊销 |
| `U-9` / `RISK-E-04` | 第三方市场组件利用 WASI 垫片差异或宿主环境变量继承逃逸沙箱 | `nlos-wasm-sandbox` 锁定 Wasmtime 版本并关闭默认宿主环境继承，执行 Windows/macOS/Linux 三平台 `path_open`/网络外连/内存炸弹/CPU fuel 超限负向对抗回归 |
| `U-7` / `U-8` / `RISK-E-05` | 第三方包依赖声明使用浮动 `latest` 标签导致升级后静默拉取未授权新权限组件 | 在 `nlos-publisher-trust` 与 `nlos-plan` 落地跨生态 Typed Selector -> 不可变 `(PackageDigest, GenerationHandle)` 绑定，未重新经 `Z4` 授权严禁静默扩权升级 |

### 5.3 退出与跨代演进策略

1. 若未来出现确有必要突破 `SABI major = 1` 线协议结构的代际演进需求（`SABI v2.0`），严禁原地修改 `nlos.sabi.v1` 命名空间，必须并列开辟 `schema/nlos/sabi/v2/` 新通道，在 `ServiceDirectory` 中支持 `v1` 与 `v2` 双协议栈并存，并由核外 `nlos-compat-shim` 提供至少两个大周期的 `v1 -> v2` 桥接窗口。
2. 若某一具体历史纪元垫片在跨越 `SUNSET` 周期且生态遥测确认零活跃安装后需要退役，须通过独立变更回执归档该垫片模块，同时保留其拒绝旧包的清晰错误码与 `nlos-migrate` 离线升级工具。

---

## 6. 验证计划与复审触发器（Validation Plan & Review Triggers）

### 6.1 证据晋升阶梯规划

| 阶段波次 | 目标工作包 | 验证重点与计划产出证据路径 | 目标保障等级 |
|---|---|---|---|
| `W76` / `W76.1`（当前） | 立项、冻结收口排期与总体设计 | 本文档（`ADR-0027`）与 [stage-e-progress.md](../stage-e-progress.md) | `DESIGN` |
| `W77-PRE`（先行波，已于 `2026-10-30` `DONE`） | `E0` | `nlos-up` 一键初始化与跨平台开发夹具、`nlos-driver-llm-http` 用户态本地 KeyPool/HTTP 模型驱动（`LlmEnforcementShim` 脱敏托管 + `nlos-resource` v8 五维 Token 守恒回执）、桌面 `keypool_bridge`/Omnibar 辅助面板、`starter-assistant-app` 与本地试用打包脚本验证（[E-TRIAL-001](../../evidence/stage-e/e-trial-001-llm-http-driver.md)、[E-TRIAL-002](../../evidence/stage-e/e-trial-002-quickstart-and-starter-app.md)） | **已达成 `H4`** |
| `W77` | `E1` | 第 7 通道 `nlos.sabi.PrincipalHandshake` 翻转为 `frozen: true`（达成 9/9 SABI 通道全量入冻）、五语言跨纪元 Golden Wire Corpus、`COMPAT-VER-002` 未知关键扩展拒绝与 `COMPAT-DEPRECATE-001` 弃用遥测测试（`docs/evidence/stage-e/e-sabi-*.md`） | `H2 -> H4` |

| `W78` | `E2`、`E3` | 核外 `nlos-compat-shim` 协议转换与零特权旁路验证、`nlos-migrate` 静态检查与三种迁移回滚演练（`docs/evidence/stage-e/e-shim-*.md`、`e-migrate-*.md`，触发本 ADR 晋升 `ACCEPTED` 评审） | `H4` |
| `W79` | `E4`、`E5` | 发布者证书链/单调 `RevocationEpoch`/`ApplicationId` 防抢注、Typed Selector 锁定、OCI 商店注册表与 `nlos-certify` 四门自动化认证测试（`docs/evidence/stage-e/e-trust-*.md`、`e-store-*.md`、`e-certify-*.md`） | `H4 -> H7` |
| `W80` | `E6` | 三平台 `nlos-wasm-sandbox` 强沙箱逃逸拦截（闭合 `U-9`）、解耦第三方参考 App/Driver 样板包、桌面商店权限透明视图与供应链 `H7` 对抗回归（`docs/evidence/stage-e/e-sandbox-*.md`、`e-market-*.md`、`e-security-*.md`） | `H5 -> H7` |
| `W81` | `E7` | `[ROAD-E-001]` 主系统 `N-1 -> N` 零重编译跨版本升级全链路门禁、基于 `ledger-schema v1.3`（`W76` `ed64ae3` 已落地）完成阶段 E 机器台账全量回填与独立终审（`docs/evidence/stage-e/e-road-001-*.md`、`reviews/w81-stage-e-exit-gates.md`，触发本 ADR 晋升 `VERIFIED`） | `H7` |

### 6.2 复审触发器

1. **五语言黄金向量或核外垫片性能/语义反例**：若 `W77/W78` 实测发现某一 SABI 通道在跨版本翻译中无法保持幂等键或 `Receipt` 签名摘要语义闭环，立即暂停 `SABI v1.0` 冻结并修订 §2.1/§2.2 转换边界。
2. **Wasm Component Model（WASI 0.2+）升级切换**：当 `nlos-wasm-sandbox` 从锁定版 WASI preview1 升级至 Component Model 时，触发 [ADR-0015](./0015-dual-layer-untrusted-execution-isolation.md) 触发器 3 与本 ADR §2.3 沙箱白名单联合复审。
3. **跨组织联邦商店互认需求开启**：若未来解除 `v0.5 §29.2`「全球联邦与跨组织清算」延后边界、引入多独立商店根 CA 交叉信任，须另立新 ADR 扩展根信任锚模型。
4. **出现 `major = 2` 破坏性协议演进提案**：任何无法通过 `nlos.sabi.v1` additive 扩展表达的系统契约变更，必须在进入代码前立项新 ADR 并同步提供 `nlos-compat-shim` 桥接与 `nlos-migrate` 规则。
