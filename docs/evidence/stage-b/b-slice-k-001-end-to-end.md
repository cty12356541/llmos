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
  → competing_attempts 4 passed / 0 failed（2026-08-30 追加，见 §7）：
    competing_attempts_cas_issues_exactly_one_permit_requester_a_first
    competing_attempts_cas_issues_exactly_one_permit_requester_b_first
    cancel_racing_a_live_permit_linearizes_permit_first_with_single_commit
    cancel_before_any_permit_request_fails_closed_both_attempts
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
4. ~~**ProcessAuthority 未入链**：FiberSpec 的 `agent_instance_id/process_id` 等为 fixture 标识，未经 `nlos-process` 权威注册（`register_fiber_incarnation`/binding 已存在但非本链最小路径）；纵切面的 Process/AgentInstance 段以标识传递代替权威物化。~~ **已勾销（2026-08-30，§8）**：`SliceKRuntime` 已持有 `ProcessAuthority`，三条链（happy/cancel/recovery）与 competing_attempts 车道的全部 fiber 均在 spawn 前经 `create_isolation_domain + register_delegated_process` 权威物化，FiberSpec 的 process/agent 字段改从注册回执取；crash recovery 断言绑定存活与重放幂等。遗留子项：fiber incarnation/snapshot 层（`register_fiber_incarnation`/`write_fiber_entry_snapshot`）仍未入链（见 §8.4）。
5. **Operation 与 TaskWriteSet effect 端点未打通**：driver Operation 走 `nlos-store` 的 register/dispatch/complete 自治路径，未注册为 Task participant / 未进入 `TaskWriteSet` effect endpoint（v24+ 能力存在但需要 participant registry 全套接线）；本链的 Artifact 写经 permit-bound staged publication 授权，operation 完成回执不进入 effect history。**待 effect 切片统一**。

## 5. 已知限制（如实声明，不冒充产品级）

- **单机单进程**：全部权威为单写者 SQLite；纵切面未含跨进程 IPC（CLI 走进程内 dispatch 替身）、未含 ADR-0011 签名贯穿的线上传输。
- **演示级而非产品级**：id/key 为种子 fixture 值；竞争语义已由进程内顺序线性化测试覆盖（§7），尚未覆盖真并发线程交织下的 permit CAS 竞争、无 multi-party、无策略引擎；错误面为组装层透传。
- **NL 路径未接**：inspect/control 的自然语言面不存在，仅稳定文本输出可供后续 NL 层消费。
- **crash 模型为 drop+reopen**：kill -9 类比（OS 页缓存存活），真实掉电由各权威自身既有的 fault 矩阵覆盖，本车道未重复建设。
- **fiber replay 语义**：恢复靠 commit-coordinator 重放 durable 前缀（landed 机制），不是重跑 fiber future；进程内 fiber 状态机随进程消失。
- **未运行项**：`cargo --workspace` 级 test/clippy/fmt（任务约束仅允许 `-p nlos-slice-k` 定向命令）；Windows 实机未验证（纯 SQLite+std + 双工具链门作为代理证据）；CI 接线未做；e2e 未覆盖「验签失败→拒装」负路径（verify-then-commit fail-closed 已由 nlos-artifact/nlos-application 各自证据覆盖）。

## 6. PARTIAL_PASS 结论

纵切面 12 步全部以已落地 API 贯通并有集成测试与可 grep 演示输出背书——这是议题 31 §6 Slice K 证据门的首次实体兑现；但按 §4/§5 的剩余缺口与限制（Application↔Task 无权威关联、无跨进程面、单机演示级；Process binding 物化已于 2026-08-30 由 §8 勾销），维持 `PARTIAL_PASS`，不宣称 Slice K 完成。

## 7. 竞争场景补充证据（2026-08-30 追加：双 Attempt 竞争 CommitPermit + cancel/commit 竞态）

