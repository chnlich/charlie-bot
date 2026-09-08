"""Unit tests for the shared bounded LRU memo (src/core/memo.py).

The per-route memos (plans, triggers, thread metadata, store) inherit these
behaviors; each consumer's suite covers its own wiring, not the class again.
"""
from __future__ import annotations

import os
from pathlib import Path

from src.core.memo import BoundedMemo, StatSignatureMemo


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


def _stat(path: Path, mtime_ns: int, size: int) -> os.stat_result:
  """A real stat of a file rewritten to *size* bytes and *mtime_ns*, the way the memo's
  consumers see signatures: from files whose writes publish through renames and utimes."""
  path.write_bytes(b"x" * size)
  os.utime(path, ns=(mtime_ns, mtime_ns))
  return path.stat()


def test_fresh_serves_a_matching_signature(tmp_path: Path) -> None:
  """fresh returns the recorded value while the stat's (mtime_ns, size) is unchanged."""
  memo: StatSignatureMemo[str, dict] = StatSignatureMemo(4)
  st = _stat(tmp_path / "f", mtime_ns=1_000, size=5)
  memo.record("a", st, {"v": 1})

  assert memo.fresh("a", st) == {"v": 1}


def test_fresh_misses_a_moved_signature(tmp_path: Path) -> None:
  """A content change moves mtime_ns or size, and fresh must miss the stale entry."""
  memo: StatSignatureMemo[str, dict] = StatSignatureMemo(4)
  memo.record("a", _stat(tmp_path / "f", mtime_ns=1_000, size=5), {"v": 1})

  assert memo.fresh("a", _stat(tmp_path / "g", mtime_ns=1_001, size=5)) is None
  assert memo.fresh("a", _stat(tmp_path / "g", mtime_ns=1_000, size=6)) is None
  assert memo.fresh("missing", _stat(tmp_path / "g", mtime_ns=1_000, size=5)) is None


def test_fresh_miss_after_restat_never_serves_stale_bytes(tmp_path: Path) -> None:
  """Re-recording under a newer stat replaces the entry the next stat can match."""
  memo: StatSignatureMemo[str, dict] = StatSignatureMemo(4)
  memo.record("a", _stat(tmp_path / "f", mtime_ns=1_000, size=5), {"v": 1})
  memo.record("a", _stat(tmp_path / "f", mtime_ns=2_000, size=7), {"v": 2})

  assert memo.fresh("a", _stat(tmp_path / "g", mtime_ns=1_000, size=5)) is None
  assert memo.fresh("a", _stat(tmp_path / "g", mtime_ns=2_000, size=7)) == {"v": 2}
