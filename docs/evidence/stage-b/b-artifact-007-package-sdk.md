# B-ARTIFACT-007：Package SDK 开发者工具链（`nlos-package` CLI）

> 状态：`PARTIAL PASS`
>
> 日期：2026-09-21
>
> 对应：[进度单 §6.5.3](../../management/stage-b-progress.md) W33-A 车道行（B1-4，验收门「样板包可构建、可验签」）；ROAD-B-001 生态门（为 B1-5 第三方样板应用提供非内核视角开发路径）
>
> 实现：`crates/nlos-artifact/src/bin/nlos-package.rs`（唯一新增源文件）；开发者文档 [docs/developers/packaging.md](../../developers/packaging.md)
>
> 依赖前序：B-ARTIFACT-003（`verify_package` 签名验证面）、B-PLAN-002/W28-B（`tasks` 模板段与 `verify_package_with_tasks`）、B-APPLICATION-001（receipt→安装 digest-binding，本切片只消费其入参语义）

## 1. 本切片目标

给第三方开发者一条不触内核内部 API 的打包路径：`nlos-package keygen`（开发签名密钥描述子）→ `build <DIR>`（开发者目录 → 自包含已签名包文件）→ `verify <PKG>`（走 `nlos-artifact` 权威验签管线，产出与安装链衔接的 verified receipt）。CLI 不拥有任何自有验证逻辑——verify 半边全部委托 `ArtifactStore::verify_package` / `verify_package_with_tasks` + `IdentityAuthority` 当前 key binding 验签。

## 2. 实现事实

### 2.1 二进制落位与依赖晋升（写集内最小解释）

按任务书约束（根 Cargo.toml 禁改、bin-only），二进制落 `nlos-artifact`（包签名/验签面的 owner crate）`src/bin/nlos-package.rs`。因 src/bin 目标不能链接 dev-dependencies，`crates/nlos-artifact/Cargo.toml` 将 `ed25519-dalek` 从 dev-dependencies 晋升为 dependencies（workspace 版本不变；库本身零改动、零新增 API——该 crate 测试本就以 `SigningKey::from_bytes` 构造签名密钥，晋升只是让 bin 复用同一构造路径）。此解释镜像 B-PLAN-002 §4 的 Cargo.toml 先例；不认可则回退 = 删依赖行 + 删 bin + 删测试。

### 2.2 三命令与文件格式

- **keygen**：`--seed <HEX64>` 必填（工具链刻意不内置随机源、零新增依赖；熵由开发者供给，如 `openssl rand -hex 32`）。从公钥确定性派生 profile/policy/bootstrap 幂等键；principal id 不落文件，每次经 throwaway `IdentityAuthority::bootstrap_principal` 由身份权威派生（单一派生权威，CLI 不复制派生公式）。密钥文件 unix 下 0600；有效期窗 `[0, i64::MAX]`（u64::MAX 不入 SQLite INTEGER，本地实证 typed 报错后收敛）。
- **build**：解析行式 `package.manifest`（`package-id`/`version`（u64 或按 `pack_package_version` 同公式的点分三元组）/`entry`/可选 `task`），读载荷与五个模板 digest 体文件，调共享 `validate_task_templates` 前置校验段形状，对域分隔 manifest 摘要签名，写出规范长度前缀帧包文件（magic `nlos/package-file/v1`、u64 BE、内嵌公钥描述子与载荷）。**构建零墙钟输入**：artifact_id 由 `SHA-256(域‖package_id‖version‖name)` 确定性派生，同目录+同密钥 ⇒ 逐字节相同输出（测试钉死）。
- **verify**：解析包文件（截断/尾随字节/越界名长/未知 role/kind 字节全 typed 拒绝）→ 把每个 entry 物化进真实 `ArtifactStore`（create 幂等键派生自 artifact_id + put revision 0，重复物化天然 replay）→ bootstrap（或 replay）签名者 → 走权威 `verify_package(_with_tasks)`。验签幂等键从 manifest 摘要确定性派生：同包对持久 store 重验输出 `REPLAYED` 且 receipt 逐字节相同（durable receipt 权威语义的直接复用）。`--store`/`--identity` 缺省用临时目录验后即删。

### 2.3 退出码契约（typed）

`0` 成功 · `1` 用法 · `2` 输入畸形（manifest/密钥/包文件解析与形状，含 `PackageManifestInvalid`）· `3` 验签/身份失败（`PackageSignatureInvalid`/`PackagePrincipalUnknown`/`PackageKeyRevoked`/`PackageIdentity`/`IdempotencyConflict`）· `4` 内容绑定失败（`PackageTampered`/`ArtifactNotFound`）· `5` 内部 I/O/存储失败。映射为 `from_artifact_error` 的穷举 match。

