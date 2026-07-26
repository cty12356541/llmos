"""SQLite 语义原子存储（议题 21 定案 + 议题 26 修订 4/6）。

不变式（全部被 tests/ 锁定）：
- 不可变：atoms 表只有 INSERT；UPDATE/DELETE 由触发器物理拒绝；"修改"= 新原子 + lineage。
- 墓碑撤回：retract 写入 tombstone 原子；查询时 tombstone 优先（修订 6），被撤回原子隐藏。
- 内容哈希去重：id = sha256(kind + content [+ target/endpoints])，同 content 重写入返回同 id。
- 内容不透明：content 在本模块中仅被哈希、原样写入、原样读出（test_content_never_parsed 静态锁定）。
- verified 写入限制（修订 4）：只有 deterministic / independent-verifier 能写 verified。
- 验证状态折叠：语义状态变更不改写原子，只追加 verdict 事件（议题 24 Q3），读出时折叠。
"""

from __future__ import annotations

import json
import sqlite3
import time
from collections import deque
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import NoReturn

from .errors import ForbiddenVerifiedWrite, ImmutableViolation, UnknownAtom
from .model import (
    VERIFIED_WRITERS,
    AssertionLevel,
    AtomId,
    AtomKind,
    AtomView,
    Judgment,
    JudgmentMethod,
    LinkRelation,
    Provenance,
    Validity,
    Verification,
    VerificationMethod,
    VerificationStatus,
    compute_atom_id,
    compute_chain_hash,
    parse_enum,
)

_SCHEMA = """
CREATE TABLE atoms (
  id            TEXT PRIMARY KEY,
  kind          TEXT NOT NULL,
  content       TEXT NOT NULL,
  assertion     TEXT,
  source        TEXT NOT NULL,
  lineage       TEXT NOT NULL,
  chain_hash    TEXT NOT NULL,
  created       REAL NOT NULL,
  ttl           REAL,
  signature     TEXT,
  target        TEXT,
  relation      TEXT,
  endpoint_a    TEXT,
  endpoint_b    TEXT,
  judgment_by   TEXT,
  judgment_method TEXT
);
CREATE TABLE verdicts (
  atom_id TEXT NOT NULL,
  status  TEXT NOT NULL,
  by      TEXT NOT NULL,
  method  TEXT NOT NULL,
  ts      REAL NOT NULL
);
CREATE TRIGGER atoms_no_update BEFORE UPDATE ON atoms
BEGIN SELECT RAISE(ABORT, 'semantic atoms are immutable'); END;
CREATE TRIGGER atoms_no_delete BEFORE DELETE ON atoms
BEGIN SELECT RAISE(ABORT, 'semantic atoms are immutable'); END;
"""


