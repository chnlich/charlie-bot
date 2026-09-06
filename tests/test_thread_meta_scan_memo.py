"""Unit tests for the thread-metadata scan memo behind the sidebar deep probe.

``iter_recent_thread_metas`` is the read path of both the boot recovery scan and
the status poll's dirty-session deep probe; the memo must keep the scan's
verdicts identical while a repeat scan over unchanged files reads no content.
"""
from __future__ import annotations

import json
import os
import threading
from pathlib import Path

import pytest
from conftest import count_path_read_text, make_home_config, write_thread_meta

import src.core.init_worker_recovery as worker_recovery_module
from src.core.init_worker_recovery import (
    _reset_thread_meta_memo_for_tests,
    iter_recent_thread_metas,
)
from src.core.models import utc_now


@pytest.fixture(autouse=True)
def _clean_memo() -> None:
  _reset_thread_meta_memo_for_tests()
  yield
  _reset_thread_meta_memo_for_tests()


def _rewrite_atomically(path: Path, meta: dict) -> None:
  """Publish meta through the same tmp-file rename the real metadata writers use."""
  tmp = path.with_name("metadata.json.memo-test")
  tmp.write_text(json.dumps(meta), encoding="utf-8")
  os.replace(tmp, path)


def _scan(threads_dir: Path) -> list[dict]:
  return [meta for _dir, _path, meta in iter_recent_thread_metas(threads_dir, utc_now(), "thread_meta_read_failed")]


def test_repeat_scan_reads_no_files(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = make_home_config(tmp_path)
  for i in range(3):
    write_thread_meta(cfg, "s1", {"id": f"t{i}", "status": "completed"})

  first = _scan(cfg.sessions_dir / "s1" / "threads")
  assert {m["id"] for m in first} == {"t0", "t1", "t2"}

  reads = count_path_read_text(monkeypatch, lambda path: True)
  for _ in range(3):
    again = _scan(cfg.sessions_dir / "s1" / "threads")
    assert {m["id"] for m in again} == {m["id"] for m in first}
  assert reads == []


def test_rereads_after_atomic_rewrite(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  threads_dir = cfg.sessions_dir / "s1" / "threads"
  path = write_thread_meta(cfg, "s1", {"id": "t1", "status": "running"})
  assert [m["status"] for m in _scan(threads_dir)] == ["running"]

  _rewrite_atomically(path, {"id": "t1", "status": "completed"})
  assert [m["status"] for m in _scan(threads_dir)] == ["completed"]


def test_rereads_after_same_size_rewrite(tmp_path: Path) -> None:
  """Same byte size, new mtime_ns: the key's mtime half must move the verdict.

  A content change that keeps the size constant exercises mtime_ns alone —
  without this, a suite whose rewrites all change size could pass on the size
  half while the mtime half was broken.
  """
  cfg = make_home_config(tmp_path)
  threads_dir = cfg.sessions_dir / "s1" / "threads"
  path = write_thread_meta(cfg, "s1", {"id": "t1", "status": "completed", "note": "aaaa"})
  assert [m["note"] for m in _scan(threads_dir)] == ["aaaa"]

  _rewrite_atomically(path, {"id": "t1", "status": "completed", "note": "bbbb"})
  st = path.stat()
  os.utime(path, ns=(st.st_atime_ns, st.st_mtime_ns + 1_000_000))
  assert [m["note"] for m in _scan(threads_dir)] == ["bbbb"]


def test_same_signature_still_serves_memo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Same (mtime_ns, size) after a scan: the memo hit must not re-read."""
  cfg = make_home_config(tmp_path)
  threads_dir = cfg.sessions_dir / "s1" / "threads"
  path = write_thread_meta(cfg, "s1", {"id": "t1", "status": "completed"})
  assert len(_scan(threads_dir)) == 1
  signature = (path.stat().st_mtime_ns, path.stat().st_size)

  reads = count_path_read_text(monkeypatch, lambda p: True)
  assert len(_scan(threads_dir)) == 1
  assert reads == []
  assert (path.stat().st_mtime_ns, path.stat().st_size) == signature


def test_out_of_window_files_stay_unread(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  from datetime import timedelta

  from src.core import init as init_module

  cfg = make_home_config(tmp_path)
  threads_dir = cfg.sessions_dir / "s1" / "threads"
  write_thread_meta(cfg, "s1", {"id": "old", "status": "running"})
  old_ts = (utc_now() - init_module.RUNNING_SCAN_WINDOW - timedelta(days=1)).timestamp()
  os.utime(threads_dir / "old" / "metadata.json", (old_ts, old_ts))

  assert _scan(threads_dir) == []
  reads = count_path_read_text(monkeypatch, lambda path: True)
  assert _scan(threads_dir) == []
  assert reads == []


def test_failed_parse_is_not_memoized(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = make_home_config(tmp_path)
  threads_dir = cfg.sessions_dir / "s1" / "threads"
  path = write_thread_meta(cfg, "s1", {"id": "t1", "status": "completed"})
  path.write_text("{not json", encoding="utf-8")

  assert _scan(threads_dir) == []
  reads = count_path_read_text(monkeypatch, lambda path: True)
  assert _scan(threads_dir) == []
  assert len(reads) == 1


def test_memo_lock_is_released_across_yield(tmp_path: Path) -> None:
  """A consumer suspended at a yield point must not hold the memo lock."""
  cfg = make_home_config(tmp_path)
  write_thread_meta(cfg, "s1", {"id": "t1", "status": "completed"})
  threads_dir = cfg.sessions_dir / "s1" / "threads"
  _scan(threads_dir)  # warm the memo so the second scan takes the hit path

  gen = iter_recent_thread_metas(threads_dir, utc_now(), "thread_meta_read_failed")
  next(gen)
  acquired = threading.Event()

  def grab() -> None:
    worker_recovery_module._thread_meta_memo.get("metadata.json/not-in-the-memo")
    acquired.set()

  grabber = threading.Thread(target=grab)
  grabber.start()
  grabber.join(timeout=5)
  assert acquired.is_set()
  gen.close()
