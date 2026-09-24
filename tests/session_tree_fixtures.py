"""Fully synthetic, portable homes exercising the eleven migration categories.

Everything under the fixture home is generated at test time (no host
identifiers, no credentials, no private logs): UUID-shaped session ids,
UUID-shaped event/thread ids, one synthetic backend id, and a throwaway git
repo created inside the fixture home for landing checks and cron prompt
resolution. The builder is the single fixture source shared by the
session-tree migration tests.
"""

from __future__ import annotations

import json
import os
import subprocess
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

from src.core.json_utils import atomic_write_text
from src.core.models import (
    MasterRunRecord,
    PendingTrigger,
    RunRecord,
    SessionMetadata,
    SlackOrigin,
    TaskSpec,
    TaskType,
    ThreadMetadata,
)

BASE = datetime(2026, 1, 5, 12, 0, 0, tzinfo=UTC)

_TRANSIENT = {
    "has_running_tasks", "has_pending_trigger", "pending_trigger_count",
    "next_trigger_at", "has_pending_plan_approval", "schedule_cron",
    "schedule_enabled", "schedule_next_run", "schedule_timezone",
    "schedule_project", "schedule_allow_failure", "thinking_since",
}

# Well-known synthetic session ids (UUID-shaped, nothing host-derived).
S_ORDINARY = "1a1a1111-0000-4000-8000-000000000001"
S_PENDING = "2a2a2222-0000-4000-8000-000000000002"
S_PM = "3a3a3333-0000-4000-8000-000000000003"
S_ARCHIVED = "4a4a4444-0000-4000-8000-000000000004"
S_FORK = "5a5a5555-0000-4000-8000-000000000005"
S_PREDECESSOR = "6a6a6666-0000-4000-8000-000000000006"
S_TAIL = "7a7a7777-0000-4000-8000-000000000007"
S_SCHEDULED = "8a8a8888-0000-4000-8000-000000000008"
S_STEPS = "9a9a9999-0000-4000-8000-000000000009"
S_IMPROVE = "aaaa0000-0000-4000-8000-00000000000a"
S_WORKERS = "bbbb2222-0000-4000-8000-00000000000b"
S_V2 = "cccc3333-0000-4000-8000-00000000000c"
S_FAILED_ROUNDS = "d1d1d111-0000-4000-8000-000000000001"
S_UNBOUND = "d2d2d222-0000-4000-8000-000000000002"
S_TIMES = "d3d3d333-0000-4000-8000-000000000003"
S_SCHED_UNPROVEN = "d4d4d444-0000-4000-8000-000000000004"
S_IDENTICAL = "d5d5d555-0000-4000-8000-000000000005"
S_REVIEW_RETRY = "d6d6d666-0000-4000-8000-000000000006"
S_IDENT_BASE = "d7d7d777-0000-4000-8000-000000000007"
S_RESUME = "d8d8d888-0000-4000-8000-000000000008"
S_STRADDLE = "d9d9d999-0000-4000-8000-000000000009"
T_RETRY_FAILED = "f0f0f000-0000-4000-8000-000000000001"
T_RETRY_OK = "f0f0f000-0000-4000-8000-000000000002"
T_REVIEW_CONFLICT = "f1f1f111-0000-4000-8000-000000000001"

T_WORK = "eeee1111-0000-4000-8000-000000000001"
T_REVIEW = "eeee1111-0000-4000-8000-000000000003"
T_COMPLETED = "eeee1111-0000-4000-8000-000000000002"
T_STEP0 = "cccccccc-0000-4000-8000-000000000001"
T_STEP1 = "cccccccc-0000-4000-8000-000000000002"
T_ITER1 = "bbbb1111-0000-4000-8000-000000000001"
T_ITER2 = "bbbb1111-0000-4000-8000-000000000002"
GROUP = "ops-alpha"


def iso(offset_minutes: float) -> str:
  return (BASE + timedelta(minutes=offset_minutes)).isoformat()


def _meta_json(meta: SessionMetadata) -> str:
  return meta.model_dump_json(indent=2, exclude=_TRANSIENT)


def ev(event_type: str, offset: float, event_id: str, *, actor: str = "user",
       source_session_id: str | None = None, **extra: Any) -> dict:
  event = {"id": event_id, "type": event_type, "timestamp": iso(offset),
           "actor": actor, "source_session_id": source_session_id}
  event.update(extra)
  return event


