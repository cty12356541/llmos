# B-TASK-008B1：Process/AgentInstance/IsolationDomain owner binding

- 状态：`PARTIAL_PASS`
- 日期：2026-08-13
- 范围：把 Process authority 的当前执行 binding 接入 B-TASK-008A 的 snapshot/read-set seal；不等同于完整 TaskWriteSet 或完整 BirthDecision。

## 结论

`nlos-process` 新增 `inspect_binding_endpoint_proof`。它先通过既有 active Process readback 验证 Process generation/fencing token 与 IsolationDomain generation/fence，再返回 owner-derived participant identity、participant generation 和 admission Receipt。participant identity 稳定绑定 ProcessId；Process restore 会推进 generation/fence 并产生新的 admission Receipt；旧 domain/process binding 的 proof readback fail closed。

`SqliteTaskAuthority::register_process_binding_participant` 直接读取并检查 Process binding 的 TaskId、AttemptId、Attempt generation 和 expected Process generation，然后以 owner proof 更新 OPEN participant registry。`seal_task_write_set_with_process_authority` 再次逐字段调用 ProcessAuthority verify，检查 owner record 属于当前 TaskAttempt，并要求对应 Process endpoint 已经存在于 registry；seal 不会在 registry 背后自动扩集合。

带 Process binding 的 `TaskWriteSetRecord` 保存 Process/AgentInstance/IsolationDomain identity、generation/fencing token 与 owner participant proof。schema v14 使用 immutable child table 持久化该 binding；schema v15 将既有 participant type check 从 1–6 原子迁移为 1–7，逐字复制历史 participant rows，不为历史写集补造 Process 事实。Process binding 字段与 snapshot/head/group/participant/artifact roots 一同进入 canonical write-set root；同 key 同 bytes 可 replay，异 binding 或 owner readback 漂移 fail closed。

## 验证

- `cargo test -p nlos-process -p nlos-task --quiet`：通过；覆盖 endpoint proof replay、IsolationDomain rotation fence、Process restore generation fence、Task registry pre-registration、TaskWriteSet seal/replay/restart。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过。
- 旧 schema fixtures（v1–v13）迁移到 schema v15 的 user_version 与既有回归测试通过。

## 明确缺口

本切片仍是单节点 reference authority：没有完整跨 authority BirthDecision/prepare→activate、Channel endpoint、Semantic admission/control、Resource/Reservation read set、planned effect/publication、跨 term takeover/attestation 或真实宿主 Process enforcement。下一切片为 B-TASK-008B2 Semantic admission 与 Resource/Reservation binding。
