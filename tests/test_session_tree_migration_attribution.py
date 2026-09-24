"""Migration attribution repair: input/Run/loop/delivery/append-prove evidence.

Every case drives the real migration CLI or the owning converter functions on
fully synthetic retained files written in the legacy producers' formats, and
asserts the causal contract (which fact proves which disposition), not a
helper's current heuristic. Companion to tests/test_session_tree_migration.py,
which owns the end-to-end apply/retry/rollback flows.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
import session_tree_fixtures as fx
from conftest import reset_config_caches

from src.core import event_types as ET
from src.core import session_tree_migration as migration
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager


def point_home(monkeypatch: pytest.MonkeyPatch, home: Path) -> None:
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  reset_config_caches()


def run_cli(monkeypatch: pytest.MonkeyPatch, home: Path, *args: str) -> tuple[int, str, str]:
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
  decoder = json.JSONDecoder()
  start = out.rfind("\n{")
  if start < 0:
    start = out.find("{")
  return decoder.decode(out[start:].strip())


def dry_run(monkeypatch: pytest.MonkeyPatch, home: Path, manifest_path: Path):
  code, out, err = run_cli(monkeypatch, home, "--dry-run", "--output", str(manifest_path))
  assert code in (0, 1), err
  manifest = migration.MigrationManifest.model_validate_json(manifest_path.read_text())
  return code, manifest, json.loads(out)


def tree_of(home: Path) -> TaskTreeManager:
  os.environ["CHARLIEBOT_HOME"] = str(home)
  reset_config_caches()
  import src.core.config as core_config
  cfg = core_config.get_config()
  return TaskTreeManager(cfg, SessionManager(cfg))


def manager_mapping(manifest, session_id: str):
  return next(m for m in manifest.mappings
              if m.source_kind not in {"manager_turn_log", "worker_thread"} and m.source_id == session_id)


def worker_mapping(manifest: migration.MigrationManifest, needle: str):
  return next(m for m in manifest.mappings
              if m.source_kind == "worker_thread" and needle in m.source_id)


def run_records(tree: TaskTreeManager, session_id: str, kind: str | None = None):
  return [r for r in tree.runs.list_run_records_sync(session_id)
          if kind is None or r.kind == kind]


def terminal(tree: TaskTreeManager, session_id: str, run_id: str):
  return tree.runs.terminal_outcome(tree.runs.load_events_sync(session_id), run_id)


def input_ids_of(tree: TaskTreeManager, session_id: str, run_id: str) -> list[str]:
  record = next(r for r in tree.runs.list_run_records_sync(session_id) if r.id == run_id)
  return list(record.input_event_ids)


# ---------------------------------------------------------------------------
# Historical input disposition and Run ownership
# ---------------------------------------------------------------------------


def test_failed_named_round_preserved_and_retry_confirms(tmp_path, monkeypatch):
  """A failed named round stays a failed execution; a later proven retry confirms.

  The failed round's raw stream settled with a success-shaped result but the
  consumer's MASTER_DONE records exit 1: the round's own completion fact
  outranks the raw shape, the failed run_finished acknowledges nothing, and
  the successful retry binds the same input id to its own run without erasing
  the failed attempt.
  """
  home = fx.build_failed_rounds_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0 and manifest.unresolved == []
  sid = fx.S_FAILED_ROUNDS
  u_retry = f"{sid[:8]}-0000-0000-0000-00000000000a"
  u_zero = f"{sid[:8]}-0000-0000-0000-00000000000d"
  summary = manager_mapping(manifest, sid).detail["input_disposition"]
  assert summary["confirmed_bound"] == 1  # the retry round
  assert summary["failed_attempts_bound"] == 2  # the failed round + the zero-output round
  assert summary["pending"] == 1  # u_zero waits behind its standing failed run
  assert summary["confirmed_unbound"] == 0
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  tree = tree_of(home)
  turns = run_records(tree, sid, "manager_turn")
  assert len(turns) == 3
  outcomes = {r.id: terminal(tree, sid, r.id) for r in turns}
  assert sorted(outcomes.values()) == ["failed", "failed", "success"]
  retry_runs = [r for r in turns if u_retry in input_ids_of(tree, sid, r.id)]
  assert len(retry_runs) == 2  # the failed attempt and the successful retry
  retry_outcomes = {r.id: outcomes[r.id] for r in retry_runs}
  assert sorted(retry_outcomes.values()) == ["failed", "success"]
  zero_runs = [r for r in turns if u_zero in input_ids_of(tree, sid, r.id)]
  assert len(zero_runs) == 1 and outcomes[zero_runs[0].id] == "failed"
  # History-only inputs: the zero-output input stays a failed execution with
  # its exact log and imports as history, never re-queued; the report names it.
  events = tree.fact_history(sid)
  boundary = [e for e in events if e.get("type") == ET.TASK_IMPORTED]
  assert len(boundary) == 1
  assert boundary[0]["pending_inputs"] == []
  assert f"{sid}:{u_zero}" in [u.source_id for u in manifest.import_report]


def test_failed_inflight_result_is_not_an_acknowledgement(tmp_path, monkeypatch):
  """The recorded in-flight turn's retained failed result is not successful handling."""
  home = fx.build_inflight_failed_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0 and manifest.unresolved == []
  sid = fx.S_UNBOUND
  u = f"{sid[:8]}-0000-0000-0000-00000000000a"
  summary = manager_mapping(manifest, sid).detail["input_disposition"]
  assert summary == {"confirmed_bound": 0, "confirmed_unbound": 0,
                     "failed_attempts_bound": 1, "pending": 1, "uncertain": 0}
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  tree = tree_of(home)
  turns = run_records(tree, sid, "manager_turn")
  assert len(turns) == 1
  assert terminal(tree, sid, turns[0].id) == "failed"
  assert input_ids_of(tree, sid, turns[0].id) == [u]


