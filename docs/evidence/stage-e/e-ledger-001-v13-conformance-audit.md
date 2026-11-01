# E-LEDGER-001：阶段 E ledger-schema v1.3 台账覆盖率机械核验与合规审计证据（W81-E7B）

- **Evidence ID**: `e-ledger-001-v13-conformance-audit`
- **Wave / Lane**: `W81 / W81-E7B-LEDGER-SYNC`（里程碑 `M-E5` 中路，工作包 `E7` 台账覆盖率核验车道）
- **日期**: `2026-11-01`
- **Assurance Level**: `H4`（纯 Python 3 标准库确定性本地机械核验、13 项正负向篡改注入单测与 `TemporaryDirectory` 隔离合流预演验证；不声称 `H7` 安全对抗渗透或生产运营证明）
- **状态**: `VERIFIED`（经 `W81-INT` 合流回填三份权威机器台账并通过 `lint_claims.py` `204/204` `0 ERROR` 与 `verify_stage_e_ledger_coverage.py --mode strict` 硬门禁，正式晋升 `VERIFIED`）
- **权威规范与治理依据**:
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §28.5（阶段 E 长期兼容制度、应用/驱动商店治理与生态工具链及 `[ROAD-E-001]`）
  - [ledger-schema.md](../../management/ledger-schema.md)（`v1.3` `FROZEN-additive`：`stage: E` 枚举扩域、`Claim <= Evidence` 支撑格 `rank(POC)=3, rank(Hn)=n`、双向 `docs/evidence/stage-e/` 索引与 §5 Canonical YAML 子集）
  - [stage-e-progress.md](../../management/stage-e-progress.md)（§E.1.1 规范映射表、§E.2.2 `E0` + `E1..E7` 工作包矩阵、§E.3.2 `W81-E7B` 独占写集契约、§E.5.2 `RISK-E-01..05` 预登记风险、§E.5.5 退出门禁 3 与门禁 4）
  - [exit-unknown-risks.md](../../management/exit-unknown-risks.md)（阶段 E 关联未知项 `U-7`、`U-8`、`U-9` 收口状态）
  - [ADR-0027 Stage E SABI 长期向后兼容与商店治理](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md)
  - [scripts/lint_claims.py](../../../scripts/lint_claims.py) 与 [scripts/test_lint_claims.py](../../../scripts/test_lint_claims.py)（`W76-SCHEMA-V13` 基线校验器）

---

## 1. 交付物架构与机械核验门禁（`tools/verify_stage_e_ledger_coverage.py`）

在 [tools/verify_stage_e_ledger_coverage.py](../../../tools/verify_stage_e_ledger_coverage.py) 中以零第三方依赖纯 Python 3 标准库（`argparse`、`importlib.util`、`json`、`pathlib`、`re`、`shutil`、`sys`、`tempfile`、`unittest`）实现阶段 E 台账覆盖率机械核验器与候选回填生成器，直接复用 [scripts/lint_claims.py](../../../scripts/lint_claims.py) 的内置 Canonical YAML 子集解析器、引用解析器与支撑格校验器：

1. **权威源动态提取（`extract_stage_e_spec`）**：
   - 动态解析 [stage-e-progress.md](../../management/stage-e-progress.md) §E.1.1 映射表提取全部 `44` 项规范 Requirement ID（含 `ROAD-E-001`），并从 §E.2.2 工作包矩阵自动提取补充规范条款 `DUI-ASSOC-001`（`E5` 行），合计 **`45` 项规范 Requirement ID**；
   - 从 §E.2.2 第一列提取全部 **`8` 项阶段 E 工作包 canonical ID**（`E-TRIAL-BOOTSTRAP`、`E-SABI-POLICY-CORPUS`、`E-COMPAT-SHIM`、`E-MIGRATE-KIT`、`E-PUBLISHER-TRUST`、`E-STORE-CERTIFY`、`E-MARKET-SANDBOX-H7`、`E-UPGRADE-GATE-ROADE001`），合计 **`53` 项阶段 E 必检台账声明 ID**；
   - 从 §E.5.2 风险表动态提取 **`RISK-E-01`..`RISK-E-05`** 的权威 `category` 与 `severity`（全部为 `P1`），并绑定跨阶段继承风险 `RISK-B-05` 与 `RISK-B-09`。
2. **全局唯一性分类与防重复回填（`classify_stage_e_claims`）**：
   - `claims.yaml` 强制 `requirement_id` 跨 `B / C / D / E` 全阶段全局唯一；
   - 核验器将 `53` 项阶段 E ID 对照既有 [claims.yaml](../../management/claims.yaml) 自动分为两类：
     - **(a) 已在前序阶段（`B / C / D`）注册**：若存在则仅做非退化核验，严禁重复发射同名 `requirement_id`（由内置单测 `test_04` 专项覆盖）；
     - **(b) 尚未进入 `claims.yaml`**：自动生成严格符合 [ledger-schema.md](../../management/ledger-schema.md) §5 Canonical YAML 子集规范的 `stage: E`、`status: PARTIAL_PASS` 候选条目（零行尾注释、枚举裸 token、特殊字符与日期双引号转义、非空 `SATISFIED-WITH-BOUNDARIES` `limitations`）。
