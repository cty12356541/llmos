# B-DRIVER-001：最小 provider driver plane（确定性 fake provider）

- 状态：`PARTIAL`
- 日期：2026-09-20（W30-B）／ 2026-09-21（W30-C 追加）
- Owner：`nlos-driver-mock`（`MockProvider` 核心 + 认证 typed IPC 面 + provider cache 降级面）
- 关联 Requirement：`MODEL-OP-001`、`IO-ASYNC-001`、`TCB-ENDPOINT-001`（对齐 [ADR-0002](../../management/adrs/0002-stage-b-sqlite-operation-authority.md)）；ROAD-B-003 B3-4 前半（W30-B）＋后半（W30-C）
- 关联 ADR：[ADR-0002](../../management/adrs/0002-stage-b-sqlite-operation-authority.md)（SQLite Operation 权威）、[ADR-0011](../../management/adrs/0011-ipc-principal-auth-signature-passthrough.md)（IPC principal 认证）、[ADR-0013](../../management/adrs/0013-cross-authority-verify-then-commit-contract.md)（owner 回读在 Task 事务之外——降级不得破坏此边界，本切片全部 owner 事实取自权威回读，无 Task 事务侧自报）
- 关联车道：W30-B（driver 面本体）；W30-C（provider cache 降级 + 投机副作用 fence，本节）；三平台复验与 §6 ROAD-B-003 行更新归 W30-E

## 证明范围

本切片为 ROAD-B-003 补上缺失的 provider 面：新 crate `crates/nlos-driver-mock`
提供确定性 fake provider，两个面共享同一个核心，无任何私有 canonical 状态：

1. **核心（`provider::MockProvider`，进程内 handle）**：
   - `register` 幂等落入 `SqliteOperationStore::register`，回执取
     `inspect_endpoint_proof` 的 authority 派生 admission receipt；
   - `dispatch` 走 B-OP-FENCE-002 的 durable prepare→activate 边界
     （`prepare_dispatch` → `activate_dispatch`），一次性 callback ticket、
     preparation/activation receipt 全部由权威返回；
   - `complete` 从 durable 行重建 owner-bound `CallbackTicket`（绝不采信调用方
     自报 owner 事实），终态 outcome 由 domain-separated SHA-256 从
     `(operation, callback, seed)` 确定性派生（receipt 取前 16 字节、
     digest[16] 选择 Completed/Failed/PartialEffect/EffectUnknown），提交走
     callback identity fence。无墙钟、无随机数：同一 seed 与同一 durable 库
     逐字节复现同一终态。
2. **typed IPC 面（`ipc` + `authenticated`，Unix）**：SABI `Envelope` 上的
   register/dispatch/complete 三个 `MUTATION` 方法；payload 为 crate 内
   确定性 minicbor map 编码（严格字段数/升序 key/定宽 id/失败即关 schema
   身份）。服务入口**只有** ADR-0011 challenge-response 认证路径
   （`AuthenticatedMockDriverServer::serve_one`：OS pre-gate → challenge →
   attestation 经 `IdentityAuthority` 验签、`verified_at_ms` 取
   `AuthorityClock::inspect_wall` → 验证过的 principal 进入每次授权决策）；
   本 crate 无明文 IPC 捷径。授权经注入的 `MockDriverAuthorizer`，无默认
   放行策略。
3. **重启 replay**：provider 进程 drop + 重启（store/identity/clock 全部
   重开、同 socket 路径重绑）后，in-flight dispatch 精确 replay：同一
   preparation/activation receipt 与 callback ticket（`replayed=true`），
   不二次派发；已完成操作按 operation identity 幂等重放终态；换 seed
   重放已终态 callback 被 `CallbackIdentityConflict` 拒绝。

## 验证

- `crates/nlos-driver-mock/tests/provider_core.rs`（9 项，跨平台）：全链
  register→dispatch→complete 落库与回执、in-flight dispatch 重启精确 replay、
  已完成操作重启幂等 replay、seed 确定性派生、冲突 spec 复用拒绝、
  complete-before-dispatch 拒绝、伪造 callback 拒绝、终态后换 seed 冲突、
  exact register replay。