def test_proven_success_without_run_binding_stays_unbound(tmp_path, monkeypatch):
  """A proven successful round with a missing log is handled but not bound to a Run."""
  home = fx.build_unbound_success_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0 and manifest.unresolved == []
  sid = fx.S_UNBOUND
  summary = manager_mapping(manifest, sid).detail["input_disposition"]
  assert summary["confirmed_unbound"] == 1
  assert summary["pending"] == 0
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  tree = tree_of(home)
  assert run_records(tree, sid, "manager_turn") == []
  # The unbound handling fact never becomes a pending input and never
  # invents a run attribution.
  events = tree.fact_history(sid)
  boundary = [e for e in events if e.get("type") == ET.TASK_IMPORTED]
  assert boundary[0]["pending_inputs"] == []
  assert not any(r.input_event_ids for r in run_records(tree, sid))


def test_malformed_input_times_follow_the_positional_replay_rule(tmp_path, monkeypatch):
  """Missing/unparseable input timestamps never decide the disposition.

  The old system's replay rule (unanswered_user_events) is positional: an
  input with no MASTER_DONE after it is read as unhandled even when its own
  timestamp is unparseable; one shadowed by a later completed unnamed round
  is unproven. Under the history-only input policy both import as reported
  history and neither queues.
  """
  home = fx.build_malformed_times_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0, manifest.unresolved
  sid = fx.S_TIMES
  u1 = f"{sid[:8]}-0000-0000-0000-00000000000a"
  u2 = f"{sid[:8]}-0000-0000-0000-00000000000b"
  summary = manager_mapping(manifest, sid).detail["input_disposition"]
  assert summary["pending"] == 1
  assert summary["uncertain"] == 1
  report = {u.source_id: u.reason for u in manifest.import_report}
  assert "replay rule" in report[f"{sid}:{u1}"]
  assert "replay rule" not in report[f"{sid}:{u2}"]
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  boundary = [e for e in tree_of(home).fact_history(sid) if e.get("type") == ET.TASK_IMPORTED]
  assert boundary[0]["pending_inputs"] == []


def test_scheduled_wake_without_proven_handling_is_reported(tmp_path, monkeypatch):
  """A scheduled wake consumed by nothing provable is reported history, never a pending input."""
  home = fx.build_scheduled_unproven_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0, manifest.unresolved
  sid = fx.S_SCHED_UNPROVEN
  st_int = f"{sid[:8]}-0000-0000-0000-00000000000a"
  st_echo = f"{sid[:8]}-0000-0000-0000-00000000000b"
  report_ids = [u.source_id for u in manifest.import_report]
  assert f"{sid}:{st_int}" in report_ids  # its round started and never completed
  assert f"{sid}:{st_echo}" in report_ids  # no identity-backed launch echo
  summary = manager_mapping(manifest, sid).detail["input_disposition"]
  assert summary["pending"] == 0 and summary["confirmed_bound"] == 0
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0


