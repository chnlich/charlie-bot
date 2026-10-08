#!/usr/bin/env python3
"""Real-browser harness for the session tree UI (integrated sidebar + main chat).

Executable repo-owned recipe (not a prose runbook). Reviewer reads this file
before running it. Isolation contract:

- App under test. The real shipped application (``server.app``: pages, static,
  files, task APIs, websockets) served by uvicorn with ``lifespan="off"`` — no
  crash recovery, scheduler, trigger scans or global cleanup. It binds one
  explicit unused local port; the production service is never started,
  stopped or contacted.
- Synthetic home. A fresh temporary CHARLIEBOT_HOME carries the config and a
  synthetic operator key; inherited production credentials are cleared from
  the harness environment. The seeded backend is a scripted ``codex`` entry
  that never launches (no browser flow dispatches a chat input), so there are
  no real model, cron or external side effects and no real data copies.
- Synthetic data. The scenario tree (root manager → feature manager → two
  workers, run records with recorded facts, pending inputs, and one unread
  reply on the feature manager seeded through SessionLifecycle.mark_unread),
  the sidebar views' schedule fixtures (two bound nodes via cron.d
  session_id bindings, one archived firing under its bound node, one starred
  node, one broken cron file), all seeded through the same task_sessions
  owner the APIs serve, in this process only. Every UI mutation under test
  then rides the real HTTP API from the browser.
- Browser. System google-chrome (checked first; an absent binary is an
  explicit failure — never a faked pass) driven over CDP with a private
  ``--user-data-dir`` profile inside the harness temp dir. Screenshots,
  per-scenario assertion results and the exact tested commit land in
  --evidence-dir (default: a directory under the host temp dir, never in git).

Run:  uv run python tools/browser_harness_session_tree.py \
        [--evidence-dir DIR] [--keep] [--chrome BIN]
"""

from __future__ import annotations

import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
  sys.path.insert(0, str(REPO_ROOT))

import argparse  # noqa: E402
import asyncio  # noqa: E402
import base64  # noqa: E402
import json  # noqa: E402
import os  # noqa: E402
import shutil  # noqa: E402
import socket  # noqa: E402
import subprocess  # noqa: E402
import tempfile  # noqa: E402
import threading  # noqa: E402
import time  # noqa: E402
import urllib.request  # noqa: E402
from collections.abc import Callable  # noqa: E402
from datetime import datetime, timedelta, timezone  # noqa: E402

# Evidence defaults to a host temp directory so the public repo carries no
# host path; pass --evidence-dir to keep evidence with its owning session.
EVIDENCE_ROOT_DEFAULT = Path(tempfile.gettempdir()) / "charliebot-session-tree-evidence"


def open_evidence_dir(evidence_dir: Path) -> str:
  """Create the run's evidence directory and return the commit it documents (the
    checkout's HEAD), so every artifact names the code it was produced from.
    """
  evidence_dir.mkdir(parents=True, exist_ok=True)
  return subprocess.run(["git", "rev-parse", "HEAD"], cwd=REPO_ROOT, capture_output=True, text=True,
                        check=True).stdout.strip()


def log(message: str) -> None:
  print(message, flush=True)


def stop_child(proc: subprocess.Popen | None, grace_s: float, kill_reap_s: float) -> None:
  """Stop a harness child process: SIGTERM, wait up to ``grace_s``, then SIGKILL and
    reap within ``kill_reap_s``. A None handle or an already-exited child needs no stop.
    """
  if proc is None or proc.poll() is not None:
    return
  proc.terminate()
  try:
    proc.wait(timeout=grace_s)
  except subprocess.TimeoutExpired:
    proc.kill()
    proc.wait(timeout=kill_reap_s)


def fail(message: str) -> None:
  raise SystemExit(f"BROWSER HARNESS FAILED: {message}")


def pick_free_port() -> int:
  with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
    sock.bind(("127.0.0.1", 0))
    return int(sock.getsockname()[1])


def mint_access_key(prefix: str) -> str:
  """The trial home's operator credential: the caller's prefix plus 16 random hex chars."""
  return prefix + os.urandom(8).hex()


def write_credentials_yaml(home: Path, access_key: str) -> None:
  """Seed the trial home's credentials.yaml with its operator key.

  The body is the ``charliebot.access_key`` secret shape the credentials
  loader reads (src/infra/credentials.py).
  """
  (home / "credentials.yaml").write_text(f"charliebot:\n  access_key: {access_key}\n", encoding="utf-8")


# ---------------------------------------------------------------------------
# CDP mini-client
# ---------------------------------------------------------------------------


class CDP:
  """Minimal Chrome DevTools Protocol client over the browser endpoint."""

  def __init__(self, ws) -> None:
    self._ws = ws
    self._next_id = 1
    self._pending: dict[int, asyncio.Future] = {}
    self._events: list[dict] = []
    self.console_errors: list[str] = []
    self.list_fetch_counts: list[str] = []
    self.mutations: list[dict] = []
    self._reader = asyncio.create_task(self._read_loop())

  async def close(self) -> None:
    """Close the browser websocket; the reader loop ends with it."""
    await self._ws.close()

  async def _read_loop(self) -> None:
    try:
      async for raw in self._ws:
        msg = json.loads(raw)
        if "id" in msg:
          fut = self._pending.pop(msg["id"], None)
          if fut and not fut.done():
            if "error" in msg:
              fut.set_exception(RuntimeError(str(msg["error"])))
            else:
              fut.set_result(msg.get("result", {}))
        else:
          self._events.append(msg)
          method = msg.get("method", "")
          if method == "Runtime.consoleAPICalled" and msg["params"].get("type") == "error":
            self.console_errors.append(json.dumps(msg["params"].get("args", []))[:500])
          if method == "Runtime.exceptionThrown":
            self.console_errors.append(str(msg["params"].get("exceptionDetails", {}))[:500])
          if method == "Network.requestWillBeSent":
            request = msg["params"]["request"]
            url = request["url"]
            if request.get("method") == "GET" and url.endswith("/api/sessions/"):
              self.list_fetch_counts.append(url)
            if request.get("method") in ("POST", "PATCH") and "/api/sessions" in url:
              # Every outgoing mutation with its exact target URL
              # and body — the ground truth for "which task did
              # this dialog actually submit against".
              self.mutations.append(
                  {
                      "url": url,
                      "method": request.get("method"),
                      "body": (request.get("postData") or "")[:600],
                  })
    except Exception as exc:  # reader exit is fine at shutdown
      log(f"cdp reader stopped: {exc!r}")

  async def send(self, method: str, params: dict | None = None, session_id: str | None = None) -> dict:
    msg_id = self._next_id
    self._next_id += 1
    payload = {"id": msg_id, "method": method, "params": params or {}}
    if session_id:
      payload["sessionId"] = session_id
    fut = asyncio.get_running_loop().create_future()
    self._pending[msg_id] = fut
    await self._ws.send(json.dumps(payload))
    return await asyncio.wait_for(fut, timeout=30)

  def drain_list_fetches(self) -> list[str]:
    seen = self.list_fetch_counts
    self.list_fetch_counts = []
    return seen

  def mutations_since(self, mark: int) -> list[dict]:
    return self.mutations[mark:]

  def mutation_mark(self) -> int:
    return len(self.mutations)


# ---------------------------------------------------------------------------
# Scenario seeding (through the task_sessions owner, in-process only)
# ---------------------------------------------------------------------------


def append_events_line(path: Path, event: dict) -> None:
  """One event line onto a run's events.jsonl (the append-only shape)."""
  with path.open("a", encoding="utf-8") as f:
    f.write(json.dumps(event) + "\n")


def grow_run_events(path: Path, stop: threading.Event, counter: dict) -> None:
  """Append one committed assistant turn to the live Run's events log every
    ~1.2 s until *stop* — the growing file the streaming assertions read."""
  n = 0
  while not stop.is_set() and n < 120:
    if stop.wait(1.2):
      break
    n += 1
    append_events_line(
        path, {
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": f"streamed progress line {n}"
                }]
            },
            "timestamp": datetime.now(timezone.utc).isoformat(),
        })
    counter["lines"] = n


def seed_memory_store(home: Path) -> None:
  """A minimal memory store: one resident topic (full delivery) and one
    non-resident topic (index delivery), both master-audience — so the Context
    panel shows full-versus-index provenance from the real assembly."""
  memory_dir = home / "memory"
  (memory_dir / "entries" / "program-notes").mkdir(parents=True)
  (memory_dir / "entries" / "archive-notes").mkdir(parents=True)
  (memory_dir / "topics").write_text("program-notes resident\narchive-notes\n", encoding="utf-8")
  (memory_dir / "entries" / "program-notes" / "deploy-runbook.md").write_text(
      "---\nscope: host\ntopic: program-notes\naudience: master\n"
      "title: Deploy runbook\n---\nDeploy through the pinned recipe; verify the health endpoint first.\n",
      encoding="utf-8")
  (memory_dir / "entries" / "archive-notes" / "old-migration.md").write_text(
      "---\nscope: host\ntopic: archive-notes\naudience: master\n"
      "title: Old migration notes\n---\nThe 2024 migration is complete; query on demand only.\n",
      encoding="utf-8")


def first_registered_origin() -> tuple[str, BaseModel]:
  """The field name and one built model of the first registered session-metadata ``*_origin`` field.

  The model's required ``str`` fields take synthetic digit strings. Called after
  ``registrations.register_all()``, so every feature package's registration is visible.
  """
  from typing import get_args

  from pydantic import BaseModel

  from src.infra import metadata_slot_registration
  from src.infra.deferred import import_attr

  for registration in metadata_slot_registration.registered():
    if registration.on != metadata_slot_registration.ON_SESSION:
      continue
    model = import_attr(registration.model)
    for field_name, field in model.model_fields.items():
      if not field_name.endswith("_origin"):
        continue
      candidates = get_args(field.annotation) or (field.annotation,)
      origin_model = next((c for c in candidates if isinstance(c, type) and issubclass(c, BaseModel)), None)
      if origin_model is None:
        continue
      values = {
          name: str(900_000_000_000_000_010 + index)
          for index, (name, info) in enumerate(origin_model.model_fields.items())
          if info.is_required() and info.annotation is str
      }
      return field_name, origin_model(**values)
  raise RuntimeError("no registered session-metadata model has an *_origin field")


