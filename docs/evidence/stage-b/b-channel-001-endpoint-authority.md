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
