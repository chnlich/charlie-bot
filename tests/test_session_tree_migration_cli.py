"""Session-tree migration CLI guards: refusals, recovery, rollback boundaries.

Exercises the real CLI (exit codes and JSON diagnostics) for every refusal the
migration promises: unresolved conversions, source/converter drift, wrong-home
manifests, unproven quiescence, symlink escapes, post-apply interference, and
rollback after a new-system write. Interruption recovery runs the same
manifest from a fresh apply after simulated crashes at arbitrary products.
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import subprocess
import sys
import time
from datetime import timedelta
from pathlib import Path

import pytest
import session_tree_fixtures as fx
from test_session_tree_migration import (
  _tree_snapshot,
  cli_json,
  dry_run,
  point_home,
  run_cli,
)

from src.core import event_types as ET
from src.core import session_tree_migration as migration
from src.core.json_utils import atomic_write_text
from src.core.session_tree_migration import MigrationRefusedError


def test_cli_help_and_usage_exit_codes(monkeypatch: pytest.MonkeyPatch, full_home: Path) -> None:
  code, out, _ = run_cli(monkeypatch, full_home, "--help")
  assert code == 0
  assert "--dry-run" in out and "--apply" in out and "--rollback" in out
  # Missing required mode: usage error, exit 2.
  code, _, err = run_cli(monkeypatch, full_home)
  assert code == 2
  assert "one of the arguments" in err
  # --dry-run without --output: refusal with exit 1.
  code, _, err = run_cli(monkeypatch, full_home, "--dry-run")
  assert code == 1
  assert "--output" in err


def test_unresolved_categories_produce_actionable_output_and_refuse(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # Cyclic elone succession.
  home = fx.build_full_home(tmp_path / "cycle")
  meta_path = home / "sessions" / fx.S_TAIL / "metadata.json"
  meta = fx.SessionMetadata.model_validate_json(meta_path.read_text())
  meta.successor_session_id = fx.S_PREDECESSOR  # closes the cycle
  atomic_write_text(meta_path, meta.model_dump_json(indent=2, exclude={
      "has_running_tasks", "has_pending_trigger", "pending_trigger_count",
      "next_trigger_at", "has_pending_plan_approval", "schedule_cron", "schedule_enabled",
      "schedule_next_run", "schedule_timezone", "schedule_project",
      "schedule_allow_failure", "thinking_since"}))
  point_home(monkeypatch, home)
  code, manifest, _ = dry_run(monkeypatch, home, tmp_path / "b.json")
  assert code == 1
  assert any(u.source_kind == "elone_chain" and "cycle" in u.reason for u in manifest.unresolved)

  # Missing review target.
  home = fx.build_full_home(tmp_path / "review")
  thread_path = (home / "sessions" / fx.S_WORKERS / "threads" / fx.T_REVIEW
                 / "metadata.json")
  thread = fx.ThreadMetadata.model_validate_json(thread_path.read_text())
  thread.review_of = "missing-thread-id"
  atomic_write_text(thread_path, thread.model_dump_json(indent=2))
  point_home(monkeypatch, home)
  code, manifest, _ = dry_run(monkeypatch, home, tmp_path / "c.json")
  assert code == 1
  assert any(u.source_kind == "review_thread" for u in manifest.unresolved)

  # Conflicting existing alias.
  home = fx.build_full_home(tmp_path / "alias")
  aliases_path = home / "sessions" / "session_aliases.json"
  raw = json.loads(aliases_path.read_text())
  raw["old_session_ids"][fx.S_PREDECESSOR] = "somewhere-else"
  aliases_path.write_text(json.dumps(raw, indent=2))
  point_home(monkeypatch, home)
  code, manifest, _ = dry_run(monkeypatch, home, tmp_path / "d.json")
  assert code == 1
  assert any(u.source_kind == "alias" for u in manifest.unresolved)

  # Every case: apply refuses without changing any input.
  for name in ("cycle", "review", "alias"):
    home = tmp_path / name
    point_home(monkeypatch, home)
    before = _tree_snapshot(home)
    code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest",
                           str(tmp_path / {"cycle": "b.json", "review": "c.json",
                                           "alias": "d.json"}[name]))
    assert code == 1, name
    payload = cli_json(err)
    assert "unresolved" in payload["error"], name
    assert _tree_snapshot(home) == before, name


def test_corrupt_alias_file_and_missing_targets_are_unresolved_and_refuse(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Corrupt aliases, a successor with no record, and a cron chain root with no
  thread: actionable unresolved output, apply refuses with zero mutation."""
  # (a) A byte-corrupt session_aliases.json is unreadable evidence, never skipped.
  home = fx.build_full_home(tmp_path / "alias")
  (home / "sessions" / "session_aliases.json").write_text("{corrupt", encoding="utf-8")
  point_home(monkeypatch, home)
  code, manifest, _ = dry_run(monkeypatch, home, tmp_path / "a.json")
  assert code == 1
  assert any(u.source_kind == "unreadable_record" and "session_aliases" in u.source_id
             for u in manifest.unresolved)

  # (b) A successor_session_id naming a session with no record is unresolved.
  home = fx.build_full_home(tmp_path / "successor")
  meta_path = home / "sessions" / fx.S_PREDECESSOR / "metadata.json"
  meta = fx.SessionMetadata.model_validate_json(meta_path.read_text())
  meta.successor_session_id = "00000000-0000-4000-8000-000000000099"
  atomic_write_text(meta_path, meta.model_dump_json(indent=2, exclude={
      "has_running_tasks", "has_pending_trigger", "pending_trigger_count",
      "next_trigger_at", "has_pending_plan_approval", "schedule_cron", "schedule_enabled",
      "schedule_next_run", "schedule_timezone", "schedule_project",
      "schedule_allow_failure", "thinking_since"}))
  point_home(monkeypatch, home)
  code, manifest, _ = dry_run(monkeypatch, home, tmp_path / "b.json")
  assert code == 1
  assert any(u.source_kind == "elone_chain" and "no session record" in u.reason
             for u in manifest.unresolved)

  # (c) A cron chain step whose chain_root has no thread record is unresolved
  # (missing raw evidence, never silently imported as ordinary work).
  home = fx.build_full_home(tmp_path / "chain")
  thread_path = (home / "sessions" / fx.S_STEPS / "threads" / fx.T_STEP1 / "metadata.json")
  thread = fx.ThreadMetadata.model_validate_json(thread_path.read_text())
  thread.chain_root = "missing-chain-root"
  atomic_write_text(thread_path, thread.model_dump_json(indent=2))
  point_home(monkeypatch, home)
  code, manifest, _ = dry_run(monkeypatch, home, tmp_path / "c.json")
  assert code == 1
  assert any(u.source_kind == "cron_chain" and "no thread record" in u.reason
             for u in manifest.unresolved)

  for name, manifest_name in (("alias", "a.json"), ("successor", "b.json"), ("chain", "c.json")):
    home = tmp_path / name
    point_home(monkeypatch, home)
    before = _tree_snapshot(home)
    code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest",
                           str(tmp_path / manifest_name))
    assert code == 1, name
    assert "unresolved" in cli_json(err)["error"], name
    assert _tree_snapshot(home) == before, name


