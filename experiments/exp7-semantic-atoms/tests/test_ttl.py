"""不变式：TTL 过期（遗忘机制的存储层挂钩，议题 21 validity.ttl）。"""

from __future__ import annotations

from sematom import AssertionLevel, AtomStore

S = AssertionLevel.SPECULATION


def test_expired_atom_hidden_from_get(store: AtomStore, clock):
    atom_id = store.write_claim("短期记忆", source="s", assertion=S, ttl=60.0)
    assert store.get(atom_id) is not None
    clock.advance(61.0)
    assert store.get(atom_id) is None
    # 显式 now 参数可回看历史时刻（审计/replay 友好）
    assert store.get(atom_id, now=1_030.0) is not None


def test_no_ttl_never_expires(store: AtomStore, clock):
    atom_id = store.write_claim("永久事实", source="s", assertion=S)
    clock.advance(10**9)
    assert store.get(atom_id) is not None
    assert store.expired() == []


def test_expired_listing_for_forgetting(store: AtomStore, clock):
    keep = store.write_claim("保留", source="s", assertion=S, ttl=500.0)
    drop1 = store.write_claim("遗忘一", source="s", assertion=S, ttl=10.0)
    drop2 = store.write_claim("遗忘二", source="s", assertion=S, ttl=20.0)
    clock.advance(25.0)
    assert set(store.expired()) == {drop1, drop2}
    assert keep not in store.expired()


def test_boundary_exact_expiry(store: AtomStore):
    atom_id = store.write_claim("边界", source="s", assertion=S, ttl=10.0)
    # created=1000, ttl=10 → 1010 时刻过期（created + ttl <= now）
    assert store.get(atom_id, now=1_009.9) is not None
    assert store.get(atom_id, now=1_010.0) is None
