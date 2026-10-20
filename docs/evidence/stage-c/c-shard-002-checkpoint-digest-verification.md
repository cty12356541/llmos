# C-SHARD-002 / W54-CS0B：checkpoint digest 钉面与 fail-closed 验证器

- 状态：`PASS`（ADR-0019 R1.2.5 观察面「登记不验证」的验证半边终结：结构化 digest 语义 + 验证 API；executor 消费半边属 C-SHARD-003）
- 日期：2026-10-08
- Owner：`C-SHARD` 计划 Phase 0（[c-shard-plan.md](../../management/c-shard-plan.md) CS0-B 车道）
- 设计依据：[ADR-0019](../../management/adrs/0019-cross-cell-commit-semantics-extension.md) R1.2.3（trail 可见性，未覆盖窗口 PARTIAL/UNCERTAIN）+ R1.2.5（checkpoint 是观察面）；digest 只钉（轴,前缀）二元组
- 分支：`feat/cs0b-checkpoint-digest`；lane commit `be92e5f22`；base `396ab30e9`；合入 main `4178c0ae5`（W54 波）

## 1. 实现事实

- `crates/nlos-cell/src/federation.rs`（additive，+259 行）：
  - `CheckpointDigest`：域分隔 SHA-256，输入 = `"llmos/reconciliation-checkpoint/v1" || be64(boot) || be64(epoch) || be64(token) || canonical(prefix)`（域串纪律沿 `derive_assignment_id` 先例）；规范文本形 `v1:sha256:<64 小写 hex>`（74B，适配 ≤256B 单行摘要轴，**存储格式不变**）；仅小写一种拼写可解码。
  - `CheckpointPrefix`：声称 durable 前缀的规范化表示，字节文法 `be64(count) || (be64(len) || bytes)*`——条目边界与条目数长度钉死（`[ab,c]`≠`[abc]`，空前缀≠单空条目）；条目语义留给 executor。
  - `CheckpointFact::verify_digest(&prefix)`：按 fact 自身三轴重算比对，类型化四态 `Verified / Mismatch{presented,recomputed} / FreeText / Corrupt`——不匹配与损坏显式 fail-closed；自由文本按 R1.2.3 诚实归 `FreeText`（未覆盖）。
- 模块 :38-40 deferred 声明精确化：验证 API 已提供（CS0-B），executor 消费属 CS0-C。既有 API 签名、存储格式、错误枚举零改动。
- `sha2.workspace = true` 入 nlos-cell（Cargo.lock 随动 1 行）。

## 2. 验证（2026-10-08，macOS arm64 darwin 25.6.0）

```text
cargo test -p nlos-cell                      → 32 passed / 0 failed / 2 ignored（既有子进程辅助）
cargo fmt --check                            → 退出码 0
cargo clippy -p nlos-cell --all-targets      → 退出码 0，零警告（pedantic 开启）
cargo check -p nlos-lease -p nlos-slice-k    → 退出码 0（下游缝隙保险）
integrator 全仓（merge 后）：test --workspace 1907/0/30
```

新测试 `crates/nlos-cell/tests/federation_digest.rs`（5 个具名测试）：digest 确定性（同前缀同摘要、任一轴/字节变化摘要变）、验证器四态（Verified/Mismatch/FreeText/Corrupt）、round-trip（落盘 8 行 key=value 形状钉面）。既有 9 个 federation 测试零回归。

## 3. 边界与限制（诚实域）

- 不声称 executor 已实现（C-SHARD-003 义务）、不绑定 DIST-TASK record（CS1-D 义务）、不声称跨机。
- verify 只钉（轴,前缀）二元组；轴与外部已知 fence 的比对属 executor。
- 语义域：单机（ADR-0018），引证为跨机证据即违 RISK-B-11。

## 4. 复现

```text
cargo test -p nlos-cell --test federation_digest -- --nocapture
```
