#!/usr/bin/env python3
"""claims/risks/evidence-index 机器台账 lint v1（schema 见 docs/management/ledger-schema.md）。

零依赖（stdlib）：内置 canonical YAML 子集解析器；环境装有 pyyaml 时交叉验证两者解析一致。

检查项：
  (a) refs 可解析——repo 相对路径存在或 commit 串（7–40 hex）；evidence_refs 额外
      限定为 evidence-index 已收录的 docs/evidence/stage-b/ 路径；
  (b) 枚举/必填字段/未知字段（v1 FROZEN）/ID 唯一/引用文法合法性；
  (c) Claim≤Evidence——DESIGN ⇒ 空 evidence_refs；非 DESIGN ⇒ ≥1 条 evidence 且
      rank(assurance) ≤ max(rank(被引索引条目))，rank(POC)=3、rank(Hn)=n；
  (d) evidence-index 与 docs/evidence/stage-b/ 目录双向一致。

跑法：python3 scripts/lint_claims.py [--root REPO_ROOT]；退出码 0=PASS，1=存在 ERROR。
"""
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

CLAIMS_PATH = "docs/management/claims.yaml"
RISKS_PATH = "docs/management/risks.yaml"
INDEX_PATH = "docs/management/evidence-index.yaml"
EVIDENCE_DIR = "docs/evidence/stage-b"

CLAIM_STATUSES = {"DONE", "PARTIAL_PASS", "IN_PROGRESS", "READY", "BLOCKED", "NOT_STARTED"}
CLAIM_ASSURANCES = {"DESIGN", "H0", "H1", "H2", "H3", "H4", "H5", "H6", "H7", "H8"}
EVIDENCE_ASSURANCES = {"POC", "H0", "H1", "H2", "H3", "H4", "H5", "H6", "H7", "H8"}
RISK_CATEGORIES = {
    "security-bypass", "durable-state-loss", "resource-oversell", "cancel-effect-unknown",
    "premature-format-freeze", "runtime-ui-lockin", "scale-impersonation",
    "single-person-dependency", "third-party-supply-chain",
}
RISK_SEVERITIES = {"P0", "P1", "P2"}
RISK_STATUSES = {"open", "partial", "mitigated", "closed"}

CLAIM_KEYS = {
    "requirement_id", "stage", "status", "implementation_refs", "test_refs",
    "evidence_refs", "assurance", "limitations", "source",
}
RISK_REQUIRED_KEYS = {"id", "category", "severity", "status", "description"}
RISK_OPTIONAL_KEYS = {"mitigation", "evidence_refs", "review_point"}
INDEX_KEYS = {"id", "path", "title", "scope", "assurance", "date"}

# Claim≤Evidence 支撑格：POC 证据最多支撑 H3 claim。
SUPPORT_RANK = {"POC": 3, "H0": 0, "H1": 1, "H2": 2, "H3": 3, "H4": 4,
                "H5": 5, "H6": 6, "H7": 7, "H8": 8}
CLAIM_RANK = {"H0": 0, "H1": 1, "H2": 2, "H3": 3, "H4": 4,
              "H5": 5, "H6": 6, "H7": 7, "H8": 8}

COMMIT_RE = re.compile(r"^[0-9a-f]{7,40}$")
DATE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
KEY_RE = re.compile(r"^([A-Za-z_][A-Za-z0-9_-]*):(?: (.*))?$")


class YamlError(ValueError):
    """canonical YAML 子集解析失败。"""


# ---------------------------------------------------------------------------
# canonical YAML 子集解析器（顶层块序列/块映射；双引号或受限裸标量；[] 空列表）
# ---------------------------------------------------------------------------

