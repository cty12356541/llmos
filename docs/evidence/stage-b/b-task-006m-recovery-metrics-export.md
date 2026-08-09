# B-TASK-006M：recovery metrics export

> 状态：`PARTIAL PASS`　　日期：2026-08-09
>
> 对应：`B-TASK-006J`、`B-TASK-006K`、`B-TASK-006L`

## 已实现事实

1. `nlos-system-control` 新增 backend-neutral `RecoveryMetricsSink`；host adapter 可映射到 OpenMetrics、ETW、signposts 等后端，而无需改变 authority 或 SABI schema。
2. catalog 固定区分 monotonic counter（cycles/inspected/finalized）与 point-in-time gauge（consecutive failures/retry delay/durable retrying/escalated/unacknowledged/resolved），worker lifecycle 保持 typed enum。
3. 每次 export 都以 TaskAuthority live summary 覆盖 worker cache 中的 durable gauge，避免 scrape 暴露过期 escalation/acknowledgement 数量。
4. metrics 不包含 plan ID、Principal、reason 或本地 diagnostic string；per-plan drill-down 继续通过授权后的 SystemControl `get`。
5. sink failure 在首个失败样本停止并返回 typed backend error；TaskAuthority read failure 与 sink failure 不混淆。

## 验证与边界

integration test 用故意过期的 worker gauge 验证 exporter 返回 live TaskAuthority 值，并核对稳定 metric name/kind；`nlos-system-control` 共 6 项 integration tests 通过。

本证据为单节点本地 H3 / `PARTIAL PASS`。这里只定义 backend-neutral export contract 和 snapshot push，不含具体 OpenMetrics HTTP endpoint、ETW/signpost adapter、scrape auth、retention/alert rules 或三平台验证。
