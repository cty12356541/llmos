# B-ARTIFACT-003：Package envelope 签名验证最小前缀

> 状态：`PARTIAL PASS`
>
> 日期：2026-08-29
>
> 对应：v0.5 总纲 §23.2（Package model 最小子集）、ADR-0010（签名失败语义镜像）
>
> 实现：`crates/nlos-artifact` schema v4（`src/package.rs`、`schema.rs::migrate_v4`、`store.rs::verify_package` 接线）

## 1. 本切片目标

在 B-ARTIFACT-001/002 的内容寻址 store、五步写入协议、staged publication 与 immutable receipt 地基上，增加最小签名 Package 验证前缀：`PackageManifest` 最小类型（package_id / version / entries 子集）、签名 principal 的 Ed25519 签名验签（经 `nlos-identity` 当前 key binding，revoked fail-closed，镜像 ADR-0010 失败语义）、manifest 条目 ↔ artifact heads 的逐条内容寻址绑定（不符 typed fail-closed），以及 durable verified receipt（immutable、重启 replay、幂等键）。

本切片由前一个代理实施大部分后被超时中断；本接管代理完成勘察、对照 `nlos-identity` 实际 API 复核后按原设计续完，未重写。

## 2. 已实现事实

1. **最小 manifest 类型**：`PackageManifest { package_id, version, entries }`，每条目 `PackageManifestEntry { name, artifact_id, digest, role }`；role（`Executable` / `BackgroundService` / `Data`）是签名内的声明性元数据，本切片不强制行为。§23.2 的 applications/components/imports/exports/resources/data/lifecycle/security 均为后续切片。
2. **域分隔签名消息**：`package_manifest_message` 以 `llmos/artifact/package-manifest/v1` 域分隔符起头，镜像 `nlos-capability` 的固定域分隔风格，并扩展为 canonical framing：u64 BE 定宽字段 + 条目计数 + 条目名长度前缀，保证不同 manifest 不产生同一字节流（单测覆盖 name 边界位移、条目顺序、role、version 均参与摘要）。
3. **经 nlos-identity 当前 binding 验签**：`verify_package` 只接收 `SignedPackage { manifest, signer, signature }`，调 `IdentityAuthority::verify_capability_command_signature` 由 authority 自行解析 signer 当前 key binding（Ed25519 `verify_strict`），调用方永远不能 pin 一个 key。
4. **fail-closed 固定顺序，镜像 ADR-0010**：manifest 形状校验（非空、bounded NUL-free 名、唯一名）→ 幂等 replay（durable receipt 为权威，永不重验，故 key 事后吊销不影响既有 receipt 重放）→ 当前 binding 验签（unknown principal / revoked key / invalid signature 分别 typed 为 `PackagePrincipalUnknown` / `PackageKeyRevoked` / `PackageSignatureInvalid`，其余 identity 失败收敛为 `PackageIdentity`）→ 内容绑定。所有失败发生在任何 durable 写之前。
5. **内容寻址绑定**：每条 entry 声明 digest 必须等于该 artifact 当前 head digest；headless artifact（`head_revision == 0`）视为绑定失败；未知 artifact 为 typed `ArtifactNotFound`；不符为 typed `PackageTampered { entry, expected, actual }`。绑定读取与 receipt 插入共用一个 `BEGIN IMMEDIATE` 事务，并发 head 推进无法滑入 check 与 commit 之间。
6. **durable verified receipt（schema v4）**：`package_verification_receipts` 单表 v3 → v4 纯增量迁移，记录 manifest digest、package_id/version、entry_count、signer principal/key_id/key_generation、64 字节签名、验证时间；DDL trigger 拒绝 UPDATE/DELETE（immutable）。receipt ID 由 `IdempotencyKey + manifest_digest` 确定性派生；同 key 同签名包跨重启重放返回逐字节相同 receipt（`Replayed`），同 key 不同请求形状为 typed `IdempotencyConflict`；事务内二次查 key 覆盖 identity 校验窗口内的并发提交。
7. **故障注入**：`fault_injection.rs` 新增 receipt 提交期硬 I/O 错误用例（`FailWritesAfter` shim）：无半截 receipt，disarm 后同一请求原样成功（幂等重做）；identity authority 使用默认 VFS 不受 shim 影响。

## 3. 验收测试

新增 `crates/nlos-artifact/tests/package_signature.rs`（7 用例）与 `src/package.rs` 内嵌 framing 单测，覆盖：正常签名验证与逐条 head 绑定、receipt 可按 id 回读、幂等 replay 与重启 replay、key 吊销后 replay 仍存活且新验签 fail-closed、签名后篡改 manifest/entry digest 为 `PackageSignatureInvalid` 且零 durable 状态、head 推进后的 stale 绑定为 `PackageTampered`（含 headless 与 ghost artifact）、未知 signer 为 `PackagePrincipalUnknown`、manifest 形状校验（空 entries / 重复名 / 空名）。

本地验证命令与结果（2026-08-29，接管代理运行）：

```text
cargo test -p nlos-artifact                                # PASS：43 passed / 0 failed
  （lib 单测 4 + fault_injection 10 + happy_path 6 + immutable_head 4
    + package_signature 7 + recovery 5 + staged_publication 7）
cargo clippy -p nlos-artifact --all-targets -- -D warnings  # PASS：0 warning
cargo fmt -p nlos-artifact                                  # PASS：无 diff
```

最终结果以本 canonical commit 的验证记录为准。

## 4. 证据等级与限制

证据等级：单节点局部 H3，`PARTIAL PASS`。

明确不声明：

- **无安装/更新生命周期**：本切片只验证 envelope，不安装、不激活、不更新、不卸载；无 Application/Installation 模型。
- **manifest 是最小子集**：§23.2 的 applications、components、imports、exports、resources、data、lifecycle、security 字段均未建模。
- **单 principal 签名、无信任链**：只验证唯一 signer 的当前 binding；无 trust root、无证书/签名链、无多签或 threshold 策略。
- **无跨进程验证**：签名 envelope 的跨进程传输、序列化格式与对端验证不在本切片。
- **KeyPurpose 限制（如实记录）**：包 manifest 验签复用 `nlos-identity` 现有 `verify_capability_command_signature` API，故 key 侧沿用 `KeyPurpose::SemanticSigning` 限制；专用的 package-signing key purpose 是 identity 侧后续切片。
- 未改变 B-ARTIFACT-001/002 的 `LOCAL_SINGLE_NODE`、无 GC/retention/encryption/provenance/legal hold/sync backend、域内原子发布（非跨 authority 事务）等限制。
- 未运行 `cargo test --workspace`、`cargo clippy --workspace`（任务边界禁 `--workspace`）、真实断电、三平台 CI 或生产级并发性能验证。

## 5. 下一步

后续切片按序补齐：identity 侧增加 package-signing key purpose 与独立验证入口；manifest 扩展至完整 §23.2 字段并引入 schema 版本化；安装/更新生命周期消费 verified receipt（安装记录引用 receipt id）；跨 authority 的 Task/Package 联合提交与 PackageEnvelope 的跨进程序列化格式。
