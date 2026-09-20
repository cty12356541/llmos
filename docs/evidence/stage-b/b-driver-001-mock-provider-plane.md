# B-DRIVER-001：最小 provider driver plane（确定性 fake provider）

- 状态：`PARTIAL`
- 日期：2026-09-20
- Owner：`nlos-driver-mock`（`MockProvider` 核心 + 认证 typed IPC 面）
- 关联 Requirement：`MODEL-OP-001`、`IO-ASYNC-001`、`TCB-ENDPOINT-001`（对齐 [ADR-0002](../../management/adrs/0002-stage-b-sqlite-operation-authority.md)）；ROAD-B-003 B3-4 前半（W30-B）
- 关联 ADR：[ADR-0002](../../management/adrs/0002-stage-b-sqlite-operation-authority.md)（SQLite Operation 权威）、[ADR-0011](../../management/adrs/0011-ipc-principal-auth-signature-passthrough.md)（IPC principal 认证）
- 关联车道：W30-B（本切片）；W30-C（provider cache 降级 + 投机副作用 fence，显式不在本切片）

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

## 明确未完成

- **provider cache 降级与投机副作用 fence（ROAD-B-003 最后两项）是 W30-C**，
  依赖本切片的 driver 面；本 crate 不含任何 cache/fence 语义。
- payload codec 是 crate 内确定性 CBOR，未进 `nlos-schema` 冻结 protobuf
  通道；driver 面升级为正式 schema 通道需显式决策（ADR-0014 冻结纪律）。
- 认证 transport 入口当前仅 Unix（`nlos-ipc` handshake transport 的
  named-pipe 对应面未接）；Windows 认证入口待后续波次。
- server 语义为每连接一请求（`serve_one` 先例），长连接循环服务、并发
  连接管理与 OS 级 RNG nonce 供给（生产 host 接线）未含。
- 未含 conformance server binary、跨进程真实 kill 注入（重启语义以
  store 重开模拟，与 B-OP-FENCE-002 同口径）；真实 driver（非 mock）、
  provider cache、跨 authority TaskWriteSet 接线均不在本切片。