3. **四种 CLI 运行模式**：
   - `--mode strict`（默认模式，供 `W81-INT` 合流后与常设 CI 作为阶段 E 硬退出门）：依次核验（1）`lint_claims.py` `0 ERROR`；（2）`45` 项规范 Requirement ID + `8` 项 `E-*` 工作包 100% 在册、全局 ID 唯一且 `assurance != DESIGN`（仅显式豁免既有治理/硬件掉电基线项 `B-MGMT`、`C-LIFECYCLE`、`C-POWER-LOSS`），同时复核 `rank(claim.assurance) <= max(rank(evidence_refs))`；（3）`RISK-E-01..05`、`RISK-B-05`、`RISK-B-09` 全部在册且类别/严重度与 §E.5.2 一致，全表未关闭 `P0` 风险数 `== 0`；（4）`docs/evidence/stage-e/**/*.md`（`>= 16` 份，含三份 `W81` 证据）与 `evidence-index.yaml` 100% 双向一致；（5）[stage-e-progress.md](../../management/stage-e-progress.md)、[ADR-0027](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) 与 `docs/evidence/stage-e/**/*.md` 零象形 Emoji、零断链相对路径。
   - `--mode pre-backfill`（`W81-E7B` 合流前预演门禁）：先校验当前工作区共享台账基线零异常（透明识别并仅豁免当前分支尚未合入 `evidence-index.yaml` 的 `W81` 待索引新证据文件），随后在 `tempfile.TemporaryDirectory()` 中构造隔离的 `W81-INT` 合流后模拟仓库树（仅对真实存在于当前工作区的引用路径生成轻量占位符，并为尚未合流的并行兄弟车道 `W81-E7A` / `W81-E7C` 交付路径建立显式 stub，杜绝掩盖真实路径缺失），追加生成的三段候选 YAML 后完整执行 `--mode strict` 与 `lint_claims.py`。
   - `--emit-candidates [--section all|evidence|claims|risks]`：向标准输出打印可直接供 `W81-INT` 审阅并合入 [evidence-index.yaml](../../management/evidence-index.yaml)、[claims.yaml](../../management/claims.yaml)、[risks.yaml](../../management/risks.yaml) 的 Canonical YAML 候选块。
   - `--self-test` 与 `--json`：内置 `13` 项正负向 `unittest` 套件与机器可读 JSON 审计报告输出。

---

## 2. 阶段 E 全量 Requirement ID 与工作包台账分类审计矩阵（`116 -> 169`）

### 2.1 分类统计真值

| 统计维度 | 数量 | 说明 |
| :--- | :---: | :--- |
| 既有 `claims.yaml` 基线条目数（`W80-INT` 基线 `a44d4cd4`） | `116` | `Stage B = 71`（`ROAD-B-*` / `B-*` / `X-*`）、`Stage C = 35`（`ROAD-C-*` / `C-*`）、`Stage D = 10`（`ROAD-D-001` / `UI-PARITY-001` / `D-*`） |
| `stage-e-progress.md` §E.1.1 规范 Requirement ID | `44` | 含阶段退出总门禁 `ROAD-E-001` 及 `43` 项跨领域规范条款 |
| `stage-e-progress.md` §E.2.2 补充规范 Requirement ID | `1` | `DUI-ASSOC-001`（见 §E.2.2 `E5` 行：默认 URI/MIME/文件关联防静默劫持门禁） |
| `stage-e-progress.md` §E.2.2 工作包 canonical ID（`E0` + `E1..E7`） | `8` | `E-TRIAL-BOOTSTRAP`..`E-UPGRADE-GATE-ROADE001` |
| **阶段 E 必检台账声明总计** | **`53`** | `45` 项规范 Requirement ID + `8` 项工作包 ID |
| 分类 `(a)`：已在 `B / C / D` 阶段占用同名 `requirement_id` | `0` | 前序阶段全部采用 `B-*` / `C-*` / `D-*` 工作包标识，与 `[XXX-YYY-001]` 条款 ID 零重名冲突；前序 `116` 条目全部保持非退化原貌 |
| 分类 `(b)`：本次新增 `stage: E` 候选条目（`status: PARTIAL_PASS`） | **`53`** | 其中 `assurance: H4` 共 `44` 条、`assurance: H7` 共 `9` 条（每一条 `H7` 均至少引用一份 `H7` 级证据） |
| **`W81-INT` 回填后 `claims.yaml` 总条目数** | **`169`** | `B: 71` + `C: 35` + `D: 10` + `E: 53 = 169`（全局唯一性 100%） |

### 2.2 全量 53 项阶段 E 声明逐项审计表

