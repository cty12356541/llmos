#!/usr/bin/env python3
"""lint_claims.py 测试（零依赖，stdlib unittest）。

跑法：python3 scripts/test_lint_claims.py（或仓库根 python3 -m unittest scripts.test_lint_claims）

覆盖：
  1. canonical YAML 子集解析器（结构、注释、引号转义、保留结构拒绝）；
  2. 装有 pyyaml 时，三份真实台账文件两种解析器结果等价（skipOtherwise）；
  3. lint 规则逐条：refs 可解析、枚举、Claim≤Evidence 格、DESIGN 空引用、
     PARTIAL_PASS limitations 非空、索引双向一致、未知字段/重复 ID、风险枚举；
  4. 真实仓库台账自检（self-run gate：新增证据文件未登记索引即失败）。
"""
import importlib.util
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "lint_claims.py"
REPO_ROOT = Path(__file__).resolve().parent.parent

_spec = importlib.util.spec_from_file_location("lint_claims", SCRIPT)
lint = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(lint)

GOOD_CLAIMS = """\
# 注释行应被忽略
- requirement_id: TEST-001
  stage: B
  status: DONE
  implementation_refs:
    - crates/demo
  test_refs:
    - 3d81a90
  evidence_refs:
    - docs/evidence/stage-b/demo-001.md
  assurance: H2
  limitations: []
  source: "stage-b-progress.md §3 TEST-001 行（base 3d81a90）"

- requirement_id: TEST-002
  stage: B
  status: IN_PROGRESS
  implementation_refs: []
  test_refs: []
  evidence_refs: []
  assurance: DESIGN
  limitations:
    - "仅有设计"
  source: "stage-b-progress.md §3 TEST-002 行（base 3d81a90）"
"""

GOOD_RISKS = """\
- id: RISK-B-01
  category: security-bypass
  severity: P1
  status: partial
  description: "旁路风险"
  mitigation: "已有缓解"
  evidence_refs:
    - docs/evidence/stage-b/demo-001.md
  review_point: "阶段 B 退出评审门"

- id: RISK-B-02
  category: single-person-dependency
  severity: P2
  status: open
  description: "单人依赖"
"""

GOOD_INDEX = """\
- id: demo-001
  path: docs/evidence/stage-b/demo-001.md
  title: "DEMO-001：演示证据"
  scope: "演示证据"
  assurance: H3
  date: "2026-09-20"
"""


def make_repo(**overrides) -> Path:
    """构造最小台账 fixture 仓库；overrides 覆盖三份 yaml。"""
    root = Path(tempfile.mkdtemp(prefix="lint-claims-test-"))
    mgmt = root / "docs/management"
    ev = root / "docs/evidence/stage-b"
    mgmt.mkdir(parents=True)
    ev.mkdir(parents=True)
    (ev / "demo-001.md").write_text("# DEMO-001\n", encoding="utf-8")
    (root / "crates/demo").mkdir(parents=True)
    (mgmt / "claims.yaml").write_text(overrides.get("claims", GOOD_CLAIMS), encoding="utf-8")
    (mgmt / "risks.yaml").write_text(overrides.get("risks", GOOD_RISKS), encoding="utf-8")
    (mgmt / "evidence-index.yaml").write_text(overrides.get("index", GOOD_INDEX), encoding="utf-8")
    return root


def errors(root: Path):
    findings, _ = lint.run(root)
    return [f for f in findings if f[0] == "ERROR"]


class ParserTests(unittest.TestCase):
    def test_structure_and_comments(self):
        doc = lint.parse_canonical_yaml(GOOD_CLAIMS)
        self.assertIsInstance(doc, list)
        self.assertEqual(doc[0]["requirement_id"], "TEST-001")
        self.assertEqual(doc[0]["implementation_refs"], ["crates/demo"])
        self.assertEqual(doc[1]["limitations"], ["仅有设计"])
        self.assertEqual(lint.parse_canonical_yaml("# 只有注释\n"), None)

    def test_quoted_escapes(self):
        doc = lint.parse_canonical_yaml('- id: t\n  k: "a\\"b\\\\c"\n')
        self.assertEqual(doc[0]["k"], 'a"b\\c')

    def test_rejects_bad_indent_and_reserved_plain(self):
        with self.assertRaises(lint.YamlError):
            lint.parse_canonical_yaml("- id: t\n   k: v\n")  # 缩进不一致
        with self.assertRaises(lint.YamlError):
            lint.parse_canonical_yaml('- id: t\n  k: a: b\n')  # 裸标量含冒号
        with self.assertRaises(lint.YamlError):
            lint.parse_canonical_yaml('- id: t\n  k: "未闭合\n')

    @unittest.skipUnless(importlib.util.find_spec("yaml") is not None, "pyyaml 未安装")
    def test_pyyaml_equivalence_on_repo_files(self):
        import yaml
        for rel in ("docs/management/claims.yaml", "docs/management/risks.yaml",
                    "docs/management/evidence-index.yaml"):
            path = REPO_ROOT / rel
            if not path.exists():
                continue
            text = path.read_text(encoding="utf-8")
            self.assertEqual(yaml.safe_load(text), lint.parse_canonical_yaml(text), rel)


class GoodFixtureTests(unittest.TestCase):
    def test_good_repo_passes(self):
        errs = errors(make_repo())
        self.assertEqual(errs, [])

    def test_commit_shaped_ref_accepted(self):
        root = make_repo(claims=GOOD_CLAIMS.replace("- crates/demo", "- abc1234"))
        self.assertEqual(errors(root), [])