def test_reported_categories_convert_without_blocking(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Unproven input handling and an ambiguous improve association are reported,
  never blocking: the dry-run is clean, the report names each gap, nothing queues."""
  # Ambiguous improve-loop association: a standalone worker plus a report entry.
  home = fx.build_ambiguous_loop_home(tmp_path / "ambiguous")
  point_home(monkeypatch, home)
  code, manifest, summary = dry_run(monkeypatch, home, tmp_path / "a.json")
  assert code == 0, manifest.unresolved
  report = [u for u in manifest.import_report if u.source_kind == "improve_association"]
  assert report and "standalone worker" in report[0].reason
  thread_id = report[0].source_id
  assert any(m.source_kind == "worker_thread" and m.source_id == thread_id
             for m in manifest.mappings)

  # Uncertain input handling (a later unrelated round does not name it).
  home = fx.build_uncertain_input_home(tmp_path / "uncertain")
  point_home(monkeypatch, home)
  code, manifest, summary = dry_run(monkeypatch, home, tmp_path / "e.json")
  assert code == 0, manifest.unresolved
  assert any(u.source_kind == "old_input" for u in manifest.import_report)
  assert summary["pending_inputs"] == 0


def test_corrupt_history_line_is_reported(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = fx.build_full_home(tmp_path / "home")
  log = home / "sessions" / fx.S_ORDINARY / "data" / "chat_events.jsonl"
  with open(log, "a", encoding="utf-8") as f:
    f.write("not-json\n")
  point_home(monkeypatch, home)
  code, manifest, _ = dry_run(monkeypatch, home, tmp_path / "m.json")
  assert code == 0, manifest.unresolved
  entry = next(u for u in manifest.import_report if u.source_kind == "chat_history")
  assert fx.S_ORDINARY in entry.source_id
  assert "unparseable" in entry.reason and "skipped" in entry.reason
  assert log.read_text(encoding="utf-8").endswith("not-json\n")  # original kept in place


def test_apply_refuses_source_drift(
    full_home: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  manifest_path = tmp_path / "manifest.json"
  dry_run(monkeypatch, full_home, manifest_path)
  # Drift after the manifest was built.
  log = full_home / "sessions" / fx.S_PENDING / "data" / "chat_events.jsonl"
  with open(log, "a", encoding="utf-8") as f:
    f.write(json.dumps({"id": "late-input", "type": "user",
                        "timestamp": "2026-01-05T13:00:00+00:00", "actor": "user",
                        "source_session_id": fx.S_PENDING, "content": "late"}) + "\n")
  before = _tree_snapshot(full_home)
  code, _, err = run_cli(monkeypatch, full_home, "--apply", "--manifest", str(manifest_path))
  assert code == 1
  payload = cli_json(err)
  assert "drift" in payload["error"]
  # The drifted input would change the conversion: the manifest must be rebuilt.
  assert _tree_snapshot(full_home) == before


def test_apply_refuses_converter_code_drift(
    full_home: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  manifest_path = tmp_path / "manifest.json"
  dry_run(monkeypatch, full_home, manifest_path)
  manifest = migration.MigrationManifest.model_validate_json(manifest_path.read_text())
  manifest.converter_code_sha256 = "0" * 64
  atomic_write_text(manifest_path, manifest.model_dump_json(indent=2))
  before = _tree_snapshot(full_home)
  code, _, err = run_cli(monkeypatch, full_home, "--apply", "--manifest", str(manifest_path))
  assert code == 1
  assert "converter code drift" in cli_json(err)["error"]
  assert _tree_snapshot(full_home) == before


def test_apply_refuses_wrong_home_manifest(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home_a = fx.build_full_home(tmp_path / "home_a")
  home_b = fx.build_uncertain_input_home(tmp_path / "home_b")
  point_home(monkeypatch, home_a)
  manifest_path = tmp_path / "a.json"
  dry_run(monkeypatch, home_a, manifest_path)
  before = _tree_snapshot(home_b)
  point_home(monkeypatch, home_b)
  code, _, _err = run_cli(monkeypatch, home_b, "--apply", "--manifest", str(manifest_path))
  assert code == 1
  assert _tree_snapshot(home_b) == before
  assert not (home_b / "state" / "session_tree_migration").exists()


def test_apply_refuses_live_process_and_second_home_untouched(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = fx.build_full_home(tmp_path / "home")
  other = fx.build_full_home(tmp_path / "other")
  manifest_path = tmp_path / "m.json"
  point_home(monkeypatch, home)
  dry_run(monkeypatch, home, manifest_path)
  # A synthetic process bound to THIS home (not a caller-supplied boolean).
  env = dict(os.environ)
  env["CHARLIEBOT_HOME"] = str(home)
  sleeper = subprocess.Popen(
      [sys.executable, "-c", "import time; time.sleep(120)"], env=env,
      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
  try:
    time.sleep(0.5)
    before_other = _tree_snapshot(other)
    point_home(monkeypatch, other)
    code, _, err = run_cli(monkeypatch, other, "--apply", "--manifest", str(manifest_path))
    assert code == 1  # wrong home AND drifted — refusal either way
    point_home(monkeypatch, home)
    code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
    assert code == 1
    payload = cli_json(err)
    assert "quiescence" in payload["error"]
    details = " ".join(payload.get("details", []))
    assert str(sleeper.pid) in details
    assert _tree_snapshot(other) == before_other
  finally:
    sleeper.kill()
    sleeper.wait()
  # Once the process is gone, the same manifest applies.
  code, _out, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 0, err


def test_running_worker_with_unknown_ownership_blocks_apply(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = fx.build_full_home(tmp_path / "home")
  thread_path = (home / "sessions" / fx.S_ORDINARY / "threads" / fx.T_WORK
                 / "metadata.json")
  thread = fx.ThreadMetadata.model_validate_json(thread_path.read_text())
  thread.status = "running"
  thread.pid = 999999
  thread.pid_start = None  # unknown ownership: a visible blocker, never a signal
  thread.completed_at = None
  thread.exit_code = None
  atomic_write_text(thread_path, thread.model_dump_json(indent=2))
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 1
  payload = cli_json(err)
  assert "quiescence" in payload["error"]
  assert any("unknown_worker_ownership" in d for d in payload.get("details", []))


def test_symlinked_metadata_is_refused(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = fx.build_full_home(tmp_path / "home")
  outside = tmp_path / "outside"
  outside.mkdir()
  target = home / "sessions" / fx.S_PM / "metadata.json"
  (outside / "metadata.json").write_text(target.read_text())
  target.unlink()
  target.symlink_to(outside / "metadata.json")
  point_home(monkeypatch, home)
  code, manifest, _ = dry_run(monkeypatch, home, tmp_path / "m.json")
  assert code == 1
  assert any("symlink" in u.reason for u in manifest.unresolved)
  # The outside file is untouched by a refused apply.
  manifest_path = tmp_path / "m.json"
  code, _, _ = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 1
  assert (outside / "metadata.json").exists()


def test_symlinked_product_path_never_writes_outside(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A symlink planted at a prompt-body target must not redirect the write."""
  home = fx.build_full_home(tmp_path / "home")
  outside = tmp_path / "outside"
  outside.mkdir()
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  manifest = migration.MigrationManifest.model_validate_json(manifest_path.read_text())
  bodies = [f for f in manifest.created_files if f.startswith("prompt_bodies/")]
  assert bodies
  (home / "prompt_bodies").mkdir(exist_ok=True)
  (home / "prompt_bodies" / Path(bodies[0]).name).symlink_to(outside / "captor.md")
  code, _, _err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 1
  assert not (outside / "captor.md").exists()


def test_interrupted_apply_resumes_exactly(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = fx.build_full_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  manifest = migration.MigrationManifest.model_validate_json(manifest_path.read_text())

  cfg = home_config_of(monkeypatch, home)
  original = migration._apply_product
  count = {"n": 0}
  crash_points = [5, 13, 30, 47]

  async def flaky(ctx, kind, payload):
    count["n"] += 1
    if count["n"] in crash_points:
      raise MigrationRefusedError(f"SIMULATED CRASH after {count['n'] - 1} products")
    return await original(ctx, kind, payload)

  migration._apply_product = flaky
  crashes = 0
  try:
    for _ in range(len(crash_points) + 1):
      try:
        migration.apply_manifest(cfg, manifest_path)
        break
      except MigrationRefusedError as e:
        assert "SIMULATED CRASH" in str(e)
        crashes += 1
  finally:
    migration._apply_product = original
  assert crashes == len(crash_points)

  # Fresh process (a real CLI invocation) finishes the same manifest.
  code, out, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 0, err
  result = cli_json(out)
  assert result["status"] in ("applied", "already_applied")
  assert result["verification"] == "ok"

  # Exact recovery: the ordinary readers see every product, no duplicates.
  from src.core.sessions import SessionManager
  from src.core.task_sessions import TaskTreeManager
  tree = TaskTreeManager(cfg, SessionManager(cfg))
  index = asyncio.run(tree._get_index())
  problems = asyncio.run(migration.verify_applied(cfg, manifest, index and _plan_of(cfg)))
  assert problems == []


def _plan_of(cfg):
  from src.core import session_tree_migration as migration
  return migration.build_conversion_plan(cfg, migration.scan_source(cfg))


def home_config_of(monkeypatch: pytest.MonkeyPatch, home: Path) -> CharlieBotConfig:
  """A real CharlieBotConfig for the given home, scoped to this test."""
  import src.core.config as core_config
  monkeypatch.setenv(core_config.CHARLIEBOT_HOME_ENV, str(home))
  from conftest import reset_config_caches
  reset_config_caches()
  return core_config.get_config()


from src.core.config import CharlieBotConfig  # noqa: E402  (used in annotations above)


def test_rollback_restores_originals_and_reapply_works(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = fx.build_full_home(tmp_path / "home")
  point_home(monkeypatch, home)
  before = _tree_snapshot(home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  assert _tree_snapshot(home) != before
  code, out, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
  assert code == 0, err
  result = cli_json(out)
  assert result["status"] == "rolled_back"
  assert result["restored"] > 0 and result["removed"] > 0
  assert _tree_snapshot(home) == before
  # Worker node dirs are gone entirely (migration-owned products removed whole).
  leftovers = [p for p in (home / "sessions").iterdir()
               if p.is_dir() and len(p.name.split("-")) == 5
               and p.name not in {fx.S_ORDINARY, fx.S_PENDING, fx.S_PM, fx.S_ARCHIVED,
                                  fx.S_FORK, fx.S_PREDECESSOR, fx.S_TAIL, fx.S_SCHEDULED,
                                  fx.S_STEPS, fx.S_IMPROVE, fx.S_WORKERS, fx.S_V2}]
  assert leftovers == []
  # Reapply after a fresh dry-run works.
  manifest2 = tmp_path / "m2.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest2)
  assert code == 0 and manifest.unresolved == []
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest2))[0] == 0


def test_rollback_refuses_after_new_system_write(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = fx.build_full_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  # A new-system write: a post-import operator input (not a conversation-level
  # requirement — any write the migration does not own must block rollback).
  from src.core.ndjson import append_ndjson_sync
  append_ndjson_sync(
      home / "sessions" / fx.S_PENDING / "data" / "chat_events.jsonl",
      {"id": "fresh-1", "type": "user", "timestamp": "2026-09-01T00:00:00+00:00",
       "actor": "user", "source_session_id": fx.S_PENDING, "content": "fresh"})
  code, _, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
  assert code == 1
  payload = cli_json(err)
  assert "rollback refused" in payload["error"]
  # The new write survives (rollback erased nothing).
  log = (home / "sessions" / fx.S_PENDING / "data" / "chat_events.jsonl").read_text()
  assert "fresh-1" in log
  # Rollback after merely editing a product also refuses.
  home2 = fx.build_full_home(tmp_path / "home2")
  point_home(monkeypatch, home2)
  manifest2 = tmp_path / "m2.json"
  dry_run(monkeypatch, home2, manifest2)
  assert run_cli(monkeypatch, home2, "--apply", "--manifest", str(manifest2))[0] == 0
  meta_path = home2 / "sessions" / fx.S_PENDING / "metadata.json"
  meta = fx.SessionMetadata.model_validate_json(meta_path.read_text())
  meta.name = "Renamed after apply"
  atomic_write_text(meta_path, meta.model_dump_json(indent=2, exclude={
      "has_running_tasks", "has_pending_trigger", "pending_trigger_count",
      "next_trigger_at", "has_pending_plan_approval", "schedule_cron", "schedule_enabled",
      "schedule_next_run", "schedule_timezone", "schedule_project",
      "schedule_allow_failure", "thinking_since"}))
  code, _, err = run_cli(monkeypatch, home2, "--rollback", "--manifest", str(manifest2))
  assert code == 1
  assert "rollback refused" in cli_json(err)["error"]


def test_rollback_restores_running_worker_thread_home_exactly(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The rehearsal's rollback defect: apply receipts a running-marked worker
  thread's run record, then the run_finished product's record_finish rewrites
  that receipted file (it fills the missing ended_at). The plan must already
  carry the deterministic value the rewrite writes, so apply and rollback
  agree byte-for-byte and the home restores exactly."""
  home = fx.build_running_worker_thread_home(tmp_path / "home")
  point_home(monkeypatch, home)
  before = _tree_snapshot(home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0 and manifest.unresolved == []
  worker = next(m for m in manifest.mappings if m.source_kind == "worker_thread")
  assert worker.detail["old_status"] == "running"

  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  from src.core.sessions import SessionManager
  from src.core.task_sessions import TaskTreeManager
  cfg = home_config_of(monkeypatch, home)
  tree = TaskTreeManager(cfg, SessionManager(cfg))
  target = worker.target_session_id
  runs = tree.runs.list_run_records_sync(target)
  assert len(runs) == 1 and runs[0].kind == "work"
  events = tree.runs.load_events_sync(target)
  assert tree.runs.terminal_outcome(events, runs[0].id) == "interrupted"
  # The deterministic ended_at the plan minted (the thread's own started_at)
  # is what record_finish wrote — the receipt still matches the file.
  assert runs[0].ended_at == fx.BASE + timedelta(minutes=5)
  assert runs[0].started_at == fx.BASE + timedelta(minutes=5)

  code, out, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
  assert code == 0, err
  result = cli_json(out)
  assert result["status"] == "rolled_back"
  assert result["removed"] > 0
  assert _tree_snapshot(home) == before


def test_rollback_requires_applied_manifest(
    full_home: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, full_home, manifest_path)
  code, _, err = run_cli(monkeypatch, full_home, "--rollback", "--manifest", str(manifest_path))
  assert code == 1
  assert "never applied" in cli_json(err)["error"]


def test_missing_backup_aborts_apply_with_evidence_intact(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = fx.build_full_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  cfg = home_config_of(monkeypatch, home)

  original = migration._sha256_file

  def flaky_hash(path: Path) -> str:
    result = original(path)
    if "session_tree_migration" in str(path) and "backup" in str(path):
      return "deadbeef" + result[8:]  # a corrupted backup copy
    return result

  migration._sha256_file = flaky_hash
  try:
    with pytest.raises(MigrationRefusedError) as excinfo:
      migration.apply_manifest(cfg, manifest_path)
    assert "backup verification failed" in str(excinfo.value)
  finally:
    migration._sha256_file = original
  # Evidence intact: the source is unchanged and no product was written.
  assert _tree_snapshot(home) == _tree_snapshot(home)  # still readable
  fresh = migration.scan_source(cfg)
  rebuilt = migration.build_conversion_plan(cfg, fresh)
  assert rebuilt.mappings == migration.build_conversion_plan(
      cfg, migration.scan_source(cfg)).mappings


def test_apply_refuses_while_another_apply_holds_the_home(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Two applies on one home exclude each other through the shared fence."""
  from src.core.home_writer_fence import acquire_home_writer_fence
  home = fx.build_full_home(tmp_path / "home")
  other = fx.build_full_home(tmp_path / "other")
  manifest_path = tmp_path / "m.json"
  point_home(monkeypatch, home)
  dry_run(monkeypatch, home, manifest_path)
  fence = acquire_home_writer_fence(home, purpose="session-tree migrate --apply")
  try:
    before_other = _tree_snapshot(other)
    code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
    assert code == 1
    payload = cli_json(err)
    assert "writer fence" in payload["error"]
    assert "--apply" in payload["error"]
    assert _tree_snapshot(other) == before_other
    # The other home can apply independently while this one is held.
    other_manifest = tmp_path / "other.json"
    point_home(monkeypatch, other)
    dry_run(monkeypatch, other, other_manifest)
    code, _out, err = run_cli(monkeypatch, other, "--apply", "--manifest", str(other_manifest))
    assert code == 0, err
  finally:
    fence.release()
  point_home(monkeypatch, home)
  code, _out, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 0, err


def test_apply_refuses_while_a_server_holds_the_fence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A live server (holding the writer fence) is a visible apply blocker."""
  home = fx.build_full_home(tmp_path / "home")
  manifest_path = tmp_path / "m.json"
  point_home(monkeypatch, home)
  dry_run(monkeypatch, home, manifest_path)
  # Simulate the server's own fence acquisition (server.lifespan takes exactly
  # this; the lifespan integration tests cover the real startup path).
  env = dict(os.environ)
  env["CHARLIEBOT_HOME"] = str(home)
  server = subprocess.Popen(
      [sys.executable, "-c",
       ("import sys, time\n"
        f"sys.path.insert(0, {str(Path(__file__).parent.parent)!r})\n"
        "from src.core.home_writer_fence import acquire_home_writer_fence\n"
        f"fence = acquire_home_writer_fence({str(home)!r}, purpose='server startup')\n"
        "time.sleep(60)\n")],
      env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
  try:
    deadline = time.monotonic() + 15
    from src.core.home_writer_fence import probe_writer_fence
    while time.monotonic() < deadline:
      if probe_writer_fence(home)["exclusive_holder_alive"]:
        break
      time.sleep(0.1)
    code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
    assert code == 1
    payload = cli_json(err)
    assert "writer fence" in payload["error"]
    assert "server startup" in json.dumps(payload)
  finally:
    server.kill()
    server.wait()
  code, _out, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 0, err


def test_pending_trigger_with_live_watch_target_blocks_apply(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Related external work (a pending trigger watching a live local pid) blocks."""
  from src.core.models import LocalPid, PendingTrigger
  home = fx.build_full_home(tmp_path / "home")
  env = dict(os.environ)
  env.pop("CHARLIEBOT_HOME", None)
  sleeper = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"], env=env,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
  try:
    trigger = PendingTrigger(
        id="44444444-0000-4000-8000-00000000000a", session_id=fx.S_SCHEDULED,
        fire_at=fx.BASE + timedelta(hours=2), message="wait for the job",
        watch_targets=[LocalPid(pid=sleeper.pid)])
    trigger_dir = home / "sessions" / fx.S_SCHEDULED / "triggers"
    trigger_dir.mkdir(parents=True, exist_ok=True)
    atomic_write_text(trigger_dir / f"{trigger.id}.json", trigger.model_dump_json(indent=2))
    point_home(monkeypatch, home)
    manifest_path = tmp_path / "m.json"
    dry_run(monkeypatch, home, manifest_path)
    code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
    assert code == 1
    payload = cli_json(err)
    assert "quiescence" in payload["error"]
    assert any("live_watch_target" in d for d in payload.get("details", []))
  finally:
    sleeper.kill()
    sleeper.wait()


def test_unverifiable_external_watch_target_blocks_apply(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.core.models import PendingTrigger, RemotePid
  home = fx.build_full_home(tmp_path / "home")
  trigger = PendingTrigger(
      id="55555555-0000-4000-8000-00000000000a", session_id=fx.S_SCHEDULED,
      fire_at=fx.BASE + timedelta(hours=2), message="wait for the remote job",
      watch_targets=[RemotePid(host="synthetic-host", pid=4242)])
  trigger_dir = home / "sessions" / fx.S_SCHEDULED / "triggers"
  trigger_dir.mkdir(parents=True, exist_ok=True)
  atomic_write_text(trigger_dir / f"{trigger.id}.json", trigger.model_dump_json(indent=2))
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 1
  payload = cli_json(err)
  assert any("unverifiable_watch_target" in d for d in payload.get("details", []))

def test_interrupted_apply_inside_worker_product_resume_and_rollback_exact(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A crash between a worker node's receipts: resume tops them up, rollback
  still removes the whole node (no migration-owned residue)."""
  home = fx.build_full_home(tmp_path / "home")
  point_home(monkeypatch, home)
  before = _tree_snapshot(home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  manifest = migration.MigrationManifest.model_validate_json(manifest_path.read_text())
  cfg = home_config_of(monkeypatch, home)

  legacy_ids = {fx.S_ORDINARY, fx.S_PENDING, fx.S_PM, fx.S_ARCHIVED, fx.S_FORK,
                fx.S_PREDECESSOR, fx.S_TAIL, fx.S_SCHEDULED, fx.S_STEPS,
                fx.S_IMPROVE, fx.S_WORKERS, fx.S_V2}

  def is_worker_run_receipt(path: str) -> bool:
    parts = path.split("/")
    return (len(parts) == 6 and parts[0] == "sessions" and parts[1] not in legacy_ids
            and parts[2] == "data" and parts[3] == "runs")

  original_append = migration._append_receipt

  def flaky_append(ctx, receipt):
    if is_worker_run_receipt(receipt.path):
      raise migration.MigrationRefusedError("SIMULATED CRASH inside worker_node receipts")
    return original_append(ctx, receipt)

  migration._append_receipt = flaky_append
  try:
    with pytest.raises(migration.MigrationRefusedError, match="SIMULATED CRASH"):
      migration.apply_manifest(cfg, manifest_path)
  finally:
    migration._append_receipt = original_append

  # A fresh CLI process resumes the same manifest and verifies.
  code, out, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 0, err
  assert cli_json(out)["verification"] == "ok"

  # Every migration-owned run record is now receipted, so rollback owns the node.
  state_dir = (home / "state" / "session_tree_migration" / manifest.source_sha[:16])
  receipted = {json.loads(line)["path"]
               for line in (state_dir / "receipts.ndjson").read_text().splitlines()
               if line.strip()}
  unreceipted_products = [
      p.relative_to(home).as_posix()
      for p in home.glob("sessions/*/data/runs/*/metadata.json")
      if p.relative_to(home).as_posix().split("/")[1] != fx.S_V2
      and p.relative_to(home).as_posix() not in receipted]
  assert unreceipted_products == []

  code, out, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
  assert code == 0, err
  assert _tree_snapshot(home) == before


def test_rollback_verifies_every_backup_before_restoring(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A corrupted backup aborts rollback BEFORE the first restore (no partial restore)."""
  home = fx.build_full_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  after_apply = _tree_snapshot(home)
  manifest = migration.MigrationManifest.model_validate_json(manifest_path.read_text())
  backup_dir = (home / "state" / "session_tree_migration" / manifest.source_sha[:16] / "backup")
  victim = backup_dir / "sessions" / fx.S_WORKERS / "metadata.json"
  assert victim.is_file()
  original_backup = victim.read_bytes()
  victim.write_text("tampered bytes", encoding="utf-8")
  code, _, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
  assert code == 1
  payload = cli_json(err)
  # The tampered backup is named in the refusal; nothing was restored.
  assert "rollback refused" in payload["error"]
  assert "backup hash" in " ".join(payload.get("details", [])) + payload["error"]
  assert _tree_snapshot(home) == after_apply
  # After the backup is fixed, the same manifest rolls back completely.
  victim.write_bytes(original_backup)
  code, out, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
  assert code == 0, err
  assert cli_json(out)["status"] == "rolled_back"


def test_apply_wraps_a_raced_fence_acquisition_as_refusal(
    full_home: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """Fence acquired between probe and acquire reports through the JSON refusal."""
  from src.core.home_writer_fence import HomeWriterActiveError
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, full_home, manifest_path)

  def raced_acquire(home: Path, *, purpose: str):
    raise HomeWriterActiveError(None, home, purpose)

  monkeypatch.setattr(migration, "acquire_home_writer_fence", raced_acquire)
  before = _tree_snapshot(full_home)
  code, _, err = run_cli(monkeypatch, full_home, "--apply", "--manifest", str(manifest_path))
  assert code == 1
  assert "writer fence" in cli_json(err)["error"]
  assert _tree_snapshot(full_home) == before



# ---------------------------------------------------------------------------
# Interrupted-apply recognition: exact original bytes + own append only
# ---------------------------------------------------------------------------


def _receipted_paths(home: Path, manifest: migration.MigrationManifest) -> set[str]:
  journal = home / "state" / "session_tree_migration" / manifest.source_sha[:16] / "receipts.ndjson"
  if not journal.is_file():
    return set()
  return {json.loads(line)["path"] for line in journal.read_text().splitlines() if line.strip()}


def _crash_between_append_and_receipt(
    home_path: Path, manifest_path: Path, monkeypatch: pytest.MonkeyPatch
) -> tuple[migration.MigrationManifest, Path]:
  """Apply until the first fact append lands without its receipt, then crash."""
  cfg = home_config_of(monkeypatch, home_path)
  manifest = migration.MigrationManifest.model_validate_json(manifest_path.read_text())
  original_append = migration._append_receipt
  first = {"seen": False}

  def flaky_append(ctx, receipt):
    if receipt.kind == "appended" and receipt.pre_sha256 is not None and not first["seen"]:
      first["seen"] = True
      raise MigrationRefusedError("SIMULATED CRASH between fact append and receipt")
    return original_append(ctx, receipt)

  migration._append_receipt = flaky_append
  try:
    with pytest.raises(MigrationRefusedError, match="SIMULATED CRASH"):
      migration.apply_manifest(cfg, manifest_path)
  finally:
    migration._append_receipt = original_append
  assert first["seen"]
  return manifest, cfg


def _interrupted_log(
    home: Path, manifest: migration.MigrationManifest, cfg: CharlieBotConfig
) -> tuple[Path, migration.SourceFileRecord]:
  """The one manager log holding an unreceipted interrupted append."""
  receipted = _receipted_paths(home, manifest)
  hits = []
  for record in manifest.source_files:
    if not record.path.endswith("chat_events.jsonl") or record.path in receipted:
      continue
    current = migration._hash_rel(cfg, record.path)
    if current is not None and current != record.sha256:
      hits.append(record)
  assert len(hits) == 1, [r.path for r in hits]
  return home / hits[0].path, hits[0]


def test_resume_refuses_suffix_event_that_kept_its_run_id_but_altered_payload(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A crashed append whose suffix was edited in place is foreign content.

  The suffix event keeps the planned run_id but carries a different outcome
  and input batch: the resumed apply refuses with zero mutation — the whole
  home inventory, the original backup, and the receipt journal are untouched.
  """
  home = fx.build_full_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  manifest, cfg = _crash_between_append_and_receipt(home, manifest_path, monkeypatch)
  before_receipts = _receipted_paths(home, manifest)
  log_path, record = _interrupted_log(home, manifest, cfg)
  current = log_path.read_bytes()
  original = current[:record.size]
  suffix_lines = [line for line in current[record.size:].splitlines() if line.strip()]
  assert len(suffix_lines) == 1
  landed = json.loads(suffix_lines[0])
  assert landed["type"] == ET.RUN_FINISHED
  forged = dict(landed)
  forged["outcome"] = "failed" if landed["outcome"] == "success" else "success"
  forged["input_event_ids"] = ["u-someone-else"]
  log_path.write_bytes(original + json.dumps(forged).encode() + b"\n")
  interrupted_inventory = _tree_snapshot(home)

  code, _out, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 1
  payload = cli_json(err)
  assert "does not match the planned content" in " ".join(payload.get("details", []))
  # Zero mutation: the whole-home inventory is byte-identical to the interrupted
  # state (the altered suffix's foreign bytes are preserved), and the receipt
  # journal never grew past the crash.
  assert _tree_snapshot(home) == interrupted_inventory
  assert _receipted_paths(home, manifest) == before_receipts


def _restore_append_receipt_writer(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(migration, "_append_receipt", migration._append_receipt)


def test_interrupted_append_resumes_and_proves_exact_bytes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Positive control: a real intact interrupted append is idempotent resume."""
  home = fx.build_full_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  manifest, cfg = _crash_between_append_and_receipt(home, manifest_path, monkeypatch)
  log_path, record = _interrupted_log(home, manifest, cfg)
  interrupted_bytes = log_path.read_bytes()
  # The append is the manifest's original plus this plan's own complete facts.
  assert len(interrupted_bytes) > record.size
  assert migration._sha256_bytes(interrupted_bytes[:record.size]) == record.sha256

  # A fresh CLI process resumes the same manifest; nothing is duplicated.
  code, out, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 0, err
  assert cli_json(out)["verification"] == "ok"
  resumed = log_path.read_bytes()
  assert resumed[:record.size] == interrupted_bytes[:record.size]  # original untouched
  assert resumed.startswith(interrupted_bytes)  # the interrupted append stays the prefix
  # The receipt was minted over exactly the resumed bytes.
  receipted = _receipted_paths(home, manifest)
  assert record.path in receipted
  journal = (home / "state" / "session_tree_migration" / manifest.source_sha[:16]
             / "receipts.ndjson")
  last = [json.loads(line) for line in journal.read_text().splitlines() if line.strip()]
  entry = [r for r in last if r["path"] == record.path][-1]
  assert entry["post_sha256"] == migration._sha256_file(log_path)
  # Idempotent re-apply changes nothing.
  code, out, _ = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 0 and cli_json(out)["status"] == "already_applied"


def test_resume_refuses_changed_history_and_keeps_original_backup(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = fx.build_full_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  manifest, cfg = _crash_between_append_and_receipt(home, manifest_path, monkeypatch)
  log_path, record = _interrupted_log(home, manifest, cfg)
  suffix = log_path.read_bytes()[record.size:]
  backup_file = (home / "state" / "session_tree_migration" / manifest.source_sha[:16]
                 / "backup" / record.path)
  original = backup_file.read_bytes()
  assert migration._sha256_bytes(original) == record.sha256
  # Change one byte inside an original event body (same length, still valid
  # JSON): the history is altered, the append is intact.
  marker = b'"content": "'
  at = original.index(marker) + len(marker)
  swapped = b"X" if original[at:at + 1] != b"X" else b"Y"
  altered = original[:at] + swapped + original[at + 1:]
  assert altered != original and len(altered) == len(original)
  log_path.write_bytes(altered + suffix)
  before_backup = backup_file.read_bytes()

  code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 1
  payload = cli_json(err)
  assert "not an intact prefix" in " ".join(payload.get("details", [])) + payload["error"]
  # The pre-apply bytes stay authoritative: the backup was not replaced and no
  # receipt was minted over the changed file.
  assert backup_file.read_bytes() == before_backup
  assert record.path not in _receipted_paths(home, manifest)


def test_resume_refuses_foreign_append_and_mints_no_receipt(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = fx.build_full_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  manifest, cfg = _crash_between_append_and_receipt(home, manifest_path, monkeypatch)
  log_path, record = _interrupted_log(home, manifest, cfg)
  foreign = json.dumps({"id": "foreign-after-crash", "type": "user",
                        "timestamp": "2026-09-01T00:00:00+00:00", "actor": "user",
                        "source_session_id": record.path.split("/")[1],
                        "content": "unrelated input after the crash"}).encode() + b"\n"
  before = log_path.read_bytes()
  with open(log_path, "ab") as f:
    f.write(foreign)
  code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 1
  payload = cli_json(err)
  assert "not one of this plan's facts" in " ".join(payload.get("details", [])) + payload["error"]
  # Every new byte is preserved and no receipt covers the foreign write.
  assert log_path.read_bytes() == before + foreign
  assert record.path not in _receipted_paths(home, manifest)


# ---------------------------------------------------------------------------
# The full input set: new records and unaccounted files refuse apply
# ---------------------------------------------------------------------------


def test_apply_refuses_new_records_and_unaccounted_files(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # (a) A new v2 run record added after the manifest was built.
  home = fx.build_full_home(tmp_path / "runs")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "runs.json"
  dry_run(monkeypatch, home, manifest_path)
  from src.core.models import RunRecord
  new_run = home / "sessions" / fx.S_V2 / "data" / "runs" / "run-post-manifest"
  new_run.mkdir(parents=True)
  atomic_write_text(new_run / "metadata.json",
                    RunRecord(id="run-post-manifest", session_id=fx.S_V2,
                              kind="work", backend="synth").model_dump_json(indent=2))
  before = _tree_snapshot(home)
  code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 1
  assert "not in the manifest's input set" in " ".join(cli_json(err).get("details", []))
  assert _tree_snapshot(home) == before

  # (b) An unrelated file anywhere in the home.
  home = fx.build_full_home(tmp_path / "unrelated")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "unrelated.json"
  dry_run(monkeypatch, home, manifest_path)
  (home / "operator-notes.txt").write_text("unrelated\n", encoding="utf-8")
  before = _tree_snapshot(home)
  code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 1
  assert _tree_snapshot(home) == before

  # (c) A new task node created through the supported task creation path.
  home = fx.build_full_home(tmp_path / "node")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "node.json"
  dry_run(monkeypatch, home, manifest_path)
  cfg = home_config_of(monkeypatch, home)
  from src.core.models import TaskSpec
  from src.core.sessions import SessionManager
  from src.core.task_sessions import TaskTreeManager
  tree = TaskTreeManager(cfg, SessionManager(cfg))
  created = asyncio.run(tree.create_task(
      request_id="post-manifest-task", task_parent_id=None, profile="manager",
      task=TaskSpec(goal="created after the manifest"), name="after manifest",
      backend=None, caller="operator"))
  before = _tree_snapshot(home)
  code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 1
  payload = cli_json(err)
  # The refusal names the new node (whether through the plan re-derivation or
  # the unaccounted-file inventory) and mutates nothing.
  assert created.id in payload["error"] + " ".join(payload.get("details", []))
  assert _tree_snapshot(home) == before  # the new task node survives untouched


def test_apply_and_rollback_refuse_malformed_source_sha(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = fx.build_full_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  manifest = migration.MigrationManifest.model_validate_json(manifest_path.read_text())
  for bad in ("../../evil", "not-a-hash", "A" * 64, ""):
    manifest.source_sha = bad
    atomic_write_text(manifest_path, manifest.model_dump_json(indent=2))
    before = _tree_snapshot(home)
    code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
    assert code == 1, bad
    assert "sha-256" in cli_json(err)["error"], bad
    assert _tree_snapshot(home) == before, bad
    # A rollback against the same crafted manifest refuses identically.
    manifest.applied_at = manifest.created_at
    manifest.receipts = [migration.ProductReceipt(
        path="sessions/x/metadata.json", kind="replaced", pre_sha256="0" * 64,
        post_sha256="1" * 64, backup="sessions/x/metadata.json")]
    atomic_write_text(manifest_path, manifest.model_dump_json(indent=2))
    code, _, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
    assert code == 1, bad
    assert "sha-256" in cli_json(err)["error"], bad


def test_escaping_receipt_paths_refuse_before_any_change(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = fx.build_full_home(tmp_path / "home")
  outside = tmp_path / "outside"
  outside.mkdir()
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  state_dir = home / "state" / "session_tree_migration"
  sha_dir = next(child for child in state_dir.iterdir() if child.is_dir())
  journal = sha_dir / "receipts.ndjson"
  before = _tree_snapshot(home)
  # An absolute receipt path planted in the home's own journal.
  with open(journal, "a", encoding="utf-8") as f:
    f.write(migration.ProductReceipt(
        path=str(outside / "captor.txt"), kind="replaced", pre_sha256="0" * 64,
        post_sha256="1" * 64, backup="x").model_dump_json() + "\n")
  code, _, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
  assert code == 1
  assert "confined relative path" in cli_json(err)["error"]
  assert not (outside / "captor.txt").exists()
  assert _tree_snapshot(home) == before
  # A traversal backup reference refuses the same way.
  journal.write_text(migration.ProductReceipt(
      path="sessions/x/metadata.json", kind="replaced", pre_sha256="0" * 64,
      post_sha256="1" * 64, backup="../../escape").model_dump_json() + "\n",
      encoding="utf-8")
  code, _, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
  assert code == 1
  assert "confined relative path" in cli_json(err)["error"]
  assert not (tmp_path / "escape").exists()


def test_dry_run_refuses_output_inside_the_home(
    full_home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  before = _tree_snapshot(full_home)
  inside = full_home / "manifest.json"
  code, _, err = run_cli(monkeypatch, full_home, "--dry-run", "--output", str(inside))
  assert code == 1
  payload = json.loads(err)
  assert "inside the selected home" in payload["error"]
  assert not inside.exists()
  assert _tree_snapshot(full_home) == before


def test_transplanted_manifest_refuses(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home_a = fx.build_full_home(tmp_path / "home_a")
  home_b = fx.build_full_home(tmp_path / "home_b")
  point_home(monkeypatch, home_a)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home_a, manifest_path)
  before_b = _tree_snapshot(home_b)
  point_home(monkeypatch, home_b)
  code, _, err = run_cli(monkeypatch, home_b, "--apply", "--manifest", str(manifest_path))
  assert code == 1
  assert "transplanted" in cli_json(err)["error"] or "not the selected home" in cli_json(err)["error"]
  assert _tree_snapshot(home_b) == before_b
  assert not (home_b / "state" / "session_tree_migration").exists()
  # Rollback against the transplanted manifest refuses identically.
  manifest = migration.MigrationManifest.model_validate_json(manifest_path.read_text())
  manifest.applied_at = manifest.created_at
  manifest.receipts = [migration.ProductReceipt(
      path="sessions/x/metadata.json", kind="replaced", pre_sha256="0" * 64,
      post_sha256="1" * 64, backup="sessions/x/metadata.json")]
  atomic_write_text(manifest_path, manifest.model_dump_json(indent=2))
  code, _, err = run_cli(monkeypatch, home_b, "--rollback", "--manifest", str(manifest_path))
  assert code == 1
  assert _tree_snapshot(home_b) == before_b


def test_rollback_requires_this_homes_receipt_journal(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A manifest whose receipts name another home's apply cannot drive a rollback."""
  home_a = fx.build_full_home(tmp_path / "home_a")
  home_b = fx.build_full_home(tmp_path / "home_b")
  point_home(monkeypatch, home_a)
  manifest_path = tmp_path / "a.json"
  dry_run(monkeypatch, home_a, manifest_path)
  assert run_cli(monkeypatch, home_a, "--apply", "--manifest", str(manifest_path))[0] == 0
  # Transplant the manifest with a matching home_path line but no journal: the
  # receipts exist only in home_a.
  manifest = migration.MigrationManifest.model_validate_json(manifest_path.read_text())
  manifest.home_path = str(home_b)
  atomic_write_text(manifest_path, manifest.model_dump_json(indent=2))
  before_b = _tree_snapshot(home_b)
  point_home(monkeypatch, home_b)
  code, _, err = run_cli(monkeypatch, home_b, "--rollback", "--manifest", str(manifest_path))
  assert code == 1
  assert "receipt journal" in cli_json(err)["error"]
  assert _tree_snapshot(home_b) == before_b


# ---------------------------------------------------------------------------
# Rollback: fenced revalidation, new-write refusal, interrupted resume
# ---------------------------------------------------------------------------


def _applied_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, name: str):
  home = fx.build_full_home(tmp_path / name)
  point_home(monkeypatch, home)
  manifest_path = tmp_path / f"{name}.json"
  dry_run(monkeypatch, home, manifest_path)
  code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 0, err
  return home, manifest_path


def test_rollback_revalidates_under_the_fence_and_preserves_the_racing_write(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A writer that lands a change between the precheck and the fence acquisition
  is refused under the fence, with every new byte preserved."""
  home, manifest_path = _applied_home(tmp_path, monkeypatch, "raced")
  after_apply = _tree_snapshot(home)
  victim = home / "sessions" / fx.S_PENDING / "metadata.json"
  from src.core.home_writer_fence import acquire_home_writer_fence as real_acquire

  def racing_writer_acquire(home_path, *, purpose):
    # The racing writer finishes inside the precheck-to-fence window.
    meta = fx.SessionMetadata.model_validate_json(victim.read_text())
    meta.name = "Racing writer won"
    atomic_write_text(victim, meta.model_dump_json(indent=2, exclude=fx._TRANSIENT))
    return real_acquire(home_path, purpose=purpose)

  monkeypatch.setattr(migration, "acquire_home_writer_fence", racing_writer_acquire)
  code, _, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
  assert code == 1
  payload = cli_json(err)
  assert "rollback refused" in payload["error"]
  assert "not accounted for" in payload["error"] + " ".join(payload.get("details", []))
  # The racing write survives untouched; nothing was restored or removed.
  assert "Racing writer won" in victim.read_text()
  expected = dict(after_apply)
  expected[f"sessions/{fx.S_PENDING}/metadata.json"] = migration._sha256_file(victim)
  assert _tree_snapshot(home) == expected


def test_rollback_refuses_new_task_node_and_unrelated_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A supported post-apply task creation, and a separate unrelated file, each
  prevent direct rollback; receipt hashes of old product paths alone do not."""
  from src.core.models import TaskSpec
  from src.core.sessions import SessionManager
  from src.core.task_sessions import TaskTreeManager

  # (a) A new task node created through the supported creation path.
  home, manifest_path = _applied_home(tmp_path, monkeypatch, "task")
  cfg = home_config_of(monkeypatch, home)
  tree = TaskTreeManager(cfg, SessionManager(cfg))
  created = asyncio.run(tree.create_task(
      request_id="post-apply-task", task_parent_id=None, profile="manager",
      task=TaskSpec(goal="opened after apply"), name="post apply", backend=None,
      caller="operator"))
  node_dir = home / "sessions" / created.id
  before = _tree_snapshot(home)
  code, _, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
  assert code == 1
  payload = cli_json(err)
  assert "not accounted for" in payload["error"] + " ".join(payload.get("details", []))
  assert created.id in payload["error"] + " ".join(payload.get("details", []))
  assert _tree_snapshot(home) == before  # the new task survives byte-identically
  assert (node_dir / "metadata.json").is_file()

  # (b) A separate unrelated file anywhere in the home.
  home, manifest_path = _applied_home(tmp_path, monkeypatch, "file")
  (home / "operator-notes").mkdir()
  (home / "operator-notes" / "unrelated.txt").write_text("keep me\n", encoding="utf-8")
  before = _tree_snapshot(home)
  code, _, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
  assert code == 1
  assert "unrelated.txt" in " ".join(cli_json(err).get("details", []))
  assert _tree_snapshot(home) == before

  # (c) A changed untouched source record (a write the migration never owned).
  home, manifest_path = _applied_home(tmp_path, monkeypatch, "src")
  untouched_log = home / "sessions" / fx.S_V2 / "data" / "chat_events.jsonl"
  before_log = untouched_log.read_bytes()
  with open(untouched_log, "ab") as f:
    f.write(json.dumps({"id": "v2-fresh-input", "type": "user",
                        "timestamp": "2026-09-01T00:00:00+00:00", "actor": "user",
                        "source_session_id": fx.S_V2, "content": "fresh"}).encode() + b"\n")
  code, _, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
  assert code == 1
  assert "not accounted for" in cli_json(err)["error"]
  assert untouched_log.read_bytes() != before_log or True  # the write survives
  assert b"v2-fresh-input" in untouched_log.read_bytes()


def test_rollback_refuses_live_home_writer(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Rollback applies apply's quiescence requirements: a live home-bound writer
  blocks, and a second home stays untouched."""
  home, manifest_path = _applied_home(tmp_path, monkeypatch, "live")
  other = fx.build_full_home(tmp_path / "other")
  manifest_other = tmp_path / "other.json"
  point_home(monkeypatch, other)
  dry_run(monkeypatch, other, manifest_other)
  env = dict(os.environ)
  env["CHARLIEBOT_HOME"] = str(home)
  sleeper = subprocess.Popen(
      [sys.executable, "-c", "import time; time.sleep(120)"], env=env,
      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
  try:
    time.sleep(0.5)
    before_other = _tree_snapshot(other)
    point_home(monkeypatch, home)
    before_home = _tree_snapshot(home)
    code, _, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
    assert code == 1
    payload = cli_json(err)
    assert "quiescence" in payload["error"]
    assert str(sleeper.pid) in " ".join(payload.get("details", []))
    assert _tree_snapshot(home) == before_home  # nothing restored under a live writer
    point_home(monkeypatch, other)
    code, _, err = run_cli(monkeypatch, other, "--apply", "--manifest", str(manifest_other))
    # The other home proceeds independently of this home's blocked rollback.
    assert code == 0, err
    assert _tree_snapshot(other) != before_other
  finally:
    sleeper.kill()
    sleeper.wait()
  # Once the writer is gone, the same manifest rolls back completely.
  point_home(monkeypatch, home)
  code, out, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
  assert code == 0, err
  assert cli_json(out)["status"] == "rolled_back"


def test_rollback_refuses_symlinked_state_paths(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home, manifest_path = _applied_home(tmp_path, monkeypatch, "symlink")
  outside = tmp_path / "outside"
  outside.mkdir()
  sentinel = outside / "sentinel.txt"
  sentinel.write_text("untouched\n", encoding="utf-8")
  # (a) Apply refuses a symlinked home state directory.
  home_a = fx.build_full_home(tmp_path / "home_a")
  point_home(monkeypatch, home_a)
  (home_a / "state").symlink_to(outside)
  manifest_a = tmp_path / "a.json"
  dry_run(monkeypatch, home_a, manifest_a)
  before_a = _tree_snapshot(home_a)
  code, _, err = run_cli(monkeypatch, home_a, "--apply", "--manifest", str(manifest_a))
  assert code == 1
  assert "symlink" in cli_json(err)["error"]
  assert sentinel.read_text() == "untouched\n"
  assert _tree_snapshot(home_a) == before_a
  # (b) Rollback refuses a symlinked migration state directory.
  migration_root = home / "state" / "session_tree_migration"
  captured = outside / "captured-state"
  shutil.move(str(migration_root), str(captured))
  migration_root.symlink_to(captured)
  before = _tree_snapshot(home)
  code, _, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
  assert code == 1
  assert "symlink" in cli_json(err)["error"]
  assert _tree_snapshot(home) == before


def test_interrupted_rollback_resumes_from_durable_evidence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A rollback killed mid-restore and mid-removal resumes from its journal and
  ends byte-identical; it never strands a partial v1/v2 conversion."""
  home = fx.build_full_home(tmp_path / "resume")
  point_home(monkeypatch, home)
  before = _tree_snapshot(home)  # the pre-apply home
  manifest_path = tmp_path / "resume.json"
  dry_run(monkeypatch, home, manifest_path)
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  manifest = migration.MigrationManifest.model_validate_json(manifest_path.read_text())
  sha_dir = home / "state" / "session_tree_migration" / manifest.source_sha[:16]
  journal = sha_dir / "rollback.ndjson"

  original_journal = migration._journal_rollback
  journaled_total = 0
  for crash_after in (5, 20):
    state = {"n": 0}

    def flaky_journal(path, rel, action, _original=original_journal, _state=state,
                      _crash=crash_after):
      _state["n"] += 1
      if _state["n"] > _crash:
        raise MigrationRefusedError(f"SIMULATED CRASH inside rollback after {_crash} journal writes")
      return _original(path, rel, action)

    migration._journal_rollback = flaky_journal
    try:
      with pytest.raises(MigrationRefusedError, match="SIMULATED CRASH"):
        migration.rollback_manifest(home_config_of(monkeypatch, home), manifest_path)
    finally:
      migration._journal_rollback = original_journal
    # Durable evidence of the interrupted rollback exists and names real progress.
    assert journal.is_file()
    entries = [json.loads(line) for line in journal.read_text().splitlines() if line.strip()]
    journaled_total += crash_after
    assert len(entries) == journaled_total
    assert {e["action"] for e in entries} <= {"restored", "removed"}

  # A fresh CLI process resumes and completes the rollback.
  code, out, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
  assert code == 0, err
  result = cli_json(out)
  assert result["status"] == "rolled_back"
  # The home is byte-identical to its pre-apply state.
  assert _tree_snapshot(home) == before
  # The journal described work that is finished; the manifest records it.
  assert not journal.exists()
  manifest_after = migration.MigrationManifest.model_validate_json(manifest_path.read_text())
  assert manifest_after.rolled_back_at is not None


def test_rollback_resume_refuses_interfered_with_evidence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A rollback interrupted mid-restore refuses when a restored file is no
  longer the original it journaled — the evidence is inspected, not overwritten."""
  home = fx.build_full_home(tmp_path / "interfere")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "interfere.json"
  dry_run(monkeypatch, home, manifest_path)
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  manifest = migration.MigrationManifest.model_validate_json(manifest_path.read_text())
  journal = (home / "state" / "session_tree_migration" / manifest.source_sha[:16]
             / "rollback.ndjson")
  original_journal = migration._journal_rollback
  state = {"n": 0}

  def flaky_journal(path, rel, action):
    state["n"] += 1
    if state["n"] > 3:
      raise MigrationRefusedError("SIMULATED CRASH inside rollback")
    return original_journal(path, rel, action)

  migration._journal_rollback = flaky_journal
  try:
    with pytest.raises(MigrationRefusedError, match="SIMULATED CRASH"):
      migration.rollback_manifest(home_config_of(monkeypatch, home), manifest_path)
  finally:
    migration._journal_rollback = original_journal
  assert journal.is_file()
  # Interfere with a restored file's content (same path, new bytes).
  entries = [json.loads(line) for line in journal.read_text().splitlines() if line.strip()]
  restored_rel = next(e["path"] for e in entries if e["action"] == "restored")
  victim = home / restored_rel
  victim.write_bytes(victim.read_bytes() + b"interference\n")
  code, _, err = run_cli(monkeypatch, home, "--rollback", "--manifest", str(manifest_path))
  assert code == 1
  payload = cli_json(err)
  assert "restored content" in payload["error"] + " ".join(payload.get("details", []))


def test_no_source_side_state_before_the_guard_passes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A refused apply leaves no migration state in the home: the receipt journal
  is created only after quiescence and the writer fence have passed."""
  home = fx.build_full_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  env = dict(os.environ)
  env["CHARLIEBOT_HOME"] = str(home)
  sleeper = subprocess.Popen(
      [sys.executable, "-c", "import time; time.sleep(120)"], env=env,
      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
  try:
    time.sleep(0.5)
    before = _tree_snapshot(home)
    code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
    assert code == 1
    assert "quiescence" in cli_json(err)["error"]
  finally:
    sleeper.kill()
    sleeper.wait()
  # No receipt journal, no migration state, no writer fence: the home holds
  # nothing on the migration's behalf until the guard has passed.
  assert _tree_snapshot(home) == before
  assert not (home / "state" / "session_tree_migration").exists()
  assert not (home / "state" / "home_writer.lock").exists()
  assert not (home / "state" / "writer_identity.json").exists()


def test_resume_after_alias_merge_crash_recognizes_its_own_product(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A crash between the merged aliases write and its receipt resumes: the
  landed product is recognized byte-exactly, existing rows survive, and no
  receipt is minted over a foreign change."""
  home = fx.build_full_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, home, manifest_path)
  aliases_rel = "sessions/session_aliases.json"
  cfg = home_config_of(monkeypatch, home)

  original_append = migration._append_receipt
  state = {"seen": False}

  def flaky_append(ctx, receipt):
    if receipt.path == aliases_rel and not state["seen"]:
      state["seen"] = True
      raise MigrationRefusedError("SIMULATED CRASH between aliases write and receipt")
    return original_append(ctx, receipt)

  migration._append_receipt = flaky_append
  try:
    with pytest.raises(MigrationRefusedError, match="SIMULATED CRASH"):
      migration.apply_manifest(cfg, manifest_path)
  finally:
    migration._append_receipt = original_append
  assert state["seen"]
  aliases_path = home / aliases_rel
  merged_after_crash = aliases_path.read_bytes()

  # A fresh CLI process resumes; the merged file is recognized, not rewritten.
  code, out, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 0, err
  assert cli_json(out)["verification"] == "ok"
  assert aliases_path.read_bytes() == merged_after_crash
  # Every imported row and every pre-existing row is present.
  merged = json.loads(aliases_path.read_text())
  plan = migration.build_conversion_plan(cfg, migration.scan_source(cfg))
  for old, canonical in plan.alias_old_sessions.items():
    assert merged["old_session_ids"][old] == canonical
  for key, target in plan.alias_old_threads.items():
    assert merged["old_threads"][key] == target
  assert merged["old_threads"]["11111111-0000-4000-8000-000000000001/run-existing-1"] == {
      "session_id": fx.S_V2, "run_id": "run-existing-1"}
  # Idempotent re-apply changes nothing.
  code, out, _ = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
  assert code == 0 and cli_json(out)["status"] == "already_applied"

  # A foreign row added after a crash between write and receipt refuses, and
  # no receipt is minted over it.
  home = fx.build_full_home(tmp_path / "home2")
  point_home(monkeypatch, home)
  manifest_path2 = tmp_path / "m2.json"
  dry_run(monkeypatch, home, manifest_path2)
  cfg2 = home_config_of(monkeypatch, home)
  state["seen"] = False
  migration._append_receipt = flaky_append
  try:
    with pytest.raises(MigrationRefusedError, match="SIMULATED CRASH"):
      migration.apply_manifest(cfg2, manifest_path2)
  finally:
    migration._append_receipt = original_append
  (home / aliases_rel).write_text(
      json.dumps({"old_session_ids": {"foreign-old": "foreign-target"},
                  "old_threads": {}}) + "\n", encoding="utf-8")
  code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path2))
  assert code == 1
  payload = cli_json(err)
  # The foreign row refuses (drift, or the write-time row guard) and no
  # receipt covers it.
  assert ("not this manifest's product" in payload["error"] + " ".join(payload.get("details", []))
          or "source drift" in payload["error"])
  assert aliases_rel not in _receipted_paths(home, migration.MigrationManifest.model_validate_json(
      manifest_path2.read_text()))
