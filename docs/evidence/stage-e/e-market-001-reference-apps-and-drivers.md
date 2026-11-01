# E-MARKET-001：解耦第三方参考 App/Driver 市场样板与冻结签名 N-1 基准包证据（`W80-E6B`）

- **Evidence ID**: `e-market-001-reference-apps-and-drivers`
- **Wave / Lane**: `W80 / W80-E6B-ECOSYSTEM-SAMPLES`（工作包 `E6` 样板包面）
- **日期**: `2026-11-01`
- **Assurance Level**: `H4`（纯确定性本地宿主单元/集成测试 + 真实工具链 CLI 实跑 + 字节确定性复现验证；不声称 `H7` 生产环境或跨主系统升级证明——后者属 `W81` `ROAD-E-001` 总门禁）
- **状态**: `VERIFIED`（经 `W80-INT` 四车道工作区合流复验通过，晋升 canonical `DONE`）
- **权威规范与治理依据**:
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §15.1（`DRV-NEGO-001`、`DRV-PROTO-001`、`DRV-LIFE-001`）、§23.1–§23.2（`PKG-MULTI-001`、`PKG-SEP-001`、`PKG-IMM-001`、`PKG-SEED-001`）、§28.5（`ROAD-E-001` 消费品）
  - [ADR-0027 Stage E SABI 长期向后兼容与商店治理](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) §2.4 支柱四（解耦构建的第三方参考生态样板矩阵 + 步骤 A「N-1 产物冻结」）
  - [ADR-0024 Driver Provider 线级载荷 Codec 冻结](../../management/adrs/0024-driver-provider-payload-codec-freeze.md)（`nlos.sabi.DriverProvider` v1.0 冻结通道）
  - [stage-e-progress.md](../../management/stage-e-progress.md) §E.2.2 工作包 `E6` 第 2 条、§E.3.2 `W80-E6B` 行
  - 先例:[e-trial-002-quickstart-and-starter-app.md](e-trial-002-quickstart-and-starter-app.md)（`W77-PRE-C` starter-assistant-app 独立 workspace 先例）

---

## 1. 解耦样板矩阵：2 个多组件 Application + 2 个用户态 Driver（零内核私有依赖）

全部样板位于 `examples/` 下两个**独立 workspace 根**（空 `[workspace]` 成员表先例，不改根 `Cargo.toml` / 根 `Cargo.lock`，各自持有独立 `Cargo.lock`），直接依赖仅为仓库内公共权威 crate 与 `ecosystem-driver-kit`（自研共享 kit）；既有外部 crate 仅沿用 starter 先例同版本（`ed25519-dalek 3.0.0`、`sha2 0.11.x`），**零新增第三方依赖**。

### 1.1 多组件 Application × 2（`PKG-MULTI-001` / `PKG-SEP-001` / `PKG-IMM-001` / `PKG-SEED-001`）

| 样板 | 发布者 | 包 ID | entry 角色 | 任务 DAG |
|---|---|---|---|---|
| [examples/ecosystem-apps/knowledge-observatory-app](../../../examples/ecosystem-apps/knowledge-observatory-app) | `nlos.market.observatory-works` | `6b6e6f776c65646f6273763130303031` | executable + background-service + driver 桥 + UI surface 卡 + 2 只读 seed（6 entry） | `harvest-sources(agent-role) → build-index(executable) → publish-digest(agent-role)`；v1.1.0 追加 `verify-digest` |
| [examples/ecosystem-apps/field-survey-app](../../../examples/ecosystem-apps/field-survey-app) | `nlos.market.fieldkit-labs` | `6669656c647375727665797731303030` | 同上（6 entry） | `plan-route(agent-role) → collect-notes(executable) → sync-notes(executable)`；v1.1.0 追加 `review-notes` |

每个 App 以公共权威 crate（`nlos-slice-k` / `nlos-application` / `nlos-artifact` / `nlos-identity` / `nlos-plan` / `nlos-resource` / `nlos-task` / `nlos-types`）驱动完整第三方回路：签名包构建 + 一致性校验 → 验签安装 → payload 执行 → 只读 seed 读回 → 五维 `LlmTokenBudget` 逐步计量 → 依赖围栏负向门 → 带因果血缘的 `ArtifactStore` 修订 → `SameMajor` 热升级 + 计划 revision 2 追加节点（上游节点执行冻结保持）→ 干净卸载 + fail-closed payload 拒绝。实测输出（节选）：