def test_identical_request_bodies_bind_to_their_own_rounds(tmp_path, monkeypatch):
  """Identical text in two requests, plus a later quoting transcript.

  Binding follows the retained round identity (marker session id + producer
  write ordering); identical text in a later transcript and an assistant
  quote transfer nothing.
  """
  home = fx.build_identical_requests_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0 and manifest.unresolved == []
  sid = fx.S_IDENTICAL
  u1 = f"{sid[:8]}-0000-0000-0000-00000000000a"
  u2 = f"{sid[:8]}-0000-0000-0000-00000000000b"
  summary = manager_mapping(manifest, sid).detail["input_disposition"]
  assert summary["confirmed_bound"] == 2
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  tree = tree_of(home)
  turns = run_records(tree, sid, "manager_turn")
  assert len(turns) == 3  # two request rounds + the later quoting turn
  bound = {r.id: input_ids_of(tree, sid, r.id) for r in turns}
  by_log = {r.raw_log_ref.split("/")[-2]: r.id for r in turns}
  assert bound[by_log[fx.iso(10)]] == [u1]
  assert bound[by_log[fx.iso(70)]] == [u2]
  # The quoting turn acknowledges nothing and carries no invented batch.
  assert bound[by_log[fx.iso(120)]] == []


def test_resumed_round_binds_the_resumed_transport(tmp_path, monkeypatch):
  """A crash-then-resume round: the MASTER_DONE closes the LATEST open round.

  Both launches adopt the same backend session id (the resume anchor), so
  adoption identity alone cannot separate the two transports; the old
  projection's interval rule — the round the consumer settles is the latest
  opened one — binds the input to the resumed turn's own raw log. The dead
  turn's unsettled log inherits nothing.
  """
  home = fx.build_resume_bound_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0 and manifest.unresolved == []
  sid = fx.S_RESUME
  u = f"{sid[:8]}-0000-0000-0000-00000000000a"
  summary = manager_mapping(manifest, sid).detail["input_disposition"]
  assert summary["confirmed_bound"] == 1
  assert summary["confirmed_unbound"] == 0
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  tree = tree_of(home)
  turns = run_records(tree, sid, "manager_turn")
  assert len(turns) == 1  # the crashed transport is unproven history, not a Run
  assert input_ids_of(tree, sid, turns[0].id) == [u]
  assert turns[0].raw_log_ref.endswith(f"{fx.iso(60)}/agent.raw.ndjson")


def test_straddled_scheduled_wake_binds_its_own_round(tmp_path, monkeypatch):
  """A wake fired inside an earlier identical-text round binds only its OWN round.

  Both transports echo the recurring wake text, so an echo alone cannot
  transfer the wake to the straddling round (which began before the trigger
  and belongs to the earlier wake). Only the round that began after the
  trigger provably consumed it.
  """
  home = fx.build_scheduled_straddle_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0 and manifest.unresolved == []
  sid = fx.S_STRADDLE
  st = f"{sid[:8]}-0000-0000-0000-00000000000a"
  summary = manager_mapping(manifest, sid).detail["input_disposition"]
  assert summary["confirmed_bound"] == 1
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  tree = tree_of(home)
  turns = run_records(tree, sid, "manager_turn")
  assert len(turns) == 2
  wake_runs = [r for r in turns if st in input_ids_of(tree, sid, r.id)]
  assert len(wake_runs) == 1
  assert wake_runs[0].raw_log_ref.endswith(f"{fx.iso(10)}/agent.raw.ndjson")


# ---------------------------------------------------------------------------
# Improve loop association
# ---------------------------------------------------------------------------


