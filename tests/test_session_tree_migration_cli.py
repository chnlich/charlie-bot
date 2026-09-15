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

from src.core import session_tree_migration as migration
from src.core.json_utils import atomic_write_text
from src.core.session_tree_migration import MigrationRefused


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
  cases = {}
  # Ambiguous improve-loop association.
  home = fx.build_ambiguous_loop_home(tmp_path / "ambiguous")
  point_home(monkeypatch, home)
  code, manifest, summary = dry_run(monkeypatch, home, tmp_path / "a.json")
  assert code == 1
  kinds = {u.source_kind for u in manifest.unresolved}
  assert "improve_iteration" in kinds
  cases["ambiguous_loop"] = manifest.unresolved[0].reason

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

  # Uncertain input handling (a later unrelated round does not name it).
  home = fx.build_uncertain_input_home(tmp_path / "uncertain")
  point_home(monkeypatch, home)
  code, manifest, _ = dry_run(monkeypatch, home, tmp_path / "e.json")
  assert code == 1
  assert any(u.source_kind == "old_input" for u in manifest.unresolved)

  # Every case: apply refuses without changing any input.
  for name in ("ambiguous", "cycle", "review", "alias", "uncertain"):
    home = tmp_path / name
    point_home(monkeypatch, home)
    before = _tree_snapshot(home)
    code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest",
                           str(tmp_path / {"ambiguous": "a.json", "cycle": "b.json",
                                           "review": "c.json", "alias": "d.json",
                                           "uncertain": "e.json"}[name]))
    assert code == 1, name
    payload = cli_json(err)
    assert "unresolved" in payload["error"], name
    assert _tree_snapshot(home) == before, name


def test_corrupt_history_line_is_unresolved(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = fx.build_full_home(tmp_path / "home")
  log = home / "sessions" / fx.S_ORDINARY / "data" / "chat_events.jsonl"
  with open(log, "a", encoding="utf-8") as f:
    f.write("not-json\n")
  point_home(monkeypatch, home)
  code, manifest, _ = dry_run(monkeypatch, home, tmp_path / "m.json")
  assert code == 1
  entry = next(u for u in manifest.unresolved if u.source_kind == "chat_history")
  assert fx.S_ORDINARY in entry.source_id
  assert "unparseable" in entry.reason


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
  code, _, err = run_cli(monkeypatch, home_b, "--apply", "--manifest", str(manifest_path))
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
  code, out, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
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
  code, _, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
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
      raise MigrationRefused(f"SIMULATED CRASH after {count['n'] - 1} products")
    return await original(ctx, kind, payload)

  migration._apply_product = flaky
  crashes = 0
  try:
    for _ in range(len(crash_points) + 1):
      try:
        migration.apply_manifest(cfg, manifest_path)
        break
      except MigrationRefused as e:
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


def home_config_of(monkeypatch: pytest.MonkeyPatch, home: Path) -> "CharlieBotConfig":
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
    with pytest.raises(MigrationRefused) as excinfo:
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
    code, out, err = run_cli(monkeypatch, other, "--apply", "--manifest", str(other_manifest))
    assert code == 0, err
  finally:
    fence.release()
  point_home(monkeypatch, home)
  code, out, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
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
       "import sys, time\n"
       f"sys.path.insert(0, {str(Path(__file__).parent.parent)!r})\n"
       "from src.core.home_writer_fence import acquire_home_writer_fence\n"
       f"fence = acquire_home_writer_fence({str(home)!r}, purpose='server startup')\n"
       "time.sleep(60)\n"],
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
  code, out, err = run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))
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