| # | `requirement_id` | 权威来源章节 | 合流前 `claims.yaml` 状态 | 候选等级 (`stage / status / assurance`) | 最高支撑证据与核心验证载体（满足 `Claim <= Evidence`） |
| :---: | :--- | :--- | :--- | :--- | :--- |
| 1 | `ROAD-E-001` | `§E.1.1 / §E.2.2 E7 / §E.5.5` | 未登记 -> 新增 | `E / PARTIAL_PASS / H7` | `e-road-001` (`H7`), [e-market-001](e-market-001-reference-apps-and-drivers.md) (`H4`), `w81-stage-e-exit-gates` (`H7`) |
| 2 | `COMPAT-VER-001` | `§E.1.1 / §E.2.2 E1,E3,E5` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-sabi-001](e-sabi-001-version-matrix-and-deprecation.md) (`H4`), [e-migrate-001](e-migrate-001-static-checker-and-rollbacks.md) (`H4`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`) |
| 3 | `COMPAT-VER-002` | `§E.1.1 / §E.2.2 E1,E2,E3` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-sabi-001](e-sabi-001-version-matrix-and-deprecation.md) (`H4`), [e-shim-001](e-shim-001-out-of-core-epoch-adapter.md) (`H4`), [e-migrate-001](e-migrate-001-static-checker-and-rollbacks.md) (`H4`) |
| 4 | `COMPAT-DEPRECATE-001` | `§E.1.1 / §E.2.2 E1,E3` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-sabi-001](e-sabi-001-version-matrix-and-deprecation.md) (`H4`), [e-migrate-001](e-migrate-001-static-checker-and-rollbacks.md) (`H4`) |
| 5 | `KABI-IDEM-001` | `§E.1.1 / §E.2.2 E1,E2,E5` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-sabi-002](e-sabi-002-five-lang-golden-corpus.md) (`H4`), [e-shim-001](e-shim-001-out-of-core-epoch-adapter.md) (`H4`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`) |
| 6 | `KABI-HANDLE-001` | `§E.1.1 / §E.2.2 E2,E4` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-shim-001](e-shim-001-out-of-core-epoch-adapter.md) (`H4`), [e-trust-001](e-trust-001-publisher-pki-revocation-and-selectors.md) (`H4`) |
| 7 | `KABI-SVC-001` | `§E.1.1 / §E.2.2 E0,E1,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-trial-001](e-trial-001-llm-http-driver.md) (`H4`), [e-sabi-001](e-sabi-001-version-matrix-and-deprecation.md) (`H4`), [e-market-001](e-market-001-reference-apps-and-drivers.md) (`H4`) |
| 8 | `KABI-ERR-001` | `§E.1.1 / §E.2.2 E1,E2` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-sabi-002](e-sabi-002-five-lang-golden-corpus.md) (`H4`), [e-shim-001](e-shim-001-out-of-core-epoch-adapter.md) (`H4`) |
| 9 | `KABI-UNCERTAIN-001` | `§E.1.1 / §E.2.2 E2,E3,E5` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-shim-001](e-shim-001-out-of-core-epoch-adapter.md) (`H4`), [e-migrate-001](e-migrate-001-static-checker-and-rollbacks.md) (`H4`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`) |
| 10 | `SABI-DISC-001` | `§E.1.1 / §E.2.2 E0,E1` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-trial-001](e-trial-001-llm-http-driver.md) (`H4`), [e-sabi-001](e-sabi-001-version-matrix-and-deprecation.md) (`H4`), [e-sabi-002](e-sabi-002-five-lang-golden-corpus.md) (`H4`) |
| 11 | `SABI-COMMON-001` | `§E.1.1 / §E.2.2 E1` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-sabi-001](e-sabi-001-version-matrix-and-deprecation.md) (`H4`), [e-sabi-002](e-sabi-002-five-lang-golden-corpus.md) (`H4`) |
| 12 | `SABI-RECEIPT-001` | `§E.1.1 / §E.2.2 E0,E1,E5` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-trial-001](e-trial-001-llm-http-driver.md) (`H4`), [e-sabi-002](e-sabi-002-five-lang-golden-corpus.md) (`H4`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`) |
| 13 | `SABI-AUTH-001` | `§E.1.1 / §E.2.2 E0,E1` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-trial-001](e-trial-001-llm-http-driver.md) (`H4`), [e-sabi-002](e-sabi-002-five-lang-golden-corpus.md) (`H4`) |
| 14 | `SABI-TRANSPORT-001` | `§E.1.1 / §E.2.2 E1,E2` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-sabi-002](e-sabi-002-five-lang-golden-corpus.md) (`H4`), [e-shim-001](e-shim-001-out-of-core-epoch-adapter.md) (`H4`) |
| 15 | `SABI-CONFORM-001` | `§E.1.1 / §E.2.2 E1,E5` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-sabi-001](e-sabi-001-version-matrix-and-deprecation.md) (`H4`), [e-sabi-002](e-sabi-002-five-lang-golden-corpus.md) (`H4`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`) |
| 16 | `DRV-NEGO-001` | `§E.1.1 / §E.2.2 E0,E2,E5,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-shim-001](e-shim-001-out-of-core-epoch-adapter.md) (`H4`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`), [e-market-001](e-market-001-reference-apps-and-drivers.md) (`H4`) |
| 17 | `DRV-BOUND-001` | `§E.1.1 / §E.2.2 E2,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-shim-001](e-shim-001-out-of-core-epoch-adapter.md) (`H4`), [e-sandbox-001](e-sandbox-001-cross-platform-wasm-process-isolation.md) (`H4`) |
| 18 | `DRV-ACTIVE-001` | `§E.1.1 / §E.2.2 E2,E5` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-shim-001](e-shim-001-out-of-core-epoch-adapter.md) (`H4`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`) |
| 19 | `DRV-CRED-001` | `§E.1.1 / §E.2.2 E0,E3,E5` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-trial-001](e-trial-001-llm-http-driver.md) (`H4`), [e-migrate-001](e-migrate-001-static-checker-and-rollbacks.md) (`H4`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`) |
| 20 | `DRV-PROTO-001` | `§E.1.1 / §E.2.2 E0,E1,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-sabi-002](e-sabi-002-five-lang-golden-corpus.md) (`H4`), [e-sandbox-001](e-sandbox-001-cross-platform-wasm-process-isolation.md) (`H4`), [e-market-001](e-market-001-reference-apps-and-drivers.md) (`H4`) |
| 21 | `DRV-LIFE-001` | `§E.1.1 / §E.2.2 E0,E2,E5,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-shim-001](e-shim-001-out-of-core-epoch-adapter.md) (`H4`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`), [e-sandbox-001](e-sandbox-001-cross-platform-wasm-process-isolation.md) (`H4`), [e-market-001](e-market-001-reference-apps-and-drivers.md) (`H4`) |
| 22 | `DRV-SHIM-001` | `§E.1.1 / §E.2.2 E2` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-shim-001](e-shim-001-out-of-core-epoch-adapter.md) (`H4`) |
| 23 | `DRV-DISC-001` | `§E.1.1 / §E.2.2 E4,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-trust-001](e-trust-001-publisher-pki-revocation-and-selectors.md) (`H4`), [e-market-001](e-market-001-reference-apps-and-drivers.md) (`H4`) |
| 24 | `PKG-SEP-001` | `§E.1.1 / §E.2.2 E5,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-store-001](e-store-001-oci-index-and-entitlement-ledger.md) (`H4`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`), [e-market-001](e-market-001-reference-apps-and-drivers.md) (`H4`) |
| 25 | `PKG-IMM-001` | `§E.1.1 / §E.2.2 E0,E3,E5` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-trial-002](e-trial-002-quickstart-and-starter-app.md) (`H4`), [e-migrate-001](e-migrate-001-static-checker-and-rollbacks.md) (`H4`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`) |
| 26 | `PKG-SEED-001` | `§E.1.1 / §E.2.2 E3,E5,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-migrate-001](e-migrate-001-static-checker-and-rollbacks.md) (`H4`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`), [e-market-001](e-market-001-reference-apps-and-drivers.md) (`H4`) |
| 27 | `PKG-UPDATE-001` | `§E.1.1 / §E.2.2 E3,E5,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H7` | [e-security-001](e-security-001-h7-supply-chain-and-revocation-adversarial.md) (`H7`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`), [e-migrate-001](e-migrate-001-static-checker-and-rollbacks.md) (`H4`) |
| 28 | `PKG-UNINSTALL-001` | `§E.1.1 / §E.2.2 E5,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H7` | [e-security-001](e-security-001-h7-supply-chain-and-revocation-adversarial.md) (`H7`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`) |
| 29 | `PKG-OCI-001` | `§E.1.1 / §E.2.2 E5,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H7` | [e-security-001](e-security-001-h7-supply-chain-and-revocation-adversarial.md) (`H7`), [e-store-001](e-store-001-oci-index-and-entitlement-ledger.md) (`H4`), [e-market-002](e-market-002-desktop-store-transparency-ui.md) (`H4`) |
| 30 | `PKG-MULTI-001` | `§E.1.1 / §E.2.2 E0,E5,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-trial-002](e-trial-002-quickstart-and-starter-app.md) (`H4`), [e-store-001](e-store-001-oci-index-and-entitlement-ledger.md) (`H4`), [e-market-001](e-market-001-reference-apps-and-drivers.md) (`H4`) |
| 31 | `PKG-APPID-001` | `§E.1.1 / §E.2.2 E4,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H7` | [e-security-001](e-security-001-h7-supply-chain-and-revocation-adversarial.md) (`H7`), [e-trust-001](e-trust-001-publisher-pki-revocation-and-selectors.md) (`H4`) |
| 32 | `TCB-BYPASS-001` | `§E.1.1 / §E.2.2 E2,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-shim-001](e-shim-001-out-of-core-epoch-adapter.md) (`H4`), [e-sandbox-001](e-sandbox-001-cross-platform-wasm-process-isolation.md) (`H4`) |
| 33 | `TCB-EGRESS-001` | `§E.1.1 / §E.2.2 E0,E5,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-trial-001](e-trial-001-llm-http-driver.md) (`H4`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`), [e-sandbox-001](e-sandbox-001-cross-platform-wasm-process-isolation.md) (`H4`) |
| 34 | `TCB-DOWNGRADE-001` | `§E.1.1 / §E.2.2 E4,E5,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-store-001](e-store-001-oci-index-and-entitlement-ledger.md) (`H4`), [e-sandbox-001](e-sandbox-001-cross-platform-wasm-process-isolation.md) (`H4`), [e-market-002](e-market-002-desktop-store-transparency-ui.md) (`H4`) |
| 35 | `CAP-UNFORGE-001` | `§E.1.1 / §E.2.2 E4,E5` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-trust-001](e-trust-001-publisher-pki-revocation-and-selectors.md) (`H4`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`) |
| 36 | `CAP-ATTEN-001` | `§E.1.1 / §E.2.2 E2,E4,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-shim-001](e-shim-001-out-of-core-epoch-adapter.md) (`H4`), [e-trust-001](e-trust-001-publisher-pki-revocation-and-selectors.md) (`H4`), [e-sandbox-001](e-sandbox-001-cross-platform-wasm-process-isolation.md) (`H4`) |
| 37 | `CAP-REVOKE-001` | `§E.1.1 / §E.2.2 E4,E5,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H7` | [e-security-001](e-security-001-h7-supply-chain-and-revocation-adversarial.md) (`H7`), [e-trust-001](e-trust-001-publisher-pki-revocation-and-selectors.md) (`H4`), [e-store-001](e-store-001-oci-index-and-entitlement-ledger.md) (`H4`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`) |
| 38 | `TUI-CONSENT-001` | `§E.1.1 / §E.2.2 E0,E5,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-trial-001](e-trial-001-llm-http-driver.md) (`H4`), [e-market-002](e-market-002-desktop-store-transparency-ui.md) (`H4`) |
| 39 | `NLOS-DEFER-001` | `§E.1.1 / §E.2.3` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-store-001](e-store-001-oci-index-and-entitlement-ledger.md) (`H4`), 本证据 `e-ledger-001` (`H4`) |
| 40 | `EVID-SCOPE-001` | `§E.1.1 / §E.2.1 / §E.5.5` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | 本证据 `e-ledger-001` (`H4`) |
| 41 | `EVID-NONLINEAR-001` | `§E.1.1 / §E.3.2` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | 本证据 `e-ledger-001` (`H4`) |
| 42 | `EVID-NEG-001` | `§E.1.1 / §E.2.2 E2,E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H7` | [e-security-001](e-security-001-h7-supply-chain-and-revocation-adversarial.md) (`H7`), [e-sandbox-001](e-sandbox-001-cross-platform-wasm-process-isolation.md) (`H4`), [e-shim-001](e-shim-001-out-of-core-epoch-adapter.md) (`H4`) |
| 43 | `EVID-REPRO-001` | `§E.1.1 / §E.2.2 E0,E6,E7` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-trial-001](e-trial-001-llm-http-driver.md) (`H4`), [e-market-001](e-market-001-reference-apps-and-drivers.md) (`H4`), 本证据 `e-ledger-001` (`H4`) |
| 44 | `EVID-GEN-001` | `§E.1.1 / §E.2.2 E1` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-sabi-001](e-sabi-001-version-matrix-and-deprecation.md) (`H4`), [e-sabi-002](e-sabi-002-five-lang-golden-corpus.md) (`H4`) |
| 45 | `DUI-ASSOC-001` | `§E.2.2 E5` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`) |
| 46 | `E-TRIAL-BOOTSTRAP` | `§E.2.2 E0` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-trial-001](e-trial-001-llm-http-driver.md) (`H4`), [e-trial-002](e-trial-002-quickstart-and-starter-app.md) (`H4`) |
| 47 | `E-SABI-POLICY-CORPUS` | `§E.2.2 E1` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-sabi-001](e-sabi-001-version-matrix-and-deprecation.md) (`H4`), [e-sabi-002](e-sabi-002-five-lang-golden-corpus.md) (`H4`) |
| 48 | `E-COMPAT-SHIM` | `§E.2.2 E2` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-shim-001](e-shim-001-out-of-core-epoch-adapter.md) (`H4`) |
| 49 | `E-MIGRATE-KIT` | `§E.2.2 E3` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-migrate-001](e-migrate-001-static-checker-and-rollbacks.md) (`H4`) |
| 50 | `E-PUBLISHER-TRUST` | `§E.2.2 E4` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-trust-001](e-trust-001-publisher-pki-revocation-and-selectors.md) (`H4`) |
| 51 | `E-STORE-CERTIFY` | `§E.2.2 E5` | 未登记 -> 新增 | `E / PARTIAL_PASS / H4` | [e-store-001](e-store-001-oci-index-and-entitlement-ledger.md) (`H4`), [e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md) (`H4`) |
| 52 | `E-MARKET-SANDBOX-H7` | `§E.2.2 E6` | 未登记 -> 新增 | `E / PARTIAL_PASS / H7` | [e-security-001](e-security-001-h7-supply-chain-and-revocation-adversarial.md) (`H7`), [e-sandbox-001](e-sandbox-001-cross-platform-wasm-process-isolation.md) (`H4`), [e-market-001](e-market-001-reference-apps-and-drivers.md) (`H4`), [e-market-002](e-market-002-desktop-store-transparency-ui.md) (`H4`) |
| 53 | `E-UPGRADE-GATE-ROADE001` | `§E.2.2 E7 / §E.3.2 W81` | 未登记 -> 新增 | `E / PARTIAL_PASS / H7` | `e-road-001` (`H7`), 本证据 `e-ledger-001` (`H4`), `w81-stage-e-exit-gates` (`H7`) |