def test_loop_association_requires_controller_identity(tmp_path, monkeypatch):
  """A matching report filename + description without causal identity is never
  associated: the iteration imports as a standalone worker and is reported."""
  home = fx.build_full_home(tmp_path / "home")
  # Strip the controller identity the full home's iterations carry: the
  # thread keeps the description and the loop keeps its report, but the
  # recorded repo/work-branch/worktree identity is gone.
  thread_dir = home / "sessions" / fx.S_IMPROVE / "threads" / fx.T_ITER1
  meta = json.loads((thread_dir / "metadata.json").read_text())
  meta["repo_path"] = ""
  meta["branch_name"] = ""
  meta.pop("worktree_path", None)
  from src.core.json_utils import atomic_write_text
  atomic_write_text(thread_dir / "metadata.json",
                    json.dumps(meta, indent=2) + "\n")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0, manifest.unresolved
  report = [u for u in manifest.import_report if u.source_kind == "improve_association"]
  assert len(report) == 1
  assert "identity" in report[0].reason
  # The positively proven iteration keeps its mapping; the stripped one is a
  # standalone worker, not guessed into the loop.
  mappings = [m for m in manifest.mappings if m.source_kind == "improve_iteration"]
  assert len(mappings) == 1
  assert mappings[0].detail["iteration"] == 2
  assert any(m.source_kind == "worker_thread" and m.source_id == report[0].source_id
             for m in manifest.mappings)


def test_loop_association_rejects_foreign_launch_echo(tmp_path, monkeypatch):
  """Two loops with the same goal and repo/branch: ambiguity, never a guess."""
  home = fx.build_ambiguous_loop_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0, manifest.unresolved
  report = [u for u in manifest.import_report if u.source_kind == "improve_association"]
  assert len(report) == 1 and "ambiguous" in report[0].reason


# ---------------------------------------------------------------------------
# Completed import: review, landing, delivery provenance
# ---------------------------------------------------------------------------


def test_failed_review_attempt_with_accepted_retry_imports_completed(tmp_path, monkeypatch):
  """The reviewer-retry policy: the chain's latest accepted attempt closes the task."""
  home = fx.build_review_retry_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0 and manifest.unresolved == []
  mapping = worker_mapping(manifest, fx.T_WORK)
  assert mapping.disposition == "worker_work_completed"
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  tree = tree_of(home)
  node = mapping.target_session_id
  reviews = run_records(tree, node, "review")
  assert len(reviews) == 2  # both attempts are retained Runs
  outcomes = sorted(terminal(tree, node, r.id) for r in reviews)
  assert outcomes == ["failed", "success"]
  # The retry chain is preserved in the Runs' records.
  retries = [r for r in reviews if r.retry_of_run_id]
  assert len(retries) == 1
  facts = tree.facts_of(node)
  close = [e for e in facts.events_by_id.values() if e.get("type") == ET.TASK_CLOSED]
  assert len(close) == 1 and close[0]["outcome"] == "completed"


def test_completed_review_metadata_with_failed_result_cannot_close(tmp_path, monkeypatch):
  """Completed review metadata versus a retained failed review result: stays open."""
  home = fx.build_review_metadata_conflict_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0
  mapping = worker_mapping(manifest, fx.T_WORK)
  assert mapping.disposition == "worker_work_unproven"
  assert "latest retained review attempt" in (mapping.reason or "")
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  tree = tree_of(home)
  node = mapping.target_session_id
  facts = tree.facts_of(node)
  assert facts.task_state != "completed"
  assert not any(e.get("type") == ET.TASK_CLOSED
                 for e in facts.events_by_id.values())


def test_reused_branch_tip_cannot_prove_landing(tmp_path, monkeypatch):
  """A branch moved after the run: reachability of the current tip proves nothing."""
  home = fx.build_moved_branch_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0
  mapping = worker_mapping(manifest, fx.T_WORK)
  assert mapping.disposition == "worker_work_unproven"
  assert "moved or was reused" in (mapping.reason or "")


