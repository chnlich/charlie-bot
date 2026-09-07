"""Unit tests for the sidebar probe's single-walk plumbing.

The probe-input signature walk and the deep probe's read cores both need the
same scandir+stat phase over a session's thread metadata and trigger files; the
walked pairs let the probe cores consume the signature walk's stats so a
post-write poll walks the corpus once. The walked paths must stay verdict- and
yield-identical to the self-walked ones.
"""
from __future__ import annotations

import json
import os
from datetime import datetime, timedelta
from pathlib import Path

import pytest
from conftest import count_path_read_text, make_home_config, write_thread_meta

from src.core import init as init_module
from src.core import sidebar_state
from src.core.init import iter_recent_thread_metas
from src.core.init_worker_recovery import walk_thread_meta_stats
from src.core.models import utc_now
from src.core.sessions import (
  _sidebar_probe_walk,
  pending_trigger_state_sync,
  probe_sidebar_state_sync,
  selective_probe_sidebar_state,
)


@pytest.fixture(autouse=True)
def _clean_probe_state(monkeypatch: pytest.MonkeyPatch):
  from src.core.init_worker_recovery import _reset_thread_meta_memo_for_tests
  from src.core.sessions import _reset_trigger_meta_memo_for_tests

  _reset_thread_meta_memo_for_tests()
  _reset_trigger_meta_memo_for_tests()
  sidebar_state.reset_for_tests()
  yield
  _reset_thread_meta_memo_for_tests()
  _reset_trigger_meta_memo_for_tests()
  sidebar_state.reset_for_tests()


def _write_trigger(triggers_dir: Path, name: str, trigger: dict) -> Path:
  triggers_dir.mkdir(parents=True, exist_ok=True)
  path = triggers_dir / name
  path.write_text(json.dumps(trigger), encoding="utf-8")
  return path


def test_walked_thread_scan_yields_identical_triples(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  threads_dir = cfg.sessions_dir / "s1" / "threads"
  write_thread_meta(cfg, "s1", {"id": "t0", "status": "running"})
  write_thread_meta(cfg, "s1", {"id": "t1", "status": "completed"})
  old_ts = (utc_now() - init_module.RUNNING_SCAN_WINDOW - timedelta(days=1)).timestamp()
  os.utime(cfg.sessions_dir / "s1" / "threads" / "t1" / "metadata.json", (old_ts, old_ts))

  direct = list(iter_recent_thread_metas(threads_dir, utc_now(), "thread_meta_read_failed"))
  walked = list(iter_recent_thread_metas(
    threads_dir, utc_now(), "thread_meta_read_failed",
    walked=walk_thread_meta_stats(threads_dir, "thread_meta_read_failed")))
  expected = [(
    str(threads_dir / "t0"),
    str(threads_dir / "t0" / "metadata.json"),
    {"id": "t0", "status": "running"},
  )]
  assert direct == walked == expected


def test_walked_thread_scan_missing_dir_and_bare_thread_dir(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  threads_dir = cfg.sessions_dir / "s1" / "threads"
  (threads_dir / "bare").mkdir(parents=True)  # thread dir without metadata.json

  assert walk_thread_meta_stats(cfg.sessions_dir / "s1" / "nope", "thread_meta_read_failed") == []
  walked = walk_thread_meta_stats(threads_dir, "thread_meta_read_failed")
  assert walked == []  # the bare dir's stat failed with FileNotFoundError — nothing to read
  assert list(iter_recent_thread_metas(
    threads_dir, utc_now(), "thread_meta_read_failed", walked=walked)) == []


def test_walked_trigger_scan_parity_and_non_regular_gate(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  triggers_dir = cfg.sessions_dir / "s1" / "triggers"
  _write_trigger(triggers_dir, "a.json", {"status": "pending", "fire_at": "2026-10-01T00:00:00+00:00"})
  _write_trigger(triggers_dir, "b.json", {"status": "fired"})
  (triggers_dir / "dir.json").mkdir()  # a directory named like a trigger file

  _, inputs = _sidebar_probe_walk(
    cfg.sessions_dir / "s1" / "threads", triggers_dir, cfg.sessions_dir / "s1" / "plans.json")
  assert inputs.trigger_files is not None
  expected = (1, datetime.fromisoformat("2026-10-01T00:00:00+00:00"))
  assert pending_trigger_state_sync(triggers_dir) == expected
  assert pending_trigger_state_sync(triggers_dir, walked=inputs.trigger_files) == expected

  # A missing triggers dir walks to None; the core answers its empty state either way.
  missing = cfg.sessions_dir / "s1" / "no-triggers"
  _, inputs = _sidebar_probe_walk(
    cfg.sessions_dir / "s1" / "threads", missing, cfg.sessions_dir / "s1" / "plans.json")
  assert inputs.trigger_files is None
  assert pending_trigger_state_sync(missing, walked=None) == (0, None)


def test_post_write_probe_parses_only_the_moved_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = make_home_config(tmp_path)
  threads_dir = cfg.sessions_dir / "s1" / "threads"
  for i in range(4):
    write_thread_meta(cfg, "s1", {"id": f"t{i}", "status": "completed"})
  spec = ("s1", threads_dir, cfg.sessions_dir / "s1" / "triggers", cfg.sessions_dir / "s1" / "plans.json")

  entries, sigs = selective_probe_sidebar_state([spec], deep=False)  # cold probe
  assert entries["s1"]["thread_running"] is False
  sidebar_state.store_probe_signature("s1", sigs["s1"])

  # One writer publish: the tmp-file rename every thread-metadata writer performs.
  victim = threads_dir / "t0" / "metadata.json"
  tmp = victim.with_name("metadata.json.probe-test")
  tmp.write_text(json.dumps({"id": "t0", "status": "running"}), encoding="utf-8")
  os.replace(tmp, victim)

  reads = count_path_read_text(monkeypatch, lambda path: path.name == "metadata.json")
  entries, _ = selective_probe_sidebar_state([spec], deep=False)
  assert entries["s1"]["thread_running"] is True
  assert reads == [victim]
  assert sidebar_state.probe_signature("s1") is not None


def test_post_write_probe_verdict_matches_full_probe(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  threads_dir = cfg.sessions_dir / "s1" / "threads"
  write_thread_meta(cfg, "s1", {"id": "t0", "status": "running"})
  write_thread_meta(cfg, "s1", {"id": "t1", "status": "completed"})
  _write_trigger(cfg.sessions_dir / "s1" / "triggers", "a.json",
         {"status": "pending", "fire_at": "2026-10-01T00:00:00+00:00"})
  spec = ("s1", threads_dir, cfg.sessions_dir / "s1" / "triggers", cfg.sessions_dir / "s1" / "plans.json")

  sig, inputs = _sidebar_probe_walk(*spec[1:])
  full = probe_sidebar_state_sync([spec])
  walked = probe_sidebar_state_sync([spec], walked={"s1": inputs})
  assert full == walked
  assert sig[3] > 0  # the rollover element tracks the youngest in-window metadata
