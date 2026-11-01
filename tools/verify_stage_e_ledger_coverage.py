#!/usr/bin/env python3
"""Stage E ledger-schema v1.3 mechanical coverage auditor & candidate backfill generator.

Zero third-party dependencies (Python 3 stdlib only); reuses scripts/lint_claims.py
for canonical YAML subset parsing, PyYAML equivalence (when installed), ref resolution,
Claim <= Evidence rank checks, and bidirectional evidence-index verification.

Modes:
  --mode strict (default):
      Hard exit gate for post-backfill repository (used by W81-INT and CI).
      Verifies:
        1. scripts/lint_claims.py passes with 0 ERROR;
        2. 100% of stage-e-progress.md section E.1.1 Requirement IDs (44, including
           ROAD-E-001) + section E.2.2 DUI-ASSOC-001 (45 normative Requirement IDs)
           + all 8 Stage E Work Package IDs (E-TRIAL-BOOTSTRAP..E-UPGRADE-GATE-ROADE001,
           53 Stage E claim IDs total) exist in claims.yaml with globally unique
           requirement_id and assurance != DESIGN (exempting intentionally deferred
           hardware bench claims such as C-POWER-LOSS in Stage C);
        3. RISK-E-01..05 (matching section E.5.2 categories & severities), RISK-B-05,
           and RISK-B-09 exist in risks.yaml, and non-closed P0 risk count == 0;
        4. All docs/evidence/stage-e/**/*.md files (>= 16, including e-road-001-*,
           e-ledger-001-*, and reviews/w81-stage-e-exit-gates.md) are bidirectionally
           indexed in evidence-index.yaml;
        5. Zero pictographic emoji and zero broken relative filesystem markdown links
           across docs/management/stage-e-progress.md, docs/management/adrs/0027-*.md,
           and docs/evidence/stage-e/**/*.md.

  --mode pre-backfill:
      Pre-integration verification gate for W81-E7B before W81-INT edits shared
      ledger files. Audits current repository baseline (allowing only pending W81
      candidate evidence files not yet merged into evidence-index.yaml), constructs
      an isolated TemporaryDirectory simulated post-W81-INT tree with generated
      candidate YAML blocks appended, and runs full --mode strict verification
      against the simulated tree.

  --emit-candidates [--section all|evidence|claims|risks]:
      Emits canonical YAML v1.3 candidate blocks for W81-INT integration.

  --self-test:
      Runs built-in unittest suite (>= 12 positive and negative tamper tests).
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import re
import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parent.parent

STAGE_E_PROGRESS_REL = "docs/management/stage-e-progress.md"
LEDGER_SCHEMA_REL = "docs/management/ledger-schema.md"
EXIT_UNKNOWN_RISKS_REL = "docs/management/exit-unknown-risks.md"
ADR_GLOB_REL = "docs/management/adrs/0027-*.md"
STAGE_E_EVIDENCE_DIR_REL = "docs/evidence/stage-e"

INTENTIONALLY_DEFERRED_DESIGN_CLAIMS = {"B-MGMT", "C-LIFECYCLE", "C-POWER-LOSS"}
CROSS_STAGE_REQUIRED_RISKS = ("RISK-B-05", "RISK-B-09")

W81_PENDING_EVIDENCE_PATHS = (
    "docs/evidence/stage-e/e-road-001-zero-recompile-host-upgrade-gate.md",
    "docs/evidence/stage-e/e-ledger-001-v13-conformance-audit.md",
    "docs/evidence/stage-e/reviews/w81-stage-e-exit-gates.md",
)

W81_PENDING_CODE_DIRS = (
    "tests/stage_e_road_e001_upgrade_gate",
)

W81_PENDING_CODE_FILES = (
    "scripts/run_road_e001_upgrade_matrix.py",
    "tools/verify_stage_e_ledger_coverage.py",
)

REQUIRED_STAGE_E_MIN_EVIDENCE_COUNT = 16

# Pictographic emoji code point ranges expressed via ASCII hex escapes so this
# source file itself contains zero pictographic emoji characters on disk.
PICTOGRAPHIC_EMOJI_RE = re.compile(
    "["
    "\U0001F1E6-\U0001F1FF"
    "\U0001F300-\U0001F5FF"
    "\U0001F600-\U0001F64F"
    "\U0001F680-\U0001F6FF"
    "\U0001F700-\U0001F77F"
    "\U0001F780-\U0001F7FF"
    "\U0001F800-\U0001F8FF"
    "\U0001F900-\U0001F9FF"
    "\U0001FA00-\U0001FA6F"
    "\U0001FA70-\U0001FAFF"
    "\u2600-\u26FF"
    "\u2700-\u27BF"
    "\uFE0F"
    "]"
)

MARKDOWN_LINK_RE = re.compile(r"!?\[[^\]]*\]\(([^)]+)\)")
NORMATIVE_REQ_TOKEN_RE = re.compile(r"`([A-Z]+-(?:[A-Z0-9]+-)*\d{3})`")
WORK_PACKAGE_TOKEN_RE = re.compile(r"`(E-[A-Z0-9-]+)`")


def load_lint_claims_module(root: Path | None = None):
    """Load scripts/lint_claims.py from root (if present) or REPO_ROOT."""
    candidates = []
    if root is not None:
        candidates.append(root / "scripts" / "lint_claims.py")
    candidates.append(REPO_ROOT / "scripts" / "lint_claims.py")
    for script_path in candidates:
        if script_path.is_file():
            spec = importlib.util.spec_from_file_location("nlos_lint_claims", script_path)
            if spec is None or spec.loader is None:
                raise RuntimeError(f"Unable to load module spec from {script_path}")
            mod = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(mod)
            return mod
    raise FileNotFoundError("scripts/lint_claims.py not found")


# Candidate W81 evidence-index.yaml entries (+3 items: 201 -> 204).
CANDIDATE_EVIDENCE_ENTRIES: tuple[dict[str, Any], ...] = (
    {
        "id": "e-road-001-zero-recompile-host-upgrade-gate",
        "path": "docs/evidence/stage-e/e-road-001-zero-recompile-host-upgrade-gate.md",
        "title": "E-ROAD-001：主系统 N-1 -> N 零重编译跨版本升级全链路门禁证据（W81-E7A）",
        "scope": "冻结签名 N-1 参考多组件应用与用户态驱动在主系统原地升级至 N 后零重编译/零重打包/零重签恢复、持久态读取、能力续期、IPC、nlos-compat-shim 垫片协商、驱动热插拔、故障回滚与零残留卸载端到端验证",
        "assurance": "H7",
        "date": "2026-11-01",
    },
    {
        "id": "e-ledger-001-v13-conformance-audit",
        "path": "docs/evidence/stage-e/e-ledger-001-v13-conformance-audit.md",
        "title": "E-LEDGER-001：阶段 E ledger-schema v1.3 台账覆盖率机械核验与合规审计证据（W81-E7B）",
        "scope": "tools/verify_stage_e_ledger_coverage.py 纯标准库机械核验器（13 项正负向篡改单测与 pre-backfill 模拟合流门禁）：覆盖阶段 E 全部 45 项规范 Requirement ID、8 项 E0..E7 工作包、RISK-E-01..05 风险、16 份阶段 E 证据双向索引、Claim<=Evidence 支撑格及零 Emoji/零断链审计",
        "assurance": "H4",
        "date": "2026-11-01",
    },
    {
        "id": "w81-stage-e-exit-gates",
        "path": "docs/evidence/stage-e/reviews/w81-stage-e-exit-gates.md",
        "title": "W81 阶段 E 退出门禁独立终审报告（W81-E7C）",
        "scope": "独立终审核查 v0.5 §28.5 四大交付物、ROAD-E-001 零重编译跨版本升级实证、E0+E1..E7 全部 16 份证据、ledger-schema v1.3 台账一致性、零 Emoji 与全仓断链扫描、P0=0 及 §E.5.5 六项退出门满足度",
        "assurance": "H7",
        "date": "2026-11-01",
    },
)


# Authoritative Stage E candidate claim metadata catalog covering all 45 normative
# Requirement IDs (44 from section E.1.1 + DUI-ASSOC-001 from section E.2.2) and
# all 8 Stage E Work Package canonical IDs (E0 + E1..E7 = 53 entries total).
STAGE_E_CLAIM_CATALOG: dict[str, dict[str, Any]] = {
    "ROAD-E-001": {
        "implementation_refs": [
            "crates/nlos-compat-shim",
            "crates/nlos-migrate",
            "crates/nlos-package",
            "crates/nlos-store-registry",
            "scripts/run_road_e001_upgrade_matrix.py",
        ],
        "test_refs": [
            "tests/stage_e_road_e001_upgrade_gate",
            "crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-road-001-zero-recompile-host-upgrade-gate.md",
            "docs/evidence/stage-e/e-market-001-reference-apps-and-drivers.md",
            "docs/evidence/stage-e/reviews/w81-stage-e-exit-gates.md",
        ],
        "assurance": "H7",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在本地确定性多纪元宿主矩阵上实证冻结签名 N-1（target_sabi_epoch=1）参考多组件应用与用户态驱动在主系统原地升级至 N 后零重编译/零重打包/零重签完成状态恢复、能力续期、IPC、nlos-compat-shim 垫片协商、驱动热插拔、回滚与零残留卸载；公网多租户云市场分发、商业法币计费及维护者签署 SABI v1.0 / EXITED 批复保持显式边界",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E7 / §E.5.5 ROAD-E-001（W81）：PARTIAL_PASS",
    },
    "COMPAT-VER-001": {
        "implementation_refs": [
            "schema/nlos/sabi/v1",
            "tools/sabi-compat-diff/sabi_compat_diff.py",
            "crates/nlos-migrate",
            "crates/nlos-certify/src/gate1_protocol.rs",
        ],
        "test_refs": [
            "tools/sabi-compat-diff/test_sabi_compat_diff.py",
            "tests/conformance/schema/test_sabi_compat_diff_gate.py",
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-sabi-001-version-matrix-and-deprecation.md",
            "docs/evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已冻结 SABI v1 N/N-1/N-2 三纪元兼容窗口与 APPLICATION_PLATFORM/DESKTOP SurfaceProfile 准入矩阵并由 Gate 1 与 sabi-compat-diff 机械守护；正式 SABI v1.0 里程碑签署需维护者终批（关联 RISK-E-01 / RISK-B-05）",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E1,E3,E5（W77-W79）：PARTIAL_PASS",
    },
    "COMPAT-VER-002": {
        "implementation_refs": [
            "schema/nlos/sabi/v1",
            "crates/nlos-migrate",
            "crates/nlos-compat-shim",
            "crates/nlos-certify/src/gate1_protocol.rs",
        ],
        "test_refs": [
            "crates/nlos-migrate/tests/static_checker.rs",
            "crates/nlos-compat-shim/tests/mount_and_protocol_translation.rs",
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-sabi-001-version-matrix-and-deprecation.md",
            "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md",
            "docs/evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已实证 SABI 线级扩展协商、可选字段安全降级与未知 critical 扩展 fail-closed 拒绝；跨大版本 SABI v2 破坏性迁移不在 v1 周期范围内",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E1,E2,E3（W77-W78）：PARTIAL_PASS",
    },
    "COMPAT-DEPRECATE-001": {
        "implementation_refs": [
            "schema/nlos/sabi/v1/deprecation-manifest.yaml",
            "tools/sabi-compat-diff/sabi_compat_diff.py",
            "crates/nlos-migrate",
        ],
        "test_refs": [
            "tools/sabi-compat-diff/test_sabi_compat_diff.py",
            "tests/conformance/schema/test_sabi_compat_diff_gate.py",
            "crates/nlos-migrate/tests/static_checker.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-sabi-001-version-matrix-and-deprecation.md",
            "docs/evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已通过 deprecation-manifest.yaml、sabi-compat-diff 与 nlos-migrate 升级处方强制执行不少于 2 个次版本/纪元的弃用缓冲窗口与移除阻断；仅覆盖本地确定性 CI 门禁，不含外部注册中心遥测",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E1,E3（W77-W78）：PARTIAL_PASS",
    },
    "KABI-IDEM-001": {
        "implementation_refs": [
            "crates/nlos-types",
            "crates/nlos-operation",
            "crates/nlos-compat-shim",
            "crates/nlos-certify/src/gate1_protocol.rs",
        ],
        "test_refs": [
            "crates/nlos-compat-shim/tests/mount_and_protocol_translation.rs",
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md",
            "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在 SABI 黄金语料、nlos-compat-shim 旧包翻译面与 Gate 1 认证审计中强制校验 128-bit IdempotencyKey 无损透传与同键重放幂等；外部不受控第三方 SaaS 远端幂等取决于对应驱动副作用级别声明",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E1,E2,E5（W77-W79）：PARTIAL_PASS",
    },
    "KABI-HANDLE-001": {
        "implementation_refs": [
            "crates/nlos-types",
            "crates/nlos-capability",
            "crates/nlos-compat-shim",
            "crates/nlos-plan/src/ecosystem_selector.rs",
        ],
        "test_refs": [
            "crates/nlos-compat-shim/tests/drv_shim_adversarial_isolation.rs",
            "crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md",
            "docs/evidence/stage-e/e-trust-001-publisher-pki-revocation-and-selectors.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在兼容垫片与 7 类跨生态 Typed Selector 解析路径强制执行不透明不可伪造句柄绑定，拒绝裸宿主路径或跨主体句柄伪造；确定性本地 H4 集成测试级别",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E2,E4（W78-W79）：PARTIAL_PASS",
    },
    "KABI-SVC-001": {
        "implementation_refs": [
            "crates/nlos-up",
            "crates/nlos-driver-llm-http",
            "examples/ecosystem-drivers/openai-compat-driver",
        ],
        "test_refs": [
            "crates/nlos-up/tests/bootstrap_smoke.rs",
            "tests/conformance/schema/test_sabi_compat_diff_gate.py",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-trial-001-llm-http-driver.md",
            "docs/evidence/stage-e/e-sabi-001-version-matrix-and-deprecation.md",
            "docs/evidence/stage-e/e-market-001-reference-apps-and-drivers.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：微内核保持零厂商 LLM API 与零应用市场框架侵入，nlos-up 试用栈与 OpenAI/MCP 生态驱动均严格运行于用户态 DriverProvider + EnforcementShim 边界之外；商业多供应商云路由属 E-deferred",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E0,E1,E6（W77-PRE..W80）：PARTIAL_PASS",
    },
    "KABI-ERR-001": {
        "implementation_refs": [
            "crates/nlos-types",
            "crates/nlos-compat-shim",
            "schema/nlos/sabi/v1/envelope.proto",
        ],
        "test_refs": [
            "tests/conformance/schema/test_golden_immutability.py",
            "crates/nlos-compat-shim/tests/mount_and_protocol_translation.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md",
            "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在 SABI v1 envelope 黄金线级向量与 nlos-compat-shim 跨纪元错误码映射中保持微内核标准错误分类零静默吞没、零敏感宿主路径泄漏；本地 H4 确定性验证级别",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E1,E2（W77-W78）：PARTIAL_PASS",
    },
    "KABI-UNCERTAIN-001": {
        "implementation_refs": [
            "crates/nlos-operation",
            "crates/nlos-compat-shim",
            "crates/nlos-migrate",
            "crates/nlos-certify/src/gate1_protocol.rs",
        ],
        "test_refs": [
            "crates/nlos-compat-shim/tests/mount_and_protocol_translation.rs",
            "crates/nlos-migrate/tests/static_checker.rs",
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md",
            "docs/evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在兼容垫片、nlos-migrate 静态审计、Gate 1 商店认证门与 mcp-bridge-driver 中强制保留 UNCERTAIN 终态并校验同幂等键对账契约；物理外部硬件设备在途擦除不在软件对账范围内",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E2,E3,E5（W78-W80）：PARTIAL_PASS",
    },
    "SABI-DISC-001": {
        "implementation_refs": [
            "crates/nlos-service-directory",
            "schema/nlos/sabi/v1/service_directory.proto",
            "crates/nlos-up",
        ],
        "test_refs": [
            "crates/nlos-up/tests/bootstrap_smoke.rs",
            "tests/conformance/schema/test_golden_immutability.py",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-trial-001-llm-http-driver.md",
            "docs/evidence/stage-e/e-sabi-001-version-matrix-and-deprecation.md",
            "docs/evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已冻结 ServiceDirectory v1 发现协议黄金向量并在本地试用栈与 SurfaceProfile 边界门禁完成确定性验证；跨公网联邦服务目录发现属 E-deferred",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E0,E1（W77-PRE..W77）：PARTIAL_PASS",
    },
    "SABI-COMMON-001": {
        "implementation_refs": [
            "schema/nlos/sabi/v1/envelope.proto",
            "crates/nlos-schema",
            "tools/check_golden_immutability.py",
        ],
        "test_refs": [
            "tests/conformance/schema/test_golden_immutability.py",
            "tests/conformance/schema/test_sabi_compat_diff_gate.py",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-sabi-001-version-matrix-and-deprecation.md",
            "docs/evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已冻结 SABI v1 公共信封（版本、追踪、相关 ID 与结构化错误头）并通过黄金线级不可变门禁校验；正式 SABI v1.0 签署保留维护者批复门控",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E1（W77）：PARTIAL_PASS",
    },
    "SABI-RECEIPT-001": {
        "implementation_refs": [
            "crates/nlos-up",
            "crates/nlos-certify/src/receipt.rs",
            "crates/nlos-store-registry",
        ],
        "test_refs": [
            "crates/nlos-up/tests/bootstrap_smoke.rs",
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-trial-001-llm-http-driver.md",
            "docs/evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已实证本地试用栈审计收据、SABI 黄金收据向量与域分离签名 StoreCertificationReceipt 全链路验签；外部第三方透明日志公证属 E-deferred",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E0,E1,E5（W77-PRE..W79）：PARTIAL_PASS",
    },
    "SABI-AUTH-001": {
        "implementation_refs": [
            "schema/nlos/sabi/v1/principal_handshake.proto",
            "crates/nlos-identity",
            "crates/nlos-up",
        ],
        "test_refs": [
            "crates/nlos-up/tests/bootstrap_smoke.rs",
            "tests/conformance/schema/test_golden_immutability.py",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-trial-001-llm-http-driver.md",
            "docs/evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已冻结 PrincipalHandshake v1 线级黄金向量并在 nlos-up 本地栈实证主体身份与能力握手绑定；企业外部 OIDC/SSO 联合身份目录属 E-deferred",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E0,E1（W77-PRE..W77）：PARTIAL_PASS",
    },
    "SABI-TRANSPORT-001": {
        "implementation_refs": [
            "crates/nlos-ipc",
            "crates/nlos-channel",
            "crates/nlos-compat-shim",
        ],
        "test_refs": [
            "tests/conformance/ipc",
            "crates/nlos-compat-shim/tests/mount_and_protocol_translation.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md",
            "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已实证传输无关 SABI 帧封装与 nlos-compat-shim 旧版本消息无损转译（零特权旁路）；跨数据中心广域网传输优化不在阶段 E 核心范围内",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E1,E2（W77-W78）：PARTIAL_PASS",
    },
    "SABI-CONFORM-001": {
        "implementation_refs": [
            "schema/nlos/sabi/v1",
            "tools/check_golden_immutability.py",
            "crates/nlos-certify/src/gate1_protocol.rs",
        ],
        "test_refs": [
            "tests/conformance/schema/test_golden_immutability.py",
            "tests/conformance/schema/test_sabi_compat_diff_gate.py",
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-sabi-001-version-matrix-and-deprecation.md",
            "docs/evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已交付 SABI 9/9 通道黄金线级语料不可变门禁与商店上架 Gate 1 自动化一致性认证；驱动包扩展副作用清单 sidecar 与 nlos-package 四键基础清单的语法分界已在 E-MARKET-001 如实记录",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E1,E5（W77,W79）：PARTIAL_PASS",
    },
    "DRV-NEGO-001": {
        "implementation_refs": [
            "schema/nlos/sabi/v1/driver_provider.proto",
            "crates/nlos-compat-shim",
            "examples/ecosystem-drivers/ecosystem-driver-kit",
        ],
        "test_refs": [
            "crates/nlos-compat-shim/tests/mount_and_protocol_translation.rs",
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
            "docs/evidence/stage-e/e-market-001-reference-apps-and-drivers.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在 nlos-compat-shim、Gate 1（SEMANTIC_SETTLEMENT->RESOURCE_ESCROW 依赖门控）及 MCP/OpenAI 参考驱动套件中实证协议版本交集协商与不交集 fail-closed 断连；外部商业网关实时在线联调属后续生态扩展",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E0,E2,E5,E6（W77-PRE..W80）：PARTIAL_PASS",
    },
    "DRV-BOUND-001": {
        "implementation_refs": [
            "crates/nlos-compat-shim",
            "crates/nlos-wasm-sandbox",
        ],
        "test_refs": [
            "crates/nlos-compat-shim/tests/drv_shim_adversarial_isolation.rs",
            "crates/nlos-wasm-sandbox/tests/escape_matrix.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md",
            "docs/evidence/stage-e/e-sandbox-001-cross-platform-wasm-process-isolation.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已实证用户态驱动与兼容垫片严格隔离于内核特权域之外，社区不可信扩展强制封入双层 Wasm 子进程沙箱；Windows 11 本机实测通过，Linux/macOS #[cfg(unix)] 分支由远端三平台 CI 守护",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E2,E6（W78,W80）：PARTIAL_PASS",
    },
    "DRV-ACTIVE-001": {
        "implementation_refs": [
            "crates/nlos-lease",
            "crates/nlos-compat-shim",
            "crates/nlos-application",
        ],
        "test_refs": [
            "crates/nlos-compat-shim/tests/mount_and_protocol_translation.rs",
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在驱动热替换、兼容垫片挂载切换与事务升级演练中强制代数/租约围栏防双活脑裂；本地确定性 H4 沙箱级别",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E2,E5（W78-W79）：PARTIAL_PASS",
    },
    "DRV-CRED-001": {
        "implementation_refs": [
            "crates/nlos-up",
            "crates/nlos-driver-llm-http",
            "crates/nlos-migrate",
            "crates/nlos-certify/src/gate2_supply_chain.rs",
        ],
        "test_refs": [
            "crates/nlos-up/tests/bootstrap_smoke.rs",
            "crates/nlos-migrate/tests/static_checker.rs",
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-trial-001-llm-http-driver.md",
            "docs/evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已实证驱动凭据仅经受控句柄注入、nlos-migrate 与 Gate 2 硬编码密钥扫描强制脱敏阻断且日志零明文回显；硬件 TPM/HSM 外部密钥托管属 E-deferred",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E0,E3,E5（W77-PRE..W79）：PARTIAL_PASS",
    },
    "DRV-PROTO-001": {
        "implementation_refs": [
            "schema/nlos/sabi/v1/driver_provider.proto",
            "examples/ecosystem-drivers/mcp-bridge-driver",
            "examples/ecosystem-drivers/openai-compat-driver",
        ],
        "test_refs": [
            "tests/conformance/schema/test_golden_immutability.py",
            "crates/nlos-wasm-sandbox/tests/escape_matrix.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md",
            "docs/evidence/stage-e/e-sandbox-001-cross-platform-wasm-process-isolation.md",
            "docs/evidence/stage-e/e-market-001-reference-apps-and-drivers.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在冻结 DriverProvider v1 通道之上通过版本化扩展封装交付 MCP（JSON-RPC 2.0 stdio）与 OpenAI-compatible（同步+SSE 流式）两款解耦用户态驱动（31/31 测试）；以脚本化传输 double 验证协议语义，未依赖外部商业在线网关",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E0,E1,E6（W77,W80）：PARTIAL_PASS",
    },
    "DRV-LIFE-001": {
        "implementation_refs": [
            "crates/nlos-compat-shim",
            "crates/nlos-wasm-sandbox",
            "examples/ecosystem-drivers/ecosystem-driver-kit",
        ],
        "test_refs": [
            "crates/nlos-compat-shim/tests/mount_and_protocol_translation.rs",
            "crates/nlos-wasm-sandbox/tests/escape_matrix.rs",
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
            "docs/evidence/stage-e/e-sandbox-001-cross-platform-wasm-process-isolation.md",
            "docs/evidence/stage-e/e-market-001-reference-apps-and-drivers.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已实证驱动协作退出/强杀终态分类（GuestExited/TrapKind）、四类 EffectDescriptor 声明校验及崩溃转 UNCERTAIN 后的同幂等键对账恢复；本地 H4 确定性宿主验证级别",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E0,E2,E5,E6（W78-W80）：PARTIAL_PASS",
    },
    "DRV-SHIM-001": {
        "implementation_refs": [
            "crates/nlos-compat-shim/src/lib.rs",
        ],
        "test_refs": [
            "crates/nlos-compat-shim/tests/mount_and_protocol_translation.rs",
            "crates/nlos-compat-shim/tests/drv_shim_adversarial_isolation.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已交付零 unsafe 纯用户态无特权兼容垫片 crates/nlos-compat-shim，覆盖 N-1/N-2 旧包消息翻译、驱动适配与对抗性越权阻断回归；后续新增跨代垫片规则仍须按 RISK-E-02 复审",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E2（W78）：PARTIAL_PASS",
    },
    "DRV-DISC-001": {
        "implementation_refs": [
            "crates/nlos-plan/src/ecosystem_selector.rs",
            "crates/nlos-service-directory",
        ],
        "test_refs": [
            "crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-trust-001-publisher-pki-revocation-and-selectors.md",
            "docs/evidence/stage-e/e-market-001-reference-apps-and-drivers.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在 nlos-plan 落地全部 7 类跨生态 Typed Selector 不可变句柄发现与 G3 WaitingAuthorization->Materializing 四门条件物化门控（正式闭合 U-7/U-8）；跨组织远程目录联邦发现属 E-deferred",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E4,E6（W79-W80）：PARTIAL_PASS",
    },
    "PKG-SEP-001": {
        "implementation_refs": [
            "crates/nlos-package",
            "crates/nlos-application",
            "crates/nlos-store-registry",
        ],
        "test_refs": [
            "crates/nlos-store-registry/tests/store_registry_gate.rs",
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-store-001-oci-index-and-entitlement-ledger.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
            "docs/evidence/stage-e/e-market-001-reference-apps-and-drivers.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在 OCI CAS 商店布局、Gate 3/4 沙箱与参考多组件应用中严格隔离只读代码镜像、UserDataContainer 与私有缓存目录；本地文件系统确定性沙箱验证级别",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E5,E6（W79-W80）：PARTIAL_PASS",
    },
    "PKG-IMM-001": {
        "implementation_refs": [
            "crates/nlos-package",
            "crates/nlos-migrate",
            "crates/nlos-certify/src/gate2_supply_chain.rs",
        ],
        "test_refs": [
            "crates/nlos-migrate/tests/static_checker.rs",
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-trial-002-quickstart-and-starter-app.md",
            "docs/evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在 nlos-migrate 与 Gate 2 包纯度扫描中强制只读内容寻址载荷校验并拦截混入的可变 SQLite/WAL 二进制头；不依赖 OS 内核级 dm-verity 块设备挂载",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E0,E3,E5（W77-PRE..W79）：PARTIAL_PASS",
    },
    "PKG-SEED-001": {
        "implementation_refs": [
            "crates/nlos-package",
            "crates/nlos-migrate",
            "examples/ecosystem-apps/knowledge-observatory-app",
        ],
        "test_refs": [
            "crates/nlos-migrate/tests/migration_sandbox.rs",
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
            "docs/evidence/stage-e/e-market-001-reference-apps-and-drivers.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在 starter-assistant-app 与两款 6-entry 生态参考应用中实证显式只读 seed 槽位物化至 UserDataContainer 且零反向污染封存包体；确定性本地 H4 验证级别",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E3,E5,E6（W78-W80）：PARTIAL_PASS",
    },
    "PKG-UPDATE-001": {
        "implementation_refs": [
            "crates/nlos-migrate",
            "crates/nlos-certify/src/gate3_update_rollback.rs",
            "crates/nlos-application",
        ],
        "test_refs": [
            "crates/nlos-migrate/tests/migration_sandbox.rs",
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
            "crates/nlos-certify/tests/h7_supply_chain_adversarial.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
            "docs/evidence/stage-e/e-security-001-h7-supply-chain-and-revocation-adversarial.md",
        ],
        "assurance": "H7",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在 Gate 3 与 H7 恶意升级崩溃对抗矩阵中实证健康探针失败与半途崩溃后 UserDataContainer SHA-256 树摘要逐位原子回滚及漂移注入捕获，并对 IRREVERSIBLE 自动升级默认阻断；真实硬件断电故障台架与 C-POWER-LOSS 同口径保持显式边界",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E3,E5,E6（W78-W80）：PARTIAL_PASS",
    },
    "PKG-UNINSTALL-001": {
        "implementation_refs": [
            "crates/nlos-application",
            "crates/nlos-certify/src/gate4_uninstall_audit.rs",
        ],
        "test_refs": [
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
            "crates/nlos-certify/tests/h7_supply_chain_adversarial.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
            "docs/evidence/stage-e/e-security-001-h7-supply-chain-and-revocation-adversarial.md",
        ],
        "assurance": "H7",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在 Gate 4 与 H7 卸载残留对抗测试中实证活跃后台任务门控、能力即时吊销、零私有缓存残留、共享导出 Artifact 完好可读及默认 100% 保留 UserDataContainer（仅签名 Z4 收据可清除）；本地确定性对抗回归级别",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E5,E6（W79-W80）：PARTIAL_PASS",
    },
    "PKG-OCI-001": {
        "implementation_refs": [
            "crates/nlos-store-registry/src/lib.rs",
            "desktop/src-tauri/src/store_catalog.rs",
        ],
        "test_refs": [
            "crates/nlos-store-registry/tests/store_registry_gate.rs",
            "crates/nlos-certify/tests/h7_supply_chain_adversarial.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-store-001-oci-index-and-entitlement-ledger.md",
            "docs/evidence/stage-e/e-market-002-desktop-store-transparency-ui.md",
            "docs/evidence/stage-e/e-security-001-h7-supply-chain-and-revocation-adversarial.md",
        ],
        "assurance": "H7",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已落地 LocalOciCasStore 双重 SHA-256 与字节长度校验、SQLite WAL+FULL 商店治理目录物理解耦及 H7 OCI 层篡改/截断/缺失摘要 100% 拦截；公网分布式 CDN 镜像同步属 E-deferred",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E5,E6（W79-W80）：PARTIAL_PASS",
    },
    "PKG-MULTI-001": {
        "implementation_refs": [
            "crates/nlos-store-registry",
            "examples/starter-assistant-app",
            "examples/ecosystem-apps/knowledge-observatory-app",
            "examples/ecosystem-apps/field-survey-app",
        ],
        "test_refs": [
            "crates/nlos-store-registry/tests/store_registry_gate.rs",
            "crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-trial-002-quickstart-and-starter-app.md",
            "docs/evidence/stage-e/e-store-001-oci-index-and-entitlement-ledger.md",
            "docs/evidence/stage-e/e-market-001-reference-apps-and-drivers.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在商店五类槽位复合包索引与两款 6-entry 多组件参考应用（executable + background-service + driver + UI surface + 2 read-only seeds）完成端到端封存与安装升级实证；本地 H4 验证级别",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E0,E5,E6（W77-PRE..W80）：PARTIAL_PASS",
    },
    "PKG-APPID-001": {
        "implementation_refs": [
            "crates/nlos-publisher-trust/src/lib.rs",
            "crates/nlos-store-registry/src/lib.rs",
            "crates/nlos-certify/src/receipt.rs",
        ],
        "test_refs": [
            "crates/nlos-publisher-trust/tests/publisher_trust_gate.rs",
            "crates/nlos-certify/tests/h7_supply_chain_adversarial.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-trust-001-publisher-pki-revocation-and-selectors.md",
            "docs/evidence/stage-e/e-security-001-h7-supply-chain-and-revocation-adversarial.md",
        ],
        "assurance": "H7",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在信任权威、商店上架门与认证收据签发门三处联合强制 (PublisherId, ApplicationId) 首次注册归属锁定、三方联合签名迁移及 H7 跨发布者抢注 100% 拦截；外部法律商标仲裁流程属 E-deferred",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E4,E6（W79-W80）：PARTIAL_PASS",
    },
    "DUI-ASSOC-001": {
        "implementation_refs": [
            "crates/nlos-certify/src/gate2_supply_chain.rs",
        ],
        "test_refs": [
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：Gate 2 自动化认证流水线强制拦截任何静默篡改默认 URI/MIME/文件类型关联而不经用户显式授权的上架包声明；宿主 OS 级文件关联注册同步不在阶段 E 商店离线流水线范围内",
        ],
        "source": "stage-e-progress.md §E.2.2 E5（W79）：PARTIAL_PASS",
    },
    "TCB-BYPASS-001": {
        "implementation_refs": [
            "crates/nlos-wasm-sandbox/src/lib.rs",
            "crates/nlos-compat-shim/src/lib.rs",
        ],
        "test_refs": [
            "crates/nlos-wasm-sandbox/tests/escape_matrix.rs",
            "crates/nlos-compat-shim/tests/drv_shim_adversarial_isolation.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md",
            "docs/evidence/stage-e/e-sandbox-001-cross-platform-wasm-process-isolation.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：nlos-wasm-sandbox 将引擎执行面收敛为 pub(crate) 且生产路径仅经独立空环境子进程 nlos-wasm-sandbox-host 承载，nlos-compat-shim 零特权运行于 EnforcementShim 之外；写授信树内极端符号链接 TOCTOU 窗口与 Linux/macOS CI 分支在 E-SANDBOX-001 如实标注",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E2,E6（W78,W80）：PARTIAL_PASS",
    },
    "TCB-EGRESS-001": {
        "implementation_refs": [
            "crates/nlos-up",
            "crates/nlos-certify/src/gate2_supply_chain.rs",
            "crates/nlos-wasm-sandbox",
        ],
        "test_refs": [
            "crates/nlos-up/tests/bootstrap_smoke.rs",
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
            "crates/nlos-wasm-sandbox/tests/escape_matrix.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-trial-001-llm-http-driver.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
            "docs/evidence/stage-e/e-sandbox-001-cross-platform-wasm-process-isolation.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已实证 nlos-up 默认仅绑定 127.0.0.1、Gate 2 拦截 platform-core/TCB 组件违规请求 net.egress、以及 Wasm 强沙箱零 socket 导入注册（导入 sock_open 实例化即拒绝）；宿主 OS 防火墙级全局流量过滤不在本层范围内",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E0,E5,E6（W77-PRE..W80）：PARTIAL_PASS",
    },
    "TCB-DOWNGRADE-001": {
        "implementation_refs": [
            "crates/nlos-store-registry",
            "crates/nlos-wasm-sandbox",
            "desktop/src-tauri/src/store_catalog.rs",
            "desktop/src/views/store-catalog",
        ],
        "test_refs": [
            "crates/nlos-store-registry/tests/store_registry_gate.rs",
            "desktop/src-tauri/tests/store_catalog_trust_side.rs",
            "desktop/src/views/store-catalog/__tests__/store-catalog-transparency.test.ts",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-store-001-oci-index-and-entitlement-ledger.md",
            "docs/evidence/stage-e/e-sandbox-001-cross-platform-wasm-process-isolation.md",
            "docs/evidence/stage-e/e-market-002-desktop-store-transparency-ui.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在商店目录差异引擎与桌面透明面板中强制拦截沙箱降档与跨版本权限/不可逆副作用提权，必须经证书链在线复验通过并触发 Z4 显式重授权；确定性组件与桥接集成测试级别（不含人工物理窗口点击录制）",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E4,E5,E6（W79-W80）：PARTIAL_PASS",
    },
    "CAP-UNFORGE-001": {
        "implementation_refs": [
            "crates/nlos-capability",
            "crates/nlos-publisher-trust",
            "crates/nlos-certify/src/receipt.rs",
        ],
        "test_refs": [
            "crates/nlos-publisher-trust/tests/publisher_trust_gate.rs",
            "crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-trust-001-publisher-pki-revocation-and-selectors.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在三级发布者 PKI、StoreCertificationReceipt 与 SignedEntitlementToken 中采用显式域分离 Ed25519 签名与 SigningKeyCustodyVault（OperatorFileSealed KEK）密钥托管；物理硬件 HSM 托管属 E-deferred",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E4,E5（W79）：PARTIAL_PASS",
    },
    "CAP-ATTEN-001": {
        "implementation_refs": [
            "crates/nlos-capability",
            "crates/nlos-compat-shim",
            "crates/nlos-plan/src/ecosystem_selector.rs",
            "crates/nlos-wasm-sandbox",
        ],
        "test_refs": [
            "crates/nlos-compat-shim/tests/drv_shim_adversarial_isolation.rs",
            "crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs",
            "crates/nlos-wasm-sandbox/tests/escape_matrix.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md",
            "docs/evidence/stage-e/e-trust-001-publisher-pki-revocation-and-selectors.md",
            "docs/evidence/stage-e/e-sandbox-001-cross-platform-wasm-process-isolation.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已在兼容垫片权限不可放大测试、G3 跨生态选择器条件物化门与 WASI 预打开只读/路径围栏中实证能力单调衰减；确定性本地 H4 测试级别",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E2,E4,E6（W78-W80）：PARTIAL_PASS",
    },
    "CAP-REVOKE-001": {
        "implementation_refs": [
            "crates/nlos-publisher-trust",
            "crates/nlos-store-registry",
            "crates/nlos-certify/src/gate4_uninstall_audit.rs",
            "desktop/src-tauri/src/store_catalog.rs",
        ],
        "test_refs": [
            "crates/nlos-publisher-trust/tests/publisher_trust_gate.rs",
            "crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs",
            "crates/nlos-certify/tests/h7_supply_chain_adversarial.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-trust-001-publisher-pki-revocation-and-selectors.md",
            "docs/evidence/stage-e/e-store-001-oci-index-and-entitlement-ledger.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
            "docs/evidence/stage-e/e-security-001-h7-supply-chain-and-revocation-adversarial.md",
        ],
        "assurance": "H7",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已实证单调 RevocationEpoch（含 SQLite RAISE(ABORT) 防回滚/防反吊销触发器）、Level 3 QuarantineInstallation 级联能力吊销与应用禁用、以及卸载即时能力吊销 H7 对抗回归；持有宿主文件系统根写权限整库替换旧 DB 副本的防御依赖上层纪元水位对账",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E4,E5,E6（W79-W80）：PARTIAL_PASS",
    },
    "TUI-CONSENT-001": {
        "implementation_refs": [
            "desktop/src-tauri/src/store_catalog.rs",
            "desktop/src/views/store-catalog",
            "desktop/src/trusted-ui",
        ],
        "test_refs": [
            "desktop/src-tauri/tests/store_catalog_trust_side.rs",
            "desktop/src/views/store-catalog/__tests__/store-catalog-transparency.test.ts",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-trial-001-llm-http-driver.md",
            "docs/evidence/stage-e/e-market-002-desktop-store-transparency-ui.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：桌面商店敏感动作（INSTALL / IRREVERSIBLE_UPGRADE / DOWNGRADE_SANDBOX）强制在线复验发布者证书链并 100% 投影至既有 Z4 实心确认屏（零自绘确认弹窗、零行话、零 Emoji）；确定性桥接与前端 1933 断言组件测试级别",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E0,E5,E6（W77-PRE..W80）：PARTIAL_PASS",
    },
    "NLOS-DEFER-001": {
        "implementation_refs": [
            "docs/management/stage-e-progress.md",
            "docs/management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md",
            "crates/nlos-store-registry",
        ],
        "test_refs": [
            "crates/nlos-store-registry/tests/store_registry_gate.rs",
            "tools/verify_stage_e_ledger_coverage.py",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-store-001-oci-index-and-entitlement-ledger.md",
            "docs/evidence/stage-e/e-ledger-001-v13-conformance-audit.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已严格守住 E-core 与 E-deferred 边界——仅实现不透明 SignedEntitlementToken 离线验签与幂等席位核销槽位，显式延后法币支付清结算、公网多租户云市场运营、商业硬件 HSM 公证 CA 与自动 ABI 猜测生成",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.3（W76-W81）：PARTIAL_PASS",
    },
    "EVID-SCOPE-001": {
        "implementation_refs": [
            "scripts/lint_claims.py",
            "tools/verify_stage_e_ledger_coverage.py",
            "docs/management/ledger-schema.md",
        ],
        "test_refs": [
            "scripts/test_lint_claims.py",
            "tools/verify_stage_e_ledger_coverage.py",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-ledger-001-v13-conformance-audit.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：已由 lint_claims.py（44/44 单测）与 verify_stage_e_ledger_coverage.py（13/13 正负向单测）机械强制阶段 E 全量条目满足 rank(claim.assurance) <= max(rank(evidence_refs)) 且无越级 DESIGN/EXITED 冒充",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.1 / §E.5.5（W76,W81）：PARTIAL_PASS",
    },
    "EVID-NONLINEAR-001": {
        "implementation_refs": [
            "docs/management/stage-e-progress.md",
            "tools/verify_stage_e_ledger_coverage.py",
        ],
        "test_refs": [
            "scripts/test_lint_claims.py",
            "tools/verify_stage_e_ledger_coverage.py",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-ledger-001-v13-conformance-audit.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：阶段 E 全程（W76..W81）严格执行独立 Git Worktree 互斥写集、CANDIDATE->VERIFIED 波屏障由单一 Integrator 晋升及双向证据索引核验；后续阶段演化继续沿用同一机器台账契约",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.3.2（W76-W81）：PARTIAL_PASS",
    },
    "EVID-NEG-001": {
        "implementation_refs": [
            "crates/nlos-wasm-sandbox",
            "crates/nlos-compat-shim",
            "crates/nlos-certify",
        ],
        "test_refs": [
            "crates/nlos-wasm-sandbox/tests/escape_matrix.rs",
            "crates/nlos-compat-shim/tests/drv_shim_adversarial_isolation.rs",
            "crates/nlos-certify/tests/h7_supply_chain_adversarial.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md",
            "docs/evidence/stage-e/e-sandbox-001-cross-platform-wasm-process-isolation.md",
            "docs/evidence/stage-e/e-security-001-h7-supply-chain-and-revocation-adversarial.md",
        ],
        "assurance": "H7",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：阶段 E 兼容垫片、双层 Wasm 强沙箱 21 类逃逸矩阵、供应链 H7 六类攻击矩阵及台账篡改自测均强制断言具体错误变体/errno/触发器消息（零裸 is_err()）；非无界模糊渗透或外部红队测试",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E2,E6（W78,W80-W81）：PARTIAL_PASS",
    },
    "EVID-REPRO-001": {
        "implementation_refs": [
            "crates/nlos-up",
            "examples/ecosystem-apps/seal-n1-baseline.sh",
            "tools/verify_stage_e_ledger_coverage.py",
        ],
        "test_refs": [
            "crates/nlos-up/tests/bootstrap_smoke.rs",
            "tools/verify_stage_e_ledger_coverage.py",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-trial-001-llm-http-driver.md",
            "docs/evidence/stage-e/e-market-001-reference-apps-and-drivers.md",
            "docs/evidence/stage-e/e-ledger-001-v13-conformance-audit.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：每份阶段 E 证据均附确定性单命令复现入口（含 nlos-up 一键本地栈、seal-n1-baseline.sh 连续双跑 16/16 封存产物 SHA-256 逐字节一致及台账覆盖率核验器）；跨三平台远端 CI 构建缓存耗时不计入确定性基准",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E0,E6,E7（W77-PRE..W81）：PARTIAL_PASS",
    },
    "EVID-GEN-001": {
        "implementation_refs": [
            "tools/check_golden_immutability.py",
            "tools/check-generated-schema.mjs",
            "tools/sabi-compat-diff/sabi_compat_diff.py",
        ],
        "test_refs": [
            "tests/conformance/schema/test_golden_immutability.py",
            "tests/conformance/schema/test_sabi_compat_diff_gate.py",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-sabi-001-version-matrix-and-deprecation.md",
            "docs/evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：SABI v1 生成 schema、冻结黄金线级向量与锁文件供应链均纳入 tests/conformance/schema 常设自动化门禁防漂移守护；仅覆盖仓库内确定性离线工具链",
        ],
        "source": "stage-e-progress.md §E.1.1 / §E.2.2 E1（W77-W79）：PARTIAL_PASS",
    },
    "E-TRIAL-BOOTSTRAP": {
        "implementation_refs": [
            "crates/nlos-up",
            "crates/nlos-driver-llm-http",
            "docs/developers/quickstart-trial.md",
            "examples/starter-assistant-app",
        ],
        "test_refs": [
            "crates/nlos-up/tests/bootstrap_smoke.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-trial-001-llm-http-driver.md",
            "docs/evidence/stage-e/e-trial-002-quickstart-and-starter-app.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：工作包 E0（W77-PRE / M-E0.5）已交付一键本地试用栈 crates/nlos-up、OpenAI 兼容真实大模型 HTTP 驱动（含 glm-keypool 接管 smoke）、15 分钟开发者快速上手指南与独立 workspace starter-assistant-app（v1.0.0/v1.1.0）；微内核保持零厂商 API 侵入",
        ],
        "source": "stage-e-progress.md §E.2.2 E0（W77-PRE）：PARTIAL_PASS",
    },
    "E-SABI-POLICY-CORPUS": {
        "implementation_refs": [
            "schema/nlos/sabi/v1",
            "tools/check_golden_immutability.py",
            "docs/management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md",
        ],
        "test_refs": [
            "tests/conformance/schema/test_golden_immutability.py",
            "tests/conformance/schema/test_sabi_compat_diff_gate.py",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-sabi-001-version-matrix-and-deprecation.md",
            "docs/evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：工作包 E1（W77 / M-E1）已补齐 SABI v1 9/9 通道 schema、三纪元版本演进矩阵、deprecation-manifest.yaml 与冻结黄金线级语料不可变门禁；正式 SABI v1.0 冻结签署待维护者终批（关联 RISK-E-01 / RISK-B-05）",
        ],
        "source": "stage-e-progress.md §E.2.2 E1（W77）：PARTIAL_PASS",
    },
    "E-COMPAT-SHIM": {
        "implementation_refs": [
            "crates/nlos-compat-shim",
        ],
        "test_refs": [
            "crates/nlos-compat-shim/tests/mount_and_protocol_translation.rs",
            "crates/nlos-compat-shim/tests/drv_shim_adversarial_isolation.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：工作包 E2（W78 / M-E2 左半）已交付 #![forbid(unsafe_code)] 纯用户态无特权兼容垫片 crates/nlos-compat-shim，实证 N-1/N-2 协议与驱动消息翻译、幂等键与 UNCERTAIN 无损透传及对抗性越权阻断；后续新纪元翻译规则按 RISK-E-02 持续复核",
        ],
        "source": "stage-e-progress.md §E.2.2 E2（W78）：PARTIAL_PASS",
    },
    "E-MIGRATE-KIT": {
        "implementation_refs": [
            "crates/nlos-migrate",
            "tools/sabi-compat-diff",
        ],
        "test_refs": [
            "crates/nlos-migrate/tests/static_checker.rs",
            "crates/nlos-migrate/tests/migration_sandbox.rs",
            "tools/sabi-compat-diff/test_sabi_compat_diff.py",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：工作包 E3（W78 / M-E2 右半）已交付 nlos-migrate CLI/库（静态清单兼容性检查、包纯度与密钥脱敏扫描、UpgradePrescription 生成及三态迁移原子回滚沙箱）与纯标准库 tools/sabi-compat-diff；严禁任何未经声明的启发式 ABI 猜测生成",
        ],
        "source": "stage-e-progress.md §E.2.2 E3（W78）：PARTIAL_PASS",
    },
    "E-PUBLISHER-TRUST": {
        "implementation_refs": [
            "crates/nlos-publisher-trust",
            "crates/nlos-plan/src/ecosystem_selector.rs",
        ],
        "test_refs": [
            "crates/nlos-publisher-trust/tests/publisher_trust_gate.rs",
            "crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-trust-001-publisher-pki-revocation-and-selectors.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：工作包 E4（W79 / M-E3 左车道）已交付三级发布者 Ed25519 PKI（SigningKeyCustodyVault KEK 托管与双态密钥轮换）、SQLite 防回滚单调 RevocationEpoch、ApplicationId 首次注册防抢注与 nlos-plan 全部 7 类跨生态 Typed Selector + G3 四门条件物化（闭合 U-7/U-8）；商业硬件 HSM 公证 CA 属 E-deferred",
        ],
        "source": "stage-e-progress.md §E.2.2 E4（W79）：PARTIAL_PASS",
    },
    "E-STORE-CERTIFY": {
        "implementation_refs": [
            "crates/nlos-store-registry",
            "crates/nlos-certify",
            "tests/conformance/schema/test_lockfile_supply_chain_gate.py",
        ],
        "test_refs": [
            "crates/nlos-store-registry/tests/store_registry_gate.rs",
            "crates/nlos-certify/tests/certify_pipeline_gate.rs",
            "crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-store-001-oci-index-and-entitlement-ledger.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
        ],
        "assurance": "H4",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：工作包 E5（W79 / M-E3 中右车道）已交付 LocalOciCasStore 五槽位商店治理目录、SignedEntitlementToken 幂等核销、三级撤回级联吊销及 nlos-certify 四门自动化认证流水线（含纯 Rust 锁文件哈希/SPDX 供应链审计推进 RISK-B-09 open->partial）；法币支付清结算与公网多租户商店运营属 E-deferred",
        ],
        "source": "stage-e-progress.md §E.2.2 E5（W79）：PARTIAL_PASS",
    },
    "E-MARKET-SANDBOX-H7": {
        "implementation_refs": [
            "crates/nlos-wasm-sandbox",
            "examples/ecosystem-apps",
            "examples/ecosystem-drivers",
            "desktop/src-tauri/src/store_catalog.rs",
            "desktop/src/views/store-catalog",
        ],
        "test_refs": [
            "crates/nlos-wasm-sandbox/tests/escape_matrix.rs",
            "desktop/src-tauri/tests/store_catalog_trust_side.rs",
            "desktop/src/views/store-catalog/__tests__/store-catalog-transparency.test.ts",
            "crates/nlos-certify/tests/h7_supply_chain_adversarial.rs",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-sandbox-001-cross-platform-wasm-process-isolation.md",
            "docs/evidence/stage-e/e-market-001-reference-apps-and-drivers.md",
            "docs/evidence/stage-e/e-market-002-desktop-store-transparency-ui.md",
            "docs/evidence/stage-e/e-security-001-h7-supply-chain-and-revocation-adversarial.md",
        ],
        "assurance": "H7",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：工作包 E6（W80 / M-E4）已交付双层不可信 Wasm 强沙箱（54/54 测试，闭合 U-9）、2 款参考多组件应用 + 2 款用户态驱动及 16/16 字节确定性封存 N-1 基准包、桌面实心商店 L1/L2/L3 透明面板与 Z4 联动（1933 断言）及 H7 供应链/吊销六类攻击对抗回归；Linux/macOS #[cfg(unix)] 沙箱分支由远端三平台 CI 常态守护",
        ],
        "source": "stage-e-progress.md §E.2.2 E6（W80）：PARTIAL_PASS",
    },
    "E-UPGRADE-GATE-ROADE001": {
        "implementation_refs": [
            "scripts/run_road_e001_upgrade_matrix.py",
            "tools/verify_stage_e_ledger_coverage.py",
        ],
        "test_refs": [
            "tests/stage_e_road_e001_upgrade_gate",
            "tools/verify_stage_e_ledger_coverage.py",
        ],
        "evidence_refs": [
            "docs/evidence/stage-e/e-road-001-zero-recompile-host-upgrade-gate.md",
            "docs/evidence/stage-e/e-ledger-001-v13-conformance-audit.md",
            "docs/evidence/stage-e/reviews/w81-stage-e-exit-gates.md",
        ],
        "assurance": "H7",
        "limitations": [
            "SATISFIED-WITH-BOUNDARIES：工作包 E7（W81 / M-E5）已交付 ROAD-E-001 主系统 N-1->N 零重编译升级全链路门禁、ledger-schema v1.3 机械覆盖率核验器及阶段 E 独立退出终审；阶段 E 顶层状态仍需维护者审阅全部退出门证据后正式批复 EXITED",
        ],
        "source": "stage-e-progress.md §E.2.2 E7 / §E.3.2 W81（W81）：PARTIAL_PASS",
    },
}


# Authoritative Stage E candidate risks.yaml metadata catalog (RISK-E-01..05).
STAGE_E_RISK_CATALOG: dict[str, dict[str, Any]] = {
    "RISK-E-01": {
        "category": "premature-format-freeze",
        "severity": "P1",
        "status": "partial",
        "description": "在兼容性边界与演进规则未充分暴露前过早宣称 SABI v1.0 冻结，导致后续小版本被迫破坏契约（关联 RISK-B-05）",
        "mitigation": "已通过 E1 SABI 9/9 通道黄金线级语料不可变门禁（check_golden_immutability.py）、E3 sabi-compat-diff 破坏性变更阻断器、E6 四款解耦生态样板与 E7 ROAD-E-001 N-1->N 零重编译升级矩阵完成多工作负载实证；正式 SABI v1.0 冻结签署仍保留维护者终批门控",
        "evidence_refs": [
            "docs/evidence/stage-e/e-sabi-001-version-matrix-and-deprecation.md",
            "docs/evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md",
            "docs/evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md",
            "docs/evidence/stage-e/e-road-001-zero-recompile-host-upgrade-gate.md",
        ],
        "review_point": "阶段 E 退出呈批维护者签署 SABI v1.0 正式冻结及后续每次 SABI 次版本演进 PR 时复核",
    },
    "RISK-E-02": {
        "category": "security-bypass",
        "severity": "P1",
        "status": "partial",
        "description": "旧协议兼容垫片（nlos-compat-shim）在翻译旧包请求时发生权限放大、丢弃幂等键或越权绕过权威中介",
        "mitigation": "nlos-compat-shim 以 #![forbid(unsafe_code)] 纯用户态零特权组件运行在 EnforcementShim 之外，所有翻译输出强制重经身份/能力/策略权威中介并无损保留 128-bit IdempotencyKey 与 UNCERTAIN 状态，已通过对抗性越权隔离测试与 ROAD-E-001 跨版本升级验证",
        "evidence_refs": [
            "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md",
            "docs/evidence/stage-e/e-road-001-zero-recompile-host-upgrade-gate.md",
        ],
        "review_point": "后续新增 N-1/N-2 垫片翻译分支或新 SABI 通道扩展时强制补跑对抗性越权回归",
    },
    "RISK-E-03": {
        "category": "third-party-supply-chain",
        "severity": "P1",
        "status": "partial",
        "description": "商店元数据重放攻击、私钥失窃、恶意定制包或跨发布者抢注既有 ApplicationId（关联 RISK-B-09）",
        "mitigation": "已落地三级发布者 Ed25519 PKI（SigningKeyCustodyVault KEK 托管与双态密钥轮换）、SQLite RAISE(ABORT) 防回滚单调 RevocationEpoch、首次注册 ApplicationId 防抢注、纯 Rust 锁文件哈希/SPDX 许可证供应链门禁与 H7 六类攻击对抗回归；物理硬件 HSM 与商业外部公证 CA 属 E-deferred 运营扩展",
        "evidence_refs": [
            "docs/evidence/stage-e/e-trust-001-publisher-pki-revocation-and-selectors.md",
            "docs/evidence/stage-e/e-store-001-oci-index-and-entitlement-ledger.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
            "docs/evidence/stage-e/e-security-001-h7-supply-chain-and-revocation-adversarial.md",
        ],
        "review_point": "阶段 E 退出后接入公网多租户商店分发、商业 CA 公证或物理硬件 HSM 密钥托管前复审",
    },
    "RISK-E-04": {
        "category": "security-bypass",
        "severity": "P1",
        "status": "partial",
        "description": "跨 OS 与 WASI 碎片化导致第三方不可信扩展逃逸沙箱（关联 U-9）",
        "mitigation": "已落地 nlos-wasm-sandbox L1 专职空环境宿主子进程（env_clear + 显式 argv + 墙钟死线硬杀）与 L2 手写 13 导入 WASI 白名单（零 socket 导入注册、词法+canonicalize 双重路径围栏、64KiB 页硬限、确定性 fuel 与 epoch 中断，Windows 11 本机 54/54 通过闭合 U-9）；Linux/macOS #[cfg(unix)] 符号链接分支与写授信树内极端 TOCTOU 窗口由三平台 CI 与后续生产硬化持续守护",
        "evidence_refs": [
            "docs/evidence/stage-e/e-sandbox-001-cross-platform-wasm-process-isolation.md",
            "docs/evidence/stage-e/e-market-002-desktop-store-transparency-ui.md",
        ],
        "review_point": "远端三平台 CI 常态化守护及未来扩展 WASI 导入面（如熵源或多线程提案）前强制复审",
    },
    "RISK-E-05": {
        "category": "durable-state-loss",
        "severity": "P1",
        "status": "partial",
        "description": "跨版本升级时数据迁移半途崩溃导致用户数据损坏，或将不可逆 schema 破坏伪装为自动回滚",
        "mitigation": "nlos-migrate 与 nlos-certify Gate 3 强制在独立快照沙箱演练健康探针失败与半途崩溃原子回滚（校验 UserDataContainer 全目录 SHA-256 树摘要逐位全等且零 .tmp/.partial 残留），默认阻断 IRREVERSIBLE 自动升级并在桌面商店强制联动 Z4 显式重授权；真实跨主机物理断电台架与 C-POWER-LOSS 同口径保持显式边界",
        "evidence_refs": [
            "docs/evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md",
            "docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md",
            "docs/evidence/stage-e/e-security-001-h7-supply-chain-and-revocation-adversarial.md",
            "docs/evidence/stage-e/e-road-001-zero-recompile-host-upgrade-gate.md",
        ],
        "review_point": "后续引入大体积跨代状态迁移器或物理断电故障台架实测时复核",
    },
}


def _yaml_quote(value: str) -> str:
    """Format a string as a canonical YAML double-quoted scalar (only \\ and \" escapes)."""
    cleaned = value.replace("\r", " ").replace("\n", " ").replace("\t", " ")
    escaped = cleaned.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def _extract_markdown_section(text: str, heading_prefix: str) -> str:
    """Extract markdown section body starting at ###/## heading_prefix up to next ##/### heading."""
    normalized = text.replace("\r\n", "\n")
    pattern = re.compile(
        r"^" + re.escape(heading_prefix) + r"[^\n]*\n(.*?)(?=\n#{2,3}(?!#)\s|\Z)",
        re.M | re.S,
    )
    match = pattern.search(normalized)
    if match is None:
        raise ValueError(f"Unable to locate section '{heading_prefix}' in {STAGE_E_PROGRESS_REL}")
    return match.group(1)


def extract_stage_e_spec(root: Path) -> dict[str, Any]:
    """Dynamically extract Stage E authoritative Requirement IDs, Work Packages, and Risks."""
    progress_path = root / STAGE_E_PROGRESS_REL
    if not progress_path.is_file():
        raise FileNotFoundError(f"Missing authoritative progress document: {progress_path}")
    text = progress_path.read_text(encoding="utf-8")

    # 1. Section E.1.1 normative Requirement IDs (column 2 of mapping table).
    sec_e11 = _extract_markdown_section(text, "### E.1.1")
    e11_ids: list[str] = []
    for line in sec_e11.splitlines():
        stripped = line.strip()
        if not stripped.startswith("|") or stripped.startswith("|---") or "阶段 E 角色" in stripped:
            continue
        cols = [c.strip() for c in stripped.strip("|").split("|")]
        if len(cols) >= 2:
            for tok in NORMATIVE_REQ_TOKEN_RE.findall(cols[1]):
                if tok not in e11_ids:
                    e11_ids.append(tok)

    # 2. Section E.2.2 Work Package canonical IDs (column 1) + additional normative IDs (e.g. DUI-ASSOC-001).
    sec_e22 = _extract_markdown_section(text, "### E.2.2")
    work_package_ids: list[str] = []
    e22_additional_req_ids: list[str] = []
    for line in sec_e22.splitlines():
        stripped = line.strip()
        if not stripped.startswith("|") or stripped.startswith("|---") or "工作包" in stripped:
            continue
        cols = [c.strip() for c in stripped.strip("|").split("|")]
        if cols:
            for wp in WORK_PACKAGE_TOKEN_RE.findall(cols[0]):
                if wp not in work_package_ids:
                    work_package_ids.append(wp)
        for req_tok in NORMATIVE_REQ_TOKEN_RE.findall(stripped):
            if req_tok not in e11_ids and req_tok not in e22_additional_req_ids:
                e22_additional_req_ids.append(req_tok)

    # 3. Section E.5.2 Stage E Risk IDs, categories, and severities.
    sec_e52 = _extract_markdown_section(text, "### E.5.2")
    stage_e_risks: list[dict[str, str]] = []
    seen_risks: set[str] = set()
    for line in sec_e52.splitlines():
        stripped = line.strip()
        if not stripped.startswith("|") or "RISK-E-" not in stripped:
            continue
        cols = [c.strip() for c in stripped.strip("|").split("|")]
        if len(cols) >= 3:
            m_id = re.search(r"`(RISK-E-\d{2})`", cols[0])
            m_cat = re.search(r"`([a-z0-9-]+)`", cols[1])
            m_sev = re.search(r"`(P[0-3])`", cols[2])
            if m_id and m_cat and m_sev and m_id.group(1) not in seen_risks:
                seen_risks.add(m_id.group(1))
                stage_e_risks.append(
                    {
                        "id": m_id.group(1),
                        "category": m_cat.group(1),
                        "severity": m_sev.group(1),
                    }
                )

    normative_requirement_ids = list(e11_ids) + list(e22_additional_req_ids)
    all_required_claim_ids = list(normative_requirement_ids) + list(work_package_ids)

    return {
        "e11_requirement_ids": e11_ids,
        "e22_additional_requirement_ids": e22_additional_req_ids,
        "normative_requirement_ids": normative_requirement_ids,
        "work_package_ids": work_package_ids,
        "all_required_claim_ids": all_required_claim_ids,
        "stage_e_risks": stage_e_risks,
        "cross_stage_risk_ids": list(CROSS_STAGE_REQUIRED_RISKS),
    }


def classify_stage_e_claims(root: Path, spec: dict[str, Any] | None = None) -> dict[str, Any]:
    """Classify Stage E authoritative claim IDs against live docs/management/claims.yaml."""
    lc = load_lint_claims_module(root)
    if spec is None:
        spec = extract_stage_e_spec(root)
    claims_path = root / lc.CLAIMS_PATH
    claims = lc.parse_canonical_yaml(claims_path.read_text(encoding="utf-8"))
    if not isinstance(claims, list):
        raise ValueError(f"{claims_path} root must be a YAML list")

    by_id: dict[str, list[dict[str, Any]]] = {}
    for entry in claims:
        if isinstance(entry, dict) and isinstance(entry.get("requirement_id"), str):
            by_id.setdefault(entry["requirement_id"], []).append(entry)

    duplicate_ids = sorted(rid for rid, items in by_id.items() if len(items) > 1)
    registered_earlier_stage: list[str] = []
    registered_stage_e: list[str] = []
    missing_from_claims: list[str] = []

    for req_id in spec["all_required_claim_ids"]:
        entries = by_id.get(req_id, [])
        if not entries:
            missing_from_claims.append(req_id)
        else:
            stage = entries[0].get("stage")
            if stage == "E":
                registered_stage_e.append(req_id)
            else:
                registered_earlier_stage.append(req_id)

    return {
        "total_existing_claims": len(claims),
        "duplicate_requirement_ids": duplicate_ids,
        "registered_earlier_stage": registered_earlier_stage,
        "registered_stage_e": registered_stage_e,
        "missing_from_claims": missing_from_claims,
        "existing_by_id": {k: v[0] for k, v in by_id.items() if v},
    }


def render_evidence_candidates_yaml(entries: list[dict[str, Any]]) -> str:
    """Serialize candidate evidence-index.yaml entries in canonical YAML v1.3 subset."""
    if not entries:
        return ""
    lines = [
        "# ---------------------------------------------------------------------------",
        "# Stage E W81 Exit Gate & Conformance Audit Evidence (candidate backfill)",
        "# ---------------------------------------------------------------------------",
    ]
    for item in entries:
        lines.append(f"- id: {item['id']}")
        lines.append(f"  path: {item['path']}")
        lines.append(f"  title: {_yaml_quote(item['title'])}")
        lines.append(f"  scope: {_yaml_quote(item['scope'])}")
        lines.append(f"  assurance: {item['assurance']}")
        lines.append(f"  date: {_yaml_quote(item['date'])}")
    return "\n".join(lines) + "\n"


def render_claims_candidates_yaml(entries: list[dict[str, Any]]) -> str:
    """Serialize candidate claims.yaml entries in canonical YAML v1.3 subset."""
    if not entries:
        return ""
    lines = [
        "# ===========================================================================",
        "# Stage E — Long-Term SABI Compatibility, Store Governance & Ecosystem (W81)",
        "# ===========================================================================",
    ]
    for idx, item in enumerate(entries):
        if idx > 0:
            lines.append("")
        lines.append(f"- requirement_id: {item['requirement_id']}")
        lines.append(f"  stage: {item['stage']}")
        lines.append(f"  status: {item['status']}")
        if item["implementation_refs"]:
            lines.append("  implementation_refs:")
            for ref in item["implementation_refs"]:
                lines.append(f"    - {ref}")
        else:
            lines.append("  implementation_refs: []")
        if item["test_refs"]:
            lines.append("  test_refs:")
            for ref in item["test_refs"]:
                lines.append(f"    - {ref}")
        else:
            lines.append("  test_refs: []")
        if item["evidence_refs"]:
            lines.append("  evidence_refs:")
            for ref in item["evidence_refs"]:
                lines.append(f"    - {ref}")
        else:
            lines.append("  evidence_refs: []")
        lines.append(f"  assurance: {item['assurance']}")
        if item["limitations"]:
            lines.append("  limitations:")
            for lim in item["limitations"]:
                lines.append(f"    - {_yaml_quote(lim)}")
        else:
            lines.append("  limitations: []")
        lines.append(f"  source: {_yaml_quote(item['source'])}")
    return "\n".join(lines) + "\n"


def render_risks_candidates_yaml(entries: list[dict[str, Any]]) -> str:
    """Serialize candidate risks.yaml entries in canonical YAML v1.3 subset."""
    if not entries:
        return ""
    lines = [
        "# ---------------------------------------------------------------------------",
        "# Stage E — Compatibility, Shim, Supply Chain, Sandbox & Migration Risks (W81)",
        "# ---------------------------------------------------------------------------",
    ]
    for idx, item in enumerate(entries):
        if idx > 0:
            lines.append("")
        lines.append(f"- id: {item['id']}")
        lines.append(f"  category: {item['category']}")
        lines.append(f"  severity: {item['severity']}")
        lines.append(f"  status: {item['status']}")
        lines.append(f"  description: {_yaml_quote(item['description'])}")
        lines.append(f"  mitigation: {_yaml_quote(item['mitigation'])}")
        if item.get("evidence_refs"):
            lines.append("  evidence_refs:")
            for ref in item["evidence_refs"]:
                lines.append(f"    - {ref}")
        else:
            lines.append("  evidence_refs: []")
        lines.append(f"  review_point: {_yaml_quote(item['review_point'])}")
    return "\n".join(lines) + "\n"


def generate_candidate_data(root: Path) -> dict[str, Any]:
    """Build structured candidate entries for evidence-index.yaml, claims.yaml, and risks.yaml."""
    lc = load_lint_claims_module(root)
    spec = extract_stage_e_spec(root)
    classification = classify_stage_e_claims(root, spec)

    index_raw = lc.parse_canonical_yaml((root / lc.INDEX_PATH).read_text(encoding="utf-8"))
    risks_raw = lc.parse_canonical_yaml((root / lc.RISKS_PATH).read_text(encoding="utf-8"))
    existing_index_ids = {
        e["id"] for e in (index_raw or []) if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    existing_index_paths = {
        e["path"]
        for e in (index_raw or [])
        if isinstance(e, dict) and isinstance(e.get("path"), str)
    }
    existing_risk_ids = {
        r["id"] for r in (risks_raw or []) if isinstance(r, dict) and isinstance(r.get("id"), str)
    }

    evidence_candidates = [
        dict(item)
        for item in CANDIDATE_EVIDENCE_ENTRIES
        if item["id"] not in existing_index_ids and item["path"] not in existing_index_paths
    ]

    claim_candidates: list[dict[str, Any]] = []
    for req_id in classification["missing_from_claims"]:
        if req_id not in STAGE_E_CLAIM_CATALOG:
            raise KeyError(
                f"Authoritative Stage E ID '{req_id}' from {STAGE_E_PROGRESS_REL} "
                f"is missing from STAGE_E_CLAIM_CATALOG"
            )
        meta = STAGE_E_CLAIM_CATALOG[req_id]
        claim_candidates.append(
            {
                "requirement_id": req_id,
                "stage": "E",
                "status": "PARTIAL_PASS",
                "implementation_refs": list(meta["implementation_refs"]),
                "test_refs": list(meta["test_refs"]),
                "evidence_refs": list(meta["evidence_refs"]),
                "assurance": meta["assurance"],
                "limitations": list(meta["limitations"]),
                "source": meta["source"],
            }
        )

    risk_candidates: list[dict[str, Any]] = []
    for risk_spec in spec["stage_e_risks"]:
        rid = risk_spec["id"]
        if rid in existing_risk_ids:
            continue
        if rid not in STAGE_E_RISK_CATALOG:
            raise KeyError(
                f"Authoritative Stage E Risk '{rid}' from {STAGE_E_PROGRESS_REL} "
                f"is missing from STAGE_E_RISK_CATALOG"
            )
        meta = STAGE_E_RISK_CATALOG[rid]
        risk_candidates.append(
            {
                "id": rid,
                "category": risk_spec["category"],
                "severity": risk_spec["severity"],
                "status": meta["status"],
                "description": meta["description"],
                "mitigation": meta["mitigation"],
                "evidence_refs": list(meta["evidence_refs"]),
                "review_point": meta["review_point"],
            }
        )

    return {
        "spec": spec,
        "classification": classification,
        "evidence_candidates": evidence_candidates,
        "claim_candidates": claim_candidates,
        "risk_candidates": risk_candidates,
        "yaml": {
            "evidence": render_evidence_candidates_yaml(evidence_candidates),
            "claims": render_claims_candidates_yaml(claim_candidates),
            "risks": render_risks_candidates_yaml(risk_candidates),
        },
    }


def _strip_code_fences_and_inline_spans(markdown_text: str) -> str:
    """Strip fenced code blocks and inline backtick code spans before markdown link scanning."""
    out_lines: list[str] = []
    in_fence = False
    fence_char = ""
    fence_len = 0
    for line in markdown_text.replace("\r\n", "\n").splitlines():
        lstripped = line.lstrip()
        m_fence = re.match(r"^(`{3,}|~{3,})", lstripped)
        if m_fence:
            marker = m_fence.group(1)
            if not in_fence:
                in_fence = True
                fence_char = marker[0]
                fence_len = len(marker)
                continue
            if marker[0] == fence_char and len(marker) >= fence_len:
                in_fence = False
                fence_char = ""
                fence_len = 0
                continue
        if not in_fence:
            cleaned = re.sub(r"(`+)([^`]*?)\1", "", line)
            out_lines.append(cleaned)
    return "\n".join(out_lines)


def collect_stage_e_markdown_targets(root: Path) -> list[Path]:
    """Return all Stage E governance & evidence markdown files subject to emoji/link audit."""
    targets: list[Path] = []
    prog = root / STAGE_E_PROGRESS_REL
    if prog.is_file():
        targets.append(prog)
    targets.extend(sorted(root.glob(ADR_GLOB_REL)))
    ev_dir = root / STAGE_E_EVIDENCE_DIR_REL
    if ev_dir.is_dir():
        targets.extend(sorted(ev_dir.rglob("*.md")))
    return targets


def scan_emoji_and_relative_links(root: Path, md_files: list[Path] | None = None) -> dict[str, Any]:
    """Check markdown files for zero pictographic emoji and zero broken relative filesystem links."""
    if md_files is None:
        md_files = collect_stage_e_markdown_targets(root)
    emoji_violations: list[dict[str, Any]] = []
    broken_links: list[dict[str, Any]] = []
    checked_links = 0

    for md_path in md_files:
        rel_doc = md_path.relative_to(root).as_posix()
        text = md_path.read_text(encoding="utf-8")
        for line_no, line in enumerate(text.replace("\r\n", "\n").splitlines(), start=1):
            found = PICTOGRAPHIC_EMOJI_RE.findall(line)
            if found:
                codepoints = sorted({f"U+{ord(ch):04X}" for ch in found})
                emoji_violations.append(
                    {
                        "file": rel_doc,
                        "line": line_no,
                        "codepoints": codepoints,
                    }
                )

        stripped = _strip_code_fences_and_inline_spans(text)
        for match in MARKDOWN_LINK_RE.finditer(stripped):
            raw_target = match.group(1).strip()
            if not raw_target:
                continue
            if raw_target.startswith(
                ("#", "http://", "https://", "mailto:", "conversation://", "file://")
            ):
                continue
            # Strip optional markdown link title e.g. path "title"
            target_no_title = raw_target.split()[0].strip("<>")
            path_part = target_no_title.split("#", 1)[0].strip()
            if not path_part:
                continue
            checked_links += 1
            resolved = (md_path.parent / path_part).resolve()
            if not resolved.exists():
                broken_links.append(
                    {
                        "file": rel_doc,
                        "target": raw_target,
                        "resolved": str(resolved),
                    }
                )

    return {
        "scanned_files": [p.relative_to(root).as_posix() for p in md_files],
        "scanned_file_count": len(md_files),
        "checked_relative_links": checked_links,
        "emoji_violations": emoji_violations,
        "broken_links": broken_links,
    }


def audit_strict(root: Path) -> dict[str, Any]:
    """Execute strict Stage E post-backfill ledger coverage & hygiene verification."""
    root = root.resolve()
    lc = load_lint_claims_module(root)
    errors: list[str] = []

    # 1. Run canonical scripts/lint_claims.py verification.
    lint_findings, lint_stats = lc.run(root)
    lint_errors = [f for f in lint_findings if f[0] == "ERROR"]
    for _, where, msg in lint_errors:
        errors.append(f"[lint_claims:{where}] {msg}")

    # 2. Extract authoritative Stage E spec & parse ledger files.
    spec = extract_stage_e_spec(root)
    claims_path = root / lc.CLAIMS_PATH
    risks_path = root / lc.RISKS_PATH
    index_path = root / lc.INDEX_PATH

    claims = lc.parse_canonical_yaml(claims_path.read_text(encoding="utf-8"))
    risks = lc.parse_canonical_yaml(risks_path.read_text(encoding="utf-8"))
    index = lc.parse_canonical_yaml(index_path.read_text(encoding="utf-8"))

    if not isinstance(claims, list) or not isinstance(risks, list) or not isinstance(index, list):
        errors.append("[schema] claims.yaml, risks.yaml, and evidence-index.yaml must parse to lists")
        return {"ok": False, "mode": "strict", "errors": errors}

    index_by_path: dict[str, dict[str, Any]] = {}
    for entry in index:
        if isinstance(entry, dict) and isinstance(entry.get("path"), str):
            index_by_path[entry["path"]] = entry

    claims_by_id: dict[str, list[dict[str, Any]]] = {}
    for entry in claims:
        if isinstance(entry, dict) and isinstance(entry.get("requirement_id"), str):
            claims_by_id.setdefault(entry["requirement_id"], []).append(entry)

    duplicate_claim_ids = sorted(rid for rid, items in claims_by_id.items() if len(items) > 1)
    for rid in duplicate_claim_ids:
        errors.append(f"[claims:uniqueness] duplicate requirement_id '{rid}' across claims.yaml")

    missing_normative_ids: list[str] = []
    missing_work_package_ids: list[str] = []
    design_assurance_violations: list[str] = []
    rank_violations: list[str] = []

    for req_id in spec["normative_requirement_ids"]:
        if req_id not in claims_by_id:
            missing_normative_ids.append(req_id)
            errors.append(f"[claims:coverage] missing Stage E normative Requirement ID '{req_id}'")

    for wp_id in spec["work_package_ids"]:
        if wp_id not in claims_by_id:
            missing_work_package_ids.append(wp_id)
            errors.append(f"[claims:coverage] missing Stage E Work Package ID '{wp_id}'")

    # Verify non-regression across all claims and Stage E assurance != DESIGN + Claim <= Evidence.
    required_set = set(spec["all_required_claim_ids"])
    for rid, entries in claims_by_id.items():
        claim = entries[0]
        assurance = claim.get("assurance")
        if rid in required_set:
            if assurance == "DESIGN" and rid not in INTENTIONALLY_DEFERRED_DESIGN_CLAIMS:
                design_assurance_violations.append(rid)
                errors.append(
                    f"[claims:assurance] Stage E required claim '{rid}' cannot remain at assurance=DESIGN"
                )
            ev_refs = claim.get("evidence_refs") or []
            if isinstance(assurance, str) and assurance in lc.SUPPORT_RANK and assurance != "DESIGN":
                ev_ranks = [
                    lc.SUPPORT_RANK[index_by_path[p]["assurance"]]
                    for p in ev_refs
                    if isinstance(p, str)
                    and p in index_by_path
                    and index_by_path[p].get("assurance") in lc.SUPPORT_RANK
                ]
                max_ev_rank = max(ev_ranks) if ev_ranks else -1
                if lc.SUPPORT_RANK[assurance] > max_ev_rank:
                    rank_violations.append(rid)
                    errors.append(
                        f"[claims:rank] '{rid}' assurance={assurance} exceeds max evidence rank={max_ev_rank}"
                    )
        else:
            # Earlier-stage non-regression check: only explicitly deferred items may stay DESIGN with NOT_STARTED.
            if (
                claim.get("stage") in {"B", "C", "D"}
                and assurance == "DESIGN"
                and rid not in INTENTIONALLY_DEFERRED_DESIGN_CLAIMS
            ):
                errors.append(
                    f"[claims:regression] earlier-stage claim '{rid}' unexpectedly regressed to DESIGN"
                )

    # 3. Verify risks.yaml: RISK-E-01..05, RISK-B-05, RISK-B-09, and open/non-closed P0 == 0.
    risks_by_id: dict[str, dict[str, Any]] = {}
    open_p0_risks: list[str] = []
    for entry in risks:
        if isinstance(entry, dict) and isinstance(entry.get("id"), str):
            risks_by_id[entry["id"]] = entry
            if entry.get("severity") == "P0" and entry.get("status") != "closed":
                open_p0_risks.append(entry["id"])
                errors.append(
                    f"[risks:p0] non-closed P0 risk '{entry['id']}' (status={entry.get('status')}) blocks exit"
                )

    missing_stage_e_risks: list[str] = []
    for risk_spec in spec["stage_e_risks"]:
        rid = risk_spec["id"]
        if rid not in risks_by_id:
            missing_stage_e_risks.append(rid)
            errors.append(f"[risks:coverage] missing Stage E risk '{rid}' in risks.yaml")
        else:
            actual = risks_by_id[rid]
            if actual.get("category") != risk_spec["category"]:
                errors.append(
                    f"[risks:category] '{rid}' category={actual.get('category')} "
                    f"!= section E.5.2 '{risk_spec['category']}'"
                )
            if actual.get("severity") != risk_spec["severity"]:
                errors.append(
                    f"[risks:severity] '{rid}' severity={actual.get('severity')} "
                    f"!= section E.5.2 '{risk_spec['severity']}'"
                )

    for cross_rid in spec["cross_stage_risk_ids"]:
        if cross_rid not in risks_by_id:
            errors.append(f"[risks:cross-stage] missing inherited risk '{cross_rid}' in risks.yaml")

    # 4. Verify docs/evidence/stage-e/**/*.md count (>= 16) and bidirectional indexing.
    stage_e_ev_dir = root / STAGE_E_EVIDENCE_DIR_REL
    stage_e_disk_files = (
        sorted(p.relative_to(root).as_posix() for p in stage_e_ev_dir.rglob("*.md"))
        if stage_e_ev_dir.is_dir()
        else []
    )
    stage_e_indexed_files = sorted(
        p for p in index_by_path if p.startswith(STAGE_E_EVIDENCE_DIR_REL + "/")
    )

    if len(stage_e_disk_files) < REQUIRED_STAGE_E_MIN_EVIDENCE_COUNT:
        errors.append(
            f"[evidence:count] expected >= {REQUIRED_STAGE_E_MIN_EVIDENCE_COUNT} Stage E evidence files, "
            f"found {len(stage_e_disk_files)}"
        )

    for req_ev in W81_PENDING_EVIDENCE_PATHS:
        if req_ev not in stage_e_disk_files:
            errors.append(f"[evidence:w81] missing required W81 Stage E evidence file on disk: {req_ev}")
        if req_ev not in index_by_path:
            errors.append(f"[evidence:w81] missing required W81 Stage E evidence entry in index: {req_ev}")

    unindexed_stage_e = sorted(set(stage_e_disk_files) - set(stage_e_indexed_files))
    missing_disk_stage_e = sorted(set(stage_e_indexed_files) - set(stage_e_disk_files))
    for p in unindexed_stage_e:
        errors.append(f"[evidence:sync] unindexed Stage E evidence file on disk: {p}")
    for p in missing_disk_stage_e:
        errors.append(f"[evidence:sync] indexed Stage E evidence file missing on disk: {p}")

    # 5. Emoji & broken relative markdown link scan across Stage E docs.
    hygiene = scan_emoji_and_relative_links(root)
    for item in hygiene["emoji_violations"]:
        errors.append(
            f"[hygiene:emoji] pictographic emoji {item['codepoints']} in {item['file']}:{item['line']}"
        )
    for item in hygiene["broken_links"]:
        errors.append(
            f"[hygiene:link] broken relative markdown link '{item['target']}' in {item['file']}"
        )

    stage_counts: dict[str, int] = {}
    for entry in claims:
        if isinstance(entry, dict) and isinstance(entry.get("stage"), str):
            stage_counts[entry["stage"]] = stage_counts.get(entry["stage"], 0) + 1

    return {
        "ok": len(errors) == 0,
        "mode": "strict",
        "root": str(root),
        "counts": {
            "e11_normative_requirements": len(spec["e11_requirement_ids"]),
            "e22_additional_requirements": len(spec["e22_additional_requirement_ids"]),
            "total_normative_requirements": len(spec["normative_requirement_ids"]),
            "work_packages": len(spec["work_package_ids"]),
            "total_stage_e_required_claims": len(spec["all_required_claim_ids"]),
            "claims_total": len(claims),
            "claims_by_stage": stage_counts,
            "risks_total": len(risks),
            "stage_e_risks_expected": len(spec["stage_e_risks"]),
            "open_p0_risks": len(open_p0_risks),
            "evidence_index_total": len(index),
            "evidence_on_disk_total": lint_stats.get("on_disk", 0),
            "stage_e_evidence_on_disk": len(stage_e_disk_files),
            "stage_e_evidence_indexed": len(stage_e_indexed_files),
            "markdown_files_scanned": hygiene["scanned_file_count"],
            "relative_links_checked": hygiene["checked_relative_links"],
        },
        "missing": {
            "normative_requirements": missing_normative_ids,
            "work_packages": missing_work_package_ids,
            "stage_e_risks": missing_stage_e_risks,
            "unindexed_stage_e_evidence": unindexed_stage_e,
        },
        "hygiene": hygiene,
        "lint_error_count": len(lint_errors),
        "errors": errors,
    }


def _materialize_existing_repo_placeholder(src_root: Path, dst_root: Path, rel_path: str) -> None:
    """Create a lightweight file or directory placeholder in dst_root ONLY if rel_path exists in src_root."""
    src = (src_root / rel_path).resolve()
    try:
        norm_rel = src.relative_to(src_root.resolve()).as_posix()
    except ValueError:
        return
    if not src.exists():
        return
    dst = dst_root / norm_rel
    if dst.exists():
        return
    if src.is_dir():
        dst.mkdir(parents=True, exist_ok=True)
    else:
        dst.parent.mkdir(parents=True, exist_ok=True)
        dst.write_bytes(b"")


def build_simulated_post_backfill_tree(src_root: Path, dst_root: Path) -> dict[str, Any]:
    """Construct an isolated simulated post-W81-INT repository tree in dst_root."""
    src_root = src_root.resolve()
    dst_root = dst_root.resolve()
    lc = load_lint_claims_module(src_root)

    # 1. Copy docs/management and docs/evidence verbatim, plus scripts/lint_claims.py.
    shutil.copytree(src_root / "docs" / "management", dst_root / "docs" / "management")
    shutil.copytree(src_root / "docs" / "evidence", dst_root / "docs" / "evidence")
    (dst_root / "scripts").mkdir(parents=True, exist_ok=True)
    shutil.copy2(src_root / "scripts" / "lint_claims.py", dst_root / "scripts" / "lint_claims.py")

    # 2. Generate candidate backfill blocks from src_root.
    candidates = generate_candidate_data(src_root)

    # 3. Materialize lightweight placeholders for paths that genuinely exist in src_root
    #    and are referenced by existing claims, candidate claims, or Stage E markdown links.
    existing_claims = lc.parse_canonical_yaml(
        (src_root / lc.CLAIMS_PATH).read_text(encoding="utf-8")
    )
    referenced_paths: set[str] = set()
    for claim in list(existing_claims or []) + list(candidates["claim_candidates"]):
        if not isinstance(claim, dict):
            continue
        for key in ("implementation_refs", "test_refs"):
            for ref in claim.get(key) or []:
                if isinstance(ref, str) and not lc.COMMIT_RE.match(ref):
                    referenced_paths.add(ref)

    for rel in sorted(referenced_paths):
        _materialize_existing_repo_placeholder(src_root, dst_root, rel)

    # Materialize targets of relative markdown links ONLY when they already exist in src_root.
    for md_path in collect_stage_e_markdown_targets(src_root):
        stripped = _strip_code_fences_and_inline_spans(md_path.read_text(encoding="utf-8"))
        for match in MARKDOWN_LINK_RE.finditer(stripped):
            raw_target = match.group(1).strip()
            if not raw_target or raw_target.startswith(
                ("#", "http://", "https://", "mailto:", "conversation://", "file://")
            ):
                continue
            target_no_title = raw_target.split()[0].strip("<>")
            path_part = target_no_title.split("#", 1)[0].strip()
            if not path_part:
                continue
            resolved = (md_path.parent / path_part).resolve()
            if resolved.exists():
                try:
                    rel_to_src = resolved.relative_to(src_root).as_posix()
                    _materialize_existing_repo_placeholder(src_root, dst_root, rel_to_src)
                except ValueError:
                    pass

    # 4. Stub pending sibling W81 deliverables (W81-E7A / W81-E7B / W81-E7C) if not yet merged in src_root.
    stubbed_w81_paths: list[str] = []
    for rel_dir in W81_PENDING_CODE_DIRS:
        target = dst_root / rel_dir
        if not target.exists():
            target.mkdir(parents=True, exist_ok=True)
            stubbed_w81_paths.append(rel_dir)

    for rel_file in W81_PENDING_CODE_FILES:
        target = dst_root / rel_file
        if not target.exists():
            target.parent.mkdir(parents=True, exist_ok=True)
            if (src_root / rel_file).is_file():
                shutil.copy2(src_root / rel_file, target)
            else:
                target.write_text("# W81 pending stub\n", encoding="utf-8")
                stubbed_w81_paths.append(rel_file)

    for rel_ev in W81_PENDING_EVIDENCE_PATHS:
        target = dst_root / rel_ev
        if not target.exists():
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(
                f"# Stub Evidence ({rel_ev})\n\nPending W81 sibling lane merge.\n",
                encoding="utf-8",
            )
            stubbed_w81_paths.append(rel_ev)

    # 5. Append generated candidate canonical YAML blocks to the simulated ledger files.
    if candidates["yaml"]["evidence"]:
        with (dst_root / lc.INDEX_PATH).open("a", encoding="utf-8", newline="\n") as fh:
            fh.write("\n" + candidates["yaml"]["evidence"])
    if candidates["yaml"]["claims"]:
        with (dst_root / lc.CLAIMS_PATH).open("a", encoding="utf-8", newline="\n") as fh:
            fh.write("\n" + candidates["yaml"]["claims"])
    if candidates["yaml"]["risks"]:
        with (dst_root / lc.RISKS_PATH).open("a", encoding="utf-8", newline="\n") as fh:
            fh.write("\n" + candidates["yaml"]["risks"])

    return {
        "candidates": candidates,
        "stubbed_w81_paths": stubbed_w81_paths,
    }


def audit_pre_backfill(root: Path) -> dict[str, Any]:
    """Audit pre-backfill baseline and verify simulated post-W81-INT strict pass."""
    root = root.resolve()
    lc = load_lint_claims_module(root)
    errors: list[str] = []

    # Step 1: Audit current untouched shared ledger baseline on root.
    base_findings, base_stats = lc.run(root)
    pending_w81_unindexed: list[str] = []
    unexpected_base_errors: list[str] = []
    allowed_unindexed_msgs = {
        f"目录文件未收录: {rel}": rel for rel in W81_PENDING_EVIDENCE_PATHS
    }

    for level, where, msg in base_findings:
        if level != "ERROR":
            continue
        if where == "evidence-index" and msg in allowed_unindexed_msgs:
            pending_w81_unindexed.append(allowed_unindexed_msgs[msg])
        else:
            unexpected_base_errors.append(f"[{where}] {msg}")
            errors.append(f"[baseline:lint_claims:{where}] {msg}")

    if base_stats.get("index", 0) < 201:
        errors.append(
            f"[baseline:evidence-index] expected >= 201 indexed evidence items, got {base_stats.get('index', 0)}"
        )

    base_risks = lc.parse_canonical_yaml((root / lc.RISKS_PATH).read_text(encoding="utf-8"))
    base_open_p0 = [
        r["id"]
        for r in (base_risks or [])
        if isinstance(r, dict) and r.get("severity") == "P0" and r.get("status") != "closed"
    ]
    if base_open_p0:
        errors.append(f"[baseline:risks] unexpected non-closed P0 risks: {base_open_p0}")

    # Also check hygiene on all existing Stage E markdown files currently on disk in root.
    base_hygiene = scan_emoji_and_relative_links(root)
    for item in base_hygiene["emoji_violations"]:
        errors.append(
            f"[baseline:hygiene:emoji] pictographic emoji {item['codepoints']} in {item['file']}:{item['line']}"
        )
    for item in base_hygiene["broken_links"]:
        errors.append(
            f"[baseline:hygiene:link] broken relative markdown link '{item['target']}' in {item['file']}"
        )

    # Step 2: Build simulated post-W81-INT tree in TemporaryDirectory and run strict audit.
    with tempfile.TemporaryDirectory(prefix="nlos_w81_stage_e_ledger_") as tmp_dir:
        tmp_root = Path(tmp_dir)
        sim_info = build_simulated_post_backfill_tree(root, tmp_root)
        sim_strict = audit_strict(tmp_root)

    for err in sim_strict["errors"]:
        errors.append(f"[simulated-post-backfill] {err}")

    cand = sim_info["candidates"]
    cls = cand["classification"]

    return {
        "ok": len(errors) == 0,
        "mode": "pre-backfill",
        "root": str(root),
        "baseline": {
            "claims_total": cls["total_existing_claims"],
            "risks_total": len(base_risks or []),
            "evidence_indexed": base_stats.get("index", 0),
            "evidence_on_disk": base_stats.get("on_disk", 0),
            "pending_w81_unindexed_on_disk": pending_w81_unindexed,
            "unexpected_lint_errors": len(unexpected_base_errors),
            "open_p0_risks": len(base_open_p0),
            "markdown_files_scanned": base_hygiene["scanned_file_count"],
            "relative_links_checked": base_hygiene["checked_relative_links"],
        },
        "classification": {
            "e11_normative_requirements": len(cand["spec"]["e11_requirement_ids"]),
            "e22_additional_requirements": len(cand["spec"]["e22_additional_requirement_ids"]),
            "total_normative_requirements": len(cand["spec"]["normative_requirement_ids"]),
            "work_packages": len(cand["spec"]["work_package_ids"]),
            "total_stage_e_required_claims": len(cand["spec"]["all_required_claim_ids"]),
            "registered_earlier_stage_count": len(cls["registered_earlier_stage"]),
            "registered_earlier_stage_ids": cls["registered_earlier_stage"],
            "registered_stage_e_count": len(cls["registered_stage_e"]),
            "missing_to_backfill_count": len(cls["missing_from_claims"]),
            "missing_to_backfill_ids": cls["missing_from_claims"],
            "candidate_evidence_entries_to_add": len(cand["evidence_candidates"]),
            "candidate_claim_entries_to_add": len(cand["claim_candidates"]),
            "candidate_risk_entries_to_add": len(cand["risk_candidates"]),
        },
        "simulated_post_backfill": {
            "ok": sim_strict["ok"],
            "stubbed_w81_paths": sim_info["stubbed_w81_paths"],
            "counts": sim_strict["counts"],
            "error_count": len(sim_strict["errors"]),
        },
        "errors": errors,
    }


class StageELedgerCoverageSelfTests(unittest.TestCase):
    """Built-in >= 12 positive & negative tamper tests for verify_stage_e_ledger_coverage.py."""

    _tmp_ctx: tempfile.TemporaryDirectory[str] | None = None
    base_sim_root: Path = REPO_ROOT

    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp_ctx = tempfile.TemporaryDirectory(prefix="nlos_w81_selftest_base_")
        cls.base_sim_root = Path(cls._tmp_ctx.name) / "sim_repo"
        cls.base_sim_root.mkdir(parents=True, exist_ok=True)
        build_simulated_post_backfill_tree(REPO_ROOT, cls.base_sim_root)

    @classmethod
    def tearDownClass(cls) -> None:
        if cls._tmp_ctx is not None:
            cls._tmp_ctx.cleanup()
            cls._tmp_ctx = None

    def _clone_sim_repo(self) -> tuple[tempfile.TemporaryDirectory[str], Path]:
        tmp = tempfile.TemporaryDirectory(prefix="nlos_w81_case_")
        dst = Path(tmp.name) / "repo"
        shutil.copytree(self.base_sim_root, dst)
        return tmp, dst

    def test_01_extract_stage_e_spec_counts_and_ids(self) -> None:
        spec = extract_stage_e_spec(REPO_ROOT)
        self.assertEqual(len(spec["e11_requirement_ids"]), 44)
        self.assertEqual(spec["e22_additional_requirement_ids"], ["DUI-ASSOC-001"])
        self.assertEqual(len(spec["normative_requirement_ids"]), 45)
        self.assertIn("ROAD-E-001", spec["normative_requirement_ids"])
        self.assertEqual(len(spec["work_package_ids"]), 8)
        self.assertEqual(len(spec["all_required_claim_ids"]), 53)
        self.assertEqual([r["id"] for r in spec["stage_e_risks"]], [f"RISK-E-0{i}" for i in range(1, 6)])

    def test_02_pre_backfill_and_simulated_strict_pass(self) -> None:
        report = audit_pre_backfill(REPO_ROOT)
        self.assertTrue(report["ok"], msg=f"pre-backfill failed: {report['errors']}")
        self.assertEqual(report["baseline"]["open_p0_risks"], 0)
        sim_counts = report["simulated_post_backfill"]["counts"]
        self.assertEqual(sim_counts["claims_total"], 169)
        self.assertEqual(sim_counts["risks_total"], 17)
        self.assertEqual(sim_counts["evidence_index_total"], 204)
        self.assertEqual(sim_counts["stage_e_evidence_indexed"], 16)

    def test_03_emit_candidates_canonical_yaml_roundtrip(self) -> None:
        lc = load_lint_claims_module(REPO_ROOT)
        cand = generate_candidate_data(REPO_ROOT)
        for section_name, expected_len in (("evidence", 3), ("claims", 53), ("risks", 5)):
            yaml_block = cand["yaml"][section_name]
            parsed = lc.parse_canonical_yaml(yaml_block)
            self.assertIsInstance(parsed, list)
            self.assertEqual(len(parsed), expected_len)

    def test_04_earlier_stage_preregistered_id_prevents_duplicate_emission(self) -> None:
        tmp, repo = self._clone_sim_repo()
        try:
            # Build a pre-backfill-like claims.yaml where TUI-CONSENT-001 is already registered under Stage D.
            lc = load_lint_claims_module(repo)
            claims = lc.parse_canonical_yaml((REPO_ROOT / lc.CLAIMS_PATH).read_text(encoding="utf-8"))
            prereg_block = render_claims_candidates_yaml(
                [
                    {
                        "requirement_id": "TUI-CONSENT-001",
                        "stage": "D",
                        "status": "PARTIAL_PASS",
                        "implementation_refs": ["desktop/src/trusted-ui"],
                        "test_refs": ["desktop/src/trusted-ui"],
                        "evidence_refs": ["docs/evidence/stage-e/e-trial-001-llm-http-driver.md"],
                        "assurance": "H4",
                        "limitations": ["Already registered in Stage D fixture"],
                        "source": "stage-d-progress.md §D.3.1 TUI-CONSENT-001",
                    }
                ]
            )
            base_claims_text = (REPO_ROOT / lc.CLAIMS_PATH).read_text(encoding="utf-8")
            (repo / lc.CLAIMS_PATH).write_text(base_claims_text + "\n" + prereg_block, encoding="utf-8")
            # Reset index & risks to baseline so generate_candidate_data computes fresh delta.
            shutil.copy2(REPO_ROOT / lc.INDEX_PATH, repo / lc.INDEX_PATH)
            shutil.copy2(REPO_ROOT / lc.RISKS_PATH, repo / lc.RISKS_PATH)

            cand = generate_candidate_data(repo)
            self.assertIn("TUI-CONSENT-001", cand["classification"]["registered_earlier_stage"])
            emitted_ids = [c["requirement_id"] for c in cand["claim_candidates"]]
            self.assertNotIn("TUI-CONSENT-001", emitted_ids)
            self.assertEqual(len(emitted_ids), 52)
            self.assertEqual(len(claims) + 1 + len(emitted_ids), 169)
        finally:
            tmp.cleanup()

    def test_05_negative_missing_stage_e_requirement_id(self) -> None:
        tmp, repo = self._clone_sim_repo()
        try:
            claims_file = repo / "docs/management/claims.yaml"
            text = claims_file.read_text(encoding="utf-8")
            # Replace DRV-SHIM-001 with an unrelated synthetic ID so Stage E coverage misses DRV-SHIM-001.
            claims_file.write_text(
                text.replace("- requirement_id: DRV-SHIM-001", "- requirement_id: E-SYNTHETIC-EXTRA"),
                encoding="utf-8",
            )
            res = audit_strict(repo)
            self.assertFalse(res["ok"])
            self.assertIn("DRV-SHIM-001", res["missing"]["normative_requirements"])
        finally:
            tmp.cleanup()

    def test_06_negative_duplicate_requirement_id(self) -> None:
        tmp, repo = self._clone_sim_repo()
        try:
            claims_file = repo / "docs/management/claims.yaml"
            dup_yaml = render_claims_candidates_yaml(
                [
                    {
                        "requirement_id": "COMPAT-VER-001",
                        "stage": "E",
                        "status": "PARTIAL_PASS",
                        "implementation_refs": ["schema/nlos/sabi/v1"],
                        "test_refs": ["tests/conformance/schema/test_sabi_compat_diff_gate.py"],
                        "evidence_refs": [
                            "docs/evidence/stage-e/e-sabi-001-version-matrix-and-deprecation.md"
                        ],
                        "assurance": "H4",
                        "limitations": ["Duplicate test entry"],
                        "source": "stage-e-progress.md §E.1.1",
                    }
                ]
            )
            with claims_file.open("a", encoding="utf-8", newline="\n") as fh:
                fh.write("\n" + dup_yaml)
            res = audit_strict(repo)
            self.assertFalse(res["ok"])
            self.assertTrue(any("COMPAT-VER-001" in e for e in res["errors"]))
        finally:
            tmp.cleanup()

    def test_07_negative_claim_exceeds_evidence_assurance(self) -> None:
        tmp, repo = self._clone_sim_repo()
        try:
            claims_file = repo / "docs/management/claims.yaml"
            text = claims_file.read_text(encoding="utf-8")
            target = (
                "- requirement_id: E-COMPAT-SHIM\n"
                "  stage: E\n"
                "  status: PARTIAL_PASS"
            )
            self.assertIn(target, text)
            # Escalate E-COMPAT-SHIM (backed only by H4 e-shim-001) to H8.
            idx = text.index(target)
            mutated = text[:idx] + text[idx:].replace("  assurance: H4", "  assurance: H8", 1)
            claims_file.write_text(mutated, encoding="utf-8")
            res = audit_strict(repo)
            self.assertFalse(res["ok"])
            self.assertTrue(any("E-COMPAT-SHIM" in e for e in res["errors"]))
        finally:
            tmp.cleanup()

    def test_08_negative_stage_e_design_assurance_rejected(self) -> None:
        tmp, repo = self._clone_sim_repo()
        try:
            claims_file = repo / "docs/management/claims.yaml"
            text = claims_file.read_text(encoding="utf-8")
            # Replace DUI-ASSOC-001 block with legal DESIGN + empty evidence_refs; lint_claims passes,
            # but Stage E exit gate must reject DESIGN assurance on a required Stage E ID.
            pattern = re.compile(
                r"- requirement_id: DUI-ASSOC-001\n.*?  source: [^\n]+\n",
                re.S,
            )
            replacement = (
                "- requirement_id: DUI-ASSOC-001\n"
                "  stage: E\n"
                "  status: NOT_STARTED\n"
                "  implementation_refs: []\n"
                "  test_refs: []\n"
                "  evidence_refs: []\n"
                "  assurance: DESIGN\n"
                "  limitations:\n"
                '    - "Tampered to DESIGN"\n'
                '  source: "stage-e-progress.md §E.2.2 E5"\n'
            )
            mutated, count = pattern.subn(replacement, text, count=1)
            self.assertEqual(count, 1)
            claims_file.write_text(mutated, encoding="utf-8")
            res = audit_strict(repo)
            self.assertFalse(res["ok"])
            self.assertTrue(any("DUI-ASSOC-001" in e and "DESIGN" in e for e in res["errors"]))
        finally:
            tmp.cleanup()

    def test_09_negative_open_p0_risk_rejected(self) -> None:
        tmp, repo = self._clone_sim_repo()
        try:
            risks_file = repo / "docs/management/risks.yaml"
            p0_block = render_risks_candidates_yaml(
                [
                    {
                        "id": "RISK-E-99",
                        "category": "security-bypass",
                        "severity": "P0",
                        "status": "open",
                        "description": "Injected open P0 risk",
                        "mitigation": "None",
                        "evidence_refs": [],
                        "review_point": "Immediate",
                    }
                ]
            )
            with risks_file.open("a", encoding="utf-8", newline="\n") as fh:
                fh.write("\n" + p0_block)
            res = audit_strict(repo)
            self.assertFalse(res["ok"])
            self.assertTrue(any("RISK-E-99" in e and "P0" in e for e in res["errors"]))
        finally:
            tmp.cleanup()

    def test_10_negative_missing_stage_e_risk_rejected(self) -> None:
        tmp, repo = self._clone_sim_repo()
        try:
            risks_file = repo / "docs/management/risks.yaml"
            text = risks_file.read_text(encoding="utf-8")
            risks_file.write_text(
                text.replace("- id: RISK-E-04", "- id: RISK-E-94"),
                encoding="utf-8",
            )
            res = audit_strict(repo)
            self.assertFalse(res["ok"])
            self.assertIn("RISK-E-04", res["missing"]["stage_e_risks"])
        finally:
            tmp.cleanup()

    def test_11_negative_unindexed_stage_e_evidence_rejected(self) -> None:
        tmp, repo = self._clone_sim_repo()
        try:
            rogue = repo / "docs/evidence/stage-e/e-rogue-unindexed-001.md"
            rogue.write_text("# Rogue Unindexed Evidence\n", encoding="utf-8")
            res = audit_strict(repo)
            self.assertFalse(res["ok"])
            self.assertIn(
                "docs/evidence/stage-e/e-rogue-unindexed-001.md",
                res["missing"]["unindexed_stage_e_evidence"],
            )
        finally:
            tmp.cleanup()

    def test_12_negative_emoji_and_broken_relative_markdown_link_rejected(self) -> None:
        tmp, repo = self._clone_sim_repo()
        try:
            target_md = repo / "docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md"
            rocket_char = chr(0x1F680)
            with target_md.open("a", encoding="utf-8", newline="\n") as fh:
                fh.write(f"\nTamper line {rocket_char} and [broken](./nonexistent-file-xyz.md)\n")
            res = audit_strict(repo)
            self.assertFalse(res["ok"])
            self.assertTrue(any("[hygiene:emoji]" in e for e in res["errors"]))
            self.assertTrue(any("[hygiene:link]" in e for e in res["errors"]))
        finally:
            tmp.cleanup()

    def test_13_verifier_source_contains_zero_pictographic_emoji(self) -> None:
        src_text = Path(__file__).resolve().read_text(encoding="utf-8")
        self.assertEqual(PICTOGRAPHIC_EMOJI_RE.findall(src_text), [])


def _print_human_report(report: dict[str, Any]) -> None:
    mode = report["mode"]
    status = "PASS" if report["ok"] else "FAIL"
    if mode == "strict":
        c = report["counts"]
        print(
            f"[stage-e-ledger:{mode}] {status} | "
            f"Stage E reqs={c['total_normative_requirements']} (E.1.1={c['e11_normative_requirements']} + E.2.2={c['e22_additional_requirements']}), "
            f"work_packages={c['work_packages']}, "
            f"claims={c['claims_total']}, risks={c['risks_total']} (open_P0={c['open_p0_risks']}), "
            f"evidence={c['evidence_index_total']}/{c['evidence_on_disk_total']} "
            f"(stage-e={c['stage_e_evidence_indexed']}/{c['stage_e_evidence_on_disk']}), "
            f"md_scanned={c['markdown_files_scanned']} (links={c['relative_links_checked']})"
        )
    else:
        b = report["baseline"]
        cl = report["classification"]
        sim = report["simulated_post_backfill"]["counts"]
        print(
            f"[stage-e-ledger:{mode}] {status}\n"
            f"  baseline : claims={b['claims_total']}, risks={b['risks_total']} (open_P0={b['open_p0_risks']}), "
            f"evidence={b['evidence_indexed']}/{b['evidence_on_disk']} "
            f"(pending_w81_unindexed={len(b['pending_w81_unindexed_on_disk'])})\n"
            f"  classify : normative_reqs={cl['total_normative_requirements']} (44 E.1.1 + 1 E.2.2), "
            f"work_packages={cl['work_packages']}, "
            f"earlier_stage={cl['registered_earlier_stage_count']}, "
            f"to_backfill_claims={cl['candidate_claim_entries_to_add']}, "
            f"to_backfill_risks={cl['candidate_risk_entries_to_add']}, "
            f"to_backfill_evidence={cl['candidate_evidence_entries_to_add']}\n"
            f"  simulated: claims={sim['claims_total']}, risks={sim['risks_total']} (open_P0={sim['open_p0_risks']}), "
            f"evidence={sim['evidence_index_total']}/{sim['evidence_on_disk_total']} "
            f"(stage-e={sim['stage_e_evidence_indexed']}/{sim['stage_e_evidence_on_disk']}), "
            f"errors={report['simulated_post_backfill']['error_count']}"
        )
    for err in report["errors"]:
        print(f"ERROR {err}")


def run_self_tests() -> int:
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(StageELedgerCoverageSelfTests)
    runner = unittest.TextTestRunner(verbosity=2)
    result = runner.run(suite)
    return 0 if result.wasSuccessful() else 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Stage E ledger-schema v1.3 mechanical coverage auditor & candidate backfill generator"
    )
    parser.add_argument(
        "--root",
        default=str(REPO_ROOT),
        help="Repository root directory (defaults to parent of tools/)",
    )
    parser.add_argument(
        "--mode",
        choices=("strict", "pre-backfill"),
        default="strict",
        help="Verification mode: 'strict' (post-backfill hard gate) or 'pre-backfill' (W81-E7B simulation gate)",
    )
    parser.add_argument(
        "--emit-candidates",
        action="store_true",
        help="Emit canonical YAML v1.3 candidate backfill blocks to stdout",
    )
    parser.add_argument(
        "--section",
        choices=("all", "evidence", "claims", "risks"),
        default="all",
        help="Candidate YAML section to emit when --emit-candidates is set",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run built-in unittest verification suite (>= 12 positive and negative tamper tests)",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit machine-readable JSON audit report",
    )
    args = parser.parse_args(argv)

    if args.self_test:
        return run_self_tests()

    root = Path(args.root).resolve()

    if args.emit_candidates:
        cand = generate_candidate_data(root)
        if args.section == "all":
            sections = [
                ("# === BEGIN evidence-index.yaml CANDIDATE APPEND ===", cand["yaml"]["evidence"]),
                ("# === BEGIN claims.yaml CANDIDATE APPEND ===", cand["yaml"]["claims"]),
                ("# === BEGIN risks.yaml CANDIDATE APPEND ===", cand["yaml"]["risks"]),
            ]
            out = "\n".join(f"{hdr}\n{body}".rstrip() for hdr, body in sections) + "\n"
            sys.stdout.write(out)
        else:
            sys.stdout.write(cand["yaml"][args.section])
        return 0

    report = audit_strict(root) if args.mode == "strict" else audit_pre_backfill(root)
    if args.json:
        print(json.dumps(report, ensure_ascii=False, indent=2))
    else:
        _print_human_report(report)
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
