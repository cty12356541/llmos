# B-DEBUG-001：最小 debugger 面（fiber snapshot/replay inspect CLI + 事件流回放）

> 状态：`PARTIAL_PASS`（单机 SQLite 只读检查级 `H3`）
>
> 日期：2026-09-21
>
> 对应：[进度单 §6.5.3](../../management/stage-b-progress.md) W33-G 车道行（X-3 后半，验收门「快照检查/回放可跑」）；[§6.5.4 决策点 2](../../management/stage-b-progress.md#654-决策点状态2026-09-20-更新)已钉死裁剪口径：debugger = snapshot/replay inspect CLI——无 live-attach、无 mutation
>
> 实现：`crates/nlos-runtime-tokio/src/bin/nlos-debug.rs`（`nlos-debug` bin，全 CLI 逻辑与 CLI 级测试同文件，house bin-test 模式，镜像 W33-A `nlos-package` 先例）+ `crates/nlos-runtime-tokio/Cargo.toml`（`rusqlite` 从 dev-deps 晋升为常规依赖——src/bin 目标不能链接 dev-dependencies，注释与 W33-A 的 ed25519-dalek 晋升同款）+ `Cargo.lock`（该依赖变更的机械伴生，根 `Cargo.toml` 零改动）
>
> 依赖前序：[ADR-0009](../../management/adrs/0009-fiber-event-sourced-resume.md)/[ADR-0012](../../management/adrs/0012-fiber-projection-registration-and-entry-snapshot.md)（被检查的 replay/snapshot 机制本体）、B-PROCESS-001/002/003（process 权威 + fiber incarnation + 入口快照）、B-WAIT-001（wait registry）、ADR-0017（三域恢复台账）

## 1. 本切片目标与边界

决策点 2 钉死的边界：debugger 是**只读检查面**——把 ADR-0009/0012 已落地的 durable replay/snapshot 面和 task 权威的三域恢复台账，以 CLI 形式暴露给开发者做事后检查（post-mortem）。三命令：

```text
nlos-debug snapshot inspect <STORE>
nlos-debug replay <STORE> (--task <HEX32> | --binding <HEX32>)
nlos-debug recovery <STORE> [--now <MS>]
```

非目标（显式排除）：live-attach、任何权威写入、快照/重放的**执行**（回放 walkthrough 报告的是「resume 机制会恢复什么」，不真的 re-arm——`resume_binding` 的执行路径不在本面）。

`<STORE>` 为一个目录，按规范名识别四个权威库：`channel-authority.db`、`wait-authority.db`、`process-authority.db`、`task.sqlite3`（接受 slice-k 别名 `tasks.sqlite3`）。缺失的 face 显式降级（逐 face 打 `absent` 行）；`replay` 必须 wait face（投影器必然咨询 wait registry）、`--task`/`recovery` 必须 task face，缺则 typed 失败。

## 2. 写集清单

- `crates/nlos-runtime-tokio/src/bin/nlos-debug.rs`（新 bin：CLI + 只读 helper + 8 项 CLI 级测试）
- `crates/nlos-runtime-tokio/Cargo.toml`（仅 `rusqlite.workspace = true` 晋升 + 理由注释）
- `Cargo.lock`（依赖边机械更新，无版本变化）
- `docs/evidence/stage-b/b-debug-001-minimal-face.md`（本文件）+ `docs/management/evidence-index.yaml`（追加收录）

其余文件零改动；根 `Cargo.toml` 零改动（无新 crate、无新 workspace member）。

## 3. 实现事实（只读纪律三层）

权威 crate 不提供只读 open（全部 `OpenFlags::default()` 即 RW|CREATE），本面以三层机制兑现「零 mutation」承诺：

1. **Preflight 钉版本**：每个库文件先经 `SQLITE_OPEN_READ_ONLY` 裸连接读 `user_version`，与本面钉住的当前版本（wait=1、channel=3、process=5、task=44）比对——不符即 typed 拒绝（exit 3），debugger **永远不可能成为创建或迁移权威库的写者**。钉值由 `schema_pins_match_fresh_authority_stores` 测试守卫：权威升版本时该测试红，强制同步钉值。枚举面（按 task 找 binding、process/fiber/snapshot 列举——权威未暴露 listing API 的表面）只经这些只读连接读。
2. **细读走权威自有 API**：`WaitAuthority::list_waits`、`SqliteTaskAuthority::inspect_task`/`list_due_*`/`summarize_*`/`inspect_*_recovery`/`list_*_alerts`，以及回放 walkthrough 本体 = **真机制** `BindingEventProjection::project`（ADR-0009 决定 1 + ADR-0012 登记式投影）+ `ResumePlan::all_pending`（canonical 计划）。对已存在、当前版本、WAL 的库，权威 open 路径无 durable 写（`journal_mode=WAL` 对已 WAL 库是无操作读；`synchronous`/`foreign_keys`/`busy_timeout` 连接局部；迁移链在当前版本跳过）。
3. **Tripwire**：渲染完成后经新只读连接复读每个 touched 库的 `user_version`，漂移即 exit 5。另有测试级断言 `debugger_leaves_every_authority_store_logically_unchanged`：三命令全跑前后，对四库全表做逻辑 dump（逐表逐行），逐字节相等。

事件流 walkthrough 的呈现语义与 `ResumeReport` 桶一一对应：`PENDING`→`would-rearm`（canonical 计划会 re-arm 的 wait id 列在 `resume-plan path=A`）、`WOKEN`→`already-woken`、`CANCELLED`→`cancelled`、effect/queue→`report-only`；B 路径状态（`resume-plan path=B`）从 process 库只读枚举入口快照（`incarnation`/`digest`/`input_len`）。`--task` 的 binding 解析跨权威取并集：task 库 `effect_fiber_registrations` 的 DISTINCT binding ∪ process 库该 task 各 process 的 fiber incarnation heads，排序去重后逐 binding 走查。

`recovery --now` 缺省 `i64::MAX`（确定性：列全部未 Finalized 的 open plan，与 backoff 无关；`--now <MS>` 可显式收敛 due 口径）。escalated 计划不进 due 列表（等人工 ack），其台账细节经 alert 行内联渲染（alert 自带完整 recovery record）。

## 4. CLI 面

```text
nlos-debug snapshot inspect <STORE>      # wait registry 全行 + process/fiber/incarnation/entry-snapshot durable 面
nlos-debug replay <STORE> --task HEX32   # task 头 + 该 task 全部 binding 的逐事件回放走查 + A/B 恢复计划
nlos-debug replay <STORE> --binding HEX32# 单 binding 回放走查
nlos-debug recovery <STORE> [--now MS]   # artifact/semantic/resource 三域恢复摘要 + open plan + 台账 + alert
```

typed exit codes：`0` 成功 · `1` usage · `2` 输入畸形（hex/`--now`）· `3` store 失败（缺 store/缺 face/钉版本不符/权威读失败）· `4` 选择目标不存在（未知 task）· `5` 只读 tripwire（正常不可达）。

## 5. 验证（TDD：CLI 级集成测试，house bin-test 模式）

```text
cargo test -p nlos-runtime-tokio
  → 27 test suites 全绿（128 passed / 0 failed；其中 nlos-debug bin 8 passed + 1 ignored 手动 smoke）
cargo clippy -p nlos-runtime-tokio --all-targets --all-features -- -D warnings → 0 warning / 0 error
cargo fmt -p nlos-runtime-tokio --check → 通过
```

真实二进制实跑（`cargo run --bin nlos-debug`，fixture store 由 ignored 测试 `leave_fixture_store_for_manual_smoke` 物化）：三命令均 exit 0，输出见 §3/§4 语义；连跑两次输出逐字节相同（确定性 + 库未被触碰）。

测试名 → W33-G 验收门映射：

| 测试 | 覆盖 |
|---|---|
| `snapshot_inspect_renders_expected_durable_state` | 快照检查可跑：wait registry 四行三态 + 三 process（2 active/1 crashed）+ fiber registry + 入口快照（digest/input_len/written_at） |
| `replay_walkthrough_uses_real_projection_and_is_deterministic` | 回放可跑：三权威五事件注册序（1050<1100<1150<1400<1500）、action 桶与 ResumeReport 一致、`resume-plan path=A` 恰为 PENDING wait id、B 路径快照在场；两次运行逐字节相等；外 binding 排除 |
| `replay_task_selector_resolves_bindings_across_authorities` | `--task` 跨权威解析 binding 并集（effect 侧 + process 侧）；未知 task → exit 4 |
| `recovery_renders_three_domains_and_ledger_states` | 三域摘要 + retrying 台账（due 面）+ escalated 台账（alert 面内联）+ acknowledged 状态 |
| `exit_codes_are_typed` | 无参/未知子命令/未知 flag→1；坏 hex/坏 `--now`→2；缺 store/空目录/钉版本不符→3；未知 task→4 |
| `debugger_leaves_every_authority_store_logically_unchanged` | 只读断言：三命令前后四库全表逻辑 dump 逐字节相等 |
| `schema_pins_match_fresh_authority_stores` | 钉值漂移守卫：权威升 schema 版本即红 |
| `hex_selector_parsing_accepts_exact_hex_and_rejects_shapes` | 选择器解析边界 |

## 6. 明确未完成（PARTIAL_PASS 保持）

- **回放 walkthrough 是报告不是执行**：不调用 `resume_binding`/`resume_from_snapshot` 的执行路径（决策点 2 边界——执行属恢复机制本体，B-PROCESS-002 已覆盖）；如需「真 re-arm 一步」演示，另立切片。
- **semantic/resource 台账 fixture 未构造**：渲染面三域齐备，但测试 fixture 只构造了 artifact 域台账（semantic/resource 渲染零摘要）；两域计划构造依赖各自 coordinator 面，留待后续。
- **store 布局假设**：task 库识别 `task.sqlite3`/`tasks.sqlite3` 两个规范名；slice-k 之外的其他 host 自定义布局（如 process 库放子目录）不支持——按权威当前全部 harness 的根目录平铺布局实现。
- **process 面 platform kill receipt / 批量 cancel receipt 未渲染**：最小面取 incarnation/快照/终态标记三表；kill receipt 属 process 恢复细节，递延。
- **`--now` 无 wall-clock 缺省**：刻意（确定性优先）；真实 due 检查由恢复 worker 持有时钟，不由 debugger 假设。
