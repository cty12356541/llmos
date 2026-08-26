# B-CHANNEL-001：Durable Channel endpoint authority

- 状态：`PARTIAL_PASS`
- 日期：2026-08-24（增量验收）
- Owner：`ChannelAuthority`（`crates/nlos-channel`，SQLite schema v1）
- 关联上下文：进度单 `B-PARTICIPANT` / `B-OP-FENCE` / `B-TASK-008C2G-*` 各行均把 "Channel endpoint" 列为未决项；本切片是该未决项的第一个 owner-authority 前缀
- Canonical commit：`6b7285ed3a52d2eb2703ac11407dcc4a242ff228`（`feat: add durable channel endpoint authority`，2026-08-23 23:42:34 +0800）

## 1. 证明范围（实现事实）

本切片在 `crates/nlos-channel` 建立单机 durable Channel endpoint authority 的最小前缀：

- **稳定 ChannelId（authority 派生）**：`ChannelId = SHA-256("nlos/channel/id/v1" ‖ len(idempotency_key) ‖ len(capacity_bytes) ‖ len(policy_digest))` 前 16 字节，domain-separated、长度前缀编码；不是调用者提交字段。
- **Owner 派生 generation / fencing token**：create 写入 `Generation::INITIAL`；fencing token 为 32 字节 `SHA-256("nlos/channel/fence/v1" ‖ channel_id ‖ generation ‖ idempotency_key)`，由 owner 按 generation 派生。rotate 以 `expected_generation + expected_fencing_token` 做 CAS：`Immediate` 事务内 guarded `UPDATE ... WHERE current_generation=? AND current_fencing_token=?`，affected 行数 ≠ 1 即 `StaleChannel`；generation 递增用 `checked_next`，耗尽返回 `GenerationExhausted` fail-closed。
- **capacity / policy digest**：`capacity_bytes >= MIN_CAPACITY_BYTES(=1)`，`policy_digest` 为 32 字节，存入 head 行与每代 generation 行；rotate 沿用当前 capacity/policy，不改写历史 generation 行。
- **create / rotate / inspect 语义**：
  - `create_channel`：capacity 校验通过后，在单个 `Immediate` 事务内同时写 `channels` head、`channel_topic_identities`、`channel_generations` v1 与 `channel_endpoint_proofs`；相同 idempotency key + 相同 capacity/policy 重放返回 `Replayed` 原 record；payload 漂移或派生 id 与既有不同 key 冲突返回 `IdempotencyConflict`。
  - `rotate_channel`：同 key 同请求重放返回 `Replayed`（并回读校验 rotation receipt 与 generation 行 fence 一致，不一致判 `CorruptRecord`）；未知 Channel `ChannelNotFound`；旧 generation/fence `StaleChannel`；成功时同事务写新 generation 行、新 endpoint proof、rotation receipt 并 CAS 推进 head。
  - `inspect_channel` / `inspect_endpoint_proof`：head↔generation join 回读，head fence 与当前 generation fence 不一致判 `CorruptRecord`；endpoint proof 与 authority 派生值逐字节比对，不一致或当前 generation 缺 proof 判 `CorruptRecord`。
- **Authority 派生 TaskParticipantId endpoint proof**：`participant_id = SHA-256("nlos/channel-topic/participant/v1" ‖ channel_id)` 前 16 字节，同一 Channel 跨 generation 稳定；`admission_receipt_id = SHA-256("nlos/channel-topic/admission/v1" ‖ channel_id ‖ generation ‖ fencing_token)` 前 16 字节，逐 generation 变化。proof 只能由 owner 回读获得，无公开构造函数，消费者不能拿调用者自填的 tuple 冒充。
- **重启 replay**：`open()` 强制 `WAL + synchronous=FULL(2) + foreign_keys`，任一不满足返回 `DurabilityUnavailable` fail-closed；schema `user_version` 0→1 迁移，未知版本 `SchemaVersionUnsupported`。重开 authority 后 head、endpoint proof 逐字段相等，rotate 同 key 重放返回原 record。
- **stale generation / idempotency 冲突 fail-closed**：capacity 重绑定 → `IdempotencyConflict`；旧 generation rotate → `StaleChannel`；同 rotation key 漂移 expected fence → `IdempotencyConflict`。
- **零容量 pre-write 拒绝**：`capacity_bytes = 0` 在拿锁、开事务、任何 durable 写之前被 `validate_capacity` 拒绝（`InvalidCapacity`）。
- **DDL 防护**：STRICT 表 + 长度/CHECK 约束；`channel_generations`、`channel_topic_identities`、`channel_endpoint_proofs` 由 trigger 禁 UPDATE/DELETE（immutable）。

## 2. 本地验证证据

