# B-TASK-006P：共享 nominal identity spine

> 状态：`PARTIAL PASS`　　日期：2026-08-09
>
> 对应：`[MODEL-ID-001]`、`[TASK-WRITE-002]`、[ADR-0005](../../management/adrs/0005-task-write-set-authority-first.md)

## 已实现事实

1. `nlos-types` 补齐 v0.5 TaskWriteSet 前置链需要的共享 identity：Process/Agent/Isolation、Task/Effect、Intent/Control、Context/Namespace/Capability、Resource/Lease/Reservation、Channel/Device/Driver 与 Key 使用 16-byte nominal object ID；`SemanticEventId` 按 §16.1 的 SHA-256 EventId 公式使用独立 32-byte identity。
2. `TaskGroupId`、`EffectSlotId`、`EffectPermitId` 从 `nlos-task` 的 crate-local 定义迁移为共享类型，消除同名但不可互换的第二类型源。
3. `nlos-task` 从 crate root 重新导出上述共享类型，既有调用点和 integration tests 无需改写，证明公共 Rust 导入路径保持兼容。
4. 所有新增 ID 具备固定宽度 round-trip、按类型名区分的 deterministic debug 表达、`Eq/Ord/Hash`，且不泄漏 SQLite、runtime 或 OS handle；object identity 与 content-derived event identity 不混为同一宽度。

## 验证

- `cargo fmt --all -- --check`
- `cargo check --workspace --all-targets`
- `cargo test -p nlos-types -p nlos-task`
- `cargo clippy --workspace --all-targets -- -D warnings`

新增单元测试覆盖 TaskWriteSet 代表性 16-byte binding ID 及 32-byte `SemanticEventId` 的稳定字节/类型表达；`nlos-task` 既有 TaskGroup/Effect integration tests 继续通过，覆盖共享类型替换后的 API 兼容性。

## 边界

本证据只证明 Rust workspace 内共享 typed identity 的 H3 本地基线，不证明对应对象的 authority 已实现。Process/IsolationDomain、Semantic/Resource/Driver、participant registry 仍须分别提供 durable fact、generation/CAS、冲突/replay 和重启 Evidence。SABI schema 与多语言 SDK 尚未冻结或生成这些新增身份；`LogicalEffectId`、内容摘要等其他 32-byte content identity 仍由各自 authority 定义。完整 TaskWriteSet 仍未实现。
