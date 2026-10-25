# ADR-0024：Driver Provider 线级载荷 Codec 晋升为 ADR-0014 冻结通道（`nlos.sabi.DriverProvider` v1.0）

- 状态：`ACCEPTED / VERIFIED`（2026-10-25，`W65-CPROVIDER-REAL` 落地并验证）
- 日期：2026-10-25
- Owner：`nlos-schema` REGISTRY / `nlos-driver-mock` / `#9 C-PROVIDER-REAL`
- 关联 Requirement：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §25.1 `[TASK-PROVIDER-001]`、§28.1 `[ROAD-A-003]`、§28.2 `[ROAD-B-003]`；兼容规则承 [ADR-0003](0003-stage-b-idl-and-canonical-encoding.md) 与 [ADR-0014](0014-schema-channel-freeze-v1-beta.md)（`COMPAT-VER-001`、`COMPAT-VER-002`、`COMPAT-DEPRECATE-001`）
- 关联工作包与风险项：阶段 B → 阶段 C 移交项 `#9 C-PROVIDER-REAL`（`W65-CPROVIDER-REAL`）；关闭 `RISK-B-05 review_point`（driver codec 入通道决策）与阶段 B 退出未知项 `U-10`（真实 provider 载体接入、降级与隔离证明行为验证）
- 复审触发器：跨语言外部第三方 provider 插件（Go/C#/Python/TS）直接实现 `nlos.sabi.DriverProvider` 服务端；远端公有云 HTTP/gRPC LLM API 适配层引入流式 token 分片通道；出现需要破坏性字段变更的 v2 需求

## 上下文

阶段 B 在 `W30-B` 与 `W30-C` 构建 `nlos-driver-mock` 时，为避免在 provider 恢复/围栏语义未钉死前过早冻结协议（呼应议题 31 与 `RISK-B-05`），刻意采用 crate 内部确定性 `minicbor` 映射编码（`nlos.driver-mock.payload` v1）承载 SABI `Envelope.payload`，并在 [exit-unknown-risks.md](../exit-unknown-risks.md) 与 [stage-c-progress.md](../stage-c-progress.md) 中留下两项显式移交门：

1. `RISK-B-05 review_point`：待操作 prepare→activate→complete 持久围栏与缓存降级语义稳定后，对 driver payload codec 是否进入 [ADR-0014](0014-schema-channel-freeze-v1-beta.md) 冻结注册表作出正式裁定；
2. `U-10` / `#9 C-PROVIDER-REAL`：将进程内确定性 mock 推进为真实跨进程 OS 子进程 provider 工作进程（跨平台 Unix domain socket 与 Windows Named Pipe + [ADR-0011](0011-ipc-principal-auth-signature-passthrough.md) challenge-response 认证），并落地总纲 §25.1 `[TASK-PROVIDER-001]` 的租户/会话/缓存/数据保留隔离保证与 attestation 校验（任一保证缺失或零摘要必须自动标记回执为 `PROVIDER_ISOLATION_UNVERIFIED`）。

当前 `W30-B/C` 持久操作围栏、纪元取消仲裁与缓存观测恢复均已验证闭环，独立子进程 `nlos-provider-worker` 需要跨地址空间与宿主驱动层交换自描述、有界且受注册表版本门控的标准线级载荷，推迟入表的历史前提已消除。

## 候选

| 候选 | 结论 |
|---|---|
| A. 晋升为第 8 个 canonical SABI protobuf 通道 `nlos.sabi.DriverProvider`（`v1.0`, `frozen: true`），同时保留 `nlos-driver-mock::codec`（`minicbor`）作为向后兼容垫片 | **采纳** |
| B. 继续以 crate 私有 `minicbor` 编码承载跨进程 `nlos-provider-worker` 载荷 | 否决：游离于 `nlos-schema` 统一 `SchemaIdentity`、critical-extension fail-closed 校验与 golden 字节锁之外，无法形成稳定的跨进程/跨语言 SABI 通道契约 |
| C. 入表 `nlos.sabi.DriverProvider` 的同时立即删除 `nlos-driver-mock::codec`（`minicbor`） | 否决：违反 [ADR-0014](0014-schema-channel-freeze-v1-beta.md) 与 [ADR-0011](0011-ipc-principal-auth-signature-passthrough.md) 的 additive 兼容过渡纪律，无收益地破坏既有 `W30-B/C` 回归测试调用方 |

