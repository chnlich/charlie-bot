"""Unit tests for the shared bounded LRU memo (src/core/memo.py).

The per-route memos (plans, triggers, thread metadata, store) inherit these
behaviors; each consumer's suite covers its own wiring, not the class again.
"""
from __future__ import annotations

from src.core.memo import BoundedMemo


def test_store_evicts_beyond_limit() -> None:
  """A store past the limit evicts the least-recently-used entries first."""
  memo: BoundedMemo[str, int] = BoundedMemo(2)
  memo.store("a", 1)
  memo.store("b", 2)
  memo.store("c", 3)

  assert list(memo) == ["b", "c"]
  assert len(memo) == 2


def test_get_hit_refreshes_recency() -> None:
  """A hit moves the key to the newest end, protecting it from the next eviction."""
  memo: BoundedMemo[str, int] = BoundedMemo(2)
  memo.store("a", 1)
  memo.store("b", 2)
  assert memo.get("a") == 1

  memo.store("c", 3)
  assert list(memo) == ["a", "c"]
  assert memo.get("b") is None


def test_get_miss_returns_none() -> None:
  """A miss returns None — which is why a stored value must never be None."""
  memo: BoundedMemo[str, int] = BoundedMemo(2)
  memo.store("a", 1)

  assert memo.get("missing") is None
