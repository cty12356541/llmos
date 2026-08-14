# B-TASK-008C1：sealed TaskWriteSet planned-effect binding

- 状态：`PARTIAL_PASS`
- 日期：2026-08-14
- 范围：把已由 TaskAuthority 接受的 `planned_effects` 持久化进 sealed `TaskWriteSet`，并在带 sealed root 的 `CommitPermit` issuance 时逐位复验；不等同于 planned endpoint、publication plan 或 complete TaskWriteSet。

## 结论

`TaskWriteSetRequest.planned_effects` 现在随 seal 一起进入 schema v17 的 immutable child table。每个 child 保存有序 effect descriptor 的 intent spec、stable action slot、target authority object、effect class、idempotency scope、logical effect identity、derived idempotency identity、required flag、condition/success/action digests；父行保存 canonical `effect_set_root`。写入后 update/delete 均由 immutable triggers 拒绝，重启回读会重新校验 dense sequence、derived identity 和 scalar range。

带 effect 的 sealed write set 使用 v3 write-set root，把 `effect_set_root` 纳入 canonical extended domain；没有 planned effect 的历史行继续使用原 v1/v2 domain，并以零 effect root 保持 root 兼容。迁移会检查 parent column、child table 和两个 immutable triggers 的完整性，partial schema fail closed。

当 `PermitRequest` 指定已 sealed `TaskWriteSet` root 时，TaskAuthority 会回读并校验当前 task generation、planned-effect canonical root、effect vector 和 write-set root，要求 request 与 sealed child 逐位相等；不同 effect、篡改 child、重启后的 root 漂移和幂等 replay 均被拒绝。对应 effect slot 的 canonical root 与 permit record 保持一致。

## 验证

- `cargo test -p nlos-task --test participant_registry --quiet`：11 项通过。
- `cargo test -p nlos-task --quiet`：通过，包含旧 schema fixture migration、legacy permit compatibility 和 effect permit/replay 覆盖。
- `cargo clippy -p nlos-task --all-targets --all-features -- -D warnings`：通过。
- `cargo fmt --all -- --check`：通过。
- `cargo test --workspace --all-targets --quiet`：通过。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过。

新增 participant integration test 覆盖 seal → exact permit → effect slot、不同 planned effect 的 sealed-root conflict、重启后的 child immutability、精确回读和原幂等键 replay rejection。

## 明确缺口

本切片只证明 planned effect descriptor 在 sealed write set 与带 sealed root 的 permit 之间的 durable binding。`PlannedEffect` 仍没有 per-effect operation/driver/channel endpoint 或 reservation linkage；Semantic publication/Durability receipt、Artifact publication plan、Resource activation/consume/finalize、phantom/range serializability、跨 authority prepare→activate、term takeover、attestation 和宿主 enforcement 仍未接入。因此 `complete TaskWriteSet` 仍未完成。

为保留 B-TASK-002/003 已验证的历史兼容契约，legacy `PermitRequest` 若没有匹配 sealed write-set row，仍可走旧的 planned-effect path；本切片只对命中的 sealed root 强制 exact effect binding。这一兼容边界不是新的权威 seal，后续 `B-TASK-008C2` 必须决定并实现 planned endpoint/publication 的完整入口与迁移策略。
