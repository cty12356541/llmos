# B-TASK-007C2：TaskAuthority verified Driver/Resource registration

## 1. 验收对象

本切片消费 [B-TASK-007C1](./b-task-007c1-resource-endpoint-proofs.md) 的 ResourceAuthority proofs，使 TaskAuthority 以 direct owner readback + OPEN registry CAS 注册 Driver gateway 与 Resource/Ledger endpoint，并正确处理 Driver generation rotation。

## 2. 实现事实

- `nlos-task` 直接依赖具体 `ResourceAuthority`；Driver 注册 API 只接收 `DriverId + expected Driver generation`，Resource/Ledger API 只接收 `ResourceAccountId + expected endpoint generation`，不接受 participant identity/Receipt tuple。
- TaskAuthority 在进入 Task transaction 前向 ResourceAuthority 回读当前 proof，并逐位比较 caller 计划的 generation；unknown/stale/corrupt proof 以 typed source/generation error 返回且不修改 registry。
- proof 验证后复用 B-TASK-007B2 的 OPEN registry expected generation/root CAS，新 endpoint 进入 successor generation/root。
- 同一 participant identity 的严格更高 generation 可替换旧 tuple，并绑定 owner 产生的新 Receipt；participant 数不增长。相同 tuple replay，旧/相同 generation 的不同 tuple、Receipt collision 或跨 participant type identity reuse 均 fail closed。
- Driver rotation 后，用旧 planned generation 注册会在 Task mutation 前拒绝；使用当前 generation 会在 successor registry 中替换原 Driver participant。结果跨 Task/Resource authority restart 保持稳定。

## 3. 验证

```text
cargo test -p nlos-task
cargo clippy -p nlos-task --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

以上命令均以退出码 0 通过。participant registry integration tests 从 5 项增至 6 项并全部通过。新增生命周期测试覆盖 direct Resource readback、planned-generation mismatch 零变更、Driver/Resource 注册、Driver rotation replacement、stable identity/new Receipt、duplicate replay 与 restart。

## 4. 证据等级与限制

结论：`PARTIAL PASS / H3 local verified Resource registration`。

- API 仍由上层显式编排；Reservation/quote 创建尚未形成不可激活 prepare 并与 registry CAS 做 prepare→activate 接线。
- PermitRequest 的 planned effect descriptor 尚不携带并验证具体 Driver/Reservation participant binding，因此 permit issuance 还不能自动证明每个 effect endpoint 均已预注册。
- EffectPermit、TaskCommitReceipt/finalize 尚未逐位复制或在线重验 participant registry binding。
- Channel/Topic endpoint、TaskAuthorityAssignment term/lease、takeover fence/barrier 与跨进程 signature/attestation 未实现。

下一验收门：`B-TASK-007D1` 把 participant registry generation/root 逐位绑定到 EffectPermit 与 Task commit/closure Receipt，并在 effect issuance、dispatch/finalize 前重验；完整 planned endpoint seal validation 随完整 TaskWriteSet 一并收口。