def _scalar(tok: str) -> str:
    tok = tok.strip()
    if tok.startswith('"'):
        if tok == '""':
            return ""
        body, out, i, closed = tok[1:], [], 0, False
        while i < len(body):
            ch = body[i]
            if ch == "\\":
                if i + 1 >= len(body) or body[i + 1] not in '"\\':
                    raise YamlError(f"不支持的转义: {body[i:i + 2]}")
                out.append(body[i + 1])
                i += 2
            elif ch == '"':
                if i != len(body) - 1:
                    raise YamlError(f"引号后有残留内容: {tok[:40]}…")
                closed = True
                i += 1
            else:
                out.append(ch)
                i += 1
        if not closed:
            raise YamlError(f"引号标量不成对: {tok[:40]}…")
        return "".join(out)
    if ": " in tok or tok.endswith(":") or " #" in tok or tok.startswith(("&", "*", "!")):
        raise YamlError(f"裸标量含保留结构（需双引号）: {tok[:40]}…")
    return tok


def _parse_block(lines: list[tuple[int, str]], i: int, indent: int):
    if lines[i][1] == "-" or lines[i][1].startswith("- "):
        return _parse_seq(lines, i, indent)
    return _parse_map(lines, i, indent)


def _parse_seq(lines: list[tuple[int, str]], i: int, indent: int):
    out = []
    while i < len(lines) and lines[i][0] == indent and (
        lines[i][1] == "-" or lines[i][1].startswith("- ")
    ):
        rest = lines[i][1][1:].strip()
        if rest == "":
            i += 1
            if i >= len(lines) or lines[i][0] <= indent:
                raise YamlError("序列项后缺少嵌套块")
            val, i = _parse_block(lines, i, lines[i][0])
            out.append(val)
        elif KEY_RE.match(rest) and not rest.startswith('"'):
            # 映射起始于 dash 行：视为缩进 +2 的映射首行继续解析。
            merged = [(indent + 2, rest)] + lines[i + 1:]
            val, consumed = _parse_map(merged, 0, indent + 2)
            out.append(val)
            i += consumed
        else:
            out.append(_scalar(rest))
            i += 1
    if i < len(lines) and lines[i][0] > indent:
        raise YamlError(f"缩进不一致（第 {i} 个逻辑行）")
    return out, i


def _parse_map(lines: list[tuple[int, str]], i: int, indent: int):
    out: dict = {}
    key = None
    while i < len(lines) and lines[i][0] == indent:
        m = KEY_RE.match(lines[i][1])
        if not m:
            raise YamlError(f"非 key: value 行: {lines[i][1][:40]}…")
        key, rest = m.group(1), m.group(2)
        if rest is None or rest.strip() == "":
            i += 1
            if i < len(lines) and lines[i][0] > indent:
                val, i = _parse_block(lines, i, lines[i][0])
            else:
                raise YamlError(f"键 {key} 缺少值或嵌套块")
        elif rest.strip() == "[]":
            val, i = [], i + 1
        else:
            val, i = _scalar(rest), i + 1
        if key in out:
            raise YamlError(f"重复键: {key}")
        out[key] = val
    if i < len(lines) and lines[i][0] > indent:
        raise YamlError(f"缩进不一致（键 {key} 之后）")
    return out, i


def parse_canonical_yaml(text: str):
    lines = []
    for raw in text.splitlines():
        stripped = raw.strip()
        if not stripped or stripped.startswith("#"):
            continue
        indent = len(raw) - len(raw.lstrip(" "))
        if "\t" in raw[:indent]:
            raise YamlError("不得使用 Tab 缩进")
        lines.append((indent, stripped))
    if not lines:
        return None
    if lines[0][0] != 0:
        raise YamlError("顶层必须顶格")
    value, i = _parse_block(lines, 0, 0)
    if i != len(lines):
        raise YamlError(f"第 {i} 个逻辑行起内容无法归位")
    return value


# ---------------------------------------------------------------------------
# 校验
# ---------------------------------------------------------------------------

def _is_commit(ref: str) -> bool:
    return bool(COMMIT_RE.match(ref))


def _path_exists(root: Path, ref: str) -> bool:
    parts = ref.split("/")
    if ref.startswith("/") or ".." in parts or "" in parts:
        return False
    return (root / ref).exists()


