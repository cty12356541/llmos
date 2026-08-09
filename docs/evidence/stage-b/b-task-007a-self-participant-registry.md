# B-TASK-007A：TaskStore self-participant registry

## 1. 验收对象

本切片落实 v0.5 `[DIST-TASK-001]`、`[DIST-TASK-004]` 与 `[TASK-COMMIT-001]` 的最小 authority-first registry 基线：TaskAuthority 不能在没有 durable participant generation/root 的情况下签发新 CommitPermit，也不能让调用者自报 TaskStore endpoint identity。

## 2. 实现事实

- `nlos-types` 新增稳定 nominal `TaskParticipantId` / `TaskParticipantRegistryId`；schema v11 在 authority database 首次创建时用 SQLite `randomblob(16)` 分配并持久化唯一 TaskStore participant identity，外部 API 没有注入入口。
- 新 Task 注册的同一事务创建 generation 1 OPEN registry，只含 TaskStore self participant、generation 1 与 authority 派生 Admission Receipt；registry root 覆盖 Task/generation、registry generation/prior root 和完整有序 participant tuple。
- registry、participant 与 create/freeze Receipt 均 durable；identity/root/participant/Receipt 在 storage trigger 层不可改删，只有 registry lifecycle state 可做受限 CAS。
- CommitPermit issuance 在原 permit/control epoch transaction 内把当前 OPEN registry CAS 为 `FROZEN_FOR_PERMIT`，追加 freeze Receipt，并逐位复制 registry generation/root 到 permit；失败会连同 freeze 一起回滚。
- permit exact replay 返回原 binding。旧 permit 已知闭合后的下一次竞争不会解冻旧 registry，而是从其 root 建立新 generation、复制完整 participant set，再冻结新 generation。
- schema v10→v11 保留既有 permit 为显式 `participant_registry_binding=None`，不为历史事实伪造 proof；历史迁移测试的结构化 downgrade 可识别完整 v11 schema 并安全重盖版本号。

## 3. 验证

```text
cargo test -p nlos-task
cargo clippy -p nlos-task --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

结果：原 TaskAuthority 全套测试及 3 项新增 participant registry integration tests 全部通过，全 workspace check/test/Clippy 零失败、零警告。新增测试覆盖 authority-assigned self identity/restart/DDL immutability、permit-time atomic freeze/replay、permit closure 后新 generation/prior-root chaining。

## 4. 证据等级与限制

结论：`PARTIAL PASS / H3 local TaskStore participant baseline`。

- 当前 registry 只包含 TaskStore self participant；尚无外部 participant registration API，因此不能宣称完整覆盖 Artifact head、Semantic admission、Channel/Topic、Driver gateway 或 Resource/Ledger endpoint。
- 这是刻意的 strict authority-first 边界：在各 endpoint 提供 authority-assigned identity/generation 和不可外部构造 proof 前，不接受 caller-supplied participant tuple。
- 还没有 TaskAuthorityAssignment/term/control-epoch takeover、FROZEN_FOR_TAKEOVER/exact fence set、authority signature、跨进程 admission 或 fault VFS matrix。
- EffectPermit/TaskCommitReceipt 尚未逐位复制并在线重验 registry binding；本切片只建立 CommitPermit issuance gate。

下一验收门：`B-TASK-007B` 为 Semantic admission 与 Artifact head 建立 verified endpoint proof，提供 OPEN registry generation CAS registration，并在 seal/permit 前验证完整 planned participant set。