class LintRuleTests(unittest.TestCase):
    def _expect(self, marker, **overrides):
        errs = errors(make_repo(**overrides))
        self.assertTrue(any(marker in f[2] for f in errs),
                        f"期望发现含 {marker!r}，实际: {errs}")

    def test_evidence_ref_not_indexed(self):
        claims = GOOD_CLAIMS.replace(
            "docs/evidence/stage-b/demo-001.md",
            "docs/evidence/stage-b/other-999.md")
        self._expect("未收录进 evidence-index", claims=claims)

    def test_evidence_ref_outside_stage_b_dir(self):
        claims = GOOD_CLAIMS.replace(
            "docs/evidence/stage-b/demo-001.md",
            "docs/management/README.md")
        self._expect("越界", claims=claims)

    def test_impl_ref_unresolvable(self):
        self._expect("不可解析",
                     claims=GOOD_CLAIMS.replace("- crates/demo", "- crates/nope"))

    def test_commit_ref_accepted(self):
        root = make_repo(claims=GOOD_CLAIMS.replace("- crates/demo", "- abc1234"))
        self.assertEqual(errors(root), [])

    def test_bad_status_enum(self):
        self._expect("status 非法", claims=GOOD_CLAIMS.replace("status: DONE", "status: PASS"))

    def test_bad_assurance_enum(self):
        self._expect("assurance 非法", claims=GOOD_CLAIMS.replace("assurance: H2", "assurance: H9"))

    def test_non_design_claim_requires_evidence(self):
        claims = GOOD_CLAIMS.replace(
            "  evidence_refs:\n    - docs/evidence/stage-b/demo-001.md",
            "  evidence_refs: []")
        self._expect("至少需要 1 条 evidence_ref", claims=claims)

    def test_claim_exceeds_evidence_support(self):
        self._expect("Claim≤Evidence 违例",
                     claims=GOOD_CLAIMS.replace("assurance: H2", "assurance: H5"))

    def test_design_claim_with_evidence(self):
        claims = GOOD_CLAIMS.replace(
            "  evidence_refs: []\n  assurance: DESIGN",
            "  evidence_refs:\n    - docs/evidence/stage-b/demo-001.md\n  assurance: DESIGN")
        self._expect("DESIGN 时 evidence_refs 必须为空", claims=claims)

    def test_partial_pass_requires_limitations(self):
        claims = GOOD_CLAIMS.replace("status: DONE", "status: PARTIAL_PASS")
        self._expect("limitations 不得为空", claims=claims)

    def test_poc_evidence_caps_claim_at_h3(self):
        claims = GOOD_CLAIMS.replace("assurance: H2", "assurance: H4")
        index = GOOD_INDEX.replace("assurance: H3", "assurance: POC")
        self._expect("Claim≤Evidence 违例", claims=claims, index=index)

    def test_unknown_claim_field_rejected(self):
        self._expect("未知字段",
                     claims=GOOD_CLAIMS.replace("  source:", "  extra: x\n  source:"))

    def test_duplicate_requirement_id(self):
        self._expect("requirement_id 重复",
                     claims=GOOD_CLAIMS + GOOD_CLAIMS[GOOD_CLAIMS.index("- requirement_id: TEST-001"):])

    def test_unindexed_file_on_disk(self):
        root = make_repo()
        (root / "docs/evidence/stage-b/demo-002.md").write_text("# x\n", encoding="utf-8")
        errs = errors(root)
        self.assertTrue(any("目录文件未收录" in f[2] for f in errs), errs)

    def test_index_path_missing_on_disk(self):
        self._expect("指向不存在的文件",
                     index=GOOD_INDEX.replace("demo-001.md", "gone-001.md"))

    def test_index_bad_date(self):
        self._expect("date 必须为 YYYY-MM-DD",
                     index=GOOD_INDEX.replace('date: "2026-09-20"', 'date: "2026-9-20"'))

    def test_index_design_assurance_rejected(self):
        self._expect("assurance 非法",
                     index=GOOD_INDEX.replace("assurance: H3", "assurance: DESIGN"))

    def test_risk_bad_severity(self):
        self._expect("severity 非法", risks=GOOD_RISKS.replace("severity: P1", "severity: P3"))

    def test_risk_bad_category(self):
        self._expect("category 非法",
                     risks=GOOD_RISKS.replace("security-bypass", "alien-risk"))

    def test_risk_bad_status(self):
        self._expect("status 非法", risks=GOOD_RISKS.replace("status: partial", "status: maybe"))

    def test_risk_evidence_must_be_indexed(self):
        self._expect("未收录进 evidence-index",
                     risks=GOOD_RISKS.replace("demo-001.md", "demo-777.md"))

    def test_p0_open_is_info_not_error(self):
        root = make_repo(risks=GOOD_RISKS.replace("severity: P1", "severity: P0"))
        findings, _ = lint.run(root)
        self.assertTrue(any(f[0] == "INFO" and "P0" in f[2] for f in findings))
        self.assertEqual([f for f in findings if f[0] == "ERROR"], [])


class RealRepoTests(unittest.TestCase):
    def test_real_ledger_lints_clean(self):
        """仓库自检门：三份台账 + 索引双向一致必须全绿。"""
        findings, stats = lint.run(REPO_ROOT)
        errs = [f for f in findings if f[0] == "ERROR"]
        self.assertEqual(errs, [], f"真实台账存在 lint 错误: {errs}")
        self.assertGreater(stats["index"], 0)
        self.assertEqual(stats["index"], stats["on_disk"])


if __name__ == "__main__":
    unittest.main()
