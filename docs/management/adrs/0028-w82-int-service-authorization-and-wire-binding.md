# ADR-0028：W82-INT 三新服务权限映射、线端命令签名、子预算绑定与实时挂载架构决策

- **状态**：**PROPOSED**（DESIGN 草案 — 尚未获人类维护者批准；在维护者签认前，daemon 侧对 GAP-1/GAP-2B/GAP-3 与三新服务挂载保持 fail-closed，严禁跳过本 ADR 绕过安全边界）
- **日期**：2026-11-21
- **Owner**：W82-INT 集成工作族（`nlos-system-control` / `nlos-capability` / `nlos-driver-llm-http` / `nlos-wasm-sandbox` / `nlos-schema` / Python & TypeScript SDK）
- **基线快照**：`9414000f9b19179ca0922d95bbf81e21eff137ab`（Wave `W82-R3` 第一波次独立架构决策车道 `W82-R3-GAP-ADR`）
- **关联 Requirement**：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §6 `TCB-BYPASS-001`、§7 `CAP-UNFORGE-001`、`CAP-ATTEN-001`、`CAP-REVOKE-001`、§9 `COMPAT-VER-001`、`COMPAT-VER-002`、`SABI-DISC-001`、`SABI-COMMON-001`、`SABI-RECEIPT-001`、`SABI-AUTH-001`、§28.5 `[ROAD-E-001]`；[stage-e-progress.md](../stage-e-progress.md) §E.6.1.8
- **关联前序 ADR**：[ADR-0010](./0010-signed-capability-commands.md)、[ADR-0011](./0011-ipc-principal-auth-signature-passthrough.md)、[ADR-0014](./0014-schema-channel-freeze-v1-beta.md)、[ADR-0015](./0015-dual-layer-untrusted-execution-isolation.md)、[ADR-0022](./0022-operator-root-capability-issuance.md)、[ADR-0027](./0027-stage-e-sabi-long-term-compatibility-and-store-governance.md)
- **关联设计与证据**：[w82-b-design-draft.md](../w82-b-design-draft.md)、[w82-b-contract-20261116.md](../../evidence/stage-e/w82-b-contract-20261116.md)、[w82-int-precheck-20261121.md](../../evidence/stage-e/w82-int-precheck-20261121.md)、[w82-b3-tool-service-20261116.md](../../evidence/stage-e/w82-b3-tool-service-20261116.md)、[del-base-coverage-audit.md](../del-base-coverage-audit.md)
- **复审触发器**：见第 8 节

---

## 1. 上下文与全 8 通道实时挂载阻塞全貌（Context & Blocking Surface）

### 1.1 SABI 12 通道与 Daemon 全 8 通道挂载范围澄清