---

## 3. 风险台账（`RISK-E-01..05`、跨阶段 `RISK-B-05/09`）与未知项（`U-7..U-9`）审计矩阵（`P0 = 0`）

- **基线核验**：当前 [risks.yaml](../../management/risks.yaml) 共 `12` 条目（`RISK-B-01`..`RISK-B-12`），未关闭 `P0` 风险数为 **`0`**（分布：`P1/closed 1, P1/open 1, P1/partial 8, P2/partial 2`）。
- **回填增量**：新增 `RISK-E-01`..`RISK-E-05` 共 `5` 条（全部 `severity: P1`、`status: partial`），合流后总计 **`17` 条风险**，未关闭 `P0` 风险数保持 **`0`**。

| 风险 / 未知项 ID | 类别 (`category`) | 严重度与候选状态 | 阶段 E 缓解实证与诚实残域 (`review_point`) |
| :--- | :--- | :--- | :--- |
| `RISK-E-01` | `premature-format-freeze` | `P1 / partial` | 已由 [e-sabi-001](e-sabi-001-version-matrix-and-deprecation.md)、[e-sabi-002](e-sabi-002-five-lang-golden-corpus.md)、[e-migrate-001](e-migrate-001-static-checker-and-rollbacks.md) 与 `e-road-001` 多工作负载实证冻结条件；正式 `SABI v1.0` 冻结签署与关联项 `RISK-B-05` 同步保留维护者终批门控 |
| `RISK-E-02` | `security-bypass` | `P1 / partial` | `crates/nlos-compat-shim` 以 `#![forbid(unsafe_code)]` 零特权运行于 `EnforcementShim` 之外，所有翻译输出重经权威中介并保留 `IdempotencyKey` 与 `UNCERTAIN`（[e-shim-001](e-shim-001-out-of-core-epoch-adapter.md)、`e-road-001`）；新增旧纪元垫片分支时强制复审 |
| `RISK-E-03` | `third-party-supply-chain` | `P1 / partial` | 三级发布者 PKI、SQLite `RAISE(ABORT)` 防回滚 `RevocationEpoch`、首次注册 `ApplicationId` 防抢注、纯 Rust 锁文件哈希/SPDX 门禁与 H7 六类攻击矩阵全部通过（[e-trust-001](e-trust-001-publisher-pki-revocation-and-selectors.md)、[e-store-001](e-store-001-oci-index-and-entitlement-ledger.md)、[e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md)、[e-security-001](e-security-001-h7-supply-chain-and-revocation-adversarial.md)）；外部公网运营、商业 CA 与物理硬件 HSM 属 `E-deferred` |
| `RISK-E-04` | `security-bypass` | `P1 / partial` | `crates/nlos-wasm-sandbox` L1 空环境专职子进程 + L2 手写 13 导入 WASI 白名单（零 socket 导入、双重路径围栏、64KiB 页硬限、fuel/epoch 中断，`54/54` 测试通过，[e-sandbox-001](e-sandbox-001-cross-platform-wasm-process-isolation.md)）；Linux/macOS `#[cfg(unix)]` 分支与极端写树内 TOCTOU 由远端三平台 CI 持续守护 |
| `RISK-E-05` | `durable-state-loss` | `P1 / partial` | `nlos-migrate` 与 Gate 3 强制演练探针失败与半途崩溃后 `UserDataContainer` SHA-256 树摘要逐位原子回滚，默认阻断 `IRREVERSIBLE` 自动升级并联动桌面商店 `Z4` 重授权（[e-migrate-001](e-migrate-001-static-checker-and-rollbacks.md)、[e-certify-001](e-certify-001-automated-conformance-and-uninstall-gate.md)、[e-security-001](e-security-001-h7-supply-chain-and-revocation-adversarial.md)）；跨主机物理断电台架与 `C-POWER-LOSS` 同口径保持显式边界 |
| `RISK-B-05`（跨阶段） | `premature-format-freeze` | `P1 / partial` | 已在 [risks.yaml](../../management/risks.yaml) 在册；`SABI v1.0` 多阶段消费者与生态样板验证已完成，待维护者审阅 `W81` 退出门后裁定正式冻结 |
| `RISK-B-09`（跨阶段） | `third-party-supply-chain` | `P1 / open -> partial` | 已在 [risks.yaml](../../management/risks.yaml) 在册；`W79` 已落地纯 Rust 锁文件完整性与递归下降 SPDX 许可证审计器及常设门禁 [test_lockfile_supply_chain_gate.py](../../../tests/conformance/schema/test_lockfile_supply_chain_gate.py)（根工作区 `166` Cargo + `119` npm 及 `desktop` `78` npm 包 `0 BLOCKER`） |
| `U-7` / `U-8` / `U-9` | [exit-unknown-risks.md](../../management/exit-unknown-risks.md) | **`CLOSED`（`3/3`）** | `U-7`（跨生态 Typed Selector 不可变句柄解析）与 `U-8`（G3 `WaitingAuthorization -> Materializing` 四门物化）已于 `W79` 闭合；`U-9`（三平台双层不可信 Wasm 强沙箱）已于 `W80` 闭合 |

