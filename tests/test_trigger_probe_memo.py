"""Unit tests for the trigger-file scan memo behind the sidebar deep probe.

``pending_trigger_state_sync`` is the read path of the status poll's
dirty-session deep probe; the memo must keep the (pending count, earliest fire)
verdict identical while a repeat scan over unchanged files reads no content.
"""
from __future__ import annotations

import json
import os
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import count_path_read_text, make_home_config

from src.core.config import CharlieBotConfig
from src.core.sessions import (
    _reset_trigger_meta_memo_for_tests,
    pending_trigger_state_sync,
)


@pytest.fixture(autouse=True)
def _clean_memo() -> None:
  _reset_trigger_meta_memo_for_tests()
  yield
  _reset_trigger_meta_memo_for_tests()


def _write_trigger(cfg: CharlieBotConfig, session_id: str, trigger: dict) -> Path:
  triggers_dir = cfg.sessions_dir / session_id / "triggers"
  triggers_dir.mkdir(parents=True, exist_ok=True)
  path = triggers_dir / f"{trigger['id']}.json"
  path.write_text(json.dumps(trigger), encoding="utf-8")
  return path


def _rewrite_atomically(path: Path, trigger: dict) -> None:
  """Publish trigger through the same tmp-file rename the real _save_trigger uses."""
  tmp = path.with_name("trigger.json.memo-test")
  tmp.write_text(json.dumps(trigger), encoding="utf-8")
  os.replace(tmp, path)


def _pending(tid: str, hours_ahead: int) -> dict:
  fire_at = (datetime.now(UTC) + timedelta(hours=hours_ahead)).isoformat()
  return {
      "id": tid,
      "session_id": "s1",
      "fire_at": fire_at,
      "message": "m",
      "status": "pending",
      "watch_targets": [],
  }


def _probe(triggers_dir: Path):
  return pending_trigger_state_sync(triggers_dir)


def test_verdict_and_repeat_reads_no_files(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = make_home_config(tmp_path)
  triggers_dir = cfg.sessions_dir / "s1" / "triggers"
  _write_trigger(cfg, "s1", _pending("t1", 3))
  _write_trigger(cfg, "s1", _pending("t2", 1))
  _write_trigger(cfg, "s1", {**_pending("t3", 2), "status": "fired"})

  count, earliest = _probe(triggers_dir)
  assert count == 2
  assert earliest is not None and earliest < datetime.now(UTC) + timedelta(hours=2)

  reads = count_path_read_text(monkeypatch, lambda path: True)
  for _ in range(3):
    assert _probe(triggers_dir) == (count, earliest)
  assert reads == []


def test_rereads_after_atomic_rewrite(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  triggers_dir = cfg.sessions_dir / "s1" / "triggers"
  path = _write_trigger(cfg, "s1", _pending("t1", 3))
  assert _probe(triggers_dir)[0] == 1

  _rewrite_atomically(path, {**_pending("t1", 3), "status": "cancelled"})
  assert _probe(triggers_dir) == (0, None)


def test_rereads_after_same_size_rewrite(tmp_path: Path) -> None:
  """Same byte size, new mtime_ns: the key's mtime half must move the verdict.

  A content change that keeps the size constant exercises mtime_ns alone —
  without this, a suite whose rewrites all change size could pass on the size
  half while the mtime half was broken.
  """
  cfg = make_home_config(tmp_path)
  triggers_dir = cfg.sessions_dir / "s1" / "triggers"
  path = _write_trigger(cfg, "s1", _pending("t1", 10))
  first = _probe(triggers_dir)[1]

  _rewrite_atomically(path, _pending("t1", 30))
  st = path.stat()
  os.utime(path, ns=(st.st_atime_ns, st.st_mtime_ns + 1_000_000))
  second = _probe(triggers_dir)[1]
  assert second is not None and first is not None and second > first


def test_earliest_fire_tracks_a_new_pending_file(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  triggers_dir = cfg.sessions_dir / "s1" / "triggers"
  _write_trigger(cfg, "s1", _pending("t1", 5))
  assert _probe(triggers_dir)[0] == 1

  _write_trigger(cfg, "s1", _pending("t2", 1))
  count, earliest = _probe(triggers_dir)
  assert count == 2
  assert earliest is not None and earliest < datetime.now(UTC) + timedelta(hours=2)


def test_failed_parse_is_not_memoized(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = make_home_config(tmp_path)
  triggers_dir = cfg.sessions_dir / "s1" / "triggers"
  path = _write_trigger(cfg, "s1", _pending("t1", 3))
  path.write_text("{not json", encoding="utf-8")

  assert _probe(triggers_dir) == (0, None)
  reads = count_path_read_text(monkeypatch, lambda path: True)
  assert _probe(triggers_dir) == (0, None)
  assert len(reads) == 1
