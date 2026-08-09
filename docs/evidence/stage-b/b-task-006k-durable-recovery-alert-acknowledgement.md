# B-TASK-006K：durable recovery alert acknowledgement

> 状态：`PARTIAL PASS`　　日期：2026-08-09
>
> 对应：`ADR-0004`、`B-TASK-006I`、`B-TASK-006J`、`[CTRL-SAFETY-001]`

## 已实现事实

1. TaskAuthority schema v9 新增 immutable recovery-alert acknowledgement receipt；它绑定 exact `plan_id + total_failures` escalation instance、Principal、IdempotencyKey 与确认时间。
2. acknowledgement 使用 failure-count CAS；stale UI 不能确认后续 escalation。相同 key 的相同请求跨重启返回原 Receipt，相同 key 改绑其他请求 fail-closed。
3. acknowledge 只消除该 escalation 的未确认告警，不会隐式 resume、重试或把 Artifact commit 标成成功；resume 仍是独立的显式 CAS transition。
4. bounded alert list 返回 escalated recovery 的 typed authority source、计数/时间和可选 Receipt；aggregate health 新增 `unacknowledged_escalated`，worker health 同步该 durable gauge。对外候选数据不依赖本地错误字符串。
5. v8→v9 migration 不制造历史 acknowledgement；Receipt 表以 SQLite trigger 禁止 UPDATE/DELETE。

## 验证与边界

新增 integration tests 覆盖首次确认、重启 exact replay、stale CAS、idempotency conflict、确认不触发 resume、bounded alert list、未确认 gauge、DDL immutability 与 v8→v9 migration；原 Artifact commit/recovery 与 coordinator worker 测试保持通过。

本证据为单节点本地 H3 / `PARTIAL PASS`。当前权威事实与脱敏 domain model 已成立，但尚未生成/接通 SystemControl protobuf、ServiceDirectory binding 和真实 IPC；Capability 校验、统一 ControlCommand envelope、外部 metrics exporter、真实 VFS/进程故障和三平台 CI 仍未完成。
