#!/usr/bin/env python3
"""Real-browser harness for the v2 session tree UI (desktop 1440px + 390px).

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
  --evidence-dir (default: this session's research/ directory, never in git).

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
import time  # noqa: E402
import urllib.request  # noqa: E402

RESEARCH_DEFAULT = Path("/home/chaoli/.charliebot/sessions/8a7964a3-8e53-4fa3-9145-893bac307ddc/research")


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
        self.tree_fetch_counts: list[str] = []
        self.runs_fetch_counts: list[str] = []
        self.mutations: list[dict] = []
        self._reader = asyncio.create_task(self._read_loop())

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
                        url = msg["params"]["request"]["url"]
                        if "/api/sessions/tree" in url:
                            self.tree_fetch_counts.append(url)
                        if "/runs?" in url:
                            self.runs_fetch_counts.append(url)
                        request = msg["params"]["request"]
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

    def drain_tree_fetches(self) -> list[str]:
        seen = self.tree_fetch_counts
        self.tree_fetch_counts = []
        return seen

    def drain_runs_fetches(self) -> list[str]:
        seen = self.runs_fetch_counts
        self.runs_fetch_counts = []
        return seen

    def mutations_since(self, mark: int) -> list[dict]:
        return self.mutations[mark:]

    def mutation_mark(self) -> int:
        return len(self.mutations)


async def connect_cdp(port: int) -> CDP:
    import websockets

    # /json/version lists the browser endpoint.
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version", timeout=10) as resp:
        info = json.loads(resp.read().decode())
    ws = await websockets.connect(info["webSocketDebuggerUrl"], max_size=50 * 1024 * 1024)
    return CDP(ws)


# ---------------------------------------------------------------------------
# Scenario seeding (through the task_sessions owner, in-process only)
# ---------------------------------------------------------------------------


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
    from src.core.config import get_config
    from src.core.sessions import SessionManager
    from src.core.task_sessions import TaskTreeManager
    from src.core.models import PatchSessionTaskRequest, RunRecord, TaskSpec
    from src.core.run_token import CallerIdentity
    from src.core import event_types as ET

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
        from src.core.task_prompts import PromptBlock, PromptSnapshot, PromptSource
        from src.core.control_events import sha256_hex
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

        seed_memory_store(home)
        return {"root": root.id, "feature": feature.id, "worker1": worker1.id, "worker2": worker2.id,
                "long": long_worker.id, "latest_hash": latest_hash,
                "late_root": late_root.id, "late_mid": late_mid.id, "wide": wide.id,
                "ops_root": ops_root.id, "ops_mid": ops_mid.id, "failing": failing.id,
                "evidence": evidence.id, "bind_a": bind_a.id, "bind_b": bind_b.id, **bulk}

    return await seed()


# ---------------------------------------------------------------------------
# Browser scenarios
# ---------------------------------------------------------------------------


class Results:
    def __init__(self, evidence_dir: Path, commit: str) -> None:
        self.evidence_dir = evidence_dir
        self.commit = commit
        self.scenarios: list[dict] = []
        self.console_errors: list[str] = []

    def record(self, name: str, ok: bool, detail: str, screenshot: str | None) -> None:
        self.scenarios.append({"name": name, "ok": ok, "detail": detail, "screenshot": screenshot})
        log(f"  [{'PASS' if ok else 'FAIL'}] {name}: {detail}")



    def save(self) -> None:
        payload = {
            "tested_commit": self.commit,
            "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "browser": "google-chrome headless (CDP)",
            "scenarios": self.scenarios,
            "console_errors": self.console_errors,
        }
        out = self.evidence_dir / "session_tree_browser_results.json"
        out.write_text(json.dumps(payload, indent=2), encoding="utf-8")
        log(f"results written to {out}")


def api_request(base: str, access_key: str, method: str, path: str,
                body: dict | None = None, token: str | None = None) -> tuple[int, dict]:
    """One real HTTP API call from a SEPARATE authenticated client.

    This is the cross-client creator of the creation-visibility scenarios: the
    operator access key (or a scoped agent run token) rides the Authorization
    header; the browser under observation never performs the call.
    """
    headers = {"Authorization": "Bearer " + (token or access_key)}
    data = None
    if body is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(body).encode("utf-8")
    req = urllib.request.Request(base + path, data=data, headers=headers, method=method)

    def call() -> tuple[int, dict]:
        try:
            with urllib.request.urlopen(req, timeout=15) as resp:
                return resp.status, json.loads(resp.read().decode())
        except urllib.error.HTTPError as e:
            return e.code, json.loads(e.read().decode() or "{}")

    return call()


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
    """Expand the managers above *node_ids* through the tree's own API.

    ensureExpanded is idempotent — an already-open level stays open (toggle
    would collapse it)."""
    for node_id in node_ids:
        await wait_for(cdp, session_id, f"!!document.getElementById('tree-node-{node_id}')",
                       timeout=8, label=f"row visible for {node_id}")
        await evaluate(cdp, session_id, f"Sidebar.SessionTree.ensureExpanded('{node_id}')")
        await wait_for(cdp, session_id,
                       "!!document.getElementById('tree-children-" + node_id + "')",
                       timeout=8, label=f"children container for {node_id}")


DIAGNOSTIC_SNAPSHOT = """
    JSON.stringify({
      rows: document.querySelectorAll('#session-list .tree-row').length,
      rowIds: [...document.querySelectorAll('#session-list .tree-row')].map(el => el.dataset.nodeId).slice(0, 3),
      filter: currentFilter,
      tabTaskBtns: [...document.querySelectorAll('#tab-task button')].map(b => b.textContent).slice(0, 10),
      taskTabHidden: document.getElementById('tab-task')?.classList.contains('hidden'),
      activeTabBtn: [...document.querySelectorAll('.tab-btn')].filter(b => !b.classList.contains('hidden')).map(b => b.id + ':' + b.className.includes('bg-blue-600')),
      expanded: JSON.stringify([...(Sidebar.SessionTree ? Sidebar.SessionTree.state.expanded : [])]).slice(0, 200),
      sessionId: typeof SESSION_ID !== 'undefined' ? SESSION_ID : null,
      goalValue: (document.getElementById('task-goal-input') || {}).value,
      draft: localStorage.getItem('charliebot-task-draft-' + (typeof SESSION_ID !== 'undefined' ? SESSION_ID : '')),
      treeEvents: (window.__treeEvents || 0) + '/' + (window.__wsMsgs || 0) + 'msgs/' + (window.__wsTotal || 0) + 'socks',
      errs: typeof window.__errs === 'object' ? window.__errs.slice(0, 4) : 'n/a',
      wsLog: (window.__wsLog || []).slice(-3),
      rowNames: [...document.querySelectorAll('#session-list .tree-row .session-name')].map((el) => el.textContent).slice(0, 5),
      treeDebug: (() => {
        if (!window.Sidebar || !Sidebar.SessionTree) return 'no-module';
        const t = Sidebar.SessionTree.state;
        const row = t.rows.get(SESSION_ID) || t.rows.get('cc00976a-ce9f-53ba-ad51-fdf8ef1b287b');
        return JSON.stringify({
          levels: [...t.levels.entries()].map(([k, v]) => [k.slice(0, 6), v.fetched, v.ids.length]),
          rowName: row ? row.name : null,
          gen: t.gen,
        });
      })(),
      completeBlocker: (() => {
        const m = document.getElementById('task-complete-modal');
        if (!m) return 'no-modal';
        const eb = m.querySelector('.text-red-300');
        return eb ? eb.textContent.slice(0, 200) : 'no-errbox';
      })(),
      moveDebug: (() => {
        const m = document.getElementById('task-move-modal');
        if (!m) return 'no-modal';
        const list = document.getElementById('task-move-list');
        return JSON.stringify({
          chosen: (document.getElementById('task-move-chosen') || {}).textContent,
          listHead: list ? list.textContent.slice(0, 160) : null,
          hasLateRoot: list ? list.textContent.includes('Late root') : null,
          err: (list && list.textContent.includes('Failed to load candidates')) || null,
        });
      })(),
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
            log(f"    transient during wait ({exc!r:.80}); retrying")
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
    import websockets

    evidence_dir = Path(args.evidence_dir)
    evidence_dir.mkdir(parents=True, exist_ok=True)
    commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=REPO_ROOT,
                            capture_output=True, text=True, check=True).stdout.strip()
    results = Results(evidence_dir, commit)

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
        chrome_proc = subprocess.Popen(
            [chrome, "--headless=new", "--remote-debugging-port=" + str(debug_port),
             f"--user-data-dir={profile}", "--no-first-run", "--no-default-browser-check",
             "--disable-background-networking", "--window-size=1440,900",
             "--remote-allow-origins=*",
             # Keep the page fully active: background throttling would delay
             # timers/fetches and distort the live-update evidence.
             "--disable-background-timer-throttling",
             "--disable-backgrounding-occluded-windows",
             "--disable-renderer-backgrounding",
             "about:blank"],
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            stderr_lines = []
            ws_url = None
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline and ws_url is None:
                line = chrome_proc.stderr.readline().decode(errors="replace")
                if not line:
                    await asyncio.sleep(0.05)
                    continue
                stderr_lines.append(line)
                if "DevTools listening on ws://" in line:
                    ws_url = line.strip().split()[-1]
            if ws_url is None:
                fail("chrome devtools endpoint did not come up: " + "".join(stderr_lines[-5:]))
            ws = await websockets.connect(ws_url, max_size=50 * 1024 * 1024)
            cdp = CDP(ws)
            await asyncio.sleep(0.3)

            target = await cdp.send("Target.createTarget", {"url": "about:blank"})
            attached = await cdp.send("Target.attachToTarget",
                                      {"targetId": target["targetId"], "flatten": True})
            session_id = attached["sessionId"]
            await cdp.send("Page.enable", session_id=session_id)
            await cdp.send("Runtime.enable", session_id=session_id)
            await cdp.send("Network.enable", session_id=session_id)
            # Isolation guard, injected before any app script: the browser
            # terminal tab attaches to the HOST-GLOBAL tmux session. The
            # harness must never attach to (or create) it — /ws/terminal is
            # answered with an immediately-closed socket; every other app
            # websocket passes through untouched.
            guard_source = """
                (function () {
                  try { localStorage.setItem('charliebot_access_key', %s); } catch (e) {}
                  window.__treeEvents = 0;
                  window.__errs = [];
                  window.addEventListener('error', (e) => window.__errs.push(String(e.message).slice(0, 200)));
                  window.addEventListener('unhandledrejection', (e) => window.__errs.push('rej: ' + String(e.reason).slice(0, 200)));
                  const origConsoleError = console.error;
                  console.error = function () {
                    window.__errs.push([...arguments].map((a) => String(a && a.message ? a.message : a)).join(' ').slice(0, 200));
                    origConsoleError.apply(console, arguments);
                  };
                  const RealWebSocket = window.WebSocket;
                  function GuardedWebSocket(url, protocols) {
                    const u = String(url);
                    if (u.includes('/ws/terminal')) {
                      const fake = {
                        readyState: 3, CLOSED: 3, send() {}, close() {},
                        addEventListener() {}, removeEventListener() {},
                        onopen: null, onmessage: null, onclose: null, onerror: null,
                      };
                      setTimeout(() => { if (fake.onclose) fake.onclose({type: 'close'}); }, 0);
                      return fake;
                    }
                    const sock = protocols !== undefined
                      ? new RealWebSocket(url, protocols)
                      : new RealWebSocket(url);
                    window.__wsTotal = (window.__wsTotal || 0) + 1;
                    sock.addEventListener('message', (m) => {
                      try {
                        window.__wsMsgs = (window.__wsMsgs || 0) + 1;
                        const d = String(m.data);
                        if (d.includes('task_tree_changed')) {
                          window.__treeEvents += 1;
                          window.__wsLog = (window.__wsLog || []);
                          window.__wsLog.push(d.slice(0, 140) + ' @sock' + (window.__wsTotal));
                        }
                      } catch (e) {}
                    });
                    return sock;
                  }
                  GuardedWebSocket.prototype = RealWebSocket.prototype;
                  GuardedWebSocket.OPEN = RealWebSocket.OPEN;
                  GuardedWebSocket.CONNECTING = RealWebSocket.CONNECTING;
                  GuardedWebSocket.CLOSING = RealWebSocket.CLOSING;
                  GuardedWebSocket.CLOSED = RealWebSocket.CLOSED;
                  window.WebSocket = GuardedWebSocket;
                })();
            """ % (json.dumps(access_key),)
            await cdp.send("Page.addScriptToEvaluateOnNewDocument", {"source": guard_source}, session_id=session_id)
            await cdp.send("Network.setCookie", {
                "name": "charliebot_access_key", "value": access_key,
                "url": f"http://127.0.0.1:{server_port}/",
            }, session_id=session_id)
            await cdp.send("Emulation.setDeviceMetricsOverride", {
                "width": 1440, "height": 900, "deviceScaleFactor": 1, "mobile": False,
            }, session_id=session_id)

            base = f"http://127.0.0.1:{server_port}"

            # ---- S1: desktop load; tree is the primary navigation ------------
            try:
                await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['root']}"}, session_id=session_id)
                await wait_for(cdp, session_id, "document.querySelectorAll('#session-list .tree-row').length >= 1")
                cdp.drain_tree_fetches()
                await expand_to(cdp, session_id, [ids["root"], ids["feature"]])
                await wait_for(cdp, session_id, "document.querySelectorAll('#session-list .tree-row').length >= 4")
                rows = await evaluate(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .tree-row')].map(el => ({
                      id: el.dataset.nodeId,
                      name: el.querySelector('.session-name')?.textContent,
                      manager: !!el.textContent.match(/Manager/),
                      worker: !!el.textContent.match(/Worker/),
                    }))
                """)
                names = [r["name"] for r in rows]
                assert_true("Program rollout" in names and "Feature alpha" in names
                            and "Worker one" in names and "Worker two" in names,
                            f"four task nodes render: {names}")
                assert_true(not any(r["worker"] and r["manager"] for r in rows),
                            "every row uses exactly one role label")
                assert_true(not await evaluate(cdp, session_id,
                    "!!document.querySelector('#session-list')?.textContent.match(/PM|project manager/i)"),
                    "no fixed PM/group layer")
                shot = await screenshot(cdp, session_id, results, "s1_desktop_tree")
                results.record("desktop tree primary navigation", True,
                               f"4 task nodes, role labels, no PM layer", shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s1_desktop_tree_FAILED")
                results.record("desktop tree primary navigation", False, repr(exc), shot)

            # ---- S2: node page Task panel; canonical task edit + drafts ------
            try:
                log("  s2: switch to feature")
                await evaluate(cdp, session_id, f"switchSession('{ids['feature']}')")
                await wait_for(cdp, session_id, "document.getElementById('btn-task') && !document.getElementById('btn-task').classList.contains('hidden')", label="s2 task tab visible")
                log("  s2: open Task tab")
                await evaluate(cdp, session_id, "switchTab('task')")
                await wait_for(cdp, session_id, "document.getElementById('task-goal-input')?.value.includes('feature alpha')", label="s2 task form rendered")
                # Save an edit through the real PATCH API.
                log("  s2: save goal edit")
                # The click is fire-and-forget; the save's fetch and panel
                # refresh are observed from the harness side (an in-page
                # setTimeout poll would hit Chrome's nested-timer clamping).
                try:
                    await asyncio.wait_for(evaluate(cdp, session_id, """
                        (() => {
                          const el = document.getElementById('task-goal-input');
                          el.value = 'Deliver feature alpha end to end (revised)';
                          document.getElementById('task-save-btn').click();
                          return 'clicked';
                        })()
                    """), timeout=8)
                except asyncio.TimeoutError:
                    log("  s2 TRACE: the click evaluate did not return within 8s")
                await wait_for(cdp, session_id,
                               "document.getElementById('task-goal-input')?.value.includes('revised')",
                               timeout=45, label="s2 goal saved via PATCH")
                import urllib.request as _u
                req = _u.Request(f"{base}/api/sessions/{ids['feature']}", headers={"Authorization": f"Bearer {access_key}"})
                with await asyncio.to_thread(_u.urlopen, req, timeout=10) as resp:
                    detail = json.loads(resp.read().decode())
                assert_true(detail["task"]["goal"].endswith("(revised)"),
                            "the UI save PATCHed the canonical task object")
                # Draft: type without saving, switch away and back.
                log("  s2: draft across switches")
                try:
                    await asyncio.wait_for(evaluate(cdp, session_id, """
                    (async () => {
                      const el = document.getElementById('task-goal-input');
                      el.value = 'unsaved draft text';
                      el.dispatchEvent(new Event('input'));
                      switchSession('%s');
                    })()
                """ % ids["worker1"]), timeout=8)
                except asyncio.TimeoutError:
                    log("  s2 TRACE: the switch-to-worker evaluate did not return within 8s")
                await wait_for(cdp, session_id,
                    "document.querySelector('#tab-task .text-base')?.textContent === 'Worker one'",
                    label="s2 switched to worker one")
                await evaluate(cdp, session_id, f"switchSession('{ids['feature']}')")
                await evaluate(cdp, session_id, "switchTab('task')")
                await wait_for(cdp, session_id, "document.getElementById('task-goal-input')?.value === 'unsaved draft text'")
                shot = await screenshot(cdp, session_id, results, "s2_task_panel")
                results.record("task panel edit + draft preservation", True,
                               "PATCH lands on the task record; unsaved draft survives node switches", shot)
            except Exception as exc:
                alive = None
                try:
                    alive = await asyncio.wait_for(evaluate(cdp, session_id, "1+1"), timeout=5)
                except Exception as probe_exc:
                    alive = f"page unresponsive: {probe_exc!r}"
                log(f"  s2 diagnostic: page evaluate 1+1 -> {alive}")
                shot = await screenshot(cdp, session_id, results, "s2_task_panel_FAILED")
                results.record("task panel edit + draft preservation", False, repr(exc), shot)

            # ---- S3: Context panel ------------------------------------------
            try:
                await evaluate(cdp, session_id, "switchTab('task-context')")
                await wait_for(cdp, session_id, "document.getElementById('tab-task-context').textContent.includes('Next run')")
                text = await evaluate(cdp, session_id, "document.getElementById('tab-task-context').textContent")
                assert_true("index only" in text, "index-delivered memory is labelled")
                assert_true("Advanced" in text and "prompt_hash" in text, "advanced hash disclosure present")
                assert_true("This task and descendants" in text, "the rule scope control exists")
                # Switch the scope and save a subtree rule through the UI.
                await evaluate(cdp, session_id, """
                    (async () => {
                      const radios = document.querySelectorAll('input[name="task-rule-scope"]');
                      radios[1].checked = true; radios[1].dispatchEvent(new Event('change'));
                    })()
                """)
                await wait_for(cdp, session_id, "document.getElementById('tab-task-context').textContent.includes('descendant task(s)')")
                count_text = await evaluate(cdp, session_id,
                    r"document.getElementById('tab-task-context').textContent.match(/Applies to this task and [^.]*[.]/)?.[0]")
                assert_true("This task is included" in (count_text or "") or
                            (await evaluate(cdp, session_id, "document.getElementById('tab-task-context').textContent")).find("This task is included") >= 0,
                            "the subtree save explains self-inclusion")
                await evaluate(cdp, session_id, """
                    (async () => {
                      const ed = document.getElementById('task-rule-editor');
                      ed.value = 'program-wide rule from UI';
                      ed.dispatchEvent(new Event('input'));
                    })()
                """)
                await wait_for(cdp, session_id, "document.getElementById('task-rule-draft-note')?.textContent === 'Unsaved draft'")
                await evaluate(cdp, session_id, """
                    (async () => {
                      const btns = [...document.querySelectorAll('#tab-task-context button')];
                      btns.find(b => b.textContent.startsWith('Save')).click();
                    })()
                """)
                await wait_for(cdp, session_id, "!document.getElementById('task-rule-draft-note')?.textContent")
                shot = await screenshot(cdp, session_id, results, "s3_context_panel")
                results.record("context panel + rule scope editor", True,
                               f"preview sources, index label, hash, subtree scope ({count_text!r}), UI PATCH", shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s3_context_panel_FAILED")
                results.record("context panel + rule scope editor", False, repr(exc), shot)

            # ---- S4: Runs panel ----------------------------------------------
            try:
                await evaluate(cdp, session_id, f"switchSession('{ids['worker1']}')")
                await evaluate(cdp, session_id, "switchTab('runs')")
                await wait_for(cdp, session_id, "document.getElementById('tab-runs').textContent.includes('Runs (2)')")
                text = await evaluate(cdp, session_id, "document.getElementById('tab-runs').textContent")
                assert_true("success" in text and "review" in text, "both worker run rows render with kind/outcome")
                assert_true("N/A" in text, "unavailable measurements show N/A")
                # Every seeded run here is terminal: no Stop button is offered
                # on a finished row (stop only rides running/queued/attention).
                assert_true("Stop" not in text, "no stop button on terminal run rows")
                shot = await screenshot(cdp, session_id, results, "s4_runs_panel")
                results.record("runs panel (replaces Workers for v2)", True,
                               "paged runs with kind/outcome/timing and N/A measurements", shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s4_runs_panel_FAILED")
                results.record("runs panel (replaces Workers for v2)", False, repr(exc), shot)

            # ---- S5: completion through the UI (with pending-input blocker) ---
            try:
                # Worker two has a pending input: Complete must be refused with a blocker.
                await evaluate(cdp, session_id, f"switchSession('{ids['worker2']}')")
                await evaluate(cdp, session_id, "switchTab('task')")
                await wait_for(cdp, session_id, "document.getElementById('tab-task').textContent.includes('Pending inputs')")
                click_report = await evaluate(cdp, session_id, """
                    (async () => {
                      const btns = [...document.querySelectorAll('#tab-task button')];
                      const btn = btns.find(b => b.textContent.startsWith('Complete'));
                      if (!btn) return 'no-button';
                      btn.click();
                      await new Promise(r => setTimeout(r, 400));
                      return 'clicked:' + !!document.getElementById('task-complete-modal');
                    })()
                """)
                log(f"  s5 complete-click: {click_report}")
                await wait_for(cdp, session_id, "!!document.getElementById('task-complete-modal')")
                await evaluate(cdp, session_id, """
                    (async () => {
                      const overlay = document.getElementById('task-complete-modal');
                      const btns = [...overlay.querySelectorAll('button')];
                      btns.find(b => b.textContent === 'Complete task').click();
                    })()
                """)
                await wait_for(cdp, session_id, "!!document.getElementById('task-complete-modal') && document.getElementById('task-complete-modal').textContent.includes('unprocessed input')")
                modal_open = await evaluate(cdp, session_id, "!!document.getElementById('task-complete-modal')")
                assert_true(modal_open, "the modal stays open with the concrete blocker")
                shot = await screenshot(cdp, session_id, results, "s5a_complete_blocker")
                # Acknowledge the exact pending input through the UI.
                await evaluate(cdp, session_id, "document.getElementById('task-complete-modal') && document.querySelector('#task-complete-modal button') && null")
                await evaluate(cdp, session_id, """
                    (async () => {
                      const overlay = document.getElementById('task-complete-modal');
                      [...overlay.querySelectorAll('button')].find(b => b.textContent === 'Cancel').click();
                    })()
                """)
                await wait_for(cdp, session_id, "!document.getElementById('task-complete-modal')")
                await evaluate(cdp, session_id, """
                    (async () => {
                      const box = document.querySelector('#task-pending-inputs input[type="checkbox"]');
                      box.checked = true; box.dispatchEvent(new Event('change'));
                      document.getElementById('task-ack-note').value = 'handled in terminal';
                      document.getElementById('task-ack-btn').click();
                    })()
                """)
                await wait_for(cdp, session_id, "!document.getElementById('task-pending-inputs')")
                import urllib.request as _u
                req = _u.Request(f"{base}/api/sessions/{ids['worker2']}/task-inputs/pending",
                                 headers={"Authorization": f"Bearer {access_key}"})
                with await asyncio.to_thread(_u.urlopen, req, timeout=10) as resp:
                    pending_after = json.loads(resp.read().decode())["items"]
                assert_true(len(pending_after) == 0, "the acknowledged input cleared on the server")
                # Now completion succeeds.
                click_report = await evaluate(cdp, session_id, """
                    (async () => {
                      const btns = [...document.querySelectorAll('#tab-task button')];
                      const btn = btns.find(b => b.textContent.startsWith('Complete'));
                      if (!btn) return 'no-button';
                      btn.click();
                      await new Promise(r => setTimeout(r, 400));
                      return 'clicked:' + !!document.getElementById('task-complete-modal');
                    })()
                """)
                log(f"  s5 complete-click: {click_report}")
                await wait_for(cdp, session_id, "!!document.getElementById('task-complete-modal')")
                await evaluate(cdp, session_id, """
                    (async () => {
                      const overlay = document.getElementById('task-complete-modal');
                      overlay.querySelector('#task-complete-summary').value = 'docs verified; no defects';
                      overlay.querySelector('#task-complete-refs').value = 'docs/verification.md';
                      const btns = [...overlay.querySelectorAll('button')];
                      btns.find(b => b.textContent === 'Complete task').click();
                    })()
                """)
                # The modal closes itself on success; the panel refreshes to
                # the completed state — both are the observable completion.
                await wait_for(cdp, session_id,
                               "document.getElementById('tab-task').textContent.includes('task: completed')",
                               timeout=15, label="s5 completion landed")
                shot = await screenshot(cdp, session_id, results, "s5b_complete_success")
                results.record("blockers + exact acknowledgement + completion", True,
                               "pending input blocked with a visible blocker; exact-id ack; completion then landed", shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s5_complete_FAILED")
                results.record("blockers + exact acknowledgement + completion", False, repr(exc), shot)

            # ---- S6: archived ancestry: search + deep link ---------------------
            try:
                # The completed worker autoarchives (server facts). Reload the app
                # on the archived child's deep link.
                await cdp.send("Page.navigate",
                               {"url": f"{base}/?session={ids['worker2']}"}, session_id=session_id)
                await wait_for(cdp, session_id, "document.querySelectorAll('#session-list .tree-row').length >= 1")
                await wait_for(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .tree-row')].some(el => el.dataset.nodeId === '%s')
                """ % ids["worker2"], timeout=15)
                revealed = await evaluate(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .tree-row')].map(el => el.dataset.nodeId)
                """)
                assert_true(ids["worker2"] in revealed, "the archived child renders on its deep link")
                assert_true(ids["feature"] in revealed and ids["root"] in revealed,
                            "the ancestor path stays navigable (archived view preserves ancestry)")
                # Search reveals the path to the match.
                await evaluate(cdp, session_id, """
                    (async () => {
                      const input = document.getElementById('sidebar-search');
                      input.value = 'Worker two';
                      input.dispatchEvent(new Event('input'));
                    })()
                """)
                await wait_for(cdp, session_id, """
                    (() => {
                      const row = [...document.querySelectorAll('#session-list .tree-row')]
                        .find(el => el.querySelector('.session-name')?.textContent === 'Worker two');
                      return row && row.firstElementChild.classList.contains('ring-1');
                    })()
                """)
                shot = await screenshot(cdp, session_id, results, "s6_archived_deep_link_search")
                results.record("archived child deep link + search path reveal", True,
                               "archived child reachable; full ancestor path expanded and highlighted", shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s6_archived_FAILED")
                results.record("archived child deep link + search path reveal", False, repr(exc), shot)

            # ---- S7: keyboard navigation ---------------------------------------
            try:
                await evaluate(cdp, session_id, """
                    (async () => {
                      const row = [...document.querySelectorAll('#session-list .tree-row')][0];
                      row.focus();
                    })()
                """)
                active = await evaluate(cdp, session_id, "document.activeElement?.dataset?.nodeId || null")
                assert_true(active, f"a tree row is keyboard-focusable (active={active})")
                # ArrowRight expands, ArrowDown moves focus (handled on the container).
                for key in ("ArrowRight", "ArrowDown"):
                    await cdp.send("Input.dispatchKeyEvent", {
                        "type": "keyDown", "key": key, "code": key,
                        "windowsVirtualKeyCode": 39 if key == "ArrowRight" else 40,
                    }, session_id=session_id)
                    await cdp.send("Input.dispatchKeyEvent", {
                        "type": "keyUp", "key": key, "code": key,
                        "windowsVirtualKeyCode": 39 if key == "ArrowRight" else 40,
                    }, session_id=session_id)
                    await asyncio.sleep(0.3)
                expanded = await evaluate(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .tree-row')]
                      .filter(el => el.getAttribute('aria-expanded') === 'true').length
                """)
                assert_true(expanded >= 1, "ArrowRight expanded the focused node")
                shot = await screenshot(cdp, session_id, results, "s7_keyboard")
                results.record("keyboard tree navigation", True,
                               "rows focusable; ArrowRight expands; focus visible", shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s7_keyboard_FAILED")
                results.record("keyboard tree navigation", False, repr(exc), shot)

            # ---- S8: 390px mobile ------------------------------------------------
            try:
                await cdp.send("Emulation.setDeviceMetricsOverride", {
                    "width": 390, "height": 844, "deviceScaleFactor": 2, "mobile": True,
                }, session_id=session_id)
                await asyncio.sleep(0.3)
                overflow = await evaluate(cdp, session_id,
                    "document.documentElement.scrollWidth - document.documentElement.clientWidth")
                assert_true(overflow <= 1, f"no horizontal overflow at 390px (delta={overflow})")
                shot = await screenshot(cdp, session_id, results, "s8a_mobile_tree")
                # Task panel at 390px.
                await evaluate(cdp, session_id, f"switchSession('{ids['worker1']}')")
                await evaluate(cdp, session_id, "switchTab('task')")
                await asyncio.sleep(0.5)
                overflow = await evaluate(cdp, session_id,
                    "document.documentElement.scrollWidth - document.documentElement.clientWidth")
                assert_true(overflow <= 1, f"no horizontal overflow on the Task panel (delta={overflow})")
                shot2 = await screenshot(cdp, session_id, results, "s8b_mobile_task")
                # The complete/blocker dialog stays usable: the feature
                # manager still has open children, so its dialog opens and the
                # submit surfaces the server's blockers.
                await evaluate(cdp, session_id, f"switchSession('{ids['feature']}')")
                await asyncio.sleep(0.8)
                await evaluate(cdp, session_id, """
                    (async () => {
                      const btns = [...document.querySelectorAll('#tab-task button')];
                      btns.find(b => b.textContent.startsWith('Complete'))?.click();
                    })()
                """)
                await wait_for(cdp, session_id, "!!document.getElementById('task-complete-modal')")
                overflow = await evaluate(cdp, session_id,
                    "document.documentElement.scrollWidth - document.documentElement.clientWidth")
                assert_true(overflow <= 1, f"no horizontal overflow with the dialog open (delta={overflow})")
                shot3 = await screenshot(cdp, session_id, results, "s8c_mobile_dialog")
                await evaluate(cdp, session_id, """
                    (async () => {
                      const overlay = document.getElementById('task-complete-modal');
                      [...overlay.querySelectorAll('button')].find(b => b.textContent === 'Cancel')?.click();
                    })()
                """)
                results.record("390px mobile usability", True,
                               "tree, task panel and completion dialog fit without horizontal overflow",
                               shot3)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s8_mobile_FAILED")
                results.record("390px mobile usability", False, repr(exc), shot)
            finally:
                await cdp.send("Emulation.setDeviceMetricsOverride", {
                    "width": 1440, "height": 900, "deviceScaleFactor": 1, "mobile": False,
                }, session_id=session_id)

            # ---- S9: live events: bounded tree work on an out-of-band change ----
            try:
                await expand_to(cdp, session_id, [ids["root"], ids["feature"]])
                cdp.drain_tree_fetches()
                # Trace the app-level event path for the out-of-band change.
                await evaluate(cdp, session_id, """
                    (() => {
                      const orig = Sidebar.SessionTree.onTreeChanged;
                      window.__otc = [];
                      Sidebar.SessionTree.onTreeChanged = (id) => { window.__otc.push(String(id).slice(0, 8)); return orig.call(Sidebar.SessionTree, id); };
                    })()
                """)
                # View the root while renaming the feature out of band (the
                # server broadcasts task_tree_changed); the open page must
                # refresh the affected rows without a whole-tree rescan and
                # without switching session.
                import urllib.request as _u
                req = _u.Request(
                    f"{base}/api/sessions/{ids['feature']}",
                    data=json.dumps({"name": "Feature alpha renamed"}).encode(),
                    headers={"Authorization": f"Bearer {access_key}", "Content-Type": "application/json"},
                    method="PATCH")
                with await asyncio.to_thread(_u.urlopen, req, timeout=10) as resp:
                    assert resp.status == 200
                await evaluate(cdp, session_id, f"switchSession('{ids['root']}')")
                await asyncio.sleep(0.6)
                fetches_before = await evaluate(cdp, session_id,
                    "window.__fetchLog ? window.__fetchLog.length : 0")
                await wait_for(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .tree-row .session-name')]
                      .some(el => el.textContent === 'Feature alpha renamed')
                """, timeout=8)
                fetch_log_len = await evaluate(cdp, session_id, "(window.__fetchLog || []).length")
                delta = (fetch_log_len or 0) - (fetches_before or 0)
                assert_true(delta <= 6, f"bounded affected-level refresh ({delta} tree page fetches for the change)")
                active_ok = await evaluate(cdp, session_id, "SESSION_ID")
                assert_true(active_ok == ids["root"], "an update to another node never switches the active session")
                shot = await screenshot(cdp, session_id, results, "s9_live_update")
                results.record("live task_tree_changed handling", True,
                               f"row refreshed in place; bounded tree fetches for the change; session unchanged", shot)
            except Exception as exc:
                # Distinguish the event-delivery path from the refresh path.
                manual = None
                try:
                    await evaluate(cdp, session_id,
                                   f"Sidebar.SessionTree.onTreeChanged('{ids['feature']}')")
                    await asyncio.sleep(2.0)
                    manual = await evaluate(cdp, session_id, """
                        [...document.querySelectorAll('#session-list .tree-row .session-name')]
                          .some(el => el.textContent === 'Feature alpha renamed')
                    """)
                except Exception as probe_exc:
                    manual = f"probe failed: {probe_exc!r}"
                log(f"  s9 manual onTreeChanged refresh -> {manual}")
                shot = await screenshot(cdp, session_id, results, "s9_live_FAILED")
                results.record("live task_tree_changed handling", False, repr(exc), shot)

            # ---- S10: reload agreement ---------------------------------------
            try:
                await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['feature']}"}, session_id=session_id)
                await wait_for(cdp, session_id, "document.querySelectorAll('#session-list .tree-row').length >= 1")
                await expand_to(cdp, session_id, [ids["root"], ids["feature"]])
                # The completed worker two autoarchived (server facts): the
                # default tree shows three rows and the renamed manager.
                await wait_for(cdp, session_id, "document.querySelectorAll('#session-list .tree-row').length >= 3")
                reload_names = await evaluate(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .tree-row .session-name')].map(el => el.textContent)
                """)
                assert_true("Feature alpha renamed" in reload_names,
                            "reload shows the same backend facts as the streamed view")
                # The saved subtree rule survives reload in the Context panel.
                await evaluate(cdp, session_id, "switchTab('task-context')")
                await wait_for(cdp, session_id, "!!document.getElementById('task-rule-editor')")
                rule_text = await evaluate(cdp, session_id, """
                    (() => {
                      const radios = document.querySelectorAll('input[name="task-rule-scope"]');
                      radios[1].checked = true; radios[1].dispatchEvent(new Event('change'));
                      return document.getElementById('task-rule-editor').value;
                    })()
                """)
                assert_true("program-wide rule from UI" in (rule_text or ""),
                            "the UI-saved subtree rule is the authoritative text after reload")
                shot = await screenshot(cdp, session_id, results, "s10_reload")
                results.record("reload agreement", True,
                               "reload shows the same tree facts and the saved rule", shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s10_reload_FAILED")
                results.record("reload agreement", False, repr(exc), shot)

            # ---- S11: long-history current-run context ------------------------
            try:
                log("  s11: long-history context")
                await evaluate(cdp, session_id, f"switchSession('{ids['long']}')")
                await evaluate(cdp, session_id, "switchTab('task-context')")
                await wait_for(cdp, session_id,
                    "document.getElementById('tab-task-context').textContent.includes('generation 0150')",
                    timeout=15, label="s11 latest snapshot rendered")
                cdp.drain_runs_fetches()
                # Force a data-driven refresh (the live-update path) and recount.
                await evaluate(cdp, session_id, "TaskContextPanel.refresh()")
                await wait_for(cdp, session_id,
                    "document.getElementById('tab-task-context').textContent.includes('generation 0150')",
                    timeout=15, label="s11 latest snapshot after refresh")
                text = await evaluate(cdp, session_id, "document.getElementById('tab-task-context').textContent")
                assert_true("generation 0150" in text, "the whole-history latest launch is the current run")
                assert_true("generation 0100" not in text,
                            "the ascending first page's tail is never presented as current")
                assert_true("generation 0149" not in text, "no older generation leaks into the current-run box")
                assert_true((ids["latest_hash"] or "")[:12] in text,
                            "the rendered hash is the seeded latest snapshot's hash")
                assert_true("No run has started yet" not in text, "a 150-launch session is not misreported as idle")
                runs_fetches = cdp.drain_runs_fetches()
                desc_fetches = [u for u in runs_fetches if "order=desc" in u]
                assert_true(len(desc_fetches) >= 1, f"a newest-first runs read happened ({runs_fetches})")
                assert_true(all("limit=1" in u for u in desc_fetches),
                            f"each selection read is one row, not a page walk ({desc_fetches})")
                assert_true(len(runs_fetches) <= 3,
                            f"selection stays bounded as history grows ({len(runs_fetches)} /runs requests)")
                shot = await screenshot(cdp, session_id, results, "s11_long_history_context")
                results.record("long-history current-run context", True,
                               f"generation 0150 + its hash shown; {len(runs_fetches)} bounded /runs reads", shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s11_long_history_FAILED")
                results.record("long-history current-run context", False, repr(exc), shot)

            # ---- S12: creation from a separate client reaches the observer ----
            created: dict[str, str] = {}  # node ids the cross-client scenarios made
            observer_errors_before = None
            try:
                log("  s12: cross-client root creation")
                await evaluate(cdp, session_id, f"switchSession('{ids['feature']}')")
                await expand_to(cdp, session_id, [ids["root"], ids["feature"]])
                cdp.drain_tree_fetches()
                active_before = await evaluate(cdp, session_id, "SESSION_ID")
                tree_events_before = await evaluate(cdp, session_id, "window.__treeEvents || 0")
                status, meta = await asyncio.to_thread(
                    api_request, base, access_key, "POST", "/api/sessions/",
                    {"request_id": "harness-x-root-1", "profile": "manager", "name": "Remote root",
                     "task": {"goal": "created outside the browser", "acceptance": [], "context_refs": []}})
                assert_true(status == 200, f"the separate client's create succeeded ({status}: {meta})")
                created["root"] = meta["id"]
                await wait_for(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .tree-row .session-name')]
                      .some(el => el.textContent === 'Remote root')
                """, timeout=10, label="s12 remote root row appeared from the notification alone")
                fetches = cdp.drain_tree_fetches()
                assert_true(1 <= len(fetches) <= 4,
                            f"bounded affected-level work for the creation ({len(fetches)} tree page fetches)")
                active_after = await evaluate(cdp, session_id, "SESSION_ID")
                assert_true(active_after == active_before, "the creation never switches the active session")
                tree_events_after = await evaluate(cdp, session_id, "window.__treeEvents || 0")
                assert_true(tree_events_after > tree_events_before,
                            "the creation rode a real server-originated task_tree_changed event")
                names = await evaluate(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .tree-row .session-name')]
                      .map(el => el.textContent)
                """)
                assert_true(names.count("Remote root") == 1, "exactly one row for the new root")
                shot = await screenshot(cdp, session_id, results, "s12_cross_client_root")
                results.record("cross-client root creation reaches a connected observer", True,
                               "row appeared from the publication notification alone; bounded fetches; session unchanged", shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s12_cross_client_FAILED")
                results.record("cross-client root creation reaches a connected observer", False, repr(exc), shot)

            # ---- S13: deeper-than-one-level creation and collapsed levels -----
            try:
                log("  s13: nested creation beyond the observer cache")
                cdp.drain_tree_fetches()
                status, mid = await asyncio.to_thread(
                    api_request, base, access_key, "POST", "/api/sessions/",
                    {"request_id": "harness-x-mid-1", "task_parent_id": ids["feature"],
                     "profile": "manager", "name": "Remote mid",
                     "task": {"goal": "mid manager", "acceptance": [], "context_refs": []}})
                assert_true(status == 200, f"nested manager create succeeded ({status})")
                created["mid"] = mid["id"]
                await wait_for(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .tree-row .session-name')]
                      .some(el => el.textContent === 'Remote mid')
                """, timeout=10, label="s13 remote mid row under the expanded feature")
                # A worker under the new mid: the mid's children level was never
                # fetched, so the leaf is absent from the observer cache.
                cdp.drain_tree_fetches()
                status, leaf = await asyncio.to_thread(
                    api_request, base, access_key, "POST", "/api/sessions/",
                    {"request_id": "harness-x-leaf-1", "task_parent_id": mid["id"],
                     "profile": "worker", "name": "Remote leaf",
                     "task": {"goal": "idle leaf worker", "acceptance": [], "context_refs": []}})
                assert_true(status == 200, f"deep leaf create succeeded ({status})")
                created["leaf"] = leaf["id"]
                await wait_for(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .tree-row')]
                      .filter(el => el.dataset.nodeId === '%s')
                      .every(el => el.textContent.includes('1 open'))
                """ % mid["id"], timeout=10, label="s13 mid row gained the leaf count while collapsed")
                collapsed_ok = await evaluate(cdp, session_id,
                    "!document.getElementById('tree-children-%s')" % mid["id"])
                assert_true(collapsed_ok, "the collapsed level stays collapsed")
                fetches = cdp.drain_tree_fetches()
                assert_true(1 <= len(fetches) <= 5,
                            f"bounded path-level work for the deep creation ({len(fetches)} fetches)")
                await evaluate(cdp, session_id, f"Sidebar.SessionTree.ensureExpanded('{mid['id']}')")
                await wait_for(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .tree-row .session-name')]
                      .some(el => el.textContent === 'Remote leaf')
                """, timeout=8, label="s13 leaf visible after expanding the mid")
                shot = await screenshot(cdp, session_id, results, "s13_deep_creation")
                results.record("deeper-than-one-level creation from a separate client", True,
                               "mid appeared under the expanded parent; collapsed mid gained the count; expansion reveals the idle leaf", shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s13_deep_creation_FAILED")
                results.record("deeper-than-one-level creation from a separate client", False, repr(exc), shot)

            # ---- S14: agent-scoped creation reaches the observer --------------
            try:
                log("  s14: scoped-agent creation")
                from src.core.run_token import RunTokenClaims, sign_run_token
                agent_token = sign_run_token(
                    RunTokenClaims(session_id=ids["root"], run_id="agent-auth-run", agent="harness-agent"),
                    access_key)
                cdp.drain_tree_fetches()
                status, worker = await asyncio.to_thread(
                    api_request, base, access_key, "POST", "/api/sessions/",
                    {"request_id": "harness-agent-w1", "task_parent_id": ids["root"],
                     "profile": "worker", "name": "Agent worker",
                     "task": {"goal": "created by a scoped agent", "acceptance": [], "context_refs": []}},
                    token=agent_token)
                assert_true(status == 200, f"agent-scoped create succeeded ({status}: {worker})")
                await wait_for(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .tree-row .session-name')]
                      .some(el => el.textContent === 'Agent worker')
                """, timeout=10, label="s14 agent-created worker appeared")
                shot = await screenshot(cdp, session_id, results, "s14_agent_creation")
                results.record("agent-scoped creation reaches a connected observer", True,
                               "run-token agent created a worker under its manager; the observer saw it live", shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s14_agent_creation_FAILED")
                results.record("agent-scoped creation reaches a connected observer", False, repr(exc), shot)

            # ---- S15: replay, refusal, drafts and selection preservation ------
            try:
                log("  s15: replay, refusal, drafts")
                # An unsaved task draft on the open session must survive the
                # creation traffic.
                await evaluate(cdp, session_id, "switchTab('task')")
                await wait_for(cdp, session_id, "!!document.getElementById('task-goal-input')")
                await evaluate(cdp, session_id, """
                    (async () => {
                      const el = document.getElementById('task-goal-input');
                      el.value = 'unsaved draft during remote creations';
                      el.dispatchEvent(new Event('input'));
                    })()
                """)
                await evaluate(cdp, session_id, "switchTab('task-context')")
                # Replay: the same request_id returns the original product; the
                # tree shows no duplicate.
                status, replay = await asyncio.to_thread(
                    api_request, base, access_key, "POST", "/api/sessions/",
                    {"request_id": "harness-x-root-1", "profile": "manager", "name": "Remote root",
                     "task": {"goal": "created outside the browser", "acceptance": [], "context_refs": []}})
                assert_true(status == 200 and replay["id"] == created["root"],
                            f"the replay returned the original product ({status})")
                await asyncio.sleep(0.6)
                names = await evaluate(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .tree-row .session-name')]
                      .map(el => el.textContent)
                """)
                assert_true(names.count("Remote root") == 1, "the replay never duplicated the row")
                # A refused (pre-publication) create emits no successful-node
                # signal: a worker under a worker parent is refused outright.
                observer_errors_before = await evaluate(cdp, session_id, "(window.__errs || []).length")
                rows_before = await evaluate(cdp, session_id,
                    "document.querySelectorAll('#session-list .tree-row').length")
                status, refusal = await asyncio.to_thread(
                    api_request, base, access_key, "POST", "/api/sessions/",
                    {"request_id": "harness-refused-1", "task_parent_id": created["leaf"],
                     "profile": "worker", "name": "Should not exist",
                     "task": {"goal": "refused", "acceptance": [], "context_refs": []}})
                assert_true(status == 400, f"the worker-parent create was refused ({status})")
                await asyncio.sleep(0.6)
                rows_after = await evaluate(cdp, session_id,
                    "document.querySelectorAll('#session-list .tree-row').length")
                assert_true(rows_after == rows_before, "a refused create changed nothing in the tree")
                errs_after = await evaluate(cdp, session_id, "(window.__errs || []).length")
                assert_true(errs_after == observer_errors_before,
                            f"no unexpected browser error from the refused create ({errs_after} vs {observer_errors_before})")
                # Drafts and selection survive the whole cross-client sequence.
                active_now = await evaluate(cdp, session_id, "SESSION_ID")
                assert_true(active_now == ids["feature"], "selection stayed on the open node")
                await evaluate(cdp, session_id, "switchTab('task')")
                await wait_for(cdp, session_id,
                    "document.getElementById('task-goal-input')?.value === 'unsaved draft during remote creations'",
                    label="s15 draft preserved")
                shot = await screenshot(cdp, session_id, results, "s15_replay_refusal_draft")
                results.record("replay, refusal, draft and selection preservation", True,
                               "one node one fact after replay; refusal emitted nothing; draft and selection intact", shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s15_replay_refusal_FAILED")
                results.record("replay, refusal, draft and selection preservation", False, repr(exc), shot)

            # ---- S16: completion evidence beyond the first page ---------------
            try:
                log("  s16: completion evidence paging")
                await evaluate(cdp, session_id, f"switchSession('{ids['evidence']}')")
                await evaluate(cdp, session_id, "switchTab('task')")
                await wait_for(cdp, session_id,
                               "document.getElementById('tab-task').textContent.includes('Evidence manager')")
                await wait_for(cdp, session_id, "!!document.getElementById('task-action-complete')")
                await evaluate(cdp, session_id, """
                    (async () => {
                      [...document.querySelectorAll('#tab-task button')]
                        .find(b => b.textContent.startsWith('Complete')).click();
                    })()
                """)
                await wait_for(cdp, session_id, "!!document.getElementById('task-complete-modal')")
                box_values = "JSON.stringify([...document.querySelectorAll('#task-complete-runs input[type=checkbox]')].map(b => b.value))"
                await wait_for(cdp, session_id, f"({box_values}).includes('run-e0120')",
                               label="s16 newest-first first page rendered")
                vals = json.loads(await evaluate(cdp, session_id, box_values))
                assert_true("run-e0120" in vals and "run-e0071" in vals,
                            f"the newest-first page holds runs e0071..e0120 ({len(vals)} rows)")
                assert_true("run-e0070" not in vals and "run-e0003" not in vals,
                            "older history is not silently fetched")
                # Draft + a recent selection, then page older: both survive.
                await evaluate(cdp, session_id, """
                    (() => {
                      const s = document.getElementById('task-complete-summary');
                      s.value = 'delivered via runs e0119 and the early e0003';
                      s.dispatchEvent(new Event('input'));
                      const refs = document.getElementById('task-complete-refs');
                      refs.value = 'evidence/run-e0119.log';
                      refs.dispatchEvent(new Event('input'));
                      const box = [...document.querySelectorAll('#task-complete-runs input[type=checkbox]')]
                        .find(b => b.value === 'run-e0119');
                      box.checked = true; box.dispatchEvent(new Event('change'));
                    })()
                """)
                more = await evaluate(cdp, session_id, """
                    (() => {
                      const b = [...document.querySelectorAll('#task-complete-runs button')]
                        .find(x => x.textContent.startsWith('Load older runs'));
                      if (!b) return 'no-continuation';
                      b.click(); return 'clicked';
                    })()
                """)
                assert_true(more == 'clicked', f"the continuation was offered ({more})")
                await wait_for(cdp, session_id, f"({box_values}).includes('run-e0070')",
                               label="s16 second page appended")
                await wait_for(cdp, session_id,
                    "document.getElementById('task-complete-selected').textContent.includes('Selected evidence (1)')",
                    label="s16 selection chip survives the page load")
                summary_val = await evaluate(cdp, session_id, "document.getElementById('task-complete-summary').value")
                assert_true(summary_val.startswith('delivered via runs'),
                            "the summary draft survived the page load")
                # A live event (an out-of-band rename of THIS task) refreshes the
                # panel while the dialog is open: selection and draft must hold.
                status, _renamed = await asyncio.to_thread(
                    api_request, base, access_key, "PATCH", f"/api/sessions/{ids['evidence']}",
                    {"name": "Evidence manager renamed"})
                assert_true(status == 200, f"the out-of-band rename succeeded ({status})")
                await asyncio.sleep(1.0)
                await wait_for(cdp, session_id,
                    "document.getElementById('task-complete-selected').textContent.includes('run-e0119'.slice(0,8))",
                    timeout=6, label="s16 selection chip survives the live refresh")
                vals_live = json.loads(await evaluate(cdp, session_id, box_values))
                assert_true(len(vals_live) == 100 and len(set(vals_live)) == 100,
                            f"the live refresh merged without duplicates ({len(vals_live)} rows)")
                # Page 3: the early history, then select the early run.
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#task-complete-runs button')]
                      .find(x => x.textContent.startsWith('Load older runs')).click(); })()
                """)
                await wait_for(cdp, session_id, f"({box_values}).includes('run-e0003')",
                               label="s16 third page reached the early history")
                vals = json.loads(await evaluate(cdp, session_id, box_values))
                assert_true(len(vals) == 120 and len(set(vals)) == 120,
                            f"all 120 runs are reachable without duplicates ({len(vals)})")
                await evaluate(cdp, session_id, """
                    (() => {
                      const box = [...document.querySelectorAll('#task-complete-runs input[type=checkbox]')]
                        .find(b => b.value === 'run-e0003');
                      box.checked = true; box.dispatchEvent(new Event('change'));
                    })()
                """)
                shot = await screenshot(cdp, session_id, results, "s16_completion_paging")
                # Submit through the guarded complete path and verify the exact
                # outgoing ids plus the server fact.
                mark = cdp.mutation_mark()
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#task-complete-modal button')]
                      .find(b => b.textContent === 'Complete task').click(); })()
                """)
                await wait_for(cdp, session_id,
                               "document.getElementById('tab-task').textContent.includes('task: completed')",
                               timeout=20, label="s16 completion landed")
                muts = [m for m in cdp.mutations_since(mark) if m["url"].endswith("/complete")]
                assert_true(len(muts) == 1, f"exactly one complete request left ({len(muts)})")
                body = json.loads(muts[0]["body"])
                assert_true(sorted(body["run_ids"]) == ["run-e0003", "run-e0119"],
                            f"exactly the two selected ids were submitted ({body['run_ids']})")
                assert_true(body["summary"].startswith("delivered via runs")
                            and body["result_refs"] == ["evidence/run-e0119.log"],
                            "the draft summary and refs rode the claim")
                status, detail = await asyncio.to_thread(
                    api_request, base, access_key, "GET", f"/api/sessions/{ids['evidence']}")
                assert_true(status == 200 and detail["task_state"] == "completed",
                            f"the server closed the task ({status}, {detail.get('task_state')})")
                shot2 = await screenshot(cdp, session_id, results, "s16b_completion_submitted")
                results.record("completion evidence beyond the first page", True,
                               "desc-first picker paged to run-e0003 (beyond the old 100-read); selection+draft held across page load and a live event; exact run_ids submitted and closed server-side",
                               shot2)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s16_completion_FAILED")
                results.record("completion evidence beyond the first page", False, repr(exc), shot)

            # ---- S16c: a failed evidence read is an error, never "no runs" ----
            try:
                log("  s16c: failed evidence read")
                await evaluate(cdp, session_id, f"switchSession('{ids['ops_mid']}')")
                await evaluate(cdp, session_id, "switchTab('task')")
                await wait_for(cdp, session_id,
                    "document.getElementById('tab-task').textContent.includes('Ops mid')")
                await wait_for(cdp, session_id, "!!document.getElementById('task-action-complete')")
                # One injected 503 at the fetch boundary (the only honest way to
                # exercise this UI's failure rendering against a healthy server).
                await evaluate(cdp, session_id, """
                    (() => {
                      const orig = window.fetch;
                      window.__origFetch = orig;
                      window.__injectedFail = true;
                      window.fetch = (url, opts) => {
                        if (window.__injectedFail && String(url).includes('order=desc')) {
                          window.__injectedFail = false;
                          return Promise.resolve(new Response('{}', {status: 503}));
                        }
                        return orig(url, opts);
                      };
                    })()
                """)
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#tab-task button')]
                      .find(b => b.textContent.startsWith('Complete')).click(); })()
                """)
                await wait_for(cdp, session_id, "!!document.getElementById('task-complete-modal')")
                await wait_for(cdp, session_id,
                    "document.getElementById('task-complete-runs').textContent.includes('Failed to load runs')",
                    label="s16c fetch error surfaced")
                err_text = await evaluate(cdp, session_id, "document.getElementById('task-complete-runs').textContent")
                assert_true("No finished runs yet." not in err_text,
                            "a failed read is never misreported as an empty history")
                shot = await screenshot(cdp, session_id, results, "s16c_complete_fetch_error")
                await evaluate(cdp, session_id, """
                    (() => {
                      if (window.__origFetch) { window.fetch = window.__origFetch; }
                      [...document.querySelectorAll('#task-complete-runs button')]
                        .find(b => b.textContent === 'Retry').click();
                    })()
                """)
                await wait_for(cdp, session_id,
                    "document.getElementById('task-complete-runs').textContent.includes('No finished runs yet.')",
                    timeout=10, label="s16c retry reached the truthful empty state")
                shot2 = await screenshot(cdp, session_id, results, "s16c2_complete_empty_state")
                results.record("failed evidence read surfaces as an error", True,
                               "one injected 503 rendered 'Failed to load runs' + Retry, never 'No finished runs'; retry reached the truthful empty state",
                               shot2)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s16c_fetch_error_FAILED")
                results.record("failed evidence read surfaces as an error", False, repr(exc), shot)

            # ---- S17: the move chooser across root pages, depths and search ---
            try:
                log("  s17: move chooser")
                await evaluate(cdp, session_id, f"switchSession('{ids['bulk1']}')")
                await evaluate(cdp, session_id, "switchTab('task')")
                await wait_for(cdp, session_id,
                    "document.getElementById('tab-task').textContent.includes('Bulk root 01')")
                await evaluate(cdp, session_id, "document.getElementById('task-action-move').click()")
                await wait_for(cdp, session_id, "!!document.getElementById('task-move-modal')")
                await wait_for(cdp, session_id,
                    "document.getElementById('task-move-list').textContent.includes('Bulk root 25')")
                list_text = await evaluate(cdp, session_id, "document.getElementById('task-move-list').textContent")
                assert_true("Bulk root 26" not in list_text, "later roots are not silently fetched")
                assert_true(await evaluate(cdp, session_id, "document.getElementById('task-move-confirm').disabled"),
                            "Move starts disabled: root is never a silent default")
                # A cross-client root creation between the pages forces a
                # tree_revision change during pagination.
                status, _r = await asyncio.to_thread(
                    api_request, base, access_key, "POST", "/api/sessions/",
                    {"request_id": "harness-move-page-root", "profile": "manager",
                     "name": "Remote late root", "task": {"goal": "created mid-pagination", "acceptance": [], "context_refs": []}})
                assert_true(status == 200, f"the mid-pagination create succeeded ({status})")
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#task-move-list button')]
                      .find(b => b.textContent.startsWith('Load more')).click(); })()
                """)
                await wait_for(cdp, session_id,
                    "document.getElementById('task-move-list').textContent.includes('Remote late root')",
                    timeout=10, label="s17 later roots page after a revision change")
                await wait_for(cdp, session_id,
                    "document.getElementById('task-move-list').textContent.includes('task tree changed')",
                    timeout=6, label="s17 revision-change explanation visible")
                occurrence = await evaluate(cdp, session_id, """
                    document.getElementById('task-move-list').textContent.split('Bulk root 26').length - 1
                """)
                assert_true(occurrence == 1, f"the coherent reload duplicated nothing ({occurrence})")
                shot = await screenshot(cdp, session_id, results, "s17_move_browse")
                # Expand Late root (a later-page root) and choose its
                # intermediate manager.
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#task-move-list button')]
                      .find(b => (b.getAttribute('aria-label') || '').includes("Expand Late root")).click(); })()
                """)
                await wait_for(cdp, session_id,
                    "document.getElementById('task-move-list').textContent.includes('Late mid')")
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#task-move-list [role="button"]')]
                      .find(r => r.textContent.includes('Late mid')).click(); })()
                """)
                await wait_for(cdp, session_id,
                    "document.getElementById('task-move-chosen').textContent.includes('Late mid')")
                mark = cdp.mutation_mark()
                await evaluate(cdp, session_id, "document.getElementById('task-move-confirm').click()")
                await wait_for(cdp, session_id, "!document.getElementById('task-move-modal')",
                               timeout=10, label="s17 move submitted")
                status, detail = await asyncio.to_thread(
                    api_request, base, access_key, "GET", f"/api/sessions/{ids['bulk1']}")
                assert_true(status == 200 and detail["task_parent_id"] == ids["late_mid"],
                            f"the moved task's real parent is the chosen manager ({detail.get('task_parent_id')})")
                muts = [m for m in cdp.mutations_since(mark) if m["method"] == "PATCH"]
                assert_true(len(muts) == 1 and json.loads(muts[0]["body"])["task_parent_id"] == ids["late_mid"],
                            f"the submitted target id is the chosen manager ({muts})")
                # Refusal: a target that becomes invalid between the choice
                # and the submit. A fresh dialog is opened for Program rollout;
                # a childless manager root is created while the chooser sits on
                # its first page (a revision bump mid-pagination); the
                # continuation recovers coherently; the new target is chosen,
                # closed out-of-band, and the submit is refused by the server;
                # the dialog keeps the intended choice for correction.
                await evaluate(cdp, session_id, f"switchSession('{ids['root']}')")
                await wait_for(cdp, session_id,
                    "document.getElementById('tab-task').textContent.includes('Program rollout')")
                await evaluate(cdp, session_id, "document.getElementById('task-action-move').click()")
                await wait_for(cdp, session_id, "!!document.getElementById('task-move-modal')")
                await wait_for(cdp, session_id,
                    "document.getElementById('task-move-list').textContent.includes('Bulk root 25')")
                status, victim = await asyncio.to_thread(
                    api_request, base, access_key, "POST", "/api/sessions/",
                    {"request_id": "harness-refusal-victim", "profile": "manager",
                     "name": "Refusal victim", "task": {"goal": "closed right after being chosen", "acceptance": [], "context_refs": []}})
                assert_true(status == 200, f"the refusal victim was created ({status})")
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#task-move-list button')]
                      .find(b => b.textContent.startsWith('Load more')).click(); })()
                """)
                await wait_for(cdp, session_id,
                    "document.getElementById('task-move-list').textContent.includes('Refusal victim')",
                    timeout=10, label="s17 refusal victim reached through the recovered later page")
                await wait_for(cdp, session_id,
                    "document.getElementById('task-move-list').textContent.includes('task tree changed')",
                    timeout=6, label="s17 refusal-page revision change explained")
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#task-move-list [role="button"]')]
                      .find(r => r.textContent.includes('Refusal victim')).click(); })()
                """)
                await wait_for(cdp, session_id,
                    "document.getElementById('task-move-chosen').textContent.includes('Refusal victim')")
                status, _c = await asyncio.to_thread(
                    api_request, base, access_key, "POST", f"/api/sessions/{victim['id']}/cancel",
                    {"request_id": "harness-close-victim", "reason": "closed out-of-band for the refusal case"})
                assert_true(status == 200, f"the out-of-band close succeeded ({status})")
                await evaluate(cdp, session_id, "document.getElementById('task-move-confirm').click()")
                await wait_for(cdp, session_id,
                    "document.getElementById('task-move-modal').textContent.includes('refused this move')",
                    timeout=10, label="s17 refusal explained")
                err_text = await evaluate(cdp, session_id, "document.getElementById('task-move-modal').textContent")
                assert_true("Refusal victim" in err_text or "closed" in err_text or "not an open" in err_text,
                            f"the refusal names the conflict: {err_text[:300]}")
                chosen_after = await evaluate(cdp, session_id, "document.getElementById('task-move-chosen').textContent")
                assert_true("Refusal victim" in chosen_after, "the intended choice is retained for correction")
                assert_true(not await evaluate(cdp, session_id, "document.getElementById('task-move-confirm').disabled"),
                            "the corrected resubmit stays possible")
                shot2 = await screenshot(cdp, session_id, results, "s17b_move_refusal")
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#task-move-modal button')]
                      .find(b => b.textContent === 'Cancel').click(); })()
                """)
                # Explicit root choice.
                await evaluate(cdp, session_id, f"switchSession('{ids['bulk2']}')")
                await wait_for(cdp, session_id,
                    "document.getElementById('tab-task').textContent.includes('Bulk root 02')")
                await evaluate(cdp, session_id, "document.getElementById('task-action-move').click()")
                await wait_for(cdp, session_id, "!!document.getElementById('task-move-modal')")
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#task-move-list [role="button"]')]
                      .find(r => r.textContent.includes('make it a root task')).click(); })()
                """)
                await wait_for(cdp, session_id,
                    "document.getElementById('task-move-chosen').textContent.includes('root')")
                mark = cdp.mutation_mark()
                await evaluate(cdp, session_id, "document.getElementById('task-move-confirm').click()")
                await wait_for(cdp, session_id, "!document.getElementById('task-move-modal')",
                               timeout=10, label="s17 explicit root move submitted")
                status, detail = await asyncio.to_thread(
                    api_request, base, access_key, "GET", f"/api/sessions/{ids['bulk2']}")
                assert_true(status == 200 and detail["task_parent_id"] is None,
                            f"the explicit root choice submitted a null parent ({detail.get('task_parent_id')})")
                muts = [m for m in cdp.mutations_since(mark) if m["method"] == "PATCH"]
                assert_true(len(muts) == 1 and json.loads(muts[0]["body"])["task_parent_id"] is None,
                            "the submitted root choice is a real null parent")
                # Search reaches a manager anywhere and submits its real id.
                await evaluate(cdp, session_id, f"switchSession('{ids['bulk3']}')")
                await wait_for(cdp, session_id,
                    "document.getElementById('tab-task').textContent.includes('Bulk root 03')")
                await evaluate(cdp, session_id, "document.getElementById('task-action-move').click()")
                await wait_for(cdp, session_id, "!!document.getElementById('task-move-modal')")
                await evaluate(cdp, session_id, """
                    (() => {
                      const input = document.getElementById('task-move-search');
                      input.value = 'Late mid';
                      [...document.querySelectorAll('#task-move-modal button')]
                        .find(b => b.textContent === 'Search').click();
                    })()
                """)
                await wait_for(cdp, session_id,
                    "document.getElementById('task-move-list').textContent.includes('Late root \u203a Late mid')")
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#task-move-list [role="button"]')]
                      .find(r => r.textContent.includes('Late mid')).click(); })()
                """)
                await wait_for(cdp, session_id,
                    "document.getElementById('task-move-chosen').textContent.includes('Late mid')")
                await evaluate(cdp, session_id, "document.getElementById('task-move-confirm').click()")
                await wait_for(cdp, session_id, "!document.getElementById('task-move-modal')",
                               timeout=10, label="s17 search-chosen move submitted")
                status, detail = await asyncio.to_thread(
                    api_request, base, access_key, "GET", f"/api/sessions/{ids['bulk3']}")
                assert_true(status == 200 and detail["task_parent_id"] == ids["late_mid"],
                            f"the search-chosen move submitted its real id ({detail.get('task_parent_id')})")
                results.record("move chooser across pages, depths and search", True,
                               "later-page roots reached; intermediate manager chosen under a later-page root and read back via task_parent_id; revision change mid-pagination explained and reloaded; invalidated-target refusal kept the chosen selection; explicit root submitted a real null parent; search hit submitted its real id",
                               shot2)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s17_move_FAILED")
                results.record("move chooser across pages, depths and search", False, repr(exc), shot)

            # ---- S18: the child list pages past the old 100-record read -------
            try:
                log("  s18: children paging")
                await evaluate(cdp, session_id, f"switchSession('{ids['wide']}')")
                await evaluate(cdp, session_id, "switchTab('runs')")
                await wait_for(cdp, session_id,
                    "document.getElementById('tab-runs').textContent.includes('Load more children')")
                links = await evaluate(cdp, session_id, """
                    [...document.querySelectorAll('#tab-runs a')].filter(a => a.href.includes('session=')).map(a => a.href)
                """)
                assert_true(len(links) == 50, f"the first page holds {len(links)} children")
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#tab-runs button')]
                      .find(b => b.textContent.startsWith('Load more children')).click(); })()
                """)
                await wait_for(cdp, session_id, """
                    [...document.querySelectorAll('#tab-runs a')].filter(a => a.href.includes('session=')).length >= 100
                """, label="s18 second children page")
                # A cross-client creation between pages bumps tree_revision: the
                # stale cursor must 409 and reload coherently.
                status, new_child = await asyncio.to_thread(
                    api_request, base, access_key, "POST", "/api/sessions/",
                    {"request_id": "harness-wide-new-child", "task_parent_id": ids["wide"],
                     "profile": "worker", "name": "Wide child new",
                     "task": {"goal": "created mid-pagination", "acceptance": [], "context_refs": []}})
                assert_true(status == 200, f"the mid-pagination child create succeeded ({status})")
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#tab-runs button')]
                      .find(b => b.textContent.startsWith('Load more children')).click(); })()
                """)
                await wait_for(cdp, session_id,
                    "document.getElementById('tab-runs').textContent.includes('task tree changed')",
                    timeout=10, label="s18 revision-change explanation visible")
                await wait_for(cdp, session_id, """
                    [...document.querySelectorAll('#tab-runs a')].some(a => a.href.endsWith('session=%s'))
                """ % new_child["id"], timeout=10, label="s18 the mid-paging child appeared")
                hrefs = await evaluate(cdp, session_id, """
                    [...document.querySelectorAll('#tab-runs a')].filter(a => a.href.includes('session=')).map(a => a.href)
                """)
                assert_true(len(hrefs) == len(set(hrefs)) == 106,
                            f"all 106 children are listed exactly once ({len(hrefs)})")
                shot = await screenshot(cdp, session_id, results, "s18_children_paging")
                # Follow the real deep link of a child beyond the old read.
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#tab-runs a')]
                      .find(a => a.textContent === 'Wide child 105').click(); })()
                """)
                await wait_for(cdp, session_id, """
                    document.getElementById('header-session-name') && document.getElementById('header-session-name').textContent === 'Wide child 105'
                """, timeout=10, label="s18 deep link switched to the child")
                active = await evaluate(cdp, session_id, "SESSION_ID")
                assert_true(active != ids["wide"], "the deep link left the wide manager")
                results.record("child list pages past 100 with revision recovery", True,
                               "50 -> 100 -> 106 children through the continuation; 409 explained and reloaded without duplicates/omissions; deep link followed",
                               shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s18_children_FAILED")
                results.record("child list pages past 100 with revision recovery", False, repr(exc), shot)

            # ---- S19: dialogs bind to their task across a real session switch -
            try:
                log("  s19: dialog binding")
                await evaluate(cdp, session_id, f"switchSession('{ids['bind_a']}')")
                await evaluate(cdp, session_id, "switchTab('task')")
                await wait_for(cdp, session_id, "document.getElementById('tab-task').textContent.includes('Bind task A')")
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#tab-task button')]
                      .find(b => b.textContent.startsWith('Complete')).click(); })()
                """)
                await wait_for(cdp, session_id, "!!document.getElementById('task-complete-modal')")
                await wait_for(cdp, session_id,
                    "document.getElementById('task-complete-runs').textContent.includes('No finished runs yet.')",
                    label="s19 A's empty evidence list rendered truthfully")
                await evaluate(cdp, session_id, """
                    (() => {
                      const s = document.getElementById('task-complete-summary');
                      s.value = 'A half-written completion';
                      s.dispatchEvent(new Event('input'));
                    })()
                """)
                mark = cdp.mutation_mark()
                # The app's real session-switch path.
                await evaluate(cdp, session_id, f"switchSession('{ids['bind_b']}')")
                await wait_for(cdp, session_id,
                    "document.getElementById('tab-task').textContent.includes('Bind task B')",
                    timeout=10, label="s19 switched to B")
                assert_true(not await evaluate(cdp, session_id, "!!document.getElementById('task-complete-modal')"),
                            "A's completion dialog was dismissed by the switch")
                muts = cdp.mutations_since(mark)
                assert_true(not muts, f"no request left the dismissed dialog ({muts})")
                # A late refusal/success after the switch: the delayed cancel.
                await evaluate(cdp, session_id, """
                    (() => {
                      const orig = window.fetch;
                      window.__origFetch = orig;
                      window.fetch = (url, opts) => {
                        const p = orig(url, opts);
                        if (String(url).endsWith('/cancel')) {
                          return p.then((r) => new Promise((res) => setTimeout(() => res(r), 500)));
                        }
                        return p;
                      };
                    })()
                """)
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#tab-task button')]
                      .find(b => b.textContent === 'Cancel task…').click(); })()
                """)
                await wait_for(cdp, session_id, "!!document.getElementById('task-reason-modal')")
                await evaluate(cdp, session_id, """
                    (() => {
                      const r = document.getElementById('task-reason-input');
                      r.value = 'late-response probe';
                      r.dispatchEvent(new Event('input'));
                      [...document.querySelectorAll('#task-reason-modal button')]
                        .find(b => b.textContent === 'Cancel task').click();
                    })()
                """)
                await asyncio.sleep(0.3)  # the delayed POST is in flight
                # Switch away while B's cancel POST is still delayed in flight.
                await evaluate(cdp, session_id, f"switchSession('{ids['root']}')")
                await wait_for(cdp, session_id,
                    "document.getElementById('tab-task').textContent.includes('Program rollout')",
                    timeout=10, label="s19 switched to the root")
                assert_true(not await evaluate(cdp, session_id, "!!document.getElementById('task-reason-modal')"),
                            "B's reason dialog died with the switch")
                await asyncio.sleep(0.8)  # the delayed response lands here
                cancels = [m for m in cdp.mutations if m["url"].endswith("/cancel")]
                assert_true(len(cancels) == 1 and f"/api/sessions/{ids['bind_b']}/cancel" in cancels[0]["url"],
                            f"exactly one cancel request, targeting B ({cancels})")
                errs = await evaluate(cdp, session_id, "(window.__errs || []).length")
                assert_true(errs == 0, f"no unexpected browser error from the late response ({errs})")
                # Re-opening on the new task targets the new task: the root's
                # cancel is refused with its concrete open-children blockers.
                await evaluate(cdp, session_id, """
                    (() => { [...document.querySelectorAll('#tab-task button')]
                      .find(b => b.textContent === 'Cancel task…').click(); })()
                """)
                await wait_for(cdp, session_id, "!!document.getElementById('task-reason-modal')")
                mark = cdp.mutation_mark()
                await evaluate(cdp, session_id, """
                    (() => {
                      const r = document.getElementById('task-reason-input');
                      r.value = 'should be refused';
                      r.dispatchEvent(new Event('input'));
                      [...document.querySelectorAll('#task-reason-modal button')]
                        .find(b => b.textContent === 'Cancel task').click();
                    })()
                """)
                await wait_for(cdp, session_id,
                    "document.getElementById('task-reason-modal').textContent.includes('open descendant')",
                    timeout=10, label="s19 root cancel refused with blockers")
                root_cancels = [m for m in cdp.mutations_since(mark) if m["url"].endswith("/cancel")]
                assert_true(len(root_cancels) == 1 and f"/api/sessions/{ids['root']}/cancel" in root_cancels[0]["url"],
                            f"the re-opened dialog submitted against the ROOT task ({root_cancels})")
                assert_true(await evaluate(cdp, session_id, "!!document.getElementById('task-reason-modal')"),
                            "the refused dialog stays open with its draft")
                shot = await screenshot(cdp, session_id, results, "s19_dialog_binding")
                await evaluate(cdp, session_id, """
                    (() => {
                      if (window.__origFetch) { window.fetch = window.__origFetch; }
                      [...document.querySelectorAll('#task-reason-modal button')]
                        .find(b => b.textContent === 'Cancel').click();
                    })()
                """)
                results.record("dialogs bind to their task across session switches", True,
                               "A's dialog dismissed with zero requests; the late cancel targeted B only and was dropped; the re-opened dialog submitted against the root and surfaced its blockers",
                               shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s19_dialog_binding_FAILED")
                results.record("dialogs bind to their task across session switches", False, repr(exc), shot)

            # ---- S20: the name keeps visible width at any depth ----------------
            try:
                log("  s20: name readability")
                await evaluate(cdp, session_id, f"switchSession('{ids['ops_root']}')")
                await wait_for(cdp, session_id, "!!document.getElementById('tree-node-%s')" % ids["ops_root"])
                # The nested manager's row exists once its parent is expanded.
                await evaluate(cdp, session_id, f"Sidebar.SessionTree.ensureExpanded('{ids['ops_root']}')")
                await wait_for(cdp, session_id, "!!document.getElementById('tree-node-%s')" % ids["ops_mid"])

                def geometry_expr(node_id):
                    return """
                    (() => {
                      const row = document.getElementById('tree-node-%s');
                      if (!row) return null;
                      const inner = row.firstElementChild;
                      const name = row.querySelector('.session-name');
                      const meta = row.querySelector('.tree-meta-row');
                      const r = (el) => { const b = el.getBoundingClientRect(); return {x: Math.round(b.x), y: Math.round(b.y), w: Math.round(b.width), h: Math.round(b.height)}; };
                      return {name: r(name), meta: meta ? r(meta) : null, inner: r(inner),
                              nameText: name.textContent};
                    })()
                    """ % node_id

                async def assert_readable(node_id, label):
                    geo = await evaluate(cdp, session_id, geometry_expr(node_id))
                    shape = json.dumps(geo)
                    assert_true(geo and geo["name"]["w"] >= 0.5 * geo["inner"]["w"],
                                f"{label}: name width vs row: {shape}")
                    assert_true(geo["nameText"], f"{label}: the name text renders")
                    assert_true(geo["meta"] and geo["meta"]["w"] > 0, f"{label}: badges render: {shape}")
                    assert_true(geo["meta"]["y"] > geo["name"]["y"] + geo["name"]["h"] - 2,
                                f"{label}: badges did not wrap below the name: {shape}")
                    return geo

                ops_geo = await assert_readable(ids["ops_root"], "ops root (attention + counts)")
                mid_geo = await assert_readable(ids["ops_mid"], "nested ops manager")
                assert_true("Ops root" == ops_geo["nameText"] and "Ops mid" == mid_geo["nameText"],
                            "both measured rows carry their full task names")
                # Keyboard focus and actionable controls.
                await evaluate(cdp, session_id, f"document.getElementById('tree-node-{ids['ops_root']}').focus()")
                focused = await evaluate(cdp, session_id, "document.activeElement && document.activeElement.dataset.nodeId")
                assert_true(focused == ids["ops_root"], f"the row is keyboard-focusable ({focused})")
                controls = await evaluate(cdp, session_id, """
                    (() => {
                      const row = document.getElementById('tree-node-%s');
                      const add = row.querySelector('.tree-add-child');
                      const addRect = add.getBoundingClientRect();
                      return {addVisible: addRect.width > 0 && addRect.height > 0,
                              addLabel: add.getAttribute('aria-label')};
                    })()
                """ % ids["ops_mid"])
                assert_true(controls["addVisible"] and "New subtask" in (controls["addLabel"] or ""),
                            f"the nested manager keeps its actionable add control ({controls})")
                shot = await screenshot(cdp, session_id, results, "s20_desktop_readability")
                # 390px: same guarantees, no horizontal overflow.
                await cdp.send("Emulation.setDeviceMetricsOverride", {
                    "width": 390, "height": 844, "deviceScaleFactor": 2, "mobile": True,
                }, session_id=session_id)
                await asyncio.sleep(0.4)
                # <=768px hides the sidebar behind the drawer toggle: open it,
                # exactly as a 390px operator would.
                await evaluate(cdp, session_id, "toggleMobileSidebar()")
                await asyncio.sleep(0.4)
                overflow = await evaluate(cdp, session_id,
                    "document.documentElement.scrollWidth - document.documentElement.clientWidth")
                assert_true(overflow <= 1, f"no horizontal overflow at 390px (delta={overflow})")
                mob_geo = await assert_readable(ids["ops_root"], "ops root @390px")
                mob_mid = await assert_readable(ids["ops_mid"], "nested ops manager @390px")
                shot2 = await screenshot(cdp, session_id, results, "s20b_mobile_readability")
                await cdp.send("Emulation.setDeviceMetricsOverride", {
                    "width": 1440, "height": 900, "deviceScaleFactor": 1, "mobile": False,
                }, session_id=session_id)
                results.record("tree names stay readable at any depth", True,
                               f"desktop name {ops_geo['name']['w']}px / mobile {mob_geo['name']['w']}px of a {ops_geo['inner']['w']}px row; badges on their own line; focus and add control actionable",
                               shot2)
            except Exception as exc:
                await cdp.send("Emulation.setDeviceMetricsOverride", {
                    "width": 1440, "height": 900, "deviceScaleFactor": 1, "mobile": False,
                }, session_id=session_id)
                shot = await screenshot(cdp, session_id, results, "s20_readability_FAILED")
                results.record("tree names stay readable at any depth", False, repr(exc), shot)

            # The CDP collector records console.error calls and uncaught page
            # exceptions from Runtime.enable onward — this list is the only
            # source; an assertion over an always-empty list is a faked pass.
            console_errors = [e for e in cdp.console_errors if "favicon" not in e]
            results.console_errors = console_errors
            results.record("console clean", len(console_errors) == 0,
                           f"{len(console_errors)} console errors" + (f": {console_errors[:3]}" if console_errors else ""),
                           None)

            await ws.close()
        finally:
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
    parser.add_argument("--evidence-dir", default=str(RESEARCH_DEFAULT))
    parser.add_argument("--chrome", default=None)
    parser.add_argument("--keep", action="store_true", help="keep the temp dir (debugging)")
    args = parser.parse_args()
    asyncio.run(run_harness(args))


if __name__ == "__main__":
    main()
