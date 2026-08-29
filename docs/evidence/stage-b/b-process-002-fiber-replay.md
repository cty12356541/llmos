# B-PROCESS-002：Fiber replay 最小前缀（事件溯源续跑）

- 状态：`PARTIAL_PASS`（单机 `H3`）
- 日期：2026-08-29
- Owner：TokioRuntimeAdapter / WaitAuthority
- 设计依据：[ADR-0009](../../management/adrs/0009-fiber-event-sourced-resume.md)（用户 2026-08-29 定案：事件溯源续跑为主 + 受控快照兜底）
- 关联工作包：`B-PROCESS-002`（本切片）；`B-WAIT-001`（durable 事实源）

## 1. 实现事实

- **投影**：`BindingEventProjection::project` 按 binding 从 wait registry 投影事件流（typed、注册时间序、跨 channel 保序；空流合法）。事实源暂为 wait registry 唯一含 binding 列的 authority；effect/queue 投影待各 authority 增 binding 关联列（ADR「不私加 authority」原则，登记后续切片）。
- **契约**：`ResumableBinding::resume(&BindingReplay) -> ResumePlan`——计划只 gate arming；框架在任何 arming（含 self-flip 这一唯一 durable 写）之前 fail-closed 校验（`ResumePlanMismatch`）。
- **`resume_binding`**：gate 镜像 rearm（shutdown/stale/terminal 零副作用）；PENDING 事件复用 `arm_durable_row`（高水位覆盖 → self-notify 自翻转 satisfied；否则同 key 重挂，取代语义与 rearm 一致）；WOKEN → `already_woken` 纯报告（at-least-once 保住，不消费 placeholder）；CANCELLED → 零动作；二次 resume = 幂等重放（前者 Cancelled，durable 行字段级不变）。
- **ADR-0009 第 3 条落地**：重放只覆盖 durable 交互边界，纯内部计算段不进事件流（显式语义损失，复审触发器）；重放消费幂等性由既有 durable 去重承担，replay 设施对 durable 面只读。
- **B 路径占位**：`SnapshotResumable` marker（语义镜像 B-TASK-006O），不接线；快照保留策略留实现切片。
- nlos-wait additive：`list_waits_for_binding`（owner readback、完整行校验、零 durable 副作用）。

## 2. 验证

```text
cargo test -p nlos-runtime-tokio（c5144a8 后）
  → 55 passed / 1 ignored / 0 failed（基线 45+1 无回归，新增 fiber_replay 10 项）
cargo test -p nlos-wait → 26 passed / 0 failed
cargo clippy --workspace --all-targets -- -D warnings → 0 warning / 0 error
cargo fmt --all --check → 通过
```

覆盖：投影正确性/空流、resume 满链路（重挂→notify+deliver→Woken）、已满足自翻转、WOKEN/CANCELLED 分桶、二次 resume 取代 + durable 零副作用、stale/shutdown、rearm 互操作、契约拒绝（re-drive/外部计划）零状态泄漏。

## 3. Canonical commit

- `c5144a8` feat: add fiber replay projection and resume contract

## 4. 明确未完成（PARTIAL_PASS 保持）

- effect/queue 投影（需 authority binding 关联列）；B 快照路径实现与保留策略；fiber 代次与 binding 的 durable 关联；跨进程/跨机 replay（blocked-by B-TASK-006L）。
