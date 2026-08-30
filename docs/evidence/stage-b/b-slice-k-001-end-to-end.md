# B-SLICE-K-001：第一纵切面端到端（signed Package → … → inspect）

- 状态：`PARTIAL_PASS`（单机单进程、进程内 inspect、演示级纵切面首次贯通）
- 日期：2026-08-30
- Owner：`crates/nlos-slice-k`（纯组装 crate，不含任何权威语义）
- 设计依据：[管理 README §12](../../management/README.md) 第一纵切面定义；议题 31「只有纵切面能证明 P5 差异」首次实体化；组装纪律：只用已落地公开 API，不发明新语义
- base HEAD：`bb93e2f`（CI 三平台绿基线）

## 1. 实现事实

**写集**：`crates/nlos-slice-k/**`、根 `Cargo.toml`（members 一行）、本 evidence。其余 crate 零改动（已落地权威只消费；并行车道写集未触碰）。

**`SliceKRuntime`（组装器，`src/runtime.rs`）**：一个 `open(root)` 构造并持有全部纵切面权威——`IdentityAuthority`（identity/）、`ArtifactStore`（artifacts/）、`ApplicationAuthority`（applications/）、`SqliteTaskAuthority`（tasks.sqlite3）、`AuthorityClock`（clock/）、`SqliteOperationStore`（operations.sqlite3）。它只做两件事：固定子路径命名 + 一次性打开；所有权威保持各自 schema、WAL/FULL 硬校验与 fail-closed 语义。重开同一 root 即崩溃恢复入口。附带 `inspect_chain`（从各权威直读的进程内 inspect 视图，CLI/NL 的 in-process 替身，选后者是为跨平台——无 Unix socket 门控需求）。

**纵切面逐步 → 落地 API 与 Receipt**（组装逻辑在 `src/package.rs` / `src/fiber.rs` / `src/chain.rs`，测试与演示 bin 共用同一组合代码）：

| 步骤 | 组合的已落地 API | 产物（Receipt 摘要，demo 实跑输出） |
|---|---|---|
| 1. 签包 | `nlos-identity::bootstrap_principal`（Ed25519 publisher，key validity `[0, i64::MAX]` ms 以兼容真实 wall 时间戳）+ `ArtifactStore::create_artifact/put_revision` + `package_manifest_message` + `ed25519-dalek` 签名（签名者侧 fixture，先例即 nlos-artifact 签名测试） | `STEP 01 sign-package done package=2a2a2a2a… signer=78c1b72e…` |
| 2. 验签 | `ArtifactStore::verify_package`（签名 + head 绑定，FINALIZED gate） | `RECEIPT kind=package-verification id=9a5a64c5… entries=1` |
| 3. 安装 | `ApplicationAuthority::install_application`（authority-first：仅凭 receipt id 回读验证事实，七式 digest 绑定） | `RECEIPT kind=installation id=9b4a39b5… application=f8f451f6…` |
| 4. Task/Attempt | `SqliteTaskAuthority::register_task / register_attempt`（冻结快照 bundle、cancellation scope） | `RECEIPT kind=commit-permit id=6a9c4b9a… task=3e3e… attempt=3f3f…` |
| 5. CommitPermit | `request_commit_permit_with_authorities_struct`（`Authorities` struct 入口优先，本链 `Authorities::default()`；write_set_root = `artifact_publication_plan_root(&[expectation])`） | 同上 receipt；permit 状态经 `inspect_permit` 读回 |
| 6. Fiber 物化 | `TokioRuntimeAdapter::spawn_fiber`（`nlos-runtime` 契约：`FiberSpec` 携带 attempt/scope 身份） | `STEP 06 fiber-operation operation=5656… plan=0bae48ad… fiber=Completed` |
| 7. 异步 Driver Operation | fiber future 内 `SqliteOperationStore::register → dispatch → complete`（owner_fiber 绑定该 fiber，one-shot token + 完成回执） | operation 终态 `Completed { receipt_id: 5858… }` |
| 8. Artifact 写 + 计划 | fiber future 内 `ArtifactStore::stage_revision`（permit + write_set_root 授权）→ `plan_artifact_commit` | plan `Publishing → Ready → Finalized` |
| 9. TaskCommitReceipt | `ArtifactCommitCoordinator::converge_pending`（verify-then-commit 收敛） | `RECEIPT kind=task-commit id=700dc2b0… head=1 publications=1` |
| 10. cancel | `cancel_task`（`Applied{cancel_epoch=1, closed_attempts}`）+ `RuntimeAdapter::cancel_scope` → 新 fiber 准入 `RuntimeError::Cancelled` → 后续 permit 请求 `CancelledBeforeEffect` | `STEP 10 cancel applied epoch=1 closed_attempts=1`；`STEP 12 task_state=Cancelled converged_plans=0` |
| 11. crash recovery | drop 全部权威（kill -9 类比：无任何关闭路径）→ `SliceKRuntime::open` 同 root 重开 → `converge_pending` 重放 durable 前缀（staged revision + plan，ADR-0009/0012 事件溯源续跑精神）→ 二次 drain 空（无双重提交）；验签 receipt 同请求重放 `Replayed` | `RECEIPT kind=task-commit-recovered id=b9ff8135… head=1 publications=1`；`INSPECT-RECOVERED task head_commit_seq=1, attempt state=Committed, artifact_head revision=2` |
| 12. inspect | `inspect_chain` 从各权威直读，稳定 `key=value` 行 | `INSPECT application=installed generation=1 / task head_commit_seq=1 / artifact_head revision=2 / operation state=Completed` |