def _check_ref_list(where: str, refs, field: str, root: Path, index_paths: set,
                    findings: list, evidence_only: bool) -> list:
    """返回可解析为索引条目的 evidence 路径（供格校验）。"""
    resolved = []
    if not isinstance(refs, list):
        findings.append(("ERROR", where, f"{field} 必须为列表"))
        return resolved
    for ref in refs:
        if not isinstance(ref, str) or not ref:
            findings.append(("ERROR", where, f"{field} 含非字符串/空引用"))
            continue
        if evidence_only:
            if not ref.startswith(EVIDENCE_DIR + "/"):
                findings.append(("ERROR", where, f"evidence_refs 越界（仅允许 {EVIDENCE_DIR}/）: {ref}"))
            elif ref not in index_paths:
                findings.append(("ERROR", where, f"evidence_refs 未收录进 evidence-index: {ref}"))
            else:
                resolved.append(ref)
        elif not (_path_exists(root, ref) or _is_commit(ref)):
            findings.append(("ERROR", where, f"{field} 引用不可解析（路径不存在且非 commit 串）: {ref}"))
    return resolved


def _require_str_list(where: str, val, field: str, findings: list) -> None:
    if not isinstance(val, list) or any(not isinstance(x, str) or not x for x in val):
        findings.append(("ERROR", where, f"{field} 必须为非空字符串列表"))


def check_claims(claims, root: Path, index_paths: set, index_rank: dict, findings: list) -> dict:
    counts: dict = {}
    if not isinstance(claims, list):
        findings.append(("ERROR", "claims", "顶层必须为条目列表"))
        return counts
    seen = set()
    for c in claims:
        if not isinstance(c, dict):
            findings.append(("ERROR", "claims", "条目必须为映射"))
            continue
        rid = c.get("requirement_id", "<missing>")
        where = f"claims/{rid}"
        unknown = set(c) - CLAIM_KEYS
        missing = CLAIM_KEYS - set(c)
        if unknown:
            findings.append(("ERROR", where, f"未知字段（v1 FROZEN）: {sorted(unknown)}"))
        if missing:
            findings.append(("ERROR", where, f"缺少必填字段: {sorted(missing)}"))
            continue
        if rid in seen:
            findings.append(("ERROR", where, "requirement_id 重复"))
        seen.add(rid)
        status, assurance = c["status"], c["assurance"]
        counts[status] = counts.get(status, 0) + 1
        if status not in CLAIM_STATUSES:
            findings.append(("ERROR", where, f"status 非法: {status}"))
        if assurance not in CLAIM_ASSURANCES:
            findings.append(("ERROR", where, f"assurance 非法: {assurance}"))
        if c["stage"] != "B":
            findings.append(("ERROR", where, f"stage 非法（v1 仅 B）: {c['stage']}"))
        if not isinstance(c["source"], str) or not c["source"].strip():
            findings.append(("ERROR", where, "source 必须为非空字符串（断言出处）"))
        _require_str_list(where, c["limitations"], "limitations", findings)
        if status == "PARTIAL_PASS" and isinstance(c["limitations"], list) \
                and not c["limitations"]:
            findings.append(("ERROR", where, "status=PARTIAL_PASS 时 limitations 不得为空"))
        _require_str_list(where, c["implementation_refs"], "implementation_refs", findings)
        _require_str_list(where, c["test_refs"], "test_refs", findings)
        cited = _check_ref_list(where, c["evidence_refs"], "evidence_refs", root,
                                index_paths, findings, evidence_only=True)
        _check_ref_list(where, c["implementation_refs"], "implementation_refs", root,
                        index_paths, findings, evidence_only=False)
        _check_ref_list(where, c["test_refs"], "test_refs", root,
                        index_paths, findings, evidence_only=False)
        # (c) Claim≤Evidence
        if assurance == "DESIGN":
            if isinstance(c["evidence_refs"], list) and c["evidence_refs"]:
                findings.append(("ERROR", where, "assurance=DESIGN 时 evidence_refs 必须为空"))
        elif assurance in CLAIM_RANK:
            if isinstance(c["evidence_refs"], list) and not c["evidence_refs"]:
                findings.append(("ERROR", where, f"assurance={assurance} 为非 DESIGN claim，至少需要 1 条 evidence_ref"))
            ranks = [index_rank[p] for p in cited if p in index_rank]
            if ranks and CLAIM_RANK[assurance] > max(ranks):
                findings.append((
                    "ERROR", where,
                    f"Claim≤Evidence 违例：assurance={assurance} (rank {CLAIM_RANK[assurance]})"
                    f" > 被引证据最高支撑 rank {max(ranks)}",
                ))
    return counts


