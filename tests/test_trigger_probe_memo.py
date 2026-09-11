"""Unit tests for the trigger-file scan memo behind the sidebar deep probe.

``pending_trigger_state_sync`` is the read path of the status poll's
dirty-session deep probe; the memo must keep the (pending count, earliest fire)
verdict identical while a repeat scan over unchanged files reads no content.
"""
from __future__ import annotations

import json
import os
import shutil
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import (
    count_path_read_text,
    fresh_state_fixture,
    make_home_config,
    publish_same_size_rewrite,
    publish_via_tmp_rename,
)

from src.core.config import CharlieBotConfig
from src.core.sessions import (
    _reset_trigger_meta_memo_for_tests,
    pending_trigger_state_sync,
)

_clean_memo = fresh_state_fixture(_reset_trigger_meta_memo_for_tests)


def _write_trigger(cfg: CharlieBotConfig, session_id: str, trigger: dict) -> Path:
  triggers_dir = cfg.sessions_dir / session_id / "triggers"
  triggers_dir.mkdir(parents=True, exist_ok=True)
  path = triggers_dir / f"{trigger['id']}.json"
  path.write_text(json.dumps(trigger), encoding="utf-8")
  return path


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

  publish_via_tmp_rename(path, json.dumps({**_pending("t1", 3), "status": "cancelled"}), "trigger.json.memo-test")
  assert _probe(triggers_dir) == (0, None)


def test_rereads_after_same_size_rewrite(tmp_path: Path) -> None:
  """Same byte size, new mtime_ns: the key's mtime half must move the verdict."""
  cfg = make_home_config(tmp_path)
  triggers_dir = cfg.sessions_dir / "s1" / "triggers"
  path = _write_trigger(cfg, "s1", _pending("t1", 10))
  first = _probe(triggers_dir)[1]

  publish_same_size_rewrite(path, json.dumps(_pending("t1", 30)), "trigger.json.memo-test")
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


def test_failed_parse_rereads_once_per_directory_state(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A corrupt file re-reads once per proved directory state, not once per scan.

  The parse failure keeps the file out of the per-file memo, but the verdict
  serves the proved directory state — so the repeat scan reads nothing, and the
  next directory move (any atomic rename into the dir) re-reads the file once
  for the new state.
  """
  cfg = make_home_config(tmp_path)
  triggers_dir = cfg.sessions_dir / "s1" / "triggers"
  path = _write_trigger(cfg, "s1", _pending("t1", 3))
  path.write_text("{not json", encoding="utf-8")

  assert _probe(triggers_dir) == (0, None)
  reads = count_path_read_text(monkeypatch, lambda path: True)
  assert _probe(triggers_dir) == (0, None)
  assert reads == []

  publish_via_tmp_rename(path, "{still not json", "trigger.json.memo-test")
  assert _probe(triggers_dir) == (0, None)
  assert len(reads) == 1


def test_walked_path_stores_and_serves_the_verdict(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The probe's walked shape keys its verdict on the walk-instant directory signature."""
  cfg = make_home_config(tmp_path)
  triggers_dir = cfg.sessions_dir / "s1" / "triggers"
  _write_trigger(cfg, "s1", _pending("t1", 3))

  st = os.stat(triggers_dir)
  dir_sig = (st.st_mtime_ns, st.st_size)
  walked = [(str(p), p.stat()) for p in sorted(triggers_dir.glob("*.json"))]
  first = pending_trigger_state_sync(triggers_dir, walked=walked, dir_sig=dir_sig)
  assert first[0] == 1 and first[1] is not None

  reads = count_path_read_text(monkeypatch, lambda path: True)
  fresh_walked = [(str(p), p.stat()) for p in sorted(triggers_dir.glob("*.json"))]
  assert pending_trigger_state_sync(triggers_dir, walked=fresh_walked, dir_sig=dir_sig) == first
  assert reads == []  # the verdict serves the proved state; the walked pairs are not re-read

  cancelled = json.dumps({**_pending("t1", 3), "status": "cancelled"})
  publish_via_tmp_rename(triggers_dir / "t1.json", cancelled, "trigger.json.memo-test")
  st = os.stat(triggers_dir)
  moved_walked = [(str(p), p.stat()) for p in sorted(triggers_dir.glob("*.json"))]
  assert pending_trigger_state_sync(
      triggers_dir, walked=moved_walked, dir_sig=(st.st_mtime_ns, st.st_size)) == (0, None)


def test_walked_path_rereads_after_the_directory_moves(tmp_path: Path) -> None:
  """A rename into the dir moves its signature; the walked shape re-walks once for the new state."""
  cfg = make_home_config(tmp_path)
  triggers_dir = cfg.sessions_dir / "s1" / "triggers"
  path = _write_trigger(cfg, "s1", _pending("t1", 3))

  st = os.stat(triggers_dir)
  walked = [(str(p), p.stat()) for p in sorted(triggers_dir.glob("*.json"))]
  assert pending_trigger_state_sync(triggers_dir, walked=walked, dir_sig=(st.st_mtime_ns, st.st_size))[0] == 1

  publish_via_tmp_rename(path, json.dumps({**_pending("t1", 3), "status": "cancelled"}), "trigger.json.memo-test")
  st = os.stat(triggers_dir)
  moved_walked = [(str(p), p.stat()) for p in sorted(triggers_dir.glob("*.json"))]
  assert pending_trigger_state_sync(
      triggers_dir, walked=moved_walked, dir_sig=(st.st_mtime_ns, st.st_size)) == (0, None)


def test_missing_dir_answers_empty_and_drops_the_verdict(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  triggers_dir = cfg.sessions_dir / "s1" / "triggers"
  _write_trigger(cfg, "s1", _pending("t1", 3))
  assert _probe(triggers_dir)[0] == 1

  shutil.rmtree(triggers_dir)
  assert _probe(triggers_dir) == (0, None)
  triggers_dir.mkdir(parents=True)
  (triggers_dir / "t2.json").write_text(json.dumps(_pending("t2", 1)), encoding="utf-8")
  assert _probe(triggers_dir)[0] == 1