async def seed_scenario(home: Path) -> dict:
  """Create the acceptance scenario's task tree and recorded run facts.

    The operator-actions scenarios need collection sizes beyond one page:
    more roots than the move chooser's page, a manager with more direct
    children than the old single 100-record read, and a run history deeper
    than the completion dialog's page — all seeded through the same
    task_sessions owner the APIs serve, in-process only.
    """
  os.environ["CHARLIEBOT_HOME"] = str(home)
  from src.app import registrations
  from src.infra import event_types as ET
  from src.infra.config import get_config
  from src.infra.models import PatchSessionTaskRequest, RunRecord, TaskSpec, ThreadMetadata
  from src.runtime.run_token import CallerIdentity
  from src.runtime.session_anchors import SessionAnchors
  from src.runtime.session_events import SessionEvents
  from src.runtime.session_fork import SessionFork
  from src.runtime.session_lifecycle import SessionLifecycle
  from src.runtime.session_listing import SessionListing
  from src.runtime.session_search import SessionSearch
  from src.runtime.session_sidebar import SessionSidebar
  from src.runtime.session_store import SessionStore
  from src.runtime.sessions import SessionManager
  from src.runtime.task_sessions import TaskTreeManager

  registrations.register_all()
  cfg = get_config()
  store = SessionStore(cfg)
  sidebar = SessionSidebar(cfg, store)
  events = SessionEvents(cfg, store)
  lifecycle = SessionLifecycle(cfg, store, events)
  session_mgr = SessionManager(
      cfg, store, events, sidebar, SessionListing(cfg, store, sidebar), SessionSearch(cfg, store, events, sidebar),
      lifecycle, SessionFork(cfg, store, events), SessionAnchors(cfg, store, events))
  tree = TaskTreeManager(cfg, session_mgr)
  OP = CallerIdentity(kind="operator")

  async def seed() -> dict:
    # Bulk roots first: created_at order puts them on the move chooser's
    # first pages, so "Program rollout" and everything created after land
    # on a LATER page.
    bulk = {}
    for i in range(1, 29):
      meta = await tree.create_task(
          request_id=f"seed-bulk-{i}",
          task_parent_id=None,
          profile="manager",
          task=TaskSpec(goal=f"bulk root {i:02d}"),
          name=f"Bulk root {i:02d}",
          backend=None,
          caller=OP)
      if i <= 3:
        bulk[f"bulk{i}"] = meta.id
    root = await tree.create_task(
        request_id="seed-root",
        task_parent_id=None,
        profile="manager",
        task=None,
        name="Program rollout",
        backend=None,
        caller=OP)
    await tree.patch_task(
        root.id,
        PatchSessionTaskRequest(
            task={
                "goal": "Ship the program rollout",
                "acceptance": ["all features delivered"],
                "context_refs": [],
                "repo_path": None,
                "base_branch": None,
                "task_type": None,
                "keep_worktree": False
            }),
        caller=OP)
    feature = await tree.create_task(
        request_id="seed-feature",
        task_parent_id=root.id,
        profile="manager",
        task=None,
        name="Feature alpha",
        backend=None,
        caller=OP)
    await tree.patch_task(
        feature.id,
        PatchSessionTaskRequest(
            task={
                "goal": "Deliver feature alpha end to end",
                "acceptance": ["tests pass"],
                "context_refs": [],
                "repo_path": None,
                "base_branch": None,
                "task_type": "implement",
                "keep_worktree": False
            }),
        caller=OP)
    worker1 = await tree.create_task(
        request_id="seed-w1",
        task_parent_id=feature.id,
        profile="worker",
        task=None,
        name="Worker one",
        backend=None,
        caller=OP)
    worker2 = await tree.create_task(
        request_id="seed-w2",
        task_parent_id=feature.id,
        profile="worker",
        task=None,
        name="Worker two",
        backend=None,
        caller=OP)
    # Recorded run facts: worker one finished a work run and a review run
    # (one leaf, two run rows); worker two has a pending input.
    await tree.runs.register_run(
        RunRecord(
            id="run-w1-work", session_id=worker1.id, kind="work", backend="fake-scripted", model="scripted-model"),
        task_spec_text="worker spec")
    await tree.dispatch.finish_run(worker1.id, "run-w1-work", outcome="success")
    await tree.runs.register_run(
        RunRecord(
            id="run-w1-review",
            session_id=worker1.id,
            kind="review",
            backend="fake-scripted",
            model="scripted-model",
            review_of_run_id="run-w1-work"),
        task_spec_text="review spec")
    await tree.dispatch.finish_run(worker1.id, "run-w1-review", outcome="success")
    # The delivered report lands on the feature manager as a pending input;
    # acknowledge it so the feature task stays editable (worker two's user
    # input stays pending on purpose — the blocker scenario uses it).
    # A successfully delivered worker autoarchives (server facts); the
    # scenario keeps it visible via its own presentation preference.
    await tree.patch_task(worker1.id, PatchSessionTaskRequest(presentation="shown"), caller=OP)
    report_inputs = tree.dispatch.pending_inputs(feature.id)
    if report_inputs:
      await tree.completion.acknowledge_inputs(
          feature.id,
          request_id="seed-ack-report",
          input_ids=[str(e["id"]) for e in report_inputs],
          note="seed: report accepted",
          caller=OP)
    await tree.dispatch.admit_input(
        worker2.id, event_type=ET.USER, content="Please also verify the docs page", actor="user")
    # The root manager carries the real-user takeoff authorization an
    # agent-scoped creation is judged against (takeoff_gate).
    await tree.events.append(
        root.id, {
            "id": "seed-takeoff-user",
            "type": ET.USER,
            "timestamp": "2026-01-01T00:00:00+00:00",
            "content": "take off and run the program rollout"
        })
    await tree.completion.acknowledge_inputs(
        root.id,
        request_id="seed-ack-takeoff",
        input_ids=["seed-takeoff-user"],
        note="seed: operator authorization",
        caller=OP)
    # Long-history node: 150 started runs whose committed snapshots are
    # distinguishable (generation 0001..0150), plus never-launched
    # reservations. The current-run Context selection must show generation
    # 0150 — the whole-history latest launch — not the first page's tail.
    from src.runtime.control_events import sha256_hex
    from src.runtime.task_prompts import PromptBlock, PromptSnapshot, PromptSource
    long_worker = await tree.create_task(
        request_id="seed-long",
        task_parent_id=feature.id,
        profile="worker",
        task=TaskSpec(goal="carry a long run history", task_type="implement"),
        name="Long history worker",
        backend=None,
        caller=OP)
    base = datetime(2026, 1, 1, tzinfo=timezone.utc)
    latest_hash = None
    for i in range(1, 151):
      run_id = f"run-{i:04d}"
      await tree.runs.register_run(
          RunRecord(
              id=run_id,
              session_id=long_worker.id,
              kind="work",
              backend="fake-scripted",
              model="scripted-model",
              started_at=base + timedelta(minutes=i)))
      text = f"managed instructions generation {i:04d}"
      block = PromptBlock(
          sources=(PromptSource(scope="base", source_ref="base:work", source_session_id=None),),
          body_ref=sha256_hex(text),
          delivery="full",
          text=text)
      snapshot = PromptSnapshot(blocks=(block,))
      snap_path = tree.runs.run_dir(long_worker.id, run_id) / "prompt_snapshot.json"
      snap_path.parent.mkdir(parents=True, exist_ok=True)
      snap_path.write_text(json.dumps(snapshot.to_json_dict()), encoding="utf-8")
      await tree.runs.record_observation(long_worker.id, run_id, prompt_snapshot_ref=str(snap_path))
      await tree.dispatch.finish_run(long_worker.id, run_id, outcome="success")
      if i == 150:
        latest_hash = snapshot.prompt_hash
    for q in ("queued-a", "queued-b"):
      await tree.runs.register_run(RunRecord(id=q, session_id=long_worker.id, kind="work"))
    # An active Run identity on the root manager: the seeded agent caller's
    # run token must bind a launched, non-terminal Run (run_identity_refusal).
    from src.runtime.runs import read_pid_stat
    pid_start, _state = read_pid_stat(os.getpid())
    await tree.runs.register_run(
        RunRecord(id="agent-auth-run", session_id=root.id, kind="manager_turn", pid=os.getpid(), pid_start=pid_start))

    # --- operator-actions scenarios (S16-S20) ---------------------------
    # A late root holding an intermediate manager (a move target reached
    # through the chooser's later roots page) and a wide manager whose
    # direct-children list exceeds the old single 100-record read.
    late_root = await tree.create_task(
        request_id="seed-late-root",
        task_parent_id=None,
        profile="manager",
        task=TaskSpec(goal="late root for move paging"),
        name="Late root",
        backend=None,
        caller=OP)
    late_mid = await tree.create_task(
        request_id="seed-late-mid",
        task_parent_id=late_root.id,
        profile="manager",
        task=TaskSpec(goal="intermediate manager"),
        name="Late mid",
        backend=None,
        caller=OP)
    wide = await tree.create_task(
        request_id="seed-wide",
        task_parent_id=late_mid.id,
        profile="manager",
        task=TaskSpec(goal="a manager with more children than one page"),
        name="Wide manager",
        backend=None,
        caller=OP)
    for i in range(1, 106):
      await tree.create_task(
          request_id=f"seed-wide-child-{i}",
          task_parent_id=wide.id,
          profile="worker",
          task=TaskSpec(goal=f"wide child {i:03d}"),
          name=f"Wide child {i:03d}",
          backend=None,
          caller=OP)

    # A three-level ops tree whose leaf has a FAILED run: the terminal Run
    # paints no sidebar activity, so the rows the readability scenario
    # measures carry their names and subtree counts alone.
    ops_root = await tree.create_task(
        request_id="seed-ops-root",
        task_parent_id=None,
        profile="manager",
        task=TaskSpec(goal="ops root"),
        name="Ops root",
        backend=None,
        caller=OP)
    ops_mid = await tree.create_task(
        request_id="seed-ops-mid",
        task_parent_id=ops_root.id,
        profile="manager",
        task=TaskSpec(goal="ops mid manager"),
        name="Ops mid",
        backend=None,
        caller=OP)
    failing = await tree.create_task(
        request_id="seed-failing",
        task_parent_id=ops_mid.id,
        profile="worker",
        task=TaskSpec(goal="a worker whose run failed"),
        name="Failing worker",
        backend=None,
        caller=OP)
    await tree.runs.register_run(
        RunRecord(
            id="run-fail-1",
            session_id=failing.id,
            kind="work",
            backend="fake-scripted",
            model="scripted-model",
            started_at=base + timedelta(minutes=500)))
    await tree.dispatch.finish_run(failing.id, "run-fail-1", outcome="failed")

    # Completion-evidence MANAGER: 120 successful manager_turn runs. A
    # manager never auto-completes (that is worker-only), so the task stays
    # open and manually completable, with early evidence beyond the first
    # desc page and beyond the old 100-record read.
    evidence = await tree.create_task(
        request_id="seed-evidence",
        task_parent_id=feature.id,
        profile="manager",
        task=TaskSpec(goal="carry enough runs to page the evidence picker"),
        name="Evidence manager",
        backend=None,
        caller=OP)
    for i in range(1, 121):
      run_id = f"run-e{i:04d}"
      await tree.runs.register_run(
          RunRecord(
              id=run_id,
              session_id=evidence.id,
              kind="manager_turn",
              backend="fake-scripted",
              model="scripted-model",
              started_at=base + timedelta(minutes=1000 + i)))
      await tree.dispatch.finish_run(evidence.id, run_id, outcome="success")

    # Dialog-binding pair: A is a run-less worker (open, completable in
    # principle, nothing auto-closes it), B is a childless manager (its
    # cancel succeeds — used for the late-response drop).
    bind_a = await tree.create_task(
        request_id="seed-bind-a",
        task_parent_id=root.id,
        profile="worker",
        task=TaskSpec(goal="dialog binding task A"),
        name="Bind task A",
        backend=None,
        caller=OP)
    bind_b = await tree.create_task(
        request_id="seed-bind-b",
        task_parent_id=None,
        profile="manager",
        task=TaskSpec(goal="dialog binding task B"),
        name="Bind task B",
        backend=None,
        caller=OP)

    # --- the live worker: a REAL process mid-Run --------------------------
    # The node's own Run marks it busy (thinking_since at the recorded
    # started_at), the collapsed parent's gear stands in for it, and
    # the events file grows on a real thread while the browser scenario
    # watches the transcript stream. The display backend differs from the
    # inherited metadata.backend, so a correct page shows the Run's backend.
    from src.runtime.control_events import build_control_event
    from src.runtime.runs import read_pid_stat

    # The live worker's own delegating manager: an otherwise idle parent,
    # so its collapsed row's stand-in shows the gear. (The root's own
    # agent-auth Run has no terminal fact and a stale identity, so its own
    # row paints nothing and the stand-in is what shows.)
    live_parent = await tree.create_task(
        request_id="seed-live-parent",
        task_parent_id=root.id,
        profile="manager",
        task=TaskSpec(goal="delegate the live worker"),
        name="Live rollout",
        backend=None,
        caller=OP)
    live = await tree.create_task(
        request_id="seed-live",
        task_parent_id=live_parent.id,
        profile="worker",
        task=TaskSpec(goal="watch this worker run live"),
        name="Live worker",
        backend=None,
        caller=OP)
    live_run_dir = tree.runs.run_dir(live.id, "run-live")
    # The delivered history: one successful work Run with the four evidence
    # refs, so the delivery close has real links to show.
    await tree.runs.register_run(
        RunRecord(
            id="run-live-done",
            session_id=live.id,
            kind="work",
            backend="scripted-live",
            model="scripted-model",
            started_at=base + timedelta(minutes=900),
            repo_path=str(home / "harness-repo"),
            base_branch="main",
            branch_name="task/live-delivered"))
    append_events_line(
        live_run_dir.parent / "run-live-done" / "events.jsonl", {
            "type": ET.USER,
            "content": "deliver the checked piece",
            "timestamp": (base + timedelta(minutes=900)).isoformat()
        })
    await tree.runs.record_launch(live.id, "run-live-done", pid=424100, pid_start="1-424100")
    await tree.runs.record_observation(
        live.id,
        "run-live-done",
        raw_log_ref=str(live_run_dir.parent / "run-live-done" / "raw.log"),
        events_ref=str(live_run_dir.parent / "run-live-done" / "events.jsonl"),
        result_ref=str(live_run_dir.parent / "run-live-done" / "raw.log"))
    # The delivered Run's terminal fact lands at the runs layer only: the
    # dispatch funnel's follow-up would evaluate automatic completion for
    # a successful worker work Run, closing (and so derived-archiving) the
    # very node the live scenario watches. The node under test stays open
    # with one delivered Run in its history.
    async with tree.control_lock:
      done_run = await tree.runs.record_finish_locked(live.id, "run-live-done", "success")
    await tree.runs.notify_liveness(live.id, done_run, launched=False)

    # The live Run: a real sleep process, recorded identity, growing events.
    live_proc = subprocess.Popen(["/bin/sleep", "240"])
    live_pid_start, _state = read_pid_stat(live_proc.pid)
    assert live_pid_start is not None, "read_pid_stat failed for the live process"
    live_events = live_run_dir / "events.jsonl"
    live_events.parent.mkdir(parents=True, exist_ok=True)
    append_events_line(
        live_events, {
            "type": ET.USER,
            "content": "watch this worker run live",
            "timestamp": datetime.now(timezone.utc).isoformat()
        })
    append_events_line(
        live_events, {
            "type": ET.SYSTEM,
            "subtype": ET.CONTEXT_READING,
            "context_reading":
                {
                    "context_tokens": 42000,
                    "context_full": 200000,
                    "context_compact_at": 160000,
                    "model": "scripted-model"
                },
            "timestamp": datetime.now(timezone.utc).isoformat()
        })
    await tree.runs.register_run(
        RunRecord(
            id="run-live",
            session_id=live.id,
            kind="work",
            backend="scripted-live",
            model="scripted-model",
            started_at=datetime.now(timezone.utc)))
    await tree.runs.record_observation(
        live.id, "run-live", repo_path=str(home / "harness-repo"), base_branch="main", branch_name="task/run-live")
    await tree.runs.record_launch(live.id, "run-live", pid=live_proc.pid, pid_start=live_pid_start)
    live_stop = threading.Event()
    live_counter = {"lines": 0}
    threading.Thread(target=grow_run_events, args=(live_events, live_stop, live_counter), daemon=True).start()

    # The parent's Delegated card: a new-style delegation whose event
    # carries the child session id.
    await tree.events.append(
        live_parent.id,
        build_control_event(
            ET.TASK_DELEGATED,
            actor="agent",
            source_session_id=live_parent.id,
            request_id="seed-delegate-live",
            thread_id="run-live",
            child_session_id=live.id,
            description="watch this worker run live",
            backend="scripted-live",
            model="scripted-model",
            **{
                ET.DELEGATE_INVOCATION:
                    {
                        "task_type": "implement",
                        "repo_path": str(home / "harness-repo"),
                        "base_branch": "main",
                        "task_spec_file": "task.md",
                        "reviewer_context_file": None,
                        "keep_worktree": False,
                        "backend": "scripted-live"
                    }
            }))

    # --- the legacy worker thread: the 4.1 thread view --------------------
    # A pre-task-tree delegation lives in the parent session's threads/
    # directory; the sidebar projects it as a read-only row and the thread
    # URL opens its transcript in the main chat.
    legacy = await tree.create_task(
        request_id="seed-legacy-operator",
        task_parent_id=None,
        profile="manager",
        task=None,
        name="Legacy operator",
        backend="fake-scripted",
        caller=OP)
    legacy_thread_id = "thread-legacy-1"
    thread_dir = home / "sessions" / legacy.id / "threads" / legacy_thread_id
    (thread_dir / "data").mkdir(parents=True)
    started = base + timedelta(minutes=800)
    thread_meta = ThreadMetadata(
        id=legacy_thread_id,
        session_id=legacy.id,
        description="Review: ## Goal",
        status="completed",
        started_at=started,
        completed_at=started + timedelta(minutes=9),
        backend="fake-scripted",
        pid=424050,
        pid_start="1-424050",
        exit_code=0)
    (thread_dir / "metadata.json").write_text(thread_meta.model_dump_json(indent=2), encoding="utf-8")
    thread_events = []
    for i, (kind, text) in enumerate([(ET.USER, "review the delivered diff"),
                                      (ET.ASSISTANT, "review verdict: approve")]):
      event = {"type": kind, "timestamp": (started + timedelta(minutes=i)).isoformat()}
      if kind == ET.USER:
        event["content"] = text
      else:
        event["message"] = {"content": [{"type": "text", "text": text}]}
      thread_events.append(event)
    thread_events.append({"type": "master_done", "timestamp": (started + timedelta(minutes=5)).isoformat()})
    (thread_dir / "data" / "events.jsonl").write_text(
        "".join(json.dumps(e) + "\n" for e in thread_events), encoding="utf-8")

    # --- the withheld launch and the legacy cron session (S24) -----------
    # A cancelled task's queued Run carries its durable run_launch_withheld
    # fact (one per run and reason) and one blocked report to its parent;
    # the transcript header reads "withheld · <reason>". A pre-plan legacy
    # cron session keeps parenting its firings' worker leaves.
    withhold_parent = await tree.create_task(
        request_id="seed-withhold-parent",
        task_parent_id=None,
        profile="manager",
        task=TaskSpec(goal="hold a cancelled worker"),
        name="Withhold parent",
        backend=None,
        caller=OP)
    withhold_worker = await tree.create_task(
        request_id="seed-withhold-worker",
        task_parent_id=withhold_parent.id,
        profile="worker",
        task=TaskSpec(goal="a run the cancel withheld"),
        name="Withheld worker",
        backend=None,
        caller=OP)
    # The cancel lands before the queued Run exists (a queued Run would
    # block the cancel); the later launch attempt is what the fact records.
    await tree.completion.cancel_task(
        withhold_worker.id, request_id="seed-withhold-cancel", reason="operator cancelled", caller=OP)
    await tree.runs.register_run(
        RunRecord(
            id="run-withheld",
            session_id=withhold_worker.id,
            kind="work",
            backend="fake-scripted",
            model="scripted-model"))
    withheld_reason = f"task {withhold_worker.id} is cancelled"
    withheld_event = build_control_event(
        ET.RUN_LAUNCH_WITHHELD,
        actor="system",
        source_session_id=withhold_worker.id,
        event_id="seed-withheld-fact",
        run_id="run-withheld",
        reason=withheld_reason)
    await tree.events.append(withhold_worker.id, withheld_event)
    from src.runtime.control_events import ACTOR_SYSTEM
    await tree.dispatch.deliver_child_report(
        withhold_worker.id,
        source_event=withheld_event,
        outcome="blocked",
        summary=withheld_reason,
        result_refs=["run:run-withheld"],
        recipient=withhold_parent.id,
        actor=ACTOR_SYSTEM)

    # --- the sidebar views' schedule fixtures (S25-S28) -------------------
    # Two manager nodes carry bound tasks (one enabled, one disabled) via
    # the cron.d session_id binding; one archived worker under the bound
    # node gives the Archive view its dimmed context ancestor; the feature
    # manager is starred for Later; one broken cron file feeds the
    # Workspace error badge. The host files point at prompt files under
    # the synthetic home and ride the same loader the production cron dir
    # uses; JSON bodies are valid YAML.
    bound_node = await tree.create_task(
        request_id="seed-bound-node",
        task_parent_id=None,
        profile="manager",
        task=TaskSpec(goal="fire on a schedule"),
        name="Bound nightly",
        backend=None,
        caller=OP)
    paused_node = await tree.create_task(
        request_id="seed-paused-node",
        task_parent_id=None,
        profile="manager",
        task=TaskSpec(goal="a paused schedule"),
        name="Paused nightly",
        backend=None,
        caller=OP)
    archived_child = await tree.create_task(
        request_id="seed-archived-child",
        task_parent_id=bound_node.id,
        profile="worker",
        task=TaskSpec(goal="a firing the user archived"),
        name="Archived firing",
        backend=None,
        caller=OP)
    await tree.runs.register_run(
        RunRecord(
            id="run-archived-child",
            session_id=archived_child.id,
            kind="work",
            backend="fake-scripted",
            model="scripted-model"),
        task_spec_text="firing spec")
    # The delivered run derives the archive: the worker leaves the active
    # lists and rides /api/sessions/archived under its still-active parent.
    await tree.dispatch.finish_run(archived_child.id, "run-archived-child", outcome="success")
    await lifecycle.star_session(feature.id)

    # --- the Threads view's chat-thread subtree (S25b) ------------------
    # One chat-origin session in the group named for its channel, with one
    # delegated child: Workspace lists neither, the Threads pill lists the
    # pair nested, and no group-header plus button renders there. The origin
    # is the first registered session-metadata ``*_origin`` field's model,
    # built with synthetic digit strings, so the fixture names no platform.
    origin_field, origin = first_registered_origin()
    chat_thread = await tree.create_task(
        request_id="seed-chat-thread",
        task_parent_id=None,
        profile="manager",
        task=None,
        name="Chat #general 2026",
        backend=None,
        group="Chat #general",
        slot_values={origin_field: origin},
        caller=OP)
    chat_thread_child = await tree.create_task(
        request_id="seed-chat-child",
        task_parent_id=chat_thread.id,
        profile="manager",
        task=TaskSpec(goal="the thread session's delegated child"),
        name="Chat thread child",
        backend=None,
        caller=OP)

    # --- the row-menu scenarios' missing row kinds (S30-S32) --------------
    # An archived ROOT manager (the archived root's Move-to-group menu) and
    # a named group's root (the group header's + and gear): the two row
    # kinds the tree above never produces, seeded through the same owner.
    archived_root = await tree.create_task(
        request_id="seed-archived-root",
        task_parent_id=None,
        profile="manager",
        task=TaskSpec(goal="an archived root manager"),
        name="Archived root",
        backend=None,
        caller=OP)
    await lifecycle.archive_session(archived_root.id)
    grouped_root = await tree.create_task(
        request_id="seed-grouped-root",
        task_parent_id=None,
        profile="manager",
        task=TaskSpec(goal="a root inside a named group"),
        name="Grouped root",
        backend=None,
        caller=OP,
        group="Alpha team")
    # The hover-scope scenario hovers one row of a named group and checks
    # its neighbours, so the group needs at least three roots.
    alpha_second = await tree.create_task(
        request_id="seed-alpha-second",
        task_parent_id=None,
        profile="manager",
        task=TaskSpec(goal="a root inside the named group"),
        name="Alpha second",
        backend=None,
        caller=OP,
        group="Alpha team")
    alpha_third = await tree.create_task(
        request_id="seed-alpha-third",
        task_parent_id=None,
        profile="manager",
        task=TaskSpec(goal="a root inside the named group"),
        name="Alpha third",
        backend=None,
        caller=OP,
        group="Alpha team")

    prompts_dir = home / "prompts"
    prompts_dir.mkdir(parents=True, exist_ok=True)
    cron_d = home / "config.d" / "cron.d"
    cron_d.mkdir(parents=True, exist_ok=True)
    for task_name, node_id, enabled, allow_failure in (("harness-daily", bound_node.id, True, False),
                                                       ("harness-paused", paused_node.id, False, True)):
      (prompts_dir / f"{task_name}.md").write_text(f"synthetic prompt for {task_name}\n", encoding="utf-8")
      (cron_d / f"{task_name}.yaml").write_text(
          json.dumps(
              {
                  "cron": "0 9 * * *",
                  "prompt_file": str(prompts_dir / f"{task_name}.md"),
                  "timezone": "America/Los_Angeles",
                  "enabled": enabled,
                  "allow_failure": allow_failure,
                  "session_id": node_id,
              },
              indent=2),
          encoding="utf-8")
    # The paused node's firing bookkeeping (the touch Last-line scenario):
    # one recorded fire with a failed status and a timestamp, through the
    # same owner entry the scheduler itself calls; the task's
    # allow_failure makes the row carry the "(review needed)" suffix, the
    # longest the Last line renders. The enabled bound node stays
    # unseeded, so its row keeps carrying no Last line.
    from src.infra.models import LastRunStatus
    await tree.update_slot_fields(
        paused_node.id,
        "cron",
        last_scheduled_run=(base + timedelta(minutes=1500)).isoformat(),
        last_run_status=LastRunStatus.FAILED)
    # The broken file: an inline prompt is a load error, so the loader
    # surfaces one error entry and the Workspace badge counts it.
    (cron_d / "harness-broken.yaml").write_text(
        json.dumps({
            "cron": "0 9 * * *",
            "prompt": "an inline prompt is a cron.d load error",
        }), encoding="utf-8")

    seed_memory_store(home)
    return {
        "root": root.id,
        "feature": feature.id,
        "worker1": worker1.id,
        "worker2": worker2.id,
        "long": long_worker.id,
        "latest_hash": latest_hash,
        "late_root": late_root.id,
        "late_mid": late_mid.id,
        "wide": wide.id,
        "ops_root": ops_root.id,
        "ops_mid": ops_mid.id,
        "failing": failing.id,
        "evidence": evidence.id,
        "bind_a": bind_a.id,
        "bind_b": bind_b.id,
        "live": live.id,
        "live_parent": live_parent.id,
        "legacy": legacy.id,
        "legacy_thread": legacy_thread_id,
        "withhold_parent": withhold_parent.id,
        "withhold_worker": withhold_worker.id,
        "bound_node": bound_node.id,
        "paused_node": paused_node.id,
        "archived_child": archived_child.id,
        "archived_root": archived_root.id,
        "grouped_root": grouped_root.id,
        "group_name": "Alpha team",
        "chat_thread": chat_thread.id,
        "chat_thread_child": chat_thread_child.id,
        "alpha_second": alpha_second.id,
        "alpha_third": alpha_third.id,
        "live_run": "run-live",
        "_live_handles":
            {
                "process": live_proc,
                "stop": live_stop,
                "counter": live_counter,
                "tree": tree,
                "lifecycle": lifecycle
            },
        **bulk
    }

  return await seed()