class AtomStore:
    """单文件 SQLite 语义原子存储（无服务端、无网络）。"""

    def __init__(self, path: str | Path = ":memory:", *, clock: Callable[[], float] = time.time):
        self._db = sqlite3.connect(str(path))
        self._db.row_factory = sqlite3.Row
        self._clock = clock
        self._db.executescript(_SCHEMA)

    def close(self) -> None:
        self._db.close()

    # ---- 写入（只增不改）----

    def write_claim(
        self,
        content: str,
        *,
        source: str,
        assertion: AssertionLevel,
        lineage: Iterable[AtomId] = (),
        ttl: float | None = None,
        signature: str | None = None,
    ) -> AtomId:
        atom_id = compute_atom_id(AtomKind.CLAIM, content)
        self._insert(atom_id, AtomKind.CLAIM, content, source=source,
                     assertion=assertion, lineage=tuple(lineage), ttl=ttl, signature=signature)
        return atom_id

    def retract(self, target: AtomId, *, source: str, reason: str = "") -> AtomId:
        """撤回 = 写入 tombstone 原子指向目标（议题 21 Q2）。tombstone 自身也是原子。"""
        if not self._exists(target):
            raise UnknownAtom(f"撤回目标不存在: {target}")
        tomb_id = compute_atom_id(AtomKind.TOMBSTONE, reason, target=target)
        self._insert(tomb_id, AtomKind.TOMBSTONE, reason, source=source,
                     assertion=None, lineage=(target,), target=target)
        return tomb_id

    def record_verdict(
        self,
        atom_id: AtomId,
        status: VerificationStatus,
        *,
        by: str,
        method: VerificationMethod,
    ) -> None:
        """语义状态变更 = 追加 verdict 事件（议题 24 Q3），不改写原子本体。

        修订 4：verified 只能由独立验证者或确定性 checker 写入；self-attested 被拒绝。
        """
        if not self._exists(atom_id):
            raise UnknownAtom(f"verdict 目标不存在: {atom_id}")
        if status is VerificationStatus.VERIFIED and method not in VERIFIED_WRITERS:
            allowed = sorted(m.value for m in VERIFIED_WRITERS)
            raise ForbiddenVerifiedWrite(
                f"verified 只能由 {allowed} 写入，收到 {method.value}"
            )
        self._db.execute(
            "INSERT INTO verdicts (atom_id, status, by, method, ts) VALUES (?,?,?,?,?)",
            (atom_id, status.value, by, method.value, self._clock()),
        )
        self._db.commit()

    def update_atom(self, *_args: object, **_kwargs: object) -> NoReturn:
        """议题 21 Q2：原子不可变。任何 update 尝试报错——修改 = 新原子 + lineage。"""
        raise ImmutableViolation("语义原子不可变：修改 = 新原子 + lineage 指向父原子")

    # ---- 查询 ----

    def get(self, atom_id: AtomId, *, now: float | None = None) -> AtomView | None:
        """tombstone 优先（修订 6）：被撤回原子隐藏；TTL 过期原子同样隐藏（遗忘挂钩）。"""
        row = self._db.execute("SELECT * FROM atoms WHERE id = ?", (atom_id,)).fetchone()
        if row is None or self.is_retracted(atom_id) or self._is_expired(row, now):
            return None
        return self._to_view(row)

    def verification_of(self, atom_id: AtomId) -> Verification:
        """有效验证状态 = verdict 事件流上的 fold（最新事件胜出）。"""
        row = self._db.execute(
            "SELECT status, by, method, ts FROM verdicts WHERE atom_id = ?"
            " ORDER BY ts DESC, rowid DESC LIMIT 1",
            (atom_id,),
        ).fetchone()
        if row is None:
            return Verification()
        return Verification(
            status=parse_enum(VerificationStatus, row["status"]),
            by=row["by"],
            method=parse_enum(VerificationMethod, row["method"]),
            timestamp=row["ts"],
        )

    def is_retracted(self, atom_id: AtomId) -> bool:
        return (
            self._db.execute(
                "SELECT 1 FROM atoms WHERE kind = ? AND target = ? LIMIT 1",
                (AtomKind.TOMBSTONE.value, atom_id),
            ).fetchone()
            is not None
        )

    def descendants(self, atom_id: AtomId) -> list[AtomId]:
        """前向遍历 lineage（接触追踪的存储层支撑）：所有把 atom_id 列为祖先的原子。"""
        children: dict[str, list[str]] = {}
        for row in self._db.execute("SELECT id, lineage FROM atoms"):
            for parent in json.loads(row["lineage"]):
                children.setdefault(parent, []).append(row["id"])
        seen: set[str] = set()
        queue: deque[str] = deque(children.get(str(atom_id), []))
        order: list[AtomId] = []
        while queue:
            current = queue.popleft()
            if current in seen:
                continue
            seen.add(current)
            order.append(AtomId(current))
            queue.extend(children.get(current, []))
        return order

    def recall(self, atom_id: AtomId, *, by: str,
               method: VerificationMethod = VerificationMethod.INDEPENDENT_VERIFIER,
               ) -> list[AtomId]:
        """召回 = 沿 lineage 把后代标记为 disputed（议题 21 3.1）；返回受影响原子。

        跳过已撤回原子与 tombstone 自身（墓碑是终态元数据，不再是信念内容）。
        """
        hit = [d for d in self.descendants(atom_id)
               if not self.is_retracted(d) and self._kind_of(d) is not AtomKind.TOMBSTONE]
        for descendant in hit:
            self.record_verdict(descendant, VerificationStatus.DISPUTED, by=by, method=method)
        return hit

    def expired(self, *, now: float | None = None) -> list[AtomId]:
        """按 TTL 过期查询（遗忘机制的存储层支撑）。"""
        moment = self._clock() if now is None else now
        return [
            AtomId(row["id"])
            for row in self._db.execute(
                "SELECT id FROM atoms WHERE ttl IS NOT NULL AND created + ttl <= ?", (moment,)
            )
            if not self.is_retracted(AtomId(row["id"]))
        ]

    def row_count(self) -> int:
        return self._db.execute("SELECT COUNT(*) FROM atoms").fetchone()[0]

    # ---- 内部 ----

    def _exists(self, atom_id: AtomId) -> bool:
        return (
            self._db.execute("SELECT 1 FROM atoms WHERE id = ?", (atom_id,)).fetchone() is not None
        )

    def _insert(
        self,
        atom_id: AtomId,
        kind: AtomKind,
        content: str,
        *,
        source: str,
        assertion: AssertionLevel | None,
        lineage: tuple[AtomId, ...],
        ttl: float | None = None,
        signature: str | None = None,
        target: AtomId | None = None,
        relation: LinkRelation | None = None,
        endpoints: tuple[AtomId, AtomId] | None = None,
        judgment: Judgment | None = None,
    ) -> None:
        parents = tuple(lineage)
        for parent in parents:
            if not self._exists(parent):
                raise UnknownAtom(f"lineage 父原子不存在: {parent}")
        chain_hash = compute_chain_hash(atom_id, [self._chain_hash_of(p) for p in parents])
        endpoint_a, endpoint_b = endpoints if endpoints is not None else (None, None)
        cur = self._db.execute(
            "INSERT OR IGNORE INTO atoms (id, kind, content, assertion, source, lineage,"
            " chain_hash, created, ttl, signature, target, relation, endpoint_a, endpoint_b,"
            " judgment_by, judgment_method) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (
                atom_id, kind.value, content,
                assertion.value if assertion is not None else None,
                source, json.dumps(list(parents)), chain_hash, self._clock(), ttl, signature,
                target, relation.value if relation is not None else None,
                endpoint_a, endpoint_b,
                judgment.by if judgment is not None else None,
                judgment.method.value if judgment is not None else None,
            ),
        )
        self._db.commit()
        _ = cur  # INSERT OR IGNORE：同 id 重复写入不产生新行（内容哈希去重）

    def _chain_hash_of(self, atom_id: AtomId) -> str:
        row = self._db.execute("SELECT chain_hash FROM atoms WHERE id = ?", (atom_id,)).fetchone()
        return row["chain_hash"]

    def _kind_of(self, atom_id: AtomId) -> AtomKind:
        row = self._db.execute("SELECT kind FROM atoms WHERE id = ?", (atom_id,)).fetchone()
        return parse_enum(AtomKind, row["kind"])

    def _is_expired(self, row: sqlite3.Row, now: float | None) -> bool:
        if row["ttl"] is None:
            return False
        moment = self._clock() if now is None else now
        return row["created"] + row["ttl"] <= moment

    def _to_view(self, row: sqlite3.Row) -> AtomView:
        endpoints = (
            (AtomId(row["endpoint_a"]), AtomId(row["endpoint_b"]))
            if row["endpoint_a"] is not None
            else None
        )
        judgment = (
            Judgment(by=row["judgment_by"],
                     method=parse_enum(JudgmentMethod, row["judgment_method"]))
            if row["judgment_by"] is not None
            else None
        )
        return AtomView(
            id=AtomId(row["id"]),
            kind=parse_enum(AtomKind, row["kind"]),
            content=row["content"],
            provenance=Provenance(
                source=row["source"],
                lineage=tuple(AtomId(p) for p in json.loads(row["lineage"])),
                chain_hash=row["chain_hash"],
            ),
            assertion=(parse_enum(AssertionLevel, row["assertion"])
                       if row["assertion"] is not None else None),
            validity=Validity(created=row["created"], ttl=row["ttl"]),
            verification=self.verification_of(AtomId(row["id"])),
            signature=row["signature"],
            target=AtomId(row["target"]) if row["target"] is not None else None,
            relation=(parse_enum(LinkRelation, row["relation"])
                      if row["relation"] is not None else None),
            endpoints=endpoints,
            judgment=judgment,
        )

    def _insert_spec_atom(self, goal: str, *, source: str,
                          lineage: tuple[AtomId, ...]) -> AtomId:
        """供 spec 模块调用：规格即原子（议题 23 Q3），goal 是不透明 content。"""
        spec_id = compute_atom_id(AtomKind.SPEC, goal)
        self._insert(spec_id, AtomKind.SPEC, goal, source=source,
                     assertion=None, lineage=lineage)
        return spec_id

    def _write_link_atom(
        self,
        relation: LinkRelation,
        a: AtomId,
        b: AtomId,
        *,
        by: str,
        method: JudgmentMethod,
        source: str,
        content: str = "",
    ) -> AtomId:
        """供 links 模块调用：link atom 也是原子（议题 22 Q1）。"""
        for endpoint in (a, b):
            if not self._exists(endpoint):
                raise UnknownAtom(f"link 端点不存在: {endpoint}")
        link_id = compute_atom_id(AtomKind.LINK, content, endpoints=(a, b),
                                  relation=relation.value)
        self._insert(link_id, AtomKind.LINK, content, source=source,
                     assertion=AssertionLevel.INFERENCE, lineage=(a, b),
                     relation=relation, endpoints=(a, b),
                     judgment=Judgment(by=by, method=method))
        return link_id
