"""LinkAtom：等价/矛盾等语义关系断言也是原子（议题 22 Q1）。

- 关系类型闭集五值（EQUIVALENT/CONTRADICTS/ENTAILS/SUPPORTS/REFINES），集外值在边界被拒绝
  （议题 24 枚举冻结的运行时体现）。
- 等价关系是信念流上的图，不是身份映射：原子保持不可变，错误判断用墓碑撤回。
- 判断本身有 provenance（judgment{by, method}）；矛盾判断共存，裁决归用户态（议题 12）。
"""

from __future__ import annotations

from .model import (
    AtomId,
    AtomView,
    JudgmentMethod,
    LinkRelation,
    parse_enum,
)
from .store import AtomStore


def write_link(
    store: AtomStore,
    relation: LinkRelation | str,
    a: AtomId,
    b: AtomId,
    *,
    by: str,
    method: JudgmentMethod | str,
    source: str,
    content: str = "",
) -> AtomId:
    """写入 link atom。relation/method 接受原始字符串以演示闭集边界：集外值抛 ClosedSetViolation。
    """
    rel = relation if isinstance(relation, LinkRelation) else parse_enum(LinkRelation, relation)
    meth = method if isinstance(method, JudgmentMethod) else parse_enum(JudgmentMethod, method)
    return store._write_link_atom(rel, a, b, by=by, method=meth, source=source, content=content)


def links_between(store: AtomStore, a: AtomId, b: AtomId) -> list[AtomView]:
    """两端点间所有存活 link（矛盾判断共存，不做合并/裁决——修订 6）。"""
    return [
        view
        for view in _all_links(store)
        if view.endpoints is not None and set(view.endpoints) == {a, b}
    ]


def refinement_chain(store: AtomStore, spec_id: AtomId) -> list[AtomId]:
    """规格精化链：从 spec_id 出发沿 REFINES link 找后续版本（v2 REFINES v1）。"""
    chain = [spec_id]
    current = spec_id
    visited = {spec_id}
    while True:
        nxt = next(
            (
                view.endpoints[0]
                for view in _all_links(store)
                if view.relation is LinkRelation.REFINES
                and view.endpoints is not None
                and view.endpoints[1] == current
                and view.endpoints[0] not in visited
            ),
            None,
        )
        if nxt is None:
            return chain
        chain.append(nxt)
        visited.add(nxt)
        current = nxt


def _all_links(store: AtomStore) -> list[AtomView]:
    """所有存活的 link atom（tombstone 优先：被撤回的 link 不出现在图中）。"""
    rows = store._db.execute("SELECT id FROM atoms WHERE kind = 'link'").fetchall()
    views: list[AtomView] = []
    for row in rows:
        view = store.get(AtomId(row["id"]))
        if view is not None:
            views.append(view)
    return views