def test_retained_worktree_pins_the_result_commit_past_a_moved_branch(tmp_path, monkeypatch):
  """The run's own worktree HEAD pins the result commit and proves landing."""
  home = fx.build_pinned_worktree_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  code, manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert code == 0
  mapping = worker_mapping(manifest, fx.T_WORK)
  assert mapping.disposition == "worker_work_completed"
  commit = mapping.detail["landing_commit"]
  assert len(commit) == 40
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  tree = tree_of(home)
  node = mapping.target_session_id
  facts = tree.facts_of(node)
  close = [e for e in facts.events_by_id.values() if e.get("type") == ET.TASK_CLOSED]
  assert len(close) == 1
  # The imported close binds delivery to the immutable result commit and the
  # actual target lineage.
  assert f"landed:main@{commit}" in close[0]["result_refs"]
  # The converter-written close/report are import-time records: their
  # timestamps are the converter's write time, not a manufactured historical
  # success or receipt time.
  now = datetime.now(UTC).replace(tzinfo=None)
  written = datetime.fromisoformat(close[0]["timestamp"]).replace(tzinfo=None)
  assert abs((now - written).total_seconds()) < 120
  # The historical completion time stays on the Run.
  runs = run_records(tree, node, "work")
  assert runs[0].ended_at is not None
  assert runs[0].ended_at.date() == fx.BASE.date()


def test_converter_written_report_stays_outside_input_replay(tmp_path, monkeypatch):
  """The imported parent report is pre-boundary history, not a replayable input."""
  home = fx.build_pinned_worktree_home(tmp_path / "home")
  point_home(monkeypatch, home)
  manifest_path = tmp_path / "m.json"
  _code, _manifest, _ = dry_run(monkeypatch, home, manifest_path)
  assert run_cli(monkeypatch, home, "--apply", "--manifest", str(manifest_path))[0] == 0
  tree = tree_of(home)
  owner = fx.S_WORKERS
  events = tree.fact_history(owner)
  imported_at = next(i for i, e in enumerate(events) if e.get("type") == ET.TASK_IMPORTED)
  reports = [i for i, e in enumerate(events) if e.get("type") == ET.CHILD_REPORT]
  assert len(reports) == 1 and reports[0] < imported_at
  # The owner's pending set never grows from the imported delivery.
  assert tree.facts_of(owner).imported_pending_ids == frozenset()


# ---------------------------------------------------------------------------
# Interrupted-append proof: complete expected fact content
# ---------------------------------------------------------------------------


def _fresh_apply_context(monkeypatch, home: Path):
  point_home(monkeypatch, home)
  import src.core.config as core_config
  cfg = core_config.get_config()
  snap = migration.scan_source(cfg)
  plan = migration.build_conversion_plan(cfg, snap)
  return cfg, snap, plan


def _forged_from_planned(expected: migration.PlannedFact, field: str, value) -> dict:
  """A planned fact's own identity with one payload field altered."""
  forged = dict(expected.event)
  for name in expected.generated:
    forged.setdefault(name, fx.iso(1) if name == "timestamp" else "minted-at-write")
  forged[field] = value
  return forged


_ALTERED_CASES = [
    # (home builder, family, altered field, foreign value)
    (fx.build_failed_rounds_home, "run_finished", "outcome", "success"),
    (fx.build_failed_rounds_home, "task_imported", "pending_inputs",
     [{"source_ref": "forged", "input_id": "u-else", "event_type": ET.USER}]),
    (fx.build_pinned_worktree_home, "task_closed", "summary", "forged summary"),
    (fx.build_pinned_worktree_home, "child_report", "outcome", "failed"),
]


@pytest.mark.parametrize("case_index", range(len(_ALTERED_CASES)))
def test_altered_payload_with_planned_identity_refuses(tmp_path, monkeypatch, case_index):
  """A suffix event keeping its id/run_id but altering content is foreign content.

  Every fact family (run_finished, task_imported, task_closed, child_report)
  refuses on complete-content comparison, not id presence.
  """
  builder, family, field, value = _ALTERED_CASES[case_index]
  home = builder(tmp_path / "home")
  cfg, snap, plan = _fresh_apply_context(monkeypatch, home)
  facts = migration._planned_facts(plan)
  planned = [(node, key, expected) for node, node_facts in facts.items()
             for key, expected in node_facts.items()
             if key[0] == family]
  assert planned, f"the fixture plans no {family} fact"
  node, _key, expected = planned[0]
  log_rel = f"sessions/{node}/data/chat_events.jsonl"
  # A migration-created node's log is not a source file: the whole file is
  # the apply's append region (record None).
  record = (None if log_rel not in snap.hashes else migration.SourceFileRecord(
      path=log_rel, sha256=snap.hashes[log_rel], size=snap.files[log_rel]))
  forged = _forged_from_planned(expected, field, value)
  log_path = home / log_rel
  log_path.parent.mkdir(parents=True, exist_ok=True)
  with open(log_path, "ab") as f:
    f.write(json.dumps(forged).encode() + b"\n")
  problems = migration._log_append_problems(
      cfg, log_rel, record, facts.get(node, {}), datetime.now(UTC) - timedelta(hours=1))
  assert problems, f"{family} must refuse on altered {field}"
  assert "does not match the planned content" in problems[0]