- **定位**：议题 31 §6 证据门条 2-3（「同时启动两个竞争同一 TaskHead 的 TaskAttempt」「只有一个 Attempt 获得 CommitPermit」）与 ROAD-B-003 的**纵切面级前片**——在已贯通的 12 步纵切面上补齐竞争维度；不宣称 ROAD-B-003 全达成（跨 Task handle 泄漏、snapshot 漂移、真并发交织等仍开放）。
- **写集**：仅 `crates/nlos-slice-k/tests/competing_attempts.rs`（新增）与本 §；`nlos-task` 等已落地权威零改动、只消费。
- **构造**：同一 Task 上注册两个 Attempt（各自独立 snapshot bundle：不同 `snapshot_id`/`snapshot_digest`；各自独立 write set：不同 stage key → 不同 staging identity/digest/write_set_root；不同 idempotency key）。两组请求顺序（A 先 B 后；B 先 A 后）均测。fixture 复用 `SliceKRuntime`/`seeded_key`/`spawn_write_fiber`/`WriteFiberJob` 组装函数，未发明任何权威语义。

### 7.1 场景矩阵与实测语义（按 TaskAuthority 已落地语义如实断言）

| # | 场景 | 线性化结果（实测） | 关键断言 |
|---|---|---|---|
| 1 | 双 Attempt 请求 permit，A 先 B 后 | A `Issued`（attempt→`CommitPermit­ted`，`task.active_permit=Some(A)`）；B `Superseded{winner=A}`（attempt→`Superseded` 终态，**无 permit 行**） | 恰好一个 permit；`Superseded.winner.permit_id == A.permit_id`；head 保持 0 |
| 2 | 同上，B 先 A 后 | 与 #1 对称（胜者换为 B） | 不变量集与请求顺序无关 |
| 3 | 胜者完成提交 | fiber（operation→stage rev 2→plan）→ `converge_pending` 恰好 1 个 receipt，head 0→1，permit `Closed`，胜者 attempt→`Committed`；二次 drain 空 | head 单调且恰进一次；无双重提交；artifact head rev 2 |
| 4 | 败者重试边界（新 key + 自己的 write_set 重新请求） | `Err(TaskStoreError::InvalidAttemptState { state: Superseded })` ——终态栅栏 fail-closed，零副作用 | head 不变；失败为 typed 错误而非新 permit |
| 5 | cancel/commit 竞态：permit 先发、plan 已落、未收敛时 cancel | cancel `Applied{cancel_epoch=1, closed_attempts=[]}`——**permit 持有者（`CommitPermitted`）与已 `Superseded` 败者都不是 open candidate，cancel 不关闭任何 attempt、不清除 outstanding permit**（permit-first 线性化，`[TASK-CANCEL-002]`/`[TASK-COMMIT-003]` 已落地语义）；随后 converge 仍完成该 permit 的唯一提交 | 窗口内 head 保持 0（cancel 单独不推 head）；收敛后终态唯一且一致：head=1 且 `task.state=Cancelled`、`cancel_epoch=1` 并存；无双重终态；二次 cancel 换 key → `AlreadyCancelled{cancel_epoch:1}` 不再递增；二次 drain 空 |
| 6 | cancel 先于任何 permit 请求（双 Attempt 均 open candidate） | cancel `Applied{cancel_epoch=1, closed_attempts=[A,B]}`（各带 closure receipt）；两 Attempt 后续 permit 请求均 `CancelledBeforeEffect{receipt_id=各自 closure receipt}` | 零 permit Issued；converge 空；head 恒 0；artifact 停留 rev 1；无任何终态 commit |

### 7.2 断言纪律与语义发现

- 只断言 `SqliteTaskAuthority` 已文档化保证的不变量：唯一 permit（`[TASK-COMMIT-001]`）、head 单调恰进一次、无双重提交（二次 drain 空）、cancel 线性化（`[TASK-CANCEL-002/003]`）。全部断言与实测一致，未发现语义与文档相悖之处。
- **实测语义记录**：cancel 与已发 permit 的竞争结果**不是二选一**（既非「commit 完成且 cancel 被拒」，也非「cancel 生效则 converge 被抑制」），而是 permit-first 双落地：cancel 生效（epoch=1、task `Cancelled`）**且** outstanding permit 仍收敛出唯一 commit（head=1）。二者线性化序唯一（permit 发放先于 cancel 提交），终态无歧义、可重放、无双重提交。此为「唯一 canonical commit」在 cancel 竞争下的构造性前片证据。
- effect-history root 说明：两 Attempt 的 snapshot bundle 各自独立构造（不同 snapshot_id/digest），但 `expected_head_commit_seq/effect_history_root/retry_fence_epoch` 必须逐位绑定当前 TaskHead（空根），否则 CAS 判 `Conflicted`——这是 head-binding 验证的既定语义，非独立自由度。

