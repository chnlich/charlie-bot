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
  workers, run records with recorded facts, pending inputs) is seeded through
  the same task_sessions owner the APIs serve, in this process only. Every
  UI mutation under test then rides the real HTTP API from the browser.
- Browser. System google-chrome (checked first; an absent binary is an
  explicit failure — never a faked pass) driven over CDP with a private
  ``--user-data-dir`` profile inside the harness temp dir. Screenshots,
  per-scenario assertion results and the exact tested commit land in
  --evidence-dir (default: a directory under the host temp dir, never in git).

Run:  uv run python scripts/browser_harness_session_tree.py \
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
from datetime import datetime, timezone  # noqa: E402

# Evidence defaults to a host temp directory so the public repo carries no
# host path; pass --evidence-dir to keep evidence with its owning session.
EVIDENCE_ROOT_DEFAULT = Path(tempfile.gettempdir()) / "charliebot-session-tree-evidence"


def log(message: str) -> None:
    print(message, flush=True)


def fail(message: str) -> None:
    raise SystemExit(f"BROWSER HARNESS FAILED: {message}")


def pick_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


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
                            self.mutations.append({
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
        append_events_line(path, {
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": f"streamed progress line {n}"}]},
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
    (memory_dir / "topics").write_text(
        "program-notes resident\narchive-notes\n", encoding="utf-8")
    (memory_dir / "entries" / "program-notes" / "deploy-runbook.md").write_text(
        "---\nscope: host\ntopic: program-notes\naudience: master\n"
        "title: Deploy runbook\n---\nDeploy through the pinned recipe; verify the health endpoint first.\n",
        encoding="utf-8")
    (memory_dir / "entries" / "archive-notes" / "old-migration.md").write_text(
        "---\nscope: host\ntopic: archive-notes\naudience: master\n"
        "title: Old migration notes\n---\nThe 2024 migration is complete; query on demand only.\n",
        encoding="utf-8")


async def seed_scenario(home: Path) -> dict:
    """Create the acceptance scenario's task tree and recorded run facts.

    The operator-actions scenarios need collection sizes beyond one page:
    more roots than the move chooser's page, a manager with more direct
    children than the old single 100-record read, and a run history deeper
    than the completion dialog's page — all seeded through the same
    task_sessions owner the APIs serve, in-process only.
    """
    os.environ["CHARLIEBOT_HOME"] = str(home)
    from src.core import event_types as ET
    from src.core.config import get_config
    from src.core.models import (
        CreateSessionRequest,
        PatchSessionTaskRequest,
        RunRecord,
        TaskSpec,
        ThreadMetadata,
    )
    from src.core.run_token import CallerIdentity
    from src.core.sessions import SessionManager
    from src.core.task_sessions import TaskTreeManager

    cfg = get_config()
    session_mgr = SessionManager(cfg)
    tree = TaskTreeManager(cfg, session_mgr)
    OP = CallerIdentity(kind="operator")

    async def seed() -> dict:
        # Bulk roots first: created_at order puts them on the move chooser's
        # first pages, so "Program rollout" and everything created after land
        # on a LATER page.
        bulk = {}
        for i in range(1, 29):
            meta = await tree.create_task(
                request_id=f"seed-bulk-{i}", task_parent_id=None, profile="manager",
                task=TaskSpec(goal=f"bulk root {i:02d}"), name=f"Bulk root {i:02d}",
                backend=None, caller=OP)
            if i <= 3:
                bulk[f"bulk{i}"] = meta.id
        root = await tree.create_task(
            request_id="seed-root", task_parent_id=None, profile="manager", task=None,
            name="Program rollout", backend=None, caller=OP)
        await tree.patch_task(root.id, PatchSessionTaskRequest(
            task={"goal": "Ship the program rollout", "acceptance": ["all features delivered"],
                  "context_refs": [], "repo_path": None, "base_branch": None,
                  "task_type": None, "keep_worktree": False}), caller=OP)
        feature = await tree.create_task(
            request_id="seed-feature", task_parent_id=root.id, profile="manager", task=None,
            name="Feature alpha", backend=None, caller=OP)
        await tree.patch_task(feature.id, PatchSessionTaskRequest(
            task={"goal": "Deliver feature alpha end to end", "acceptance": ["tests pass"],
                  "context_refs": [], "repo_path": None, "base_branch": None,
                  "task_type": "implement", "keep_worktree": False}), caller=OP)
        worker1 = await tree.create_task(
            request_id="seed-w1", task_parent_id=feature.id, profile="worker", task=None,
            name="Worker one", backend=None, caller=OP)
        worker2 = await tree.create_task(
            request_id="seed-w2", task_parent_id=feature.id, profile="worker", task=None,
            name="Worker two", backend=None, caller=OP)
        # Recorded run facts: worker one finished a work run and a review run
        # (one leaf, two run rows); worker two has a pending input.
        await tree.runs.register_run(
            RunRecord(id="run-w1-work", session_id=worker1.id, kind="work",
                      backend="fake-scripted", model="scripted-model"), task_spec_text="worker spec")
        await tree.dispatch.finish_run(worker1.id, "run-w1-work", outcome="success")
        await tree.runs.register_run(
            RunRecord(id="run-w1-review", session_id=worker1.id, kind="review",
                      backend="fake-scripted", model="scripted-model", review_of_run_id="run-w1-work"),
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
                feature.id, request_id="seed-ack-report",
                input_ids=[str(e["id"]) for e in report_inputs],
                note="seed: report accepted", caller=OP)
        await tree.dispatch.admit_input(
            worker2.id, event_type=ET.USER, content="Please also verify the docs page", actor="user")
        # The root manager carries the real-user takeoff authorization an
        # agent-scoped creation is judged against (takeoff_gate).
        await tree.events.append(root.id, {
            "id": "seed-takeoff-user", "type": ET.USER,
            "timestamp": "2026-01-01T00:00:00+00:00",
            "content": "take off and run the program rollout"})
        await tree.completion.acknowledge_inputs(
            root.id, request_id="seed-ack-takeoff", input_ids=["seed-takeoff-user"],
            note="seed: operator authorization", caller=OP)
        # Long-history node: 150 started runs whose committed snapshots are
        # distinguishable (generation 0001..0150), plus never-launched
        # reservations. The current-run Context selection must show generation
        # 0150 — the whole-history latest launch — not the first page's tail.
        from datetime import datetime, timedelta, timezone

        from src.core.control_events import sha256_hex
        from src.core.task_prompts import PromptBlock, PromptSnapshot, PromptSource
        long_worker = await tree.create_task(
            request_id="seed-long", task_parent_id=feature.id, profile="worker",
            task=TaskSpec(goal="carry a long run history", task_type="implement"),
            name="Long history worker", backend=None, caller=OP)
        base = datetime(2026, 1, 1, tzinfo=timezone.utc)
        latest_hash = None
        for i in range(1, 151):
            run_id = f"run-{i:04d}"
            await tree.runs.register_run(RunRecord(
                id=run_id, session_id=long_worker.id, kind="work", backend="fake-scripted",
                model="scripted-model", started_at=base + timedelta(minutes=i)))
            text = f"managed instructions generation {i:04d}"
            block = PromptBlock(
                sources=(PromptSource(scope="base", source_ref="base:work", source_session_id=None),),
                body_ref=sha256_hex(text), delivery="full", text=text)
            snapshot = PromptSnapshot(blocks=(block,))
            snap_path = tree.runs.run_dir(long_worker.id, run_id) / "prompt_snapshot.json"
            snap_path.parent.mkdir(parents=True, exist_ok=True)
            snap_path.write_text(json.dumps(snapshot.to_json_dict()), encoding="utf-8")
            await tree.runs.record_observation(
                long_worker.id, run_id, prompt_snapshot_ref=str(snap_path))
            await tree.dispatch.finish_run(long_worker.id, run_id, outcome="success")
            if i == 150:
                latest_hash = snapshot.prompt_hash
        for q in ("queued-a", "queued-b"):
            await tree.runs.register_run(RunRecord(id=q, session_id=long_worker.id, kind="work"))
        # An active Run identity on the root manager: the seeded agent caller's
        # run token must bind a launched, non-terminal Run (run_identity_refusal).
        from src.core.runs import read_pid_stat
        pid_start, _state = read_pid_stat(os.getpid())
        await tree.runs.register_run(RunRecord(
            id="agent-auth-run", session_id=root.id, kind="manager_turn",
            pid=os.getpid(), pid_start=pid_start))

        # --- operator-actions scenarios (S16-S20) ---------------------------
        # A late root holding an intermediate manager (a move target reached
        # through the chooser's later roots page) and a wide manager whose
        # direct-children list exceeds the old single 100-record read.
        late_root = await tree.create_task(
            request_id="seed-late-root", task_parent_id=None, profile="manager",
            task=TaskSpec(goal="late root for move paging"), name="Late root",
            backend=None, caller=OP)
        late_mid = await tree.create_task(
            request_id="seed-late-mid", task_parent_id=late_root.id, profile="manager",
            task=TaskSpec(goal="intermediate manager"), name="Late mid",
            backend=None, caller=OP)
        wide = await tree.create_task(
            request_id="seed-wide", task_parent_id=late_mid.id, profile="manager",
            task=TaskSpec(goal="a manager with more children than one page"), name="Wide manager",
            backend=None, caller=OP)
        for i in range(1, 106):
            await tree.create_task(
                request_id=f"seed-wide-child-{i}", task_parent_id=wide.id, profile="worker",
                task=TaskSpec(goal=f"wide child {i:03d}"), name=f"Wide child {i:03d}",
                backend=None, caller=OP)

        # A three-level ops tree whose leaf has a FAILED run: the root and the
        # nested manager rows carry attention + subtree counts — the rows the
        # readability scenario measures.
        ops_root = await tree.create_task(
            request_id="seed-ops-root", task_parent_id=None, profile="manager",
            task=TaskSpec(goal="ops root"), name="Ops root", backend=None, caller=OP)
        ops_mid = await tree.create_task(
            request_id="seed-ops-mid", task_parent_id=ops_root.id, profile="manager",
            task=TaskSpec(goal="ops mid manager"), name="Ops mid", backend=None, caller=OP)
        failing = await tree.create_task(
            request_id="seed-failing", task_parent_id=ops_mid.id, profile="worker",
            task=TaskSpec(goal="a worker whose run failed"), name="Failing worker",
            backend=None, caller=OP)
        await tree.runs.register_run(RunRecord(
            id="run-fail-1", session_id=failing.id, kind="work", backend="fake-scripted",
            model="scripted-model", started_at=base + timedelta(minutes=500)))
        await tree.dispatch.finish_run(failing.id, "run-fail-1", outcome="failed")

        # Completion-evidence MANAGER: 120 successful manager_turn runs. A
        # manager never auto-completes (that is worker-only), so the task stays
        # open and manually completable, with early evidence beyond the first
        # desc page and beyond the old 100-record read.
        evidence = await tree.create_task(
            request_id="seed-evidence", task_parent_id=feature.id, profile="manager",
            task=TaskSpec(goal="carry enough runs to page the evidence picker"),
            name="Evidence manager", backend=None, caller=OP)
        for i in range(1, 121):
            run_id = f"run-e{i:04d}"
            await tree.runs.register_run(RunRecord(
                id=run_id, session_id=evidence.id, kind="manager_turn", backend="fake-scripted",
                model="scripted-model", started_at=base + timedelta(minutes=1000 + i)))
            await tree.dispatch.finish_run(evidence.id, run_id, outcome="success")

        # Dialog-binding pair: A is a run-less worker (open, completable in
        # principle, nothing auto-closes it), B is a childless manager (its
        # cancel succeeds — used for the late-response drop).
        bind_a = await tree.create_task(
            request_id="seed-bind-a", task_parent_id=root.id, profile="worker",
            task=TaskSpec(goal="dialog binding task A"), name="Bind task A",
            backend=None, caller=OP)
        bind_b = await tree.create_task(
            request_id="seed-bind-b", task_parent_id=None, profile="manager",
            task=TaskSpec(goal="dialog binding task B"), name="Bind task B",
            backend=None, caller=OP)

        # --- the live worker: a REAL process mid-Run --------------------------
        # The node's own Run marks it busy (thinking_since at the recorded
        # started_at), the collapsed parent's gear stands in for it, and
        # the events file grows on a real thread while the browser scenario
        # watches the transcript stream. The display backend differs from the
        # inherited metadata.backend, so a correct page shows the Run's backend.
        from src.core.control_events import build_control_event
        from src.core.runs import read_pid_stat

        # The live worker's own delegating manager: an otherwise idle parent,
        # so its collapsed row's stand-in shows the gear. (The root itself
        # carries the agent-auth Run, whose unobserved identity is its own
        # attention verdict — and a row's own state outranks any stand-in.)
        live_parent = await tree.create_task(
            request_id="seed-live-parent", task_parent_id=root.id, profile="manager",
            task=TaskSpec(goal="delegate the live worker"), name="Live rollout",
            backend=None, caller=OP)
        live = await tree.create_task(
            request_id="seed-live", task_parent_id=live_parent.id, profile="worker",
            task=TaskSpec(goal="watch this worker run live"), name="Live worker",
            backend=None, caller=OP)
        live_run_dir = tree.runs.run_dir(live.id, "run-live")
        # The delivered history: one successful work Run with the four evidence
        # refs, so the delivery close has real links to show.
        await tree.runs.register_run(RunRecord(
            id="run-live-done", session_id=live.id, kind="work",
            backend="scripted-live", model="scripted-model",
            started_at=base + timedelta(minutes=900),
            repo_path=str(home / "harness-repo"), base_branch="main",
            branch_name="task/live-delivered"))
        append_events_line(live_run_dir.parent / "run-live-done" / "events.jsonl", {
            "type": ET.USER, "content": "deliver the checked piece",
            "timestamp": (base + timedelta(minutes=900)).isoformat()})
        await tree.runs.record_launch(live.id, "run-live-done", pid=424100, pid_start="1-424100")
        await tree.runs.record_observation(
            live.id, "run-live-done",
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
        append_events_line(live_events, {
            "type": ET.USER, "content": "watch this worker run live",
            "timestamp": datetime.now(timezone.utc).isoformat()})
        append_events_line(live_events, {
            "type": ET.SYSTEM, "subtype": ET.CONTEXT_READING,
            "context_reading": {"context_tokens": 42000, "context_full": 200000,
                                "context_compact_at": 160000, "model": "scripted-model"},
            "timestamp": datetime.now(timezone.utc).isoformat()})
        await tree.runs.register_run(RunRecord(
            id="run-live", session_id=live.id, kind="work",
            backend="scripted-live", model="scripted-model",
            started_at=datetime.now(timezone.utc)))
        await tree.runs.record_observation(
            live.id, "run-live",
            repo_path=str(home / "harness-repo"), base_branch="main",
            branch_name="task/run-live")
        await tree.runs.record_launch(live.id, "run-live",
                                      pid=live_proc.pid, pid_start=live_pid_start)
        live_stop = threading.Event()
        live_counter = {"lines": 0}
        threading.Thread(target=grow_run_events, args=(live_events, live_stop, live_counter),
                         daemon=True).start()

        # The parent's Delegated card: a new-style delegation whose event
        # carries the child session id.
        await tree.events.append(live_parent.id, build_control_event(
            ET.TASK_DELEGATED, actor="agent", source_session_id=live_parent.id,
            request_id="seed-delegate-live",
            thread_id="run-live", child_session_id=live.id,
            description="watch this worker run live",
            backend="scripted-live", model="scripted-model",
            **{ET.DELEGATE_INVOCATION: {
                "task_type": "implement", "repo_path": str(home / "harness-repo"),
                "base_branch": "main", "task_spec_file": "task.md",
                "reviewer_context_file": None, "keep_worktree": False,
                "backend": "scripted-live"}}))

        # --- the legacy worker thread: the 4.1 thread view --------------------
        # A pre-task-tree delegation lives in the parent session's threads/
        # directory; the sidebar projects it as a read-only row and the thread
        # URL opens its transcript in the main chat.
        legacy = await session_mgr.create_session(
            CreateSessionRequest(name="Legacy operator"), backend="fake-scripted")
        legacy_thread_id = "thread-legacy-1"
        thread_dir = home / "sessions" / legacy.id / "threads" / legacy_thread_id
        (thread_dir / "data").mkdir(parents=True)
        started = base + timedelta(minutes=800)
        thread_meta = ThreadMetadata(
            id=legacy_thread_id, session_id=legacy.id, description="Review: ## Goal",
            status="completed", started_at=started,
            completed_at=started + timedelta(minutes=9), backend="fake-scripted",
            pid=424050, pid_start="1-424050", exit_code=0)
        (thread_dir / "metadata.json").write_text(
            thread_meta.model_dump_json(indent=2), encoding="utf-8")
        thread_events = []
        for i, (kind, text) in enumerate([
                (ET.USER, "review the delivered diff"), (ET.ASSISTANT, "review verdict: approve")]):
            event = {"type": kind, "timestamp": (started + timedelta(minutes=i)).isoformat()}
            if kind == ET.USER:
                event["content"] = text
            else:
                event["message"] = {"content": [{"type": "text", "text": text}]}
            thread_events.append(event)
        thread_events.append({"type": "master_done",
                              "timestamp": (started + timedelta(minutes=5)).isoformat()})
        (thread_dir / "data" / "events.jsonl").write_text(
            "".join(json.dumps(e) + "\n" for e in thread_events), encoding="utf-8")

        seed_memory_store(home)
        return {"root": root.id, "feature": feature.id, "worker1": worker1.id, "worker2": worker2.id,
                "long": long_worker.id, "latest_hash": latest_hash,
                "late_root": late_root.id, "late_mid": late_mid.id, "wide": wide.id,
                "ops_root": ops_root.id, "ops_mid": ops_mid.id, "failing": failing.id,
                "evidence": evidence.id, "bind_a": bind_a.id, "bind_b": bind_b.id,
                "live": live.id, "live_parent": live_parent.id, "legacy": legacy.id, "legacy_thread": legacy_thread_id,
                "live_run": "run-live",
                "_live_handles": {"process": live_proc, "stop": live_stop,
                                  "counter": live_counter, "tree": tree,
                                  "session_mgr": session_mgr},
                **bulk}

    return await seed()


# ---------------------------------------------------------------------------
# Chrome launch and page wire-up (shared by the browser harnesses)
# ---------------------------------------------------------------------------


def launch_chrome(chrome: str, profile: Path, debug_port: int, flags: list[str]) -> subprocess.Popen:
    """Start headless chrome with a CDP endpoint and a private profile; return the process.

    The caller owns the returned process: terminate and reap it when the run
    ends. *flags* carries the harness's own switches (window size, throttling
    bans); the launch sandwich around them - headless mode, the debug port,
    the profile, the start URL - is the one shared shape.
    """
    return subprocess.Popen(
        [chrome, "--headless=new", f"--remote-debugging-port={debug_port}",
         f"--user-data-dir={profile}", *flags, "about:blank"],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)


async def devtools_ws_url(chrome_proc: subprocess.Popen, timeout_s: float,
                          fail: Callable[[str], None]) -> str:
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
    attached = await cdp.send("Target.attachToTarget",
                              {"targetId": target["targetId"], "flatten": True})
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

    def __init__(self, evidence_dir: Path, commit: str, *, browser: str, results_name: str,
                 entry_point: str | None = None, invocation: list[str] | None = None) -> None:
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


def api_request(base: str, key: str, method: str, path: str, body: dict | None = None,
                *, timeout: float, token: str | None = None) -> tuple[int, dict]:
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
    res = await cdp.send("Runtime.evaluate", {
        "expression": expression, "returnByValue": True, "awaitPromise": True,
    }, session_id=session_id)
    if res.get("exceptionDetails"):
        detail = res["exceptionDetails"]
        # Full details plus the expression head: a bare description hides whether
        # the failure is a parse error of the sent text or a throw inside it.
        raise RuntimeError(
            "page evaluate failed: " + json.dumps(detail)[:600]
            + " | expression head: " + expression[:160].replace("\n", " "))
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
        await wait_for(cdp, session_id, f"!!document.getElementById('session-{node_id}')",
                       timeout=8, label=f"row visible for {node_id}")
        await evaluate(cdp, session_id, f"Sidebar.expandTreeNode('{node_id}')")
        await wait_for(cdp, session_id,
                       '(() => { const el = document.querySelector('
                       "\"[data-tree-children='" + node_id + "']\");"
                       " return el && !el.classList.contains('hidden'); })()",
                       timeout=8, label=f"children container for {node_id}")


async def reveal_row(cdp: CDP, session_id: str, node_id: str) -> None:
    """Bring one (possibly nested) row on screen for the screenshot evidence.

    A nested row sits inside its root's subtree wrapper, which the group's
    5-row preview hides with the root when the root falls past the limit (only
    the active row itself is exempt): Show all that group, then scroll the row
    into view. Expansion is the caller's (expand_to)."""
    await evaluate(cdp, session_id, f"""
        (() => {{
          const row = document.getElementById('session-{node_id}');
          let el = row;
          // The "(No group)" group's key is the empty string: test presence.
          while (el && !(el.dataset && 'sessionGroupLimitExtra' in el.dataset)) el = el.parentElement;
          if (el && el.classList.contains('hidden')) toggleSessionGroupLimit(el.dataset.sessionGroupLimitExtra);
          row.scrollIntoView({{block: 'center'}});
        }})()
    """)
    await wait_for(cdp, session_id, f"document.getElementById('session-{node_id}').offsetParent !== null",
                   timeout=8, label=f"row on screen for {node_id}")


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


async def wait_for(cdp: CDP, session_id: str, expression: str, timeout: float = 10.0,
                   label: str | None = None) -> object:
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
    raise AssertionError(
        f"timeout waiting for {label or expression[:120]}\npage state: {diag[:1200]}")


async def run_harness(args: argparse.Namespace) -> None:
    chrome = args.chrome or shutil.which("google-chrome") or shutil.which("google-chrome-stable")
    if not chrome:
        fail("google-chrome is not installed; install it or pass --chrome (no fake output)")

    evidence_dir = Path(args.evidence_dir)
    evidence_dir.mkdir(parents=True, exist_ok=True)
    commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=REPO_ROOT,
                            capture_output=True, text=True, check=True).stdout.strip()
    results = Results(evidence_dir, commit,
                      browser="google-chrome headless (CDP)",
                      results_name="session_tree_browser_results.json")

    with tempfile.TemporaryDirectory(prefix="charliebot-browser-harness-") as tmp:
        tmp_path = Path(tmp)
        home = tmp_path / "charliebot-home"
        home.mkdir()
        server_port = pick_free_port()
        config = {
            "server": {"port": server_port, "host": "127.0.0.1"},
            "backends": {"options": [{
                "id": "fake-scripted", "label": "Scripted (never launches)",
                "type": "cc-claude", "model": "scripted-model",
            }, {
                "id": "scripted-live", "label": "Scripted Live Runner",
                "type": "cc-claude", "model": "scripted-model",
            }], "preference": ["fake-scripted"]},
            "paths": {"worktree_dir": str(home / "worktrees")},
        }
        (home / "config.yaml").write_text(json.dumps(config, indent=2), encoding="utf-8")
        access_key = "harness-operator-key-" + os.urandom(8).hex()
        (home / "credentials.yaml").write_text(
            f"charliebot:\n  access_key: {access_key}\n", encoding="utf-8")
        # Clear inherited production credentials from this process env.
        for var in ("CHARLIEBOT_ACCESS_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"):
            os.environ.pop(var, None)
        os.environ["CHARLIEBOT_HOME"] = str(home)

        ids = await seed_scenario(home)

        # Isolated server: the real app, lifespan disabled.
        import uvicorn

        from server import app as server_app

        server_config = uvicorn.Config(server_app, host="127.0.0.1", port=server_port,
                                       log_level="error", lifespan="off")
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
        chrome_proc = launch_chrome(chrome, profile, debug_port, [
            "--no-first-run", "--no-default-browser-check",
            "--disable-background-networking", "--window-size=1440,900",
            "--remote-allow-origins=*",
            # Keep the page fully active: background throttling would delay
            # timers/fetches and distort the live-update evidence.
            "--disable-background-timer-throttling",
            "--disable-backgrounding-occluded-windows",
            "--disable-renderer-backgrounding",
        ])
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
            await cdp.send("Network.setCookie", {
                "name": "charliebot_access_key", "value": access_key,
                "url": f"http://127.0.0.1:{server_port}/",
            }, session_id=session_id)
            await cdp.send("Emulation.setDeviceMetricsOverride", {
                "width": 1440, "height": 900, "deviceScaleFactor": 1, "mobile": False,
            }, session_id=session_id)

            base = f"http://127.0.0.1:{server_port}"

            # ---- S1: desktop load; the session tree is the primary navigation --
            try:
                await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['root']}"}, session_id=session_id)
                await wait_for(cdp, session_id, "document.querySelectorAll('#session-list .session-name').length >= 1")
                await expand_to(cdp, session_id, [ids["root"], ids["feature"]])
                names = await evaluate(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .session-name')].map(el => el.textContent)
                """)
                assert_true("Program rollout" in names and "Feature alpha" in names
                            and "Worker one" in names and "Worker two" in names,
                            f"the task nodes render nested in the sidebar: {names}")
                active = await evaluate(cdp, session_id, "SESSION_ID")
                assert_true(active == ids["root"], "the deep link opened the root manager's chat")
                shot = await screenshot(cdp, session_id, results, "s1_desktop_tree")
                results.record("desktop tree primary navigation", ok=True,
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
                    data=json.dumps({"name": "Feature alpha renamed"}).encode(),
                    headers={"Authorization": f"Bearer {access_key}", "Content-Type": "application/json"},
                    method="PATCH")
                with await asyncio.to_thread(_u.urlopen, req, timeout=10) as resp:
                    assert resp.status == 200
                await wait_for(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .session-name')]
                      .some(el => el.textContent === 'Feature alpha renamed')
                """, timeout=8)
                fetches = cdp.drain_list_fetches()
                assert_true(1 <= len(fetches) <= 4,
                            f"bounded list work for the change ({len(fetches)} list fetches)")
                active_ok = await evaluate(cdp, session_id, "SESSION_ID")
                assert_true(active_ok == ids["root"], "an update to another node never switches the active session")
                shot = await screenshot(cdp, session_id, results, "s9_live_update")
                results.record("live tree change repaints the open sidebar", ok=True,
                               detail="row refreshed in place; bounded list fetches for the change; session unchanged", screenshot=shot)
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
                    api_request, base, access_key, "POST", "/api/sessions/",
                    {"request_id": "harness-x-root-1", "profile": "manager", "name": "Remote root",
                     "task": {"goal": "created outside the browser", "acceptance": [], "context_refs": []}},
                    timeout=15.0)
                assert_true(status == 200, f"the separate client's create succeeded ({status}: {meta})")
                await wait_for(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .session-name')]
                      .some(el => el.textContent === 'Remote root')
                """, timeout=10, label="s12 remote root row appeared from the notification alone")
                fetches = cdp.drain_list_fetches()
                assert_true(1 <= len(fetches) <= 4,
                            f"bounded list work for the creation ({len(fetches)} list fetches)")
                active_after = await evaluate(cdp, session_id, "SESSION_ID")
                assert_true(active_after == active_before, "the creation never switches the active session")
                tree_events_after = await evaluate(cdp, session_id, "window.__treeEvents || 0")
                assert_true(tree_events_after > tree_events_before,
                            "the creation rode a real server-originated task_tree_changed event")
                names = await evaluate(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .session-name')].map(el => el.textContent)
                """)
                assert_true(names.count("Remote root") == 1, "exactly one row for the new root")
                shot = await screenshot(cdp, session_id, results, "s12_cross_client_creation")
                results.record("creation from a separate client reaches the observer", ok=True,
                               detail="row appeared from the notification alone; bounded list work; session unchanged",
                               screenshot=shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s12_creation_FAILED")
                results.record("creation from a separate client reaches the observer", ok=False, detail=repr(exc), screenshot=shot)

            # ---- S13: deeper-than-one-level creation and collapsed levels -----
            try:
                log("  s13: deep creation")
                await expand_to(cdp, session_id, [ids["root"], ids["feature"]])
                status, mid = await asyncio.to_thread(
                    api_request, base, access_key, "POST", "/api/sessions/",
                    {"request_id": "harness-x-mid-1", "task_parent_id": ids["feature"],
                     "profile": "manager", "name": "Remote mid",
                     "task": {"goal": "mid manager", "acceptance": [], "context_refs": []}}, timeout=15.0)
                assert_true(status == 200, f"nested manager create succeeded ({status})")
                await wait_for(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .session-name')]
                      .some(el => el.textContent === 'Remote mid')
                """, timeout=10, label="s13 remote mid row under the expanded feature")
                # A worker under the new mid: the mid stays collapsed, so the
                # leaf is not painted — the collapsed row carries the count.
                status, _leaf = await asyncio.to_thread(
                    api_request, base, access_key, "POST", "/api/sessions/",
                    {"request_id": "harness-x-leaf-1", "task_parent_id": mid["id"],
                     "profile": "worker", "name": "Remote leaf",
                     "task": {"goal": "idle leaf worker", "acceptance": [], "context_refs": []}}, timeout=15.0)
                assert_true(status == 200, f"deep leaf create succeeded ({status})")
                await wait_for(cdp, session_id, f"""
                    (() => {{
                      const row = document.getElementById('session-{mid["id"]}');
                      const chev = row && row.querySelector('[data-tree-toggle="{mid["id"]}"]');
                      return chev && (chev.getAttribute('title') || '').includes('1 child task');
                    }})()
                """, timeout=10, label="s13 mid row gained the child count while collapsed")
                collapsed_ok = await evaluate(cdp, session_id, f"""
                    (() => {{
                      const el = document.querySelector('[data-tree-children="{mid["id"]}"]');
                      return el && el.classList.contains('hidden');
                    }})()
                """)
                assert_true(collapsed_ok, "the collapsed level stays collapsed")
                await evaluate(cdp, session_id, f"Sidebar.expandTreeNode('{mid['id']}')")
                await wait_for(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .session-name')]
                      .some(el => el.textContent === 'Remote leaf')
                """, timeout=8, label="s13 leaf visible after expanding the mid")
                shot = await screenshot(cdp, session_id, results, "s13_deep_creation")
                results.record("deeper-than-one-level creation from a separate client", ok=True,
                               detail="mid appeared under the expanded parent; collapsed mid gained the count; expansion reveals the idle leaf", screenshot=shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s13_deep_creation_FAILED")
                results.record("deeper-than-one-level creation from a separate client", ok=False, detail=repr(exc), screenshot=shot)

            # ---- S14: agent-scoped creation reaches the observer --------------
            try:
                log("  s14: scoped-agent creation")
                from src.core.run_token import RunTokenClaims, sign_run_token
                agent_token = sign_run_token(
                    RunTokenClaims(session_id=ids["root"], run_id="agent-auth-run", agent="harness-agent"),
                    access_key)
                status, worker = await asyncio.to_thread(
                    api_request, base, access_key, "POST", "/api/sessions/",
                    {"request_id": "harness-agent-w1", "task_parent_id": ids["root"],
                     "profile": "worker", "name": "Agent worker",
                     "task": {"goal": "created by a scoped agent", "acceptance": [], "context_refs": []}},
                    token=agent_token, timeout=15.0)
                assert_true(status == 200, f"agent-scoped create succeeded ({status}: {worker})")
                await wait_for(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .session-name')]
                      .some(el => el.textContent === 'Agent worker')
                """, timeout=10, label="s14 agent-created worker appeared")
                shot = await screenshot(cdp, session_id, results, "s14_agent_creation")
                results.record("agent-scoped creation reaches a connected observer", ok=True,
                               detail="run-token agent created a worker under its manager; the observer saw it live", screenshot=shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s14_agent_creation_FAILED")
                results.record("agent-scoped creation reaches a connected observer", ok=False, detail=repr(exc), screenshot=shot)

            # ---- S20: the name stays readable at any depth ---------------------
            try:
                log("  s20: name readability at depth")
                await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['ops_root']}"}, session_id=session_id)
                await wait_for(cdp, session_id, "document.querySelectorAll('#session-list .session-name').length >= 1")
                await expand_to(cdp, session_id, [ids["ops_root"]])

                async def assert_readable(node_id: str, label: str) -> dict:
                    geo = await evaluate(cdp, session_id, f"""
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
                    assert_true(geo["name"] and geo["name"]["w"] > 0 and geo["name"]["x"] >= geo["row"]["x"],
                                f"{label}: the name renders inside the row: {geo}")
                    assert_true(geo["clipped"] is not None,
                                f"{label}: the name truncation is measurable: {geo}")
                    return geo

                ops_geo = await assert_readable(ids["ops_root"], "ops root")
                mid_geo = await assert_readable(ids["ops_mid"], "nested ops manager")
                assert_true("Ops root" == ops_geo["nameText"] and "Ops mid" == mid_geo["nameText"],
                            "both measured rows carry their full task names")
                assert_true(mid_geo["name"]["x"] > ops_geo["name"]["x"] + 10,
                            f"the nested row indents behind its parent ({ops_geo['name']['x']} -> {mid_geo['name']['x']})")
                assert_true(mid_geo["newChild"], "the nested manager keeps its New child control")
                spill = await evaluate(cdp, session_id,
                    "document.documentElement.scrollWidth - document.documentElement.clientWidth")
                assert_true(spill <= 1, f"no horizontal spill at depth ({spill}px)")
                shot = await screenshot(cdp, session_id, results, "s20_desktop_readability")
                results.record("tree names stay readable at any depth", ok=True,
                               detail="full names render, truncate rather than spill, indent per level, controls kept", screenshot=shot)
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
            from src.core import event_types as ET
            from src.core.control_events import build_control_event
            try:
                log("  s21: live worker transcript")
                # (a) parent page: worker spinner, the collapsed parent's gear,
                # Delegated card live line + link — all following the status
                # poll. The expanded parent shows only its own state (its
                # descendants show theirs); collapsed, it stands in with the gear.
                live_parent = ids["live_parent"]
                parent_icons = f"""
                    ['spinner', 'worker-indicator', 'alert-indicator', 'waiting-indicator']
                      .filter(k => !document.getElementById(k + '-{live_parent}').classList.contains('hidden'))
                """
                await cdp.send("Page.navigate", {"url": f"{base}/?session={live_parent}"}, session_id=session_id)
                await wait_for(cdp, session_id, "document.querySelectorAll('#session-list .session-name').length >= 1")
                await expand_to(cdp, session_id, [ids["root"], live_parent])
                await reveal_row(cdp, session_id, live)
                await wait_for(cdp, session_id,
                               f"document.getElementById('spinner-{live}') && !document.getElementById('spinner-{live}').classList.contains('hidden')",
                               timeout=12, label="worker row spinner while its Run is live")
                shown_expanded = await evaluate(cdp, session_id, parent_icons)
                assert_true(shown_expanded == [], f"the expanded parent shows only its own (idle) state ({shown_expanded})")
                await wait_for(cdp, session_id, """
                    (() => {
                      const el = document.querySelector('.delegate-live-state[data-delegate-session]');
                      return el && el.textContent.includes('running') && el.textContent.includes('Scripted Live Runner');
                    })()
                """, timeout=12, label="Delegated card live state line (running · backend)")
                card_link = await evaluate(cdp, session_id, """
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
                await evaluate(cdp, session_id,
                               f"if (Sidebar.isTreeNodeExpanded('{live_parent}')) toggleTreeNode('{live_parent}')")
                await reveal_row(cdp, session_id, live_parent)
                await wait_for(cdp, session_id, f"JSON.stringify({parent_icons}) === '[\"worker-indicator\"]'",
                               timeout=12, label="the collapsed parent's gear stands in for the running worker")
                shot_collapsed = await screenshot(cdp, session_id, results, "s21a_parent_collapsed_gear")
                await expand_to(cdp, session_id, [live_parent])
                results.record("(a) parent sees the running worker (spinner, collapsed gear, live Delegated card)", ok=True,
                               detail="worker spinner visible with the parent expanded; the collapsed parent shows the gear "
                                      f"({shot_collapsed}); card shows 'running · Scripted Live Runner' and links the child",
                               screenshot=shot)

                # (c) the Delegated card opens the child.
                await evaluate(cdp, session_id,
                    "[...document.querySelectorAll('a[href^=\"/?session=\"]')].find(a => a.getAttribute('href') === '/?session=" + live + "').click()")
                await wait_for(cdp, session_id,
                               f"location.search === '?session={live}' && document.getElementById('header-session-name').textContent === 'Live worker'",
                               timeout=12, label="the card's link opened the child page")

                # (a) worker page, fresh mid-Run load: timer ticking, events
                # streaming, the Run's backend, the context reading.
                await wait_for(cdp, session_id,
                               "!document.getElementById('thinking').classList.contains('hidden')",
                               timeout=12, label="thinking timer visible on a fresh mid-Run load")
                t1 = await evaluate(cdp, session_id, "document.getElementById('thinking-time').textContent")
                await asyncio.sleep(1.6)
                t2 = await evaluate(cdp, session_id, "document.getElementById('thinking-time').textContent")
                assert_true(t1 != t2 and t2.endswith('s'), f"the header timer ticks ({t1} -> {t2})")
                badge = await evaluate(cdp, session_id, """
                    (() => {
                      const b = document.getElementById('backend-badge');
                      if (!b) return null;
                      const sel = b.querySelector('select');
                      return sel ? (sel.selectedOptions[0] ? sel.selectedOptions[0].text : null) : b.textContent;
                    })()
                """)
                assert_true(badge == "Scripted Live Runner", f"the header badge shows the Run's backend ({badge})")
                await wait_for(cdp, session_id, """
                    (() => {
                      const ind = document.getElementById('usage-indicator');
                      const bar = document.getElementById('usage-bar');
                      return ind && !ind.classList.contains('hidden') && bar && parseFloat(bar.style.width) > 0;
                    })()
                """, timeout=12, label="the header context reading from the Run's last context_reading")
                before = await evaluate(cdp, session_id,
                    "document.getElementById('messages').textContent.includes('streamed progress line')")
                assert_true(before, "the streamed events render in the main chat")
                last_line = await evaluate(cdp, session_id, """
                    (() => {
                      const m = document.getElementById('messages').textContent.match(/streamed progress line (\\d+)/g);
                      return m ? Math.max(...m.map(s => parseInt(s.match(/\\d+$/)[0]))) : 0;
                    })()
                """)
                await wait_for(cdp, session_id, f"""
                    (() => {{
                      const m = document.getElementById('messages').textContent.match(/streamed progress line (\\d+)/g);
                      const top = m ? Math.max(...m.map(s => parseInt(s.match(/\\d+$/)[0]))) : 0;
                      return top >= {last_line} + 2;
                    }})()
                """, timeout=8, label="new events appear in the open chat within one poll")
                shot = await screenshot(cdp, session_id, results, "s21b_worker_running")
                results.record("(a) worker page mid-Run (timer, streaming, run backend, context reading)", ok=True,
                               detail=f"timer {t1}->{t2}; badge '{badge}'; context bar painted; streamed lines appear within ~3s", screenshot=shot)

                # (e) the stop control stops the active Run.
                mark = cdp.mutation_mark()
                await evaluate(cdp, session_id,
                    "document.querySelector('#thinking button').click()")
                await wait_for(cdp, session_id, "document.getElementById('thinking').classList.contains('hidden')",
                               timeout=12, label="the thinking timer clears after the stop")
                stop_calls = [m for m in cdp.mutations_since(mark) if m["url"].endswith(f"/runs/{live_run}/cancel")]
                assert_true(len(stop_calls) == 1, f"the stop posts exactly one run cancel ({stop_calls})")
                await wait_for(cdp, session_id,
                               f"document.getElementById('spinner-{live}') && document.getElementById('spinner-{live}').classList.contains('hidden')",
                               timeout=12, label="the worker row spinner clears after the stop")
                results.record("(e) the stop control stops the active Run", ok=True,
                               detail=f"one POST /runs/{live_run}/cancel; thinking timer and row spinner clear without a reload", screenshot=None)

                # (b) the task closes: the delivery summary and the four links
                # appear in the open chat without a reload. The fact rides the
                # SERVING owner's append funnel: the seeded tree's instances
                # warmed the app-side caches long ago, and an append through
                # them would land on disk without advancing the caches the
                # APIs read.
                from src.api.deps import task_manager as serving_tree
                serving = serving_tree()
                await serving.events.append(live, build_control_event(
                    ET.TASK_CLOSED, actor="worker", source_session_id=live,
                    request_id="harness-close", outcome="completed",
                    summary="the checked piece is delivered", result_refs=["evidence/harness.txt"]))
                await wait_for(cdp, session_id, """
                    (() => {
                      const banner = document.querySelector('[data-message-role="run_delivery"]');
                      if (!banner) return false;
                      const text = banner.textContent;
                      return text.includes('Delivered') && text.includes('the checked piece is delivered')
                        && ['Raw log', 'Events', 'Result', 'Diff'].every(l => text.includes(l));
                    })()
                """, timeout=12, label="the delivery close with the four evidence links")
                # Both cues checked where they would show: the worker row's own
                # spinner (the delivered worker may already have left the list —
                # a successful delivery auto-archives it), and the parent's gear
                # with the parent collapsed (the only state the stand-in paints).
                spinner_hidden = await evaluate(cdp, session_id, f"""
                    (() => {{
                      const el = document.getElementById('spinner-{live}');
                      return !el || el.classList.contains('hidden');
                    }})()
                """)
                await evaluate(cdp, session_id,
                               f"if (Sidebar.isTreeNodeExpanded('{live_parent}')) toggleTreeNode('{live_parent}')")
                await expand_to(cdp, session_id, [ids["root"]])
                await reveal_row(cdp, session_id, live_parent)
                parent_shown = await evaluate(cdp, session_id, parent_icons)
                assert_true(spinner_hidden and parent_shown == [],
                            f"the running-state cues clear once the Run finished (parent icons {parent_shown})")
                shot = await screenshot(cdp, session_id, results, "s21c_worker_delivered")
                results.record("(b) finished: cues clear, delivery summary and four links shown", ok=True,
                               detail="banner with summary + Raw log/Events/Result/Diff; gear and spinner cleared without a reload", screenshot=shot)
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
                await wait_for(cdp, session_id, "document.querySelectorAll('#session-list .session-name').length >= 1")
                row = await evaluate(cdp, session_id, f"""
                    (() => {{
                      const row = document.getElementById('session-{ids['legacy_thread']}');
                      if (!row) return null;
                      return {{onclick: row.getAttribute('onclick') || '', text: row.textContent.slice(0, 80)}};
                    }})()
                """)
                assert_true(row and "openThreadView" in row["onclick"],
                            f"the projected thread row navigates to the thread view ({row})")
                await evaluate(cdp, session_id,
                               f"document.getElementById('session-{ids['legacy_thread']}').click()")
                await wait_for(cdp, session_id,
                               f"location.search === '?session={ids['legacy']}&thread={ids['legacy_thread']}'",
                               timeout=12, label="the row landed on the thread URL")
                await wait_for(cdp, session_id,
                               "document.getElementById('header-session-name').textContent === 'Review: ## Goal'",
                               timeout=12, label="the header names the thread")
                await wait_for(cdp, session_id,
                               "document.getElementById('messages').textContent.includes('review verdict: approve')",
                               timeout=12, label="the thread events render in the main chat")
                hidden_input = await evaluate(cdp, session_id,
                    "document.getElementById('input-area').classList.contains('hidden')")
                assert_true(hidden_input, "the thread view takes no message input")
                shot = await screenshot(cdp, session_id, results, "s22_legacy_thread_view")
                results.record("(d) a legacy thread row opens in the main chat at the thread URL", ok=True,
                               detail="row click navigates to /?session=<parent>&thread=<id>; transcript renders read-only, no input", screenshot=shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s22_FAILED")
                results.record("(d) a legacy thread row opens in the main chat at the thread URL", ok=False, detail=repr(exc), screenshot=shot)

            # The CDP collector records console.error calls and uncaught page
            # exceptions from Runtime.enable onward — this list is the only
            # source; an assertion over an always-empty list is a faked pass.
            console_errors = [e for e in cdp.console_errors if "favicon" not in e]
            results.console_errors = console_errors
            results.record("console clean", len(console_errors) == 0,
                           f"{len(console_errors)} console errors" + (f": {console_errors[:3]}" if console_errors else ""),
                           None)

            await cdp.close()
        finally:
            # The live-run scenario's real process and its events grower stop
            # with the harness, whatever the scenarios concluded.
            handles = ids.get("_live_handles") or {}
            stop_event = handles.get("stop")
            if stop_event is not None:
                stop_event.set()
            live_process = handles.get("process")
            if live_process is not None and live_process.poll() is None:
                live_process.terminate()
                try:
                    live_process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    live_process.kill()
            chrome_proc.terminate()
            try:
                chrome_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                chrome_proc.kill()
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