- `crates/nlos-driver-mock/tests/ipc_authenticated_chain.rs`（5 项，Unix）：
  认证 IPC register/dispatch/complete 全链（响应回执 = preparation/
  activation/terminal receipt）、IPC 重启 replay（dispatch 精确重放 +
  complete 首次/重放）、未知 principal 拒绝、伪造签名拒绝、attestation
  重放拒绝（nonce 单次消费）。
- `cargo test -p nlos-driver-mock`：14 passed / 0 failed。
- `cargo clippy -p nlos-driver-mock --all-targets --all-features -- -D warnings`
  通过（含 pedantic）；`cargo fmt --all -- --check` 通过。
- Windows 侧：核心与 IPC handler（非 transport）代码跨平台编译；认证
  transport 服务入口与 5 项 IPC 测试为 `#[cfg(unix)]`（见未完成）。
- 代码与证据同提交（`feat/w30-b`）；三平台 CI 复验与 §6 ROAD-B-003 行更新归 W30-E。

## W30-C 追加：provider cache 降级与投机副作用 fence（2026-09-21）

ROAD-B-003 六项语义门的最后两项（provider cache 降级、投机副作用 fence）在本切片闭合，
`nlos-driver-mock` 新增（TDD：先写失败测试再实现）：

1. **provider RPC 故障注入旋钮（`provider::ProviderFaultMode`）**：
   `nlos-store-fault` 的 arm/disarm/observed 模式上移到 provider RPC 边界——
   `MockProvider::arm_fault(FailProviderRpc)` 后，register/dispatch/complete 三面全部先过
   `ProviderFaultGate`（原子 mode + `rpcs_observed` 计数），typed 返回
   `ProviderError::Unreachable` 且不触任何 durable 写。旋钮为进程内存态，随 provider
   进程 drop 消亡（重启即恢复，与 W30-B "重启以 store 重开模拟" 同口径）。
   `store()` 读路径**刻意不在** gate 之后：provider 不可达不等于本地权威行不可查。
2. **共享 provider cache 降级态（`cache::ProviderCache`）**：消费侧共享
   `Arc<ProviderCache>`，`CacheHealth::{Healthy, Degraded{cause}}` 只记健康不记操作
   态——降级窗口不可能制造过期成功证据，恢复也不需要任何 cache 侧失效协议，直接经
   durable replay 收敛。健康转移是观测性的（每次调用仍真实尝试 RPC；确定性 mock 必须
   报告 provider 当前真值而非缓存裁定）：Unreachable ⇒ Degraded，任何 provider 应答
   （成功或 typed 权威拒绝）⇒ Healthy；熔断迟滞是生产策略不是语义底线，显式不含。
3. **IPC 面有界降级失败**：`MockDriverError::ProviderUnreachable` 映射 SABI
   `HostLost` + `RetrySameIdempotencyKey`（三个方法均按 operation/callback identity
   幂等，同字节重试安全）；失败信封不带 payload、不带回执。
4. **投机副作用 fence（`MockProvider::complete` 的 ticket 重建源收紧）**：一次性
   callback ticket 的 cancel epoch 改取 `inspect_activation_proof` 的 invariant-checked
   发放 epoch（原实现取当前行 epoch，窗口内 cancel 前进 epoch 后晚到的 provider 完成会
   被 `InvalidGeneration` 整体丢弃——证据损失而非 fence）。收紧后的语义：
   - **陈旧 generation 的投机 dispatch ⇒ typed `InvalidGeneration` 拒绝，零 durable
     痕迹**（无 preparation、无 activation、无 outbox；正确 generation 仍恰好开一次
     dispatch 边界）；
   - **cancel epoch 赢过晚到的投机完成**：epoch 已前进时晚到的完成只被
     canonicalize-for-reconciliation（outbox 只有 `ReconcileEffect`，永不 `WakeFiber`），
     事后换 seed 仍是 `CallbackIdentityConflict`；
   - **窗口内 cancel 后的再投机 dispatch**：换 callback ⇒ `DispatchPreparationConflict`，
     原 callback ⇒ 只重放原 ticket，均不开第二效应边界；
   - **fence 窗口跨重启恰好一次提交**：重启后重放 completion 是 Duplicate，outbox 不增。
   W30-B 全部 replay 测试保持绿（正常路径 epoch 相等，行为逐字节不变）；唯一错误类
   细化是 complete-before-dispatch 从 `Operation(InvalidState)` 变为更精确的
   `DispatchPreparationNotFound`（仍是 typed fail-closed、零部分状态，非弱化）。