### 7.3 验证（追加车道，base HEAD `74bb694` 工作区）

```text
cargo test -p nlos-slice-k                    → 7 passed / 0 failed（competing_attempts 4 + end_to_end 3）
cargo clippy -p nlos-slice-k --all-targets -- -D warnings          → 0 warning
cargo +nightly-2026-08-01 clippy（同前）                            → 0 warning
cargo fmt -p nlos-slice-k -- --check（stable + nightly-2026-08-01） → 干净
```

### 7.4 已知限制（本前片）

- 线程内顺序线性化（请求顺序 A→B 与 B→A 两种），非多线程真并发交织；SQLite `BEGIN IMMEDIATE` 单写者锁是串行化依据，真并发交织测试待后续车道。
- 竞争以 artifact-only permit（`planned_effects=[]`、无 effect slot）为对象；EffectPermit 层的竞争（议题 31 §6 条 4）未覆盖。
- 未覆盖：跨 Task handle 泄漏、snapshot 漂移后败者路径、多 winner 多轮竞争——ROAD-B-003 全门仍开放，本车道仅为纵切面级前片。

## 8. Process authority 物化接入纵切面（2026-08-30 追加：materialize Process → Fiber）

- **定位**：勾销 §4 缺口 4——Fiber spawn 前经 `nlos-process`（B-PROCESS-001 durable generation/fence 权威）注册进程绑定，纵切面顺序成为「materialize Process → Fiber」完整实现；只消费已落地 API，不发明 LaunchGrant/出生原子性等未落地语义。
- **写集**：仅 `crates/nlos-slice-k/**`（`Cargo.toml` 加 `nlos-process` 依赖一行、`src/{runtime,chain,package,error,lib}.rs`、`src/bin/slice-k-demo.rs`、`tests/{end_to_end,competing_attempts}.rs`）与本 §。`nlos-process` 零改动（只读消费）。`Cargo.lock` 增量为 cargo 自动解析新依赖边，属预期，未手编。base HEAD `afd05ae`。

### 8.1 接线摘要（消费的 API 与语义）

| 接线点 | 消费的已落地 API | 语义（按 ProcessAuthority 既有行为） |
|---|---|---|
| 组装器 | `ProcessAuthority::open(root/process)` 入 `SliceKRuntime.process` | 与其他权威同款 WAL/FULL 硬校验；重开同 root 即恢复入口 |
| 物化步骤 1 | `create_isolation_domain`（`policy_digest=[seed+111;32]`、key `seeded_key(seed,110)`） | 幂等 `Created/Replayed`；同 key 同 policy digest 重放返回原 domain 记录 |
| 物化步骤 2 | `register_delegated_process`（key `seeded_key(seed,113)`；present domain 当前代/fence） | 权威派生 `process_id/agent_instance_id`（SHA-256 派生，非 fixture 值）；`process_generation=1` + fencing token；幂等重放仅比对 (task, attempt, domain 三元组)，`created_at_ms` 不参与——crash 后重开换时间戳重放仍字节相等 |
| Fiber 前置 | `runtime.materialize_process(seed, task_id, attempt_id, Generation::INITIAL)`（`package.rs` 助手，紧随 `register_task_and_attempt`） | 注册回执 `ProcessBindingRecord` 即 fiber 身份来源 |
| FiberSpec | `process_id/process_generation/agent_instance_id/agent_generation` 全部改从注册回执取 | happy/cancel/recovery 三链 + competing_attempts 车道：不再有任何凭空 process/agent id |
| inspect 面 | `inspect_active_process_binding(process_id)` 入 `ChainQuery.process_id: Option<ProcessId>` → `ChainInspect.process` | fail-closed 回读（head↔binding 交叉核对 + domain 活性）；`report_lines` 新增 `process=<id> generation=<n> agent=<id>` 行，缺失为 `absent` |
| 错误面 | `SliceKError::Process(ProcessAuthorityError)` 透传 | 组装层只命名拒绝方，不加语义 |