---

## 4. 阶段 E 全量 16 份证据工件双向索引盘点（`201 -> 204`）

| # | `evidence-index.yaml` ID | 仓库相对路径 | 波次 / 车道 | 最高支撑等级 | 索引状态 |
| :---: | :--- | :--- | :--- | :---: | :--- |
| 1 | `e-trial-001-llm-http-driver` | [docs/evidence/stage-e/e-trial-001-llm-http-driver.md](e-trial-001-llm-http-driver.md) | `W77-PRE / E0` | `H4` | 已入库（`201` 基线内） |
| 2 | `e-trial-002-quickstart-and-starter-app` | [docs/evidence/stage-e/e-trial-002-quickstart-and-starter-app.md](e-trial-002-quickstart-and-starter-app.md) | `W77-PRE / E0` | `H4` | 已入库（`201` 基线内） |
| 3 | `e-sabi-001-version-matrix-and-deprecation` | [docs/evidence/stage-e/e-sabi-001-version-matrix-and-deprecation.md](e-sabi-001-version-matrix-and-deprecation.md) | `W77 / E1` | `H4` | 已入库（`201` 基线内） |
| 4 | `e-sabi-002-five-lang-golden-corpus` | [docs/evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md](e-sabi-002-five-lang-golden-corpus.md) | `W77 / E1` | `H4` | 已入库（`201` 基线内） |
| 5 | `e-shim-001-out-of-core-epoch-adapter` | [docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md](e-shim-001-out-of-core-epoch-adapter.md) | `W78 / E2` | `H4` | 已入库（`201` 基线内） |
| 6 | `e-migrate-001-static-checker-and-rollbacks` | [docs/evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md](e-migrate-001-static-checker-and-rollbacks.md) | `W78 / E3` | `H4` | 已入库（`201` 基线内） |
| 7 | `e-trust-001-publisher-pki-revocation-and-selectors` | [docs/evidence/stage-e/e-trust-001-publisher-pki-revocation-and-selectors.md](e-trust-001-publisher-pki-revocation-and-selectors.md) | `W79 / E4` | `H4` | 已入库（`201` 基线内） |
| 8 | `e-store-001-oci-index-and-entitlement-ledger` | [docs/evidence/stage-e/e-store-001-oci-index-and-entitlement-ledger.md](e-store-001-oci-index-and-entitlement-ledger.md) | `W79 / E5A` | `H4` | 已入库（`201` 基线内） |
| 9 | `e-certify-001-automated-conformance-and-uninstall-gate` | [docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md](e-certify-001-automated-conformance-and-uninstall-gate.md) | `W79 / E5B` | `H4` | 已入库（`201` 基线内） |
| 10 | `e-sandbox-001-cross-platform-wasm-process-isolation` | [docs/evidence/stage-e/e-sandbox-001-cross-platform-wasm-process-isolation.md](e-sandbox-001-cross-platform-wasm-process-isolation.md) | `W80 / E6A` | `H4` | 已入库（`201` 基线内） |
| 11 | `e-market-001-reference-apps-and-drivers` | [docs/evidence/stage-e/e-market-001-reference-apps-and-drivers.md](e-market-001-reference-apps-and-drivers.md) | `W80 / E6B` | `H4` | 已入库（`201` 基线内） |
| 12 | `e-market-002-desktop-store-transparency-ui` | [docs/evidence/stage-e/e-market-002-desktop-store-transparency-ui.md](e-market-002-desktop-store-transparency-ui.md) | `W80 / E6C` | `H4` | 已入库（`201` 基线内） |
| 13 | `e-security-001-h7-supply-chain-and-revocation-adversarial` | [docs/evidence/stage-e/e-security-001-h7-supply-chain-and-revocation-adversarial.md](e-security-001-h7-supply-chain-and-revocation-adversarial.md) | `W80 / E6D` | `H7` | 已入库（`201` 基线内） |
| 14 | `e-road-001-zero-recompile-host-upgrade-gate` | [docs/evidence/stage-e/e-road-001-zero-recompile-host-upgrade-gate.md](e-road-001-zero-recompile-host-upgrade-gate.md) | `W81 / E7A` | `H7` | 已入库（`W81-INT` `+1 -> 202`） |
| 15 | `e-ledger-001-v13-conformance-audit` | [docs/evidence/stage-e/e-ledger-001-v13-conformance-audit.md](e-ledger-001-v13-conformance-audit.md) | `W81 / E7B` | `H4` | 已入库（`W81-INT` `+1 -> 203`） |
| 16 | `w81-stage-e-exit-gates` | [docs/evidence/stage-e/reviews/w81-stage-e-exit-gates.md](reviews/w81-stage-e-exit-gates.md) | `W81 / E7C` | `H7` | 已入库（`W81-INT` `+1 -> 204`） |

