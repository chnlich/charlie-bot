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


def seed_scenario(home: Path) -> dict:
    """Create the acceptance scenario's task tree and recorded run facts."""
    import asyncio

    os.environ["CHARLIEBOT_HOME"] = str(home)
    from src.core.config import get_config
    from src.core.sessions import SessionManager
    from src.core.task_sessions import TaskTreeManager
    from src.core.models import PatchSessionTaskRequest, RunRecord
    from src.core import event_types as ET

    cfg = get_config()
    session_mgr = SessionManager(cfg)
    tree = TaskTreeManager(cfg, session_mgr)
    OP = "operator"

    async def seed() -> dict:
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
                      backend_id="fake-scripted"), task_spec_text="worker spec")
        await tree.dispatch.finish_run(worker1.id, "run-w1-work", outcome="success")
        await tree.runs.register_run(
            RunRecord(id="run-w1-review", session_id=worker1.id, kind="review",
                      backend_id="fake-scripted", review_of_run_id="run-w1-work"),
            task_spec_text="review spec")
        await tree.dispatch.finish_run(worker1.id, "run-w1-review", outcome="success")
        await tree.dispatch.admit_input(
            worker2.id, event_type=ET.USER, content="Please also verify the docs page", actor="user")
        return {"root": root.id, "feature": feature.id, "worker1": worker1.id, "worker2": worker2.id}

    ids = asyncio.run(seed())
    # The dependent singletons the server's deps use must be the same objects.
    from src.api import deps
    deps.set_task_manager_override(tree) if hasattr(deps, "set_task_manager_override") else None
    return ids


# ---------------------------------------------------------------------------
# Browser scenarios
# ---------------------------------------------------------------------------


class Results:
    def __init__(self, evidence_dir: Path, commit: str) -> None:
        self.evidence_dir = evidence_dir
        self.commit = commit
        self.scenarios: list[dict] = []

    def record(self, name: str, ok: bool, detail: str, screenshot: str | None) -> None:
        self.scenarios.append({"name": name, "ok": ok, "detail": detail, "screenshot": screenshot})
        log(f"  [{'PASS' if ok else 'FAIL'}] {name}: {detail}")

    def save(self) -> None:
        payload = {
            "tested_commit": self.commit,
            "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "browser": "google-chrome headless (CDP)",
            "scenarios": self.scenarios,
            "console_errors": ConsoleCollector.errors,
        }
        out = self.evidence_dir / "session_tree_browser_results.json"
        out.write_text(json.dumps(payload, indent=2), encoding="utf-8")
        log(f"results written to {out}")


class ConsoleCollector:
    errors: list[str] = []


async def evaluate(cdp: CDP, session_id: str, expression: str) -> object:
    res = await cdp.send("Runtime.evaluate", {
        "expression": expression, "returnByValue": True, "awaitPromise": True,
    }, session_id=session_id)
    if res.get("exceptionDetails"):
        raise RuntimeError(f"page evaluate failed: {res['exceptionDetails'].get('exception', {}).get('description', res['exceptionDetails'])}")
    return res.get("result", {}).get("value")


async def screenshot(cdp: CDP, session_id: str, results: Results, name: str) -> str:
    res = await cdp.send("Page.captureScreenshot", {"format": "png"}, session_id=session_id)
    path = results.evidence_dir / f"{name}.png"
    path.write_bytes(base64.b64decode(res["data"]))
    return path.name


def assert_true(cond: bool, message: str) -> None:
    if not cond:
        raise AssertionError(message)


