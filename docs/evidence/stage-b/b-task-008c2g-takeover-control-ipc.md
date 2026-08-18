# B-TASK-008C2G-IPC：TakeoverControl 跨进程签名 barrier 提交服务

状态：`PARTIAL_PASS`（2026-08-19）

## 1. 结论

本切片把 `record_authority_takeover_barrier_receipt_signed` 从进程内 API 提升为真实本地 IPC 服务：新增 `nlos.sabi.TakeoverControl` v1.0（payload 家族 + 64 KiB bound，registry 第 5 个 schema），新 crate `nlos-takeover-control` 提供零状态 handler（持有 `&SqliteTaskAuthority + &IdentityAuthority + pluggable TakeoverControlAuthorizer`），单一 MUTATION 方法 `submit_barrier_observation` 经 `validate_sabi_request_context(MUTATION)`（idempotency key 强制）后委托既有 store 验签路径。与 SystemControl 的 `caller==issuer` 检查刻意解耦：签名属于远端 participant principal，IPC caller 由 capability authorizer 单独授权——这是本切片的显式信任模型决策。同时偿还 B-TASK-006L 明示的债：`TakeoverControlError::to_sabi_failure` 提供完整 typed 错误→SabiFailure 映射表，失败以带 SabiErrorCode/RetryDirective 的合法响应信封跨 IPC 返回而非裸 transport 错误。2026-08-19 增量再加入 feature-gated `takeover-control-conformance` server：TypeScript/Python 客户端通过生成的 TakeoverControl payload 真实提交签名 observation，在同一 handler 上完成第二连接的 durable replay；Unix socket 与 Windows named-pipe 均复用同一 server 入口。随后并发增量在两个独立 IPC caller 上并行提交同一 signed observation，验证单 writer 序列化、单 durable row 和 verified record 一致性。最新故障增量又在同一实际 IPC handler 上注入 SQLite VFS `IOERR`/`ENOSPC` 写失败：失败均返回 typed `DURABILITY + RETRY_SAME_IDEMPOTENCY_KEY` 且不落 durable row，解除注入后用同一 key 重试恢复为一个已验签、`LocallyCovered` 的记录；再增加 `PowerLossAfter` 静默丢写模型，证明 IPC 返回的幻影记录在重开后不可见、同 key 重做恢复为单行 durable 记录。

## 2. 已实现事实

- **Schema 层**：`schema/nlos/sabi/v1/takeover_control.proto`（5 消息：SubmitBarrierObservationRequest/Target/Evidence/Signature/Record；signer 字段语义注释"来自 verified proof 而非 caller 断言"）；build.rs protos 数组、registry（`SABI_TAKEOVER_CONTROL_SCHEMA`、descriptor、REGISTRY 第 5 项）、8 个 fail-closed CompatibilityError 变体（缺失 target/evidence/signature/signer、participant_type 1..=8、16/32/64B 长度、负时间戳、零 generation、unsigned record）、encode/decode 对 + 全消息私有 validator；TS/Python 生成物 check-in（check-generated 仅新增两文件、既有零漂移）；buf lint/format/tsc 通过。
- **Handler 层**（`crates/nlos-takeover-control`，照 RecoverySystemControl 结构）：`TakeoverControl::handle` 按 service/method 分发；submit 路径 decode → u32→ParticipantType/Generation 防御性映射 → `authorize_submit_barrier_observation` → store 验签 → 响应 record 的 signer 字段全部取自返回记录（verified proof）；`failure_response(request, &error)` 构建保留 request_id/correlation 的合法失败信封。
- **Conformance server**：`takeover-control-conformance` 只在 `conformance-server` feature 下构建，启动 deterministic pending takeover fixture 并输出 bounded key/value manifest；服务两次 one-request 连接，第二次必须返回相同 durable observation。fixture 只用于测试，不是生产 daemon。
- **错误映射表**（`to_sabi_failure`，代码内文档化）：identity proof 失败（purpose/binding/revocation/validity/signature/key 查找）→ `RIGHTS + DO_NOT_RETRY`；授权拒绝 → `RIGHTS`；replay 冲突（含 unsigned/signed 混写，按 CorruptRecord 静态消息串区分）→ `CONFLICT`；takeover 状态类（not pending/fence root incomplete/task generation/registry binding mismatch）→ `STATE`；receipt 不存在 → `NOT_FOUND`；participant_type/generation 无效 → `INVALID_ARGUMENT`；store Sqlite/durability 不可用 → `DURABILITY + RETRY_SAME_IDEMPOTENCY_KEY`（幂等重试安全：exact replay 返回存量记录）；identity 存储/锁缺陷 → `DURABILITY`/`DRIVER`；未知方法 → `NOT_SUPPORTED`。
- **测试**（10 项 Rust handler/IPC 回归 + 2 项跨语言 conformance）：Rust 覆盖 duplex 内存 IPC happy path、两个独立 caller 并发提交的单 writer 线性化、真实 Unix socket + `SnapshotDirectory`/`ExactPeerAuthorizer`、错误 purpose/篡改签名/unsigned→signed 冲突/未知方法，`IOERR`/`ENOSPC` 写故障的 typed durability 映射、零幻影行和同 key 恢复，以及 `PowerLossAfter` 静默丢写后的重开隐藏与同 key 恢复；新增 TypeScript 与 Python 各启动同一 feature-gated server，按 manifest 构造 `SubmitBarrierObservationRequest`，通过真实 Unix socket（Windows CI 为 named pipe）提交签名 observation、校验 verified signer/Receipt response，并在第二连接逐字节 replay。server 退出前还验证 durable row 数为 1、coverage 为 `LocallyCovered`。