# ---------------------------------------------------------------------------
# Chrome launch and page wire-up (shared by the browser harnesses)
# ---------------------------------------------------------------------------

# The desktop-capture browser runs share one flag set: the 1440x900 capture
# viewport plus the throttling bans that keep the page fully active - a
# background-throttled timer or fetch would distort the live-update evidence.
# The blink-settings pair gives headless chrome the input profile the desktop
# viewport stands for - a hover-capable fine pointer, what a real operator's
# mouse presents; bare headless reports (hover: none), which would run the
# styles.css touch fallback and shadow the desktop hover reveal.
DESKTOP_CAPTURE_FLAGS = [
    "--no-first-run",
    "--no-default-browser-check",
    "--mute-audio",
    "--disable-background-networking",
    "--window-size=1440,900",
    "--remote-allow-origins=*",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-renderer-backgrounding",
    ("--blink-settings=primaryHoverType=2,availableHoverTypes=2,"
     "primaryPointerType=4,availablePointerTypes=4"),
]


def launch_chrome(chrome: str, profile: Path, debug_port: int, flags: list[str]) -> subprocess.Popen:
  """Start headless chrome with a CDP endpoint and a private profile; return the process.

    The caller owns the returned process: terminate and reap it when the run
    ends. *flags* carries the harness's own switches (window size, throttling
    bans); the launch sandwich around them - headless mode, the debug port,
    the profile, the start URL - is the one shared shape.
    """
  return subprocess.Popen(
      [
          chrome, "--headless=new", f"--remote-debugging-port={debug_port}", f"--user-data-dir={profile}", *flags,
          "about:blank"
      ],
      stdout=subprocess.DEVNULL,
      stderr=subprocess.PIPE)


def resolve_chrome(explicit: str | None, fail: Callable[[str], None]) -> str:
  """Resolve the chrome binary a harness drives: --chrome wins, then the two
    google-chrome installs. *fail* is the caller's own failure exit (the
    devtools_ws_url convention), so the refusal keeps the harness's prefix.
    """
  chrome = explicit or shutil.which("google-chrome") or shutil.which("google-chrome-stable")
  if not chrome:
    fail("google-chrome is not installed; install it or pass --chrome (no fake output)")
  return chrome


async def devtools_ws_url(chrome_proc: subprocess.Popen, timeout_s: float, fail: Callable[[str], None]) -> str:
  """Read chrome's stderr until the DevTools websocket endpoint appears.

    *fail* is the caller's own failure exit, so each harness keeps its
    message prefix and exit code. The captured stderr tail rides the failure
    message: a chrome that exits before announcing its endpoint explains
    itself there.
    """
  stderr_lines: list[str] = []
  ws_url = None
  deadline = time.monotonic() + timeout_s
  while time.monotonic() < deadline and ws_url is None:
    line = chrome_proc.stderr.readline().decode(errors="replace")
    if not line:
      break  # EOF: chrome exited, so no endpoint is coming
    stderr_lines.append(line)
    if "DevTools listening on ws://" in line:
      ws_url = line.strip().split()[-1]
  if ws_url is None:
    fail("chrome devtools endpoint did not come up: " + "".join(stderr_lines[-5:]))
  return ws_url


async def connect_cdp(ws_url: str) -> CDP:
  """Open the browser websocket, wrap it in CDP, and settle before the first send."""
  import websockets

  ws = await websockets.connect(ws_url, max_size=50 * 1024 * 1024)
  cdp = CDP(ws)
  await asyncio.sleep(0.3)
  return cdp


async def open_cdp_page(cdp: CDP, domains: tuple[str, ...]) -> tuple[str, str]:
  """Create one blank page target, attach flattened, enable *domains*.

    Returns (session_id, target_id): the session id drives the page's CDP
    calls; the target id is what closes the page again (Target.closeTarget).
    """
  target = await cdp.send("Target.createTarget", {"url": "about:blank"})
  attached = await cdp.send("Target.attachToTarget", {"targetId": target["targetId"], "flatten": True})
  session_id = attached["sessionId"]
  for domain in domains:
    await cdp.send(f"{domain}.enable", session_id=session_id)
  return session_id, target["targetId"]


# ---------------------------------------------------------------------------
# Browser scenarios
# ---------------------------------------------------------------------------


class Results:
  """The browser evidence file: per-scenario rows, console errors, and the run's provenance.

    ``record`` accumulates the scenario rows in call order; ``save`` writes the
    run's one JSON evidence file. ``entry_point`` and ``invocation`` are
    optional provenance keys: a harness whose results file name already says
    what ran omits both.
    """

  def __init__(
      self,
      evidence_dir: Path,
      commit: str,
      *,
      browser: str,
      results_name: str,
      entry_point: str | None = None,
      invocation: list[str] | None = None) -> None:
    self.evidence_dir = evidence_dir
    self.commit = commit
    self.browser = browser
    self.results_name = results_name
    self.entry_point = entry_point
    self.invocation = invocation
    self.scenarios: list[dict] = []
    self.console_errors: list[str] = []

  def record(self, name: str, ok: bool, detail: str, screenshot: str | None) -> None:
    self.scenarios.append({"name": name, "ok": ok, "detail": detail, "screenshot": screenshot})
    log(f"  [{'PASS' if ok else 'FAIL'}] {name}: {detail}")

  def save(self) -> None:
    payload: dict = {
        "tested_commit": self.commit,
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "browser": self.browser,
    }
    if self.entry_point is not None:
      payload["entry_point"] = self.entry_point
    if self.invocation is not None:
      payload["invocation"] = self.invocation
    payload["scenarios"] = self.scenarios
    payload["console_errors"] = self.console_errors
    out = self.evidence_dir / self.results_name
    out.write_text(json.dumps(payload, indent=2), encoding="utf-8")
    log(f"results written to {out}")


def api_request(
    base: str,
    key: str,
    method: str,
    path: str,
    body: dict | None = None,
    *,
    timeout: float,
    token: str | None = None) -> tuple[int, dict]:
  """One real HTTP API call from a SEPARATE authenticated client.

    This is the cross-client creator of the creation-visibility scenarios: the
    operator access key (or a scoped agent run token) rides the Authorization
    header; the browser under observation never performs the call. ``timeout``
    bounds one call. A 2xx body must be JSON; an error response's empty body
    parses as {}.
    """
  headers = {"Authorization": "Bearer " + (token or key)}
  data = None
  if body is not None:
    headers["Content-Type"] = "application/json"
    data = json.dumps(body).encode("utf-8")
  req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
  try:
    with urllib.request.urlopen(req, timeout=timeout) as resp:
      return resp.status, json.loads(resp.read().decode())
  except urllib.error.HTTPError as e:
    return e.code, json.loads(e.read().decode() or "{}")


async def evaluate(cdp: CDP, session_id: str, expression: str) -> object:
  res = await cdp.send(
      "Runtime.evaluate", {
          "expression": expression,
          "returnByValue": True,
          "awaitPromise": True,
      },
      session_id=session_id)
  if res.get("exceptionDetails"):
    detail = res["exceptionDetails"]
    # Full details plus the expression head: a bare description hides whether
    # the failure is a parse error of the sent text or a throw inside it.
    raise RuntimeError(
        "page evaluate failed: " + json.dumps(detail)[:600] + " | expression head: " +
        expression[:160].replace("\n", " "))
  return res.get("result", {}).get("value")


async def screenshot(cdp: CDP, session_id: str, results: Results, name: str) -> str:
  res = await cdp.send("Page.captureScreenshot", {"format": "png"}, session_id=session_id)
  path = results.evidence_dir / f"{name}.png"
  path.write_bytes(base64.b64decode(res["data"]))
  return path.name


async def expand_to(cdp: CDP, session_id: str, node_ids: list[str]) -> None:
  """Expand the managers above *node_ids* through the sidebar's own API.

    expandTreeNode is idempotent — an already-open level stays open (toggle
    would collapse it)."""
  for node_id in node_ids:
    await wait_for(
        cdp,
        session_id,
        f"!!document.getElementById('session-{node_id}')",
        timeout=8,
        label=f"row visible for {node_id}")
    await evaluate(cdp, session_id, f"Sidebar.expandTreeNode('{node_id}')")
    await wait_for(
        cdp,
        session_id,
        '(() => { const el = document.querySelector('
        "\"[data-tree-children='" + node_id + "']\");"
        " return el && !el.classList.contains('hidden'); })()",
        timeout=8,
        label=f"children container for {node_id}")


async def reveal_row(cdp: CDP, session_id: str, node_id: str) -> None:
  """Bring one (possibly nested) row on screen for the screenshot evidence.

    A nested row sits inside its root's subtree wrapper, which the group's
    5-row preview hides with the root when the root falls past the limit (only
    the active row itself is exempt): Show all that group, then scroll the row
    into view. Expansion is the caller's (expand_to)."""
  await evaluate(
      cdp, session_id, f"""
        (() => {{
          const row = document.getElementById('session-{node_id}');
          let el = row;
          // The "(No group)" group's key is the empty string: test presence.
          while (el && !(el.dataset && 'sessionGroupLimitExtra' in el.dataset)) el = el.parentElement;
          if (el && el.classList.contains('hidden')) toggleSessionGroupLimit(el.dataset.sessionGroupLimitExtra);
          row.scrollIntoView({{block: 'center'}});
        }})()
    """)
  await wait_for(
      cdp,
      session_id,
      f"document.getElementById('session-{node_id}').offsetParent !== null",
      timeout=8,
      label=f"row on screen for {node_id}")


async def click_selector_center(cdp: CDP, session_id: str, selector: str, label: str) -> dict:
  """One real pointer click (move, press, release) on the element *selector*
    names; the move first, because a row's desktop buttons only take pointers
    while the row is hovered. The element's box is the click's ground truth."""
  box = await evaluate(
      cdp, session_id, f"""
        (() => {{
          const el = document.querySelector({json.dumps(selector)});
          if (!el) return null;
          const r = el.getBoundingClientRect();
          if (r.width === 0 && r.height === 0) return null;
          return {{x: r.x + r.width / 2, y: r.y + r.height / 2, w: r.width, h: r.height}};
        }})()
    """)
  assert_true(box is not None, f"{label}: the click target renders: {selector}")
  await cdp.send(
      "Input.dispatchMouseEvent", {
          "type": "mouseMoved",
          "x": box["x"],
          "y": box["y"]
      }, session_id=session_id)
  # The move first, then check the point really hits the target: a covered
  # or pointer-transparent target fails here with the covering element's
  # identity instead of as a silent miss downstream.
  hit = await evaluate(
      cdp, session_id, f"""
        (() => {{
          const el = document.querySelector({json.dumps(selector)});
          const at = document.elementFromPoint({box['x']}, {box['y']});
          if (!el) return {{ok: false, at: 'target gone'}};
          if (el.contains(at) || at === el) return {{ok: true}};
          return {{ok: false,
                   at: at ? (at.tagName + '#' + at.id + ' .' + at.className
                             + ' title=' + (at.title || '')) : 'nothing'}};
        }})()
    """)
  assert_true(
      hit and hit.get("ok"), f"{label}: the click point misses the target "
      f"({box['x']:.0f},{box['y']:.0f}), hits: {hit and hit.get('at')}")
  for kind in ("mousePressed", "mouseReleased"):
    await cdp.send(
        "Input.dispatchMouseEvent", {
            "type": kind,
            "x": box["x"],
            "y": box["y"],
            "button": "left",
            "clickCount": 1
        },
        session_id=session_id)
  return box


async def press_escape(cdp: CDP, session_id: str) -> None:
  """One real Escape keydown+keyup through the browser's input pipeline."""
  for kind in ("keyDown", "keyUp"):
    await cdp.send(
        "Input.dispatchKeyEvent", {
            "type": kind,
            "key": "Escape",
            "code": "Escape",
            "windowsVirtualKeyCode": 27,
            "nativeVirtualKeyCode": 27
        },
        session_id=session_id)


def menu_items_as_text(items: list[dict]) -> list[str]:
  """The open menu's items in the expected lists' notation: 'sep' for a
    separator, ' (danger)' appended to a red item's label."""
  return [("sep" if item["sep"] else (item["label"] or "") + (" (danger)" if item["danger"] else "")) for item in items]


async def open_row_menu(cdp: CDP, session_id: str, selector: str, label: str) -> list[str]:
  """Click the Settings gear *selector* names and read the open .row-menu's
    items; exactly one menu may exist while it is open."""
  before = await evaluate(cdp, session_id, "document.querySelectorAll('.row-menu').length")
  assert_true(before == 0, f"{label}: no menu is open before the click ({before})")
  await click_selector_center(cdp, session_id, selector, label)
  await wait_for(
      cdp,
      session_id,
      "document.querySelectorAll('.row-menu').length === 1",
      timeout=6,
      label=f"{label}: the Settings menu opens")
  items = await evaluate(
      cdp, session_id, """
        [...document.querySelectorAll('.row-menu > *')].map(el => ({
          sep: el.classList.contains('row-menu-sep'),
          label: el.classList.contains('row-menu-item') ? el.textContent : null,
          danger: el.classList.contains('row-menu-item-danger'),
        }))
    """)
  return menu_items_as_text(items)


async def assert_menu_item_heights(cdp: CDP, session_id: str, label: str) -> float:
  """Every item of the open menu is a 44px touch row; return the minimum."""
  min_h = await evaluate(
      cdp, session_id, """
        Math.min(...[...document.querySelectorAll('.row-menu .row-menu-item')]
          .map(b => b.getBoundingClientRect().height))
    """)
  assert_true(min_h >= 44, f"{label}: every menu item is at least 44px tall (min {min_h})")
  return min_h


async def close_row_menu(cdp: CDP, session_id: str, label: str) -> None:
  """Escape closes the open menu and leaves no .row-menu behind."""
  await press_escape(cdp, session_id)
  await wait_for(
      cdp,
      session_id,
      "document.querySelectorAll('.row-menu').length === 0",
      timeout=6,
      label=f"{label}: Escape closes the menu")


def assert_menu_matches(actual: list[str], expected: list[str], label: str) -> None:
  assert_true(actual == expected, f"{label}: the menu reads {actual!r}, expected {expected!r}")


DIAGNOSTIC_SNAPSHOT = """
    JSON.stringify({
      rows: document.querySelectorAll('#session-list .session-name').length,
      rowIds: [...document.querySelectorAll('#session-list a[id^="session-"]')]
        .map(el => el.id.replace('session-', '')).slice(0, 3),
      filter: currentFilter,
      activeTabBtn: [...document.querySelectorAll('.tab-btn')].filter(b => !b.classList.contains('hidden')).map(b => b.id),
      expanded: JSON.stringify([...document.querySelectorAll('#session-list [data-tree-toggle]')]
        .filter(el => el.getAttribute('aria-expanded') === 'true').map(el => el.dataset.treeToggle)).slice(0, 200),
      sessionId: typeof SESSION_ID !== 'undefined' ? SESSION_ID : null,
      treeEvents: (window.__treeEvents || 0) + '/' + (window.__wsMsgs || 0) + 'msgs/' + (window.__wsTotal || 0) + 'socks',
      errs: typeof window.__errs === 'object' ? window.__errs.slice(0, 4) : 'n/a',
      wsLog: (window.__wsLog || []).slice(-3),
      rowNames: [...document.querySelectorAll('#session-list .session-name')].map((el) => el.textContent).slice(0, 5),
      headerName: (document.getElementById('header-session-name') || {}).textContent,
      thinking: !!(document.getElementById('thinking') && !document.getElementById('thinking').classList.contains('hidden')),
      messageCount: document.querySelectorAll('#messages [data-message-id]').length,
    })
"""


def assert_true(cond: bool, message: str) -> None:
  if not cond:
    raise AssertionError(message)


# The readiness predicate the shared post-navigation waits poll: the session
# list has rendered at least one name. Scenario-specific waits (a filter's
# rows, the bound rows) spell out their own predicates instead.
SESSION_LIST_READY_JS = "document.querySelectorAll('#session-list .session-name').length >= 1"


async def wait_for(
    cdp: CDP, session_id: str, expression: str, timeout: float = 10.0, label: str | None = None) -> object:
  """Poll a page expression until truthy; a timeout is an explicit failure."""
  started = time.monotonic()
  deadline = started + timeout
  while time.monotonic() < deadline:
    try:
      value = await asyncio.wait_for(
          evaluate(cdp, session_id, expression), timeout=min(10.0, max(1.0, deadline - time.monotonic())))
    except (RuntimeError, asyncio.TimeoutError) as exc:
      # A mid-navigation evaluate can race the page swap, and headless
      # renderers occasionally stall a single CDP evaluate; retry until
      # the deadline instead of failing the scenario on a transient.
      msg = str(exc)
      head = msg[msg.find("expression head:"):] if "expression head:" in msg else msg[-200:]
      log(f"    transient during wait ({head[:260]}); retrying")
      await asyncio.sleep(0.3)
      continue
    if value:
      if time.monotonic() - started > 2.0:
        log(f"    step slow ({time.monotonic() - started:.1f}s): {(label or expression)[:80]}")
      return value
    await asyncio.sleep(0.2)
  diag = ""
  try:
    diag = str(await evaluate(cdp, session_id, DIAGNOSTIC_SNAPSHOT))
  except Exception:
    diag = "<diag failed>"
  raise AssertionError(f"timeout waiting for {label or expression[:120]}\npage state: {diag[:1200]}")