### W30-C 验证

- `tests/degradation.rs`（5 项，跨平台）：`degraded_provider_fails_typed_and_preserves_durable_rows`
  （三面 typed Unreachable、`rpcs_observed` 精确计数、durable 行保持降级前状态、零 outbox 证据）、
  `recovery_after_provider_returns_replays_and_converges`（恢复后 dispatch 精确 replay 同
  ticket/回执、complete 幂等重放、健康回 Healthy）、
  `degradation_window_survives_provider_restart_and_converges`、
  `shared_cache_degradation_is_visible_to_every_consumer`（共享降级/共享恢复）、
  `typed_authority_rejection_does_not_degrade_the_cache`（权威 typed 拒绝 ≠ 降级）。
- `tests/effect_fence.rs`（3 项，跨平台）：`stale_generation_speculative_dispatch_is_typed_rejected_without_effect_commit`
  （窗口内投机 + 恢复后重试均被 generation fence 拒绝；`inspect_activation_proof` 的
  `DispatchPreparationNotFound` 证明零 preparation 残留；正确 generation 恰好一次 dispatch）、
  `cancel_epoch_wins_over_late_speculative_completion`（outbox 仅 `ReconcileEffect`；
  重放 Duplicate；换 seed `CallbackIdentityConflict`）、
  `fence_window_restart_commits_exactly_once`（两次重启、单次提交、冲突持续）。
- `tests/ipc_degradation.rs`（1 项，Unix）：认证 IPC 上降级 ⇒ `HostLost` +
  `RetrySameIdempotencyKey` 有界失败（无 payload/回执泄漏、durable 行保持 Registered），
  provider 返回后同请求字节（同 request id/幂等键）重试成功收敛，四轮 serve 全部留在
  typed 面内。
- `cargo test -p nlos-driver-mock`：23 passed / 0 failed（W30-B 14 项全数保持绿）。
- `cargo clippy -p nlos-driver-mock --all-targets --all-features -- -D warnings`
  通过（含 pedantic）；`cargo fmt --all -- --check` 通过。
- ADR-0013 边界复核：降级/fence 全部语义经 provider 面（owner 回读在权威侧完成），
  无 Task 事务侧自报 owner 事实引入；verify-then-commit 契约不受影响。
- 代码与证据同提交（`feat/w30-c`）；三平台 CI 复验与 §6 ROAD-B-003 行更新归 W30-E。

## 明确未完成

- ~~provider cache 降级与投机副作用 fence（ROAD-B-003 最后两项）是 W30-C~~：已于
  W30-C 落地（见上节）；三平台复验与 §6 行更新待 W30-E。
- payload codec 是 crate 内确定性 CBOR，未进 `nlos-schema` 冻结 protobuf
  通道；driver 面升级为正式 schema 通道需显式决策（ADR-0014 冻结纪律）。
- 认证 transport 入口当前仅 Unix（`nlos-ipc` handshake transport 的
  named-pipe 对应面未接）；Windows 认证入口待后续波次。
- server 语义为每连接一请求（`serve_one` 先例），长连接循环服务、并发
  连接管理与 OS 级 RNG nonce 供给（生产 host 接线）未含。
- 未含 conformance server binary、跨进程真实 kill 注入（重启语义以
  store 重开模拟，与 B-OP-FENCE-002 同口径）；真实 driver（非 mock）、
  生产级 provider cache（熔断迟滞/探测退避等策略）、跨 authority
  TaskWriteSet 接线均不在本切片（本切片的 cache 语义刻意只含观测性降级态）。
