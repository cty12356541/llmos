# B-CAPABILITY-003：Namespace hierarchy narrowing（最小前缀）

> 状态：`PARTIAL_PASS`
>
> 日期：2026-09-04
>
> 基线：HEAD `4a53b2a`；前序证据 `b-capability-001`（§4）、`b-capability-002`（§6）

## 1. 验收目标

关闭 B-CAPABILITY 未决项「Namespace hierarchy narrowing」的最小前缀，对齐 v0.5
`[CAP-ATTEN-001]` 与议题 14「命名即权限」：delegate 只能 **narrow** Namespace
scope；Semantic admission（`authorize_semantic` / `consume`）允许 requested target
等于 capability target 或位于其子树内；拒绝 scope amplification；Task target
保持 exact match。

不在本前缀范围内：call-limit 重做、跨进程认证入口、Namespace authority 派生
API、additive hierarchy 表、故障矩阵。

## 2. 层级编码（可机械验证）

选定：**16 字节 `NamespaceId` 的 zero-padded 前缀路径**（authority 未来派生时可
把 parent 域分隔前缀写入 leading bytes；本 crate 只验证前缀关系，不新增派生 API）。

规则：

- `namespace_prefix_len(id)` = 最后一个非零字节之后的长度（全零 = 根，长度 0）；
- `child` 在 `ancestor` 子树内 ⟺ `child[0..prefix_len(ancestor)] ==
  ancestor[0..prefix_len(ancestor)]`；
- `CapabilityTarget::Task` 不参与层级，仍要求字节级精确相等；
- 跨 kind（Namespace↔Task）一律 fail-closed。

该编码可 const 验证、无额外存储、与既有 16 字节 nominal `NamespaceId` 兼容。

## 3. 实现事实

`crates/nlos-capability/src/namespace.rs` 新增层级谓词；`delegate_impl` 将
`request.target != parent.target` 替换为 `target_is_within(child, parent)`；
共享 `admit_semantic` 将 `target != record.target` 替换为
`target_is_within(requested, granted)`。无 schema 迁移。

## 4. 验证证据

新增 `crates/nlos-capability/tests/namespace_hierarchy.rs` 5 项 integration tests：

1. delegate 将 Namespace target 收窄到 parent 前缀子树并成功落库；
2. delegate 向 sibling/wider prefix 尝试放大 → `ScopeAmplification`；
3. 宽 capability + 窄 requested target 的 authorize/consume 成功；越界 sibling →
   `TargetMismatch`；
4. 收窄 delegate 的 idempotency replay 跨重启逐字节相等；
5. Task target delegate/authorize 仍要求 exact match，与 Namespace 层级共存。

`src/namespace.rs` 另含 2 项单元测试（prefix_len、within 关系）。

本地验收命令与结果（全部实跑）：

| 命令 | 结果 |
| --- | --- |
| `cargo test -p nlos-capability` | PASS：32 passed / 0 failed（hierarchy 5 + unit 2 + ledger 7 + authority 8 + signed 10） |
| `cargo clippy -p nlos-capability --all-targets -- -D warnings` | PASS |
| `cargo fmt -p nlos-capability -- --check` | PASS |

写集：`crates/nlos-capability/src/{lib,namespace}.rs`、
`crates/nlos-capability/tests/namespace_hierarchy.rs`、本证据文件。

## 5. 已知限制

- **编码约定未强制派生**：crate 只验证前缀关系；若 caller 提交非 zero-padded
  随机 16 字节，层级语义退化为「全路径 16 段」leaf，仅 exact/前缀偶然重合可过；
  权威 Namespace 派生 API 仍属后续工作；
- **无 hierarchy 表**：不支持非前缀的 additive 父子边；
- **Task 无子树**：TaskId 保持 exact match；
- 无 kill-9/torn-write/ENOSPC 故障矩阵、无三平台 CI；不得外推为分布式 MAC 或
  硬件掉电保证。

## 6. B-CAPABILITY 未决项状态增量

- 「Namespace hierarchy narrowing」：**最小前缀已关闭**（本证据 `PARTIAL_PASS`）；
- call-limit 消耗账本：已由 `b-capability-002` 关闭（不变）；
- 跨进程认证入口：未变，仍开放（blocked-by B-TASK-006L）；
- AuthorityClock：未变，仍开放。
