# B-SCHEMA-014：SystemControl recovery contract

> 状态：`PARTIAL PASS`　　日期：2026-08-09
>
> 对应：`B-TASK-006K`、`[NLOS-CONTROL-001]`、`[SABI-COMMON-001]`、`[CTRL-PARITY-001]`、`[CTRL-SAFETY-001]`

## 已实现事实

1. 新增 `nlos.sabi.SystemControl` v1 protobuf schema，并进入 Rust schema registry；同一源生成 checked-in TypeScript/Python bindings。
2. `get` payload 提供有界 Artifact commit recovery metrics、typed failure authority 与 alert status；契约没有本地 diagnostic/error string 字段。
3. `submit` 使用统一 `ControlCommand` 形状：稳定 command ID、issuer Principal、GUI/NL/CLI/API/automation source、Operation scope、target ID、expected failure revision CAS、typed acknowledgement command 与有界 reason。
4. mutation result 绑定 command ID、typed lifecycle 和 Receipt reference；它不提供“手工标记成功”或绕过 TaskAuthority recovery state 的字段。
5. Rust encode/decode 对 schema identity、枚举、固定宽度 ID、非零 CAS、alert/failure 数量、时间顺序、Receipt、reason NUL/长度和 64 KiB payload fail-closed。

## 验证与边界

`nlos-schema` compatibility tests 覆盖 get/snapshot/submit/result round-trip、registry、过量 alert 与不安全 reason；Buf lint/format、TypeScript typecheck 和 Rust tests 通过。

本证据为 schema/validator H3 / `PARTIAL PASS`。尚未接入 TaskAuthority handler、common-envelope caller/idempotency/Capability 校验、ServiceDirectory binding 或真实 local IPC；当前不能声称 GUI/CLI/NL/API 已形成等价控制路径。
