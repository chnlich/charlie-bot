"""Session-tree migration: mapping coverage, apply/retry/rollback, boundaries.

Every scenario runs against fully synthetic homes built by
tests/session_tree_fixtures.py and drives the real CLI entrypoint
(``charliebot session-tree migrate``) plus the ordinary post-import readers
(TaskTreeManager's fold, RunStore, the alias store) — not converter internals
alone.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
from datetime import UTC, datetime
from pathlib import Path

import pytest
import session_tree_fixtures as fx
from conftest import reset_config_caches

from src.core import event_types as ET
from src.core import session_tree_migration as migration
from src.core.json_utils import atomic_write_text
from src.core.models import SessionMetadata, SessionStatus
from src.core.runs import RAW_LOG_NAME
from src.core.session_aliases import SessionAliasStore
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager


def home_config(home: Path):
  import src.core.config as core_config
  os.environ[core_config.CHARLIEBOT_HOME_ENV] = str(home)
  reset_config_caches()
  from src.core.config import get_config
  return get_config()


def point_home(monkeypatch: pytest.MonkeyPatch, home: Path) -> None:
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  reset_config_caches()


def run_cli(monkeypatch: pytest.MonkeyPatch, home: Path, *args: str) -> tuple[int, str, str]:
  """The real CLI dispatch path: charliebot session-tree migrate ...

  The captured stdout may carry structlog lines (e.g. the writer-fence
  acquisition) before the command's final JSON document; :func:`cli_json`
  extracts that document.
  """
  from src.cli import main as cli_main
  point_home(monkeypatch, home)
  monkeypatch.setattr(sys, "argv", ["charliebot", "session-tree", "migrate", *args])
  code = 0
  import contextlib
  import io
  out, err = io.StringIO(), io.StringIO()
  try:
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
      cli_main.main(["session-tree", "migrate", *args])
  except SystemExit as e:
    code = int(e.code or 0)
  return code, out.getvalue(), err.getvalue()


def cli_json(out: str) -> dict:
  """The command's final JSON document from a stdout that may carry log lines."""
  decoder = json.JSONDecoder()
  start = out.rfind("\n{")
  if start < 0:
    start = out.find("{")
  return decoder.decode(out[start:].strip())


def dry_run(monkeypatch: pytest.MonkeyPatch, home: Path, manifest_path: Path):
  code, out, err = run_cli(
      monkeypatch, home, "--dry-run", "--output", str(manifest_path))
  assert code in (0, 1), err
  manifest = migration.MigrationManifest.model_validate_json(manifest_path.read_text())
  return code, manifest, json.loads(out)


def tree_of(home: Path) -> TaskTreeManager:
  cfg = home_config(home)
  return TaskTreeManager(cfg, SessionManager(cfg))


# ---------------------------------------------------------------------------
# Inventory / dry-run
# ---------------------------------------------------------------------------


def _tree_snapshot(home: Path) -> dict[str, str]:
  snapshot = {}
  for path in sorted(home.rglob("*")):
    if path.is_file() and "state" not in path.relative_to(home).parts:
      snapshot[str(path.relative_to(home))] = migration._sha256_file(path)
  return snapshot