def test_suffix_event_with_unplanned_identity_refuses(tmp_path, monkeypatch):
  """A suffix event that is not one of this plan's facts refuses outright."""
  home = fx.build_failed_rounds_home(tmp_path / "home")
  cfg, snap, plan = _fresh_apply_context(monkeypatch, home)
  facts = migration._planned_facts(plan)
  node = fx.S_FAILED_ROUNDS
  log_rel = f"sessions/{node}/data/chat_events.jsonl"
  record = migration.SourceFileRecord(
      path=log_rel, sha256=snap.hashes[log_rel], size=snap.files[log_rel])
  forged = {"id": "u-not-planned", "type": ET.RUN_FINISHED,
            "run_id": "00000000-0000-4000-8000-0000000000f1", "outcome": "success",
            "input_event_ids": ["u-someone-else"], "actor": "system",
            "source_session_id": node, "timestamp": fx.iso(1)}
  with open(home / log_rel, "ab") as f:
    f.write(json.dumps(forged).encode() + b"\n")
  problems = migration._log_append_problems(
      cfg, log_rel, record, facts.get(node, {}), datetime.now(UTC) - timedelta(hours=1))
  assert problems and "not one of this plan's facts" in problems[0]


def test_append_fact_if_absent_verifies_existing_content(tmp_path, monkeypatch):
  """An id already in the log with different content refuses; identical content skips."""
  home = fx.build_unbound_success_home(tmp_path / "home")
  cfg, _snap, plan = _fresh_apply_context(monkeypatch, home)
  manager = plan.managers[0]
  facts = migration._planned_facts(plan)
  key = (str(manager.task_imported["type"]), str(manager.task_imported["id"]))
  expected = facts[manager.session_id][key]
  log = home / "sessions" / manager.session_id / "data" / "chat_events.jsonl"
  before = log.read_bytes()
  foreign = dict(manager.task_imported)
  foreign["pending_inputs"] = [{"source_ref": "forged", "input_id": "x",
                                "event_type": ET.USER}]
  with open(log, "ab") as f:
    f.write(json.dumps(foreign).encode() + b"\n")
  with pytest.raises(migration.MigrationRefusedError, match="different content"):
    migration._append_fact_if_absent(cfg, manager.session_id, dict(manager.task_imported),
                                     expected, datetime.now(UTC) - timedelta(hours=1))
  assert log.read_bytes() != before  # the forged line stays (refusal preserves bytes)


def test_run_finished_existing_fact_content_is_verified(tmp_path, monkeypatch):
  """A landed run_finished with a matching run_id but altered outcome refuses."""
  home = fx.build_failed_rounds_home(tmp_path / "home")
  _cfg, _snap, plan = _fresh_apply_context(monkeypatch, home)
  manager = plan.managers[0]
  run = manager.manager_turn_runs[0]
  run_store = tree_of(home).runs
  asyncio.run(run_store.register_run(run.record))
  # Land a foreign terminal fact under the planned run id, as a foreign
  # writer would (the RunStore's own payload guard is not the attacker here).
  foreign = {
      "id": "minted-at-write", "type": ET.RUN_FINISHED, "timestamp": fx.iso(1),
      "actor": "system", "source_session_id": manager.session_id,
      "run_id": run.record.id, "outcome": "failed",
      "input_event_ids": ["u-invented"],
  }
  asyncio.run(run_store._events.append(manager.session_id, foreign))
  events = run_store.load_events_sync(manager.session_id)
  landed = [e for e in events if e.get("type") == ET.RUN_FINISHED
            and e.get("run_id") == run.record.id]
  assert landed and landed[0]["outcome"] == "failed"
  expected = migration._expected_run_finished(run)
  mismatch = migration._fact_mismatch(expected, landed[0], datetime.now(UTC) - timedelta(hours=1))
  assert mismatch and ("outcome" in mismatch or "input_event_ids" in mismatch)