## 3. Evidence

- `cargo test -p nlos-takeover-control --features conformance-server`：10 项通过；`cargo test -p nlos-schema`：17 项通过（15 基线 + 1 validator 测试 + registry 计数 4→5 适配）。
- `cargo test --workspace --quiet`：当前 448 项全过（另 2 项既有 scale probe ignored）。
- `npm run schema:lint` / `schema:typecheck` / check-generated（仅两新文件入册，既有生成物零漂移）：通过；`npm run takeover-control:test:typescript` 与 `python tests/conformance/ipc/takeover_control.py` 均通过。
- `cargo clippy -p nlos-takeover-control -p nlos-schema --all-targets -- -D warnings`：通过；`cargo fmt --check` 清洁。
- TakeoverControl 旧版三平台 CI + MSRV 1.97：已通过（[run 32111164855](https://github.com/cty12356541/llmos/actions/runs/32111164855)，head `5c3aba6`；Windows 首轮失败暴露两个平台缺陷已修复：①Fixture 结构体字段声明序 drop 导致 remove_file 先于 SQLite 连接关闭——os error 32；②unix-only 符号在非 unix 目标 unused imports/dead_code——改为 cfg(unix) 内导入 + `cfg_attr(not(unix), allow(dead_code))` 精准豁免）。本增量 TypeScript/Python + named-pipe workflow 已由三平台 + MSRV run [32192662820](https://github.com/cty12356541/llmos/actions/runs/32192662820)（head `1519b94`）通过，Pages run [32192662841](https://github.com/cty12356541/llmos/actions/runs/32192662841) 成功。Schema fuzz smoke [run 32109263752](https://github.com/cty12356541/llmos/actions/runs/32109263752) 同批通过。
- 第九增量并发测试已由三平台 + MSRV run [32194086430](https://github.com/cty12356541/llmos/actions/runs/32194086430)（head `4a56832`）通过，Pages run [32194086576](https://github.com/cty12356541/llmos/actions/runs/32194086576) 成功。该 run 的 macOS 首次 workspace job 被既有 `nlos-commit-coordinator` 测试的偶发索引越界打断，重跑失败 job 后全绿；并发切片自身未出现反例。
- 第十增量的 IPC `IOERR`/`ENOSPC` 故障映射与同 key 恢复已在本地 9 项 Rust 测试、workspace 447 项、feature clippy/fmt 中通过；三平台 + MSRV run [32196354141](https://github.com/cty12356541/llmos/actions/runs/32196354141)（head `2afacbf`）已通过，Schema fuzz smoke run [32196354132](https://github.com/cty12356541/llmos/actions/runs/32196354132) 与 Pages run [32196354259](https://github.com/cty12356541/llmos/actions/runs/32196354259) 亦成功。
- 第十一增量的 IPC `PowerLossAfter` 静默丢写、重开隐藏与同 key 恢复已在本地 10 项 Rust 测试、workspace 448 项、feature clippy/fmt 中通过；三平台 + MSRV run [32197845869](https://github.com/cty12356541/llmos/actions/runs/32197845869)（head `f1e9602`）与 Pages run [32197845859](https://github.com/cty12356541/llmos/actions/runs/32197845859) 均成功。

## 4. 明确限制

- 当前已覆盖 TypeScript/Python 的 TakeoverControl payload/response conformance；尚未提供长期发布的 npm/PyPI SDK facade 或版本承诺。
- 防重放 = SABI idempotency key + store exact-replay 语义；**无时间窗 anti-replay**（trusted clock 是独立未来 authority），`observed_at_ms` 的 wall-clock 真实性未强制（identity key validity 已有界）。
- `ExactPeerAuthorizer` 绑定 OS 凭证（pid/uid/gid），还不是 NLOS principal 级双向 peer auth/attestation；`TakeoverControlAuthorizer` 测试中为受控 stub，未接真实 Capability authority。
- 错误映射中 CorruptRecord 按 store 静态消息串区分 CONFLICT vs STATE——串是 authority 内 `'static` 稳定值，但属结构耦合，store 侧消息变更需同步。
- 覆盖 TakeoverControl 单方法、两个 duplex caller 的并发线性化、两次串行 caller 连接、Windows named-pipe round-trip，以及 IPC 入口的 SQLite `IOERR`/`ENOSPC` typed failure/recovery 与 `PowerLossAfter` 静默丢写模型；仍无真实 Unix/named-pipe 多连接压力、进程崩溃/真实断电/WAL 撕裂尾部注入、时间窗 anti-replay。以上 transport 故障矩阵仍属后续门。