def check_risks(risks, root: Path, index_paths: set, findings: list) -> dict:
    counts: dict = {}
    if not isinstance(risks, list):
        findings.append(("ERROR", "risks", "顶层必须为条目列表"))
        return counts
    seen = set()
    for r in risks:
        if not isinstance(r, dict):
            findings.append(("ERROR", "risks", "条目必须为映射"))
            continue
        rid = r.get("id", "<missing>")
        where = f"risks/{rid}"
        unknown = set(r) - RISK_REQUIRED_KEYS - RISK_OPTIONAL_KEYS
        missing = RISK_REQUIRED_KEYS - set(r)
        if unknown:
            findings.append(("ERROR", where, f"未知字段（v1 FROZEN）: {sorted(unknown)}"))
        if missing:
            findings.append(("ERROR", where, f"缺少必填字段: {sorted(missing)}"))
            continue
        if rid in seen:
            findings.append(("ERROR", where, "id 重复"))
        seen.add(rid)
        sev, status = r["severity"], r["status"]
        key = f"{sev}/{status}"
        counts[key] = counts.get(key, 0) + 1
        if r["category"] not in RISK_CATEGORIES:
            findings.append(("ERROR", where, f"category 非法: {r['category']}"))
        if sev not in RISK_SEVERITIES:
            findings.append(("ERROR", where, f"severity 非法: {sev}"))
        if status not in RISK_STATUSES:
            findings.append(("ERROR", where, f"status 非法: {status}"))
        if not isinstance(r["description"], str) or not r["description"].strip():
            findings.append(("ERROR", where, "description 必须为非空字符串"))
        if sev == "P0" and status in ("open", "partial"):
            findings.append((
                "INFO", where,
                "P0 风险未闭环——按 README §9 阻止阶段退出（信息提示，不计错误）",
            ))
        if "evidence_refs" in r:
            _check_ref_list(where, r["evidence_refs"], "evidence_refs", root,
                            index_paths, findings, evidence_only=True)
    return counts


def check_index(index, root: Path, findings: list):
    if not isinstance(index, list):
        findings.append(("ERROR", "evidence-index", "顶层必须为条目列表"))
        return {}, set()
    seen_ids, seen_paths, rank_by_path = set(), set(), {}
    for e in index:
        if not isinstance(e, dict):
            findings.append(("ERROR", "evidence-index", "条目必须为映射"))
            continue
        where = f"evidence-index/{e.get('id', e.get('path', '<missing>'))}"
        unknown = set(e) - INDEX_KEYS
        missing = INDEX_KEYS - set(e)
        if unknown:
            findings.append(("ERROR", where, f"未知字段（v1 FROZEN）: {sorted(unknown)}"))
        if missing:
            findings.append(("ERROR", where, f"缺少必填字段: {sorted(missing)}"))
            continue
        eid, path = e["id"], e["path"]
        if eid in seen_ids:
            findings.append(("ERROR", where, "id 重复"))
        seen_ids.add(eid)
        if path in seen_paths:
            findings.append(("ERROR", where, f"path 重复: {path}"))
        seen_paths.add(path)
        if not path.startswith(EVIDENCE_DIR + "/") or not path.endswith(".md"):
            findings.append(("ERROR", where, f"path 必须形如 {EVIDENCE_DIR}/<file>.md: {path}"))
        elif not (root / path).is_file():
            findings.append(("ERROR", where, f"path 指向不存在的文件: {path}"))
        if e["assurance"] not in EVIDENCE_ASSURANCES:
            findings.append(("ERROR", where, f"assurance 非法（不可为 DESIGN）: {e['assurance']}"))
        elif path not in rank_by_path and (root / path).is_file():
            rank_by_path[path] = SUPPORT_RANK[e["assurance"]]
        if not isinstance(e["date"], str) or not DATE_RE.match(e["date"]):
            findings.append(("ERROR", where, f"date 必须为 YYYY-MM-DD: {e['date']!r}"))
    # (d) 目录双向一致
    on_disk = {f"{EVIDENCE_DIR}/{p.name}" for p in sorted((root / EVIDENCE_DIR).glob("*.md"))} \
        if (root / EVIDENCE_DIR).is_dir() else set()
    for missing_in_index in sorted(on_disk - seen_paths):
        findings.append(("ERROR", "evidence-index", f"目录文件未收录: {missing_in_index}"))
    for orphan in sorted(seen_paths - on_disk):
        findings.append(("ERROR", "evidence-index", f"索引收录了目录外/不存在文件: {orphan}"))
    return rank_by_path, seen_paths