async def run_harness(args: argparse.Namespace) -> None:
  chrome = resolve_chrome(args.chrome, fail)

  evidence_dir = Path(args.evidence_dir)
  commit = open_evidence_dir(evidence_dir)
  results = Results(
      evidence_dir, commit, browser="google-chrome headless (CDP)", results_name="session_tree_browser_results.json")

  with tempfile.TemporaryDirectory(prefix="charliebot-browser-harness-") as tmp:
    tmp_path = Path(tmp)
    home = tmp_path / "charliebot-home"
    home.mkdir()
    server_port = pick_free_port()
    config = {
        "server": {
            "port": server_port,
            "host": "127.0.0.1"
        },
        "backends":
            {
                "options":
                    [
                        {
                            "id": "fake-scripted",
                            "label": "Scripted (never launches)",
                            "type": "cc-claude",
                            "model": "scripted-model",
                        }, {
                            "id": "scripted-live",
                            "label": "Scripted Live Runner",
                            "type": "cc-claude",
                            "model": "scripted-model",
                        }
                    ],
                "preference": ["fake-scripted"]
            },
        "paths": {
            "worktree_dir": str(home / "worktrees")
        },
    }
    (home / "config.yaml").write_text(json.dumps(config, indent=2), encoding="utf-8")
    access_key = mint_access_key("harness-operator-key-")
    write_credentials_yaml(home, access_key)
    # Clear inherited production credentials from this process env.
    for var in ("CHARLIEBOT_ACCESS_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"):
      os.environ.pop(var, None)
    os.environ["CHARLIEBOT_HOME"] = str(home)

    ids = await seed_scenario(home)

    # Isolated server: the real app, lifespan disabled.
    import uvicorn

    from server import app as server_app

    server_config = uvicorn.Config(server_app, host="127.0.0.1", port=server_port, log_level="error", lifespan="off")
    server = uvicorn.Server(server_config)
    serve_task = asyncio.get_running_loop().create_task(server.serve())
    deadline = time.monotonic() + 30
    while not server.started:
      if serve_task.done():
        fail(f"isolated server failed to start: {serve_task.exception()!r}")
      if time.monotonic() > deadline:
        fail("isolated server did not start within 30s")
      await asyncio.sleep(0.05)
    log(f"isolated server on 127.0.0.1:{server_port} (lifespan off)")

    # Private browser profile.
    profile = tmp_path / "chrome-profile"
    profile.mkdir()
    debug_port = pick_free_port()
    chrome_proc = launch_chrome(chrome, profile, debug_port, DESKTOP_CAPTURE_FLAGS)
    try:
      ws_url = await devtools_ws_url(chrome_proc, 20, fail)
      cdp = await connect_cdp(ws_url)
      session_id, _target_id = await open_cdp_page(cdp, ("Page", "Runtime", "Network"))
      # Isolation guard, injected before any app script: the browser
      # terminal tab attaches to the HOST-GLOBAL tmux session. The
      # harness must never attach to (or create) it — /ws/terminal is
      # answered with an immediately-closed socket; every other app
      # websocket passes through untouched.
      guard_source = f"""
                (function () {{
                  try {{ localStorage.setItem('charliebot_access_key', {json.dumps(access_key)}); }} catch (e) {{}}
                  window.__treeEvents = 0;
                  window.__errs = [];
                  window.addEventListener('error', (e) => window.__errs.push(String(e.message).slice(0, 200)));
                  window.addEventListener('unhandledrejection', (e) => window.__errs.push('rej: ' + String(e.reason).slice(0, 200)));
                  const origConsoleError = console.error;
                  console.error = function () {{
                    window.__errs.push([...arguments].map((a) => String(a && a.message ? a.message : a)).join(' ').slice(0, 200));
                    origConsoleError.apply(console, arguments);
                  }};
                  const RealWebSocket = window.WebSocket;
                  function GuardedWebSocket(url, protocols) {{
                    const u = String(url);
                    if (u.includes('/ws/terminal')) {{
                      const fake = {{
                        readyState: 3, CLOSED: 3, send() {{}}, close() {{}},
                        addEventListener() {{}}, removeEventListener() {{}},
                        onopen: null, onmessage: null, onclose: null, onerror: null,
                      }};
                      setTimeout(() => {{ if (fake.onclose) fake.onclose({{type: 'close'}}); }}, 0);
                      return fake;
                    }}
                    const sock = protocols !== undefined
                      ? new RealWebSocket(url, protocols)
                      : new RealWebSocket(url);
                    window.__wsTotal = (window.__wsTotal || 0) + 1;
                    sock.addEventListener('message', (m) => {{
                      try {{
                        window.__wsMsgs = (window.__wsMsgs || 0) + 1;
                        const d = String(m.data);
                        if (d.includes('task_tree_changed')) {{
                          window.__treeEvents += 1;
                          window.__wsLog = (window.__wsLog || []);
                          window.__wsLog.push(d.slice(0, 140) + ' @sock' + (window.__wsTotal));
                        }}
                      }} catch (e) {{}}
                    }});
                    return sock;
                  }}
                  GuardedWebSocket.prototype = RealWebSocket.prototype;
                  GuardedWebSocket.OPEN = RealWebSocket.OPEN;
                  GuardedWebSocket.CONNECTING = RealWebSocket.CONNECTING;
                  GuardedWebSocket.CLOSING = RealWebSocket.CLOSING;
                  GuardedWebSocket.CLOSED = RealWebSocket.CLOSED;
                  window.WebSocket = GuardedWebSocket;
                }})();
            """
      await cdp.send("Page.addScriptToEvaluateOnNewDocument", {"source": guard_source}, session_id=session_id)
      await cdp.send(
          "Network.setCookie", {
              "name": "charliebot_access_key",
              "value": access_key,
              "url": f"http://127.0.0.1:{server_port}/",
          },
          session_id=session_id)
      await cdp.send(
          "Emulation.setDeviceMetricsOverride", {
              "width": 1440,
              "height": 900,
              "deviceScaleFactor": 1,
              "mobile": False,
          },
          session_id=session_id)
      base = f"http://127.0.0.1:{server_port}"

      # ---- S1: desktop load; the session tree is the primary navigation --
      try:
        await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['root']}"}, session_id=session_id)
        await wait_for(cdp, session_id, SESSION_LIST_READY_JS)
        await expand_to(cdp, session_id, [ids["root"], ids["feature"]])
        names = await evaluate(
            cdp, session_id, """
                    [...document.querySelectorAll('#session-list .session-name')].map(el => el.textContent)
                """)
        assert_true(
            "Program rollout" in names and "Feature alpha" in names and "Worker one" in names and "Worker two" in names,
            f"the task nodes render nested in the sidebar: {names}")
        active = await evaluate(cdp, session_id, "SESSION_ID")
        assert_true(active == ids["root"], "the deep link opened the root manager's chat")
        shot = await screenshot(cdp, session_id, results, "s1_desktop_tree")
        results.record(
            "desktop tree primary navigation",
            ok=True,
            detail="nested task rows render in the sidebar; the deep link opens the manager chat",
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s1_desktop_tree_FAILED")
        results.record("desktop tree primary navigation", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S9: an out-of-band change repaints the open tree in place ----
      try:
        await expand_to(cdp, session_id, [ids["root"], ids["feature"]])
        cdp.drain_list_fetches()
        # Rename the feature out of band (the server broadcasts a tree
        # change); the open page must refresh the affected row without
        # switching session, with bounded list work.
        import urllib.request as _u
        req = _u.Request(
            f"{base}/api/sessions/{ids['feature']}",
            data=json.dumps({
                "name": "Feature alpha renamed"
            }).encode(),
            headers={
                "Authorization": f"Bearer {access_key}",
                "Content-Type": "application/json"
            },
            method="PATCH")
        with await asyncio.to_thread(_u.urlopen, req, timeout=10) as resp:
          assert resp.status == 200
        await wait_for(
            cdp,
            session_id,
            """
                    [...document.querySelectorAll('#session-list .session-name')]
                      .some(el => el.textContent === 'Feature alpha renamed')
                """,
            timeout=8)
        fetches = cdp.drain_list_fetches()
        assert_true(1 <= len(fetches) <= 4, f"bounded list work for the change ({len(fetches)} list fetches)")
        active_ok = await evaluate(cdp, session_id, "SESSION_ID")
        assert_true(active_ok == ids["root"], "an update to another node never switches the active session")
        shot = await screenshot(cdp, session_id, results, "s9_live_update")
        results.record(
            "live tree change repaints the open sidebar",
            ok=True,
            detail="row refreshed in place; bounded list fetches for the change; session unchanged",
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s9_live_update_FAILED")
        results.record("live tree change repaints the open sidebar", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S12: creation from a separate client reaches the observer ----
      try:
        log("  s12: cross-client root creation")
        await evaluate(cdp, session_id, f"switchSession('{ids['feature']}')")
        await expand_to(cdp, session_id, [ids["root"], ids["feature"]])
        cdp.drain_list_fetches()
        active_before = await evaluate(cdp, session_id, "SESSION_ID")
        tree_events_before = await evaluate(cdp, session_id, "window.__treeEvents || 0")
        status, meta = await asyncio.to_thread(
            api_request,
            base,
            access_key,
            "POST",
            "/api/sessions/", {
                "request_id": "harness-x-root-1",
                "profile": "manager",
                "name": "Remote root",
                "task": {
                    "goal": "created outside the browser",
                    "acceptance": [],
                    "context_refs": []
                }
            },
            timeout=15.0)
        assert_true(status == 200, f"the separate client's create succeeded ({status}: {meta})")
        await wait_for(
            cdp,
            session_id,
            """
                    [...document.querySelectorAll('#session-list .session-name')]
                      .some(el => el.textContent === 'Remote root')
                """,
            timeout=10,
            label="s12 remote root row appeared from the notification alone")
        fetches = cdp.drain_list_fetches()
        assert_true(1 <= len(fetches) <= 4, f"bounded list work for the creation ({len(fetches)} list fetches)")
        active_after = await evaluate(cdp, session_id, "SESSION_ID")
        assert_true(active_after == active_before, "the creation never switches the active session")
        tree_events_after = await evaluate(cdp, session_id, "window.__treeEvents || 0")
        assert_true(
            tree_events_after > tree_events_before,
            "the creation rode a real server-originated task_tree_changed event")
        names = await evaluate(
            cdp, session_id, """
                    [...document.querySelectorAll('#session-list .session-name')].map(el => el.textContent)
                """)
        assert_true(names.count("Remote root") == 1, "exactly one row for the new root")
        shot = await screenshot(cdp, session_id, results, "s12_cross_client_creation")
        results.record(
            "creation from a separate client reaches the observer",
            ok=True,
            detail="row appeared from the notification alone; bounded list work; session unchanged",
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s12_creation_FAILED")
        results.record(
            "creation from a separate client reaches the observer", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S13: deeper-than-one-level creation and collapsed levels -----
      try:
        log("  s13: deep creation")
        await expand_to(cdp, session_id, [ids["root"], ids["feature"]])
        status, mid = await asyncio.to_thread(
            api_request,
            base,
            access_key,
            "POST",
            "/api/sessions/", {
                "request_id": "harness-x-mid-1",
                "task_parent_id": ids["feature"],
                "profile": "manager",
                "name": "Remote mid",
                "task": {
                    "goal": "mid manager",
                    "acceptance": [],
                    "context_refs": []
                }
            },
            timeout=15.0)
        assert_true(status == 200, f"nested manager create succeeded ({status})")
        await wait_for(
            cdp,
            session_id,
            """
                    [...document.querySelectorAll('#session-list .session-name')]
                      .some(el => el.textContent === 'Remote mid')
                """,
            timeout=10,
            label="s13 remote mid row under the expanded feature")
        # A worker under the new mid: the mid stays collapsed, so the
        # leaf is not painted — the collapsed row carries the count.
        status, _leaf = await asyncio.to_thread(
            api_request,
            base,
            access_key,
            "POST",
            "/api/sessions/", {
                "request_id": "harness-x-leaf-1",
                "task_parent_id": mid["id"],
                "profile": "worker",
                "name": "Remote leaf",
                "task": {
                    "goal": "idle leaf worker",
                    "acceptance": [],
                    "context_refs": []
                }
            },
            timeout=15.0)
        assert_true(status == 200, f"deep leaf create succeeded ({status})")
        await wait_for(
            cdp,
            session_id,
            f"""
                    (() => {{
                      const row = document.getElementById('session-{mid["id"]}');
                      const chev = row && row.querySelector('[data-tree-toggle="{mid["id"]}"]');
                      return chev && (chev.getAttribute('title') || '').includes('1 child task');
                    }})()
                """,
            timeout=10,
            label="s13 mid row gained the child count while collapsed")
        collapsed_ok = await evaluate(
            cdp, session_id, f"""
                    (() => {{
                      const el = document.querySelector('[data-tree-children="{mid["id"]}"]');
                      return el && el.classList.contains('hidden');
                    }})()
                """)
        assert_true(collapsed_ok, "the collapsed level stays collapsed")
        await evaluate(cdp, session_id, f"Sidebar.expandTreeNode('{mid['id']}')")
        await wait_for(
            cdp,
            session_id,
            """
                    [...document.querySelectorAll('#session-list .session-name')]
                      .some(el => el.textContent === 'Remote leaf')
                """,
            timeout=8,
            label="s13 leaf visible after expanding the mid")
        shot = await screenshot(cdp, session_id, results, "s13_deep_creation")
        results.record(
            "deeper-than-one-level creation from a separate client",
            ok=True,
            detail=
            "mid appeared under the expanded parent; collapsed mid gained the count; expansion reveals the idle leaf",
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s13_deep_creation_FAILED")
        results.record(
            "deeper-than-one-level creation from a separate client", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S14: agent-scoped creation reaches the observer --------------
      try:
        log("  s14: scoped-agent creation")
        from src.runtime.run_token import RunTokenClaims, sign_run_token
        agent_token = sign_run_token(
            RunTokenClaims(session_id=ids["root"], run_id="agent-auth-run", agent="harness-agent"), access_key)
        status, worker = await asyncio.to_thread(
            api_request,
            base,
            access_key,
            "POST",
            "/api/sessions/", {
                "request_id": "harness-agent-w1",
                "task_parent_id": ids["root"],
                "profile": "worker",
                "name": "Agent worker",
                "task": {
                    "goal": "created by a scoped agent",
                    "acceptance": [],
                    "context_refs": []
                }
            },
            token=agent_token,
            timeout=15.0)
        assert_true(status == 200, f"agent-scoped create succeeded ({status}: {worker})")
        await wait_for(
            cdp,
            session_id,
            """
                    [...document.querySelectorAll('#session-list .session-name')]
                      .some(el => el.textContent === 'Agent worker')
                """,
            timeout=10,
            label="s14 agent-created worker appeared")
        shot = await screenshot(cdp, session_id, results, "s14_agent_creation")
        results.record(
            "agent-scoped creation reaches a connected observer",
            ok=True,
            detail="run-token agent created a worker under its manager; the observer saw it live",
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s14_agent_creation_FAILED")
        results.record(
            "agent-scoped creation reaches a connected observer", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S20: the name stays readable at any depth ---------------------
      try:
        log("  s20: name readability at depth")
        await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['ops_root']}"}, session_id=session_id)
        await wait_for(cdp, session_id, SESSION_LIST_READY_JS)
        await expand_to(cdp, session_id, [ids["ops_root"]])

        async def assert_readable(node_id: str, label: str) -> dict:
          geo = await evaluate(
              cdp, session_id, f"""
                        (() => {{
                          const row = document.getElementById('session-{node_id}');
                          if (!row) return null;
                          const r = row.getBoundingClientRect();
                          const name = row.querySelector('.session-name');
                          const n = name ? name.getBoundingClientRect() : null;
                          return {{
                            row: {{x: r.x, w: r.width}},
                            name: n ? {{x: n.x, w: n.width, h: n.height}} : null,
                            nameText: name ? name.textContent : null,
                            clipped: name ? (name.scrollWidth - name.clientWidth) : null,
                            newChild: !!row.querySelector('button[title="New child session"]')
                          }};
                        }})()
                    """)
          assert_true(geo, f"{label}: the row renders")
          assert_true(geo["nameText"], f"{label}: the name text renders")
          assert_true(
              geo["name"] and geo["name"]["w"] > 0 and geo["name"]["x"] >= geo["row"]["x"],
              f"{label}: the name renders inside the row: {geo}")
          assert_true(geo["clipped"] is not None, f"{label}: the name truncation is measurable: {geo}")
          return geo

        ops_geo = await assert_readable(ids["ops_root"], "ops root")
        mid_geo = await assert_readable(ids["ops_mid"], "nested ops manager")
        assert_true(
            "Ops root" == ops_geo["nameText"] and "Ops mid" == mid_geo["nameText"],
            "both measured rows carry their full task names")
        assert_true(
            mid_geo["name"]["x"] > ops_geo["name"]["x"] + 10,
            f"the nested row indents behind its parent ({ops_geo['name']['x']} -> {mid_geo['name']['x']})")
        assert_true(mid_geo["newChild"], "the nested manager keeps its New child control")
        spill = await evaluate(
            cdp, session_id, "document.documentElement.scrollWidth - document.documentElement.clientWidth")
        assert_true(spill <= 1, f"no horizontal spill at depth ({spill}px)")
        shot = await screenshot(cdp, session_id, results, "s20_desktop_readability")
        results.record(
            "tree names stay readable at any depth",
            ok=True,
            detail="full names render, truncate rather than spill, indent per level, controls kept",
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s20_readability_FAILED")
        results.record("tree names stay readable at any depth", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S21: the live worker's transcript in the main chat ------------
      # A real local process is the Run: its pid is recorded through the
      # launch owner and its events file grows on a real thread during
      # the scenario. Every phase asserts what a browser operator SEES.
      live = ids["live"]
      live_run = ids["live_run"]
      handles = ids["_live_handles"]
      from src.infra import event_types as ET
      from src.runtime.control_events import build_control_event
      try:
        log("  s21: live worker transcript")
        # (a) parent page: worker spinner, the parent's gear in both
        # states, Delegated card live line + link — all following the
        # status poll. A parent row's icon reads facts only, so the
        # gear survives expansion and collapse unchanged.
        live_parent = ids["live_parent"]
        parent_icons = f"""
                    ['spinner', 'worker-indicator', 'waiting-indicator', 'subtree-unread']
                      .filter(k => !document.getElementById(k + '-{live_parent}').classList.contains('hidden'))
                """
        await cdp.send("Page.navigate", {"url": f"{base}/?session={live_parent}"}, session_id=session_id)
        await wait_for(cdp, session_id, SESSION_LIST_READY_JS)
        await expand_to(cdp, session_id, [ids["root"], live_parent])
        await reveal_row(cdp, session_id, live)
        await wait_for(
            cdp,
            session_id,
            f"document.getElementById('spinner-{live}') && !document.getElementById('spinner-{live}').classList.contains('hidden')",
            timeout=12,
            label="worker row spinner while its Run is live")
        shown_expanded = await evaluate(cdp, session_id, parent_icons)
        assert_true(
            shown_expanded == ["worker-indicator"],
            f"the expanded parent keeps the gear for its running worker ({shown_expanded})")
        await wait_for(
            cdp,
            session_id,
            """
                    (() => {
                      const el = document.querySelector('.delegate-live-state[data-delegate-session]');
                      return el && el.textContent.includes('running') && el.textContent.includes('Scripted Live Runner');
                    })()
                """,
            timeout=12,
            label="Delegated card live state line (running · backend)")
        card_link = await evaluate(
            cdp, session_id, """
                    (() => {
                      const live = document.querySelector('.delegate-live-state[data-delegate-session]');
                      if (!live) return null;
                      const box = live.closest('div.font-mono') || live.parentElement;
                      const link = box && box.querySelector('a[href^="/?session="]');
                      return link ? link.getAttribute('href') : null;
                    })()
                """)
        assert_true(card_link == f"/?session={live}", f"the Delegated card links its child ({card_link})")
        shot = await screenshot(cdp, session_id, results, "s21a_parent_running")
        await evaluate(
            cdp, session_id, f"if (Sidebar.isTreeNodeExpanded('{live_parent}')) toggleTreeNode('{live_parent}')")
        await reveal_row(cdp, session_id, live_parent)
        await wait_for(
            cdp,
            session_id,
            f"JSON.stringify({parent_icons}) === '[\"worker-indicator\"]'",
            timeout=12,
            label="the collapsed parent's gear stands in for the running worker")
        shot_collapsed = await screenshot(cdp, session_id, results, "s21a_parent_collapsed_gear")
        await expand_to(cdp, session_id, [live_parent])
        results.record(
            "(a) parent sees the running worker (spinner, gear in both states, live Delegated card)",
            ok=True,
            detail="worker spinner visible; the parent shows the gear expanded and collapsed alike "
            f"({shot_collapsed}); card shows 'running · Scripted Live Runner' and links the child",
            screenshot=shot)

        # (c) the Delegated card opens the child.
        await evaluate(
            cdp, session_id,
            "[...document.querySelectorAll('a[href^=\"/?session=\"]')].find(a => a.getAttribute('href') === '/?session="
            + live + "').click()")
        await wait_for(
            cdp,
            session_id,
            f"location.search === '?session={live}' && document.getElementById('header-session-name').textContent === 'Live worker'",
            timeout=12,
            label="the card's link opened the child page")

        # (a) worker page, fresh mid-Run load: timer ticking, events
        # streaming, the Run's backend, the context reading.
        await wait_for(
            cdp,
            session_id,
            "!document.getElementById('thinking').classList.contains('hidden')",
            timeout=12,
            label="thinking timer visible on a fresh mid-Run load")
        t1 = await evaluate(cdp, session_id, "document.getElementById('thinking-time').textContent")
        await asyncio.sleep(1.6)
        t2 = await evaluate(cdp, session_id, "document.getElementById('thinking-time').textContent")
        assert_true(t1 != t2 and t2.endswith('s'), f"the header timer ticks ({t1} -> {t2})")
        badge = await evaluate(
            cdp, session_id, """
                    (() => {
                      const b = document.getElementById('backend-badge');
                      if (!b) return null;
                      const sel = b.querySelector('select');
                      return sel ? (sel.selectedOptions[0] ? sel.selectedOptions[0].text : null) : b.textContent;
                    })()
                """)
        assert_true(badge == "Scripted Live Runner", f"the header badge shows the Run's backend ({badge})")
        await wait_for(
            cdp,
            session_id,
            """
                    (() => {
                      const ind = document.getElementById('usage-indicator');
                      const bar = document.getElementById('usage-bar');
                      return ind && !ind.classList.contains('hidden') && bar && parseFloat(bar.style.width) > 0;
                    })()
                """,
            timeout=12,
            label="the header context reading from the Run's last context_reading")
        before = await evaluate(
            cdp, session_id, "document.getElementById('messages').textContent.includes('streamed progress line')")
        assert_true(before, "the streamed events render in the main chat")
        last_line = await evaluate(
            cdp, session_id, """
                    (() => {
                      const m = document.getElementById('messages').textContent.match(/streamed progress line (\\d+)/g);
                      return m ? Math.max(...m.map(s => parseInt(s.match(/\\d+$/)[0]))) : 0;
                    })()
                """)
        await wait_for(
            cdp,
            session_id,
            f"""
                    (() => {{
                      const m = document.getElementById('messages').textContent.match(/streamed progress line (\\d+)/g);
                      const top = m ? Math.max(...m.map(s => parseInt(s.match(/\\d+$/)[0]))) : 0;
                      return top >= {last_line} + 2;
                    }})()
                """,
            timeout=8,
            label="new events appear in the open chat within one poll")
        shot = await screenshot(cdp, session_id, results, "s21b_worker_running")
        results.record(
            "(a) worker page mid-Run (timer, streaming, run backend, context reading)",
            ok=True,
            detail=f"timer {t1}->{t2}; badge '{badge}'; context bar painted; streamed lines appear within ~3s",
            screenshot=shot)

        # (e) the stop control stops the active Run.
        mark = cdp.mutation_mark()
        await evaluate(cdp, session_id, "document.querySelector('#thinking button').click()")
        await wait_for(
            cdp,
            session_id,
            "document.getElementById('thinking').classList.contains('hidden')",
            timeout=12,
            label="the thinking timer clears after the stop")
        stop_calls = [m for m in cdp.mutations_since(mark) if m["url"].endswith(f"/runs/{live_run}/cancel")]
        assert_true(len(stop_calls) == 1, f"the stop posts exactly one run cancel ({stop_calls})")
        await wait_for(
            cdp,
            session_id,
            f"document.getElementById('spinner-{live}') && document.getElementById('spinner-{live}').classList.contains('hidden')",
            timeout=12,
            label="the worker row spinner clears after the stop")
        results.record(
            "(e) the stop control stops the active Run",
            ok=True,
            detail=f"one POST /runs/{live_run}/cancel; thinking timer and row spinner clear without a reload",
            screenshot=None)

        # (b) the task closes: the delivery summary and the four links
        # appear in the open chat without a reload. The fact rides the
        # SERVING owner's append funnel: the seeded tree's instances
        # warmed the app-side caches long ago, and an append through
        # them would land on disk without advancing the caches the
        # APIs read.
        from src.runtime.task_execution import task_manager as serving_tree
        serving = serving_tree()
        await serving.events.append(
            live,
            build_control_event(
                ET.TASK_CLOSED,
                actor="worker",
                source_session_id=live,
                request_id="harness-close",
                outcome="completed",
                summary="the checked piece is delivered",
                result_refs=["evidence/harness.txt"]))
        await wait_for(
            cdp,
            session_id,
            """
                    (() => {
                      const banner = document.querySelector('[data-message-role="run_delivery"]');
                      if (!banner) return false;
                      const text = banner.textContent;
                      return text.includes('Delivered') && text.includes('the checked piece is delivered')
                        && ['Raw log', 'Events', 'Result', 'Diff'].every(l => text.includes(l));
                    })()
                """,
            timeout=12,
            label="the delivery close with the four evidence links")
        # Both cues checked where they would show: the worker row's own
        # spinner (the delivered worker may already have left the list —
        # a successful delivery auto-archives it), and the parent's gear
        # with the parent collapsed (the only state the stand-in paints).
        spinner_hidden = await evaluate(
            cdp, session_id, f"""
                    (() => {{
                      const el = document.getElementById('spinner-{live}');
                      return !el || el.classList.contains('hidden');
                    }})()
                """)
        await evaluate(
            cdp, session_id, f"if (Sidebar.isTreeNodeExpanded('{live_parent}')) toggleTreeNode('{live_parent}')")
        await expand_to(cdp, session_id, [ids["root"]])
        await reveal_row(cdp, session_id, live_parent)
        parent_shown = await evaluate(cdp, session_id, parent_icons)
        assert_true(
            spinner_hidden and parent_shown == [],
            f"the running-state cues clear once the Run finished (parent icons {parent_shown})")
        shot = await screenshot(cdp, session_id, results, "s21c_worker_delivered")
        results.record(
            "(b) finished: cues clear, delivery summary and four links shown",
            ok=True,
            detail="banner with summary + Raw log/Events/Result/Diff; gear and spinner cleared without a reload",
            screenshot=shot)
      except Exception as exc:
        # The failure probe: which link in the paint chain broke — the
        # row's presence, its limit hiding, or the stamped thinking
        # state the spinner renders from.
        shot = await screenshot(cdp, session_id, results, "s21_FAILED")
        results.record("(a)-(e) live worker transcript cycle", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S22: the legacy thread opens in the main chat ------------------
      try:
        log("  s22: legacy thread view")
        await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['legacy']}"}, session_id=session_id)
        await wait_for(cdp, session_id, SESSION_LIST_READY_JS)
        row = await evaluate(
            cdp, session_id, f"""
                    (() => {{
                      const row = document.getElementById('session-{ids['legacy_thread']}');
                      if (!row) return null;
                      return {{onclick: row.getAttribute('onclick') || '', text: row.textContent.slice(0, 80)}};
                    }})()
                """)
        assert_true(
            row and "openThreadView" in row["onclick"],
            f"the projected thread row navigates to the thread view ({row})")
        await evaluate(cdp, session_id, f"document.getElementById('session-{ids['legacy_thread']}').click()")
        await wait_for(
            cdp,
            session_id,
            f"location.search === '?session={ids['legacy']}&thread={ids['legacy_thread']}'",
            timeout=12,
            label="the row landed on the thread URL")
        await wait_for(
            cdp,
            session_id,
            "document.getElementById('header-session-name').textContent === 'Review: ## Goal'",
            timeout=12,
            label="the header names the thread")
        await wait_for(
            cdp,
            session_id,
            "document.getElementById('messages').textContent.includes('review verdict: approve')",
            timeout=12,
            label="the thread events render in the main chat")
        hidden_input = await evaluate(
            cdp, session_id, "document.getElementById('input-area').classList.contains('hidden')")
        assert_true(hidden_input, "the thread view takes no message input")
        shot = await screenshot(cdp, session_id, results, "s22_legacy_thread_view")
        results.record(
            "(d) a legacy thread row opens in the main chat at the thread URL",
            ok=True,
            detail="row click navigates to /?session=<parent>&thread=<id>; transcript renders read-only, no input",
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s22_FAILED")
        results.record(
            "(d) a legacy thread row opens in the main chat at the thread URL",
            ok=False,
            detail=repr(exc),
            screenshot=shot)

      # ---- S23: an unread reply in a child manager (the subtree mark) --
      # The reply is seeded through the same in-process owner the scenario
      # tree was seeded with — SessionLifecycle.mark_unread, the master
      # turn's own writer — never by painting the DOM. The root row shows
      # the hollow subtree-unread mark collapsed and expanded alike, the
      # child manager shows its own dot, and opening the child clears
      # both in one paint (the read path's refreshSessionIndicator).
      try:
        log("  s23: unread reply in a child manager")
        handles = ids["_live_handles"]
        await handles["lifecycle"].mark_unread(ids["feature"])
        # The serving app's metadata and listing caches revalidate on
        # their own clock (a 30 s TTL plus the listings sweep), so wait
        # until ITS list reports the flip: from then on the page's
        # first paint and its 3 s status poll agree, and the mark
        # cannot flap back off mid-assertion.
        deadline = time.monotonic() + 120
        while True:
          status, rows = await asyncio.to_thread(api_request, base, access_key, "GET", "/api/sessions/", timeout=10.0)
          status_code, states = await asyncio.to_thread(
              api_request, base, access_key, "GET", f"/api/sessions/status?ids={ids['feature']}", timeout=10.0)
          if status != 200 or status_code != 200:
            fail(f"serving fetch failed: list {status}, status {status_code}")
          row = next((r for r in rows if r.get("id") == ids["feature"]), None)
          state = states.get(ids["feature"], {})
          if row and row.get("has_unread") and state.get("has_unread"):
            break
          if time.monotonic() > deadline:
            fail(
                "the serving list/status never reported the seeded unread reply "
                f"(list={row and row.get('has_unread')}, status={state.get('has_unread')})")
          await asyncio.sleep(1.0)
        # One row's icon table: the kinds whose element lacks the
        # hidden class. Probes exist before the navigation so the
        # failure path can dump them.
        root_icons = f"""
                    ['spinner', 'worker-indicator', 'waiting-indicator', 'unread', 'subtree-unread']
                      .filter(k => !document.getElementById(k + '-{ids['root']}').classList.contains('hidden'))
                """
        feature_icons = f"""
                    ['spinner', 'worker-indicator', 'waiting-indicator', 'unread', 'subtree-unread']
                      .filter(k => !document.getElementById(k + '-{ids['feature']}').classList.contains('hidden'))
                """
        await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['root']}"}, session_id=session_id)
        await wait_for(cdp, session_id, SESSION_LIST_READY_JS)
        # Collapsed root: the mark stands in for the child's unread reply.
        await wait_for(
            cdp,
            session_id,
            f"JSON.stringify({root_icons}) === '[\"subtree-unread\"]'",
            timeout=60,
            label="the collapsed root shows the subtree mark for the child's unread reply")
        shot = await screenshot(cdp, session_id, results, "s23_root_mark_collapsed")
        # Expanded root: the mark stays — expansion never changes the icon.
        await expand_to(cdp, session_id, [ids["root"]])
        await wait_for(
            cdp,
            session_id,
            f"JSON.stringify({root_icons}) === '[\"subtree-unread\"]'",
            timeout=12,
            label="the expanded root keeps the subtree mark")
        await reveal_row(cdp, session_id, ids["feature"])
        await wait_for(
            cdp,
            session_id,
            f"JSON.stringify({feature_icons}) === '[\"unread\"]'",
            timeout=12,
            label="the child manager shows its own dot, no subtree mark")
        shot = await screenshot(cdp, session_id, results, "s23_root_mark_expanded")
        # Opening the child (the SPA switch's real read path) clears its
        # dot and the root's mark in one paint.
        await evaluate(cdp, session_id, f"document.getElementById('session-{ids['feature']}').click()")
        await wait_for(
            cdp,
            session_id,
            f"location.search === '?session={ids['feature']}' && SESSION_ID === '{ids['feature']}'",
            timeout=12,
            label="the child manager's chat opened")
        await wait_for(
            cdp,
            session_id,
            f"JSON.stringify({feature_icons}) === '[]' && JSON.stringify({root_icons}) === '[]'",
            timeout=12,
            label="opening the child clears its dot and the root's mark in one paint")
        shot = await screenshot(cdp, session_id, results, "s23_after_opening_child")
        results.record(
            "an unread reply in a child manager: root mark collapsed and expanded, cleared by opening the child",
            ok=True,
            detail="root shows subtree-unread collapsed and expanded alike; the child shows its own dot; "
            "opening the child clears both without a reload",
            screenshot=shot)
      except Exception as exc:
        diag = ""
        try:
          diag = str(
              await evaluate(
                  cdp, session_id, f"""
                        JSON.stringify({{
                          rootIcons: {root_icons},
                          featureIcons: {feature_icons},
                          rootMarkEl: !!document.getElementById('subtree-unread-{ids['root']}'),
                          featureDotEl: !!document.getElementById('unread-{ids['feature']}'),
                          rootExpanded: Sidebar.isTreeNodeExpanded('{ids['root']}'),
                        }})
                    """))
        except Exception as diag_exc:
          diag = repr(diag_exc)
        shot = await screenshot(cdp, session_id, results, "s23_FAILED")
        results.record(
            "an unread reply in a child manager: root mark collapsed and expanded, cleared by opening the child",
            ok=False,
            detail=repr(exc) + " | " + diag,
            screenshot=shot)
      # ---- S24: the withheld launch --------------------------------------
      try:
        log("  s24: withheld run")
        await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['withhold_worker']}"}, session_id=session_id)
        await wait_for(cdp, session_id, SESSION_LIST_READY_JS)
        # The worker transcript's Run header reads "withheld · <reason>".
        withheld_text = f"withheld · task {ids['withhold_worker']} is cancelled"
        await wait_for(
            cdp,
            session_id,
            f"document.getElementById('messages').textContent.includes({withheld_text!r})",
            timeout=12,
            label="the withheld header renders with its reason")
        header = await evaluate(
            cdp, session_id, """
                    (() => {
                      const el = document.querySelector('[data-run-state="withheld"]');
                      return el ? el.textContent : null;
                    })()
                """)
        assert_true(
            header and withheld_text in header, f"the Run header shows the withheld state with its reason ({header})")
        shot = await screenshot(cdp, session_id, results, "s24_withheld_run")
        results.record(
            "(a) a withheld Run shows 'withheld · <reason>' in its Run header",
            ok=True,
            detail="cancelled task's queued Run; reason from the durable run_launch_withheld fact",
            screenshot=shot)

      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s24_FAILED")
        results.record("withheld run scenario", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S25: the three view pills — Workspace, Later, Archive --------
      # The strip's labels render in order, each pill's click lands in its
      # view (Later serves /api/sessions/starred, Archive builds its
      # project-grouped tree), and coming back to Workspace repaints it.
      try:
        log("  s25: the three view pills")
        await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['root']}"}, session_id=session_id)
        await wait_for(cdp, session_id, SESSION_LIST_READY_JS)
        labels = await evaluate(
            cdp, session_id, """
                    [...document.querySelectorAll('#sidebar-filter-pills .filter-pill')].map(b => b.textContent.trim())
                """)
        assert_true(
            labels == ["Workspace", "Threads", "Later", "Archive"],
            f"the pill strip reads Workspace, Threads, Later, Archive in order ({labels})")
        assert_true(
            await
            evaluate(cdp, session_id, "document.getElementById('filter-all').classList.contains('bg-blue-600/20')"),
            "Workspace is the active pill on load")

        await evaluate(cdp, session_id, "document.getElementById('filter-starred').click()")
        await wait_for(
            cdp,
            session_id, "document.getElementById('filter-starred').classList.contains('bg-blue-600/20')"
            f" && !!document.getElementById('session-{ids['feature']}')",
            timeout=12,
            label="Later renders the starred row")
        later_names = await evaluate(
            cdp, session_id, """
                    [...document.querySelectorAll('#session-list .session-name')].map(el => el.textContent)
                """)
        assert_true(
            any("Feature alpha" in n for n in later_names),
            f"the starred feature manager renders under Later ({later_names})")
        shot = await screenshot(cdp, session_id, results, "s25_later_pill")
        results.record(
            "the Later pill serves the starred queue",
            ok=True,
            detail="strip reads Workspace/Later/Archive; Later shows the starred feature manager",
            screenshot=shot)

        await evaluate(cdp, session_id, "document.getElementById('filter-archived').click()")
        await wait_for(
            cdp,
            session_id, "document.getElementById('filter-archived').classList.contains('bg-blue-600/20')"
            " && !!document.querySelector('#session-list .session-group')",
            timeout=12,
            label="Archive renders its tree")
        await evaluate(cdp, session_id, "document.getElementById('filter-all').click()")
        await wait_for(
            cdp,
            session_id, "document.getElementById('filter-all').classList.contains('bg-blue-600/20')"
            f" && !!document.getElementById('session-{ids['root']}')",
            timeout=12,
            label="Workspace repaints on return")
        shot = await screenshot(cdp, session_id, results, "s25_pill_round_trip")
        results.record(
            "the pills round-trip Workspace / Later / Archive",
            ok=True,
            detail="each pill lands in its view; the archived tree and the Workspace tree both render",
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s25_FAILED")
        results.record("the three view pills", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S25b: the Threads pill — the chat-thread subtree's own view --
      # Workspace (the first paint's own list) lists neither the
      # chat-origin session nor its delegated child; clicking Threads
      # lists the pair nested under "Chat #general" with no
      # group-header plus button (the Settings gear stays); reloading
      # with filter=threads in the URL stays on Threads.
      try:
        log("  s25b: the Threads pill")
        await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['root']}"}, session_id=session_id)
        await wait_for(cdp, session_id, SESSION_LIST_READY_JS)
        workspace_names = await evaluate(
            cdp, session_id, """
                    [...document.querySelectorAll('#session-list .session-name')].map(el => el.textContent)
                """)
        assert_true(
            not any("Chat #general 2026" in n or "Chat thread child" in n for n in workspace_names),
            f"Workspace lists neither the thread session nor its child ({workspace_names})")

        await evaluate(cdp, session_id, "document.getElementById('filter-threads').click()")
        await wait_for(
            cdp,
            session_id, "document.getElementById('filter-threads').classList.contains('bg-blue-600/20')"
            f" && !!document.getElementById('session-{ids['chat_thread']}')",
            timeout=12,
            label="Threads renders the thread session")
        nesting = json.loads(
            await evaluate(
                cdp, session_id, f"""
                    (() => {{
                      const child = document.getElementById('session-{ids['chat_thread_child']}');
                      const group = child && child.closest('.session-group');
                      return JSON.stringify({{
                        group: group ? group.dataset.sgroupKey : null,
                        nested: !!child && child.closest('[data-tree-children="{ids['chat_thread']}"]') !== null,
                      }});
                    }})()
                """))
        assert_true(
            nesting["group"] == "Chat #general" and nesting["nested"],
            f"the child nests under its parent in the channel group ({nesting})")
        header_buttons = json.loads(
            await evaluate(
                cdp, session_id, """
                    JSON.stringify([...document.querySelectorAll('#session-list [data-sgroup-toggle-key]')]
                      .map(h => [...h.querySelectorAll('button[title]')].map(b => b.title)))
                """))
        assert_true(
            all(not any("New session in group" in t for t in titles) for titles in header_buttons),
            f"no group-header plus button renders on Threads ({header_buttons})")
        assert_true(
            all(any(t == "Settings" for t in titles) for titles in header_buttons),
            f"the group header's Settings gear stays ({header_buttons})")
        shot = await screenshot(cdp, session_id, results, "s25b_threads_pill")
        results.record(
            "the Threads pill serves the chat-thread subtree",
            ok=True,
            detail="Workspace omits the subtree; Threads nests the pair under its channel group "
            "with no group-header plus button",
            screenshot=shot)

        await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['root']}&filter=threads"}, session_id=session_id)
        await wait_for(
            cdp,
            session_id, "document.getElementById('filter-threads').classList.contains('bg-blue-600/20')"
            f" && !!document.getElementById('session-{ids['chat_thread']}')",
            timeout=12,
            label="the reload stays on Threads")
        shot = await screenshot(cdp, session_id, results, "s25b_threads_reload")
        results.record(
            "filter=threads survives a reload",
            ok=True,
            detail="the restore path re-enters Threads and refetches the chat-threads list",
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s25b_FAILED")
        results.record("the Threads pill serves the chat-thread subtree", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S26: a bound node's schedule rides its Workspace row ---------
      # The enabled bound node shows the blue clock, the "Next:" line and
      # the truncated cron · timezone line, with the Settings gear (its
      # menu carries Edit schedule); the disabled one goes grey with
      # "Disabled" and no next run.
      try:
        log("  s26: bound nodes' schedule rows in Workspace")
        await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['root']}"}, session_id=session_id)
        await wait_for(
            cdp,
            session_id, f"!!document.getElementById('session-{ids['bound_node']}')"
            f" && !!document.getElementById('session-{ids['paused_node']}')",
            timeout=12,
            label="both bound rows render")
        await reveal_row(cdp, session_id, ids["bound_node"])
        await reveal_row(cdp, session_id, ids["paused_node"])
        bound_row = await evaluate(
            cdp, session_id, f"""
                    (() => {{
                      const row = document.getElementById('session-{ids['bound_node']}');
                      const clock = row.querySelector('svg[title^="Scheduled:"]');
                      return JSON.stringify({{
                            clock: !!clock,
                            blue: !!clock && clock.classList.contains('text-blue-400'),
                            title: clock ? clock.getAttribute('title') : null,
                            next: row.textContent.includes('Next: '),
                            cronTz: row.textContent.includes('0 9 * * * · America/Los_Angeles'),
                            settings: !!row.querySelector('button[title="Settings"]'),
                      }});
                    }})()
                """)
        bound = json.loads(bound_row)
        assert_true(
            bound["clock"] and bound["blue"] and bound["title"] == "Scheduled: harness-daily",
            f"the bound row shows the blue clock naming its task ({bound})")
        assert_true(
            bound["next"] and bound["cronTz"],
            f"the bound row shows the Next line and the cron · timezone line ({bound})")
        assert_true(bound["settings"], "the bound row carries the Settings hover button")
        paused_row = await evaluate(
            cdp, session_id, f"""
                    (() => {{
                      const row = document.getElementById('session-{ids['paused_node']}');
                      const clock = row.querySelector('svg[title^="Scheduled:"]');
                      return JSON.stringify({{
                            clock: !!clock,
                            grey: !!clock && clock.classList.contains('text-slate-500'),
                            disabled: row.textContent.includes('Disabled'),
                            next: row.textContent.includes('Next: '),
                      }});
                    }})()
                """)
        paused = json.loads(paused_row)
        assert_true(
            paused["clock"] and paused["grey"] and paused["disabled"] and not paused["next"],
            f"the disabled bound row goes grey, says Disabled, and names no next run ({paused})")
        shot = await screenshot(cdp, session_id, results, "s26_bound_rows_workspace")
        results.record(
            "a bound node's schedule rides its Workspace row; disabled goes grey",
            ok=True,
            detail="blue clock + Next + cron·timezone + Settings; the disabled row is grey with Disabled",
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s26_FAILED")
        results.record("bound nodes' schedule rows in Workspace", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S27: the Workspace error badge --------------------------------
      # One broken cron file on disk; the badge renders at the top of the
      # Workspace view and opens the cron editor on the broken task.
      try:
        log("  s27: the Workspace error badge")
        await wait_for(
            cdp,
            session_id,
            'document.querySelector("#session-list [role=button][title=\'Open the first failed task\']") !== null'
            " && document.querySelector('#session-list').textContent.includes('1 scheduled tasks failed to load')",
            timeout=12,
            label="the error badge renders")
        badge = await evaluate(
            cdp, session_id, """
                    (() => {
                      const list = document.getElementById('session-list');
                      const badge = list.querySelector('[role="button"][title="Open the first failed task"]');
                      const firstGroup = list.querySelector('.session-group');
                      return JSON.stringify({
                            text: badge ? badge.textContent.trim() : null,
                            onclick: badge ? badge.getAttribute('onclick') : null,
                            onTop: badge && firstGroup ? badge.nextSibling === firstGroup || badge.compareDocumentPosition(firstGroup) & Node.DOCUMENT_POSITION_FOLLOWING : false,
                      });
                    })()
                """)
        badge_info = json.loads(badge)
        assert_true(
            badge_info["text"] == "⚠ 1 scheduled tasks failed to load",
            f"the badge names the broken count ({badge_info})")
        assert_true(
            "openCronEditor('harness-broken')" in (badge_info["onclick"] or ""),
            f"the badge opens the cron editor on the first broken task ({badge_info})")
        assert_true(badge_info["onTop"], "the badge renders at the top of the Workspace view")
        shot = await screenshot(cdp, session_id, results, "s27_workspace_error_badge")
        results.record(
            "the Workspace error badge renders from the broken cron entries",
            ok=True,
            detail="⚠ 1 scheduled tasks failed to load, above the tree, opening the cron editor",
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s27_FAILED")
        results.record("the Workspace error badge", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S28: the Archive tree with its dimmed context ancestor --------
      # The archived firing nests under its still-active bound node: the
      # ancestor arrives as a context_only row, dimmed with the active
      # tag, carrying a star and none of the archived row's actions.
      try:
        log("  s28: the Archive tree with a dimmed context node")
        await evaluate(cdp, session_id, "document.getElementById('filter-archived').click()")
        await wait_for(
            cdp,
            session_id, f"!!document.getElementById('session-{ids['archived_child']}')"
            f" && !!document.getElementById('session-{ids['bound_node']}')",
            timeout=12,
            label="the archived page and its context row render")
        status, archived_page = await asyncio.to_thread(
            api_request, base, access_key, "GET", "/api/sessions/archived?limit=100", timeout=10.0)
        assert_true(status == 200, f"the archived listing answers 200 ({status})")
        strip_all = await evaluate(
            cdp, session_id, """
                    (() => {
                      const pill = [...document.querySelectorAll('#session-list button')]
                        .find(b => b.textContent.trim().startsWith('All '));
                      return pill ? pill.textContent.replace(/[^0-9]/g, '') : null;
                    })()
                """)
        server_total = sum(g["total"] for g in archived_page["groups"])
        assert_true(
            strip_all == str(server_total),
            f"the strip's All count equals the server's archived aggregate ({strip_all} vs {server_total})")
        await expand_to(cdp, session_id, [ids["bound_node"]])
        nest = await evaluate(
            cdp, session_id, f"""
                    (() => {{
                      const child = document.getElementById('session-{ids['archived_child']}');
                      const node = document.getElementById('session-{ids['bound_node']}');
                      const container = document.querySelector("[data-tree-children='{ids['bound_node']}']");
                      const rowText = node.textContent;
                      return JSON.stringify({{
                            nested: !!container && container.contains(child),
                            dimmed: node.classList.contains('opacity-60'),
                            activeTag: rowText.includes('active'),
                            star: !!node.querySelector('button[title^="Star"]'),
                            unarchive: !!node.querySelector('button[title="Unarchive"]'),
                            childUnarchive: !!child.querySelector('button[title="Unarchive"]'),
                            childDimmed: child.classList.contains('opacity-60'),
                      }});
                    }})()
                """)
        nest_info = json.loads(nest)
        assert_true(nest_info["nested"], "the archived firing nests under its scheduled context node")
        assert_true(
            nest_info["dimmed"] and nest_info["activeTag"],
            f"the context node renders dimmed with the active tag ({nest_info})")
        assert_true(
            nest_info["star"] and not nest_info["unarchive"],
            f"the context row keeps the star and takes no unarchive action ({nest_info})")
        assert_true(
            nest_info["childUnarchive"] and not nest_info["childDimmed"],
            f"the archived child keeps its own archived row form ({nest_info})")
        await reveal_row(cdp, session_id, ids["archived_child"])
        await evaluate(
            cdp, session_id,
            f"document.getElementById('session-{ids['bound_node']}').scrollIntoView({{block: 'center'}})")
        shot = await screenshot(cdp, session_id, results, "s28_archive_context_tree")
        results.record(
            "the Archive tree nests the archived firing under its dimmed context node",
            ok=True,
            detail="context_only ancestor: dimmed, active tag, star only; the strip counts archived rows alone",
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s28_FAILED")
        results.record("the Archive tree with a dimmed context node", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S29: a row's actions take no width until the row is hovered --
      # The quantified hover-reveal contract on the seeded ops tree: at
      # rest an unstarred non-current row's name and second-line spans
      # reach the row's right edge with the four actions out of flow and
      # invisible; hovering covers the text end with the four buttons on
      # an opaque cover while the row's height and text layout stay put;
      # the current row keeps its buttons in flow; a starred row shows
      # its solid star at rest; and the star toggles in place, with no
      # list repaint, in both directions.
      try:
        log("  s29: hover reveal action buttons")
        await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['ops_root']}"}, session_id=session_id)
        await wait_for(cdp, session_id, SESSION_LIST_READY_JS)
        await expand_to(cdp, session_id, [ids["ops_root"]])
        await reveal_row(cdp, session_id, ids["ops_mid"])
        cdp.drain_list_fetches()

        # One geometry read per state: row box and paddings, the name
        # and second-line spans, and every direct action button with
        # its computed position/opacity.
        async def row_geo(node_id: str) -> dict:
          raw = await evaluate(
              cdp, session_id, f"""
                        (() => {{
                          const row = document.getElementById('session-{node_id}');
                          if (!row) return null;
                          const r = row.getBoundingClientRect();
                          const cs = getComputedStyle(row);
                          const name = row.querySelector('.session-name');
                          const n = name ? name.getBoundingClientRect() : null;
                          const line = name ? name.nextElementSibling : null;
                          const l = line ? line.getBoundingClientRect() : null;
                          const buttons = [...row.querySelectorAll(':scope > button')].map(b => {{
                            const bs = getComputedStyle(b);
                            const br = b.getBoundingClientRect();
                            return {{title: b.title, position: bs.position, opacity: bs.opacity,
                                     x: br.x, y: br.y, w: br.width, h: br.height,
                                     left: br.left, right: br.right}};
                          }});
                          return {{
                            row: {{x: r.x, y: r.y, w: r.width, h: r.height,
                                  padLeft: parseFloat(cs.paddingLeft), padRight: parseFloat(cs.paddingRight)}},
                            name: n ? {{x: n.x, y: n.y, w: n.width, h: n.height, right: n.right}} : null,
                            line: l ? {{w: l.width, right: l.right}} : null,
                            buttons: buttons,
                            starFill: (() => {{
                              const svg = row.querySelector('button[title="Star"] svg');
                              return svg ? svg.getAttribute('fill') : null;
                            }})()}};
                        }})()
                    """)
          assert_true(raw is not None, f"row {node_id} renders")
          return raw

        rest = await row_geo(ids["ops_mid"])
        row, name, line = rest["row"], rest["name"], rest["line"]
        assert_true(
            len(rest["buttons"]) == 4,
            f"the unstarred row renders its four actions: {[b['title'] for b in rest['buttons']]}")
        assert_true(
            all(b["position"] == "absolute" for b in rest["buttons"]),
            f"the actions are out of flow at rest: {[(b['title'], b['position']) for b in rest['buttons']]}")
        assert_true(
            all(float(b["opacity"]) == 0.0 for b in rest["buttons"]),
            f"the actions are invisible at rest: {[(b['title'], b['opacity']) for b in rest['buttons']]}")
        # The name spans exactly the row minus its paddings and the
        # lead the icons occupy (name.left - content left), and the
        # second-line span reaches the same right edge.
        lead = name["x"] - row["x"] - row["padLeft"]
        expected = row["w"] - row["padLeft"] - row["padRight"] - lead
        assert_true(
            abs(name["w"] - expected) <= 0.5, f"at rest the name spans the row minus lead icons and paddings: "
            f"name {name['w']:.1f}px, expected {expected:.1f}px, row {row['w']:.1f}px")
        assert_true(
            abs(line["w"] - name["w"]) <= 0.5, f"the second-line span reaches the row's right edge too "
            f"({line['w']:.1f}px vs name {name['w']:.1f}px)")

        async def hover_row(node_id: str) -> None:
          geo = await row_geo(node_id)
          await cdp.send(
              "Input.dispatchMouseEvent", {
                  "type": "mouseMoved",
                  "x": geo["row"]["x"] + geo["row"]["w"] / 2,
                  "y": geo["row"]["y"] + geo["row"]["h"] / 2,
              },
              session_id=session_id)

        async def unhover() -> None:
          await cdp.send("Input.dispatchMouseEvent", {"type": "mouseMoved", "x": 6, "y": 6}, session_id=session_id)

        # Hover through the input pipeline: the four buttons appear
        # over the text end while the row's height and the text lines
        # keep their rest layout.
        await hover_row(ids["ops_mid"])
        await wait_for(
            cdp,
            session_id,
            f"""
                    [...document.getElementById('session-{ids['ops_mid']}').querySelectorAll(':scope > button')]
                      .every(b => getComputedStyle(b).opacity === '1')
                """,
            timeout=4,
            label="s29 hover reveals the four buttons")
        hovered = await row_geo(ids["ops_mid"])
        hrow, hname, hline = hovered["row"], hovered["name"], hovered["line"]
        assert_true(
            abs(hrow["h"] - row["h"]) <= 0.5,
            f"the row's height is identical at rest and on hover ({row['h']:.1f}px -> {hrow['h']:.1f}px)")
        assert_true(
            abs(hname["w"] - name["w"]) <= 0.5 and abs(hline["w"] - line["w"]) <= 0.5,
            f"the text lines keep their rest layout on hover "
            f"({name['w']:.1f}/{line['w']:.1f}px -> {hname['w']:.1f}/{hline['w']:.1f}px)")
        titles = sorted(b["title"] for b in hovered["buttons"])
        assert_true(
            titles == sorted(["Star", "New child session", "Archive", "Settings"]),
            f"hover exposes exactly the four actions: {titles}")
        assert_true(
            all(b["position"] == "absolute" for b in hovered["buttons"]),
            "the revealed actions stack out of flow (no reflow)")
        right_edge = hrow["x"] + hrow["w"] - hrow["padRight"]
        assert_true(
            all(
                abs(b["right"] - (right_edge - 30 * i)) <= 0.5
                for i, b in enumerate(sorted(hovered["buttons"], key=lambda b: -b["right"]))),
            f"the actions stack right-to-left on the 30px pitch: "
            f"{[round(b['right'] - right_edge, 1) for b in hovered['buttons']]}")
        assert_true(
            all(
                b["y"] >= hrow["y"] - 0.5 and b["y"] + b["h"] <= hrow["y"] + hrow["h"] + 0.5
                for b in hovered["buttons"]), "the revealed actions stay inside the row's box")
        cover = await evaluate(
            cdp, session_id, f"""
                    [...document.getElementById('session-{ids['ops_mid']}').querySelectorAll(':scope > button')]
                      .map(b => {{
                        const cs = getComputedStyle(b);
                        const br = b.getBoundingClientRect();
                        return {{bg: cs.backgroundColor, image: cs.backgroundImage.startsWith('linear-gradient'),
                                 coversText: br.left < {name['right']}, opaqueRight: br.right <= {right_edge} + 8.5}};
                      }})
                """)
        assert_true(
            all(c["bg"] == "rgb(23, 32, 51)" and c["image"] for c in cover),
            f"the hovered actions sit on the panel+tint cover: {cover}")
        assert_true(
            all(c["coversText"] for c in cover),
            f"the strip covers the text end (name right {name['right']:.1f}px): {cover}")
        # The stretched cover: each revealed button's box reaches the
        # row's top and bottom edges, so no sliver of the name or the
        # second line shows above or below the strip, and the
        # hit-test 2px under the name's top edge lands on the cover,
        # not on the name.
        extents = sorted(((b["title"], b["y"], b["y"] + b["h"]) for b in hovered["buttons"]), key=lambda t: t[1])
        assert_true(
            all(b["y"] <= hrow["y"] + 1 and b["y"] + b["h"] >= hrow["y"] + hrow["h"] - 1 for b in hovered["buttons"]),
            f"each revealed button spans the row's full height "
            f"(row y {hrow['y']:.1f}..{hrow['y'] + hrow['h']:.1f}px, buttons "
            f"{[(t[0], round(t[1], 1), round(t[2], 1)) for t in extents]})")
        log(
            "    s29 cover extents: row y {:.1f}..{:.1f}px; {}".format(
                hrow["y"], hrow["y"] + hrow["h"], ", ".join(f"{t[0]} y {t[1]:.1f}..{t[2]:.1f}px" for t in extents)))
        probe = await evaluate(
            cdp, session_id, f"""
                    (() => {{
                      const row = document.getElementById('session-{ids['ops_mid']}');
                      const star = row.querySelector('button[title="Star"]');
                      const nr = row.querySelector('.session-name').getBoundingClientRect();
                      const sr = star.getBoundingClientRect();
                      const el = document.elementFromPoint(sr.left + sr.width / 2, nr.top + 2);
                      return {{found: !!el, inside: !!el && (el === star || star.contains(el)),
                              tag: el ? el.tagName : null, title: el ? (el.title || '') : null}};
                    }})()
                """)
        assert_true(
            probe["inside"], f"elementFromPoint at the Star slot 2px under the name's top edge "
            f"(y {hname['y'] + 2:.1f}px) returns the button, not the name: {probe}")
        await unhover()
        await wait_for(
            cdp,
            session_id,
            f"""
                    [...document.getElementById('session-{ids['ops_mid']}').querySelectorAll(':scope > button')]
                      .every(b => getComputedStyle(b).opacity === '0')
                """,
            timeout=4,
            label="s29 the actions hide again off hover")

        # The current session keeps its four buttons in flow at rest,
        # with the name truncating before them.
        current = await row_geo(ids["ops_root"])
        assert_true(
            len(current["buttons"]) == 4 and
            all(b["position"] == "static" and float(b["opacity"]) == 1.0 for b in current["buttons"]),
            f"the current row shows its buttons at rest in flow: "
            f"{[(b['title'], b['position'], b['opacity']) for b in current['buttons']]}")
        cur_edge = current["row"]["x"] + current["row"]["w"] - current["row"]["padRight"]
        assert_true(
            abs(max(b["right"] for b in current["buttons"]) - cur_edge) <= 0.5,
            "the current row's actions end at the row's right content edge")
        assert_true(
            current["name"]["right"] <= min(b["left"] for b in current["buttons"]) + 0.5,
            f"the current row's name truncates before its buttons "
            f"(name right {current['name']['right']:.1f}px, first button left "
            f"{min(b['left'] for b in current['buttons']):.1f}px)")

        # A starred row shows its solid star at rest, in flow at the
        # row end, with the other three actions still out of flow.
        # feature nests under the root manager: its level's container
        # starts collapsed on a fresh load, so open it before the
        # reveal walks up to the group's Show-all.
        await expand_to(cdp, session_id, [ids["root"]])
        await reveal_row(cdp, session_id, ids["feature"])
        starred = await row_geo(ids["feature"])
        star = next(b for b in starred["buttons"] if b["title"] == "Star")
        others = [b for b in starred["buttons"] if b["title"] != "Star"]
        assert_true(
            star["position"] == "static" and float(star["opacity"]) == 1.0,
            f"the starred row shows its star at rest: {star}")
        assert_true(starred["starFill"] == "currentColor", f"the rest star is solid (fill {starred['starFill']})")
        star_edge = starred["row"]["x"] + starred["row"]["w"] - starred["row"]["padRight"]
        assert_true(
            abs(star["right"] - star_edge) <= 0.5, f"the rest star sits at the row's right end "
            f"(right {star['right']:.1f}px vs edge {star_edge:.1f}px)")
        assert_true(
            starred["name"]["right"] <= star["left"] + 0.5, f"the name truncates before the rest star "
            f"(name right {starred['name']['right']:.1f}px vs star left {star['left']:.1f}px)")
        assert_true(
            all(b["position"] == "absolute" and float(b["opacity"]) == 0.0 for b in others),
            f"the starred row's other actions stay out of flow at rest: "
            f"{[(b['title'], b['position'], b['opacity']) for b in others]}")

        # Star the unstarred row through a real pointer click on the
        # revealed strip: the in-place toggle paints the rest star, the
        # pointer leaving leaves it visible, and no list repaint rides
        # either direction.
        async def click_star(node_id: str, expect_visible: bool) -> dict:
          await hover_row(node_id)
          await wait_for(
              cdp,
              session_id,
              f"""
                        [...document.getElementById('session-{node_id}').querySelectorAll(':scope > button')]
                          .every(b => getComputedStyle(b).opacity === '1')
                    """,
              timeout=4,
              label="s29 strip revealed for the star click")
          geo = await row_geo(node_id)
          btn = next(b for b in geo["buttons"] if b["title"] == "Star")
          sx, sy = btn["left"] + btn["w"] / 2, btn["y"] + btn["h"] / 2
          for kind in ("mousePressed", "mouseReleased"):
            await cdp.send(
                "Input.dispatchMouseEvent", {
                    "type": kind,
                    "x": sx,
                    "y": sy,
                    "button": "left",
                    "clickCount": 1,
                },
                session_id=session_id)
          wanted = ["!opacity-100", "text-yellow-400"] if expect_visible else ["hover:text-yellow-400"]
          wanted_js = "[" + ", ".join(json.dumps(w) for w in wanted) + "]"
          await wait_for(
              cdp,
              session_id,
              f"""
                        (() => {{
                          const btn = document.getElementById('star-{node_id}');
                          return btn && {wanted_js}.every(c => btn.classList.contains(c));
                        }})()
                    """,
              timeout=6,
              label=f"s29 star toggle painted {'the rest star' if expect_visible else 'the unstar'} in place")
          await unhover()
          # The reveal fades over 150ms: measure only once the
          # not-forced buttons have finished fading out.
          await wait_for(
              cdp,
              session_id,
              f"""
                        [...document.getElementById('session-{node_id}').querySelectorAll(':scope > button')]
                          .filter(b => !b.classList.contains('!opacity-100'))
                          .every(b => getComputedStyle(b).opacity === '0')
                    """,
              timeout=4,
              label="s29 the revealed actions faded out again")
          return await row_geo(node_id)

        async def wait_for_mutation(suffix: str, mark: int) -> dict:
          deadline = time.monotonic() + 8
          while time.monotonic() < deadline:
            hits = [m for m in cdp.mutations_since(mark) if m["url"].endswith(suffix)]
            if hits:
              return hits[0]
            await asyncio.sleep(0.2)
          raise AssertionError(f"no API mutation against {suffix} within 8s")

        # The starred-row reveal scrolled the list: bring the row
        # under the pointer back on screen before the clicks.
        await reveal_row(cdp, session_id, ids["ops_mid"])
        mark = cdp.mutation_mark()
        toggled = await click_star(ids["ops_mid"], expect_visible=True)
        star_post = await wait_for_mutation(f"/api/sessions/{ids['ops_mid']}/star", mark)
        assert_true(star_post["method"] == "POST", f"the click starred the row through the API: {star_post}")
        tstar = next(b for b in toggled["buttons"] if b["title"] == "Star")
        assert_true(
            tstar["position"] == "static" and float(tstar["opacity"]) == 1.0,
            f"the pointer away, the solid star stays at rest: {tstar}")
        assert_true(toggled["starFill"] == "currentColor", f"the rest star is solid (fill {toggled['starFill']})")
        t_edge = toggled["row"]["x"] + toggled["row"]["w"] - toggled["row"]["padRight"]
        assert_true(abs(tstar["right"] - t_edge) <= 0.5, "the toggled star sits at the row's right end")
        assert_true(toggled["name"]["right"] <= tstar["left"] + 0.5, "the name truncates before the toggled rest star")
        t_others = [b for b in toggled["buttons"] if b["title"] != "Star"]
        assert_true(
            all(b["position"] == "absolute" and float(b["opacity"]) == 0.0 for b in t_others),
            "the starred row's other actions hid again off hover")
        fetches = cdp.drain_list_fetches()
        assert_true(fetches == [], f"the star toggle repainted no list: {fetches}")

        mark = cdp.mutation_mark()
        untoggled = await click_star(ids["ops_mid"], expect_visible=False)
        star_unpost = await wait_for_mutation(f"/api/sessions/{ids['ops_mid']}/unstar", mark)
        assert_true(star_unpost["method"] == "POST", f"the second click unstarred the row: {star_unpost}")
        ustar = next(b for b in untoggled["buttons"] if b["title"] == "Star")
        assert_true(
            ustar["position"] == "absolute" and float(ustar["opacity"]) == 0.0,
            f"unstarred, the star hides from the rest state again: {ustar}")
        assert_true(
            untoggled["starFill"] == "none", f"the unstarred star is hollow again (fill {untoggled['starFill']})")
        fetches = cdp.drain_list_fetches()
        assert_true(fetches == [], f"the unstar repainted no list: {fetches}")

        shot = await screenshot(cdp, session_id, results, "s29_hover_reveal")
        results.record(
            "a row's actions take no width until the row is hovered",
            ok=True,
            detail=(
                f"rest name {name['w']:.1f}px of {expected:.1f}px expected on a {row['w']:.1f}px row, "
                f"second line {line['w']:.1f}px; hover keeps height {row['h']:.1f}px and text layout, "
                f"exposes Star/New child session/Archive/Settings on the opaque cover, each "
                f"button spanning the row's full height y {hrow['y']:.1f}..{hrow['y'] + hrow['h']:.1f}px; "
                f"current row in flow; starred rest star toggles in place, no list repaint"),
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s29_hover_reveal_FAILED")
        results.record(
            "a row's actions take no width until the row is hovered", ok=False, detail=repr(exc), screenshot=shot)

      # The desktop viewport baseline the touch scenarios must restore.
      desktop_viewport = await evaluate(cdp, session_id, "window.innerWidth + 'x' + window.innerHeight")

      # ---- S30: the desktop Settings menus' item lists -------------------
      # Every row kind's gear opens the one shared .row-menu with the
      # plan's items -- labels, order, separators and red danger items
      # compared exactly. The Workspace rows first, then the archived
      # pair in the Archive view; Escape closes between rows and exactly
      # one .row-menu ever exists.
      group_name = ids["group_name"]
      try:
        log("  s30: the desktop row menus' item lists")
        await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['ops_root']}"}, session_id=session_id)
        await wait_for(cdp, session_id, SESSION_LIST_READY_JS)
        await expand_to(cdp, session_id, [ids["root"], ids["ops_root"]])
        measured = []
        for name, node_id, expected in [
            ("unbound root manager", ids["ops_root"], ["Rename", "Move to group…", "Task & context", "Add schedule…"]),
            ("child manager", ids["ops_mid"], ["Rename", "Task & context", "Add schedule…"]),
            ("legacy profile-null row", ids["legacy"], ["Rename", "Move to group…"]),
            ("bound root manager", ids["bound_node"], ["Rename", "Move to group…", "Task & context", "Edit schedule…"]),
        ]:
          await reveal_row(cdp, session_id, node_id)
          actual = await open_row_menu(cdp, session_id, f"#session-{node_id} > button[title='Settings']", name)
          assert_menu_matches(actual, expected, name)
          measured.append(f"{name} {len(actual)}")
          await close_row_menu(cdp, session_id, name)
        header_selector = f"[data-sgroup-toggle-key='{group_name}'] > button[title='Settings']"
        await evaluate(
            cdp, session_id, f"""
                    document.querySelector("[data-sgroup-toggle-key='{group_name}']").scrollIntoView({{block: 'center'}})
                """)
        actual = await open_row_menu(cdp, session_id, header_selector, "named group header")
        assert_menu_matches(
            actual, ["New scheduled task", "Rename group", "sep", "Delete group (danger)"], "named group header")
        measured.append("named group header 4")
        await close_row_menu(cdp, session_id, "named group header")

        await evaluate(cdp, session_id, "document.getElementById('filter-archived').click()")
        await wait_for(
            cdp,
            session_id, f"!!document.getElementById('session-{ids['archived_root']}')"
            f" && !!document.getElementById('session-{ids['archived_child']}')",
            timeout=12,
            label="the archived rows render")
        await expand_to(cdp, session_id, [ids["bound_node"]])
        for name, node_id, expected in [
            ("archived root", ids["archived_root"], ["Move to group…", "sep", "Delete permanently (danger)"]),
            ("archived child", ids["archived_child"], ["Delete permanently (danger)"]),
        ]:
          await reveal_row(cdp, session_id, node_id)
          actual = await open_row_menu(cdp, session_id, f"#session-{node_id} > button[title='Settings']", name)
          assert_menu_matches(actual, expected, name)
          measured.append(f"{name} {len(actual)}")
          await close_row_menu(cdp, session_id, name)
        shot = await screenshot(cdp, session_id, results, "s30_desktop_row_menus")
        results.record(
            "the desktop Settings menus' item lists",
            ok=True,
            detail=(
                "every row kind's gear opens exactly one .row-menu with the plan's "
                "items (item counts: " + ", ".join(measured) + "); Escape closes each"),
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s30_desktop_row_menus_FAILED")
        results.record("the desktop Settings menus' item lists", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S30b: the hover reveal stays scoped to the hovered row -------
      # The live check this slice fixes: the row buttons reveal with
      # Tailwind's group-hover, which rides every ancestor carrying the
      # `group` class -- each group section wrapper carries one -- so
      # hovering anything inside a group section set every row's
      # out-of-flow buttons in it to opacity 1, painted over those
      # rows' text without their hover cover. In the seeded named group
      # (three roots): hovering one row's NAME reveals that row's four
      # actions alone; hovering the named group header reveals no row's
      # buttons, while the header's own + and gear keep today's
      # section-hover reveal.
      try:
        log("  s30b: hover scope stays on the hovered row")
        await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['ops_root']}"}, session_id=session_id)
        await wait_for(cdp, session_id, SESSION_LIST_READY_JS)
        await reveal_row(cdp, session_id, ids["alpha_second"])
        cdp.drain_list_fetches()

        # One read per hover state: every visible row's non-pinned
        # buttons (the !opacity-100 pins excluded) with their
        # computed opacity.
        async def visible_button_matrix() -> list[dict]:
          raw = await evaluate(
              cdp, session_id, """
                        (() => {
                          const rows = [...document.querySelectorAll('#session-list a.session-row')]
                            .filter(a => a.offsetParent !== null);
                          return JSON.stringify(rows.map(row => ({
                            id: row.id,
                            buttons: [...row.querySelectorAll(':scope > button')]
                              .filter(b => !b.classList.contains('!opacity-100'))
                              .map(b => ({title: b.title, opacity: getComputedStyle(b).opacity})),
                          })));
                        })()
                    """)
          return json.loads(raw)

        async def hover_point(x: float, y: float) -> None:
          await cdp.send("Input.dispatchMouseEvent", {"type": "mouseMoved", "x": x, "y": y}, session_id=session_id)
          await asyncio.sleep(0.3)

        async def hover_name(node_id: str) -> None:
          box = json.loads(
              await evaluate(
                  cdp, session_id, f"""
                        (() => {{
                          const r = document.querySelector('#session-{node_id} .session-name')
                            .getBoundingClientRect();
                          return JSON.stringify({{x: r.x + r.width / 2, y: r.y + r.height / 2}});
                        }})()
                    """))
          await hover_point(box["x"], box["y"])

        async def assert_hover_scope(hovered_id: str | None, label: str) -> tuple[int, int]:
          matrix = await visible_button_matrix()
          if hovered_id is None:
            lit = [
                (r["id"], b["title"], b["opacity"]) for r in matrix for b in r["buttons"] if float(b["opacity"]) != 0.0
            ]
            assert_true(lit == [], f"the {label} lights row buttons: {lit[:6]}")
          else:
            hovered = next(r for r in matrix if r["id"] == f"session-{hovered_id}")
            titles = sorted(b["title"] for b in hovered["buttons"])
            assert_true(
                titles == ["Archive", "New child session", "Settings", "Star"],
                f"the hovered row reveals its four actions: {titles}")
            assert_true(
                all(float(b["opacity"]) == 1.0 for b in hovered["buttons"]),
                f"the hovered row's actions are visible: {hovered['buttons']}")
            lit = [
                (r["id"], b["title"], b["opacity"]) for r in matrix if r["id"] != f"session-{hovered_id}"
                for b in r["buttons"] if float(b["opacity"]) != 0.0
            ]
            assert_true(lit == [], f"the {label} lights other rows' buttons: {lit[:6]}")
          return len(matrix), sum(len(r["buttons"]) for r in matrix)

        await hover_name(ids["alpha_second"])
        rows_h, buttons_h = await assert_hover_scope(ids["alpha_second"], "row hover")

        header_box = json.loads(
            await evaluate(
                cdp, session_id, f"""
                    (() => {{
                      const r = document.querySelector(
                        "[data-sgroup-toggle-key='{ids['group_name']}']").getBoundingClientRect();
                      return JSON.stringify({{x: r.x + r.width / 2, y: r.y + r.height / 2}});
                    }})()
                """))
        await hover_point(header_box["x"], header_box["y"])
        rows_g, buttons_g = await assert_hover_scope(None, "group-header hover")
        await hover_point(6, 6)

        shot = await screenshot(cdp, session_id, results, "s30b_hover_scope")
        results.record(
            "the hover reveal stays scoped to the hovered row",
            ok=True,
            detail=(
                f"hovering one row's name lights its Star/New child session/"
                f"Archive/Settings alone ({rows_h} visible rows, {buttons_h} "
                f"non-pinned buttons checked); hovering the named group header "
                f"lights no row's buttons ({rows_g} rows, {buttons_g} buttons "
                f"checked)"),
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s30b_hover_scope_FAILED")
        results.record("the hover reveal stays scoped to the hovered row", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S31: touch row actions at 412x915 -----------------------------
      # The phone input profile (mobile metrics, touch on, hover none and
      # pointer coarse) drives the drawer sidebar: normal and archived
      # rows show the Settings gear alone, a starred row keeps its solid
      # star as a read-only indicator, a worker row keeps Archive, the
      # named header keeps + and the gear -- every tappable box 44x44 --
      # and the bound row stays within three name lines. The touch menus
      # lead with the Later toggle and the touch-only actions; every item
      # is at least 44px tall.
      try:
        log("  s31: touch row actions at 412x915")
        await cdp.send(
            "Emulation.setDeviceMetricsOverride", {
                "mobile": True,
                "width": 412,
                "height": 915,
                "deviceScaleFactor": 2.625
            },
            session_id=session_id)
        await cdp.send(
            "Emulation.setTouchEmulationEnabled", {
                "enabled": True,
                "maxTouchPoints": 5
            }, session_id=session_id)
        await cdp.send(
            "Emulation.setEmulatedMedia",
            {"features": [{
                "name": "hover",
                "value": "none"
            }, {
                "name": "pointer",
                "value": "coarse"
            }]},
            session_id=session_id)
        emu = json.loads(
            await evaluate(
                cdp, session_id, """
                    JSON.stringify({
                      hoverNone: matchMedia('(hover: none)').matches,
                      hoverHover: matchMedia('(hover: hover)').matches,
                      coarse: matchMedia('(pointer: coarse)').matches,
                      width: window.innerWidth, height: window.innerHeight,
                    })
                """))
        assert_true(
            emu["hoverNone"] and not emu["hoverHover"] and emu["coarse"],
            f"the emulated media profile is a touch device: {emu}")
        assert_true((emu["width"], emu["height"]) == (412, 915), f"the emulated viewport is 412x915: {emu}")
        await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['ops_root']}"}, session_id=session_id)
        await wait_for(cdp, session_id, SESSION_LIST_READY_JS)
        await evaluate(cdp, session_id, "toggleMobileSidebar()")
        await wait_for(
            cdp,
            session_id,
            "document.getElementById('sidebar').classList.contains('open')",
            timeout=6,
            label="the drawer opens")
        await expand_to(cdp, session_id, [ids["root"], ids["feature"]])
        touch_numbers = []

        async def touch_buttons(node_id: str) -> list[dict]:
          raw = await evaluate(
              cdp, session_id, f"""
                        (() => {{
                          const row = document.getElementById('session-{node_id}');
                          if (!row) return null;
                          return [...row.querySelectorAll(':scope > button')]
                            .map(b => {{
                              const bs = getComputedStyle(b);
                              const br = b.getBoundingClientRect();
                              return {{title: b.title, display: bs.display, pe: bs.pointerEvents,
                                       w: Math.round(br.width * 10) / 10,
                                       h: Math.round(br.height * 10) / 10}};
                            }})
                            .filter(b => b.display !== 'none');
                        }})()
                    """)
          assert_true(raw is not None, f"the touch row {node_id} renders")
          return raw

        def assert_tappable(buttons: list[dict], label: str) -> None:
          for b in buttons:
            if b["pe"] == "none":
              continue
            assert_true(
                b["w"] >= 44 and b["h"] >= 44, f"{label}: {b['title']} is a 44x44 target, got {b['w']}x{b['h']}")

        for node_id, label in ((ids["ops_root"], "normal current row"), (ids["late_root"], "normal row")):
          await reveal_row(cdp, session_id, node_id)
          buttons = await touch_buttons(node_id)
          assert_true([b["title"] for b in buttons] == ["Settings"], f"the {label} shows Settings alone: {buttons}")
          assert_tappable(buttons, label)
          touch_numbers.append(f"{label} gear {buttons[0]['w']}x{buttons[0]['h']}")

        await reveal_row(cdp, session_id, ids["feature"])
        buttons = await touch_buttons(ids["feature"])
        assert_true(
            [b["title"] for b in buttons] == ["Star", "Settings"],
            f"the starred row shows its star and Settings: {buttons}")
        assert_true(
            buttons[0]["pe"] == "none", f"the rest star is a read-only indicator (pointer-events {buttons[0]['pe']})")
        assert_tappable(buttons, "starred row")
        touch_numbers.append(f"star pe {buttons[0]['pe']}, gear {buttons[1]['w']}x{buttons[1]['h']}")

        await reveal_row(cdp, session_id, ids["worker1"])
        buttons = await touch_buttons(ids["worker1"])
        assert_true([b["title"] for b in buttons] == ["Archive"], f"the worker row keeps Archive alone: {buttons}")
        assert_tappable(buttons, "worker row")
        touch_numbers.append(f"worker Archive {buttons[0]['w']}x{buttons[0]['h']}")

        header_buttons = await evaluate(
            cdp, session_id, f"""
                    (() => {{
                      const header = document.querySelector("[data-sgroup-toggle-key='{group_name}']");
                      if (!header) return null;
                      return [...header.querySelectorAll(':scope > button')].map(b => {{
                        const bs = getComputedStyle(b);
                        const br = b.getBoundingClientRect();
                        return {{title: b.title, pe: bs.pointerEvents,
                                 w: Math.round(br.width * 10) / 10,
                                 h: Math.round(br.height * 10) / 10}};
                      }});
                    }})()
                """)
        assert_true(
            header_buttons is not None and [b["title"] for b in header_buttons] == ["New session in group", "Settings"],
            f"the named header keeps + and gear: {header_buttons}")
        assert_tappable(header_buttons, "named group header")
        touch_numbers.append(
            f"header + {header_buttons[0]['w']}x{header_buttons[0]['h']}, "
            f"gear {header_buttons[1]['w']}x{header_buttons[1]['h']}")

        await reveal_row(cdp, session_id, ids["bound_node"])
        bound = await evaluate(
            cdp, session_id, f"""
                    (() => {{
                      const row = document.getElementById('session-{ids['bound_node']}');
                      const r = row.getBoundingClientRect();
                      const cs = getComputedStyle(row);
                      const name = row.querySelector('.session-name');
                      const lh = parseFloat(getComputedStyle(name).lineHeight);
                      const padY = parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom);
                      return {{h: Math.round(r.height * 10) / 10, padY, lineHeight: lh,
                               limit: Math.round((padY + 3 * lh) * 10) / 10}};
                    }})()
                """)
        assert_true(bound["h"] <= bound["limit"], f"the bound touch row stays within three name lines: {bound}")
        touch_numbers.append(f"bound row {bound['h']}px <= pad {bound['padY']}px + 3x{bound['lineHeight']}px")

        gear = "#session-{} > button[title='Settings']"
        # Re-reveal before each open: the geometry reads above scrolled
        # the list, and a gear below the fold click-tests as nothing.
        await reveal_row(cdp, session_id, ids["ops_root"])
        actual = await open_row_menu(cdp, session_id, gear.format(ids["ops_root"]), "touch unstarred root manager")
        assert_menu_matches(
            actual, [
                "Add to Later", "New child session", "Rename", "Move to group…", "Task & context", "Add schedule…",
                "sep", "Archive"
            ], "touch unstarred root manager")
        touch_numbers.append(
            f"root menu min item {await assert_menu_item_heights(cdp, session_id, 'touch unstarred root manager')}px")
        await close_row_menu(cdp, session_id, "touch unstarred root manager")

        await reveal_row(cdp, session_id, ids["feature"])
        actual = await open_row_menu(cdp, session_id, gear.format(ids["feature"]), "touch starred row")
        assert_true(actual[0] == "Remove from Later", f"the starred row's first item is Remove from Later: {actual}")
        assert_menu_matches(
            actual,
            ["Remove from Later", "New child session", "Rename", "Task & context", "Add schedule…", "sep", "Archive"],
            "touch starred row")
        await assert_menu_item_heights(cdp, session_id, "touch starred row")
        await close_row_menu(cdp, session_id, "touch starred row")

        await evaluate(cdp, session_id, "document.getElementById('filter-archived').click()")
        await wait_for(
            cdp,
            session_id, f"!!document.getElementById('session-{ids['archived_root']}')"
            f" && !!document.getElementById('session-{ids['archived_child']}')",
            timeout=12,
            label="the archived rows render in the drawer")
        await expand_to(cdp, session_id, [ids["bound_node"]])
        for node_id in (ids["archived_root"], ids["archived_child"]):
          await reveal_row(cdp, session_id, node_id)
          buttons = await touch_buttons(node_id)
          assert_true(
              [b["title"] for b in buttons] == ["Settings"], f"the archived touch row shows Settings alone: {buttons}")
          assert_tappable(buttons, "archived row")
        touch_numbers.append("archived gears 44x44")

        await reveal_row(cdp, session_id, ids["archived_root"])
        actual = await open_row_menu(cdp, session_id, gear.format(ids["archived_root"]), "touch archived root")
        assert_menu_matches(
            actual, ["Add to Later", "Unarchive", "Move to group…", "sep", "Delete permanently (danger)"],
            "touch archived root")
        touch_numbers.append(
            f"archived root menu min item {await assert_menu_item_heights(cdp, session_id, 'touch archived root')}px")
        shot = await screenshot(cdp, session_id, results, "s31_touch_row_actions")
        await close_row_menu(cdp, session_id, "touch archived root")

        await reveal_row(cdp, session_id, ids["archived_child"])
        actual = await open_row_menu(cdp, session_id, gear.format(ids["archived_child"]), "touch archived child")
        assert_menu_matches(
            actual, ["Add to Later", "Unarchive", "sep", "Delete permanently (danger)"], "touch archived child")
        touch_numbers.append(
            f"archived child menu min item {await assert_menu_item_heights(cdp, session_id, 'touch archived child')}px")
        await close_row_menu(cdp, session_id, "touch archived child")

        results.record(
            "the touch row actions at 412x915",
            ok=True,
            detail=("drawer open at 412x915, hover none + pointer coarse: " + "; ".join(touch_numbers)),
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s31_touch_row_actions_FAILED")
        results.record("the touch row actions at 412x915", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S31b: the scheduled row's Last line at 412x915 ----------------
      # The second live check this slice fixes: the seeded fire's Last
      # line was the one schedule line without truncate, so it wrapped
      # to a second row line on the touch drawer. The paused node
      # carries the seeded fire (failed + timestamp through
      # update_slot_fields, allow_failure on the task), so its row
      # shows the four text lines -- name, Disabled, cron - timezone,
      # Last -- each one line tall, with the Last line truncating like
      # the cron line above it and carrying its full text in title.
      try:
        log("  s31b: the scheduled row's Last line at 412x915")
        await cdp.send(
            "Emulation.setDeviceMetricsOverride", {
                "mobile": True,
                "width": 412,
                "height": 915,
                "deviceScaleFactor": 2.625
            },
            session_id=session_id)
        await cdp.send(
            "Emulation.setTouchEmulationEnabled", {
                "enabled": True,
                "maxTouchPoints": 5
            }, session_id=session_id)
        await cdp.send(
            "Emulation.setEmulatedMedia",
            {"features": [{
                "name": "hover",
                "value": "none"
            }, {
                "name": "pointer",
                "value": "coarse"
            }]},
            session_id=session_id)
        await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['root']}"}, session_id=session_id)
        await wait_for(cdp, session_id, SESSION_LIST_READY_JS)
        await evaluate(cdp, session_id, "toggleMobileSidebar()")
        await wait_for(
            cdp,
            session_id,
            "document.getElementById('sidebar').classList.contains('open')",
            timeout=6,
            label="the drawer opens")
        await reveal_row(cdp, session_id, ids["paused_node"])
        raw = await evaluate(
            cdp, session_id, f"""
                    (() => {{
                      const row = document.getElementById('session-{ids['paused_node']}');
                      if (!row) return null;
                      const name = row.querySelector('.session-name');
                      const lines = [...name.parentElement.children].map(el => {{
                        const cs = getComputedStyle(el);
                        const b = el.getBoundingClientRect();
                        return {{text: el.textContent, h: Math.round(b.height * 10) / 10,
                                 lineHeight: parseFloat(cs.lineHeight), whiteSpace: cs.whiteSpace,
                                 textOverflow: cs.textOverflow, title: el.getAttribute('title') || ''}};
                      }});
                      return JSON.stringify({{
                        rowH: Math.round(row.getBoundingClientRect().height * 10) / 10,
                        lines}});
                    }})()
                """)
        assert_true(raw is not None, "the paused scheduled row renders in the drawer")
        measured = json.loads(raw)
        lines = measured["lines"]
        texts = [line["text"] for line in lines]
        assert_true(
            len(lines) == 4 and texts[1].startswith("Disabled") and texts[2].startswith("0 9 * * *") and
            texts[3].startswith("Last: "), f"the paused row renders name, Disabled, cron and Last lines: {texts}")
        for line in lines:
          assert_true(line["h"] <= line["lineHeight"] + 1, f"each text line is one line tall: {line}")
        last, cron = lines[3], lines[2]
        assert_true(
            last["whiteSpace"] == "nowrap" and last["textOverflow"] == "ellipsis" and
            last["whiteSpace"] == cron["whiteSpace"] and last["textOverflow"] == cron["textOverflow"],
            f"the Last line truncates like the cron line above it: {last} vs {cron}")
        assert_true(
            last["title"] == last["text"] and last["title"].startswith("Last: "),
            f"the Last line's title carries its full text: {last!r}")
        shot = await screenshot(cdp, session_id, results, "s31b_last_line")
        results.record(
            "the scheduled row's Last line stays on one line at 412x915",
            ok=True,
            detail=(
                f"paused row {measured['rowH']}px tall; line heights " + "/".join(f"{line['h']}" for line in lines) +
                f"px at line-height {lines[0]['lineHeight']}px; the Last line "
                "truncates with an ellipsis like the cron line and its title "
                f"carries the full text ({last['title']})"),
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s31b_last_line_FAILED")
        results.record(
            "the scheduled row's Last line stays on one line at 412x915", ok=False, detail=repr(exc), screenshot=shot)

      # ---- S32: the cover screen at 280x800 ------------------------------
      # Same touch emulation at the Z Fold cover width: the drawer clamps
      # to the screen (its right edge at most 280) and every visible row
      # entry lies within 0..280.
      try:
        log("  s32: the cover-screen drawer at 280x800")
        await cdp.send(
            "Emulation.setDeviceMetricsOverride", {
                "mobile": True,
                "width": 280,
                "height": 800,
                "deviceScaleFactor": 2.625
            },
            session_id=session_id)
        cover = json.loads(
            await evaluate(
                cdp, session_id, """
                    JSON.stringify({
                      hoverNone: matchMedia('(hover: none)').matches,
                      width: window.innerWidth, height: window.innerHeight,
                    })
                """))
        assert_true(
            cover["hoverNone"] and (cover["width"], cover["height"]) == (280, 800),
            f"the cover emulation is 280x800 touch: {cover}")
        await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['ops_root']}"}, session_id=session_id)
        await wait_for(cdp, session_id, SESSION_LIST_READY_JS)
        await evaluate(cdp, session_id, "toggleMobileSidebar()")
        await wait_for(
            cdp,
            session_id,
            "document.getElementById('sidebar').classList.contains('open')",
            timeout=6,
            label="the drawer opens on the cover screen")
        drawer = await evaluate(
            cdp, session_id, """
                    (() => {
                      const r = document.getElementById('sidebar').getBoundingClientRect();
                      return {left: Math.round(r.left * 10) / 10, right: Math.round(r.right * 10) / 10,
                              width: Math.round(r.width * 10) / 10};
                    })()
                """)
        assert_true(drawer["right"] <= 280.5, f"the drawer's right edge stays on the cover screen: {drawer}")
        # Show the whole (No group) group first: the widest possible row
        # set is what must fit, not the 5-row preview.
        sweep = await evaluate(
            cdp, session_id, """
                    (() => {
                      toggleSessionGroupLimit('');
                      const rows = [...document.querySelectorAll('#session-list a.session-row')]
                        .filter(a => a.offsetParent !== null)
                        .map(a => {
                          const r = a.getBoundingClientRect();
                          return {id: a.id, x: Math.round(r.x * 10) / 10,
                                  right: Math.round(r.right * 10) / 10};
                        });
                      return {count: rows.length,
                              leftmost: rows.reduce((m, r) => Math.min(m, r.x), 0),
                              rightmost: rows.reduce((m, r) => Math.max(m, r.right), 0),
                              sample: rows.slice(0, 4)};
                    })()
                """)
        assert_true(sweep["count"] > 0, "rows render in the open drawer")
        assert_true(
            sweep["leftmost"] >= -0.5 and sweep["rightmost"] <= 280.5, f"every visible row lies within 0..280: {sweep}")
        shot = await screenshot(cdp, session_id, results, "s32_cover_drawer")
        results.record(
            "the cover-screen drawer at 280x800",
            ok=True,
            detail=(
                f"drawer right edge {drawer['right']}px of 280 "
                f"(width {drawer['width']}px); {sweep['count']} visible rows within "
                f"0..280, leftmost {sweep['leftmost']}px, rightmost {sweep['rightmost']}px"),
            screenshot=shot)
      except Exception as exc:
        shot = await screenshot(cdp, session_id, results, "s32_cover_drawer_FAILED")
        results.record("the cover-screen drawer at 280x800", ok=False, detail=repr(exc), screenshot=shot)

      # The touch scenarios leave the desktop emulation behind: media
      # features cleared, touch off, and the capture viewport pinned back
      # at its baseline (a bare clear leaves chrome's own window math).
      # The live document keeps its notified feature set until the next
      # navigation, so the restored profile is read on a fresh load.
      await cdp.send("Emulation.clearDeviceMetricsOverride", session_id=session_id)
      await cdp.send("Emulation.setEmulatedMedia", {"features": []}, session_id=session_id)
      await cdp.send("Emulation.setTouchEmulationEnabled", {"enabled": False}, session_id=session_id)
      base_w, base_h = desktop_viewport.split("x")
      await cdp.send(
          "Emulation.setDeviceMetricsOverride", {
              "mobile": False,
              "width": int(base_w),
              "height": int(base_h),
              "deviceScaleFactor": 1
          },
          session_id=session_id)
      await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['ops_root']}"}, session_id=session_id)
      await wait_for(cdp, session_id, SESSION_LIST_READY_JS)
      restored = json.loads(
          await evaluate(
              cdp, session_id, """
                JSON.stringify({
                  size: window.innerWidth + 'x' + window.innerHeight,
                  hoverHover: matchMedia('(hover: hover)').matches,
                  hoverNone: matchMedia('(hover: none)').matches,
                  coarse: matchMedia('(pointer: coarse)').matches,
                })
            """))
      assert_true(
          restored["size"] == desktop_viewport and restored["hoverHover"] and not restored["hoverNone"] and
          not restored["coarse"], f"the desktop emulation is restored (baseline {desktop_viewport}): {restored}")

      # The CDP collector records console.error calls and uncaught page
      # exceptions from Runtime.enable onward — this list is the only
      # source; an assertion over an always-empty list is a faked pass.
      console_errors = [e for e in cdp.console_errors if "favicon" not in e]
      results.console_errors = console_errors
      results.record(
          "console clean",
          len(console_errors) == 0,
          f"{len(console_errors)} console errors" + (f": {console_errors[:3]}" if console_errors else ""), None)

      await cdp.close()
    finally:
      # The live-run scenario's real process and its events grower stop
      # with the harness, whatever the scenarios concluded.
      handles = ids.get("_live_handles") or {}
      stop_event = handles.get("stop")
      if stop_event is not None:
        stop_event.set()
      live_process = handles.get("process")
      stop_child(live_process, grace_s=5, kill_reap_s=5)
      stop_child(chrome_proc, grace_s=5, kill_reap_s=5)
      server.should_exit = True
      try:
        await asyncio.wait_for(serve_task, timeout=10)
      except Exception as exc:
        log(f"server stop: {exc!r}")

  results.save()
  failed = [s for s in results.scenarios if not s["ok"]]
  if failed:
    fail(f"{len(failed)} scenario(s) failed: {[f['name'] for f in failed]}")
  log("BROWSER HARNESS PASSED")


def main() -> None:
  parser = argparse.ArgumentParser(description=__doc__)
  parser.add_argument("--evidence-dir", default=str(EVIDENCE_ROOT_DEFAULT))
  parser.add_argument("--chrome", default=None)
  parser.add_argument("--keep", action="store_true", help="keep the temp dir (debugging)")
  args = parser.parse_args()
  asyncio.run(run_harness(args))


if __name__ == "__main__":
  main()