---

## 5. 验证门禁执行结果与复现命令

### 5.1 复现命令与实测结果

1. **未触碰共享台账基线校验（落盘 `e-ledger-001` 前）**：
   - `python scripts/lint_claims.py` -> `claims: 116 (DONE 6, IN_PROGRESS 9, NOT_STARTED 1, PARTIAL_PASS 100) · risks: 12 (P1/closed 1, P1/open 1, P1/partial 8, P2/partial 2) · evidence-index: 201/201` -> `lint: PASS（0 项 ERROR，0 项 INFO）`，退出码 `0`；
   - `python -m unittest scripts.test_lint_claims` -> `Ran 44 tests -> OK (skipped=1)`，退出码 `0`。
   - *并发写集边界说明*：本文件落盘至 `docs/evidence/stage-e/e-ledger-001-v13-conformance-audit.md` 后，因本车道严格禁止直接编辑共享 `docs/management/evidence-index.yaml`，未合流的孤立分支上 `evidence-index` 与磁盘计数呈预期的 `201/202`（唯一差异即本候选文件待 `W81-INT` 入索引）；`--mode pre-backfill` 在 `baseline.pending_w81_unindexed_on_disk` 中透明记录该单文件状态，并在模拟 `W81-INT` 合流树中验证三份 `W81` 新证据全量入库后 `lint_claims.py` 达成 **`204/204`、`0 ERROR`**。
