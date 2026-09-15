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


def ev(type: str, offset: float, event_id: str, *, actor: str = "user",
       source_session_id: str | None = None, **extra: Any) -> dict:
  event = {"id": event_id, "type": type, "timestamp": iso(offset),
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

  def master_turn_dir(self, session_id: str, started_at: str, raw: str) -> Path:
    directory = self.home / "sessions" / session_id / "data" / "master_runs" / started_at
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "agent.raw.ndjson").write_text(raw, encoding="utf-8")
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
    subprocess.run(git + ["config", "user.email", "synth@example.invalid"], check=True)
    subprocess.run(git + ["config", "user.name", "Synth Tester"], check=True)
    subprocess.run(git + ["add", "-A"], check=True)
    subprocess.run(git + ["commit", "-qm", "synthetic base"], check=True)


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
  raw_turn = (
      json.dumps({"type": "assistant", "message": {"content": [{"type": "text", "text": handled_text}]}}) + "\n"
      + json.dumps({"type": "result", "subtype": "success", "is_error": False}) + "\n")
  raw_scheduled = json.dumps({"type": "result", "subtype": "success", "is_error": False}) + "\n"

  meta1 = SessionMetadata(id=S_ORDINARY, name="Deploy pipeline ops", backend="synth",
                          created_at=BASE, updated_at=BASE)
  meta1.cc_session_id = "native-cc-ordinary"
  # The rotation that produced chat_events.2026-W01.jsonl recorded its
  # write-time offset (three archived lines) in the metadata.
  meta1.archive_offset = 3
  meta1.master_run = MasterRunRecord(
      raw_log=str(home / "sessions" / S_ORDINARY / "data" / "master_runs" / iso(35) / "agent.raw.ndjson"),
      started_at=BASE + timedelta(minutes=35), pid=0, pid_start="0")
  builder.session(meta1)
  builder.chat_log(S_ORDINARY, [u_handled, md_handled, st_fired, ws_old],
                   archives={"chat_events.2026-W01.jsonl": [u_rotated, md_rotated, assistant_rotated]})
  builder.master_turn_dir(S_ORDINARY, iso(10), raw_turn)
  builder.master_turn_dir(S_ORDINARY, iso(35), raw_scheduled)
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
      f"cron: '30 2 * * *'\nrepo: {home / 'repo'}\n"
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
  builder.thread(S_IMPROVE, ThreadMetadata(
      id=T_ITER1, session_id=S_IMPROVE, status="completed",
      description=("Iterative improvement — iteration 1/2\n"
                   "Goal: Reduce the import time of the metrics module"),
      created_at=BASE + timedelta(minutes=101), started_at=BASE + timedelta(minutes=101),
      completed_at=BASE + timedelta(minutes=111), exit_code=0, backend="synth",
      repo_path=str(home / "repo"), branch_name="improve/metrics", base_branch="main",
      require_review=False))
  builder.thread(S_IMPROVE, ThreadMetadata(
      id=T_ITER2, session_id=S_IMPROVE, status="failed",
      description=("Iterative improvement — iteration 2/2\n"
                   "Goal: Reduce the import time of the metrics module"),
      created_at=BASE + timedelta(minutes=112), started_at=BASE + timedelta(minutes=112),
      completed_at=BASE + timedelta(minutes=120), exit_code=1, backend="synth",
      repo_path=str(home / "repo"), branch_name="improve/metrics", base_branch="main",
      require_review=False))

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
  subprocess.run(["git", "-C", str(repo), "commit", "-qm", "landed work"], check=True)
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
  builder.thread(S_IMPROVE, ThreadMetadata(
      id=T_ITER1, session_id=S_IMPROVE, status="completed",
      description=f"Iterative improvement — iteration 1/1\nGoal: {goal}",
      created_at=BASE + timedelta(minutes=1), started_at=BASE + timedelta(minutes=1),
      completed_at=BASE + timedelta(minutes=10), exit_code=0, backend="synth",
      repo_path=str(home / "repo"), branch_name="improve/probe", base_branch="main",
      require_review=False))
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
