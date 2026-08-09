# B-TASK-006P：共享 nominal identity spine

> 状态：`PARTIAL PASS`　　日期：2026-08-09
>
> 对应：`[MODEL-ID-001]`、`[TASK-WRITE-002]`、[ADR-0005](../../management/adrs/0005-task-write-set-authority-first.md)

## 已实现事实

1. `nlos-types` 补齐 v0.5 TaskWriteSet 前置链需要的 16-byte nominal identity，包括 Process/Agent/Isolation、Task/Effect、Intent/Control、Context/Namespace/Capability、Resource/Lease/Reservation、Channel/Device/Driver、Semantic 与 Key 身份。
2. `TaskGroupId`、`EffectSlotId`、`EffectPermitId` 从 `nlos-task` 的 crate-local 定义迁移为共享类型，消除同名但不可互换的第二类型源。
3. `nlos-task` 从 crate root 重新导出上述共享类型，既有调用点和 integration tests 无需改写，证明公共 Rust 导入路径保持兼容。
4. 所有新增 ID 复用同一 nominal contract：16-byte round-trip、按类型名区分的 deterministic debug 表达、`Eq/Ord/Hash`，且不泄漏 SQLite、runtime 或 OS handle。

## 验证

- `cargo fmt --all -- --check`
- `cargo check --workspace --all-targets`
- `cargo test -p nlos-types -p nlos-task`
- `cargo clippy --workspace --all-targets -- -D warnings`

新增单元测试覆盖 TaskWriteSet 代表性 binding ID 的稳定字节/类型表达；`nlos-task` 既有 TaskGroup/Effect integration tests 继续通过，覆盖共享类型替换后的 API 兼容性。

## 边界

本证据只证明 Rust workspace 内共享 nominal identity 的 H3 本地基线，不证明对应对象的 authority 已实现。Process/IsolationDomain、Semantic/Resource/Driver、participant registry 仍须分别提供 durable fact、generation/CAS、冲突/replay 和重启 Evidence。SABI schema 与多语言 SDK 尚未冻结或生成这些新增身份；`LogicalEffectId`、内容摘要等 32-byte content identity 也不属于本次 16-byte nominal ID 扩展。完整 TaskWriteSet 仍未实现。
