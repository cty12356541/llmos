# B-TASK-008C2G-IPC：TakeoverControl 跨进程签名 barrier 提交服务

状态：`PARTIAL_PASS`（2026-08-18）

## 1. 结论

本切片把 `record_authority_takeover_barrier_receipt_signed` 从进程内 API 提升为真实本地 IPC 服务：新增 `nlos.sabi.TakeoverControl` v1.0（payload 家族 + 64 KiB bound，registry 第 5 个 schema），新 crate `nlos-takeover-control` 提供零状态 handler（持有 `&SqliteTaskAuthority + &IdentityAuthority + pluggable TakeoverControlAuthorizer`），单一 MUTATION 方法 `submit_barrier_observation` 经 `validate_sabi_request_context(MUTATION)`（idempotency key 强制）后委托既有 store 验签路径。与 SystemControl 的 `caller==issuer` 检查刻意解耦：签名属于远端 participant principal，IPC caller 由 capability authorizer 单独授权——这是本切片的显式信任模型决策。同时偿还 B-TASK-006L 明示的债：`TakeoverControlError::to_sabi_failure` 提供完整 typed 错误→SabiFailure 映射表，失败以带 SabiErrorCode/RetryDirective 的合法响应信封跨 IPC 返回而非裸 transport 错误。

## 2. 已实现事实

- **Schema 层**：`schema/nlos/sabi/v1/takeover_control.proto`（5 消息：SubmitBarrierObservationRequest/Target/Evidence/Signature/Record；signer 字段语义注释"来自 verified proof 而非 caller 断言"）；build.rs protos 数组、registry（`SABI_TAKEOVER_CONTROL_SCHEMA`、descriptor、REGISTRY 第 5 项）、8 个 fail-closed CompatibilityError 变体（缺失 target/evidence/signature/signer、participant_type 1..=8、16/32/64B 长度、负时间戳、零 generation、unsigned record）、encode/decode 对 + 全消息私有 validator；TS/Python 生成物 check-in（check-generated 仅新增两文件、既有零漂移）；buf lint/format/tsc 通过。
- **Handler 层**（`crates/nlos-takeover-control`，照 RecoverySystemControl 结构）：`TakeoverControl::handle` 按 service/method 分发；submit 路径 decode → u32→ParticipantType/Generation 防御性映射 → `authorize_submit_barrier_observation` → store 验签 → 响应 record 的 signer 字段全部取自返回记录（verified proof）；`failure_response(request, &error)` 构建保留 request_id/correlation 的合法失败信封。
- **错误映射表**（`to_sabi_failure`，代码内文档化）：identity proof 失败（purpose/binding/revocation/validity/signature/key 查找）→ `RIGHTS + DO_NOT_RETRY`；授权拒绝 → `RIGHTS`；replay 冲突（含 unsigned/signed 混写，按 CorruptRecord 静态消息串区分）→ `CONFLICT`；takeover 状态类（not pending/fence root incomplete/task generation/registry binding mismatch）→ `STATE`；receipt 不存在 → `NOT_FOUND`；participant_type/generation 无效 → `INVALID_ARGUMENT`；store Sqlite/durability 不可用 → `DURABILITY + RETRY_SAME_IDEMPOTENCY_KEY`（幂等重试安全：exact replay 返回存量记录）；identity 存储/锁缺陷 → `DURABILITY`/`DRIVER`；未知方法 → `NOT_SUPPORTED`。
- **测试**（6 项，recovery_control.rs 双 harness + barrier_signature.rs fixture 融合）：①duplex 内存 IPC happy path——record 断言 + 与 store 直调 receipt_id 一致 + durable 行 + coverage `LocallyCovered` + **IPC 级同请求 replay 逐字节一致**；②真实 Unix socket——`UnixListenerAdapter::bind` + `SnapshotDirectory` 注册/negotiate + **`PeerCredentialBinding::from_peer(observed)` + `ExactPeerAuthorizer`（真实 peer 门控，非 AllowPeer stub）** + `PeerIdentity::Unix` 断言 + socket 清理；③错误 purpose key → IPC 层 `RIGHTS` SabiFailure（非 transport 错误）+ 零 durable 行；④篡改签名 → `RIGHTS` + 零行；⑤unsigned→signed 冲突 → `CONFLICT` + 原 unsigned 行完好；⑥未知方法 → `NOT_SUPPORTED`。

## 3. Evidence

- `cargo test -p nlos-takeover-control`：6 项通过；`cargo test -p nlos-schema`：17 项通过（15 基线 + 1 validator 测试 + registry 计数 4→5 适配）。
- `cargo test --workspace --quiet`：431 项全过（415 基线 + 16 新增）。
- `npm run schema:lint` / `schema:typecheck` / check-generated（仅两新文件入册，既有生成物零漂移）：通过。
- `cargo clippy -p nlos-takeover-control -p nlos-schema --all-targets -- -D warnings`：通过；`cargo fmt --check` 清洁。
- 三平台 CI + MSRV 1.97：已通过（[run 32111164855](https://github.com/cty12356541/llmos/actions/runs/32111164855)，head `5c3aba6`；Windows 首轮失败暴露两个平台缺陷已修复：①Fixture 结构体字段声明序 drop 导致 remove_file 先于 SQLite 连接关闭——os error 32；②unix-only 符号在非 unix 目标 unused imports/dead_code——改为 cfg(unix) 内导入 + `cfg_attr(not(unix), allow(dead_code))` 精准豁免）。Schema fuzz smoke [run 32109263752](https://github.com/cty12356541/llmos/actions/runs/32109263752) 同批通过。

## 4. 明确限制

- Rust-only conformance（SystemControl 先例）；TS/Python 客户端 conformance 与 SDK facade 延后至独立切片。
- 防重放 = SABI idempotency key + store exact-replay 语义；**无时间窗 anti-replay**（trusted clock 是独立未来 authority），`observed_at_ms` 的 wall-clock 真实性未强制（identity key validity 已有界）。
- `ExactPeerAuthorizer` 绑定 OS 凭证（pid/uid/gid），还不是 NLOS principal 级双向 peer auth/attestation；`TakeoverControlAuthorizer` 测试中为受控 stub，未接真实 Capability authority。
- 错误映射中 CorruptRecord 按 store 静态消息串区分 CONFLICT vs STATE——串是 authority 内 `'static` 稳定值，但属结构耦合，store 侧消息变更需同步。
- 覆盖 TakeoverControl 单方法单调用方场景；无并发多 caller 竞争、无 Windows named pipe handler round-trip、无崩溃注入（IPC 传输层自身的故障矩阵超出本切片）。