**演示 bin**（`slice-k-demo`，crate 内 `[[bin]]`）：单进程顺序跑 happy chain → cancel → crash recovery（drop+reopen）三场景，每步打印 `[slice-k]`-前缀稳定可 grep 的 Receipt 摘要行与 inspect 行，最后 `DONE`。不依赖 CLI bin（自带 inspect 输出）。

**Windows/CI 纪律**：纯 SQLite + std；inspect 为进程内 dispatch，无 Unix socket、无 `cfg(unix)` 门控需求；clippy 双工具链零警告；fmt 双工具链 `--check` 干净。

## 2. 验证（base `bb93e2f` 工作区，含并行车道未提交改动；定向 `-p` 命令）

```text
cargo test -p nlos-slice-k
  → end_to_end 3 passed / 0 failed：
    full_vertical_slice_produces_every_receipt_and_is_inspectable
    cancel_closes_attempt_fences_permit_and_runtime_scope
    drop_reopen_replays_durable_prefix_to_consistent_terminal_state
cargo clippy -p nlos-slice-k --all-targets -- -D warnings            → 0 warning
cargo +nightly-2026-08-01 clippy（同前）                              → 0 warning
cargo fmt -p nlos-slice-k -- --check（双工具链）                       → 干净
cargo run -p nlos-slice-k --bin slice-k-demo                          → 跑通（输出见 §1 步骤表，末行 DONE）
```

断言要点（非穷举）：验签 receipt signer/manifest digest 与签名 fixture 绑定；installation→verification 的 receipt-id/digest 链一致；fiber 写落地 revision 2（包载荷为 1）；permit `Closed`、attempt `Committed`、task `head_commit_seq=1`；cancel 后 `cancel_epoch=1`、attempt `Cancelled`、permit 请求被 `CancelledBeforeEffect` 拒绝、runtime scope 拒绝新 fiber、零 commit plan；crash 后 durable 前缀逐字段等于 crash 前（task head 0、head rev 1、installation/operation 终态逐字节相等），重开收敛 receipt `new_head_commit_seq=1`、artifact head rev 2 且 digest 等于嵌套 publication receipt、二次 drain 空、验签同 key 重放 `Replayed` 逐字节相等。

## 3. Canonical commits

