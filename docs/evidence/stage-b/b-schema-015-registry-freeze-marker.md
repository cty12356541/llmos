# B-SCHEMA-015：REGISTRY 冻结标记（ADR-0014 决定 1 后续实现切片）

> 状态：`PASS`　　日期：2026-08-30
>
> 对应：`B-TYPES`、`[ADR-0014](../../management/adrs/0014-schema-channel-freeze-v1-beta.md)`、`[COMPAT-VER-001]`、`[TYPE-GEN-001]`

## 已实现事实

1. `nlos-schema` 的 canonical `SchemaDescriptor` 增加 `frozen: bool` 字段（ADR-0014 v1-beta 冻结语义的 REGISTRY 机械写入）：前 6 个条目（Envelope v1.1、ServiceDirectory、OperationControl、SystemControl、TakeoverControl、WaitControl）`frozen: true`；第 7 个条目 `nlos.sabi.PrincipalHandshake`（ADR-0014 决定 2 additive 落表）`frozen: false`，保持开放。
2. 新增查询 API `registry_frozen(name: &str) -> Option<bool>`：已知条目返回其冻结标志，未知条目返回 `None`。
3. 新增测试 `registry_freeze_markers_match_adr_0014`（`crates/nlos-schema/tests/compatibility.rs`）：断言 6 项 `Some(true)`、PrincipalHandshake `Some(false)`、未知条目 `None`，并逐条目校验 descriptor 元数据与查询 API 一致。
4. 纯内部结构变化：`SchemaDescriptor` 仅在 `nlos-schema` crate 内构造/消费（全仓 grep 确认），proto/生成物/golden 零变化，wire 零扰动。

## 验证命令与结果

- `cargo test -p nlos-schema`：21 passed / 0 failed（含新增冻结测试）。
- `cargo clippy -p nlos-schema --all-targets --all-features -- -D warnings`（stable 与 `+nightly-2026-08-01` 双工具链）：均通过，0 warning。
- `cargo fmt -p nlos-schema -- --check`（stable 与 `+nightly-2026-08-01` 双工具链）：均通过。
- `npm run schema:check-generated`：exit 0；重新 `buf generate` 后 `git status schema/` 零变更，证明 wire/生成物零 diff（硬门）。

## 已知限制

- `frozen` 标志是 REGISTRY 元数据防线，不是编译期强制：它使「冻结条目」状态可机械查询与测试钉定，但 wire 字节级锁定仍由既有 conformance golden 逐字节防线承担（双层防线：golden 管字节、本标记管注册表语义）。
- 未实现编译期 macro 强制（禁止对 frozen 条目生成 breaking diff 的 proc-macro 层）；按 ADR-0014 决定 1 属本切片范围外，登记为后续工作。
- `frozen: false` 的 PrincipalHandshake 条目在 v1.0 晋升 ADR（ADR-0014 决定 4）落地时应一并翻转为 frozen。
