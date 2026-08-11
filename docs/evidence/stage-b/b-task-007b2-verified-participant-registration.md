# B-TASK-007B2：TaskAuthority verified participant registration CAS

## 1. 验收对象

本切片消费 [B-TASK-007B1](./b-task-007b1-authority-endpoint-proofs.md) 的 authority-owned proofs，落实 v0.5 `[DIST-TASK-001]` 的最小 verified registration 路径：TaskAuthority 不接收 caller 构造的 participant tuple，只有 OPEN registry 的精确 generation/root 持有者可以注册新 Artifact/Semantic endpoint。

## 2. 实现事实

- `nlos-task` 的 Artifact 注册 API 只接收 owning `ArtifactStore`、`ArtifactId`、Task 与 expected registry binding；Semantic 注册 API 只接收 owning `SemanticAuthority`、Task 与 expected binding。TaskAuthority 直接调用具体 authority typed inspect API，调用者不能注入 participant identity/generation/Receipt。
- proof readback 在 Task transaction 之前完成；unknown/unavailable/corrupt owner proof 以 typed source error 返回，不修改 Task registry。
- 新 participant 在 `BEGIN IMMEDIATE` 中验证当前 Task generation、重算 registry root、逐位比较 expected generation/root、要求 state=OPEN，再把旧 generation CAS 为 SUPERSEDED，并创建包含完整稳定排序 participant set 的 successor OPEN generation/root/create Receipt。
- participant set 上限为 256；TaskStore self 不能通过外部路径注入；participant identity 或 endpoint Receipt 与已有不同 tuple 冲突时 fail closed。
- 已存在的完全相同 endpoint 收敛为 `Replayed(current_registry)`，不新增 generation；这证明 membership idempotency，不声称历史 registration response 的 byte-exact replay。
- CommitPermit issuance 继续在同一 Task transaction 冻结注册后的最新 generation/root；冻结后新 endpoint registration 被拒绝且 registry 不变。

## 3. 验证

```text
cargo test -p nlos-task
cargo clippy -p nlos-task --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

新增 2 项 integration tests（participant registry 合计 5 项）覆盖 Artifact/Semantic direct readback、successor root chain、duplicate replay、restart、unknown owner proof、stale expected binding、permit-time freeze 与失败零变更；全 workspace check/test/Clippy 零失败、零警告。

## 4. 证据等级与限制

结论：`PARTIAL PASS / H3 local verified registration CAS`。

- 当前 verified registration 只覆盖 Artifact head 与 Semantic admission；Driver gateway、Resource/Ledger、Channel/Topic 尚未建立同级 endpoint proof/registration。
- 本切片注册 endpoint 本身，尚未把具体 staged publication/semantic prepare 的 admission 与 registry CAS 做跨 authority prepare→activate 接线；因此不能宣称完全满足 `[DIST-TASK-001]` 的 operation admission 原子性。
- 没有跨进程签名/attestation、TaskAuthorityAssignment term/lease、takeover fence/barrier Receipt 或 VFS/process-crash fault matrix。
- EffectPermit、TaskCommitReceipt 与 finalize 尚未逐位复制/重验 registry binding。

下一验收门：`B-TASK-007C1` 为 Driver gateway 与 Resource/Ledger 建立 authority-owned endpoint proofs，再接入 Task registry；随后补 EffectPermit/TaskCommitReceipt binding 与在线重验。
