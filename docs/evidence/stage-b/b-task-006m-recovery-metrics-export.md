# B-TASK-006M：recovery metrics export

> 状态：`PARTIAL PASS`　　日期：2026-08-23（第二十六增量）
>
> 对应：`B-TASK-006J`、`B-TASK-006K`、`B-TASK-006L`

## 已实现事实

1. `nlos-system-control` 新增 backend-neutral `RecoveryMetricsSink`；host adapter 可映射到 OpenMetrics、ETW、signposts 等后端，而无需改变 authority 或 SABI schema。
2. catalog 固定区分 monotonic counter（cycles/inspected/finalized）与 point-in-time gauge（consecutive failures/retry delay/durable retrying/escalated/unacknowledged/resolved），worker lifecycle 保持 typed enum。
3. 每次 export 都以 TaskAuthority live summary 覆盖 worker cache 中的 durable gauge，避免 scrape 暴露过期 escalation/acknowledgement 数量。
4. metrics 不包含 plan ID、Principal、reason 或本地 diagnostic string；per-plan drill-down 继续通过授权后的 SystemControl `get`。
5. sink failure 在首个失败样本停止并返回 typed backend error；TaskAuthority read failure 与 sink failure 不混淆。

## 验证与边界

既有 integration test 用故意过期的 worker gauge 验证 exporter 返回 live TaskAuthority 值，并核对稳定 metric name/kind；本轮新增
`metrics_export_contract.rs`，以独立的可移植 sink 验证完整的 typed catalog 顺序（1 个 lifecycle、3 个 counter、6 个 gauge）、
单次 health snapshot 边界以及首个 sink failure 的有界短路（失败后不继续写入后续指标）。`nlos-system-control` 现有与新增 metrics 相关测试均通过。

本轮本地验证：

- `cargo test -p nlos-system-control --test metrics_export_contract --quiet`：3 项通过。
- `cargo test -p nlos-system-control --quiet`：该 crate 的 2 + 7 项 integration tests 通过。
- `cargo fmt --all -- --check` 与 `cargo clippy -p nlos-system-control --all-targets --all-features -- -D warnings`：通过。

新增测试使用真实 `SqliteTaskAuthority` 临时数据库，但只依赖 backend-neutral sink 接口，未把任何平台 exporter 或本地诊断文本引入测试契约；本批 Rust cross-platform/MSRV CI [32624822987](https://github.com/cty12356541/llmos/actions/runs/32624822987) 的 Ubuntu/Windows/macOS/MSRV jobs 与 Pages [32624822965](https://github.com/cty12356541/llmos/actions/runs/32624822965) 已成功。

本证据为单节点本地 H3 / `PARTIAL PASS`。这里只定义 backend-neutral export contract 和 snapshot push；本轮三平台 CI 只验证可移植契约回归，不等同具体 OpenMetrics HTTP endpoint、ETW/signpost adapter、scrape auth、retention/alert rules 或生产 exporter。

## 2026-08-23 增量

新增 `metrics_export_contract.rs`：用真实临时 `SqliteTaskAuthority` 验证完整 typed catalog 的固定顺序与 metric name 稳定性；`FlappingHealth` 生成计数证明一次 `export_metrics` 只读取一次 health snapshot，live TaskAuthority durable gauges 覆盖 worker cache；lifecycle/counter/gauge sink failure matrix 验证首个失败即短路并返回 typed backend error。metrics 专属测试现为 3 项（本轮新增 2 项）。该增量仍只证明 backend-neutral contract；三平台 exporter、scrape auth 和生产 sink 仍未完成。
