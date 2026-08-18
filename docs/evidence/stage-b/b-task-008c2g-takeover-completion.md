# B-TASK-008C2G-COMPLETION：takeover 完成与 successor assignment 激活

状态：`PARTIAL_PASS`（2026-08-18）

## 1. 结论

本切片解锁 v32–v35 刻意推迟的最终语义：当 pending takeover receipt 的 barrier coverage 达到 `LocallyCovered` 且 **manifest 全部成员的观察都经 principal 验签**（v36 signer 列非空）时，`complete_authority_takeover` 在单个 `BEGIN IMMEDIATE` 事务内原子完成——receipt `barrier_state Pending→Complete` 并写入派生的 `new_assignment_id`、旧 assignment `TakeoverPending→Fenced` CAS、插入新 term 的 successor assignment（Active）。schema v37 以 v24 式整表拷贝放宽 v32 的两处表级 CHECK（`barrier_state IN (1,2)`、`new_assignment_id` NULL 或 16B），并把无差别 immutable trigger 窄化为"仅允许唯一合法终态转移"（其余任何 UPDATE 仍 ABORT）。BARRIER-SIG 的验签工作在本切片兑现为协议强制：**unsigned 观察不足以激活 successor**。

## 2. 已实现事实

- **schema v37**：`migrate_v37` + `SCHEMA_V37_SQL`（v24 整表拷贝先例：DROP 双 trigger → 建临时表（仅放宽两处 CHECK）→ 显式列 INSERT…SELECT → DROP 旧表 → RENAME → 重建窄化 immutable + 原样 no_delete → `PRAGMA user_version=37`）。**FK 处理**：本表是 barrier_receipts 的 FK 父表且 authority 连接 `foreign_keys=ON`（open_with_vfs:122），DROP 父表会因子行触发 FK violation——迁移在事务外显式 `PRAGMA foreign_keys=OFF` 再恢复 ON（pragma 事务内无效，故置于事务两侧）。幂等探测：`sqlite_master.sql` 的 trigger 文本含 `OLD.barrier_state`/`NEW.barrier_state` 守卫即已迁移；混合/缺失形状 → `CorruptRecord("partial v37 takeover completion schema")`。
- **窄化 trigger**：BEFORE UPDATE，仅放行 `OLD.barrier_state=1 AND NEW.barrier_state=2 AND NEW.new_assignment_id NOT NULL AND OLD.new_assignment_id IS NULL` 且其余 19 列逐列 `IS NOT` 相等；authority_lease.rs:718 的裸 `SET new_assignment_id` 断言在新 trigger 下仍正确 ABORT（既有测试零修改通过）。
- **API `complete_authority_takeover(request)`**：时间戳校验 → 加载 receipt（ReceiptNotFound）→ `request.lease.binding() != receipt.new_authority_lease_binding` → `AuthorityLeaseBindingMismatch`（replay 亦强制）→ **replay 短路**：已 Complete 时验证 successor assignment Active 且 id 一致，返回以 assignment.created_at_ms 为 completed_at 的记录（durable 确定性，跨重启逐位相等）→ Pending 路径：`validate_authority_lease_binding_in_transaction`（新 term lease 仍 live 且逐位等于 receipt 绑定；续约后 epoch 变化会 fail-closed）→ **coverage 内联重算**（不复用 Pending-only 的 inspect API）：manifest 加载 + root 复算 + 逐观察绑定校验（mirror 1952-1963）→ missing 集非空或 root None/manifest 空 → typed 拒绝 → **全签名门**：任一观察 `signer.is_none()` → `BarrierObservationUnsigned`（新增的唯一 TaskStoreError 变体）→ 派生 successor id → UPDATE receipt 终态转移 → 旧 assignment CAS `TakeoverPending→Fenced`（错态 → `AuthorityLeaseFenced`）→ 插入 successor assignment（Active，binding 取 receipt 的 new_authority 列）→ commit。
- **明确不变**：registry 保持 `FrozenForTakeover`（successor term 下的新 registry generation/permit 签发是下一门）；barrier 观察表/trigger、fence 表、adoption（reconcile）零改动；`ensure_active_assignment` 未复用（其拒绝非 Active 既有态）。
- **不声称**：签名证明 signer 认可观察材料，不证明远端 barrier 物理完成（`[LEASE-FENCE-001]` 的完整 barrier ACK 语义仍属远端验证后续）；cross-term adoption 未实现。

## 3. Evidence

- `cargo test -p nlos-task --quiet`：176 项全过（167 基线 + 9 新增）；既有测试改动仅为 4 处 user_version 戳 36→37。
- 新 `tests/takeover_completion.rs` 9 项：signed LocallyCovered 完成链（Complete + successor Active + 旧 Fenced + registry 仍冻结 + 重启回读）、byte-equal replay、unsigned 拒绝（`BarrierObservationUnsigned`，零状态变更）、ManifestUnavailable 拒绝（NULL exact root 的未终结 permit fixture，见 §4 取舍）、expired lease 拒绝（`AuthorityLeaseExpired`）、wrong/stale lease 拒绝（term-3 抢占后 `AuthorityLeaseFenced`）、完成后 immutability（回退/改 id/改他列/DELETE 全 ABORT）、v36→v37 迁移（伪造 v36 形状重开迁移、receipt 逐位存活、`PRAGMA foreign_key_check` 空、raw FK-ON INSERT-then-ROLLBACK 证明子 FK 解析、迁移库上完整激活链）、fault-VFS IoErr 行（typed 失败零半截状态、disarm 后成功）。
- 三套 takeover 系 fault 矩阵回归：takeover(8)/barrier_signature(7)/lease_binding(7) 全绿。
- `cargo clippy -p nlos-task --all-targets -- -D warnings` 通过；`cargo fmt --check` 清洁；`cargo build -p nlos-commit-coordinator -p nlos-system-control -p nlos-takeover-control` 通过。
- `cargo test --workspace --quiet`：440 项全过（431+9）。三平台 CI + MSRV 已通过（run 32115886849，head `b52ca2a`，四平台一次全绿）。

## 4. 明确限制

- T4 采用 ManifestUnavailable 变体而非 ≥2 成员 manifest：构造多成员 manifest 需 owner 验证 endpoint/process-binding 写集路径（不成比例）；ManifestUnavailable 与 missing-member 走同一内联 coverage 门（root Some + manifest 非空 + 差集空），已覆盖关键分支。
- 完成语义是单 authority 本地的：观察的 signer 是"该观察材料的签名者"，principal↔participant 绑定策略（哪个 principal 有权为哪个 endpoint 签观察）尚未定义——当前任何 `BarrierObservationSigning` key 的有效签名都满足门。
- replay 返回的 completed_at_ms 取自 successor assignment 的 created_at_ms（durable），与请求时间戳不同也不拒绝（决策已 durable）；但 replay 仍强制 lease binding 与 receipt 一致。
- 迁移期间 `foreign_keys` 短暂 OFF（仅 v37 拷贝窗口，事务外切换）；kill-9 落在迁移事务内的行为由 SQLite 原子性保证，但未对 v37 迁移本身注入故障（后续 F4 全集矩阵的可选项）。
- registry 在完成后仍保持冻结，直到独立的 successor-registry hand-off；该 hand-off
  现由 [B-TASK-008C2G-SUCCESSOR-REGISTRY](./b-task-008c2g-successor-registry.md)
  提供本地 generation/assignment rotation 与新 permit 接线。cross-term adoption
  （含放宽 `reject_takeover_fence`/`validate_permit_authority_lease` 的旧 term 拒绝）
  仍为 NEXT。