```text
[observatory] PACKAGES_CONFORMANT v1_entries=6 v1_tasks=3 v1_bytes=2538 v2_tasks=4 v2_bytes=2828
[observatory] PLAN_GATE_BLOCKED node=aa52c031837f0da1 unresolved_deps=1
[observatory] UPDATE_V1_1 generation=2 / PLAN_REV2 revision=2 chain_len=2 nodes=4
[observatory] SUMMARY ... llm_receipts=4 consumed_tokens=3540 uninstall_generation=2
[field-survey] SUMMARY ... llm_steps=4 consumed_tokens=2770 uninstall_generation=2
```

负向门同样实测：两 App 均覆盖依赖未就绪拒绝；`field-survey-app` 额外覆盖 `SameMinor` 升级拒绝与 `[PLAN-DAG-001]` G1 执行冻结节点改写拒绝；`knowledge-observatory-app` 额外覆盖五维预算耗尽 fail-closed 与幂等重放。

### 1.2 用户态 Driver 适配器 × 2(版本化扩展封装,`DRV-PROTO-001`)

| 样板 | 协议 | 版本化扩展 | 关键机制 |
|---|---|---|---|
| [examples/ecosystem-drivers/mcp-bridge-driver](../../../examples/ecosystem-drivers/mcp-bridge-driver) | **MCP 兼容**（JSON-RPC 2.0 + 换行分隔 stdio 帧） | `llmos.ext.driver.mcp-bridge@1.0`,支持 `2025-03-26` / `2024-11-05` | `initialize` 握手 fail-closed 版本协商（不交集即断连 `Closed`）、`tools/list` / `tools/call`、**四类 `EffectDescriptor` 工具目录**（READ_ONLY / REVERSIBLE / COMPENSATABLE+补偿器 / IRREVERSIBLE）、传输静默注入下的崩溃 `UNCERTAIN` → 同幂等键 reconcile → Ready 恢复 → 同键重放不重复执行 |
| [examples/ecosystem-drivers/openai-compat-driver](../../../examples/ecosystem-drivers/openai-compat-driver) | **OpenAI-compatible**（`/v1/chat/completions` 同步 + SSE 流式） | `llmos.ext.driver.openai-compat@1.0`,wire `v1` | 请求/响应 codec（`usage` 三元组）、`chat.completion.chunk` 增量聚合与 `data: [DONE]` 终止、真实 `nlos-resource` `ResourceAuthority` 五维预算计量（请求摘要派生幂等键，同键重放同一收据、预算耗尽 fail-closed） |

共享 kit [ecosystem-driver-kit](../../../examples/ecosystem-drivers/ecosystem-driver-kit) 提供有界 RFC 8259 codec（深度 64 上限、代理对/控制字符 fail-closed）、`effect = <id> class=<TOKEN> reconcile=same-idempotency-key [compensation=<id>]` 声明清单解析（mutating 缺 reconcile 或禁用指令一律拒绝，镜像 Gate 1 语义）、`PendingCallFence`（dispatch → crash→`EffectUnknown` → 同键 reconcile → settled 重放；异操作同键冲突拒绝）与 `DRV-NEGO-001` 版本交集协商。

**诚实边界**：受 `nlos-package` CLI 构建器清单语法限制（仅认 `package-id/version/entry/task` 四键），`effect =` 声明行无法进入 `package.manifest`（属 nlos-migrate/certify 扩展语法，`certify.manifest` sidecar 亦不收编 base 键）；因此 Driver 包的四类 `EffectDescriptor` 与 reconcile 声明以包内数据文件（`extensions/*-effects.manifest`，由适配器在运行时解析并强制校验）与 kit/适配器协商面承载，并由上述单元测试验证，而非 Gate 1 的 manifest 级检查。此差异已如实登记，不冒充 Gate 1 已覆盖驱动副作用完备性。

---

## 2. 仓库既有工具链端到端封存（`nlos-package` → `nlos-certify` → `nlos-publisher-trust` → `nlos-store-registry`）

### 2.1 一条命令复现

```bash
bash examples/ecosystem-apps/seal-n1-baseline.sh
```

脚本（[seal-n1-baseline.sh](../../../examples/ecosystem-apps/seal-n1-baseline.sh)）依次执行：

