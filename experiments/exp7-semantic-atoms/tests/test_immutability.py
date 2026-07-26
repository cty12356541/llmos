"""不变式：原子不可变（议题 21 Q2）——任何 update 报错；修改 = 新原子 + lineage。"""

from __future__ import annotations

import sqlite3

import pytest

from sematom import AssertionLevel, AtomStore, ImmutableViolation


def _claim(store: AtomStore, content: str = "v1 断言", **kw):
    return store.write_claim(content, source="agent-a",
                             assertion=AssertionLevel.INFERENCE, **kw)


def test_update_api_raises(store: AtomStore):
    atom_id = _claim(store)
    with pytest.raises(ImmutableViolation):
        store.update_atom(atom_id, content="篡改")


def test_raw_sql_update_blocked_by_trigger(store: AtomStore):
    atom_id = _claim(store)
    with pytest.raises(sqlite3.IntegrityError, match="immutable"):
        store._db.execute("UPDATE atoms SET content = 'x' WHERE id = ?", (atom_id,))


def test_raw_sql_delete_blocked_by_trigger(store: AtomStore):
    atom_id = _claim(store)
    with pytest.raises(sqlite3.IntegrityError, match="immutable"):
        store._db.execute("DELETE FROM atoms WHERE id = ?", (atom_id,))


def test_revision_is_new_atom_with_lineage(store: AtomStore):
    v1 = _claim(store, "结论：温度为 36.5")
    v2 = _claim(store, "结论：温度为 37.1", lineage=(v1,))
    assert v1 != v2
    v1_view = store.get(v1)
    v2_view = store.get(v2)
    assert v1_view is not None and v1_view.content == "结论：温度为 36.5"  # 旧版本原样保留
    assert v2_view is not None and v2_view.provenance.lineage == (v1,)