2. **核验器内置 13 项正负向篡改自测套件**：
   ```bash
   python tools/verify_stage_e_ledger_coverage.py --self-test
   ```
   实测 `13/13` 全部 `OK`（退出码 `0`）：
   - `test_01_extract_stage_e_spec_counts_and_ids`：验证动态提取 `44 + 1 = 45` 项规范 Requirement ID、`8` 项工作包（合计 `53` 项）及 `RISK-E-01..05`；
   - `test_02_pre_backfill_and_simulated_strict_pass`：验证 `pre-backfill` 基线与模拟合流后 `strict` 全量通过（`169` claims、`17` risks、`204/204` evidence、`16/16` stage-e evidence、`0` errors）；
   - `test_03_emit_candidates_canonical_yaml_roundtrip`：验证 `--emit-candidates` 三段输出经 `lint_claims.parse_canonical_yaml` 无损解析（`3` evidence + `53` claims + `5` risks）；
   - `test_04_earlier_stage_preregistered_id_prevents_duplicate_emission`：验证前序阶段已注册 ID 自动归入 `registered_earlier_stage` 且零重复发射；
   - `test_05`..`test_12`（八类负向篡改拦截）：缺失阶段 E Requirement ID（`DRV-SHIM-001`）、重复 `requirement_id`（`COMPAT-VER-001`）、`Claim > Evidence` 越级声明（`H4` 证据伪称 `H8`）、阶段 E 必检项残留 `assurance: DESIGN`、注入未关闭 `P0` 风险、缺失 `RISK-E-04`、未入索引野生阶段 E 证据文件、以及注入象形 Emoji 与断链相对路径，100% 被精确捕获并拒绝；
   - `test_13_verifier_source_contains_zero_pictographic_emoji`：自检 Python 源码自身磁盘字节零象形 Emoji。