- 本 Attempt 按任务约束禁止 git 写操作；写集（`crates/nlos-slice-k/**`、根 `Cargo.toml` members 一行、本 evidence）按原子提交规范留待编排者/integrator 基于上述验证结果落库。`Cargo.lock` 增量为 cargo 自动解析新 member，属预期，未手编。
- base HEAD `bb93e2f`；工作区同时存在并行车道（system-control/sdk、b-task-006m、b-sdk-csharp）的未提交改动，均不在本车道写集，未触碰。

## 4. 缺口清单（链路中发现的真实缺口与最小绕行）

1. **Application↔Task 关联无权威字段**：`TaskSpec` 当前只有 `task_id/task_generation/registered_at_ms`，没有可引用 `application_id` 或 manifest digest 的自由字段；绕行：关联关系停留在 slice 编排层（`HappyChain` 同时持有两者并同窗打印），未做确定性 ID 派生（避免发明语义）。**待 TaskAuthority 后续 schema 扩展声明式关联字段**。
2. **`request_commit_permit` 梯子构造器已 deprecated**：landed API 的权威入口已是 `*_with_authorities_struct`；本链按任务书要求采用 `Authorities` struct 入口（`Authorities::default()`），无绕行，仅登记（ladder 旧入口在本仓库部分测试先例中仍以 `#[allow(deprecated)]` 存在）。
3. **identity key validity 与真实 wall 时钟的域差**：identity 权威按逻辑 ms 比较 validity，`u64::MAX` 会撞 SQLite i64 列；绕行：fixture 用 `i64::MAX as u64` 作为 `key_valid_until_ms`。属 fixture 级取值选择，非权威缺陷。
4. **ProcessAuthority 未入链**：FiberSpec 的 `agent_instance_id/process_id` 等为 fixture 标识，未经 `nlos-process` 权威注册（`register_fiber_incarnation`/binding 已存在但非本链最小路径）；纵切面的 Process/AgentInstance 段以标识传递代替权威物化。**待后续切片把 Process binding 接入 spawn 前置校验**。
5. **Operation 与 TaskWriteSet effect 端点未打通**：driver Operation 走 `nlos-store` 的 register/dispatch/complete 自治路径，未注册为 Task participant / 未进入 `TaskWriteSet` effect endpoint（v24+ 能力存在但需要 participant registry 全套接线）；本链的 Artifact 写经 permit-bound staged publication 授权，operation 完成回执不进入 effect history。**待 effect 切片统一**。

## 5. 已知限制（如实声明，不冒充产品级）

- **单机单进程**：全部权威为单写者 SQLite；纵切面未含跨进程 IPC（CLI 走进程内 dispatch 替身）、未含 ADR-0011 签名贯穿的线上传输。
- **演示级而非产品级**：id/key 为种子 fixture 值；无并发多 Attempt 竞争、无 multi-party、无策略引擎；错误面为组装层透传。
- **NL 路径未接**：inspect/control 的自然语言面不存在，仅稳定文本输出可供后续 NL 层消费。
- **crash 模型为 drop+reopen**：kill -9 类比（OS 页缓存存活），真实掉电由各权威自身既有的 fault 矩阵覆盖，本车道未重复建设。
- **fiber replay 语义**：恢复靠 commit-coordinator 重放 durable 前缀（landed 机制），不是重跑 fiber future；进程内 fiber 状态机随进程消失。
- **未运行项**：`cargo --workspace` 级 test/clippy/fmt（任务约束仅允许 `-p nlos-slice-k` 定向命令）；Windows 实机未验证（纯 SQLite+std + 双工具链门作为代理证据）；CI 接线未做；e2e 未覆盖「验签失败→拒装」负路径（verify-then-commit fail-closed 已由 nlos-artifact/nlos-application 各自证据覆盖）。

## 6. PARTIAL_PASS 结论

纵切面 12 步全部以已落地 API 贯通并有集成测试与可 grep 演示输出背书——这是议题 31 §6 Slice K 证据门的首次实体兑现；但按 §4/§5 的缺口与限制（Process 权威未物化、Application↔Task 无权威关联、无跨进程面、单机演示级），维持 `PARTIAL_PASS`，不宣称 Slice K 完成。