def _load(path: Path, findings: list, label: str):
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        findings.append(("ERROR", label, f"无法读取 {path}: {exc}"))
        return None
    try:
        return parse_canonical_yaml(text)
    except YamlError as exc:
        findings.append(("ERROR", label, f"canonical YAML 解析失败: {exc}"))
        return None


def run(root: Path) -> tuple[list, dict]:
    """返回 (findings, stats)；findings 为 (level, where, message) 列表。"""
    findings: list = []
    stats: dict = {}
    claims = _load(root / CLAIMS_PATH, findings, "claims")
    risks = _load(root / RISKS_PATH, findings, "risks")
    index = _load(root / INDEX_PATH, findings, "evidence-index")
    # 装有 pyyaml 时交叉验证（非依赖）。
    try:
        import yaml  # type: ignore
    except ImportError:
        yaml = None
    if yaml is not None:
        for label, rel in (("claims", CLAIMS_PATH), ("risks", RISKS_PATH), ("evidence-index", INDEX_PATH)):
            loaded = {"claims": claims, "risks": risks, "evidence-index": index}[label]
            if loaded is None:
                continue  # 解析失败已报告，不重复比对
            try:
                if yaml.safe_load((root / rel).read_text(encoding="utf-8")) != loaded:
                    findings.append(("ERROR", label, "pyyaml 与 canonical 子集解析结果不一致"))
            except OSError:
                pass  # 读取失败已在 _load 报告
    rank_by_path: dict = {}
    index_paths: set = set()
    if index is not None:
        rank_by_path, index_paths = check_index(index, root, findings)
    stats["claims"] = check_claims(claims, root, index_paths, rank_by_path, findings) if claims is not None else {}
    stats["risks"] = check_risks(risks, root, index_paths, findings) if risks is not None else {}
    stats["index"] = len(index) if isinstance(index, list) else 0
    stats["on_disk"] = len(list((root / EVIDENCE_DIR).glob("*.md"))) if (root / EVIDENCE_DIR).is_dir() else 0
    return findings, stats


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description="claims/risks/evidence-index 台账 lint v1")
    parser.add_argument("--root", default=str(REPO_ROOT), help="仓库根目录（默认脚本所在仓库）")
    args = parser.parse_args(argv)
    root = Path(args.root).resolve()
    findings, stats = run(root)
    for level, where, msg in findings:
        print(f"{level} [{where}] {msg}")
    claims_stat = stats.get("claims", {})
    risks_stat = stats.get("risks", {})
    print(
        f"claims: {sum(claims_stat.values())}"
        f" ({', '.join(f'{k} {v}' for k, v in sorted(claims_stat.items())) or '无'})"
        f" · risks: {sum(risks_stat.values())}"
        f" ({', '.join(f'{k} {v}' for k, v in sorted(risks_stat.items())) or '无'})"
        f" · evidence-index: {stats.get('index', 0)}/{stats.get('on_disk', 0)}"
    )
    errors = [f for f in findings if f[0] == "ERROR"]
    print(f"lint: {'FAIL' if errors else 'PASS'}（{len(errors)} 项 ERROR，{len(findings) - len(errors)} 项 INFO）")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