fixture key 偏移 110–114（domain key/policy 字节/时间戳/注册 key），各链已用偏移均 <100，无碰撞。

### 8.2 demo 输出新增行（实跑）

```text
[slice-k] STEP 05b materialize-process done process=3786d63093b17d96 generation=1 agent=5796cac66bab5309 domain=d526a2a6a9093821
[slice-k] INSPECT process=3786d63093b17d96 generation=1 agent=5796cac66bab5309
[slice-k] STEP 14 process-binding survived process=9a6d6ee2239e1448 generation=1
[slice-k] INSPECT-RECOVERED process=9a6d6ee2239e1448 generation=1 agent=8ce1c5313bab16ef
```

STEP 05b 位于 Task 注册 receipt（commit-permit）与 STEP 06 fiber-operation 之间，呈现「materialize Process → Fiber」顺序；重开后绑定存活行与 INSPECT-RECOVERED 行同窗呈现 crash 存活事实。

### 8.3 测试增强与断言要点

- `full_vertical_slice_…`：注册回执读回 `inspect_active_process_binding` 逐字段等于 `chain.process`（generation=1）；绑定 task/attempt 与链一致；inspect 面 `ChainInspect.process` 相等且 report 行含 `process=<id> generation=1`。
- `drop_reopen_…`（crash recovery）：crash 前后 `inspect_active_process_binding` 逐字节相等；重开后代次不变（仍为注册时的当前代）；`materialize_process` 同 key 重放 → 同一 `ProcessBindingRecord`（幂等，domain 创建亦重放）。
- `cancel_…`：拒绝性 fiber spec 改用 `facts.process` 的权威身份（scope 拒绝语义不变）。
- `competing_attempts`：胜者 fiber 的 process/agent 字段改从 `materialize_process(…, winner_attempt)` 回执取；竞争语义断言全部不变、全绿。

### 8.4 验证（base HEAD `afd05ae` 工作区，定向 `-p` 命令）

```text
cargo test -p nlos-slice-k                    → 7 passed / 0 failed（end_to_end 3 + competing_attempts 4）
cargo clippy -p nlos-slice-k --all-targets -- -D warnings           → 0 warning
cargo +nightly-2026-08-01 clippy（同前）                             → 0 warning
cargo fmt -p nlos-slice-k -- --check（stable + nightly-2026-08-01）  → 干净
cargo run -p nlos-slice-k --bin slice-k-demo                         → 跑通（新增行见 §8.2，末行 DONE）
```

### 8.5 剩余缺口（如实登记）

- **LaunchGrant / 出生原子性未落地**：`nlos-process` 未提供 LaunchGrant 或跨权威出生决策面；本车道只接「durable binding 注册 + generation/fence」层，进程与 Task/Domain 之外的准备门（resource/capability/namespace prepares）仍未接入，待上游落地后另行接线。
- **fiber incarnation/snapshot 层未入链**：`register_fiber_incarnation`/`write_fiber_entry_snapshot` 已落地但本链未消费（fiber 复用与 crash-window 快照恢复属后续切片）。
- **process 绑定与 cancel 无联动语义**：task cancel 不（也不应）撤销 process binding——绑定撤销/rotate 语义上游未定义，本链只断言存活，不发明撤销。
- **未运行项**：`cargo --workspace` 级命令（任务约束仅允许 `-p nlos-slice-k`）；Windows 实机未验证（纯 SQLite+std + 双工具链门为代理证据）。

## 9. Application lifecycle uninstall 接入纵切面（2026-09-05 追加：disable → uninstall 最小前缀）

