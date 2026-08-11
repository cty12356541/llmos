# B-TASK-007C1：Driver gateway / Resource-Ledger endpoint proofs

## 1. 验收对象

本切片为 v0.5 `[DIST-TASK-001]` 的 planned EffectSlot participant coverage 建立 ResourceAuthority 前置事实：Driver gateway 与 Resource/Ledger endpoint 必须具有 owner-assigned、durable、generation-fenced 且可精确回读的 participant proof，TaskAuthority 后续不得接受 caller 自报 tuple。

## 2. 实现事实

- Resource schema v2 新增 immutable `driver_gateway_identities`、逐 Driver generation 的 `driver_gateway_endpoint_proofs` 与逐 ResourceAccount 的 `resource_ledger_endpoint_proofs`。
- Driver 注册在同一 transaction 分配稳定 `TaskParticipantId` 并为 generation 1 写 endpoint Receipt；Driver rotation 在写新 generation/fencing token 的同一 transaction 复用稳定 participant ID、推进 participant generation 并分配新 Receipt。
- ResourceAccount 创建在同一 transaction 分配 Resource/Ledger participant identity、generation 1 与 Receipt。
- typed inspect API 只返回当前 Driver generation proof或指定 account proof；unknown object、缺失/损坏 proof 与 storage failure 均 fail closed。
- v1→v2 migration 在一个事务内为全部既有 Driver generation 与 ResourceAccount 回填 proof。完整新结构配旧版本号只重盖版本；部分 table/trigger/coverage 拒绝打开。
- endpoint identity/proof 表由 storage trigger 禁止 UPDATE/DELETE；proof 跨重启逐位稳定。

## 3. 验证

```text
cargo test -p nlos-resource
cargo clippy -p nlos-resource --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Resource integration tests 从 5 项增至 7 项并全部通过。新增测试覆盖 authority assignment、Driver rotation generation/Receipt、stable participant identity、Resource/Ledger proof、restart、DDL immutability 与真实 v1 data migration coverage；全 workspace check/test/Clippy 零失败、零警告。

## 4. 证据等级与限制

结论：`PARTIAL PASS / H3 local Resource endpoint-proof baseline`。

- proof 尚未由 TaskAuthority 直接回读和注册，尚无 registry CAS/freeze/permit binding 证据。
- Resource/Ledger 当前以 reference `ResourceAccount` endpoint 表示；尚无完整多维 Ledger、consume/finalize/refund、ControllerBinding 或跨 Cell route generation。
- 没有 endpoint signature/attestation、TaskAuthorityAssignment term/lease、takeover barrier Receipt 或真实 Driver enforcement shim。
- Channel/Topic endpoint 仍不存在；本切片不伪造未实现 authority。

下一验收门：`B-TASK-007C2` 让 TaskAuthority 直接回读 ResourceAuthority，为 Driver gateway 与 Resource/Ledger 执行 OPEN registry generation/root CAS registration，并验证 stale Driver generation、replay、freeze 与 restart。