## 决定

1. **新增第 8 个 canonical SABI 注册表冻结通道 `nlos.sabi.DriverProvider`（`major: 1, minor: 0, frozen: true`）**：
   - 新增 `schema/nlos/sabi/v1/driver_provider.proto`（`package nlos.sabi.v1;`），在 `nlos-schema` 注册 `SABI_DRIVER_PROVIDER_SCHEMA = "nlos.sabi.DriverProvider"`，单帧载荷上限 `MAX_DRIVER_PROVIDER_PAYLOAD_BYTES = 16 * 1024`（16 KiB）。
   - 既有 7 个注册表条目（6 个 `frozen: true` 通道及开放态 `nlos.sabi.PrincipalHandshake`）的 descriptor 与 wire golden 字节保持 100% 零改动。
2. **有界消息面与 fail-closed 解码门控**：
   - `DriverProviderRequest`：`SchemaIdentity schema` + `oneof request { register, dispatch, complete, cancel, query_isolation }`；
   - `DriverProviderResponse`：`SchemaIdentity schema` + `oneof response { register_ack, dispatch_ack, complete_ack, cancel_ack, isolation_report }`；
   - `nlos-schema` 提供 `encode_driver_provider_request` / `decode_driver_provider_request` / `encode_driver_provider_response` / `decode_driver_provider_response`，统一强制 16B 实体/回执/回调 ID、32B 种子/摘要宽度、非零代次（generation > 0）、显式枚举值（`0 = UNSPECIFIED` 一律拒绝）、终态回执存在性不变量与 `provider_profile` 非空无 NUL 边界。
3. **`[TASK-PROVIDER-001]` 线级证明与客户端独立重验**：
   - 定义 `ProviderIsolationAttestationWire`（`tenant_isolated`、`session_ephemeral`、`cache_partitioned`、`zero_data_retention`、32B `attestation_digest`、`provider_profile`），随 `complete_ack` 与 `isolation_report` 返回。
   - 客户端驱动层（`proto_codec` / `OutOfProcessProviderDriver`）不信任字符串标签，独立核验四项布尔保证与非全零 32B `attestation_digest`：全部满足时产出 `VerifiedEndToEndIsolated`，任一为 `false` 或摘要全零时强制产出 `ProviderIsolationUnverified` 并在执行回执上打标 `PROVIDER_ISOLATION_UNVERIFIED`。
4. **保留 `minicbor` 历史兼容垫片**：
   - `nlos-driver-mock::codec`（`nlos.driver-mock.payload`）与既有 `ipc::MockDriverService` 原地保留作为 legacy compatibility shim，既有测试与调用方零破坏；
   - 真实跨进程载体 `nlos-provider-worker` 与 `OutOfProcessProviderDriver` 全链路仅使用冻结的 `nlos.sabi.DriverProvider` protobuf 通道。

## 后果与退出策略

- **正面**：闭合 `RISK-B-05 review_point` 与 `U-10`；provider 跨进程协议纳入 `ADR-0014` 统一冻结纪律（此后仅允许 additive 新增字段号或新消息类型，禁改号/删字段/改语义）并具备确定性 golden 字节锁；真实 OS 子进程 provider 在 Windows Named Pipe 与 Unix Socket 双平台共享逐字节一致的认证与业务载荷契约。
- **代价**：`nlos-driver-mock` 短期并存 `codec`（legacy CBOR shim）与 `proto_codec`（canonical frozen protobuf）两套编解码模块；兼容垫片不新增特性，待未来大版本清理窗口再行退役。
- **验证与证据**：见 [C-PROVIDER-001 证据文档](../../evidence/stage-c/c-provider-001-real-process-provider-codec.md)（`nlos-schema` 兼容性与 golden 测试 + `nlos-driver-mock` 真实 `nlos-provider-worker` 子进程 7 场景跨进程集成矩阵）。
- **退出**：若未来远端流式大模型推理需要分块流式帧或破坏性载荷变更，须按 `ADR-0014` 立新 ADR 增开 additive 消息臂或 `major = 2` 并行通道，不得原地篡改 `nlos.sabi.DriverProvider` v1 冻结字节。
