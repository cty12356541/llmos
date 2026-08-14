# B-TASK-008C2A：planned effect owner endpoint binding

- 状态：`PARTIAL_PASS`
- 日期：2026-08-14
- 范围：把 planned EffectSlot 声明的 Artifact/Semantic/Process/Driver/Resource owner endpoint 通过 authority readback 接入 sealed TaskWriteSet；不等同于 publication plan、operation activation 或 complete TaskWriteSet。

## 结论

`TaskWriteSetEffectEndpointRequest` 只允许声明有 owner authority 的 endpoint：Artifact head、Semantic admission、Process binding、Driver gateway 和 Resource ledger。TaskAuthority 在 seal 期间直接读取对应 authority 的 participant proof，并对 Process/Driver/Resource 额外校验 TaskAttempt 或 owner generation；caller 不能注入 participant identity、generation 或 admission Receipt。

每个 proof 持久化在 schema v18 的 immutable `task_write_set_effect_endpoints` child table，按 `effect_seq` 与 endpoint kind/object identity 唯一定位；父行保存 `effect_endpoint_set_root`。v18 migration 检查 parent column、child table 和两个 immutable triggers，partial schema fail closed。endpoint root 进入 v4 write-set root；v0–v17 历史写集保持零 endpoint root 和原有 v1/v2/v3 root domain。

seal 只接受已在同一 OPEN participant registry 中预注册的 endpoint。Permit issuance 命中 sealed root 时重新计算 endpoint root，并确认每个 endpoint proof 仍是 frozen registry 的成员；registry freeze 后不能在 permit 背后扩展集合。重启回读会校验 effect sequence、endpoint kind/object uniqueness、proof root 和 planned effect range。

## 验证

- `cargo test -p nlos-task --test participant_registry --quiet`：11 项通过，覆盖 Artifact、Process、Driver、Resource、Semantic 五类 endpoint proof。
- `cargo test -p nlos-task --quiet`：通过，包含历史 schema 迁移和既有 effect/publication 回归。
- `cargo clippy -p nlos-task --all-targets --all-features -- -D warnings`：通过。
- `cargo fmt --all -- --check`：通过。
- `cargo test --workspace --all-targets --quiet`：通过。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过。

Artifact endpoint 测试还验证了重启后的精确回读及 child update/delete immutable triggers；不同 owner generation、未提供对应 authority 或 effect sequence 越界均 fail closed。

## 明确缺口

本切片没有把 endpoint proof 扩展为 per-effect `ActionId`/`OperationId`/Driver invocation、Reservation activation/consume/finalize 或 Channel/Topic endpoint；Semantic target scope、Admission/Durability Receipt、Artifact publication plan/receipt 也仍未进入 TaskWriteSet。每个 planned effect 是否必须至少有一个 endpoint 的完整业务策略尚未冻结，当前只校验已声明 endpoint 的 owner fact 与 registry membership。

legacy `PermitRequest` 在没有匹配 sealed write-set row 时仍保留 B-TASK-002/003 的 planned-effect 兼容路径；本切片只对命中 sealed root 的 endpoint set 强制 exact binding。下一切片 `B-TASK-008C2B` 处理 Artifact/Semantic publication plan binding 与跨 authority publication 收口。
