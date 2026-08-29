# ADR-0010：Capability 命令签名化（principal 公钥验签接入 issue/delegate/revoke）

- 状态：ACCEPTED
- 日期：2026-08-29
- Owner：CapabilityAuthority / IdentityAuthority
- 关联 Requirement：总纲 v0.5 行 219、`[SEM-TXN-002]` 前置、重授权签字链（讨论 10）
- 关联工作包：`B-CAPABILITY-001`（扩展切片「签名命令」）、`B-IDENTITY-001`（验签消费方）
- 决策来源：用户于 2026-08-29 在四个候选切片中选择「签名 Capability 命令」（Capability-001 证据 §4 自declared 下一门的基础件）
- 复审触发器：AuthorityClock 接入（validity 追溯撤销）；Namespace hierarchy narrowing；IPC peer authentication（B-TASK-006L）落地后的远程签名传输

## 上下文

`B-CAPABILITY-001` 的 issue/delegate/revoke 目前是可信 TCB API：任何持有 authority 句柄的进程内调用者都能改写 capability 图，无身份证明。`B-IDENTITY-001` 已落地最小 principal/key authority 与真实公钥验签（`verify_semantic_signature` 风格），但未被 Capability 消费。两个证据文件的下一验收门共同指向：命令签名化。

## 决定

1. **命令必须携带行为方 principal 的签名**：`issue_root` / `delegate` / `revoke` 新增签名变体——签名消息为域分隔摘要（镜像 `semantic_signature_message` 风格），覆盖命令全部语义字段（capability 身份、scope、rights、call-limit、validity、delegate 目标、idempotency key、时间戳）；验签经 IdentityAuthority 按 behavior 方 principal 当前 key 绑定执行（generation 绑定，revoked key 一律 fail-closed）。
2. **additive + 弃用**（镜像 channel gate 先例）：新签名变体为强制边界；无签名 TCB 入口保留并 `#[deprecated]`（安全语义上不应使用，弃用登记移除为 future breaking change）。
3. **失败语义**：签名不匹配/未知 principal/key 已撤销 → typed fail-closed（`SignatureInvalid`/`PrincipalUnknown`/`KeyRevoked`），在任何 durable 写之前拒绝、零部分状态；replay 路径不重验签（durable 行为权威，镜像 wait/notify replay 先例）。
4. **显式不做**：IPC 传输与远程签名（blocked-by B-TASK-006L，本 ADR 只闭环进程内信任面）；HSM/Keychain custody；AuthorityClock（validity 仍用上层传入时间）。

## 后果与退出策略

- nlos-capability 新增 nlos-identity 依赖（authority 间单向：Capability 消费 Identity 验签，方向与 hierarchy 一致）。
- 信任面收窄：capability 图变更从「有句柄即可信」变为「有句柄 + 行为方私钥证明」；TCB 面收缩到 IdentityAuthority 的 bootstrap 与 key custody。
- 退出：签名方案若被证伪（如消息覆盖面不足被绕过），以补记修订消息构造并登记 migration；不重写历史。
