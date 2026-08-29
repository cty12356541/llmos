# ADR-0011：跨进程 Principal 认证采用签名贯穿并纳入 AuthorityClock

- 状态：ACCEPTED
- 日期：2026-08-29
- Owner：IdentityAuthority / CapabilityAuthority / B-TASK-006L
- 关联 Requirement：总纲 v0.5 §7.3 `[CAP-REVOKE-001]`（revoke MUST 经 generation/revocation epoch 立即失效旧 handle）
- 关联工作包：`B-TASK-006L`（本决策实现归属）；`B-IDENTITY-001`（principal/key authority 与验签供给）；`B-CAPABILITY-001`（签名命令远程传输消费方）；`B-WAIT-001`（跨 Principal 尾部）
- 决策来源：用户于 2026-08-29 决策会话在三候选（签名贯穿/会话票据/OS 凭证）中选择「签名贯穿」，并选择将 AuthorityClock 纳入本次范围
- 复审触发器：IPC 验签性能 benchmark 不可接受 → 会话票据 additive 叠加；跨机/多机 attestation（Stage C）；key custody 升级需求（HSM/Keychain）；AuthorityClock 与外部时间源对齐需求

## 上下文

三个 IPC 服务（SystemControl、TakeoverControl、WaitControl）目前均为本地信任域传输前缀：authorizer 为可注入占位，任何能连上 Unix socket / Windows named pipe 的 peer 即被信任。阶段 B 权威进度单中 10+ 工作包的验收尾部把本项留作未决（真实 Capability/Principal 认证、principal-level peer attestation），是当前横向控制面最长的 blocked-by 尾部。ADR-0010 的复审触发器明文包含「IPC peer authentication（B-TASK-006L）落地后的远程签名传输」，其显式不做项「IPC 传输与远程签名」待本决策闭环。供给侧，`B-IDENTITY-001` 已交付最小 principal/key authority（Ed25519 密钥对、generation 绑定、撤销、`verify_semantic_signature` 风格验签），但尚未被任何 IPC 层消费。此外，validity/anti-replay 的时间语义仍用「上层传入时间」占位（ADR-0010 显式不做项），需要本地可信时钟权威同批落地。

## 候选

| 候选 | 结论 |
|---|---|
| A. 签名贯穿（连接级 challenge-response + 命令级签名） | **采纳** |
| B. 会话票据（认证后签发短 TTL 票据，后续命令出示票据） | 否决：TTL 窗口内撤销不即时，与总纲 §7.3 `[CAP-REVOKE-001]` capability 即时撤销规则有张力；且新增 TTL 期限/续期/撤销传播决策面 |
| C. OS 凭证（Unix `SO_PEERCRED` / named pipe 进程身份） | 否决：进程 ≠ principal——一 principal 多进程/跨 OS 用户场景不成立；仅够 dev 占位 |

## 决定

1. **连接级 challenge-response**：服务端下发 nonce，客户端以 principal Ed25519 私钥签名应答；服务端经 IdentityAuthority 按 principal 当前 key generation 验签，`KeyRevoked` / `PrincipalUnknown` 一律 fail-closed，拒绝建立连接。
2. **命令级签名贯穿**：ADR-0010 的签名命令消息构造 wire 化——每条变更命令携带域分隔签名（覆盖命令语义字段与 idempotency key）；replay 不重验签（durable 行为权威，镜像 wait/notify replay 先例）。
3. **AuthorityClock 纳入本次范围**：本地单调时钟权威，为 validity/anti-replay 提供时间语义，取代「上层传入时间」占位——ADR-0010 显式不做项「AuthorityClock」就此关闭；crate 归属在实现切片定。
4. **additive + 弃用**（镜像 channel gate 先例）：本地信任域入口保留并 `#[deprecated]`，弃用登记移除为 future breaking change。
5. **显式不做**：会话票据（复审触发器驱动，如引入则 additive 叠加）；跨机 attestation（Stage C）；HSM/Keychain key custody。

## 后果与退出策略

- 解锁：`B-WAIT-001` 等 10+ 工作包的跨 Principal 验收尾部（见[阶段 B 权威进度单](../stage-b-progress.md)）；fiber 跨进程 replay（[ADR-0009](0009-fiber-event-sourced-resume.md) 复审触发器 1）；[ADR-0010](0010-signed-capability-commands.md) 签名命令的远程签名传输。
- 新增 durable 面：AuthorityClock 成为新 authority（本地单调时间源）；若实现发现需要持久化 epoch/offset 等事实，在 `B-TASK-006L` 所属切片与 Evidence 登记，不私加 authority。
- 代价：每条变更命令一次 Ed25519 验签——本地 IPC 约 50μs 级，显式接受；若 benchmark 证伪，经复审触发器以会话票据 additive 叠加（不替换签名贯穿的认证根基）。
- 退出：若签名贯穿在实现中被证伪（如 challenge-response 握手缺陷、签名覆盖面不足），以补记 ADR 修订——不重写历史；弃用期保留的本地信任域入口为回退路径。