- **定位**：ROAD-B-001 Slice K 消费 B-APPLICATION-003 `uninstall_application` 最小前缀——纵切面 lifecycle 尾从「disable + fail-closed 重装」延伸到「uninstalled 终态 + fail-closed 重装」；只消费已落地 API，不发明 Task/Process teardown 或 GC。
- **写集**：仅 `crates/nlos-slice-k/**`（`src/package.rs` 新增 `SliceKRuntime::uninstall_application`；`src/runtime.rs` `report_lines` 按 `ApplicationStatus` 输出 installed/disabled/uninstalled；`tests/lifecycle_uninstall.rs` 新增 2 用例；`src/bin/slice-k-demo.rs` STEP 09c）与本 §、`docs/evidence/stage-b/b-application-003-uninstall.md` §Slice K 短追加。`nlos-application` 零改动（只读消费）。base HEAD `544ca72`。

### 9.1 接线摘要

| 接线点 | 消费的已落地 API | 语义 |
|---|---|---|
| 组装助手 | `SliceKRuntime::uninstall_application(package_id, seed)` → `ApplicationAuthority::uninstall_application`（key `seeded_key(seed,17/18)`、时间戳取自 clock） | installed\|disabled → uninstalled CAS（代际不动）；`Uninstalled`/`Replayed` 均返回 `UninstallReceipt` |
| inspect 面 | `inspect_application` + `report_lines` status 标签 | 终态行形如 `application=uninstalled generation=<n> manifest=<hex>` |
| fail-closed | `install_verified_package` / `install_verified_package_by_id` 对已卸载 application | typed `ApplicationUninstalled` |

### 9.2 demo 输出新增行（实跑）

```text
[slice-k] STEP 09c lifecycle-uninstall begin
[slice-k] STEP 09c uninstall application=f8f451f68308ccaa generation=2 at_ms=…
[slice-k] RECEIPT kind=application-uninstall id=3c3c3c3c3c3c3c3c application=f8f451f68308ccaa generation=2
[slice-k] STEP 09c reinstall-after-uninstall refused (fail-closed)
[slice-k] INSPECT-UNINSTALLED application=uninstalled generation=2 manifest=334e1539a90df53b
```

STEP 09c 接在 STEP 09b（reinstall-advance → disable → reinstall 拒绝）之后，呈现 disabled → uninstalled 终态转移。

### 9.3 测试与断言要点

- `install_then_uninstall_reaches_terminal_state_and_refuses_reinstall`：installed 直卸载 → status `Uninstalled`、代际不动、`inspect_uninstall_receipt` 读回一致、同 package 重装 fail-closed、同 key 重放幂等。
- `install_disable_then_uninstall_reaches_terminal_state_and_refuses_reinstall`：reinstall 推进代际 → disable → uninstall → 终态 + typed `ApplicationUninstalled` 拒绝重装。

### 9.4 验证（base HEAD `544ca72` 工作区，定向 `-p` 命令）

```text
cargo test -p nlos-slice-k                    → 9 passed / 0 failed（end_to_end 3 + competing_attempts 4 + lifecycle_uninstall 2）
cargo run -p nlos-slice-k --bin slice-k-demo  → EXIT 0（新增行见 §9.2，末行 DONE）
cargo fmt -p nlos-slice-k -- --check            → 干净
cargo clippy -p nlos-slice-k --all-targets -- -D warnings
  → 依赖 crate `nlos-semantic` 既有未提交告警（非本车道写集）阻塞全 `-D warnings` 门；本 crate 源码无新增 clippy 项
```

### 9.5 剩余缺口（如实登记）

- **无 Task/Process teardown**：uninstall 不停止、不等待 happy-chain 上已 Committed 的 Task/Process（与 B-APPLICATION-003 限制一致）；纵切面只接线 application 终态 CAS。
- **无 rollback/GC**：uninstalled 行 durable 保留；无物理删行或 artifact GC。
- **lifecycle 与纵切面主链无联动**：happy chain 的 Task/Attempt/Process 在 uninstall 后仍 inspect 可读——符合当前权威语义，非 slice 发明。

