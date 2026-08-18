# B-TASK-008C2G-BARRIER-SIG：takeover barrier observation principal 签名验证

状态：`PARTIAL_PASS`（2026-08-18）

## 1. 结论

本切片为 schema v33/v35 的 takeover barrier observation 补上声明已久的"未验证远端签名"缺口的 authority 层第一半：观察提交方现在必须提供 NLOS principal 的 Ed25519 签名，由 `nlos-identity` key authority 按 binding/purpose/validity/revocation/strict-verify 链验证通过后，观察才能落盘。新增 `KeyPurpose::BarrierObservationSigning = 2`（既有 key 全部为 SemanticSigning，语义不变）；`nlos-task` schema v36 为 observation 表增加五个可空 signer 列（principal/control domain/key/key generation/signature），旧行保持 `NULL` 不伪造身份事实，coupled 触发器强制五列同现同缺。本切片不改变 coverage 判定语义、不推进 parent takeover receipt、不激活 successor assignment——这些仍是下一验收门。

## 2. 已实现事实

- `nlos-identity`：`KeyPurpose` 增加 `BarrierObservationSigning = 2`（encode/decode 接受 {1,2}，decode 其他值保持 fail-closed；schema CHECK 同步放宽为 `purpose IN (1, 2)`，存量行全为 1 不受影响）；新增 `verify_barrier_observation_signature(VerifyBarrierObservationSignatureRequest) -> VerifiedBarrierObservationSigner`，八步验证链与 `verify_semantic_authority_signature` 逐位对齐（load_current_binding → SignerBindingMismatch → KeyPurposeMismatch → KeyRevoked → KeyNotYetValid → KeyExpired → InvalidPublicKey → `verify_strict(message_digest)`），零新增错误变体（23 个既有变体全覆盖）。
- `nlos-task` 新增公开 API `record_authority_takeover_barrier_receipt_signed(identity, request, signature)`：单 `BEGIN IMMEDIATE` 事务内先跑与 unsigned 路径共享的观察核心校验（takeover Pending、`exact_fence_set_root` 存在、registry `FrozenForTakeover` binding、manifest membership + root 复算），然后对 `barrier_observation_signature_message`（domain `llmos/takeover-barrier-observation/v1`，覆盖 takeover_receipt_id、participant 四元组（type u8 + id 16B + generation 8B + admission receipt 16B）、remote_receipt_id、barrier_digest、**服务端权威 fence_set_root**）做 identity 验签，失败即整体回滚零写入。
- 落盘 signer 列取自 **verified proof**（principal/control domain/key id/key generation），不接受 caller 自报身份；signature bytes 为 caller 提供的原始签名。receipt_id 派生域与输入保持 `/v1` 不变（观察身份绑定材料而非签名）。
- schema v36（v35 ALTER 模式）：五列 `signer_principal_id/signer_control_domain_id/signer_key_id`（BLOB 16）+ `signer_key_generation`（INTEGER >=1）+ `signer_signature`（BLOB 64），各列 NULL 或长度 CHECK；`task_authority_takeover_barrier_receipts_signer_coupled` BEFORE INSERT 触发器强制五列全部 `NULL` 或全部存在；部分 schema 状态迁移 fail-closed（`partial v36 barrier signer schema`）；v35→v36 幂等迁移 + golden 旧行回读 signer=None 已验证。
- 重放语义：signed 路径命中既有行时要求全等（含 signer 字段）；unsigned→signed 或签名不同 → `CorruptRecord("takeover barrier receipt changed during replay")`；unsigned 路径行为逐位不变（既有测试零改动通过，除 6 处 user_version 戳 35→36 适配）。
- readback：`AuthorityTakeoverBarrierReceiptRecord.signer: Option<AuthorityTakeoverBarrierSigner>` 跨重启逐位回读；部分列存在（pre-trigger 数据）读回 fail-closed。
- 跨进程依赖方向 `nlos-task → nlos-identity`（L1→L4，与 nlos-semantic→nlos-identity 同构）；`TaskStoreError::BarrierSignerIdentityAuthority` 包装外部错误，沿用 lib.rs 既有 foreign-authority 包装模式。

## 3. Evidence

- `cargo test -p nlos-task --test barrier_signature`：7 项通过——happy path（sign → record → signer 字段=proof、inspect 回读、coverage `LocallyCovered`、逐字节重放）、错误 purpose（SemanticSigning key → 包装 `KeyPurposeMismatch`、零行写入、coverage `Partial`）、篡改签名（→ 包装 `InvalidSignature`、零行）、signer binding mismatch（→ 包装 `SignerBindingMismatch`、零行）、unsigned→signed 冲突（→ `CorruptRecord`、原 unsigned 行完好）、重启回读（signer 逐位相等）、v35→v36 迁移（旧行 signer=None、user_version=36）。
- `cargo test -p nlos-identity`：8 集成 + 1 单元测试通过（基线 5 + 新增 4：barrier happy path、错误 purpose、篡改签名、KeyPurpose codec 回环/拒绝 3）。
- `cargo test --workspace --quiet`：415 项全过（404 基线 + 11 新增）。
- `cargo clippy -p nlos-identity -p nlos-task --all-targets -- -D warnings`：通过。
- `cargo fmt -p nlos-identity -p nlos-task -- --check`：通过。
- `cargo build -p nlos-commit-coordinator -p nlos-system-control`：通过（公开 API 兼容）。
- 三平台 CI + MSRV 1.97 job：已通过（[run 32099012698](https://github.com/cty12356541/llmos/actions/runs/32099012698)，head `278ae53`）。

## 4. 明确限制

- 验证的是 principal 对观察材料的签名与 key 生命周期，不是跨进程通道本身：签名材料如何从远端 endpoint 安全抵达本 authority（IPC transport、防重放窗口、时钟信任）仍未接线，属于 B-TASK-008C2G 的 cross-process 后续切片。
- preimage 中的 `fence_set_root` 取服务端权威值，但 `remote_receipt_id`/`barrier_digest` 仍由 caller 提供——签名只证明 signer 认可这些 bytes，不证明远端 barrier 真实完成（`[LEASE-FENCE-001]` 的完整 barrier ACK 语义仍由后续 parent completion 切片承担）。
- `KeyPurpose::BarrierObservationSigning` 尚无 capability/授权策略约束谁能持有 barrier 签名 key（当前与 SemanticSigning 同为 trusted-bootstrap 签发）。
- unsigned 路径保留（同信任域本地使用），不强制既有调用方迁移。
- kill-9/ENOSPC/torn-WAL 对签名写路径的注入矩阵、三平台 CI 复验未运行；旧 v1 identity 库（CHECK purpose=1）无法签发 purpose=2 key，需按 fail-closed 语义重建（reference slice，无生产库）。