3. **`W81-E7B` 合流前模拟总门禁（`--mode pre-backfill`）**：
   ```bash
   python tools/verify_stage_e_ledger_coverage.py --mode pre-backfill
   python tools/verify_stage_e_ledger_coverage.py --mode pre-backfill --json
   ```
   实测输出摘要：
   ```text
   [stage-e-ledger:pre-backfill] PASS
     baseline : claims=116, risks=12 (open_P0=0), evidence=201/202 (pending_w81_unindexed=1)
     classify : normative_reqs=45 (44 E.1.1 + 1 E.2.2), work_packages=8, earlier_stage=0, to_backfill_claims=53, to_backfill_risks=5, to_backfill_evidence=3
     simulated: claims=169, risks=17 (open_P0=0), evidence=204/204 (stage-e=16/16), errors=0
   ```
4. **供 `W81-INT` 串行合流消费的候选导出与合流后硬门禁命令**：
   ```bash
   python tools/verify_stage_e_ledger_coverage.py --emit-candidates --section evidence
   python tools/verify_stage_e_ledger_coverage.py --emit-candidates --section claims
   python tools/verify_stage_e_ledger_coverage.py --emit-candidates --section risks
   # W81-INT 合入共享台账与三车道交付物后执行硬门禁：
   python scripts/lint_claims.py
   python tools/verify_stage_e_ledger_coverage.py --mode strict
   ```

---

## 6. 诚实非声明边界（Honest Non-Claims）

1. **`Claim <= Evidence` 支撑格铁律**：`53` 条阶段 E 候选声明中，仅直接引用 `H7` 级证据（[e-security-001](e-security-001-h7-supply-chain-and-revocation-adversarial.md)、`e-road-001` 或 `w81-stage-e-exit-gates`）的 `9` 条声明评定为 `assurance: H7`；其余仅由本地确定性单元/集成/CLI 证据支撑的 `44` 条声明一律保守评定为 `assurance: H4`，绝不借相邻车道 `H7` 证据越级拔高。
2. **`PARTIAL_PASS` 与 `E-deferred` 边界**：全部 `53` 条阶段 E 候选声明统一采用 `status: PARTIAL_PASS` 并携带显式 `SATISFIED-WITH-BOUNDARIES` `limitations`，如实反映法币支付清结算、公网多租户云市场同步、商业外部 CA / 物理硬件 HSM 密钥托管、跨主机物理断电故障台架（同 `C-POWER-LOSS`）及远端三平台 CI 常态守护边界（`NLOS-DEFER-001`）。
3. **严禁越权翻转 `SABI v1.0` 正式冻结或阶段 E `EXITED`**：本证据与核验器仅证明阶段 E 机器台账满足 `ledger-schema v1.3` 全量覆盖率、`P0 = 0` 与双向索引不变量；`ADR-0027` 晋升、`SABI v1.0` 正式冻结（`RISK-B-05` / `RISK-E-01`）以及阶段 E 顶层状态从实现/终审态翻转为 `EXITED`，必须由 `W81-INT` 汇总三车道证据后呈请维护者审阅批复。
