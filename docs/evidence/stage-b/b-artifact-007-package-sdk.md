# B-ARTIFACT-007：Package SDK 开发者工具链（`nlos-package` CLI）

> 状态：`PARTIAL PASS`
>
> 日期：2026-09-21
>
> 对应：[进度单 §6.5.3](../../management/stage-b-progress.md) W33-A 车道行（B1-4，验收门「样板包可构建、可验签」）与 W33-C 车道行（X-3 中，验收门「包结构/manifest/验签一致性检查器」，见 §6）；ROAD-B-001 生态门（为 B1-5 第三方样板应用提供非内核视角开发路径）
>
> 实现：`crates/nlos-artifact/src/bin/nlos-package.rs`（W33-A 唯一新增源文件；W33-C 增量见 §6.1）、`crates/nlos-artifact/src/{package_file.rs, conformance.rs}`（W33-C）；开发者文档 [docs/developers/packaging.md](../../developers/packaging.md)、[docs/developers/package-conformance.md](../../developers/package-conformance.md)
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

## 6. W33-C：包一致性 conformance kit（X-3 中，2026-09-21）

### 6.1 目标与实现事实

验收门「包结构/manifest/验签一致性检查器」：对**任意** `nlos/package-file/v1` 包文件（不限于本 CLI 构建）离线运行带稳定规则号的规则集，与 `verify`（单包权威准入）互补。

- **前置 refactor（独立提交）**：包文件 codec 自 bin 提升为库模块 `package_file.rs`——magic/签名者描述子/entry/task 帧、`manifest()`/`manifest_entry()` 投影与 `derive_artifact_id` 迁移，decode 失败改 typed `PackageFileError`（Display 字符串与既有 CLI 报文一致）。单一 codec 权威（本仓纪律），bin 行为/退出码零变化。
- **conformance 模块（`src/conformance.rs`）**：17 条规则 `PKG-CONF-001..042` 分四组——结构（001 magic/002 帧/003 entry 名/004 枚举字节）、manifest schema 含 tasks 形状（010 非空/011 名唯一/012 node key 唯一/013 无自依赖/014 依赖局部性/015/016 容量界）、签名链与摘要一致（020 内嵌公钥验签于正确面域分隔摘要、030 载荷摘要、031 artifact_id 按文档派生）、兼容窗与元数据（040 窗非空、041 窗 ≤ i64::MAX——身份台账 SQLite INTEGER 界、042 version ≠ 0）。`check_package_file` 结构失败 fail-stop（更深层不可信），解码成功后全部规则一次收集。012–016 与 `validate_task_templates` 同域两实现（权威判定 vs 规则号粒度），agreement 测试钉死。库侧 ed25519 新用途为**验签**（非签名），Cargo.toml 注释同步更新。
- **CLI 面**：`nlos-package conformance <PKGFILE>` 子命令（exit 0 CONFORMANT / 6 NONCONFORMANT / 2 不可读 / 1 用法），finding 逐行 `PKG-CONF-### <detail>` 输出至 stdout。
- **文档**：[package-conformance.md](../../developers/package-conformance.md) 规则表（每条含权威来源标注：格式/验签路径/构建惯例/kit 级，kit 严于内核准入处如实标注）；packaging.md §1/退出码表/§8 交叉引用；bin 头契约同步。

### 6.2 验证证据

新增测试 30（净增 26，codec 测试 2 个自 bin 随迁并加 typed 断言）：

- **集成（`tests/package_conformance.rs`，20，真实二进制）**：W33-A CLI 构建的包两面（legacy/templated）conformance 净通过；库面手造净包（非本 CLI 产物）净通过；每规则 fixture 精确命中——001 坏 magic、002 截断+尾随、003 非 UTF-8/NUL/超长名、004 未知 role/kind 字节（字节手术）、010 空清单、011 重名、012/013/014 模板形状、016 依赖容量界（258 模板全引用合法）、020 签名字节翻转、020+031×2 version 篡改多 finding 报告形、030 载荷篡改（签名不覆盖载荷字节，单命中）、031 偏离派生（正确签名）、040 反置窗、041 u64::MAX 窗（描述子未签名，单命中）、042 零版本；usage exit 1。
- **库单测（conformance 6 + package_file 4）**：规则号稳定且互异（`PKG-CONF-0NN` 格式）、空输入→001、最小手造包净通过、decode 失败→对应结构规则（含 040 映射）、task 规则与 `validate_task_templates` 一致性、015 界（100 001 模板，仅库级，见 §6.3）、codec 双面 round-trip、typed 失败分类、artifact-id 确定性与名边界、manifest 投影全字段。

既有面零回归（W33-A `package_sdk_cli` 7、`package_signature`/`package_task_templates` 原样绿）。本地验证（2026-09-21）：

```text
cargo test -p nlos-artifact                                        # PASS：114 passed / 0 failed（净增 26）
cargo clippy -p nlos-artifact --all-targets --all-features -- -D warnings   # PASS：exit 0
cargo fmt -p nlos-artifact -- --check                              # PASS
python3 scripts/lint_claims.py                                     # PASS（§6.2 落档时点）
```

真机手工演练（同日）：CLI 构建包 `conformance` → `CONFORMANT`/exit 0；首字节破坏 → `PKG-CONF-001` + `NONCONFORMANT … findings 1`/exit 6。

### 6.3 已知限制与 deferred minors

- 结构失败（001/002 及解码层拒绝）一次只报一个 finding 后停止；修复后重跑（文档 §1 已述）。
- `PKG-CONF-042`（version ≠ 0）为 kit 级要求，内核侧无对应拒绝路径（文档与规则表已如实标注权威来源）。
- `PKG-CONF-031`/`042` 使 kit 严于 `verify`（偏离派生的 artifact_id 仍可通过 verify 物化）——面向生产者契约，属设计意图非缺陷。
- 015 容量界仅库级测试覆盖（100 001 模板包 ~18MB，CLI fixture 性价比低，如实登记）。
- 不含开发者目录布局检查（`build` 输入面，packaging.md §2）。
- evidence-index scope 行同步收录 W33-C（AGENTS 规则 4）。

### 6.4 未运行项（W33-C）

未运行 `cargo test --workspace`（任务边界禁）、三平台 CI（波次屏障统一跑）。