class FixtureBuilder:
  """Writes one synthetic home; call helpers then build()."""

  def __init__(self, home: Path) -> None:
    self.home = home
    self.written_metadata: dict[str, SessionMetadata] = {}

  def session(self, meta: SessionMetadata) -> SessionMetadata:
    directory = self.home / "sessions" / meta.id
    (directory / "data").mkdir(parents=True, exist_ok=True)
    (directory / "threads").mkdir(exist_ok=True)
    atomic_write_text(directory / "metadata.json", _meta_json(meta))
    self.written_metadata[meta.id] = meta
    return meta

  def chat_log(self, session_id: str, events: list[dict], *,
               archives: dict[str, list[dict]] | None = None) -> None:
    log = self.home / "sessions" / session_id / "data" / "chat_events.jsonl"
    with open(log, "a", encoding="utf-8") as f:
      for event in events:
        f.write(json.dumps(event) + "\n")
    for name, archive_events in (archives or {}).items():
      archive_dir = self.home / "sessions" / session_id / "data" / "archives"
      archive_dir.mkdir(parents=True, exist_ok=True)
      with open(archive_dir / name, "a", encoding="utf-8") as f:
        for event in archive_events:
          f.write(json.dumps(event) + "\n")

  def thread(self, session_id: str, meta: ThreadMetadata, *,
             events: list[dict] | None = None, raw: str | None = None) -> None:
    directory = self.home / "sessions" / session_id / "threads" / meta.id
    (directory / "data").mkdir(parents=True, exist_ok=True)
    atomic_write_text(directory / "metadata.json", meta.model_dump_json(indent=2))
    if events:
      with open(directory / "data" / "events.jsonl", "a", encoding="utf-8") as f:
        for event in events:
          f.write(json.dumps(event) + "\n")
    if raw is not None:
      (directory / "data" / "agent.raw.ndjson").write_text(raw, encoding="utf-8")

  def master_turn_dir(self, session_id: str, started_at: str, raw: str, *,
                      mtime: str | None = None) -> Path:
    directory = self.home / "sessions" / session_id / "data" / "master_runs" / started_at
    directory.mkdir(parents=True, exist_ok=True)
    raw_path = directory / "agent.raw.ndjson"
    raw_path.write_text(raw, encoding="utf-8")
    if mtime is not None:
      # An offline copy preserves mtimes; the runtime's own completion contract
      # for raw logs reads the last write.
      stamp = datetime.fromisoformat(mtime).timestamp()
      os.utime(raw_path, (stamp, stamp))
    return directory

  def trigger(self, session_id: str, trigger: PendingTrigger) -> None:
    directory = self.home / "sessions" / session_id / "triggers"
    directory.mkdir(parents=True, exist_ok=True)
    atomic_write_text(directory / f"{trigger.id}.json", trigger.model_dump_json(indent=2))

  def loop(self, session_id: str, loop_id: str, files: dict[str, str]) -> Path:
    directory = self.home / "sessions" / session_id / "loops" / loop_id
    directory.mkdir(parents=True, exist_ok=True)
    for name, text in files.items():
      atomic_write_text(directory / name, text)
    return directory

  def cron_config(self, name: str, body_text: str) -> None:
    directory = self.home / "config.d" / "cron.d"
    directory.mkdir(parents=True, exist_ok=True)
    atomic_write_text(directory / f"{name}.yaml", body_text)

  def project(self, group: str, *, common: str, supplement: str | None) -> Path:
    directory = self.home / "projects" / group
    directory.mkdir(parents=True, exist_ok=True)
    body: dict[str, str] = {"prompt_file": "project.md"}
    if supplement is not None:
      body["manager_prompt_file"] = "manager.md"
      atomic_write_text(directory / "manager.md", supplement)
    atomic_write_text(directory / "project.md", common)
    atomic_write_text(
        directory / "project.yaml",
        json.dumps(body, indent=2) + "\n")
    return directory

  def build(self) -> Path:
    (self.home / "sessions").mkdir(parents=True, exist_ok=True)
    (self.home / "config.yaml").write_text(
        "server:\n"
        "  port: 8765\n"
        "backends:\n"
        "  options:\n"
        "    - id: synth\n"
        "      label: Synth\n"
        "      type: cc-claude\n"
        "      model: synth-model\n",
        encoding="utf-8")
    self._build_repo()
    return self.home

  def _build_repo(self) -> None:
    repo = self.home / "repo"
    (repo / "prompts").mkdir(parents=True, exist_ok=True)
    (repo / "prompts" / "nightly.md").write_text("Sweep the nightly checklist.\n", encoding="utf-8")
    env = dict(**__import__("os").environ)
    subprocess.run(
        ["git", "init", "-q", "-b", "main", str(repo)], check=True, env=env)
    git = ["git", "-C", str(repo)]
    subprocess.run([*git, "config", "user.email", "synth@example.invalid"], check=True)
    subprocess.run([*git, "config", "user.name", "Synth Tester"], check=True)
    subprocess.run([*git, "add", "-A"], check=True)
    subprocess.run([*git, "commit", "-qm", "synthetic base"], check=True)