环境：macOS 26.5.2，rustc 1.97.1（8bab26f4f 2026-07-14），cargo 1.97.1（c980f4866 2026-06-30）。

命令与结果：

```text
cargo test -p nlos-channel --quiet
  → running 3 tests（tests/channel_authority.rs）
  → test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured
  →（lib 单元测试 0 项，doc-tests 0 项）

cargo clippy -p nlos-channel --all-targets --all-features -- -D warnings
  → Checking nlos-channel v0.1.0
  → Finished `dev` profile ... in 1.30s（0 warning / 0 error）

cargo fmt --all -- --check
  → 退出码 0，无输出（通过）
```

3 项 integration tests 覆盖：

1. `channel_endpoint_proof_is_owner_assigned_and_survives_restart`：首次 create（非 Replayed）、同请求 exact replay、初始 proof、rotate 后 participant_id 不变 / participant_generation 前进 / admission_receipt_id 变化、重开 authority 后 head 与 proof 逐字段相等回读、rotate 同 key 重放返回原 record；
2. `stale_generation_and_idempotency_conflicts_fail_closed`：capacity 重绑定 `IdempotencyConflict`、旧 generation rotate `StaleChannel`、同 rotation key 漂移 expected fence `IdempotencyConflict`；
3. `zero_capacity_is_rejected_before_durable_write`：`capacity_bytes = 0` → `InvalidCapacity`。

## 3. Canonical commit / push / CI / 部署状态

各级别分开确认，不互相冒充：