## 10. Uninstall 后显式 orphan GC 接线（2026-09-06 追加：W17-001 ROAD-B-001 GC 最小前缀）

- **定位**：ROAD-B-001 GC 最小前缀——纵切面 uninstall 尾后手动调用 [`ArtifactStore::collect_orphan_blobs`](../../crates/nlos-artifact/src/gc.rs)（B-ARTIFACT-004），证明 package 生命周期中可证明孤儿 blob 可被保守 GC 收集，已引用 blob 保留；只消费已落地 API，不实现 PKG-UPDATE-001、不碰 application authority schema。
- **写集**：`crates/nlos-slice-k/**`（`package.rs` 新增 `collect_orphan_blobs`/`plant_orphan_artifact_blob`/`artifact_blob_path`/`provenance_triple`；`slice-k-demo.rs` STEP 09d；`tests/lifecycle_uninstall.rs` 新增 1 用例；`competing_attempts.rs` 补 `provenance` 字段对齐 artifact API）与本 §、`docs/evidence/stage-b/b-application-003-uninstall.md` §W17-001。`nlos-artifact`/`nlos-application` 零改动（只读消费）。base HEAD `77efcb6`。

### 10.1 接线摘要

| 接线点 | 消费的已落地 API | 语义 |
|---|---|---|
| 组装助手 | `SliceKRuntime::collect_orphan_blobs(seed)` → `ArtifactStore::collect_orphan_blobs`（key `seeded_key(seed,19/20)`、时间戳取自 clock） | 显式保守孤儿 GC；`Collected/Replayed` 均返回 `GcReceipt` |
| fixture | `plant_orphan_artifact_blob(root, tag, len)` | 模拟 package 写入残留（磁盘有 blob、无 metadata 行） |
| blob 路径 | `artifact_blob_path(runtime.root(), digest)` | 对齐 `ArtifactStore::open(root/artifacts)` → `{root}/artifacts/artifacts/blobs/` |
| 时序 | uninstall 终态 CAS 之后手动 GC | uninstall 本身不触发 GC、不解除 artifact 引用 |

### 10.2 demo 输出新增行（STEP 09d，接 09c 之后）

```text
[slice-k] STEP 09d orphan-gc begin
[slice-k] STEP 09d orphan-gc collected=2 scanned=4
[slice-k] RECEIPT kind=artifact-gc id=<hex> collected=2 scanned=4
[slice-k] STEP 09d referenced-blobs retained (fail-closed GC)
```

09c 前植入 2 个 package 孤儿 blob（tag `0xCD`/`0xCE`）；happy chain 已产生的 payload/head 引用 blob 经 GC 后仍存活。

### 10.3 测试与断言要点

- `uninstall_then_manual_gc_collects_package_orphans_and_retains_referenced_blobs`：publish→install→植入 2 孤儿→uninstall→`collect_orphan_blobs` → `collected_digests` 恰为 2 孤儿、文件删除、`package.payload_digest` blob 存活、同 key GC 重放 `Replayed` 逐字节相等。

### 10.4 验证（base HEAD `77efcb6` 工作区，定向 `-p` 命令）

```text
cargo test -p nlos-slice-k                    → 10 passed / 0 failed（end_to_end 3 + competing_attempts 4 + lifecycle_uninstall 3）
cargo clippy -p nlos-slice-k --all-targets -- -D warnings  → 0 warning
cargo fmt -p nlos-slice-k -- --check                       → 干净
```

### 10.5 剩余缺口（如实登记）

- **手动 GC、无自动触发**：与 B-ARTIFACT-004 一致；uninstall 不 schedule/sweep/open-time GC。
- **uninstall 不解除 artifact 引用**：GC 引用集仍来自 artifact store SQLite 行；package payload/head 引用 blob 机械上非孤儿，本切片只证明「可证明孤儿可删、在册引用保留」。
- **无 retention-GC / PKG-UPDATE-001 rollback**：登记为后续 ROAD-B-001 车道。
- **无 Task/Process teardown**：与 §9.5 一致。