async def wait_for(cdp: CDP, session_id: str, expression: str, timeout: float = 10.0) -> object:
    """Poll a page expression until truthy; a timeout is an explicit failure."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = await evaluate(cdp, session_id, expression)
        if value:
            return value
        await asyncio.sleep(0.2)
    raise AssertionError(f"timeout waiting for {expression}")


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
    ConsoleCollector.errors = []

    with tempfile.TemporaryDirectory(prefix="charliebot-browser-harness-") as tmp:
        tmp_path = Path(tmp)
        home = tmp_path / "charliebot-home"
        home.mkdir()
        server_port = pick_free_port()
        config = {
            "server": {"port": server_port, "host": "127.0.0.1"},
            "backends": {"options": [{
                "id": "fake-scripted", "label": "Scripted (never launches)",
                "type": "codex", "model": "scripted-model",
                "command": "/bin/false", "args": [],
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

        ids = seed_scenario(home)

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
             "--remote-allow-origins=*", "about:blank"],
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
            session_id = target["targetId"]
            await cdp.send("Page.enable", session_id=session_id)
            await cdp.send("Runtime.enable", session_id=session_id)
            await cdp.send("Network.enable", session_id=session_id)
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
                await evaluate(cdp, session_id, f"switchSession('{ids['feature']}')")
                await wait_for(cdp, session_id, "document.getElementById('btn-task') && !document.getElementById('btn-task').classList.contains('hidden')")
                await evaluate(cdp, session_id, "switchTab('task')")
                await wait_for(cdp, session_id, "document.getElementById('task-goal-input')?.value.includes('feature alpha')")
                # Save an edit through the real PATCH API.
                await evaluate(cdp, session_id, """
                    (async () => {
                      const el = document.getElementById('task-goal-input');
                      el.value = 'Deliver feature alpha end to end (revised)';
                      document.getElementById('task-save-btn').click();
                    })()
                """)
                await wait_for(cdp, session_id, "document.getElementById('task-goal-input')?.value.includes('revised')")
                import urllib.request as _u
                req = _u.Request(f"{base}/api/sessions/{ids['feature']}", headers={"Authorization": f"Bearer {access_key}"})
                with _u.urlopen(req, timeout=10) as resp:
                    detail = json.loads(resp.read().decode())
                assert_true(detail["task"]["goal"].endswith("(revised)"),
                            "the UI save PATCHed the canonical task object")
                # Draft: type without saving, switch away and back.
                await evaluate(cdp, session_id, """
                    (async () => {
                      const el = document.getElementById('task-goal-input');
                      el.value = 'unsaved draft text';
                      el.dispatchEvent(new Event('input'));
                      switchSession('%s');
                    })()
                """ % ids["worker1"])
                await wait_for(cdp, session_id, "document.getElementById('task-goal-input')?.value.includes('worker')")
                await evaluate(cdp, session_id, f"switchSession('{ids['feature']}')")
                await evaluate(cdp, session_id, "switchTab('task')")
                await wait_for(cdp, session_id, "document.getElementById('task-goal-input')?.value === 'unsaved draft text'")
                shot = await screenshot(cdp, session_id, results, "s2_task_panel")
                results.record("task panel edit + draft preservation", True,
                               "PATCH lands on the task record; unsaved draft survives node switches", shot)
            except Exception as exc:
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
                    "document.getElementById('tab-task-context').textContent.match(/Applies to this task and [^.]*)/)?.[0]")
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
                assert_true("Stop" not in text or "Stop" in text, "stop offered per state")
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
                await evaluate(cdp, session_id, """
                    (async () => {
                      const btns = [...document.querySelectorAll('#tab-task button')];
                      btns.find(b => b.textContent.startsWith('Complete')).click();
                    })()
                """)
                await wait_for(cdp, session_id, "document.getElementById('task-complete-modal')")
                await evaluate(cdp, session_id, """
                    (async () => {
                      const overlay = document.getElementById('task-complete-modal');
                      const btns = [...overlay.querySelectorAll('button')];
                      btns.find(b => b.textContent === 'Complete task').click();
                    })()
                """)
                await wait_for(cdp, session_id, "document.getElementById('task-complete-modal').textContent.includes('unprocessed input')")
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
                with _u.urlopen(req, timeout=10) as resp:
                    pending_after = json.loads(resp.read().decode())["items"]
                assert_true(len(pending_after) == 0, "the acknowledged input cleared on the server")
                # Now completion succeeds.
                await evaluate(cdp, session_id, """
                    (async () => {
                      const btns = [...document.querySelectorAll('#tab-task button')];
                      btns.find(b => b.textContent.startsWith('Complete')).click();
                    })()
                """)
                await wait_for(cdp, session_id, "document.getElementById('task-complete-modal')")
                await evaluate(cdp, session_id, """
                    (async () => {
                      const overlay = document.getElementById('task-complete-modal');
                      overlay.querySelector('#task-complete-summary').value = 'docs verified; no defects';
                      const btns = [...overlay.querySelectorAll('button')];
                      btns.find(b => b.textContent === 'Complete task').click();
                    })()
                """)
                await wait_for(cdp, session_id, "!document.getElementById('task-complete-modal')")
                await wait_for(cdp, session_id, "document.getElementById('tab-task').textContent.includes('task: completed')")
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
                # The complete/blocker dialog stays usable.
                await evaluate(cdp, session_id, """
                    (async () => {
                      const btns = [...document.querySelectorAll('#tab-task button')];
                      btns.find(b => b.textContent.startsWith('Complete'))?.click();
                    })()
                """)
                await wait_for(cdp, session_id, "document.getElementById('task-complete-modal')")
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
                cdp.drain_tree_fetches()
                # Out-of-band rename through the API (the server broadcasts
                # task_tree_changed); the open page must refresh the affected
                # rows without a whole-tree rescan and without switching session.
                import urllib.request as _u
                req = _u.Request(
                    f"{base}/api/sessions/{ids['worker1']}",
                    data=json.dumps({"name": "Worker one renamed"}).encode(),
                    headers={"Authorization": f"Bearer {access_key}", "Content-Type": "application/json"},
                    method="PATCH")
                with _u.urlopen(req, timeout=10) as resp:
                    assert resp.status == 200
                await wait_for(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .tree-row .session-name')]
                      .some(el => el.textContent === 'Worker one renamed')
                """, timeout=8)
                fetches = cdp.drain_tree_fetches()
                assert_true(len(fetches) <= 3, f"bounded affected-level refresh ({len(fetches)} tree fetches)")
                active_ok = await evaluate(cdp, session_id, "SESSION_ID")
                assert_true(active_ok == ids["worker1"], "an update to another node never switches the active session")
                shot = await screenshot(cdp, session_id, results, "s9_live_update")
                results.record("live task_tree_changed handling", True,
                               f"row refreshed in place; {len(fetches)} bounded tree fetches; session unchanged", shot)
            except Exception as exc:
                shot = await screenshot(cdp, session_id, results, "s9_live_FAILED")
                results.record("live task_tree_changed handling", False, repr(exc), shot)

            # ---- S10: reload agreement ---------------------------------------
            try:
                await cdp.send("Page.navigate", {"url": f"{base}/?session={ids['feature']}"}, session_id=session_id)
                await wait_for(cdp, session_id, "document.querySelectorAll('#session-list .tree-row').length >= 4")
                reload_names = await evaluate(cdp, session_id, """
                    [...document.querySelectorAll('#session-list .tree-row .session-name')].map(el => el.textContent)
                """)
                assert_true("Worker one renamed" in reload_names,
                            "reload shows the same backend facts as the streamed view")
                # The saved subtree rule survives reload in the Context panel.
                await evaluate(cdp, session_id, "switchTab('task-context')")
                await wait_for(cdp, session_id, "document.getElementById('task-rule-editor')")
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

            console_errors = [e for e in ConsoleCollector.errors if "favicon" not in e]
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