### 2.4 TDD 红→绿记录

测试先行：`tests/package_sdk_cli.rs`（真实二进制，`CARGO_BIN_EXE_nlos-package`，镜像 `control_command_cli.rs` 模式）首跑红（binary missing）→ 实现 bin → 绿。期间修出三类真实缺陷：`u64::MAX` 有效期不入 SQLite（identity typed 报错暴露）、manifest 里 `entry`/`task` 是可重复记录不可按标量去重、测试夹具 `{version}` 占位符未插值。clippy `-D warnings`（pedantic）逼出 needless_borrow、similar_names、single_match_else、too_many_lines（verify 拆 `ensure_root`/`verify_signed`/`print_decision`）、hex 字面量分组，均按 lint 修正而非 allow。

## 3. 验证证据

新增测试 14 个：

- **CLI 集成（`tests/package_sdk_cli.rs`，7）**：`build_then_verify_round_trip_and_persistent_replay`（keygen→build→verify VERIFIED→同 store 重验 REPLAYED 同 receipt；build/verify 摘要与 signer 一致）、`build_is_byte_reproducible_for_identical_trees`（两次构建逐字节相等）、`tampered_payload_fails_verification_with_binding_exit_code`（载荷单字节翻转 ⇒ exit 4）、`tampered_manifest_fails_signature_verification`（version 字段单字节翻转 ⇒ exit 3）、`manifest_with_tasks_builds_and_verifies_end_to_end`（W28-B 模板面全链：两模板含依赖链，build `entries 2 tasks 2` → verify VERIFIED）、`malformed_inputs_fail_typed_at_build_and_verify`（缺 manifest/重复 entry 名/悬空依赖/截断包 ⇒ exit 2）、`usage_errors_exit_one`。
- **bin 单测（7）**：版本两形态、manifest 解析正/负（含未知键、缺标量、重复名、坏 role）、密钥文件 round-trip 与坏形状（含反置有效期窗）、包文件 codec round-trip（双面）+截断/尾随字节拒绝、artifact_id 确定性与名字边界参与、行纪律。

既有面零回归：`package_signature` 7、`package_task_templates` 4 原样绿。

本地验证命令与结果（2026-09-21，final fmt 后）：

```text
cargo test -p nlos-artifact                                    # PASS：86 passed / 0 failed（含新增 14）
cargo clippy -p nlos-artifact --all-targets -- -D warnings     # PASS：exit 0
cargo fmt -p nlos-artifact -- --check                          # PASS
python3 scripts/lint_claims.py                                 # PASS：135/135（本文件已入 evidence-index）
```

真机手工演练（同日）：keygen→build（模板面）→verify `VERIFIED a7ea5992…`→重验 `REPLAYED` 同 receipt；载荷篡改 exit 4、临时 store 验签同 receipt；输出已在 docs/developers/packaging.md §5–6 引用。

## 4. 已知限制与 deferred minors（如实登记）

- **依赖晋升待追认**（§2.1）：如编排方不认可 Cargo.toml 解释，回退清单见上。
- **生产信任边界**：verify 用包内嵌公钥描述子 bootstrap 签名者，证明「签名与该密钥一致」而非信任决策；trust root/签名链/密钥托管为部署侧（文档 §4 已如实声明）。`KeyPurpose::SemanticSigning` 沿用 B-ARTIFACT-003 已登记限制。
- **verify 物化是开发者路径**：生产摄取走系统侧 store/identity 部署；同 artifact_id 异内容对持久 store 重物化会 `HeadConflict`（exit 5），属预期 typed 冲突。
- 版本点分三元组打包公式镜像 `nlos-application`（依赖方向所限）；彼侧演进需同步本 CLI 与文档（两侧各 ~10 行）。
- `nlos package install` 不在本切片（W33-B 样板全周期演示的消费端接线）。
- evidence-index 收录本文件 + 本行（AGENTS 规则 4 同步 L0 索引；与 W28-B 当年留索引车道的先例不同，本次 lint 双向一致门要求同 commit 收录，故直接收录）。

## 5. 未运行项（显式列出）

未运行 `cargo test --workspace`（任务边界禁）、三平台 CI/MSRV/Pages（波次屏障统一跑；CI clippy 1.98 的 `chunks_exact` 新 lint 已知且本切片未用该模式）、真实断电矩阵（无新 durable 协议——复用既有 store/identity 提交路径）。