def build_full_home(home: Path) -> Path:
  """The eleven-category home: zero unresolved, one proven-pending input."""
  builder = FixtureBuilder(home)
  builder.build()

  handled_text = "Please check the deploy pipeline"
  rotated_text = "Older rotated request"
  u_handled = ev("user", 0, f"{S_ORDINARY[:8]}-0000-0000-0000-00000000000a",
                 content=handled_text, source_session_id=S_ORDINARY)
  md_handled = ev("master_done", 20, f"{S_ORDINARY[:8]}-0000-0000-0000-00000000000c",
                  actor="agent", exit_code=0, input_event_id=u_handled["id"],
                  source_session_id=S_ORDINARY)
  st_fired = ev("scheduled_trigger", 30, f"{S_ORDINARY[:8]}-0000-0000-0000-00000000000b",
                actor="system", content="cron wake", source_session_id=S_ORDINARY)
  ws_old = ev("worker_summary", 25, f"{S_ORDINARY[:8]}-0000-0000-0000-00000000000d",
              actor="system", content="worker finished the job", thread_id="thread-legacy-1",
              status="completed", source_session_id=S_ORDINARY)
  u_rotated = ev("user", -1440, f"{S_ORDINARY[:8]}-0000-0000-0000-00000000000e",
                 content=rotated_text, source_session_id=S_ORDINARY)
  md_rotated = ev("master_done", -1435, f"{S_ORDINARY[:8]}-0000-0000-0000-00000000000f",
                  actor="agent", exit_code=0, input_event_id=u_rotated["id"],
                  source_session_id=S_ORDINARY)
  assistant_rotated = ev("assistant", -1434, f"{S_ORDINARY[:8]}-0000-0000-0000-000000000010",
                         actor="agent", content="done", source_session_id=S_ORDINARY)
  # The handled round's transport log adopts the same backend session id the
  # chat log's run-start marker carries; the scheduled wake's round does too,
  # and its stream holds the wake text as the launch's own prompt echo.
  raw_turn = (
      json.dumps({"type": "system", "subtype": "init",
                  "session_id": "native-cc-ordinary-turn-1"}) + "\n"
      + json.dumps({"type": "assistant", "message": {"content": [
          {"type": "text", "text": handled_text}]}}) + "\n"
      + json.dumps({"type": "result", "subtype": "success", "is_error": False,
                    "usage": {"input_tokens": 120, "output_tokens": 40,
                              "cache_read_input_tokens": 0,
                              "cache_creation_input_tokens": 0}}) + "\n")
  raw_scheduled = (
      json.dumps({"type": "system", "subtype": "init",
                  "session_id": "native-sched-1"}) + "\n"
      + json.dumps({"type": "user", "message": {"role": "user", "content": [
          {"type": "text", "text": "cron wake"}]}}) + "\n"
      + json.dumps({"type": "assistant", "message": {"content": [
          {"type": "text", "text": "cron wake acknowledged"}]}}) + "\n"
      + json.dumps({"type": "result", "subtype": "success", "is_error": False,
                    "usage": {"input_tokens": 90, "output_tokens": 25,
                              "cache_read_input_tokens": 0,
                              "cache_creation_input_tokens": 0}}) + "\n")

  meta1 = SessionMetadata(id=S_ORDINARY, name="Deploy pipeline ops", backend="synth",
                          created_at=BASE, updated_at=BASE)
  meta1.cc_session_id = "native-cc-ordinary"
  # The rotation that produced chat_events.2026-W01.jsonl recorded its
  # write-time offset (three archived lines) in the metadata.
  meta1.archive_offset = 3
  meta1.master_run = MasterRunRecord(
      raw_log=str(home / "sessions" / S_ORDINARY / "data" / "master_runs" / iso(35) / "agent.raw.ndjson"),
      started_at=BASE + timedelta(minutes=35), pid=0, pid_start="0")
  # The producers' round structure: a run-start adoption marker opens each
  # turn's interval and the round's MASTER_DONE closes it (the bare
  # session_id-only spelling is the pre-typed corpus's marker). Each round's
  # transport log was last written before its MASTER_DONE landed.
  marker_handled = {
      "id": f"{S_ORDINARY[:8]}-0000-0000-0000-0000000000b1",
      "session_id": "native-cc-ordinary-turn-1",
      "timestamp": iso(12),
  }
  marker_sched = {
      "id": f"{S_ORDINARY[:8]}-0000-0000-0000-0000000000b3",
      "session_id": "native-sched-1",
      "timestamp": iso(36),
  }
  md_sched = {
      "id": f"{S_ORDINARY[:8]}-0000-0000-0000-0000000000b2",
      "type": "master_done",
      "timestamp": iso(40),
      "actor": "agent",
      "source_session_id": S_ORDINARY,
      "exit_code": 0,
  }
  builder.session(meta1)
  builder.chat_log(S_ORDINARY,
                   [u_handled, marker_handled, md_handled, ws_old, st_fired,
                    marker_sched, md_sched],
                   archives={"chat_events.2026-W01.jsonl": [u_rotated, md_rotated, assistant_rotated]})
  builder.master_turn_dir(S_ORDINARY, iso(10), raw_turn, mtime=iso(18))
  builder.master_turn_dir(S_ORDINARY, iso(35), raw_scheduled, mtime=iso(38))
  builder.thread(S_ORDINARY, ThreadMetadata(
      id=T_WORK, session_id=S_ORDINARY, description="Do the deploy work",
      status="completed", created_at=BASE - timedelta(minutes=5), started_at=BASE - timedelta(minutes=4),
      completed_at=BASE + timedelta(minutes=24), pid=424242, pid_start="1000", exit_code=0,
      backend="synth", model="synth-model", repo_path=str(home / "repo"),
      branch_name="work/deploy", base_branch="main", require_review=False,
      claude_session_id="native-worker-deploy"))

  u_pending = ev("user", 60, f"{S_PENDING[:8]}-0000-0000-0000-00000000000a",
                 content="Follow up on the invoice", source_session_id=S_PENDING)
  meta2 = SessionMetadata(id=S_PENDING, name="Billing followups", backend="synth",
                          created_at=BASE + timedelta(minutes=50),
                          updated_at=BASE + timedelta(minutes=60))
  meta2.slack_origin = SlackOrigin(team_id="T-SYNTH", channel_id="C-SYNTH",
                                   thread_ts="1700000000.000100")
  meta2.group = GROUP
  builder.session(meta2)
  builder.chat_log(S_PENDING, [u_pending])

  builder.session(SessionMetadata(id=S_PM, name="Ops alpha PM", backend="synth",
                                  role="project", group=GROUP, created_at=BASE, updated_at=BASE))
  builder.project(
      GROUP,
      common="Use the shared ops workflow and keep deliverables in English.\n",
      supplement=("task-goal: Deliver the ops-alpha refresh with review evidence.\n"
                  "node-rules: Always update the ledger before closing a task.\n"))

  builder.session(SessionMetadata(id=S_ARCHIVED, name="Old exploration",
                                  status="archived", backend="synth",
                                  created_at=BASE, updated_at=BASE))

  builder.session(SessionMetadata(id=S_FORK, name="Forked copy", parent_session_id=S_ORDINARY,
                                  backend="synth", created_at=BASE, updated_at=BASE))
  builder.session(SessionMetadata(id=S_PREDECESSOR, name="Taken over v1",
                                  successor_session_id=S_TAIL, backend="synth",
                                  created_at=BASE, updated_at=BASE))
  builder.session(SessionMetadata(id=S_TAIL, name="Taken over v2", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  # The predecessor's own proven manager turn: same logical manager as the tail.
  builder.master_turn_dir(S_PREDECESSOR, iso(-30), raw_turn)

  builder.session(SessionMetadata(id=S_SCHEDULED, name="Nightly sweep", backend="synth",
                                  scheduled_task="nightly-sweep", created_at=BASE, updated_at=BASE))
  builder.trigger(S_SCHEDULED, PendingTrigger(
      id="22222222-0000-4000-8000-000000000009", session_id=S_SCHEDULED,
      fire_at=BASE + timedelta(hours=4), message="check the sweep"))
  builder.cron_config(
      "nightly-sweep",
      f"cron: '30 2 * * *'\ntype: normal\nrepo: {home / 'repo'}\n"
      f"prompt_file: {home / 'repo' / 'prompts/nightly.md'}\nbackend: synth\n")

  builder.session(SessionMetadata(id=S_STEPS, name="Nightly sweep steps", backend="synth",
                                  scheduled_task="nightly-sweep-steps", created_at=BASE,
                                  updated_at=BASE))
  builder.thread(S_STEPS, ThreadMetadata(
      id=T_STEP0, session_id=S_STEPS, description="Sweep · collect", status="completed",
      created_at=BASE, started_at=BASE, completed_at=BASE + timedelta(minutes=10), exit_code=0,
      backend="synth", chain_root=T_STEP0, step_index=0, require_review=False))
  builder.thread(S_STEPS, ThreadMetadata(
      id=T_STEP1, session_id=S_STEPS, description="Sweep · report", status="completed",
      created_at=BASE + timedelta(minutes=10), started_at=BASE + timedelta(minutes=10),
      completed_at=BASE + timedelta(minutes=20), exit_code=0, backend="synth",
      chain_root=T_STEP0, step_index=1, require_review=False),
      events=[ev("result", 15, f"{S_STEPS[:8]}-0000-0000-0000-00000000000a", actor="agent",
                 subtype="success", source_session_id=S_STEPS)])

  loop_state = {
      "loop_id": 3,
      "goal": "Reduce the import time of the metrics module",
      "status": "completed",
      "work_branch": "improve/metrics",
      "base_branch": "main",
      "repo_path": str(home / "repo"),
      "merge_back": False,
      "backend": "synth",
      "model": "synth-model",
      "created_at": iso(100),
      "server_pid": 424243,
  }
  builder.session(SessionMetadata(id=S_IMPROVE, name="Improve host", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  builder.loop(S_IMPROVE, "3", {
      "state.json": json.dumps(loop_state, indent=2),
      "goal.md": "Reduce the import time of the metrics module\n",
      "iter_0001.md": "Iteration 1 report: shaved 200ms.\n",
      "iter_0002.md": "Iteration 2 report: still failing.\n",
  })
  # The controller's own launch identity: both iterations ran in the loop's
  # shared worktree against the loop's recorded repo and work branch, and each
  # launch's prompt echo references the iteration report it was told to write.
  loop_worktree = str(home / "worktrees" / "improve-metrics")
  builder.thread(S_IMPROVE, ThreadMetadata(
      id=T_ITER1, session_id=S_IMPROVE, status="completed",
      description=("Iterative improvement — iteration 1/2\n"
                   "Goal: Reduce the import time of the metrics module"),
      created_at=BASE + timedelta(minutes=101), started_at=BASE + timedelta(minutes=101),
      completed_at=BASE + timedelta(minutes=111), exit_code=0, backend="synth",
      repo_path=str(home / "repo"), branch_name="improve/metrics", base_branch="main",
      worktree_path=loop_worktree, require_review=False),
      raw="Write your report to: " + str(home / "sessions" / S_IMPROVE / "loops" / "3" /
                                          "iter_0001.md") + "\n")
  builder.thread(S_IMPROVE, ThreadMetadata(
      id=T_ITER2, session_id=S_IMPROVE, status="failed",
      description=("Iterative improvement — iteration 2/2\n"
                   "Goal: Reduce the import time of the metrics module"),
      created_at=BASE + timedelta(minutes=112), started_at=BASE + timedelta(minutes=112),
      completed_at=BASE + timedelta(minutes=120), exit_code=1, backend="synth",
      repo_path=str(home / "repo"), branch_name="improve/metrics", base_branch="main",
      worktree_path=loop_worktree, require_review=False),
      raw="Write your report to: " + str(home / "sessions" / S_IMPROVE / "loops" / "3" /
                                          "iter_0002.md") + "\n")

  builder.session(SessionMetadata(id=S_WORKERS, name="Implementation hub", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  builder.thread(S_WORKERS, ThreadMetadata(
      id=T_WORK, session_id=S_WORKERS, description="Implement the parser fix",
      status="completed", created_at=BASE, started_at=BASE,
      completed_at=BASE + timedelta(minutes=30), exit_code=0, backend="synth",
      repo_path=str(home / "repo"), branch_name="work/parser", base_branch="main",
      require_review=True, claude_session_id="native-worker-parser"),
      events=[ev("result", 29, f"{S_WORKERS[:8]}-0000-0000-0000-00000000000b", actor="agent",
                 subtype="success", is_error=False, source_session_id=S_WORKERS)])
  builder.thread(S_WORKERS, ThreadMetadata(
      id=T_REVIEW, session_id=S_WORKERS, description="Review of: Implement the parser fix",
      status="failed", created_at=BASE + timedelta(minutes=31),
      started_at=BASE + timedelta(minutes=31), completed_at=BASE + timedelta(minutes=40),
      exit_code=1, backend="synth", review_of=T_WORK, require_review=False))
  builder.thread(S_WORKERS, ThreadMetadata(
      id=T_COMPLETED, session_id=S_WORKERS, description="Update the changelog entry",
      status="completed", created_at=BASE + timedelta(minutes=41),
      started_at=BASE + timedelta(minutes=41), completed_at=BASE + timedelta(minutes=50),
      exit_code=0, backend="synth", require_review=False),
      events=[ev("result", 49, f"{S_WORKERS[:8]}-0000-0000-0000-00000000000a", actor="agent",
                 subtype="success", is_error=False, source_session_id=S_WORKERS)])

  # Mixed existing v2 data: untouched by conversion.
  meta_v2 = SessionMetadata(id=S_V2, name="Fresh v2 task", schema_version=2, profile="manager",
                            task_parent_id=None, task=TaskSpec(goal="Existing v2 work"),
                            backend="synth", created_at=BASE, updated_at=BASE)
  builder.session(meta_v2)
  run_dir = home / "sessions" / S_V2 / "data" / "runs" / "run-existing-1"
  run_dir.mkdir(parents=True)
  atomic_write_text(run_dir / "metadata.json",
                    RunRecord(id="run-existing-1", session_id=S_V2, kind="work",
                              backend="synth").model_dump_json(indent=2))
  with open(home / "sessions" / S_V2 / "data" / "chat_events.jsonl", "a", encoding="utf-8") as f:
    f.write(json.dumps(ev("task_created", 200, f"{S_V2[:8]}-0000-0000-0000-00000000000a",
                          actor="user", request_id="req-existing",
                          task_parent_id=None, source_session_id=S_V2)) + "\n")
  aliases = {
      "old_session_ids": {},
      "old_threads": {"11111111-0000-4000-8000-000000000001/run-existing-1": {
          "session_id": S_V2, "run_id": "run-existing-1"}},
  }
  atomic_write_text(home / "sessions" / "session_aliases.json", json.dumps(aliases, indent=2))
  return home


def build_unproven_implement_home(home: Path) -> Path:
  """A completed implement worker whose review never happened: stays unproven."""
  builder = FixtureBuilder(home)
  builder.build()
  builder.session(SessionMetadata(id=S_WORKERS, name="Landing hub", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  repo = home / "repo"
  work_branch = "work/unproven"
  subprocess.run(["git", "-C", str(repo), "checkout", "-qb", work_branch], check=True)
  (repo / "feature.txt").write_text("unreviewed feature\n", encoding="utf-8")
  subprocess.run(["git", "-C", str(repo), "add", "-A"], check=True)
  subprocess.run(["git", "-C", str(repo), "commit", "-qm", "unreviewed work"], check=True)
  subprocess.run(["git", "-C", str(repo), "checkout", "-q", "main"], check=True)
  builder.thread(S_WORKERS, ThreadMetadata(
      id=T_WORK, session_id=S_WORKERS, description="Implement the unreviewed fix",
      status="completed", created_at=BASE, started_at=BASE,
      completed_at=BASE + timedelta(minutes=30), exit_code=0, backend="synth",
      repo_path=str(repo), branch_name=work_branch, base_branch="main",
      task_type=TaskType.IMPLEMENT, require_review=False),
      events=[ev("result", 29, f"{S_WORKERS[:8]}-0000-0000-0000-00000000000a", actor="agent",
                 subtype="success", is_error=False, source_session_id=S_WORKERS)])
  return home


def build_completed_implement_home(home: Path) -> Path:
  """A completed implement worker with review and a landed branch: imports completed."""
  builder = FixtureBuilder(home)
  builder.build()
  builder.session(SessionMetadata(id=S_WORKERS, name="Landing hub", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  repo = home / "repo"
  work_branch = "work/landed"
  subprocess.run(["git", "-C", str(repo), "checkout", "-qb", work_branch], check=True)
  (repo / "landed.txt").write_text("reviewed and landed\n", encoding="utf-8")
  subprocess.run(["git", "-C", str(repo), "add", "-A"], check=True)
  # The worker committed inside the run's own execution window: the result
  # commit's committer date pins it to this run, so a branch that moved later
  # can never pass as this run's output.
  env = dict(**os.environ)
  stamp = (BASE + timedelta(minutes=15)).strftime("%Y-%m-%dT%H:%M:%S %z")
  env["GIT_AUTHOR_DATE"] = stamp
  env["GIT_COMMITTER_DATE"] = stamp
  subprocess.run(["git", "-C", str(repo), "commit", "-qm", "landed work"], check=True, env=env)
  subprocess.run(["git", "-C", str(repo), "checkout", "-q", "main"], check=True)
  subprocess.run(["git", "-C", str(repo), "merge", "-q", "--ff-only", work_branch], check=True)
  builder.thread(S_WORKERS, ThreadMetadata(
      id=T_WORK, session_id=S_WORKERS, description="Implement the landed fix",
      status="completed", created_at=BASE, started_at=BASE,
      completed_at=BASE + timedelta(minutes=30), exit_code=0, backend="synth",
      repo_path=str(repo), branch_name=work_branch, base_branch="main",
      task_type=TaskType.IMPLEMENT, require_review=False),
      events=[ev("result", 29, f"{S_WORKERS[:8]}-0000-0000-0000-00000000000a", actor="agent",
                 subtype="success", is_error=False, source_session_id=S_WORKERS)])
  builder.thread(S_WORKERS, ThreadMetadata(
      id=T_REVIEW, session_id=S_WORKERS, description="Review of: Implement the landed fix",
      status="completed", created_at=BASE + timedelta(minutes=31),
      started_at=BASE + timedelta(minutes=31), completed_at=BASE + timedelta(minutes=40),
      exit_code=0, backend="synth", review_of=T_WORK, require_review=False),
      events=[ev("result", 39, f"{S_WORKERS[:8]}-0000-0000-0000-00000000000c", actor="agent",
                 subtype="success", is_error=False, source_session_id=S_WORKERS)])
  return home


def build_live_worker_home(home: Path) -> Path:
  """A running worker with a live process bound to the home: apply must refuse."""
  return build_full_home(home)


def build_ambiguous_loop_home(home: Path) -> Path:
  """Two identical improve loops: the iteration association is ambiguous."""
  builder = FixtureBuilder(home)
  builder.build()
  goal = "Ambiguity probe goal"
  loop_state = {
      "loop_id": 7, "goal": goal, "status": "running", "work_branch": "improve/probe",
      "base_branch": "main", "repo_path": str(home / "repo"), "merge_back": False,
      "backend": "synth", "model": "synth-model", "created_at": iso(0), "server_pid": None,
  }
  builder.session(SessionMetadata(id=S_IMPROVE, name="Ambiguous host", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  builder.loop(S_IMPROVE, "a", {"state.json": json.dumps(loop_state), "iter_0001.md": "a\n"})
  builder.loop(S_IMPROVE, "b", {"state.json": json.dumps(loop_state), "iter_0001.md": "b\n"})
  # Both loops carry the same controller identity (repo, work branch, shared
  # worktree, iteration report): the association is ambiguous, never guessed.
  builder.thread(S_IMPROVE, ThreadMetadata(
      id=T_ITER1, session_id=S_IMPROVE, status="completed",
      description=f"Iterative improvement — iteration 1/1\nGoal: {goal}",
      created_at=BASE + timedelta(minutes=1), started_at=BASE + timedelta(minutes=1),
      completed_at=BASE + timedelta(minutes=10), exit_code=0, backend="synth",
      repo_path=str(home / "repo"), branch_name="improve/probe", base_branch="main",
      worktree_path=str(home / "worktrees" / "improve-probe"), require_review=False),
      raw="Write your report to: iter_0001.md\n")
  return home


def build_uncertain_input_home(home: Path) -> Path:
  """A USER input a later unrelated scheduled round does not name: unresolved."""
  builder = FixtureBuilder(home)
  builder.build()
  u_uncertain = ev("user", 0, f"{S_PENDING[:8]}-0000-0000-0000-00000000000a",
                   content="Unattributed request", source_session_id=S_PENDING)
  md_unrelated = ev("master_done", 30, f"{S_PENDING[:8]}-0000-0000-0000-00000000000b",
                    actor="agent", exit_code=0, input_event_id=None,
                    source_session_id=S_PENDING)
  raw = json.dumps({"type": "result", "subtype": "success", "is_error": False}) + "\n"
  builder.session(SessionMetadata(id=S_PENDING, name="Uncertain hub", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  builder.chat_log(S_PENDING, [u_uncertain, md_unrelated])
  builder.master_turn_dir(S_PENDING, iso(10), raw)
  return home

def _round(marker_sid: str, *, input_event: dict | None, done_offset: float,
           done_event_id: str, session_id: str, exit_code: int = 0,
           extra_done: dict | None = None) -> tuple[dict, dict]:
  """One legacy round's chat-log half: run-start marker + its MASTER_DONE."""
  marker = {
      "id": f"m{done_event_id[-11:]}",
      "session_id": marker_sid,
      "timestamp": iso(done_offset - 3),
  }
  done = ev("master_done", done_offset, done_event_id, actor="agent",
            exit_code=exit_code, source_session_id=session_id)
  if input_event is not None:
    done["input_event_id"] = input_event["id"]
  if extra_done:
    done.update(extra_done)
  return marker, done


def _turn_raw(marker_sid: str, *, echo: str | None = None, outcome: str = "success",
              zero_usage: bool = False, with_output: bool = True) -> str:
  """A raw manager log in the claude-family stream shape the producer writes."""
  lines = [json.dumps({"type": "system", "subtype": "init", "session_id": marker_sid})]
  if echo is not None and with_output:
    lines.append(json.dumps({"type": "user", "message": {"role": "user", "content": [
        {"type": "text", "text": echo}]}}))
    lines.append(json.dumps({"type": "assistant", "message": {"content": [
        {"type": "text", "text": "acknowledged"}]}}))
  usage = ({"input_tokens": 0, "output_tokens": 0,
            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
           if zero_usage else
           {"input_tokens": 100, "output_tokens": 30,
            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0})
  result: dict = {"type": "result", "is_error": False,
                  "usage": usage}
  if outcome == "success":
    result["subtype"] = "success"
  else:
    result["subtype"] = "error_during_execution"
  lines.append(json.dumps(result))
  return "".join(line + "\n" for line in lines)


def build_failed_rounds_home(home: Path) -> Path:
  """Failed named rounds, a zero-output named round, and a later successful retry.

  - u_retry: a failed named round (its raw stream settled successfully but the
    consumer's MASTER_DONE records exit 1), then a proven successful retry:
    both runs keep the input id, the failed one as a failed execution.
  - u_zero: a zero-output named round (all-zero usage, no output signal, the
    guard's exit 1): the run imports failed and the input waits behind it.
  """
  builder = FixtureBuilder(home)
  builder.build()
  sid = S_FAILED_ROUNDS
  u_retry = ev("user", 0, f"{sid[:8]}-0000-0000-0000-00000000000a",
               content="Retry the deploy", source_session_id=sid)
  md_failed = ev("master_done", 20, f"{sid[:8]}-0000-0000-0000-00000000000b",
                 actor="agent", exit_code=1, input_event_id=u_retry["id"],
                 source_session_id=sid)
  md_retry = ev("master_done", 60, f"{sid[:8]}-0000-0000-0000-00000000000c",
                actor="agent", exit_code=0, input_event_id=u_retry["id"],
                source_session_id=sid)
  u_zero = ev("user", 70, f"{sid[:8]}-0000-0000-0000-00000000000d",
              content="Summarize the board", source_session_id=sid)
  md_zero = ev("master_done", 90, f"{sid[:8]}-0000-0000-0000-00000000000e",
               actor="agent", exit_code=1, input_event_id=u_zero["id"],
               zero_output=True, source_session_id=sid)
  m1, _ = _round("sid-failed-1", input_event=u_retry, done_offset=20,
                 done_event_id=md_failed["id"], session_id=sid)
  m2, _ = _round("sid-failed-2", input_event=u_retry, done_offset=60,
                 done_event_id=md_retry["id"], session_id=sid)
  m3, _ = _round("sid-zero-1", input_event=u_zero, done_offset=90,
                 done_event_id=md_zero["id"], session_id=sid)
  builder.session(SessionMetadata(id=sid, name="Failed rounds", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  builder.chat_log(sid, [u_retry, m1, md_failed, m2, md_retry, u_zero, m3, md_zero])
  # Round 1: the stream settled with a success-shaped result but the consumer
  # failed the round (exit 1) — the completion fact outranks the raw shape.
  builder.master_turn_dir(sid, iso(10), _turn_raw("sid-failed-1", echo="Retry the deploy"),
                          mtime=iso(18))
  builder.master_turn_dir(sid, iso(40), _turn_raw("sid-failed-2", echo="Retry the deploy"),
                          mtime=iso(58))
  builder.master_turn_dir(sid, iso(75),
                          _turn_raw("sid-zero-1", echo="Summarize the board", zero_usage=True,
                                    with_output=False),
                          mtime=iso(88))
  return home


def build_inflight_failed_home(home: Path) -> Path:
  """The recorded in-flight turn names an input whose raw result failed."""
  builder = FixtureBuilder(home)
  builder.build()
  sid = S_UNBOUND
  u = ev("user", 0, f"{sid[:8]}-0000-0000-0000-00000000000a",
         content="Ship the release", source_session_id=sid)
  meta = SessionMetadata(id=sid, name="Inflight failed", backend="synth",
                         created_at=BASE, updated_at=BASE)
  meta.master_run = MasterRunRecord(
      raw_log=str(home / "sessions" / sid / "data" / "master_runs" / iso(10) /
                  "agent.raw.ndjson"),
      started_at=BASE + timedelta(minutes=10),
      user_event_id=u["id"], pid=0, pid_start="0")
  builder.session(meta)
  builder.chat_log(sid, [u])
  builder.master_turn_dir(sid, iso(10),
                          json.dumps({"type": "result", "subtype": "error_during_execution",
                                      "is_error": True}) + "\n",
                          mtime=iso(15))
  return home


def build_unbound_success_home(home: Path) -> Path:
  """A proven successful named round whose raw log is gone: handled but unbound."""
  builder = FixtureBuilder(home)
  builder.build()
  sid = S_UNBOUND
  u = ev("user", 0, f"{sid[:8]}-0000-0000-0000-00000000000b",
         content="Audit the timers", source_session_id=sid)
  marker, md = _round("sid-gone", input_event=u, done_offset=20,
                      done_event_id=f"{sid[:8]}-0000-0000-0000-00000000000c",
                      session_id=sid)
  builder.session(SessionMetadata(id=sid, name="Unbound success", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  builder.chat_log(sid, [u, marker, md])
  # The round's execution log is missing (deleted with its directory).
  return home


def build_malformed_times_home(home: Path) -> Path:
  """Malformed input timestamps: the old replay rule is positional, not time-based.

  - S_TIMES/U1: malformed timestamp, no later round: proven unhandled (pending).
  - S_TIMES/U2: malformed timestamp, later unnamed rounds exist: uncertain.
  """
  builder = FixtureBuilder(home)
  builder.build()
  sid = S_TIMES
  u1 = ev("user", 0, f"{sid[:8]}-0000-0000-0000-00000000000a",
          content="First malformed request", source_session_id=sid)
  u1["timestamp"] = "not-a-timestamp"
  u2 = ev("user", 0, f"{sid[:8]}-0000-0000-0000-00000000000b",
          content="Second malformed request", source_session_id=sid)
  u2["timestamp"] = ""
  md_unrelated = ev("master_done", 30, f"{sid[:8]}-0000-0000-0000-00000000000c",
                    actor="agent", exit_code=0, input_event_id=None,
                    source_session_id=sid)
  marker = {"id": f"{sid[:8]}-0000-0000-0000-00000000000d",
            "session_id": "sid-times-1", "timestamp": iso(28)}
  builder.session(SessionMetadata(id=sid, name="Malformed times", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  # u1's malformed timestamp sorts it first, but the positional replay rule
  # reads order, not clocks: u1 precedes every round and is replay-proven
  # unhandled; u2 precedes the completed unnamed round and stays uncertain.
  builder.chat_log(sid, [u1, u2, marker, md_unrelated])
  builder.master_turn_dir(sid, iso(20), _turn_raw("sid-times-1", echo=None),
                          mtime=iso(29))
  return home


def build_scheduled_unproven_home(home: Path) -> Path:
  """Scheduled wakes without proven handling: interrupted round, echo-less round."""
  builder = FixtureBuilder(home)
  builder.build()
  sid = S_SCHED_UNPROVEN
  st_interrupted = ev("scheduled_trigger", 0, f"{sid[:8]}-0000-0000-0000-00000000000a",
                      actor="system", content="interrupted wake", source_session_id=sid)
  st_echoless = ev("scheduled_trigger", 60, f"{sid[:8]}-0000-0000-0000-00000000000b",
                   actor="system", content="echoless wake", source_session_id=sid)
  marker_interrupted = {"id": f"{sid[:8]}-0000-0000-0000-00000000000c",
                        "session_id": "sid-sched-int", "timestamp": iso(3)}
  marker_echoless = {"id": f"{sid[:8]}-0000-0000-0000-00000000000d",
                     "session_id": "sid-sched-echo", "timestamp": iso(63)}
  md_echoless = ev("master_done", 80, f"{sid[:8]}-0000-0000-0000-00000000000e",
                   actor="agent", exit_code=0, input_event_id=None,
                   source_session_id=sid)
  builder.session(SessionMetadata(id=sid, name="Scheduled unproven", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  builder.chat_log(sid, [st_interrupted, marker_interrupted, st_echoless,
                         marker_echoless, md_echoless])
  # The echoless wake's round is proven and successful but its transport log
  # never echoes the wake text, so it cannot prove it consumed this wake.
  builder.master_turn_dir(sid, iso(62), _turn_raw("sid-sched-echo", echo=None),
                          mtime=iso(78))
  # The interrupted wake's round started but never completed: no log result.
  builder.master_turn_dir(sid, iso(2), json.dumps({"type": "assistant", "message": {
      "content": [{"type": "text", "text": "partial"}]}}) + "\n", mtime=iso(4))
  return home


def build_identical_requests_home(home: Path) -> Path:
  """Identical input bodies in distinct requests: identity binds each to its own run."""
  builder = FixtureBuilder(home)
  builder.build()
  sid = S_IDENTICAL
  body = "Run the same migration twice"
  u1 = ev("user", 0, f"{sid[:8]}-0000-0000-0000-00000000000a", content=body,
          source_session_id=sid)
  u2 = ev("user", 60, f"{sid[:8]}-0000-0000-0000-00000000000b", content=body,
          source_session_id=sid)
  md1 = ev("master_done", 20, f"{sid[:8]}-0000-0000-0000-00000000000c",
           actor="agent", exit_code=0, input_event_id=u1["id"], source_session_id=sid)
  md2 = ev("master_done", 80, f"{sid[:8]}-0000-0000-0000-00000000000d",
           actor="agent", exit_code=0, input_event_id=u2["id"], source_session_id=sid)
  m1, _ = _round("sid-ident-1", input_event=u1, done_offset=20,
                 done_event_id=md1["id"], session_id=sid)
  m2, _ = _round("sid-ident-2", input_event=u2, done_offset=80,
                 done_event_id=md2["id"], session_id=sid)
  builder.session(SessionMetadata(id=sid, name="Identical requests", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  builder.chat_log(sid, [u1, m1, md1, u2, m2, md2])
  builder.master_turn_dir(sid, iso(10), _turn_raw("sid-ident-1", echo=body),
                          mtime=iso(18))
  builder.master_turn_dir(sid, iso(70), _turn_raw("sid-ident-2", echo=body),
                          mtime=iso(78))
  # A later turn whose stream merely quotes the first request's text: it
  # adopts its own session id and cannot inherit the first round's input.
  quote_raw = (json.dumps({"type": "system", "subtype": "init",
                           "session_id": "sid-ident-quote"}) + "\n"
               + json.dumps({"type": "assistant", "message": {"content": [
                   {"type": "text", "text": "Earlier you asked: " + body}]}}) + "\n"
               + json.dumps({"type": "result", "subtype": "success", "is_error": False,
                             "usage": {"input_tokens": 50, "output_tokens": 10,
                                       "cache_read_input_tokens": 0,
                                       "cache_creation_input_tokens": 0}}) + "\n")
  builder.master_turn_dir(sid, iso(120), quote_raw, mtime=iso(128))
  return home


def build_review_retry_home(home: Path) -> Path:
  """A failed reviewer attempt followed by a provably accepted successful retry."""
  builder = FixtureBuilder(home)
  builder.build()
  builder.session(SessionMetadata(id=S_REVIEW_RETRY, name="Review retry", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  builder.thread(S_REVIEW_RETRY, ThreadMetadata(
      id=T_WORK, session_id=S_REVIEW_RETRY, description="Implement the retry fix",
      status="completed", created_at=BASE, started_at=BASE,
      completed_at=BASE + timedelta(minutes=20), exit_code=0, backend="synth",
      task_type=TaskType.QUICK_EDIT, require_review=True),
      events=[ev("result", 19, f"{S_REVIEW_RETRY[:8]}-0000-0000-0000-00000000000a",
                 actor="agent", subtype="success", is_error=False,
                 source_session_id=S_REVIEW_RETRY)])
  builder.thread(S_REVIEW_RETRY, ThreadMetadata(
      id=T_RETRY_FAILED, session_id=S_REVIEW_RETRY,
      description="Review of: Implement the retry fix", status="failed",
      created_at=BASE + timedelta(minutes=21), started_at=BASE + timedelta(minutes=21),
      completed_at=BASE + timedelta(minutes=25), exit_code=1, backend="synth",
      review_of=T_WORK, require_review=False),
      events=[ev("result", 24, f"{S_REVIEW_RETRY[:8]}-0000-0000-0000-00000000000b",
                 actor="agent", subtype="error_during_execution", is_error=True,
                 source_session_id=S_REVIEW_RETRY)])
  builder.thread(S_REVIEW_RETRY, ThreadMetadata(
      id=T_RETRY_OK, session_id=S_REVIEW_RETRY,
      description="Review of: Implement the retry fix", status="completed",
      created_at=BASE + timedelta(minutes=30), started_at=BASE + timedelta(minutes=30),
      completed_at=BASE + timedelta(minutes=40), exit_code=0, backend="synth",
      review_of=T_WORK, require_review=False),
      events=[ev("result", 39, f"{S_REVIEW_RETRY[:8]}-0000-0000-0000-00000000000c",
                 actor="agent", subtype="success", is_error=False,
                 source_session_id=S_REVIEW_RETRY)])
  return home


def build_review_metadata_conflict_home(home: Path) -> Path:
  """Completed review metadata over a retained failed review result cannot close."""
  builder = FixtureBuilder(home)
  builder.build()
  builder.session(SessionMetadata(id=S_REVIEW_RETRY, name="Review conflict", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  builder.thread(S_REVIEW_RETRY, ThreadMetadata(
      id=T_WORK, session_id=S_REVIEW_RETRY, description="Implement the conflict fix",
      status="completed", created_at=BASE, started_at=BASE,
      completed_at=BASE + timedelta(minutes=20), exit_code=0, backend="synth",
      task_type=TaskType.QUICK_EDIT, require_review=True),
      events=[ev("result", 19, f"{S_REVIEW_RETRY[:8]}-0000-0000-0000-00000000000a",
                 actor="agent", subtype="success", is_error=False,
                 source_session_id=S_REVIEW_RETRY)])
  builder.thread(S_REVIEW_RETRY, ThreadMetadata(
      id=T_REVIEW_CONFLICT, session_id=S_REVIEW_RETRY,
      description="Review of: Implement the conflict fix", status="completed",
      created_at=BASE + timedelta(minutes=21), started_at=BASE + timedelta(minutes=21),
      completed_at=BASE + timedelta(minutes=30), exit_code=0, backend="synth",
      review_of=T_WORK, require_review=False),
      events=[ev("result", 29, f"{S_REVIEW_RETRY[:8]}-0000-0000-0000-00000000000b",
                 actor="agent", subtype="error_during_execution", is_error=True,
                 source_session_id=S_REVIEW_RETRY)])
  return home


def _implement_with_branch(home: Path, *, worktree: bool, move_branch: bool) -> Path:
  """An implement home whose result commit is pinned; optionally reuse the branch."""
  builder = FixtureBuilder(home)
  builder.build()
  builder.session(SessionMetadata(id=S_WORKERS, name="Landing hub", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  repo = home / "repo"
  work_branch = "work/pinned"
  env = dict(**os.environ)
  stamp = (BASE + timedelta(minutes=15)).strftime("%Y-%m-%dT%H:%M:%S %z")
  env["GIT_AUTHOR_DATE"] = stamp
  env["GIT_COMMITTER_DATE"] = stamp
  subprocess.run(["git", "-C", str(repo), "checkout", "-qb", work_branch], check=True)
  (repo / "pinned.txt").write_text("pinned work\n", encoding="utf-8")
  subprocess.run(["git", "-C", str(repo), "add", "-A"], check=True)
  subprocess.run(["git", "-C", str(repo), "commit", "-qm", "pinned work"], check=True, env=env)
  result_commit = subprocess.run(
      ["git", "-C", str(repo), "rev-parse", "HEAD"], check=True, capture_output=True,
      text=True).stdout.strip()
  if move_branch:
    # A later, unrelated reuse of the work branch: its tip is not this run's
    # output, and the run's window proves it.
    later = (BASE + timedelta(days=2)).strftime("%Y-%m-%dT%H:%M:%S %z")
    env["GIT_AUTHOR_DATE"] = later
    env["GIT_COMMITTER_DATE"] = later
    (repo / "pinned.txt").write_text("reused later\n", encoding="utf-8")
    subprocess.run(["git", "-C", str(repo), "add", "-A"], check=True)
    subprocess.run(["git", "-C", str(repo), "commit", "-qm", "reuse the branch"],
                   check=True, env=env)
    env["GIT_AUTHOR_DATE"] = stamp
    env["GIT_COMMITTER_DATE"] = stamp
  subprocess.run(["git", "-C", str(repo), "checkout", "-q", "main"], check=True)
  subprocess.run(["git", "-C", str(repo), "merge", "-q", "--ff-only", work_branch],
                 check=True)
  if worktree:
    # The run's own retained worktree still sits at the run's result commit,
    # even though the branch later moved past it.
    wt = home / "worktrees" / "work-pinned"
    subprocess.run(["git", "-C", str(repo), "worktree", "add", "-q",
                    "--detach", str(wt), result_commit], check=True, env=env)
  builder.thread(S_WORKERS, ThreadMetadata(
      id=T_WORK, session_id=S_WORKERS, description="Implement the pinned fix",
      status="completed", created_at=BASE, started_at=BASE,
      completed_at=BASE + timedelta(minutes=30), exit_code=0, backend="synth",
      repo_path=str(repo), branch_name=work_branch, base_branch="main",
      task_type=TaskType.IMPLEMENT, require_review=False,
      worktree_path=str(home / "worktrees" / "work-pinned") if worktree else None),
      events=[ev("result", 29, f"{S_WORKERS[:8]}-0000-0000-0000-00000000000a", actor="agent",
                 subtype="success", is_error=False, source_session_id=S_WORKERS)])
  return home


def build_resume_bound_home(home: Path) -> Path:
  """A crashed round resumed to completion under the same backend session id.

  The resume anchor reuses the crashed turn's backend id, so both transports
  adopt it; the consumer's MASTER_DONE closes the RESUMED round (the
  projection's latest-open interval), not the dead one. Only that interval
  rule binds the input to the resumed turn's own transport.
  """
  builder = FixtureBuilder(home)
  builder.build()
  sid = S_RESUME
  u = ev("user", 0, f"{sid[:8]}-0000-0000-0000-00000000000a",
         content="Retry after the crash", source_session_id=sid)
  marker_crashed = {"id": f"{sid[:8]}-0000-0000-0000-0000000000b1",
                    "session_id": "sid-resume-1", "timestamp": iso(3)}
  marker_resumed = {"id": f"{sid[:8]}-0000-0000-0000-0000000000b2",
                    "session_id": "sid-resume-1", "timestamp": iso(63)}
  done = ev("master_done", 80, f"{sid[:8]}-0000-0000-0000-0000000000b3",
            actor="agent", exit_code=0, input_event_id=u["id"], source_session_id=sid)
  builder.session(SessionMetadata(id=sid, name="Resume retry", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  builder.chat_log(sid, [u, marker_crashed, marker_resumed, done])
  # The crashed turn never settled (no terminal result → not a proven
  # completion); the resumed turn's transport adopts the same backend id.
  crashed_raw = (json.dumps({"type": "system", "subtype": "init",
                             "session_id": "sid-resume-1"}) + "\n"
                 + json.dumps({"type": "assistant", "message": {
                     "content": [{"type": "text", "text": "partial"}]}}) + "\n")
  builder.master_turn_dir(sid, iso(2), crashed_raw, mtime=iso(4))
  builder.master_turn_dir(sid, iso(60), _turn_raw("sid-resume-1", echo="Retry after the crash"),
                          mtime=iso(78))
  return home


def build_scheduled_straddle_home(home: Path) -> Path:
  """A wake fired while the previous wake's identical-text round was still open.

  Both rounds' launch prompts echo the recurring wake text; the straddling
  round BEGAN before this trigger and belongs to the earlier wake. Only the
  wake's own later round is its proven handling — the straddling round's
  identical echo must not inherit this wake.
  """
  builder = FixtureBuilder(home)
  builder.build()
  sid = S_STRADDLE
  text = "tidy the board"
  st = ev("scheduled_trigger", 5, f"{sid[:8]}-0000-0000-0000-00000000000a",
          actor="system", content=text, source_session_id=sid)
  marker_a = {"id": f"{sid[:8]}-0000-0000-0000-0000000000b1",
              "session_id": "sid-cron-a", "timestamp": iso(2)}
  done_a = ev("master_done", 8, f"{sid[:8]}-0000-0000-0000-0000000000b2",
              actor="agent", exit_code=0, source_session_id=sid)
  marker_b = {"id": f"{sid[:8]}-0000-0000-0000-0000000000b3",
              "session_id": "sid-cron-b", "timestamp": iso(12)}
  done_b = ev("master_done", 20, f"{sid[:8]}-0000-0000-0000-0000000000b4",
              actor="agent", exit_code=0, source_session_id=sid)
  builder.session(SessionMetadata(id=sid, name="Straddled wake", backend="synth",
                                  created_at=BASE, updated_at=BASE))
  # Chat order is evidence: the wake lands while round A is open; its own
  # round B launches behind A's completion.
  builder.chat_log(sid, [marker_a, st, done_a, marker_b, done_b])
  builder.master_turn_dir(sid, iso(1), _turn_raw("sid-cron-a", echo=text), mtime=iso(7))
  builder.master_turn_dir(sid, iso(10), _turn_raw("sid-cron-b", echo=text), mtime=iso(18))
  return home


def build_moved_branch_home(home: Path) -> Path:
  """The work branch was reused after the run: the tip is not this run's output."""
  return _implement_with_branch(home, worktree=False, move_branch=True)


def build_pinned_worktree_home(home: Path) -> Path:
  """A moved branch whose run's own worktree still pins the result commit."""
  return _implement_with_branch(home, worktree=True, move_branch=True)