1. 根工作区 `cargo build --locked -p nlos-package -p nlos-certify`（`CARGO_TARGET_DIR` 可注入实现并行车道隔离）；
2. 对 4 包 × 2 版本共 8 个树执行 `nlos-package keygen --seed <64hex>`（每包固定种子）→ `build --key --out <sealed>/<label>.v<version>.nlos-pkg` → `conformance` → `verify`（丢弃式仓库验签）；
3. `nlos-certify certify --package-dir <package> --upgrade-dir <package-v1.1.0>`：text 报告实跑 + `--format json` 落盘 `<sealed>/<label>.v1.0.0.certify.json`；
4. [n1-baseline-sealer](../../../examples/ecosystem-apps/n1-baseline-sealer)（本车道新增，仅编排公共库）：`OfflinePlatformRootCa`（种子 `0xE6×32`）→ `StoreIntermediateCertificate`（种子 `0xB0×32`）→ 每包 `CertifiedPublisher` 叶证书 + `SqlitePublisherTrustAuthority` + `RevocationEpoch=1` 空 CRL；接真实信任权威跑 `run_certification_pipeline` 四门并验签 `StoreCertificationReceipt`；`LocalOciCasStore` 五类槽位层写入 + `verify_stored_layout` 复核；发布者签名 `StoreReleaseSubmission` → `submit_release`（`ApplicationId` 防劫持绑定 certify 侧与运行时侧双 ID）→ `SignedCertificationAttestation`（`passed_gates_mask=0b1111`）→ `promote_candidate_to_stable` → 回读断言 `Stable`；最后哈希冻结 `.nlos-pkg` 并写 `<sealed>/<label>.v1.0.0.store-seal.txt` 封存记录（含全部摘要/收据/签名，路径统一正斜杠）。

全部种子与时间戳为固定常量（`SEAL_T0_MS = 1_767_225_600_000`），**连续两次全脚本运行 16/16 封存产物字节一致**（`diff` 零差异实测）。

### 2.2 四门认证实测（4/4 PASS × 4 包,0 blocker 0 warning）

以 `knowledge-observatory-app` v1.0.0 为例（其余三包同形，收据 ID 见 §3）：

```text
NLOS Store Certification Pipeline v1: CERTIFIED (4/4 GATES PASS)
  package: 6b6e6f776c65646f6273763130303031@1.0.0 (application_id=88d9d6afae6b0384e2809ff93d3d36dc)
  profile: APPLICATION_PLATFORM, target_sabi_epoch: 1, manifest_digest: 468ab3ddbb0b1e7e2ac50e92f1d1d41f444907c3f0a63f9ef4465924e9dbb271
  [PASS] GATE1 (blockers=0, warnings=0) -- target_sabi_epoch=1.0, host_sabi_epoch=3
  [PASS] GATE2 (scanned_files=23)   [PASS] GATE3 (v1->v2 gen=2, rollback drills=2)
  [PASS] GATE4 (task_guard=true, caps_revoked=2, cache_residue=0, user_data_preserved=true)
```

### 2.3 冻结 `.nlos-pkg` 的 CLI 全生命周期消费实跑（W81 直接输入预演）

对冻结产物用根工作区 CLI 实跑（`--root` 独立状态目录）：

```text
nlos-package install <sealed>/knowledge-observatory-app.v1.0.0.nlos-pkg --root <DIR>
  -> INSTALL ce9ae86e... decision=installed generation=1 entries=6 executables=curator
nlos-package run 6b6e6f...31 curator --root <DIR>   -> decision=executed
nlos-package update <sealed>/...v1.1.0.nlos-pkg --root <DIR> --compat same-major -> decision=updated
nlos-package uninstall 6b6e6f...31 --root <DIR>      -> decision=uninstalled
```

---

## 3. N-1 基准封存产物清单（SHA-256,冻结入库）

`N-1` 语义：包声明 `target_sabi_epoch = 1`（SABI v1 冻结纪元），封存时宿主 `host_sabi_epoch = 3`，位于 `N/N-1/N-2` 兼容窗口内；`W81` `ROAD-E-001` 升级主系统至 `N` 后须以**零重编译、零重打包、零重签**消费以下字节冻结制品（路径相对仓库根）：

| 产物 | SHA-256（前 16 hex） | 字节 |
|---|---|---|
| examples/ecosystem-apps/knowledge-observatory-app/sealed/knowledge-observatory-app.v1.0.0.nlos-pkg | `690c4e296bf3b0be` | 2538 |
| examples/ecosystem-apps/knowledge-observatory-app/sealed/knowledge-observatory-app.v1.1.0.nlos-pkg | `0bc978a23cc62571` | 2828 |
| examples/ecosystem-apps/field-survey-app/sealed/field-survey-app.v1.0.0.nlos-pkg | `4e529d61239bec8b` | 2341 |
| examples/ecosystem-apps/field-survey-app/sealed/field-survey-app.v1.1.0.nlos-pkg | `59f219d130fd1845` | 2618 |
| examples/ecosystem-drivers/mcp-bridge-driver/sealed/mcp-bridge-driver.v1.0.0.nlos-pkg | `6bff8ec8aeaff6d4` | 2327 |
| examples/ecosystem-drivers/mcp-bridge-driver/sealed/mcp-bridge-driver.v1.1.0.nlos-pkg | `9a6ec0ebb328a506` | 2778 |
| examples/ecosystem-drivers/openai-compat-driver/sealed/openai-compat-driver.v1.0.0.nlos-pkg | `db0d93d156dd07f6` | — |
| examples/ecosystem-drivers/openai-compat-driver/sealed/openai-compat-driver.v1.1.0.nlos-pkg | `0467b105904b8b01` | — |