- **本地检查**：§2 全部通过（测试 3/3、clippy 0 警告、fmt clean）。工作区存在其他任务持有的未提交变更（`nlos-resource` / `nlos-store` / `nlos-task`），均不在本切片读写集内；`git diff 6b7285e -- crates/nlos-channel` 为空，本 Evidence 基于 canonical 提交的未漂移代码。
- **提交与 push**：`HEAD == origin/main == 6b7285ed3a52d2eb2703ac11407dcc4a242ff228`（`git branch -r --contains 6b7285e` → `origin/main`、`origin/HEAD`），即该提交已推送至远程 main。提交统计：6 files changed, 1071 insertions(+)（`Cargo.lock` +9、根 `Cargo.toml` +1、`crates/nlos-channel/Cargo.toml` +15、`src/lib.rs` +767、`src/schema.rs` +91、`tests/channel_authority.rs` +188）。
- **CI（实测）**：`gh run list --branch main --limit 10` 显示该提交触发 3 个 run，`gh run view --json headSha` 确认三者 `headSha` 均为 `6b7285ed3a52d2eb2703ac11407dcc4a242ff228`，全部 success（2026-08-23T15:42:57Z 触发）：
  - Rust cross-platform verification [32649416604](https://github.com/cty12356541/llmos/actions/runs/32649416604)：windows-latest 6m7s ✓、macos-latest 3m30s ✓、ubuntu-latest 2m24s ✓、MSRV 1.97 27s ✓；
  - Deploy to GitHub Pages [32649416595](https://github.com/cty12356541/llmos/actions/runs/32649416595)：19s ✓；
  - Schema fuzz smoke [32649416610](https://github.com/cty12356541/llmos/actions/runs/32649416610)：1m10s ✓。
- **部署**：CI 成功不等于部署；本切片没有任何部署产物或发布声明。

## 4. 证据等级与明确未完成

当前为单机 SQLite 重启级 `H3 / PARTIAL_PASS`：

- 本切片**不**实现 queue delivery、Topic routing、fanout、payer accounting，也没有接入 `nlos-task` 的 TaskWriteSet / participant registry；`TaskParticipantId` 仅由 Channel owner 派生并回读，尚未在任何 Task 侧注册或复核；
- 未执行 kill-9、torn-write、ENOSPC、真实掉电或独立 fault matrix；CI 覆盖三平台编译/测试与 fuzz smoke，不构成电源/文件系统级 durability 证据；
- 单进程内 `Mutex<Connection>` 单写者，无跨进程 IPC、peer 认证、lease/takeover 或多机语义；
- capacity/policy 只在 create 时绑定、rotate 不修改，容量未做任何 admission/enforcement 执行；
- 本 Evidence 只确认 §2–§3 所列事实，不把该工作包标记为 `DONE`，不声明 `H4+`。

---

## 5. 2026-08-24 增量：Task 侧接线（Attempt TASK-CHANNEL-ENDPOINT-WIRING-01）

Base HEAD：`73a8b49844c39f7a281b91e3a51c3582610400b1`（工作树干净，无漂移）。本增量把已提交的 Channel endpoint authority 接入 `nlos-task`，关闭 B-CHANNEL-001 未决项中的 "TaskWriteSet/participant registry 接线"。**候选工作集，未提交、未推送；未修改 stage-b-progress.md。**

### 5.1 接线内容

- **participant registration**：`SqliteTaskAuthority::register_channel_participant(channel_authority, task_id, expected_registry_binding, channel_id, expected_channel_generation, registered_at_ms)`，镜像 `register_operation_binding_participant`：`inspect_endpoint_proof(channel_id)` 回读错误包成新增 `TaskStoreError::ChannelParticipantAuthority(nlos_channel::ChannelAuthorityError)`；`proof.participant_generation != expected` → 既有 `ParticipantEndpointGenerationMismatch`；以 owner 派生三元组构造 `ParticipantRecord { participant_type: ChannelTopic (code 4) }` 委托 `register_verified_participant`。
- **per-effect endpoint（seal）**：`TaskWriteSetEffectEndpointKind::ChannelTopicBinding`（kind code 7）+ 请求变体 `ChannelTopicBinding { effect_seq, channel_id, expected_channel_generation }`；`resolve_effect_endpoints` 的 Channel 分支镜像 Operation 分支：authority 缺席 → `TaskWriteSetConflict { "Channel effect endpoint requires ChannelAuthority readback" }`；校验 `proof.channel_id == channel_id` 且 `proof.participant_generation == expected_channel_generation`（不符 → typed mismatch），object_id 派生 `channel_id.into_bytes()`。kind→ParticipantType 的五处 exhaustive match（model.rs code()/from_code()；store.rs seal 路径、permit 路径、`effect_endpoint_participant`）全部补齐。
- **permit revalidation**：`validate_channel_endpoint_bindings` 镜像 `validate_operation_endpoint_bindings`，在 `issue_permit` 内紧邻 OP validator 调用；无 Channel 端点则跳过；authority 缺席 → `TaskWriteSetConflict { "Channel effect endpoint requires ChannelAuthority readback before permit freeze" }`；逐端点字节比对三个 proof 字段（participant_id / participant_generation / admission_receipt_id），不符 → `TaskWriteSetConflict { "Channel endpoint proof differs before permit freeze" }`。语义差异（刻意）：Channel inspect 不接受 generation 参数、总是返回**当前** generation 的 proof，因此 seal 与 permit 之间的 rotation 自然破坏等价比较（stale fence，OP 无法测试的场景）。
- **authority 线程化**：`channel_authority: Option<&ChannelAuthority>` 贯穿 `seal_task_write_set_inner` / `resolve_effect_endpoints` / `request_commit_permit_inner` / `compete_for_permit` / `issue_permit`；既有全部公开入口传 `None`（legacy authority-free 路径不变，由 211 项基线测试背书）；新增 channel-only 变体 `seal_task_write_set_with_channel_authority` 与 `request_commit_permit_with_channel_authority`，命名/线程化镜像 OP 对应变体。
- **schema v39 → v40**：`SCHEMA_V40_SQL` 为 v24 式表重建（endpoint 半部）：drop 两个 immutability trigger → 重建 `task_write_set_effect_endpoints`（`endpoint_kind BETWEEN 1 AND 7`）→ 原样搬运行 → rename → 重建两 trigger → `PRAGMA user_version = 40`；`migrate_v40` 幂等预检镜像 `migrate_v24`（wide-test `check(endpoint_kindbetween1and7)`，old-test `...1and6`，trigger_count == 2；部分迁移 → `CorruptRecord("partial Channel endpoint schema migration")`）。`SCHEMA_VERSION = 40`；8 个测试文件中 11 处版本 pin 39→40。
- **migrate_v24 单调幂等修正**（接线必需的最小修复）：v5/v6/v7/v8 回退式迁移测试把现代 DB 回退到旧 `user_version` 但保留 endpoint 表新形状；v24 的"已迁移"宽测试原先只识别 `1 AND 6`，遇到 v40 的 `1 AND 7` 形状误判 partial 并 fail-closed。修正为 `contains(1and6) || contains(1and7)`——已完成 v40 形状必然蕴含 v24 已完成；未知形状仍 fail-closed。
- **root-hash 稳定性**：无域版本提升——kind 7 沿用既有 `llmos/task-write-set-effect-endpoints/v1` 域（先例：kind 6 随 v24 到达时未改域）。旧 seal 字节级 replay 不变（全量既有 seal/replay 测试通过间接背书）；测试直接断言：kind 6 与 kind 7 仅差 kind 字节的两份 endpoint 集合产生不同 root、authority 实算 root 与 kind-7 公式一致、空集仍 `[0;32]`。

### 5.2 测试（tests/channel_endpoint.rs，TDD red → green）

- **red**（先写测试）：`cargo test -p nlos-task --test channel_endpoint` → 编译失败 **16 errors**，全部为"缺失变体/API"：5× `seal_task_write_set_with_channel_authority` 不存在、3× `request_commit_permit_with_channel_authority` 不存在、2× 请求枚举无 `ChannelTopicBinding`、1× kind 枚举无 `ChannelTopicBinding`、1× `register_channel_participant` 不存在、3× `nlos_channel` 未链接 + 1× 未解析 import（Cargo.toml 依赖未加）。失败原因正确（非拼写/导入疏漏）。
- **green**：`cargo test -p nlos-task --test channel_endpoint` → **6 passed; 0 failed**：
  1. `verified_channel_endpoint_is_rechecked_during_seal_and_permit`：create_channel → register（registry 含 ChannelTopic）→ seal（断言 kind/object_id/participant_generation/participant_id 持久化）→ 无 authority 的 `request_commit_permit` 精确 reason 拒绝 → channel 变体签发 + 同 key replay → 双 authority drop/重开后 write-set 逐字节相等（Task-rows replay）；
  2. `stale_expected_channel_generation_is_rejected_at_seal_without_partial_seal`：expected=gen2/current=gen1 → `ParticipantEndpointGenerationMismatch { expected: 2, current: 1 }`，且 `TaskWriteSetNotFound`（无半 seal）；
  3. `channel_rotation_between_seal_and_permit_fails_closed`：seal 后 rotate → `"Channel endpoint proof differs before permit freeze"`（OP 测不到的 rotation stale-fence 场景）；
  4. `channel_endpoint_requires_prior_participant_registration`：未注册即 seal → 既有 `"planned effect endpoint is not registered in participant registry"`；
  5. `schema_migrates_v39_endpoint_check_to_v40`：v39 形状 DB（旧 CHECK + user_version=39）重开 → user_version == 40、sqlite_master 含 `BETWEEN 1 AND 7`、两 trigger 齐备；
  6. `endpoint_root_separates_channel_kind_and_empty_set_stays_zero`：如上 root-hash 断言。

### 5.3 质量门（全部通过，命令逐字）

```text
cargo check -p nlos-task
  → Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.83s（0 error/warning）

cargo test -p nlos-task --test channel_endpoint   （red：16 compile errors → green）
  → test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured

cargo test -p nlos-task --quiet
  → 汇总 passed: 217 failed: 0（基线 211 + 新增 6；全部 27 个 test target ok）

cargo clippy -p nlos-task --all-targets --all-features -- -D warnings
  → Finished `dev` profile ... in 10.22s（0 warning / 0 error）

cargo fmt --all -- --check
  → 退出码 0，无输出（通过）

git diff --check
  → 退出码 0（无空白错误）
```

### 5.4 变更集（候选，未暂存/未提交）

`crates/nlos-task/Cargo.toml`（+`nlos-channel` path dep，按字母序）、`Cargo.lock`（path dep 的机械单行）、`src/lib.rs`（错误变体 + Display + source）、`src/model.rs`（kind 7 + 请求变体）、`src/store.rs`（registration/seal/permit/线程化/SCHEMA_VERSION=40）、`src/migrations.rs`（migrate_v40 + SCHEMA_V40_SQL + v24 宽测试单调修正）、8 个测试文件的 11 处版本 pin、`tests/channel_endpoint.rs`（新增）、本 Evidence 增量。Post-write 自检：无 unsafe、无生产 unwrap/expect、无警告抑制新增（沿用文件既有 too_many_lines/too_many_arguments/needless_pass_by_value 惯例）、无 `as` 窄化、fixture 确定性（无 sleep/线程）、legacy authority-free 路径行为不变。

### 5.5 明确未完成（PARTIAL_PASS 保持）

- **EffectPermit channel gate 未接**：`effect.rs` 未改动（后续 lane）——effect 签发/派发层尚不校验 Channel 端点；
- **combined 全-authority 构造器未加**：没有 `...with_authorities_and_channel_authority` 组合变体（文档化缺口，刻意不建）；多非-Artifact 端点混布仍需逐 authority 入口或后续组合入口；
- **queue/Topic/fanout/payer 语义仍排除**：本接线仅为 endpoint proof 绑定，不实现投递/路由/扇出/计费；
- **无 CI / 无提交 / 无推送**：本节仅为本地候选证据；`HEAD` 仍为 `73a8b49`，未 stage、未 commit、未 push，未改 `docs/management/stage-b-progress.md`；
- 无 kill-9/掉电/fault-injection 矩阵；单机单写者语义不变；不声明跨 authority 原子性。

---

## 6. EffectPermit channel gate（2026-08-24 增量）

Base HEAD：`4194998715e85797bde72f4fc517f8b30c440e66`（`feat: bind channel endpoint proofs into task write sets`，工作树干净，无漂移；开跑前 `git rev-parse HEAD` 复核一致）。本增量关闭 §5.5 未决项第一行 "EffectPermit channel gate 未接"，镜像 activation gate（`73a8b49`，`check_effect_slot_activation` / `request_effect_permit_with_operation_authority`）逐条实现。**候选工作集，未提交、未推送；未修改 stage-b-progress.md。**

### 6.1 接缝与语义

- **seam rationale**：seal（`resolve_effect_endpoints`）与 permit freeze（`validate_channel_endpoint_bindings`，4194998）各自做 owner 回读，但 permit 签发到 EffectPermit mint 之间仍存在旋转窗口。本切片在 `request_effect_permit_inner` 的 mint 直前（`check_effect_slot_activation` 之后、`derive_effect_permit_id` 之前）插入 `check_effect_slot_channel_binding`：以 `inspect_endpoint_proof(ChannelId::from_bytes(object_id))` 回读 Channel owner **当前 generation** 的 proof（该 API 不接受 generation 参数，rotation 因此天然表现为不匹配），关闭 seal→permit→mint 窗口的最后一环。
- **triple-equality 语义**：`participant_id` / `participant_generation` / `admission_receipt_id` 三字段逐一字节比较 sealed `TaskWriteSetEffectEndpoint` 行；任一不符 → 既有 `TaskWriteSetConflict { reason: "Channel endpoint proof differs before effect permit mint" }`；owner 回读错误 → 既有 `TaskStoreError::ChannelParticipantAuthority` 包裹传播（无新增错误变体、无 schema/model/migration 改动）。kind != `ChannelTopicBinding` 或无 sealed write set → 原样放行：单槽请求只需自己 kind 的 authority（operation 变体对称放行 Channel 槽），`None` 保持 legacy authority-free 行为。
- **replay 信任 Task 行**：replay 检查（`load_effect_permit_by_key`）仍是 inner 的第一步，在任何 owner 读取之前执行；已 mint 的 permit 重放返回原 durable token，Channel authority 全程不被触碰（测试 4 以空目录 authority 反证）。
- **零持久化失败语义**：gate 失败路径与 activation gate 相同——事务回滚，slot 停在 `Planned`、无 effect_permit_id；失败后的同 key legacy 调用得到 `Issued`（非 `Replayed`），证明失败的 gated 调用未持久化任何东西。
- **线程化**：`request_effect_permit_inner(&self, operation_authority, channel_authority, request)`；既有两个公开入口机械补 `None`，新增 `request_effect_permit_with_channel_authority(&ChannelAuthority, PermitRequest)` thin wrapper。未建组合多-authority 变体（单槽单 kind 使其不必要，见 §6.4）。

### 6.2 测试（tests/effect_channel_gate.rs，TDD red → green）

- **red**（先写测试）：`cargo test -p nlos-task --test effect_channel_gate` → 编译失败 **7 errors**，全部为 E0599 "no method named `request_effect_permit_with_channel_authority`"（编译器同时提示最接近的现存方法是 `request_commit_permit_with_channel_authority`，证明名称解析正确、失败原因即缺失 API 本身，非拼写/导入疏漏）。
- **green**：`cargo test -p nlos-task --test effect_channel_gate` → **6 passed; 0 failed**：
  1. `channel_gated_effect_permit_mints_one_shot_token_when_proof_matches`：create channel → register_channel_participant → `seal_task_write_set_with_channel_authority` → `request_commit_permit_with_channel_authority` → `request_effect_permit_with_channel_authority` mint one-shot token，slot `Permitted`；
  2. `channel_rotation_between_seal_and_effect_permit_fails_closed_without_partial_state`：seal+commit 后 rotate → 精确 reason `"Channel endpoint proof differs before effect permit mint"`，slot 仍 `Planned`/无 permit id；同 key legacy 调用随即 `Issued`（非 `Replayed`，证明失败零持久化——legacy 自身语义为无门禁放行，已在此断言并记录）；
  3. `channel_gate_fails_closed_on_owner_readback_error`：空目录 authority（Channel 从未存在）→ `ChannelParticipantAuthority(ChannelNotFound(_))`，无 token；
  4. `channel_gate_replay_returns_durable_token_without_owner_readback`：mint 后重开 Task authority，以全新**空目录** ChannelAuthority 同 key 重放两次 → 均返回同一 durable token（任何 owner 读取都会失败，返回 token 即证明 replay 零 owner 读；`inspect_effect_permit` 逐字段相等）；
  5. `legacy_effect_permit_issuance_skips_channel_gate_for_channel_slot`：owner 已 rotate（gate 必拒）但 authority-free `request_effect_permit` 仍 mint——执法边界只在新变体；
  6. `channel_gate_passes_through_operation_binding_slot`：OperationBinding 槽（owner 仅 Registered、无 dispatch 准备）经 channel 变体 + 空目录 ChannelAuthority → 照常 mint，channel authority 未被触碰。

### 6.3 质量门（全部通过，命令逐字）

```text
cargo check -p nlos-task
  → Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.10s（0 error/warning）

cargo test -p nlos-task --test effect_channel_gate   （red：7× E0599 → green）
  → test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured

cargo test -p nlos-task --quiet
  → 汇总 passed: 223 failed: 0（基线 217 + 新增 6；全部 27 个 test target ok）

cargo clippy -p nlos-task --all-targets --all-features -- -D warnings
  → Finished `dev` profile [unoptimized + debuginfo] target(s) in 11.07s（0 warning / 0 error）

cargo fmt --all -- --check
  → 退出码 0，无输出（通过）

git diff --check
  → 退出码 0（无空白错误）
```

### 6.4 变更集与明确未完成（PARTIAL_PASS 保持）

变更集（候选，未暂存/未提交）：`crates/nlos-task/src/effect.rs`（+96/−4：`ChannelId` import、`check_effect_slot_channel_binding` sibling checker、inner 线程化 + 新公开变体 + mint 前调用点）、`crates/nlos-task/tests/effect_channel_gate.rs`（新增，530 纯行——镜像既有单文件集成测试惯例，`effect_activation_gate.rs` 652 行 / `channel_endpoint.rs` 620 行同款）、本 Evidence 增量。Post-write 自检：无 unsafe、无生产 unwrap/expect、无新增警告抑制（沿用文件既有 needless_pass_by_value/too_many_lines 惯例）、无 `as` 窄化、fixture 确定性（原子计数器 + temp 目录，无 sleep/线程）、legacy authority-free 与 operation 变体路径行为不变（223 项全量背书）。

未完成项：

- **仍为 validation-only**：mint 时 gate 不持久化任何 Channel 事实（不记录 proof 快照/receipt），Channel 侧也不留 Task 读痕；无跨 authority 原子性声明；
- **单槽单 authority 边界**：channel 变体放行 OperationBinding 槽、operation 变体放行 ChannelTopicBinding 槽（测试 6 与 activation gate 测试 7 互为对称证明）；组合多-authority effect-permit 变体刻意不建（单槽单 kind 使其不必要，文档化缺口而非实现）；
- **无 kill-9/torn-write/掉电 fault matrix**，无 CI、无提交、无推送：本节仅为本地候选证据；`HEAD` 仍为 `4194998`，未 stage、未 commit、未 push，未改 `docs/management/stage-b-progress.md`；
- queue/Topic/fanout/payer 语义仍排除在外（§5.5 原样保持）。

---

## 7. Combined Authorities 构造器（2026-08-24 增量）

Base HEAD：`5acb1f2f7c9be36dc7b5252602d685fa8adb40b9`（`feat: gate effect permits on channel endpoint proofs`，工作树干净，无漂移；开跑前 `git rev-parse HEAD` 复核一致）。本增量关闭 §5.5 未决项 "combined 全-authority 构造器"：在此前任何构造器组合下，同时包含 `OperationBinding`（kind 6）与 `ChannelTopicBinding`（kind 7）effect 端点的写集都无法 seal/permit——梯子变体各自只携带互不相交的 authority 子集（最高梯 `_with_authorities_and_operation_authority` = P+S+R+O，无 C；`_with_channel_authority` = 仅 C）。**候选工作集，未提交、未推送；未修改 stage-b-progress.md。**

### 7.1 设计：struct-at-boundary，内层零重构

- **`pub struct Authorities<'a>`**（store.rs，经 lib.rs 导出）：六个 `Option<&'a _>` 字段逐一镜像既有 inner 线程化的确切 authority 槽位——`artifact: Option<&ArtifactStore>`、`process`、`semantic`、`resource`、`operation: Option<&SqliteOperationStore>`、`channel: Option<&ChannelAuthority>`；`#[derive(Clone, Copy, Default)]`（全 `None` 即 authority-free bundle）。`Debug` 为手写 presence-flag 实现（owner store 是无 `Debug` 的不透明 SQLite 句柄，且其 crate 在本任务写集之外）。
- **`seal_task_write_set_with_authorities_struct(&self, authorities: Authorities<'_>, request)`**：边界处解构——`artifact` 缺席 → fail-closed `TaskWriteSetConflict { "struct-based seal requires the Artifact authority" }`（每个 seal 都强制要求 Artifact，与既有 inner 签名一致）；其余五个 Option 原样传入既有 `seal_task_write_set_inner`。**不新增第二条 seal 路径**。
- **`request_commit_permit_with_authorities_struct(&self, authorities, request)`**：解构进既有 `request_commit_permit_inner`（artifact/process/resource/operation/channel，lease 传 `None`）；`semantic` 字段刻意不被消费——Semantic append 复验留在专用 Semantic-aware finalize 路径（镜像既有 `_with_authorities_and_operation_authority` 的文档化边界）；lease 绑定仍走 `request_commit_permit_with_authority_lease`。
- **纯增量**：全部 8 个既有 seal 变体与 8 个 permit 变体原样保留、零改动、零委托转换；`seal_task_write_set_inner` / `resolve_effect_endpoints` / `request_commit_permit_inner` / `compete_for_permit` / `issue_permit` 内层签名未动。无 schema/model/migration/effect.rs 改动。

### 7.2 测试（tests/combined_authority_seal.rs，TDD red → green）

- **red**（先写测试）：`cargo test -p nlos-task --test combined_authority_seal` → 编译失败 **13 errors**：1× E0432 `unresolved import nlos_task::Authorities`、5× E0599 `seal_task_write_set_with_authorities_struct` 不存在、7× E0599 `request_commit_permit_with_authorities_struct` 不存在（编译器最近邻建议均为既有梯子变体，证明失败原因即缺失 API 本身，非拼写/导入疏漏）。
- **green**：`cargo test -p nlos-task --test combined_authority_seal` → **6 passed; 0 failed**：
  1. `ladder_variants_cannot_seal_mixed_operation_channel_write_set`（缺口实证，实现前后均成立并永久 pin）：混合写集经最高梯 `_with_authorities_and_operation_authority`（P+S+R+O 真实开启）→ 精确 `"Channel effect endpoint requires ChannelAuthority readback"`；经 `_with_channel_authority` → 精确 `"Operation effect endpoint requires OperationAuthority readback"`；两次尝试后 `TaskWriteSetNotFound`（无半 seal）；
  2. `mixed_write_set_seals_permits_and_replays_via_authorities_struct`：双参与者注册（operation 按 participant_registry.rs 惯例、channel 按 channel_endpoint.rs 惯例）→ struct seal（artifact+operation+channel）→ 两端点 kind/object_id/participant_generation/participant_id 逐一断言 → struct permit 签发 + 同 key `Replayed` → 三 authority drop/重开后 write-set 逐字节相等；
  3. `struct_seal_without_channel_authority_fails_closed`：struct 缺 channel（None）→ 同一 seal 期精确 reason，且无半 seal；
  4. `channel_rotation_between_struct_seal_and_struct_permit_blocks_freeze`：seal 后 rotate → permit freeze 期精确 `"Channel endpoint proof differs before permit freeze"`（rotation stale-fence 在组合入口下保持）；
  5. `authority_absent_struct_permit_fails_closed_at_freeze`（镜像 OP 先例 participant_registry.rs:694-699）：缺 channel → 精确 freeze-gate reason；缺 operation → 精确 freeze-gate reason；随后全 authority struct permit 仍首发 `Issued`（失败零持久化）；
  6. `all_none_authorities_struct_matches_legacy_seal_and_permit`（委托等价）：同一 DB 内两个 task，A 走 legacy plain 对、B 走 struct 对（仅 seal 强制的 artifact + 其余全 `None` / permit 全 `None`）；因 registry root 覆盖 per-DB `task_authority_identity` randomblob，跨 task 全记录相等**按设计不可行**，故以双向幂等 replay 断言等价：struct 入口 replay legacy 记录、legacy 入口 replay struct 记录（seal 与 permit 各双向，seal replay 置于 permit freeze 之前以避免 registry 代际漂移）。

### 7.3 质量门（全部通过，命令逐字）

```text
cargo check -p nlos-task
  → Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.88s（0 error/warning）

cargo test -p nlos-task --test combined_authority_seal   （red：13 compile errors → green）
  → test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured

cargo test -p nlos-task --quiet
  → 汇总 passed: 229 failed: 0（基线 223 + 新增 6；全部 28 个 test target ok；既有测试文件零修改）

cargo clippy -p nlos-task --all-targets --all-features -- -D warnings
  → Finished `dev` profile [unoptimized + debuginfo] target(s) in 11.23s（0 warning / 0 error）

cargo fmt --all -- --check
  → 退出码 0，无输出（通过）

git diff --check
  → 退出码 0（无空白错误）
```

### 7.4 变更集与明确未完成（PARTIAL_PASS 保持）

变更集（候选，未暂存/未提交）：`crates/nlos-task/src/store.rs`（+112：`Authorities<'a>` 定义 + 手写 Debug + 两个 struct 入口）、`crates/nlos-task/src/lib.rs`（1 行导出改动）、`crates/nlos-task/tests/combined_authority_seal.rs`（新增，708 纯行）、本 Evidence 增量。Post-write 自检：无 unsafe、无生产 unwrap/expect、无 `as` 窄化、无新增警告抑制（沿用文件既有 needless_pass_by_value 惯例）、lifetime-clean（`Authorities<'a>` 全借用字段 + `Copy`）、fixture 确定性（原子计数器 + temp 目录，无 sleep/线程）、生产 diff 逐行核验零逃逸句柄。

未完成项：

- **既有梯子构造器全部保留**：移除/弃用是 future breaking change，刻意不做；struct 入口与梯子并存，调用方按需选择；
- **finalize 家族未吸收**：`reconcile.rs` / `resource_commit.rs` / `semantic_commit.rs` 的 finalize 入口仍是逐 authority 参数（后续吸收为 future lane）；
- **effect-permit 保持单槽单 authority**：`effect.rs` 未改动（设计如此，见 §6.4——单槽单 kind 使组合变体不必要）；
- **无 schema/model/migration 变更**（ Authorities 是纯边界类型）；
- **无 CI / 无提交 / 无推送**：本节仅为本地候选证据；`HEAD` 仍为 `5acb1f2`，未 stage、未 commit、未 push，未改 `docs/management/stage-b-progress.md`；
- 不声明跨 authority 原子性（verify-then-commit 语义不变）。

## 8. Struct 化收尾三车道：effect-permit 组合 / finalize 吸收 / 梯子弃用（2026-08-26 增量）

> Task: TASK-CHANNEL-STRUCT-CLOSURE-01；三车道并行波次（写集不相交，单一 integrator 按 hunk 拆分提交）。

### 8.1 Canonical commits

- `e8a7256` feat: combine effect permit gates behind authorities struct（车道 C）
- `ff6ccab` feat: absorb finalize ladder into spec entry（车道 B）
- `88a9770` feat: deprecate seal/permit ladder constructors（车道 A）

### 8.2 实现事实

- **车道 C（effect-permit 组合）**：新增 `EffectPermitAuthorities<'a>`（operation+channel 双 Option 槽，Clone/Copy/Default，presence-flag Debug）与 `request_effect_permit_with_authorities_struct` additive 入口——双 gate 在同一事务内依次回读比对（activation proof 先于 channel endpoint proof），都通过才 mint；任一缺席只跑在场 gate；`Default` 复现 legacy 行为；seal→mint 之间任一 authority rotate 均 fail-closed（`TaskWriteSetConflict` / `OperationParticipantAuthority` 精确错误）；失败调用零持久化；replay 只信 Task 行（双 owner 漂移后仍返回同一 durable token）。既有三入口零改动。
- **车道 B（finalize 吸收）**：新增 `FinalizeSpec<'a>`（semantic_authority / semantic_plan / persisted_envelope / authority_lease / resource_authority 五 Option 槽）与 `FinalizeSpecDecision`、`finalize_commit_v3_with_spec` additive 入口——spec 形状 fail-closed（plan 无 Semantic authority、envelope 无 plan 精确 conflict reason），按四象限解构分派既有 `finalize_impl*` inner。钉住并修复两个梯子不可表达缺口：persisted envelope + resource authority（+lease）组合、semantic guard + resource receipts（无 plan）；含重启后空 owner replay 一致性。
- **车道 A（梯子弃用）**：store.rs 15 个梯子变体（seal 8 + permit 7）加 `#[deprecated(since = "0.1.0")]`，note 指引对应 `Authorities` 槽位；逐一核验槽位映射等价。豁免 `request_commit_permit_with_authority_lease`（struct permit 入口无 lease 槽，lease 语义保留专用入口）。25 个测试文件加机械 `#![allow(deprecated)]`（`semantic_convergence.rs` include! 场景改函数/mod 级 allow），零逻辑改动；新增 `ladder_deprecation_equivalence` 双向行为等价测试。

### 8.3 验证

```text
cargo test --workspace（88a9770 后全仓门）
  → 汇总 passed: 558 failed: 0 ignored: 2（两项既有 100K scale probe）

cargo clippy --workspace --all-targets -- -D warnings → 0 warning / 0 error
cargo fmt --all --check → 通过；git diff --check → 通过
```

分车道数字：nlos-task 242（B/C 合并态）→ 244（A 后，+2 等价测试）；nlos-commit-coordinator/-system-control/-takeover-control 68 passed。

### 8.4 等级与未完成（PARTIAL_PASS 保持）

- validation-only，无 schema/model/migrations 变更；不声明跨 authority 原子性（verify-then-commit 不变）。
- 梯子函数只弃用未移除（future breaking change）；`FinalizeSpec` envelope 模式下调用方 `request` 被整体覆盖（与既有 envelope 梯子一致，doc 已注明）。
- CI / push 状态见 §9 提交回执（本节完成后随本轮统一推送）。