在基线 `9414000f9` 中，[schema/nlos/sabi/v1/deprecation-manifest.yaml](../../../schema/nlos/sabi/v1/deprecation-manifest.yaml#L6-L95) 与 [crates/nlos-schema/src/lib.rs](../../../crates/nlos-schema/src/lib.rs#L34-L64) 已注册并冻结 **12 个** SABI v1 通道（`current_epoch: 1`、`frozen_channels_total: 12`）。其中：

- **4 个基础/专用通道不走 `SystemControlDaemon` 应用服务分派**：`nlos.sabi.Envelope`（外层传输信封）、`nlos.sabi.PrincipalHandshake`（连接前 ADR-0011 挑战应答握手）、`nlos.sabi.TakeoverControl`（接管控制面）与 `nlos.sabi.DriverProvider`（内核/Shim 侧驱动提供者接口）；
- **5 个已挂载的冻结 v1.0 守护进程服务通道**：[directory_control.rs](../../../crates/nlos-system-control/src/directory_control.rs#L70-L86) 与 [auth.rs](../../../crates/nlos-system-control/src/auth.rs#L292-L302) 当前分派 `"system_control"`、`"channel"`、`"wait_control"`、`"operation_control"` 与 `"service_directory"`；
- **3 个待实时挂载的 SABI v1.1 新服务通道**：`"llm_gateway"`（`nlos.sabi.LlmGateway`）、`"tool_sandbox"`（`nlos.sabi.ToolSandbox`）与 `"blob_reference"`（`nlos.sabi.BlobReference`）。

上述 `5 + 3` 即本文与 [w82-int-precheck-20261121.md](../../evidence/stage-e/w82-int-precheck-20261121.md#L39-L49) 所称的「**全 8 通道实时挂载**」。三套服务库（`Gateway`、`ToolSandboxService`、`BlobReferenceService`）均已交付库级形态与单元测试，但受限于线协议边界、授权策略与宿主绑定缺失，在未获维护者显式裁定前，守护进程保持 fail-closed，严禁以伪造 `ChildSessionBinding`、跳过签名、硬编码放行 `CapabilityRights` 或旁路库级不变量的方式抢跑挂载。

### 1.2 基线 `9414000f9` 实查阻塞清单

| 阻塞编号 | 影响范围 | 基线实查源码与契约锚点 | 不可违反的上游约束 |
|---|---|---|---|
| **GAP-3**（决策 1） | 全部三新服务 (`llm_gateway` / `tool_sandbox` / `blob_reference`) | [schema.rs:16-19, 67-83](../../../crates/nlos-capability/src/schema.rs#L16-L19)、[model.rs:8-78](../../../crates/nlos-capability/src/model.rs#L8-L78)、[service_call.rs:147-173, 262-316](../../../crates/nlos-capability/src/service_call.rs#L147-L173)、[daemon.rs:648-684](../../../crates/nlos-system-control/src/daemon.rs#L648-L684) | [w82-b-contract-20261116.md:117](../../evidence/stage-e/w82-b-contract-20261116.md#L117)：禁止虚构 `LLM/Tool/Blob` 权位，禁止现有语义权位自动批准新服务；须守 `[ROAD-E-001]` N-1 数据库兼容性 |
| **GAP-1**（决策 2） | `BlobReferenceService` (`mint_reference` / `read_reference` / `release_reference`) + Py/TS SDK | [blob_reference.rs:196-214, 231, 475, 577-617, 920-936](../../../crates/nlos-capability/src/blob_reference.rs#L231)、[blob_model.rs:62-86, 323-380](../../../crates/nlos-capability/src/blob_model.rs#L62-L86)、[blob_reference.proto:39-95](../../../schema/nlos/sabi/v1/blob_reference.proto#L39-L95)、[envelope.proto:45-55](../../../schema/nlos/sabi/v1/envelope.proto#L45-L55)、[_client_base.py:517](../../../sdk/python/nlos_sdk/_client_base.py#L517)、[blob_reference.py:298-475](../../../sdk/python/nlos_sdk/blob_reference.py#L298-L475)、[sabi_client.ts:89, 305-308](../../../sdk/typescript/src/sabi_client.ts#L305-L308)、[blob_reference.ts:239-392](../../../sdk/typescript/src/blob_reference.ts#L239-L392) | [ADR-0010](./0010-signed-capability-commands.md)（禁止服务端代签）；[ADR-0014](./0014-schema-channel-freeze-v1-beta.md)（仅 `ADDITIVE_SAFE` 扩列）；`schema/golden/epochs/{v1.0,v1.1,v1.1-blob-20261116}/` 零字节改动 |
| **GAP-2 Part A**（现状注记） | `Gateway::handle_wire` 外层 `Envelope.schema` 错配 | [gateway_wire.rs:53, 254](../../../crates/nlos-driver-llm-http/src/gateway_wire.rs#L53)（仍检/写 `SABI_LLM_GATEWAY_SCHEMA`）对比 [tool_service.rs:1013](../../../crates/nlos-wasm-sandbox/src/tool_service.rs#L1013)、[blob_reference.rs:879](../../../crates/nlos-capability/src/blob_reference.rs#L879)、[_client_base.py:596](../../../sdk/python/nlos_sdk/_client_base.py#L596)、[sabi_client.ts:312](../../../sdk/typescript/src/sabi_client.ts#L312)（统一为 `nlos.sabi.Envelope`） | 同步车道 `W82-R3-GW-FIX` 负责修复 Part A；**本 ADR 基线工作树 `9414000f9` 尚未合流该修复**，`W82-R4` 实施前须先合入 `W82-R3-GW-FIX` |
| **GAP-2 Part B**（决策 3） | `Gateway::attenuate_budget` 的 `ChildSessionBinding` 来源与返回绑定 | [llm_gateway.rs:196-207, 875-992](../../../crates/nlos-driver-llm-http/src/llm_gateway.rs#L196-L207)、[gateway_wire.rs:36-43, 148](../../../crates/nlos-driver-llm-http/src/gateway_wire.rs#L36-L43)、[llm_gateway.proto:81-92](../../../schema/nlos/sabi/v1/llm_gateway.proto#L81-L92)、[daemon.rs:1234-1238, 1356-1431](../../../crates/nlos-system-control/src/daemon.rs#L1356-L1431) | [w82-b-contract-20261116.md:121](../../evidence/stage-e/w82-b-contract-20261116.md#L121)：禁止从 `session_hint`/自报 `caller` 推断或静默虚构无真实子会话消费的假 child |
| **挂载前置 4**（决策 4） | `ToolSandboxService` 实时挂载与异步控制接线 | [tool_service.rs:55-77, 223-264, 270-386, 1001-1062](../../../crates/nlos-wasm-sandbox/src/tool_service.rs#L223-L264)、[operation_control.rs:36-116](../../../crates/nlos-system-control/src/operation_control.rs#L36-L116)、[w82-b3-tool-service-20261116.md:119](../../evidence/stage-e/w82-b3-tool-service-20261116.md#L119) | 无需改动 `tool_service.rs` 源码即可挂载；缺省配置 fail-closed；跨通道取消须对接 `OperationControl`/`WaitControl`；诚实记录 macOS `p95 = 72.50 ms` 信号 |

---

## 2. 决策 1（`GAP-3`）：`ServiceAuthorizer` 策略与 `CapabilityRights` 权限映射

### 2.1 基线约束核查

1. `capability_heads` 表结构（[schema.rs:9-29](../../../crates/nlos-capability/src/schema.rs#L9-L29)）硬性限定：
   - `target_kind INTEGER NOT NULL CHECK(target_kind IN (1, 2))`（对应 [model.rs:8-11](../../../crates/nlos-capability/src/model.rs#L8-L11) 的 `Namespace` 与 `Task`）；
   - `rights INTEGER NOT NULL CHECK(rights > 0 AND rights <= 15)`（对应 [model.rs:38-48](../../../crates/nlos-capability/src/model.rs#L38-L48) 的 4 个权位：`SEMANTIC_APPEND = 1 << 0`、`SEMANTIC_RETRACT = 1 << 1`、`SEMANTIC_ADJUDICATE = 1 << 2`、`DELEGATE = 1 << 3`）；
   - `purpose_digest BLOB CHECK(purpose_digest IS NULL OR length(purpose_digest) = 32)`（32 字节可选目的摘要）。
2. `capability_heads` 全身份列受 `capability_heads_identity_immutable` 与 `capability_heads_no_delete` 触发器锁死（[schema.rs:67-83](../../../crates/nlos-capability/src/schema.rs#L67-L83)），当前 `SCHEMA_VERSION` 为 `3`（[lib.rs:49](../../../crates/nlos-capability/src/lib.rs#L49)）。
3. [lib.rs:415, 1269-1275](../../../crates/nlos-capability/src/lib.rs#L1269-L1275) 已提供 `purpose_is_attenuated(parent, child)`：无目的限制的父能力（`purpose_digest = None`）**允许**通过 `delegate_signed` 向下衰减签发绑定特定 `Some(digest)` 的子能力，而一旦绑定 `Some(expected)`，子能力及后续再委托均不可去掉或改写该 `purpose_digest`。同时 `SemanticAuthority` 仅认可与其自身语义事件摘要匹配的 `purpose_digest`，不会接受服务域专用的 `purpose_digest`。
4. 重要隐含不变量（实查 [blob_ledger.rs:433-449](../../../crates/nlos-capability/src/blob_ledger.rs#L433-L449)）：`BlobReferenceService` 内部会在数据库事务中再次调用 `blob_ledger::check_grant(db, source, holder, namespace, now)`，其中第 445 行强制校验 **`record.target == CapabilityTarget::Namespace(namespace)` 精确相等**（不走根命名空间 `[0; 16]` 前缀包含）。这与 [namespace.rs:23-43](../../../crates/nlos-capability/src/namespace.rs#L23-L43) 的前缀树包含语义存在细微差异，必须在 authorizer 策略中显式对齐。
5. 守护进程现有 `ControlCapabilityPolicy`（[daemon.rs:648-684](../../../crates/nlos-system-control/src/daemon.rs#L648-L684)）仅对比静态控制槽 `CONTROL_CAPABILITY_SLOT`/`CONTROL_CAPABILITY_GENERATION`，而三新服务消费 `nlos-capability::service_call::ServiceAuthorizer`（[service_call.rs:147-173](../../../crates/nlos-capability/src/service_call.rs#L147-L173)，默认 `DenyAllAuthorizer`）。

### 2.2 候选方案对比

- **方案 1A（推荐）：零 SQLite 迁移 — 基于 `purpose_digest` 服务域隔离 + 既有 4 位 `CapabilityRights` 动作映射的 `AuthorityServiceAuthorizer`**
  - **核心机制**：保持 `capability-authority.db` (`SCHEMA_VERSION = 3`) 零表结构变动，在 `nlos-system-control` 实现 `AuthorityServiceAuthorizer`（持有 `Arc<CapabilityAuthority>`），对每个 `ServiceAuthorizationBinding`（[service_call.rs:133-136](../../../crates/nlos-capability/src/service_call.rs#L133-L136)）逐一校验：
    1. **会话槽位解引用**：调用方提供的 `CallBinding.capabilities` 必须来自 Daemon 侧每会话 `u64 slot -> CapabilityId(16B)` 映射表（容量 `MAX_SERVICE_CAPABILITY_HANDLES = 64`，LRU 且使用中 pin，[w82-b-contract-20261116.md:113](../../evidence/stage-e/w82-b-contract-20261116.md#L113)）；**禁止把 `slot` 整数直接解释为 `CapabilityId`**。
    2. **权威链与持有者核验**：通过 `CapabilityAuthority::inspect_active(handle, now.wall_ms)`（[lib.rs:709-718](../../../crates/nlos-capability/src/lib.rs#L709-L718)）验证当前 `generation`、未吊销、整条祖先链有效、时间窗口 `[record.valid_from_ms, record.valid_until_ms]` 覆盖 `now.wall_ms`，且 `record.holder == binding.call.caller.principal`。
    3. **强制非空且领域分离的 `purpose_digest`（杜绝既有权限越权）**：
       为三个新服务定义固定、公开可审计的 SHA-256 域分离目的摘要常量（在 `nlos-system-control` 或 `nlos-capability::service_call` 统一导出）：
       - `PURPOSE_SERVICE_LLM_GATEWAY = SHA-256("nlos/service-purpose/v1/llm_gateway")`
       - `PURPOSE_SERVICE_TOOL_SANDBOX = SHA-256("nlos/service-purpose/v1/tool_sandbox")`
       - `PURPOSE_SERVICE_BLOB_REFERENCE = SHA-256("nlos/service-purpose/v1/blob_reference")`
       - **铁律**：凡 `record.purpose_digest == None`（例如 ADR-0022 缺省签发的无 `purpose_digest` operator root 或历史语义能力）或携带其他域 `purpose_digest` 的能力，**一律 fail-closed 拒绝（`AuthorizationError::Denied`）**。持有 `DELEGATE` 权的根/父能力必须先通过 `delegate_signed` 显式派生绑定对应服务 `purpose_digest` 且收敛权限位的子能力，方可授权调用该服务（或在部署期经 `--operator-purpose-digest` 显式绑定单一服务域根能力）。
    4. **目标作用域与既有 4 位 `CapabilityRights` 精确映射表**：
       既不新增 bit，也不让旧语义权位裸放行——在 `purpose_digest` 严格正交隔离的基础上，复用 4 个权位表达服务内最小动作集（只读查询= `SEMANTIC_ADJUDICATE`、写入/消费/触发= `SEMANTIC_APPEND`、释放/取消= `SEMANTIC_RETRACT`、跨主体预算衰减= `DELEGATE`）：

       | `ServiceMethod`（[service_call.rs:51-65](../../../crates/nlos-capability/src/service_call.rs#L51-L65)） | 要求的 `purpose_digest` | 最低 `CapabilityRights` | `CapabilityTarget` 校验规则 |
       |---|---|---|---|
       | `LlmGateway`: `QueryBudget` | `Some(PURPOSE_SERVICE_LLM_GATEWAY)` | `SEMANTIC_ADJUDICATE` | `target_is_within(Namespace(action.target.namespace), record.target)` |
       | `LlmGateway`: `LlmChat` / `RecordExternalUsage` | `Some(PURPOSE_SERVICE_LLM_GATEWAY)` | `SEMANTIC_APPEND` | 同上 |
       | `LlmGateway`: `AttenuateBudget`（同主体） | `Some(PURPOSE_SERVICE_LLM_GATEWAY)` | `SEMANTIC_APPEND` | 同上 |
       | `LlmGateway`: `AttenuateBudget`（跨主体/跨应用委派） | `Some(PURPOSE_SERVICE_LLM_GATEWAY)` | `SEMANTIC_APPEND \| DELEGATE` | 同上；并须符合决策 3 子会话绑定 |
       | `ToolSandbox`: `ListTools` / `QueryToolOperation` | `Some(PURPOSE_SERVICE_TOOL_SANDBOX)` | `SEMANTIC_ADJUDICATE` | `target_is_within(Namespace(action.target.namespace), record.target)` |
       | `ToolSandbox`: `InvokeTool` | `Some(PURPOSE_SERVICE_TOOL_SANDBOX)` | `SEMANTIC_APPEND` | 同上 |
       | `ToolSandbox`: `CancelToolOperation` | `Some(PURPOSE_SERVICE_TOOL_SANDBOX)` | `SEMANTIC_RETRACT` | 同上 |
       | `BlobReference`: `MintReference` | `Some(PURPOSE_SERVICE_BLOB_REFERENCE)` | `SEMANTIC_APPEND` | **`record.target == Namespace(action.target.namespace)`**（与 [blob_ledger.rs:445](../../../crates/nlos-capability/src/blob_ledger.rs#L445) 对齐） |
       | `BlobReference`: `ReadReference` | `Some(PURPOSE_SERVICE_BLOB_REFERENCE)` | `SEMANTIC_ADJUDICATE` | 同上（精确匹配 `token.workspace.namespace`） |
       | `BlobReference`: `ReleaseReference` | `Some(PURPOSE_SERVICE_BLOB_REFERENCE)` | `SEMANTIC_RETRACT` | 同上（精确匹配 `token.workspace.namespace`） |

    5. **`call_limit` 诚实边界**：`CapabilityAuthority::consume`（[lib.rs:621-680](../../../crates/nlos-capability/src/lib.rs#L621-L680)）依赖 `VerifiedSemanticSigner` 且服务层已有独立持久配额（`GatewayLimits` / `BlobConfig` / `ToolHostQuota`）。为防止呈现有限 `call_limit` 的能力却不扣减造成策略误导，`AuthorityServiceAuthorizer` 对 `record.call_limit.is_some()` 的服务能力默认 fail-closed 拒绝（要求服务专用能力 `call_limit = None`，由服务自身持久配额账本执行次数/预算上限）；若未来需对接 `capability_consumption_rows`，另开增量切片。
    6. **多能力挑选顺序与 `PolicyGrant` 返回**：按 `CallBinding.capabilities` 呈现顺序择一满足全部条件的 `CapabilityHandle`，返回精确绑定原 `binding`、`capability` 与 `[record.valid_from_ms, record.valid_until_ms]` 的 `PolicyGrant`，由 `authorize_call`（[service_call.rs:299-315](../../../crates/nlos-capability/src/service_call.rs#L299-L315)）完成双边复核。

- **方案 1B（备选/否决）：通过 SQLite `migrate_v4` 重建 `capability_heads` 表并扩展 `CapabilityRights` 与 `CapabilityTarget`**
  - 将 `capability_heads` 的 `CHECK(rights > 0 AND rights <= 15)` 扩为更宽位掩码（如 64-bit 独立 `LLM_CHAT` / `TOOL_INVOKE` / `BLOB_MINT` / `BLOB_READ` 位）或放宽 `target_kind IN (1..=5)`，由于 `capability_heads` 受不可变触发器锁定且 SQLite 不支持 `ALTER TABLE ALTER CONSTRAINT`，必须执行「建新表 `capability_heads_v4` -> `INSERT .. SELECT` 复制 -> 删除旧触发器与旧表 -> 重命名」全表重建迁移，并将 `SCHEMA_VERSION` 升至 `4`。

| 评估维度 | 方案 1A（推荐：`purpose_digest` + 4-bit 映射） | 方案 1B（否决：`migrate_v4` 重建表扩位） |
|---|---|---|
| 安全性与防越权 | **强隔离**：服务域与语义域由 256-bit `purpose_digest` 密码学正交；`purpose_digest = None` 既有能力 100% 拒绝；支持读写/取消/委派服务内衰减 | 位定义直观，但扩权后须同步更新全仓所有 `rights` 位掩码边界测试与祖先衰减逻辑，重建期间存在 DDL 事务复杂面 |
| 最小 TCB 增量 | **零改动 `nlos-capability` 权威状态机与 SQLite DDL**；仅在 `nlos-system-control` 新增单一可审计策略模块 | 修改核心 TCB crate `nlos-capability` 的 schema、model、命令哈希、触发器与全部迁移测试 |
| `[ROAD-E-001]` N-1 兼容 | **100% 向后/向前兼容**：既有 `user_version = 3` 数据库文件无需升级即可读写，回退零数据库降级障碍 | `user_version` 升至 `4` 后，回滚至 `N-1` 宿主会因 `SchemaVersionUnsupported(4)` 直接拒绝开库 |
| 退出成本 | 极低：若未来真需多服务混用单张能力，可随时以 `migrate_v4` 增量接入并将现有 `purpose_digest` 策略平滑迁出 | 高：一旦落盘 `v4` DDL 与新 bit 语义，历史库不可逆 |

### 2.3 决策结论（决策 1）

**拟采纳方案 1A**。维持 `capability_heads` `SCHEMA_VERSION = 3` 零迁移与 `CapabilityRights` 4 权位不变；在 `nlos-system-control` 实现 `AuthorityServiceAuthorizer`，严格执行「非空且服务域分离的 `purpose_digest` + 动作映射 `CapabilityRights` + 命名空间校验 + 会话 slot 表解引用 + `purpose_digest = None` 拒绝」。

---

## 3. 决策 2（`GAP-1`）：`BlobReferenceService` 线端命令签名与 `operation()` 摘要对齐

### 3.1 基线约束核查

1. **线端命令签名断层**：[blob_reference.rs:196-214, 231, 475](../../../crates/nlos-capability/src/blob_reference.rs#L196-L214) 要求 `mint_reference` 与 `release_reference` 必须传入 `proof: Option<BlobCommandProof<'_>>`（[blob_model.rs:62-86](../../../crates/nlos-capability/src/blob_model.rs#L62-L86) 中的 `SignedMintCommand` / `SignedReleaseCommand`），否则返回 `BlobError::MissingSignature`。验签（[blob_reference.rs:577-617](../../../crates/nlos-capability/src/blob_reference.rs#L577-L617)、`:488-497`）要求调用者（`signer == op.actor == call.caller.principal`）用其当前有效且未吊销的 `KeyPurpose::SemanticSigning` 私钥签名 `mint_command_message`（命令类型 `4`）或 `release_command_message`（命令类型 `5`，见 [blob_model.rs:323-351](../../../crates/nlos-capability/src/blob_model.rs#L323-L351)）。注：[daemon.rs:2244](../../../crates/nlos-system-control/src/daemon.rs#L2244) 引导的客户端密钥正是 `KeyPurpose::SemanticSigning`，且 [nlos-ipc/src/handshake.rs:372-379](../../../crates/nlos-ipc/src/handshake.rs#L372-L379) 握手验证同一密钥，故 Py/TS 已认证客户端天然持有可用的 `Ed25519Signer`。
2. **`operation()` 强制非空摘要断层**：[blob_reference.rs:920-936](../../../crates/nlos-capability/src/blob_reference.rs#L920-L936) 严格检查 `common.proposal_or_input_digest_sha256.as_slice() != digest` 即拒（`BlobError::BindingMismatch`），**不同于** [tool_service.rs:1056](../../../crates/nlos-wasm-sandbox/src/tool_service.rs#L1056) 和 [llm_gateway.rs:1477](../../../crates/nlos-driver-llm-http/src/llm_gateway.rs#L1477) 容忍空摘要的做法。当前 Python SDK 默认传 `b""`（[_client_base.py:517](../../../sdk/python/nlos_sdk/_client_base.py#L517)，且 [blob_reference.py:298-475](../../../sdk/python/nlos_sdk/blob_reference.py#L298-L475) 未透出参数），TypeScript SDK（[sabi_client.ts:305-308](../../../sdk/typescript/src/sabi_client.ts#L305-L308)、[blob_reference.ts:239-392](../../../sdk/typescript/src/blob_reference.ts#L239-L392)）默认传空且未自动计算输入摘要，导致连不需要命令签名的 `read_reference` 也必定失败。
3. **现行 Proto 占用编号**：
   - `blob_reference.proto`（[blob_reference.proto:39-95](../../../schema/nlos/sabi/v1/blob_reference.proto#L39-L95)）：`MintReferenceRequest` 已占用 `1..=8`（含 2026-11-16 补充裁定加入的 `recipient_principal_id = 8`）；`ReadReferenceRequest` 已占用 `1..=4`；`ReleaseReferenceRequest` 已占用 `1..=2`。
   - `envelope.proto`（[envelope.proto:45-55](../../../schema/nlos/sabi/v1/envelope.proto#L45-L55)）：`SabiRequestContext` 已占用 `1..=9`（第 9 字段为 `proposal_or_input_digest_sha256`）。
   - [auth.rs:40-44](../../../crates/nlos-system-control/src/auth.rs#L40-L44) 明确注记：连接级握手认证连接，命令级签名透传（ADR-0011 Decision 2）留待专门实现切片。

### 3.2 候选方案对比（含线协议与摘要对齐子决策）

- **方案 2A（推荐）：`blob_reference.proto` 纯增量承载 `BlobCommandScopeWire` + `BlobCommandProofWire`（`0 BREAKING`），且 SDK 端确定性计算摘要与命令签名**
  1. **Proto 纯增量扩展（[blob_reference.proto](../../../schema/nlos/sabi/v1/blob_reference.proto)）**：
     - 严禁修改 `schema/golden/epochs/{v1.0,v1.1,v1.1-blob-20261116}/` 下任何已有文件；新签名黄金向量（如需入库）独立置于新目录 `schema/golden/epochs/v1.1-int-20261121/`（延续 [w82-b-contract-20261116.md §6.1](../../evidence/stage-e/w82-b-contract-20261116.md#L148) 模式）。
     - 新增两条只增消息与顺延字段：
       ```protobuf
       message BlobCommandScopeWire {
         bytes workspace_id = 1;                  // 16 bytes
         bytes namespace_id = 2;                  // 16 bytes
         uint64 workspace_revision = 3;           // >= 1
         bytes source_capability_id = 4;          // 16 bytes; must match daemon-resolved session capability
         uint64 source_capability_generation = 5; // >= 1
         uint64 issued_at_ms = 6;                 // signer claim timestamp (checked <= server now.wall_ms)
       }

       message BlobCommandProofWire {
         BlobCommandScopeWire scope = 1;
         bytes signer_principal_id = 2;           // 16 bytes; must equal authenticated caller.principal
         bytes signature = 3;                     // 64 bytes Ed25519 over mint/release_command_message
       }

       // MintReferenceRequest: append after field 8
       //   BlobCommandProofWire command_proof = 9;
       // ReleaseReferenceRequest: append after field 2
       //   BlobCommandProofWire command_proof = 3;
       ```
     - **加密原像零破坏关键设计**：实查 [blob_model.rs:299-351](../../../crates/nlos-capability/src/blob_model.rs#L299-L351)，`mint_fields` 与 `release_fields` 逐字段显式读取请求语义字段（即 `MintReferenceRequest` 的 `2..=8` 与 `ReleaseReferenceRequest` 的 `2`），**不会序列化整个 protobuf 字节流**，因此新增 `command_proof`（field `9` / field `3`）天然不会与 `mint_command_message`、`release_command_message`、`mint_input_digest`、`release_input_digest` 产生签名自指循环，原有 Rust 原像定义可 100% 复用。总载荷增幅约 140 bytes，远低于 `MAX_BLOB_REFERENCE_PAYLOAD_BYTES` (65,536 bytes)。
     - **向后兼容与 fail-closed 规则**：无 `command_proof` 的旧版 `MintReferenceRequest`/`ReleaseReferenceRequest` 帧在 protobuf 层仍可解码，但服务层保持 `proof.ok_or(BlobError::MissingSignature)` fail-closed 拒绝，绝不从历史无签名帧隐式补签或降级放行（[ADR-0010](./0010-signed-capability-commands.md)）。
     - **Daemon 接线绑定核验**：Daemon 侧适配器解析 `command_proof` 后，必须核验 `scope.source_capability_id` 与会话 slot 表从 `SabiRequestContext.capability_handles` 解析出的 `CallBinding.capabilities` 一致（并对齐 [blob_reference.rs:627](../../../crates/nlos-capability/src/blob_reference.rs#L627) `grant.capability() == scope.source` 与 [blob_reference.rs:483](../../../crates/nlos-capability/src/blob_reference.rs#L483) `token.source == proof.scope.source`）。注：当接收方 `holder`（非 `issuer`）独立发起 `release_reference` 时，因 `:483` 校验 `proof.scope.source == token.source`，应用层约定在传递 `nlosref://<32hex>` 引用时一并传递源能力句柄或由 `MintReferenceResult` 后续按需只增返回，否则仅 `issuer` 持源能力元数据释放；在授权检查层面仍由 `check_grant(db, grant.capability(), op.actor, ...)` 验证 `holder` 自身具备释放权。
  2. **`proposal_or_input_digest_sha256` 对齐子决策（「SDK 计算 canonical digest」vs「服务端容忍空摘要」）**：
     - **选择：维持服务端 [blob_reference.rs:925](../../../crates/nlos-capability/src/blob_reference.rs#L925) 严格拒空校验不变，在 Python/TS SDK 确定性计算并填充 `proposal_or_input_digest_sha256`。**
     - 取舍理由：若将服务端 `operation()` 放松为容忍 `b""`（仅在非空时比对），虽然能免去修改 SDK `read_reference` 路径，但会使任何中继/客户端遗漏摘要字段时静默失去输入绑定保护，并破坏已通过三平台 `H4` 验收的 `BlobReferenceService` 安全不变量。反之，在 Python（[_client_base.py:517](../../../sdk/python/nlos_sdk/_client_base.py#L517) 透传该参数并在 [blob_reference.py](../../../sdk/python/nlos_sdk/blob_reference.py) 实现 `mint_input_digest`/`read_input_digest`/`release_input_digest`）与 TypeScript（[blob_reference.ts](../../../sdk/typescript/src/blob_reference.ts) 自动计算并填入 `CallOptions.proposalOrInputDigestSha256`）实现 30 行确定性 SHA-256 构造，成本极低、零安全退步，且可通过 Rust vs Python vs TypeScript 逐字节一致性测试闭环。
     - **SDK 命令签名接口**：在 Py/TS `BlobReferenceClient` 新增可选默认或逐调用 `command_signer`（`Ed25519Signer`）与 `BlobCommandScope` 输入，按 `blob_model.rs:323-351` 格式构造 `mint_command_message` / `release_command_message` 并填入 `command_proof`。当未配置 `command_signer` 时，客户端可显式报错（或仅在显式测试模式下发空 proof 触发服务端 `MissingSignature` 负向测试）。

- **方案 2B（备选/否决）：沿 [auth.rs:42](../../../crates/nlos-system-control/src/auth.rs#L42) 在 `envelope.proto` 的 `SabiRequestContext` 实现全通道通用的 ADR-0011 Decision 2 命令签名透传头**
  - 在 `SabiRequestContext` 新增 `bytes command_signature = 10` 等通用字段并升级 `Envelope` 公共验证器 `validate_sabi_request_context`，要求全部通道或带签标志通道透传外层命令签名。

| 评估维度 | 方案 2A（推荐：`blob_reference.proto` 局部增量 + SDK 计算摘要） | 方案 2B（否决：修改 `envelope.proto` `SabiRequestContext`） |
|---|---|---|
| 影响面与耦合度 | **仅改动 1 个通道（`blob_reference.proto` 2 条消息）**与对应服务 SDK 客户端；其余 11 个通道、公共 `Envelope`、`nlos-compat-shim` 及 Go/C# 公共信封路径零受波及 | **波及全部 12 个 SABI 通道**、公共 `Envelope`、`validate_sabi_request_context`、Go/C#/Py/TS 全套公共信封探针与所有既有通道的签名规范定义 |
| 域绑定语义契合度 | `BlobCommandScope` 含工作区/命名空间/修订/源能力等 `BlobReference` 私有字段，驻留 `blob_reference.proto` 语义内聚 | 通用 `SabiRequestContext` 无法自然表达 `BlobCommandScope` 专用字段，仍需在 payload 内重复携带或塞入通用扩展，冗余且易失配 |
| 安全性与 `ADR-0010` 合规 | 保持服务端零代签、维持 `operation()` 拒空严格摘要绑定，安全强度无任何折扣 | 同等合规，但跨 12 通道统一规范化签名原像未就绪，强行前置将阻塞 `W82-INT` 收口周期 |
| 退出策略 | `BlobReference` 局部增量字段与未来通用 `SabiRequestContext` 命令签名可独立并存或按 `DEPRECATED -> SUNSET` 机制演进 | 一旦进入 `SabiRequestContext` 公共字段编号即永久占号，无法轻易收缩 |

### 3.3 决策结论（决策 2）

**拟采纳方案 2A**。通过 `blob_reference.proto` 纯增量字段 `MintReferenceRequest.command_proof = 9` 与 `ReleaseReferenceRequest.command_proof = 3` 承载 `BlobCommandScopeWire` + `Ed25519` 命令签名；保持 [blob_reference.rs:925](../../../crates/nlos-capability/src/blob_reference.rs#L925) 拒空摘要不松动，由 Py/TS SDK 确定性计算 `proposal_or_input_digest_sha256` 与命令签名。

---

## 4. 决策 3（`GAP-2` Part B）：`Gateway::attenuate_budget` 的 `ChildSessionBinding` 绑定

### 4.1 基线现状与 Part A 分界声明

- **`GAP-2` Part A 现状（诚实说明）**：截至基线 `9414000f9`，同步车道 `W82-R3-GW-FIX` 尚未合入本分支工作树，因此本工作树中 [gateway_wire.rs:53, 254](../../../crates/nlos-driver-llm-http/src/gateway_wire.rs#L53) 仍校验/回写 `"nlos.sabi.LlmGateway"` 而非 `"nlos.sabi.Envelope"`。Part A 属于编解码对齐缺陷，由 `W82-R3-GW-FIX` 修复；**`W82-R4` 合流必须以 `W82-R3-GW-FIX` 合流为前置条件**。
- **`GAP-2` Part B 现状**：
  1. [llm_gateway.rs:196-200, 895](../../../crates/nlos-driver-llm-http/src/llm_gateway.rs#L196-L200) 要求 `attenuate_budget` 必须接收可信宿主机传入的 `child: Option<&ChildSessionBinding>`（包含 `recipient: RuntimeCaller`、`stable_identity: [u8; 16]` 与 `delegated: bool`）；缺失时返回 `AuthorizationError::Denied`（[gateway_wire.rs:302](../../../crates/nlos-driver-llm-http/src/gateway_wire.rs#L302) 映射为 `SabiErrorCode::Rights`）。
  2. [llm_gateway.proto:81-92](../../../schema/nlos/sabi/v1/llm_gateway.proto#L81-L92) 中 `AttenuateBudgetRequest`（`1..=3`）与 `AttenuateBudgetResult`（`1..=3`）没有任何子主体/子会话选择字段或可消费返回句柄。
  3. [daemon.rs:1356-1431](../../../crates/nlos-system-control/src/daemon.rs#L1356-L1431) 已有 `(CapabilityId, ProcessId)` 受管 Worker 绑定与 `SupervisorPidRegistry`，但尚无将「已握手活跃子会话 / 已注册受管子进程」解析为 `ChildSessionBinding` 的注册表，也无将返回的 `ChildBudgetBinding`（[llm_gateway.rs:204-207](../../../crates/nlos-driver-llm-http/src/llm_gateway.rs#L204-L207)）持久挂载至子会话后续 `llm_chat` 调用的路由索引。
  4. [w82-b-contract-20261116.md:121](../../evidence/stage-e/w82-b-contract-20261116.md#L121) 严禁从 `session_hint` 或客户端自报 `caller` 推断绑定，也严禁在无真实子会话消费时静默虚构内部 child 假装完成。

### 4.2 候选方案对比

- **方案 3A（推荐）：Daemon 受管会话/Worker 注册表解析 + `llm_gateway.proto` 纯增量选择器与返回句柄**
  1. **Wire 纯增量字段（[llm_gateway.proto](../../../schema/nlos/sabi/v1/llm_gateway.proto)）**：
     - 在 `AttenuateBudgetRequest`（已有 `1..=3`）顺延新增可选选择器（只用于从 Daemon 权威注册表中*检索并交叉核对*，**本身零授权效力**）：
       - `bytes child_process_id = 4;`（可选 16-byte 目标受管子进程 `ProcessId`）
       - `bytes child_session_id = 5;`（可选 16-byte 目标已握手子会话 `SessionId`）
       - `bytes child_principal_id = 6;`（可选 16-byte 目标主体校验锚点 `PrincipalId`，非空须为 16B 并与注册态逐字节核对）
     - 在 `AttenuateBudgetResult`（已有 `1..=3`）顺延新增可审计关联回执：
       - `bytes child_account_id = 4;`（16-byte 新建/重放子 `ResourceAccountId`，对应 `ChildBudgetBinding.account.account_id()`）
       - `bytes child_binding_digest = 5;`（32-byte 域分隔绑定摘要，供父子会话与运维对账，不可用于越权自举）
  2. **Daemon 权威 `ChildSessionBinding` 解析与子账户路由**：
     - 在 `SystemControlDaemon` 新增受管子会话/Worker 账户注册表（绑定 `ProcessAuthority`、`SupervisorPidRegistry` / `capability_workers` 与已握手连接态），仅在满足以下全部条件时才构造 `Some(ChildSessionBinding)`：
       a) 指定的目标子进程/子会话在 Daemon 权威注册表中真实存在、处于未退出/未回收活跃态且经 ADR-0011 握手或受管启动绑定了权威 `RuntimeCaller`（`principal`, `application`, `process`, `process_generation`, `session`）；
       b) 若提供了 `child_principal_id` 且非空，必须严格等于注册态 `recipient.principal`；
       c) `stable_identity` 由 Daemon 按域分隔从 `(parent_account_id, recipient.principal, recipient.application, recipient.process)` 或受管 Worker 稳定 ID 派生，不由客户端填写；
       d) 跨主体或跨应用衰减时（`caller.principal != recipient.principal || caller.application != recipient.application`），仅当父能力具备 `DELEGATE`（决策 1）且 Daemon 注册了显式父子委派关系时才置 `delegated = true`，否则置 `delegated = false`（触发 [llm_gateway.rs:917-922](../../../crates/nlos-driver-llm-http/src/llm_gateway.rs#L917-L922) fail-closed 拒绝）。
     - **未绑定时 fail-closed**：当请求未选择任何受管子对象，或目标未在 Daemon 注册/已退出/未完成绑定时，Daemon 传入 `child = None` 并返回明确的 typed `SabiFailure`（映射为 [envelope.proto:63-65](../../../schema/nlos/sabi/v1/envelope.proto#L63-L65) 中的 `SABI_ERROR_CODE_STATE` 或 `SABI_ERROR_CODE_RIGHTS` / `NOT_FOUND`），**绝不退化为 `session_hint` 猜绑或虚构内部 child**。
     - **子会话消费闭环**：当 `Gateway::handle_wire` 返回 `WireReply { child: Some(child_binding), .. }` 时，Daemon 将 `child_binding.account` (`GatewayAccountBinding`) 按子进程/子会话 `(principal, application, process, session)` 键入驻内存路由表（并可通过 Gateway 启动日志重放恢复），使子进程随后发起 `llm_chat` / `query_budget` 时自动命中其衰减后子账户，受父>=子双包络约束。
  3. **`check_digest` sidecar 耦合处置点（供 `W82-R4` 明确对齐）**：实查 [llm_gateway.rs:901-915](../../../crates/nlos-driver-llm-http/src/llm_gateway.rs#L901-L915)，`attenuate` 会把宿主机构造的 `ChildSessionBinding` sidecar 字节纳入 `digest` 校验。由于线端客户端无法预先得知 Daemon 派生的内部 `session` / `stable_identity`，`attenuate_budget` 客户端在未共享 `stable_identity` 前应将 `SabiRequestContext.proposal_or_input_digest_sha256` 留空（[llm_gateway.rs:1477](../../../crates/nlos-driver-llm-http/src/llm_gateway.rs#L1477) 合法放行），或由 `W82-R4` 在 `gateway_wire.rs` 将外部请求的 `proposal_or_input_digest_sha256` 限定为比对 wire-visible 规范化请求摘要。

- **方案 3B（备选）：Daemon 接入层暂维持 `attenuate_budget` fail-closed，待双 Worker 多会话注册落地后再开放**
  - 在 `W82-R4` 挂载初期，`llm_gateway.proto` 保持 3 字段不动，Daemon 对 `llm_gateway.attenuate_budget` 一律传 `child = None`（或显式以 `SABI_ERROR_CODE_NOT_SUPPORTED` / `STATE` 拒绝），优先用单/双独立 `provision_root` 账户解锁 `llm_chat`、`query_budget` 与 `record_external_usage` 三个方法；待双 Worker 父子注册表与跨会话测试完备后再另开增量变更开放 `attenuate_budget`。

| 评估维度 | 方案 3A（推荐：Daemon 受管注册表 + `llm_gateway.proto` 增量扩展） | 方案 3B（备选：暂保持 `attenuate_budget` fail-closed） |
|---|---|---|
| 功能完整性 | **一次性打通全 4 个 `LlmGateway` 方法的架构契约**，支撑父会话向真实子 Worker/子脑安全分账（父>=子不变量） | `attenuate_budget` 在首轮接入不可用，若外部双脑依赖预算衰减分账则需二次立项 |
| 协议前瞻性 | `llm_gateway.proto` 顺延字段 `4..=6` / `4..=5` 一次冻结到位，符合 `ADDITIVE_SAFE` 规则，避免后续再改 proto 与重新生成四语言代码 | 零 proto 改动，但协议缺口只是延后，仍需一轮 `schema:generate` 与 SDK 更新 |
| 安全性与防绕过 | **未绑定时运行期同样 fail-closed**；选择器只检索 Daemon 受管态、禁止自授，完全满足 `w82-b-contract-20261116.md §4.3` | 安全性最高（攻击面收缩为 0），但功能缺失 |
| 实施进度耦合 | 实现上可平滑分步：先合入 proto 增量与 Daemon 子会话注册表；在无注册子 Worker 的运行配置下自然维持 fail-closed | 实现最快，适合作为 `W82-R4` 万一出现工期阻塞时的降级兜底开关 |

### 4.3 决策结论（决策 3）

**拟采纳方案 3A**（并保留 3B 的运行期防线：当 Daemon 尚未登记目标受管子进程/子会话时，即使上线了增量字段，调用仍严格 fail-closed 拒绝，绝不伪造 child）。

---

## 5. 决策 4：`ToolSandboxService` 实时挂载与可信配置规范

### 5.1 无需修改库源码的可挂载性确认

经逐行核对 [crates/nlos-wasm-sandbox/src/tool_service.rs:223-264](../../../crates/nlos-wasm-sandbox/src/tool_service.rs#L223-L264) 与 [:1001-1062](../../../crates/nlos-wasm-sandbox/src/tool_service.rs#L1001-L1062)：

1. `check_envelope`（[tool_service.rs:1013](../../../crates/nlos-wasm-sandbox/src/tool_service.rs#L1013)）已正确校验外层 `SABI_ENVELOPE_SCHEMA`（`"nlos.sabi.Envelope"`），`Catalog` 与 `encode_output` 构造内层 `tool_sandbox_schema_identity()`（`"nlos.sabi.ToolSandbox"` `1.0`），与 Py/TS SDK 100% 对齐；
2. `check_digest`（[tool_service.rs:1056](../../../crates/nlos-wasm-sandbox/src/tool_service.rs#L1056)）容忍空 `proposal_or_input_digest_sha256` 且在非空时核验 `list_input_digest` / `invoke_input_digest`；
3. `ToolSandboxService::new`（[tool_service.rs:223-264](../../../crates/nlos-wasm-sandbox/src/tool_service.rs#L223-L264)）全部依赖（`host`、`registrations`、`clock`、`policy`、`store`、`limits`）均通过构造参数注入，因此 **`nlos-wasm-sandbox` 库源码零改动即可由 `nlos-system-control` 装配并挂载**。

### 5.2 宿主侧可信装配与跨通道规范

为将 `ToolSandboxService` 安全挂载至 `SystemControlDaemon`，本决策规定以下五项强制宿主规范：

1. **宿主机隔离载体二进制（`nlos-wasm-sandbox-host`）路径发现与校验**：
   - `ToolSandboxService::new`（[tool_service.rs:231-240](../../../crates/nlos-wasm-sandbox/src/tool_service.rs#L231-L240)）要求 `host` 必须为绝对路径、`canonicalize()` 成功且为普通文件（`is_file()`）。
   - 在 `DaemonOptions`（[daemon.rs:455-493](../../../crates/nlos-system-control/src/daemon.rs#L455-L493)）与 `system-control-daemon` CLI（[system-control-daemon.rs:85-125](../../../crates/nlos-system-control/src/bin/system-control-daemon.rs#L85-L125)）新增显式配置 `--wasm-sandbox-host <ABS_PATH>`。
   - **禁止生产环境盲扫 `PATH`**：生产部署必须显式传入 `--wasm-sandbox-host` 或定位到 `system-control-daemon` 同级受控安装目录中的 `nlos-wasm-sandbox-host`（Unix 校验非 group/other-writable，镜像 [daemon.rs:2400-2413](../../../crates/nlos-system-control/src/daemon.rs#L2400-L2413) 权限纪律）；**默认未配置或找不到有效路径时 fail-closed**（`tool_sandbox` 保持未装配状态）。
2. **静态/签名工具注册清单与固定 `[u8; 16]` `ToolRegistration` 路由**：
   - [tool_service.rs:55-67, 270, 300](../../../crates/nlos-wasm-sandbox/src/tool_service.rs#L55-L67) 要求每次 `list_tools` / `invoke_tool` 传入 `[u8; 16]` `registration` 路由键，而 [tool_sandbox.proto:34-52](../../../schema/nlos/sabi/v1/tool_sandbox.proto#L34-L52) 无 `registration_id` 字段。
   - 在 `DaemonOptions` 新增 `--tool-registry-manifest <PATH>`（或结构化 `Vec<ToolRegistration>` 嵌入配置）：每个条目必须静态钉死 `id: [u8; 16]`、`namespace: NamespaceId`、`generation: Generation`、位于宿主机受控只读安装目录内的 `.wasm` 绝对路径以及白名单 `RegisteredTool { name, entrypoint, profile }`（`profile` 设非零 epoch deadline 且禁占 `/__nlos_input__`）。**严禁由线端请求指定任意宿主机 `.wasm` 路径**。
   - **路由解析**：单注册表配置下直接路由到唯一 `ToolRegistration.id`；多注册表配置下，须由 `AuthorityServiceAuthorizer` 选中的授权 `CapabilityRecord.target` 唯一映射到匹配 `NamespaceId` 的 `ToolRegistration`，存在零匹配或多义匹配时 fail-closed 返回 `SABI_ERROR_CODE_NOT_FOUND` / `INVALID_ARGUMENT`。
3. **可信工作区解析器与 `PreparedToolInput`（L-015 大参数协同）**：
   - Daemon 为 `BlobReferenceService` 与工具沙箱统一装配可信 `WorkspaceResolver`（基于已修复 CVE 的 `cap-std` / `cap-fs-ext` 4.0.3 受管根目录，root 来自 `--workspace-root` 等宿主可信配置，拒绝符号链接逃逸与任意路径挂载）。
   - 未完成 `BlobReference` -> `PreparedToolInput`（[tool_service.rs:71-77, 313-321](../../../crates/nlos-wasm-sandbox/src/tool_service.rs#L71-L77)）受控只读暂存 Materializer 前，Daemon 仅通过 `invoke_tool` 开放有界小载荷工具调用，严禁伪造 `PreparedToolInput`。
4. **`OperationControl` / `WaitControl` 跨通道异步控制对接**：
   - `ToolSandboxService` 的持久操作状态保存在 `Box<dyn ToolOperationStore>`（[tool_service.rs:213](../../../crates/nlos-wasm-sandbox/src/tool_service.rs#L213)），而 Daemon 的 `OperationControlService`（[operation_control.rs:20-35](../../../crates/nlos-system-control/src/operation_control.rs#L20-L35)）目前仅读写 `daemon.operations: SqliteOperationStore`。
   - 规定 `W82-R4` 在 `nlos-system-control` 提供跨通道路由桥接（例如为 `SqliteOperationStore` 适配 `ToolOperationStore` 或在 `OperationControlService` 对未知 `OperationId` 回退查询/取消挂载中的 `ToolSandboxService::query_operation` / `cancel_operation`，同时复用当前请求的 `CallBinding` 触发 `ServiceAuthorizer` 对 `ServiceMethod::QueryToolOperation` / `CancelToolOperation` 的重授权，并向 `WaitAuthority` 投递终态唤醒事件）。在跨通道取消链路真实接线并获集成测试证明前，严禁对外宣称 `ToolSandbox` 支持客户端 `OperationControl.cancel`。
5. **默认 fail-closed 护栏与 macOS `p95 = 72.50 ms` 后续评估信号**：
   - 默认 `DaemonOptions::new(root)` 下 `wasm_sandbox_host = None` 且 `tool_registrations = vec![]`：此时若未就绪，请求 `"tool_sandbox"` 返回明确的 `SABI_ERROR_CODE_NOT_SUPPORTED` / `STATE` 失败信封（或在 `ServiceDirectory` 中仅暴露已真实装配的服务），保持零隐式执行面。
   - **性能与常驻宿主评估信号登记**：实查 [w82-b3-tool-service-20261116.md:119](../../evidence/stage-e/w82-b3-tool-service-20261116.md#L119)，在 per-call 宿主机子进程模式下 20 次热目录采样结果为 Ubuntu `p50=18.59 / p95=19.82 ms`、Windows `p50=37.53 / p95=45.42 ms`、**macOS `p50=49.00 / p95=72.50 ms`**。其中 macOS `p95` 高于 [w82-b-design-draft.md:261](../w82-b-design-draft.md#L261) 设定的 `50 ms` 门槛，已同步登记于 [del-base-coverage-audit.md:53, 162](../del-base-coverage-audit.md#L53)。本 ADR 明确 `W82-R4` 维持单次 `nlos-wasm-sandbox-host` 模型（守住 `wasi_allowlist.rs` 14 导入单点不变量与双层零共享状态安全边界），将 `macOS p95 = 72.50 ms` 作为后续 `DEL-RES`/`DEL-CAP` 专门立项评估常驻宿主方案（`NLOS-HOST-R2`）的触发信号，不构成当前挂载的阻塞条件。

---

## 6. 维护者签认矩阵（Maintainer Signoff Matrix）

维护者可按以下矩阵逐项复核；若全部按推荐采纳，只需在回复中批复 **`批准 ADR-0028: 1A/2A/3A/4`** 即可授权 `W82-R4` 启动全 8 通道实时挂载实施（或显式指定替代组合如 `1A/2A/3B/4`）：

| 决策编号 | 议题 | 推荐选项 | 备选选项 | 核心不变式与影响范围 | 解锁内容 |
|---|---|---|---|---|---|
| **决策 1**（`GAP-3`） | `ServiceAuthorizer` 与 `CapabilityRights` 权限映射 | **1A**：零 SQLite 迁移，`purpose_digest` 域隔离 + 4-bit 动作映射 | **1B**：`migrate_v4` 重建 `capability_heads` 扩位（否决） | 不破 `schema.rs:18` 约束与 N-1 库兼容；`purpose_digest = None` 既有根/语义能力严格拒绝 | 解锁全部三新服务（`llm_gateway` / `tool_sandbox` / `blob_reference`）在 Daemon 的真实授权门 |
| **决策 2**（`GAP-1`） | `BlobReference` 线端命令签名与摘要对齐 | **2A**：`blob_reference.proto` 纯增量 `command_proof`（field `9`/`3`）+ SDK 计算摘要与签名 | **2B**：改 `envelope.proto` `SabiRequestContext` 全通道透传签名（否决） | 零改动历史冻结黄金目录；保留 `blob_reference.rs:925` 拒空摘要与 `ADR-0010` 零服务端代签 | 解锁 `blob_reference.{mint,read,release}_reference` 在 Py/TS SDK 与 Daemon 的全链路联通 |
| **决策 3**（`GAP-2` Part B） | `Gateway::attenuate_budget` 子会话绑定 | **3A**：Daemon 受管注册表解析 `ChildSessionBinding` + `llm_gateway.proto` 增量字段 `4..=6` / `4..=5` | **3B**：Daemon 接入层暂维持 `attenuate_budget` fail-closed | 严禁从 `session_hint` 自授或虚构 child；未登记真实子会话时运行期 fail-closed；前置需合并 `W82-R3-GW-FIX` (Part A) | 解锁 `llm_gateway` 全 4 方法在线挂载与父子预算受控委派路径 |
| **决策 4** | `ToolSandboxService` 实时挂载与可信配置规范 | **4**：库零改动，按 `--wasm-sandbox-host` / 静态 `ToolRegistration` / `WorkspaceResolver` / `OperationControl` 桥接 / 默认 fail-closed 挂载 | 无备选（维持单次 `nlos-wasm-sandbox-host` 安全边界） | 严禁客户端指定 `.wasm` 路径；未配置保持 fail-closed；登记 macOS `p95 = 72.50 ms` 后续评估信号 | 解锁 `tool_sandbox.{list_tools,invoke_tool}` 及跨通道取消/等待实时挂载 |

---

## 7. 下一波（`W82-R4`）精确实施写集、并行规划与验证门禁

待维护者签认本 ADR 并合流同步车道 `W82-R3-GW-FIX` 后，按以下互不重叠的车道写集与屏障推进 `W82-R4`：

### 7.1 依赖拓扑与写集划分

```text
[前置屏障 B-R4-0] 维护者批准 ADR-0028 + 合流 W82-R3-GW-FIX (Part A 修复)
         │
         ▼
[Wave 1: W82-R4-PROTO-SCHEMA]  (若采 2A/3A：proto 纯增量 + manifest + nlos-schema + gen)
         │
         ├───────────────────────────────┐  (写集严格不相交，并行)
         ▼                               ▼
[Wave 2A: W82-R4-DAEMON-MOUNT]   [Wave 2B: W82-R4-SDK-PY-TS]
(nlos-system-control + 可选       (sdk/python + sdk/typescript
 gateway_wire 响应投影)            + 跨语言摘要/签名/IPC 测试)
         │                               │
         └───────────────┬───────────────┘
                         ▼
[Wave 3: W82-R4-INT-E2E] (全 8 通道实时联调 + ROAD-E-001 回归 + Evidence/台账收口)
```

| 车道 ID | 独占写集（仅列实际需改目录） | 关键交付项 |
|---|---|---|
| `W82-R4-PROTO-SCHEMA` | `schema/nlos/sabi/v1/{blob_reference.proto, llm_gateway.proto, deprecation-manifest.yaml}`、`crates/nlos-schema/**`、`gen/{python,typescript}/**`、`schema/golden/epochs/v1.1-int-20261121/**`（新建，严禁改写既有目录） | 若采 `2A`：新增 `BlobCommandScopeWire` / `BlobCommandProofWire`、`MintReferenceRequest.command_proof = 9`、`ReleaseReferenceRequest.command_proof = 3`；若采 `3A`：新增 `AttenuateBudgetRequest` 字段 `4..=6` 与 `AttenuateBudgetResult` 字段 `4..=5`；同步登记 `deprecation-manifest.yaml` 与重新生成 `gen/`（旧 20+ 历史生成文件与 `v1.0`/`v1.1`/`v1.1-blob-20261116` 黄金目录零字节漂移） |
| `W82-R4-DAEMON-MOUNT` | `crates/nlos-system-control/Cargo.toml`、`crates/nlos-system-control/src/{auth.rs, daemon.rs, directory_control.rs, operation_control.rs, bin/system-control-daemon.rs, service_auth.rs(新), llm_gateway_control.rs(新), tool_sandbox_control.rs(新), blob_control.rs(新)}`、`crates/nlos-system-control/tests/**`；另若 `3A` 在线投影：`crates/nlos-driver-llm-http/src/{gateway_wire.rs, llm_gateway.rs}`（**`crates/nlos-capability` 与 `crates/nlos-wasm-sandbox` 保持零代码改动**） | ① 实现 `AuthorityServiceAuthorizer`（方案 1A）与每会话 64 槽 LRU/pin `slot -> CapabilityId` 解析表；② `directory_control.rs` `SERVICE_OFFERINGS` 由 `5 -> 8`（按配置 fail-closed 暴露），`auth.rs` 分派扩展为 8 通道；③ `blob_control` 解码 `command_proof` 并传 `BlobCommandProof` 入 `BlobReferenceService::handle_for_ipc`；④ `llm_gateway_control` 对接 Daemon 受管子会话表并投影 `ChildBudgetBinding`；⑤ `tool_sandbox_control` 实现 `--wasm-sandbox-host` / `--tool-registry-manifest` / `WorkspaceResolver` 与 `OperationControl` 跨通道取消桥接 |
| `W82-R4-SDK-PY-TS` | `sdk/python/nlos_sdk/{_client_base.py, blob_reference.py, llm_gateway.py}`、`sdk/python/tests/**`、`sdk/typescript/src/{blob_reference.ts, llm_gateway.ts}`、`tests/conformance/ipc/**` | ① Py `_client_base.py` 开放 `proposal_or_input_digest_sha256` 传参；② Py/TS `BlobReferenceClient` 实现 `mint_input_digest` / `read_input_digest` / `release_input_digest` 与 `mint_command_message` / `release_command_message` Ed25519 签名；③ Py/TS `LlmGatewayClient.attenuate_budget` 支持可选 `child_*` 选择器并解析返回的 `child_account_id` / `child_binding_digest`；④ Rust / Python / TypeScript 对真实 8 通道 `system-control-daemon` 正负向端到端集成测试 |
| `W82-R4-INT-E2E` | `docs/evidence/stage-e/**`、`docs/management/{README.md, stage-e-progress.md, evidence-index.yaml, claims.yaml, risks.yaml}`、`crates/nlos-compat-shim` 回归复跑 | 全 8 通道联调实证、`[ROAD-E-001]` `N-1 -> N` 零重编译回归复跑、本 ADR 状态晋升评审（`PROPOSED -> ACCEPTED/VERIFIED`）及管理索引同步 |

### 7.2 `W82-R4` 强制验收门禁

1. **Schema 与黄金语料门**：
   - `npm run schema:lint`、`npm run schema:generate && npm run schema:check-generated` 零漂移；
   - `python3 tools/check_golden_immutability.py --check-git-diff 9414000f9b19179ca0922d95bbf81e21eff137ab` 退出 0（确保 `epochs/v1.0`、`epochs/v1.1`、`epochs/v1.1-blob-20261116` 与历史 7 根向量零变化）；
   - 双版本 `python3 tools/sabi-compat-diff/sabi_compat_diff.py` 实测 `compatible=true`、`0 BREAKING_VIOLATION`；
   - `cargo test -p nlos-schema` 与 `python3 -m unittest discover -s tests/conformance/schema -p "test_*.py"` 全绿。
2. **授权与安全负向门（必测红线）**：
   - **GAP-3 负向**：`purpose_digest = None` 根能力调用三新服务任意方法全部返回 `SABI_ERROR_CODE_RIGHTS`；持 `LlmGateway` 目的能力越权调用 `ToolSandbox`/`BlobReference` 被拒；持只读位（`SEMANTIC_ADJUDICATE`）调用写入/取消/委派方法被拒；伪造 `slot` 号或跨会话重放旧 `slot`/已吊销代际全部拒接。
   - **GAP-1 负向**：无 `command_proof` 触发 `MissingSignature`；篡改 `path_handle`、`recipient_principal_id`、`max_reads` 或 `scope` 字段触发验签失败；空 `proposal_or_input_digest_sha256` 或错摘要触发 `BindingMismatch`；Py/TS 与 Rust 计算同一请求输入摘要和命令签名原文逐字节相等。
   - **GAP-2B 负向**：仅填 `session_hint` 或指定未注册/已退出/未握手子进程调用 `attenuate_budget` 必须 fail-closed 拒绝、零子账户创建；真实注册子进程跨主体未持 `DELEGATE` 被拒；有效委派后父>=子预算约束在子会话实际 `llm_chat` 调用中端到端成立。
   - **决策 4 负向**：未传 `--wasm-sandbox-host` 或 `--tool-registry-manifest` 时 `ToolSandbox` 保持 fail-closed；客户端尝试注入越权工具名/非声明 `.wasm` 路径被拒；客户端经 `OperationControl.cancel` 成功终止运行中工具并收口终态，未知或跨主体取消被拒。
3. **整树构建、跨语言与治理门**：
   - `cargo fmt --all -- --check`、`cargo clippy`（`-D warnings`）、`cargo test -p nlos-system-control --features daemon`、`cargo test -p nlos-capability -p nlos-driver-llm-http -p nlos-wasm-sandbox -p nlos-compat-shim` 全绿；
   - `python3 -m unittest discover -s sdk/python/tests`、`python3 tests/conformance/ipc/sabi_v1_1_python.py`、`npm run sabi-v1-1:test:typescript`（含真实 8 通道守护进程实测）全绿；
   - `python3 scripts/run_road_e001_upgrade_matrix.py` 全绿；`python3 scripts/lint_claims.py` 与 `python3 tools/verify_stage_e_ledger_coverage.py --mode strict` 全绿、`0 ERROR`、`git diff --check` 零输出。

---

## 8. 后果、退出策略与复审触发器（Consequences, Exit Strategy & Review Triggers）

### 8.1 正面收益与显式接受的工程代价

- **正面收益**：在完全不触动 `capability_heads` 表、`SCHEMA_VERSION = 3`、`epochs/v1.0` / `epochs/v1.1` 冻结语料、`nlos-wasm-sandbox` 沙箱内核和 `nlos-capability` 核心状态机的前提下，闭合全 8 通道实时挂载所需的身份绑定、服务鉴权、线端命令签名与异步取消对接；严格落实 `w82-b-contract-20261116.md` 与 `ADR-0010`/`ADR-0011`/`ADR-0014`/`ADR-0015`/`ADR-0022`/`ADR-0027` 全套安全边界。
- **工程代价（显式接受）**：
  - `BlobReference` 的 `mint_reference`/`release_reference` 与 `LlmGateway` 的 `attenuate_budget` 引入只增 protobuf 字段（需一次 `schema:generate` 与 Py/TS/Go/C# 同步更新）；
  - `AuthorityServiceAuthorizer` 要求所有调用三新服务的会话先经 `delegate_signed` 获取绑定了非空服务 `purpose_digest` 的子能力（或在显式配置单服务 `operator_purpose_digest` 下运行），无法拿无目的限制的老根能力“裸跑”；
  - `ToolSandbox` 延续每调用 spawn `nlos-wasm-sandbox-host`，在 macOS 上承担已知约 `p95 = 72.50 ms` 的进程启动延迟。

### 8.2 退出策略

- **决策 1 退出**：若未来需扩展超过 4 位的原生能力权位或新增 `CapabilityTarget` 实体种类，可基于本 ADR 明确记录的 `purpose_digest` 映射集合编写确定性 `migrate_v4` 迁移脚本，将既有服务专用 `purpose_digest` 能力行无损翻译至新位掩码列，旧表策略保留双读过渡窗。
- **决策 2 退出**：若未来推进全控制面统一的 `SabiRequestContext` 命令签名透传（ADR-0011 Decision 2），`MintReferenceRequest.command_proof` 与 `ReleaseReferenceRequest.command_proof` 按 [ADR-0027 §2.1](./0027-stage-e-sabi-long-term-compatibility-and-store-governance.md#L66-L70) 经历 `ACTIVE -> DEPRECATED -> SUNSET` 至少两个主系统发布周期后有序退场。
- **决策 3 退出**：`AttenuateBudgetRequest` 的选择器字段均声明为可选字段；若后续演进为跨节点或独立额度凭证令牌模型，可新增字段并在 `nlos-compat-shim` 保留既有字段翻译。
- **决策 4 退出**：`ToolSandbox` 的配置接口与 `OperationControl` 桥接完全局限于 `nlos-system-control`；若后续切换为常驻 Wasm Host 池（`NLOS-HOST-R2`），仅替换宿主执行器适配层，上层 SABI `list_tools`/`invoke_tool`/`OperationControl` 线协议零改动。

### 8.3 复审触发器

1. **出现跨服务联合单能力诉求或 `call_limit` 原子消费需求**：当业务确需单张 `CapabilityId` 同时跨越多个正交服务域或复用 `capability_consumption_rows` 严格计次时，触发决策 1 复审（评估 `migrate_v4`）。
2. **ADR-0011 Decision 2 全通道统一命令签名切片立项**：触发决策 2 与 `SabiRequestContext` 对齐复审。
3. **macOS 工具调用吞吐瓶颈实证超忍受限**：当真实工作负载在 macOS 上因 `p95 = 72.50 ms`（超过 `50 ms` 门槛）引发实际调度退化且完成 `NLOS-HOST-R2` 多轮宿主协议安全论证时，触发决策 4 常驻宿主评估。
4. **跨节点/多宿主联邦 `Gateway` 或共享远程工作区开启**：当 `LlmGateway` 预算或 `BlobReference` 文件句柄跨出单机宿主机边界时，触发 [ADR-0026](./0026-cross-physical-host-transport-and-consensus-expansion.md) 与决策 2/3 联合复审。