全 16 文件哈希（8×`.nlos-pkg` + 4×`.certify.json` + 4×`.store-seal.txt`）与每包完整封存记录（发布者叶证书序列、`publisher_id`、双 `application_ids`、certify manifest digest、OCI manifest digest、四门位掩码、认证收据 ID+签名、release submission digest、attestation 签名、`Candidate->Stable epoch=1`、五类槽位映射）已入库于各 `<example>/sealed/`；每份 `.store-seal.txt` 首行即复现命令。四包认证收据 ID（sealer 全链路,与 CLI 版签名密钥不同属预期——CLI 为静态单发布者便捷验证器,sealer 为真实三级 PKI）：

- `knowledge-observatory-app`：receipt `f0deac49301d3b004a841b5b2241fa6c`，OCI `0178612d…35a22`
- `field-survey-app`：receipt `b8b4cfe956735f12190084aa3a402589`，OCI `0fafaa49…506c`
- `mcp-bridge-driver`：receipt `92ddb0b144f002c2f5ee18f6ec16a96b`，OCI `243e6886…cdc74`
- `openai-compat-driver`：receipt `e465c084b13729062a2289056416504d`，OCI `c7523ad6…1f02`

---

## 4. 验证数据（实际命令与结果数字）

| 命令 | 结果 |
|---|---|
| `cd examples/ecosystem-drivers && cargo fmt --check` | 通过 |
| `cd examples/ecosystem-drivers && cargo clippy --all-targets -- -D warnings` | 0 error / 0 warning |
| `cd examples/ecosystem-drivers && cargo test` | **31/31 通过**（kit 11 + mcp 12 + openai 8） |
| `cd examples/ecosystem-apps && cargo fmt --check` | 通过 |
| `cd examples/ecosystem-apps && cargo clippy --all-targets -- -D warnings` | 0 error / 0 warning |
| `cd examples/ecosystem-apps && cargo test` | **11/11 通过**（observatory 4 + field-survey 4 + sealer 3,其中 sealer 含隔离 fixture 全链路重放测试） |
| `bash examples/ecosystem-apps/seal-n1-baseline.sh` | 退出 0；8×build/conformance/verify + 4×certify 4/4 + 4×Candidate→Stable + 4 封存记录 |
| 连续两次全脚本运行后 `sha256sum` 全量 diff | **16/16 字节一致（零差异）** |
| `nlos-package install/run/update/uninstall`（冻结 observatory 包） | installed → executed → updated → uninstalled 全链路实跑通过 |

未运行项（如实声明）：三平台 CI 矩阵（由 `W80-INT` 合流后统一跑）；`W81` `N-1 -> N` 原地升级消费（属 `ROAD-E-001` 总门禁,本车道仅交付冻结消费品与复现命令）；驱动适配器未接真实外部 MCP 服务器 / 商业网关进程（以脚本化传输 double 覆盖协议与围栏语义,真实进程接线属后续生态工作）；`nlos-wasm-sandbox` 运行时装载（属 `W80-E6A` 车道）。

---

## 5. 对 `W81` 消费方的接口说明

1. **安装输入**：直接使用 §3 表中 `.nlos-pkg` 字节冻结制品（`nlos-package install/verify` 或等价库路径），禁止重新构建/重签;
2. **升级演练输入**：各包 `v1.1.0` 制品 + 两 App 计划 revision 2 冻结节点语义（见各 crate 测试);
3. **目录核验输入**：各 `.store-seal.txt` 的 `oci_manifest_digest` / `certification_receipt_signature` / `attestation_signature` 可由 `nlos-store-registry` 与 `nlos-certify` 公共 API 独立复核;
4. **复现**：`bash examples/ecosystem-apps/seal-n1-baseline.sh`（或 sealer 单测 `seal_all_replays_end_to_end_against_an_isolated_fixture_copy` 的进程内重放）。