def test_full_home_dry_run_covers_all_eleven_categories(
    full_home: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  before = _tree_snapshot(full_home)
  code, manifest, summary = dry_run(monkeypatch, full_home, tmp_path / "manifest.json")
  assert code == 0
  assert manifest.unresolved == []
  dispositions = {(m.source_kind, m.disposition) for m in manifest.mappings}

  # Row 1: ordinary + PM sessions become manager nodes with original ids.
  assert ("ordinary", "manager_root") in dispositions
  assert ("pm", "manager_pm") in dispositions
  # Row 2: worker thread -> worker node with a work run (final verdicts name
  # the delivery evidence: one completed import, two unproven).
  assert ("worker_thread", "worker_work_completed") in dispositions
  assert ("worker_thread", "worker_work_unproven") in dispositions
  assert sum(1 for m in manifest.mappings if m.source_kind == "worker_thread") == 3
  # Row 3: review_of -> review run on the reviewed worker.
  assert ("review_thread", "worker_review") in dispositions
  # Row 4: improve loop + iterations.
  assert ("improve_loop", "worker_improve_loop") in dispositions
  assert ("improve_iteration", "worker_iteration") in dispositions
  # Row 5: cron chain steps.
  assert ("cron_chain_step", "worker_scheduled_step") in dispositions
  # Row 6: scheduled session + cron binding + trigger.
  assert ("scheduled", "manager_scheduled") in dispositions
  assert ("cron_config", "cron_binding") in dispositions
  assert ("trigger", "trigger_kept") in dispositions
  # Row 7: fork provenance stays independent; elone predecessor aliases to the tail.
  assert ("elone_predecessor", "alias_to_tail") in dispositions
  # Row 8: unproven archived + completed-import verdicts.
  assert ("worker_thread", "worker_work_unproven") in dispositions
  assert ("worker_thread", "worker_work_completed") in dispositions
  # Row 9: project bodies (PM node carries subtree + node prompt refs).
  # Row 10: one proven-pending input.
  assert summary["pending_inputs"] == 1
  # Mixed v2 data preserved.
  assert ("v2_task", "already_v2") in dispositions
  # Historical manager turn logs.
  assert ("manager_turn_log", "manager_turn_run") in dispositions

  # Every manifest source file exists and every mapping accounts for a real
  # source; the report covers the whole input set (no silent omissions).
  for record in manifest.source_files:
    assert (full_home / record.path).is_file()
  assert summary["input_summary"]["mappings"] == len(manifest.mappings)
  # The archive rotation is part of the bound input set.
  assert any("archives" in r.path for r in manifest.source_files)
  # Dry-run mutated nothing outside the migration state dir.
  assert _tree_snapshot(full_home) == before


def test_repeated_dry_runs_preserve_source_derived_identity(
    full_home: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  _, first, _ = dry_run(monkeypatch, full_home, tmp_path / "a.json")
  _, second, _ = dry_run(monkeypatch, full_home, tmp_path / "b.json")
  assert first.source_sha == second.source_sha
  assert first.source_files == second.source_files
  identity_a = sorted((m.source_kind, m.source_id, m.target_session_id, m.target_run_id)
                      for m in first.mappings)
  identity_b = sorted((m.source_kind, m.source_id, m.target_session_id, m.target_run_id)
                      for m in second.mappings)
  assert identity_a == identity_b
  worker_ids = {m.target_session_id for m in first.mappings
                if m.source_kind in ("worker_thread", "cron_chain_step", "improve_iteration")}
  # work + completed + review-owner + cron chain + improve loop
  assert len(worker_ids) == 5
  # The cron chain's two steps share one node; the two iterations share one.
  cron_targets = {m.target_session_id for m in first.mappings if m.source_kind == "cron_chain_step"}
  assert len(cron_targets) == 1
  improve_targets = {m.target_session_id for m in first.mappings
                     if m.source_kind in ("improve_iteration", "improve_loop")}
  assert len(improve_targets) == 1


def test_dry_run_via_cli_prints_reviewable_manifest(
    full_home: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  code, out, _ = run_cli(monkeypatch, full_home, "--dry-run", "--output", str(tmp_path / "m.json"))
  assert code == 0
  payload = cli_json(out)
  assert payload["status"] == "dry_run"
  assert payload["unresolved_count"] == 0
  assert (tmp_path / "m.json").is_file()


# ---------------------------------------------------------------------------
# Apply + ordinary readers
# ---------------------------------------------------------------------------


def test_apply_then_ordinary_readers(full_home: Path, monkeypatch: pytest.MonkeyPatch,
                                     tmp_path: Path) -> None:
  manifest_path = tmp_path / "manifest.json"
  dry_run(monkeypatch, full_home, manifest_path)
  code, out, err = run_cli(monkeypatch, full_home, "--apply", "--manifest", str(manifest_path))
  assert code == 0, err
  result = cli_json(out)
  assert result["status"] == "applied"
  assert result["verification"] == "ok"

  tree = tree_of(full_home)
  index = asyncio.run(tree._get_index(force=True))
  metas = index.metas

  # Managers keep original ids; the elone predecessor is alias-only history.
  for sid in (fx.S_ORDINARY, fx.S_PENDING, fx.S_PM, fx.S_ARCHIVED, fx.S_FORK,
              fx.S_TAIL, fx.S_SCHEDULED, fx.S_STEPS, fx.S_IMPROVE, fx.S_WORKERS):
    assert metas[sid].profile == "manager", sid
  assert metas[fx.S_PREDECESSOR].profile is None
  assert metas[fx.S_V2].profile == "manager"  # mixed v2 untouched

  # Worker targets are children of their canonical owners.
  workers = [m for m in metas.values() if m.profile == "worker"]
  owners = {m.task_parent_id for m in workers}
  assert owners == {fx.S_ORDINARY, fx.S_STEPS, fx.S_IMPROVE, fx.S_WORKERS}
  by_owner: dict[str, list] = {}
  for worker in workers:
    by_owner.setdefault(worker.task_parent_id, []).append(worker)

  # Stable source-derived ids: the same manifest re-derived from a fresh dry-run
  # resolves to the same tasks.
  _, manifest2, _ = dry_run(monkeypatch, full_home, tmp_path / "m2.json")
  worker_ids = {m.target_session_id for m in manifest2.mappings
                if m.source_kind in ("worker_thread", "cron_chain_step",
                                     "improve_iteration", "improve_loop")}
  assert worker_ids == {w.id for w in workers}

  # Chronology: the cron chain's two scheduled_step runs are positions 0, 1.
  steps_node = by_owner[fx.S_STEPS][0]
  runs = tree.runs.list_run_records_sync(steps_node.id)
  positions = sorted(r.sequence_ref.position for r in runs)
  assert positions == [0, 1]
  assert all(r.kind == "scheduled_step" for r in runs)

  # Exact history/log hashes: the work run's raw log reference still resolves
  # byte-for-byte to the original retained evidence.
  ordinary_worker = by_owner[fx.S_ORDINARY][0]
  run = tree.runs.list_run_records_sync(ordinary_worker.id)[0]
  assert run.native_session_id == "native-worker-deploy"
  original_raw = (full_home / "sessions" / fx.S_ORDINARY / "threads" / fx.T_WORK
                  / "data" / RAW_LOG_NAME)
  # The original thread kept no raw log in this fixture; the run records that
  # honestly (no reference manufactured).
  assert run.raw_log_ref is None
  assert not original_raw.exists()

  # Old thread URLs resolve through the ordinary alias read.
  aliases = SessionAliasStore(full_home / "sessions")
  resolved = aliases.resolve_thread(fx.S_ORDINARY, fx.T_WORK)
  assert resolved == {"session_id": ordinary_worker.id, "run_id": run.id}

  # Review run sits on the reviewed worker with review_of pointing at its work run.
  hub_workers = by_owner[fx.S_WORKERS]
  review_target = next(w for w in hub_workers if any(
      r.kind == "review" for r in tree.runs.list_run_records_sync(w.id)))
  hub_runs = tree.runs.list_run_records_sync(review_target.id)
  work_run = next(r for r in hub_runs if r.kind == "work")
  review_run = next(r for r in hub_runs if r.kind == "review")
  assert review_run.review_of_run_id == work_run.id

  # Prompts: the PM node references the imported bodies through the ordinary
  # prompt reader, within its old scope only.
  pm = metas[fx.S_PM]
  from src.core.task_prompts import read_local_rule_body
  common = read_local_rule_body(full_home / "prompt_bodies", pm.subtree_prompt_ref,
                                owner=fx.S_PM, scope="subtree")
  assert "shared ops workflow" in common
  node_rules = read_local_rule_body(full_home / "prompt_bodies", pm.node_prompt_ref,
                                    owner=fx.S_PM, scope="node")
  assert "ledger" in node_rules
  # The unorganized root references the SAME body version (shared-version reuse).
  root = metas[fx.S_PENDING]
  assert root.subtree_prompt_ref == pm.subtree_prompt_ref
  # A group label never extends rule scope to unrelated tasks.
  assert metas[fx.S_ORDINARY].subtree_prompt_ref is None

  # Archive presentation: the old archived session imports hidden; the active
  # scheduled session is not paused; nothing fired (no pending trigger fired).
  assert tree.archived_of(index, metas[fx.S_ARCHIVED]) is True
  assert metas[fx.S_SCHEDULED].automation_paused is False
  assert metas[fx.S_SCHEDULED].scheduled_task == "nightly-sweep"

  # Slack origin kept resolvable.
  assert metas[fx.S_PENDING].slack_origin is not None

  # Fork provenance stays provenance, never decomposition.
  assert metas[fx.S_FORK].task_parent_id is None
  assert metas[fx.S_FORK].parent_session_id == fx.S_ORDINARY


def test_apply_is_idempotent_and_repeatable(
    full_home: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  manifest_path = tmp_path / "manifest.json"
  dry_run(monkeypatch, full_home, manifest_path)
  first = cli_json(run_cli(monkeypatch, full_home, "--apply",
                           "--manifest", str(manifest_path))[1])
  assert first["status"] == "applied"
  second = cli_json(run_cli(monkeypatch, full_home, "--apply",
                            "--manifest", str(manifest_path))[1])
  assert second["status"] == "already_applied"
  # No duplicated facts or nodes.
  tree = tree_of(full_home)
  index = asyncio.run(tree._get_index(force=True))
  imported_events = [
      e for sid in index.metas
      for e in tree.facts_of(sid).events_by_id.values()
      if e.get("type") == ET.TASK_IMPORTED]
  # 10 legacy managers (the elone predecessor stays history) + 5 workers; the
  # pre-existing v2 node keeps its own task_created boundary.
  assert len(imported_events) == 15
  assert sum(1 for m in index.metas.values() if m.profile == "worker") == 5


def test_import_boundary_and_recovery_facts(
    full_home: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  manifest_path = tmp_path / "manifest.json"
  dry_run(monkeypatch, full_home, manifest_path)
  assert run_cli(monkeypatch, full_home, "--apply", "--manifest", str(manifest_path))[0] == 0

  tree = tree_of(full_home)
  asyncio.run(tree._get_index(force=True))
  # Old handled USER: confirmed by the manager turn's durable run_finished fact.
  facts = tree.facts_of(fx.S_ORDINARY)
  run_finished = [e for e in facts.events_by_id.values() if e.get("type") == ET.RUN_FINISHED]
  confirmed = {i for e in run_finished for i in (e.get("input_event_ids") or [])}
  handled_id = f"{fx.S_ORDINARY[:8]}-0000-0000-0000-00000000000a"
  assert handled_id in confirmed
  # Old handled USER is NOT a pending input of the migrated manager.
  pending = tree.dispatch.pending_inputs(fx.S_ORDINARY)
  assert [e["id"] for e in pending] == []
  # The confirmed unhandled USER enters pending_inputs with a precise source ref.
  pending_pending = tree.dispatch.pending_inputs(fx.S_PENDING)
  assert [e["id"] for e in pending_pending] == [
      f"{fx.S_PENDING[:8]}-0000-0000-0000-00000000000a"]
  # The old child report (worker_summary) is history, not an input candidate.
  # No old USER event is reissued and no new USER event exists at migration time.
  user_events = [e for e in tree.facts_of(fx.S_ORDINARY).events_by_id.values()
                 if e.get("type") == ET.USER]
  # The rotated round stays in the fold's fact history (archives + live).
  assert [e["id"] for e in user_events] == [
      f"{fx.S_ORDINARY[:8]}-0000-0000-0000-00000000000e",
      f"{fx.S_ORDINARY[:8]}-0000-0000-0000-00000000000a"]

  # A later post-import input joins the pending queue through the normal fold.
  from src.core.ndjson import append_ndjson_sync
  post_input = {"id": "post-import-1", "type": ET.USER,
                "timestamp": datetime.now(UTC).isoformat(), "actor": "user",
                "source_session_id": fx.S_PENDING, "content": "post-import ask"}
  append_ndjson_sync(full_home / "sessions" / fx.S_PENDING / "data" / "chat_events.jsonl",
                     post_input)
  tree2 = tree_of(full_home)
  asyncio.run(tree2._get_index(force=True))
  pending_after = tree2.dispatch.pending_inputs(fx.S_PENDING)
  assert [e["id"] for e in pending_after] == [
      f"{fx.S_PENDING[:8]}-0000-0000-0000-00000000000a", "post-import-1"]
  # A fresh fold/recovery does not replay the historical handled event.
  from src.core.init_master_recovery import unanswered_user_events
  events = tree2.fact_history(fx.S_ORDINARY)
  assert unanswered_user_events(events, set()) == []


def test_manager_turn_runs_and_uncertain_logs(
    full_home: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  # An extra manager turn log without a result: uncertain historical evidence.
  raw_no_result = json.dumps({"type": "assistant", "message": {"content": []}}) + "\n"
  turn_dir = full_home / "sessions" / fx.S_ORDINARY / "data" / "master_runs" / fx.iso(90)
  turn_dir.mkdir(parents=True)
  (turn_dir / RAW_LOG_NAME).write_text(raw_no_result, encoding="utf-8")

  manifest_path = tmp_path / "manifest.json"
  code, manifest, _ = dry_run(monkeypatch, full_home, manifest_path)
  assert code == 0 and manifest.unresolved == []
  turn_logs = [m for m in manifest.mappings if m.source_kind == "manager_turn_log"]
  proven = [m for m in turn_logs if m.disposition == "manager_turn_run"]
  uncertain = [m for m in turn_logs if m.disposition == "historical_evidence"]
  assert len(proven) == 3  # 2 in the ordinary session + 1 predecessor turn (tail's)
  assert len(uncertain) == 1
  assert uncertain[0].reason and "raw_log_ref" in uncertain[0].detail

  assert run_cli(monkeypatch, full_home, "--apply", "--manifest", str(manifest_path))[0] == 0
  tree = tree_of(full_home)
  asyncio.run(tree._get_index(force=True))
  turns = [r for r in tree.runs.list_run_records_sync(fx.S_ORDINARY) if r.kind == "manager_turn"]
  assert len(turns) == 2
  outcomes = {r.id: tree.runs.terminal_outcome(
      tree.runs.load_events_sync(fx.S_ORDINARY), r.id) for r in turns}
  assert set(outcomes.values()) == {"success"}
  # The turn whose raw log carries the handled input's content carries its id.
  raw_content = (full_home / "sessions" / fx.S_ORDINARY / "data" / "master_runs"
                 / fx.iso(10) / RAW_LOG_NAME).read_bytes()
  assert b"deploy pipeline" in raw_content
  named = [r for r in turns if r.input_event_ids]
  assert len(named) == 1


def test_completed_import_requires_evidence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # (a) A completed quick-edit with a success result imports completed, with
  # the delivery receipt delivered to the parent.
  home = fx.build_full_home(tmp_path / "h1")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m1.json"
  dry_run(monkeypatch, home, manifest_path)
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  tree = tree_of(home)
  asyncio.run(tree._get_index(force=True))
  completed_workers = [m for m in index_metas(tree) if m.profile == "worker"
                       and tree.facts_of(m.id).task_state == "completed"]
  assert len(completed_workers) == 1
  child = completed_workers[0]
  facts = tree.facts_of(child.id)
  close = [e for e in facts.events_by_id.values() if e.get("type") == ET.TASK_CLOSED]
  assert len(close) == 1 and close[0]["outcome"] == "completed"
  parent_facts = tree.facts_of(fx.S_WORKERS)
  delivered = [e for e in parent_facts.events_by_id.values() if e.get("type") == ET.CHILD_REPORT]
  assert len(delivered) == 1
  assert delivered[0]["child_event_id"] == close[0]["id"]

  # (b) A completed implement worker whose review failed stays open (unproven):
  # required implement evidence cannot be forged into completion.
  home2 = fx.build_unproven_implement_home(tmp_path / "h2")
  point_home(monkeypatch, home2)
  manifest2_path = tmp_path / "m2.json"
  code, manifest2, summary = dry_run(monkeypatch, home2, manifest2_path)
  assert code == 0 and manifest2.unresolved == []
  verdicts = {m.disposition for m in manifest2.mappings if m.source_kind == "worker_thread"}
  assert "worker_work_unproven" in verdicts
  assert run_cli(monkeypatch, home2, "--apply", "--manifest", str(manifest2_path))[0] == 0
  tree2 = tree_of(home2)
  asyncio.run(tree2._get_index(force=True))
  worker = next(m for m in index_metas(tree2) if m.profile == "worker")
  assert tree2.facts_of(worker.id).task_state != "completed"
  mapping = next(m for m in manifest2.mappings if m.source_kind == "worker_thread")
  assert "unproven" in (mapping.reason or "")

  # (c) A completed implement worker with review AND a landed branch imports
  # completed (the full evidence set).
  home3 = fx.build_completed_implement_home(tmp_path / "h3")
  point_home(monkeypatch, home3)
  manifest3_path = tmp_path / "m3.json"
  code, manifest3, _ = dry_run(monkeypatch, home3, manifest3_path)
  assert code == 0
  mapping3 = next(m for m in manifest3.mappings if m.source_kind == "worker_thread")
  assert mapping3.disposition == "worker_work_completed"
  assert run_cli(monkeypatch, home3, "--apply", "--manifest", str(manifest3_path))[0] == 0
  tree3 = tree_of(home3)
  asyncio.run(tree3._get_index(force=True))
  worker3 = next(m for m in index_metas(tree3) if m.profile == "worker")
  assert tree3.facts_of(worker3.id).task_state == "completed"


def index_metas(tree: TaskTreeManager):
  return asyncio.run(tree._get_index()).metas.values()


def test_archived_worker_stays_open_but_hidden(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = fx.build_full_home(tmp_path / "home")
  # Archive the ordinary owner: its unproven worker keeps hidden presentation.
  meta_path = home / "sessions" / fx.S_ORDINARY / "metadata.json"
  meta = SessionMetadata.model_validate_json(meta_path.read_text())
  meta.status = SessionStatus.ARCHIVED
  atomic_write_text(meta_path, meta.model_dump_json(indent=2, exclude={
      "has_running_tasks", "has_pending_trigger", "pending_trigger_count",
      "next_trigger_at", "has_pending_plan_approval", "schedule_cron", "schedule_enabled",
      "schedule_next_run", "schedule_timezone", "schedule_project",
      "schedule_allow_failure", "thinking_since"}))
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0
  worker_mapping = next(m for m in manifest.mappings if m.source_kind == "worker_thread")
  assert worker_mapping.disposition == "worker_work_unproven"
  assert "kept_hidden" in worker_mapping.detail
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  tree = tree_of(home)
  index = asyncio.run(tree._get_index(force=True))
  worker = next(m for m in index.metas.values() if m.profile == "worker"
                and m.task_parent_id == fx.S_ORDINARY)
  assert tree.facts_of(worker.id).task_state != "completed"
  assert worker.presentation == "hidden"


def test_cron_binding_and_trigger_rebinding(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  home = fx.build_full_home(tmp_path / "home")
  # A delayed trigger on the elone predecessor rebinds to the canonical tail.
  from src.core.models import PendingTrigger
  builder_trigger = PendingTrigger(
      id="33333333-0000-4000-8000-00000000000a", session_id=fx.S_PREDECESSOR,
      fire_at=fx.BASE + timedelta_compat(hours=1), message="pred wake")
  (home / "sessions" / fx.S_PREDECESSOR / "triggers").mkdir(parents=True, exist_ok=True)
  atomic_write_text(
      home / "sessions" / fx.S_PREDECESSOR / "triggers" / f"{builder_trigger.id}.json",
      builder_trigger.model_dump_json(indent=2))
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0
  rebound = [m for m in manifest.mappings if m.disposition == "trigger_rebound"]
  assert len(rebound) == 1
  assert rebound[0].target_session_id == fx.S_TAIL
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  # The moved file lives under the tail with the rebound session_id; the old
  # path is gone; watch targets preserved; nothing fired.
  moved_path = home / "sessions" / fx.S_TAIL / "triggers" / f"{builder_trigger.id}.json"
  assert moved_path.is_file()
  assert not (home / "sessions" / fx.S_PREDECESSOR / "triggers").exists() or not any(
      (home / "sessions" / fx.S_PREDECESSOR / "triggers").iterdir())
  moved = json.loads(moved_path.read_text())
  assert moved["session_id"] == fx.S_TAIL
  # The cron config gained the explicit binding; role/group discovery replaced.
  import yaml
  body = yaml.safe_load((home / "config.d" / "cron.d" / "nightly-sweep.yaml").read_text())
  assert body["session_id"] == fx.S_SCHEDULED
  assert "project" not in body
  # The pending trigger is preserved untouched in status (never fired).
  kept = json.loads((home / "sessions" / fx.S_SCHEDULED / "triggers"
                     / "22222222-0000-4000-8000-000000000009.json").read_text())
  assert kept["status"] == "pending"


def timedelta_compat(**kwargs):
  from datetime import timedelta
  return timedelta(**kwargs)


def test_mixed_v2_data_preserved(
    full_home: Path, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  v2_dir = full_home / "sessions" / fx.S_V2
  before_meta = (v2_dir / "metadata.json").read_text()
  before_run = (v2_dir / "data" / "runs" / "run-existing-1" / "metadata.json").read_text()
  before_log = (v2_dir / "data" / "chat_events.jsonl").read_text()
  manifest_path = tmp_path / "m.json"
  dry_run(monkeypatch, full_home, manifest_path)
  assert run_cli(monkeypatch, full_home, "--apply", "--manifest", str(manifest_path))[0] == 0
  assert (v2_dir / "metadata.json").read_text() == before_meta
  assert (v2_dir / "data" / "runs" / "run-existing-1" / "metadata.json").read_text() == before_run
  assert (v2_dir / "data" / "chat_events.jsonl").read_text() == before_log
  # The existing alias row survives the merged write.
  aliases = json.loads((full_home / "sessions" / "session_aliases.json").read_text())
  assert aliases["old_threads"]["11111111-0000-4000-8000-000000000001/run-existing-1"] == {
      "session_id": fx.S_V2, "run_id": "run-existing-1"}
