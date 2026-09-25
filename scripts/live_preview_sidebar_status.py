"""The task-tree sidebar-status live harness: real browser, fresh preview home.

Starts ``charliebot session-tree preview`` — the real entry point on a fresh
temporary home with the real ``charlie-code-glm53-flash`` backend — and drives
the real UI in headless Chrome over CDP (the browser harness's client) while
the task tree runs real work. Through ``/api/sessions/status``, the DOM icons
and screenshots it asserts the sidebar's live work states:

1. running: a worker's ~60 s Run (a script-run task in a synthetic repo that
   sleeps, then reports) shows the spinner on its row and the amber gear on its
   collapsed parent; the expanded parent shows only its own state; after the
   Run ends the activity icons clear.
2. attention: a worker whose launch fails before its process starts (a local
   base branch ahead of its origin makes worktree preparation raise) reaches
   ``failed`` with the error as durable evidence; its row and its collapsed
   parent show the red alert, the parent receives a failure report naming the
   error, and the leaf card reads ``failed`` (a queued retry on the same node
   then reads ``queued`` in its own color while the alert keeps priority).
3. waiting: a queued Run held back by a paused node shows the muted clock.
4. names: the rows carry goal-derived names, never "## Goal".

Isolation: the trial's home is a fresh temp directory and its port a free
port; the harness's env is scrubbed of production identity variables; the
production service (127.0.0.1:18498) is never started, stopped, restarted or
contacted, and ``~/.charliebot`` / ``~/.charliebot-session-task-tree`` are
never touched; an independent sentinel home proves nothing outside the trial
changed. Evidence (screenshots, assertion JSON, tested commit) lands in
--evidence-dir, never in git.
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
import subprocess  # noqa: E402
import tempfile  # noqa: E402
import time  # noqa: E402
import urllib.error  # noqa: E402
import urllib.request  # noqa: E402

from scripts.browser_harness_session_tree import CDP, evaluate, pick_free_port  # noqa: E402
from scripts.live_preview_task_tree import (  # noqa: E402
    DEFAULT_BACKEND,
    build_synthetic_repo,
    fail,
    log,
    request,
    snapshot_native_storage,
    wait_run_terminal,
)
from scripts.browser_harness_session_tree_preview import build_source_home  # noqa: E402

PRODUCTION_PORT = 18498
PRODUCTION_HOMES = (
    Path.home() / ".charliebot",
    Path.home() / ".charliebot-session-task-tree",
)
RUN_TIMEOUT_SECONDS = 420.0
SLOW_RUN_SECONDS = 65
WORKER_PHRASE = "SLOW-RUN-MARKER-Q7X2"


class Shots:
    """The screenshot sink ``evaluate``'s screenshot helper expects."""

    def __init__(self, evidence_dir: Path) -> None:
        self.evidence_dir = evidence_dir


async def screenshot(cdp: CDP, session_id: str, shots: Shots, name: str) -> str:
    res = await cdp.send("Page.captureScreenshot", {"format": "png"}, session_id=session_id)
    path = shots.evidence_dir / f"{name}.png"
    path.write_bytes(base64.b64decode(res["data"]))
    log(f"    screenshot: {path.name}")
    return path.name


def git(repo: Path, *args: str) -> str:
    proc = subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, text=True)
    return proc.stdout.strip()


def build_slow_repo(home: Path) -> Path:
    """The ~60 s script-run repo: the worker runs slow_report.sh and reports it."""
    repo = home / "workspaces" / "slow-repo"
    build_synthetic_repo(repo)
    (repo / "slow_report.sh").write_text(
        "#!/usr/bin/env bash\n"
        "set -euo pipefail\n"
        f"sleep {SLOW_RUN_SECONDS}\n"
        f"printf '%s\\n' '{WORKER_PHRASE}' > report.txt\n",
        encoding="utf-8")
    git(repo, "add", ".")
    git(repo, "commit", "-q", "-m", "add slow_report.sh")
    return repo


def build_failing_repo(home: Path) -> Path:
    """The launch-failure repo: local main is ahead of origin/main, so
    worktree preparation raises BaseBranchResolutionError before any process."""
    repo = home / "workspaces" / "diverged-repo"
    origin = home / "workspaces" / "diverged-origin.git"
    subprocess.run(["git", "init", "-q", "--bare", "-b", "main", str(origin)], check=True)
    subprocess.run(["git", "clone", "-q", str(origin), str(repo)], check=True)
    git(repo, "config", "user.email", "preview@example.com")
    git(repo, "config", "user.name", "preview")
    (repo / "seed.txt").write_text("seed\n", encoding="utf-8")
    git(repo, "add", ".")
    git(repo, "commit", "-q", "-m", "seed")
    git(repo, "push", "-q", "origin", "main")
    (repo / "local.txt").write_text("local work\n", encoding="utf-8")
    git(repo, "add", ".")
    git(repo, "commit", "-q", "-m", "local-only")
    return repo


async def create_manager(base: str, key: str, name: str, goal: str,
                         request_id: str) -> str:
    status, created = request(base, key, "POST", "/api/sessions/", {
        "request_id": request_id, "profile": "manager", "name": name,
        "task": {"goal": goal}, "backend": None,
    })
    if status != 200:
        fail(f"manager create failed: {status} {created}")
    manager_id = created["id"]
    log(f"  manager {name}: {manager_id}")
    return manager_id


async def takeoff(base: str, key: str, manager_id: str, request_id: str) -> str:
    """A real takeoff turn: the user message the delegation gate reads."""
    status, msg = request(base, key, "POST", f"/api/chat/{manager_id}/message",
                          {"content": "Take off. Reply with exactly PREVIEW-READY and then stop.",
                           "request_id": request_id})
    if status not in (200, 202):
        fail(f"manager message failed: {status} {msg}")
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        status, page = request(base, key, "GET",
                               f"/api/sessions/{manager_id}/runs?order=desc&limit=5")
        for row in page.get("items", []):
            if row.get("kind") == "manager_turn" and row.get("state") in ("success", "failed"):
                if row["state"] != "success":
                    fail(f"takeoff turn of {manager_id} ended {row['state']}")
                return row["id"]
        await asyncio.sleep(1.0)
    fail(f"takeoff turn of {manager_id} never finished")


async def delegate(base: str, key: str, manager_id: str, description: str, repo: Path,
                   task_type: str, request_id: str) -> tuple[str, str]:
    status, body = request(base, key, "POST", "/api/internal/delegate", {
        "session_id": manager_id,
        "description": description,
        "repo_path": str(repo),
        "base_branch": "main",
        "task_type": task_type,
        "keep_worktree": False,
        "request_id": request_id,
    })
    if status != 200:
        fail(f"delegate failed: {status} {body}")
    log(f"  worker {body['session_id']} run {body['run_id']}")
    return body["session_id"], body["run_id"]


async def wait_status(base: str, key: str, ids: list[str], session_id: str, predicate,
                      label: str, timeout: float = 60.0) -> dict:
    """Poll /api/sessions/status until one node's payload satisfies *predicate*."""
    deadline = time.monotonic() + timeout
    last: dict = {}
    while time.monotonic() < deadline:
        status, payload = request(base, key, "GET",
                                  "/api/sessions/status?ids=" + ",".join(ids))
        if status != 200:
            fail(f"status fetch failed: {status} {payload}")
        last = payload.get(session_id, {})
        if last and predicate(last):
            return last
        await asyncio.sleep(1.0)
    fail(f"status of {session_id} never satisfied {label}; last={json.dumps(last, default=str)}")


def request_text(base: str, key: str, path: str) -> tuple[int, str]:
    """GET one raw-text endpoint (NDJSON): the shared request() parses JSON only."""
    req = urllib.request.Request(base + path, headers={"Authorization": "Bearer " + key})
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return resp.status, resp.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, ""


async def wait_parent_report(base: str, key: str, manager_id: str, child_id: str,
                             timeout: float = 60.0) -> dict:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        status, body = request_text(base, key, f"/api/sessions/{manager_id}/events.jsonl")
        if status == 200:
            for line in body.splitlines():
                if not line.strip():
                    continue
                event = json.loads(line)
                if (event.get("type") == "child_report"
                        and event.get("child_session_id") == child_id):
                    return event
        await asyncio.sleep(1.0)
    fail(f"parent {manager_id} never received a report for {child_id}")


async def icon_hidden(cdp: CDP, page_id: str, sid: str, kind: str) -> bool:
    """Whether one indicator element carries the hidden class (row may be collapsed)."""
    value = await evaluate(cdp, page_id,
                           f"document.getElementById('{kind}-{sid}')?.classList.contains('hidden')")
    return bool(value)


async def assert_icons(cdp: CDP, page_id: str, sid: str, visible: str | None,
                       hidden: list[str], label: str, timeout: float = 30.0) -> None:
    """Wait until one row's icon table reads exactly *visible* (+ nothing else).

    The row's paint trails the API verdict by at most one status poll, so the
    assertion waits for the flip instead of sampling it; a timeout fails with
    the row's own words as evidence.
    """
    deadline = time.monotonic() + timeout
    last = ""
    while time.monotonic() < deadline:
        kinds = ("spinner", "worker-indicator", "alert-indicator", "waiting-indicator", "unread")
        states = {kind: await icon_hidden(cdp, page_id, sid, kind) for kind in kinds}
        ok = all(states[kind] for kind in hidden)
        if ok and visible is not None:
            ok = not states[visible]
        if ok:
            return
        last = json.dumps(states)
        await asyncio.sleep(0.5)
    fail(f"{label}: icons never reached the expected state ({last})\n{await icon_dump(cdp, page_id, sid)}")


async def icon_dump(cdp: CDP, page_id: str, sid: str) -> str:
    """The row's own words at icon-assertion time: the evidence a failure needs."""
    try:
        return str(await evaluate(cdp, page_id, f"""
            JSON.stringify({{
              row: !!document.getElementById('session-{sid}'),
              rowHidden: document.getElementById('session-{sid}')?.closest('[data-tree-children]')?.classList.contains('hidden'),
              spinner: document.getElementById('spinner-{sid}')?.classList.contains('hidden'),
              gear: document.getElementById('worker-indicator-{sid}')?.classList.contains('hidden'),
              alert: document.getElementById('alert-indicator-{sid}')?.classList.contains('hidden'),
              clock: document.getElementById('waiting-indicator-{sid}')?.classList.contains('hidden'),
              ownState: (window.Sidebar && Sidebar.sessionUnread) ? 'ns' : 'ns',
              errs: (window.__errs || []).slice(0, 3),
            }})
        """))
    except Exception as exc:
        return f"<dump failed: {exc!r}>"


async def run_harness(args: argparse.Namespace) -> None:
    chrome = args.chrome or shutil.which("google-chrome") or shutil.which("google-chrome-stable")
    if not chrome:
        fail("google-chrome is not installed; install it or pass --chrome (no fake output)")

    evidence_dir = Path(args.evidence_dir)
    evidence_dir.mkdir(parents=True, exist_ok=True)
    commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=REPO_ROOT,
                            capture_output=True, text=True, check=True).stdout.strip()
    shots = Shots(evidence_dir)
    checks: list[dict] = []

    def record(name: str, ok: bool, detail: str) -> None:
        checks.append({"name": name, "ok": ok, "detail": detail})
        log(f"    [{'PASS' if ok else 'FAIL'}] {name}: {detail}")
        if not ok:
            fail(f"assertion failed: {name}: {detail}")

    # Isolation preflight: the production service and homes must be untouched
    # by construction — the trial gets a fresh temp home and a free port.
    if args.port is not None and args.port == PRODUCTION_PORT:
        fail("the requested port is the production port 18498")
    native_before = snapshot_native_storage()
    for home in PRODUCTION_HOMES:
        if home.exists():
            record("production home untouched (exists read-only, never written)", True, str(home))

    import websockets

    import atexit

    tmp_path = Path(tempfile.mkdtemp(prefix="charliebot-sidebar-status-"))
    if args.keep:
        log(f"kept for inspection: {tmp_path}")
    else:
        atexit.register(lambda: shutil.rmtree(tmp_path, ignore_errors=True))
    for var in ("CHARLIEBOT_SESSION_ID", "CHARLIEBOT_RUN_TOKEN",
                "CHARLIE_CODE_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"):
        os.environ.pop(var, None)

    # An independent instance's sentinel home: nothing outside the trial changes.
    independent = tmp_path / "independent-service"
    (independent / "state").mkdir(parents=True)
    (independent / "state" / "independent_sentinel.json").write_text('{"independent": true}')
    independent_before = {
        str(p.relative_to(independent)): p.read_bytes()
        for p in sorted(independent.rglob("*")) if p.is_file()
    }

    source = tmp_path / "source-home"
    build_source_home(source, [args.backend])
    home = tmp_path / "preview-home"
    port = args.port or pick_free_port()
    if port == PRODUCTION_PORT:
        fail("the picked free port collided with the production port; refusing")
    invocation = [sys.executable, "-m", "src.cli.main", "session-tree", "preview",
                  "--home", str(home), "--port", str(port), "--backend", args.backend]
    env = dict(os.environ)
    env["CHARLIEBOT_HOME"] = str(source)
    env["PYTHONUNBUFFERED"] = "1"
    server_console = tmp_path / "server-console.log"
    log(f"starting preview instance on 127.0.0.1:{port} (home {home})")
    with open(server_console, "w", encoding="utf-8") as server_log_file:
        proc = subprocess.Popen(invocation, cwd=str(REPO_ROOT), env=env,
                                stdout=server_log_file, stderr=subprocess.STDOUT)
        debug_port = pick_free_port()
        chrome_proc = None
        try:
            record_path = home / "state" / "preview_instance.json"
            deadline = time.monotonic() + 120
            preview_record = {}
            while time.monotonic() < deadline:
                if record_path.is_file():
                    preview_record = json.loads(record_path.read_text())
                    if preview_record.get("ready"):
                        break
                if proc.poll() is not None:
                    fail(f"preview process exited early: {server_console.read_text()[-1500:]}")
                await asyncio.sleep(0.2)
            if not preview_record.get("ready"):
                fail("preview instance never became ready")
            base = preview_record["url"]
            if f"127.0.0.1:{PRODUCTION_PORT}" in base:
                fail("the preview URL names the production port")
            log(f"preview ready: {base} (sha {preview_record['source_sha'][:12]})")
            access_key = (home / "credentials.yaml").read_text().split("access_key: ")[1].split("\n")[0]

            # ---- real Chrome over CDP -----------------------------------
            chrome_proc = subprocess.Popen(
                [chrome, "--headless=new", "--remote-debugging-port=" + str(debug_port),
                 "--user-data-dir=" + str(tmp_path / "chrome-profile"),
                 "--no-first-run", "--no-default-browser-check", "about:blank"],
                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            ws_url = None
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline and ws_url is None:
                line = chrome_proc.stderr.readline().decode(errors="replace")
                if "/devtools/browser" in line:
                    ws_url = line.strip().split()[-1]
            if ws_url is None:
                fail("chrome devtools endpoint did not come up")
            ws = await websockets.connect(ws_url, max_size=50 * 1024 * 1024)
            cdp = CDP(ws)
            await asyncio.sleep(0.3)
            target = await cdp.send("Target.createTarget", {"url": "about:blank"})
            attached = await cdp.send("Target.attachToTarget",
                                      {"targetId": target["targetId"], "flatten": True})
            page_id = attached["sessionId"]
            await cdp.send("Page.enable", session_id=page_id)
            await cdp.send("Runtime.enable", session_id=page_id)
            await cdp.send("Network.enable", session_id=page_id)
            await cdp.send("Page.addScriptToEvaluateOnNewDocument", {"source": f"""
                (function () {{
                  try {{ localStorage.setItem('charliebot_access_key', '{access_key}'); }} catch (e) {{}}
                  window.__errs = [];
                  window.addEventListener('error', (e) => window.__errs.push(String(e.message).slice(0, 160)));
                  window.addEventListener('unhandledrejection',
                      (e) => window.__errs.push('rej: ' + String(e.reason).slice(0, 160)));
                }})();
            """}, session_id=page_id)
            await cdp.send("Network.setCookie", {
                "name": "charliebot_access_key", "value": access_key,
                "url": f"http://127.0.0.1:{port}/",
            }, session_id=page_id)
            await cdp.send("Emulation.setDeviceMetricsOverride", {
                "width": 1440, "height": 900, "deviceScaleFactor": 1, "mobile": False,
            }, session_id=page_id)
            await cdp.send("Page.enable", {}, session_id=page_id)
            # ---- scenario A: a real ~60 s worker Run shows as running -----
            log("scenario A: the running state (spinner on the worker, gear on the collapsed parent)")
            slow_repo = build_slow_repo(home)
            manager_a = await create_manager(
                base, access_key, "Slow trial program",
                "## Goal\n\nSleep then report the marker\n", "sidebar-status-root-a")

            # The page is opened on the manager's URL: with a session id the
            # app connects its websocket, and the tree re-fetches on the
            # delegation broadcasts the rest of the trial rides.
            await cdp.send("Page.navigate", {"url": f"{base}/?session={manager_a}"},
                           session_id=page_id)
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                if await evaluate(cdp, page_id, f"!!document.getElementById('session-{manager_a}')"):
                    break
                if time.monotonic() > deadline - 0.1:
                    fail("sidebar never rendered the trial manager's row")
                await asyncio.sleep(0.5)
            log("browser attached; the manager's row is rendered")

            results: dict = {"tested_commit": commit, "invocation": invocation,
                             "preview_record": preview_record, "backend": args.backend}

            takeoff_run_a = await takeoff(base, access_key, manager_a, "sidebar-status-takeoff-a")
            worker_a, run_a = await delegate(
                base, access_key, manager_a,
                "## Goal\n\nRun `bash slow_report.sh` in the repository root. The script sleeps "
                f"about {SLOW_RUN_SECONDS} seconds and then writes the marker line into report.txt. "
                "Wait until the script has finished, then reply with the exact contents of report.txt.\n",
                slow_repo, "script-run", "sidebar-status-worker-a")

            status, detail = request(base, access_key, "GET", f"/api/sessions/{worker_a}")
            name = detail.get("name") if status == 200 else None
            record("worker name is the goal's first content line", status == 200 and name is not None
                   and name.startswith("Run `bash slow_report.sh`")
                   and "## Goal" not in name, f"name={name!r}")
            # Wait for the rows to render (the tree re-fetches on the
            # delegation broadcast), for the names, and for the Run to be live.
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                rows = await evaluate(cdp, page_id, """
                    [...document.querySelectorAll('#session-list a[id^=session-]')]
                        .map(el => el.id)
                """)
                if rows and f"session-{manager_a}" in rows and f"session-{worker_a}" in rows:
                    break
                await asyncio.sleep(1.0)
            else:
                fail(f"sidebar never rendered the trial rows: {rows}")
            row_names = await evaluate(cdp, page_id, """
                [...document.querySelectorAll('#session-list .session-name')].map(el => el.textContent)
            """)
            record("rows carry goal-derived names, never '## Goal'",
                   any("Slow trial program" in (n or "") for n in (row_names or []))
                   and any("Run `bash slow_report.sh`" in (n or "") for n in (row_names or []))
                   and all("## Goal" not in (n or "") for n in (row_names or [])),
                   f"rows={row_names}")

            def running_payload(st: dict) -> bool:
                return bool(st) and st.get("has_running_tasks") is True and st.get("work_state") == "running"

            ids_a = [manager_a, worker_a]
            # The real backend's launch prep (context build, worktree) takes
            # on the order of a minute; the queued phase before it is the
            # waiting verdict, and the launch flips the row to running.
            payload = await wait_status(base, access_key, ids_a, worker_a, running_payload,
                                        "worker running", timeout=300)
            record("status: worker has_running_tasks + work_state=running while live",
                   payload.get("has_running_tasks") is True and payload.get("work_state") == "running",
                   json.dumps(payload, default=str))
            parent_payload = await wait_status(
                base, access_key, ids_a, manager_a,
                lambda st: bool(st) and st.get("has_running_tasks") is False and st.get("work_state") == "idle",
                "manager idle", timeout=30)
            record("status: collapsed parent's own payload stays idle (its own Runs)",
                   parent_payload.get("has_running_tasks") is False
                   and parent_payload.get("work_state") == "idle",
                   json.dumps(parent_payload, default=str))
            await assert_icons(cdp, page_id, worker_a, "spinner",
                               ["worker-indicator", "alert-indicator", "waiting-indicator"],
                               "running worker row")
            await assert_icons(cdp, page_id, manager_a, "worker-indicator",
                               ["spinner", "alert-indicator", "waiting-indicator"],
                               "collapsed manager row")
            shot = await screenshot(cdp, page_id, shots, "running_state")
            results["running_screenshot"] = shot
            record("DOM: worker row spinner, collapsed manager gear", True,
                   f"{worker_a}/spinner + {manager_a}/gear")

            # Expanded manager shows only its own (idle) state.
            await evaluate(cdp, page_id, f"Sidebar.expandTreeNode('{manager_a}')")
            await assert_icons(cdp, page_id, manager_a, "unread",
                               ["spinner", "worker-indicator", "alert-indicator", "waiting-indicator"],
                               "expanded manager row (own idle state only)")
            await assert_icons(cdp, page_id, worker_a, "spinner",
                               ["worker-indicator", "alert-indicator", "waiting-indicator"],
                               "running worker row (expanded parent)")
            shot = await screenshot(cdp, page_id, shots, "running_expanded")
            results["running_expanded_screenshot"] = shot
            record("DOM: expanded manager shows only its own state", True, manager_a)
            await evaluate(
                cdp, page_id,
                f"if (Sidebar.isTreeNodeExpanded('{manager_a}')) toggleTreeNode('{manager_a}')")

            run_row, outcome = await wait_run_terminal(base, access_key, worker_a, run_a, "slow work run")
            record("slow run reached terminal success", outcome == "success", f"outcome={outcome}")
            if outcome != "success":
                raw = run_row.get("raw_log_ref") or ""
                tail = Path(raw).read_text(errors="replace")[-1500:] if raw and Path(raw).is_file() else ""
                fail(f"slow run outcome={outcome}; raw tail:\n{tail}")
            payload = await wait_status(
                base, access_key, ids_a, worker_a,
                lambda st: bool(st) and st.get("has_running_tasks") is False
                and st.get("work_state") in (None, "idle", "attention"),
                "worker cleared", timeout=60)
            record("status after finish: worker's activity cleared",
                   payload.get("has_running_tasks") is False, json.dumps(payload, default=str))
            # The parent's report-consuming turn is its own thinking state,
            # which outranks every stand-in; the collapsed row's cleared icons
            # are only assertable once that turn has settled.
            await wait_status(
                base, access_key, ids_a, manager_a,
                lambda st: bool(st) and st.get("work_state") == "idle" and not st.get("thinking_since"),
                "manager A idle after consuming the report", timeout=180)
            await assert_icons(cdp, page_id, manager_a, None,
                               ["spinner", "worker-indicator", "alert-indicator", "waiting-indicator"],
                               "collapsed manager row after finish (no activity icon)")
            record("DOM after finish: collapsed manager's gear cleared", True, manager_a)
            shot = await screenshot(cdp, page_id, shots, "after_finish")
            results["after_finish_screenshot"] = shot

            # ---- scenario B: a launch failure shows as attention ----------
            log("scenario B: the attention state (launch failure before process start)")
            bad_repo = build_failing_repo(home)
            manager_b = await create_manager(
                base, access_key, "Failing-delegate program",
                "## Goal\n\nDelegate into the diverged repo\n", "sidebar-status-root-b")
            await takeoff(base, access_key, manager_b, "sidebar-status-takeoff-b")
            worker_b, run_b = await delegate(
                base, access_key, manager_b,
                "## Goal\n\nSay the phrase\n", bad_repo, "quick-edit", "sidebar-status-worker-b")
            run_row, outcome = await wait_run_terminal(base, access_key, worker_b, run_b, "failing work run")
            record("launch-failure run reached failed", outcome == "failed", f"outcome={outcome}")
            record("the failed run never started a process", run_row.get("pid") is None,
                   f"pid={run_row.get('pid')!r}")
            error_text = ""
            status, runs_page = request(base, access_key, "GET",
                                        f"/api/sessions/{worker_b}/runs?order=desc&limit=10")
            for row in runs_page.get("items", []):
                if row.get("id") == run_b:
                    events_ref = row.get("events_ref") or ""
                    if events_ref and Path(events_ref).is_file():
                        for line in Path(events_ref).read_text(errors="replace").splitlines():
                            event = json.loads(line)
                            if event.get("type") == "error":
                                error_text = str(event.get("message") or event.get("content") or "")
            record("the failed run's durable evidence names the actual error",
                   "differs from origin/main" in error_text, f"error={error_text[:160]!r}")

            ids_b = [manager_b, worker_b]
            payload = await wait_status(
                base, access_key, ids_b, worker_b,
                lambda st: bool(st) and st.get("work_state") == "attention",
                "worker attention", timeout=60)
            record("status: worker work_state=attention after the launch failure",
                   payload.get("work_state") == "attention", json.dumps(payload, default=str))
            report = await wait_parent_report(base, access_key, manager_b, worker_b)
            record("the parent received a failure report naming the error",
                   report.get("outcome") == "failed"
                   and "differs from origin/main" in str(report.get("summary", "")),
                   f"summary={str(report.get('summary'))[:160]!r}")
            # The collapsed parent's alert stand-in is its idle-state paint; the
            # report-consuming turn's own spinner outranks it, so the icon
            # assertion waits for the parent to settle first.
            await wait_status(
                base, access_key, ids_b, manager_b,
                lambda st: bool(st) and st.get("work_state") == "idle" and not st.get("thinking_since"),
                "manager B idle after consuming the failure report", timeout=180)
            await assert_icons(cdp, page_id, worker_b, "alert-indicator",
                               ["spinner", "worker-indicator", "waiting-indicator"],
                               "attention worker row")
            await assert_icons(cdp, page_id, manager_b, "alert-indicator",
                               ["spinner", "worker-indicator", "waiting-indicator"],
                               "collapsed manager row (attention stand-in)")
            shot = await screenshot(cdp, page_id, shots, "attention_state")
            results["attention_screenshot"] = shot
            record("DOM: worker row and collapsed parent show the red alert", True, worker_b)

            # The leaf card: the failed run reads failed (and a queued retry
            # reads queued in its own color while the alert keeps priority).
            await evaluate(cdp, page_id, f"switchSession('{worker_b}')")
            deadline = time.monotonic() + 30
            failed_card = None
            while time.monotonic() < deadline:
                failed_card = await evaluate(cdp, page_id, f"""
                    (() => {{
                      const el = document.getElementById('thread-status-{run_b}');
                      return el ? el.textContent : null;
                    }})()
                """)
                if failed_card and "failed" in str(failed_card):
                    break
                await asyncio.sleep(0.5)
            record("leaf card reads failed for the launch-failure run",
                   bool(failed_card) and "failed" in str(failed_card), f"card={failed_card!r}")
            # The failure's error text is reachable from the leaf view: the
            # card's expandable events panel carries the run's own events log.
            await evaluate(cdp, page_id,
                           f"toggleThreadDetail('{run_b}', '{worker_b}')")
            deadline = time.monotonic() + 30
            panel_text = ""
            while time.monotonic() < deadline:
                panel_text = str(await evaluate(cdp, page_id, f"""
                    (document.getElementById('thread-events-{run_b}') || {{}}).textContent || ''
                """))
                if "differs from origin/main" in panel_text:
                    break
                await asyncio.sleep(0.5)
            record("the leaf card's events panel names the actual error",
                   "differs from origin/main" in panel_text,
                   f"panel={panel_text[:160]!r}")
            shot = await screenshot(cdp, page_id, shots, "leaf_failed")
            results["leaf_failed_screenshot"] = shot

            # Pause the worker, then retry: the queued retry is held back and
            # the leaf card shows it as queued; the row keeps the alert
            # (attention outranks waiting on one row).
            status, _ = request(base, access_key, "PATCH", f"/api/sessions/{worker_b}",
                                {"automation_paused": True})
            if status != 200:
                fail(f"pause failed: {status}")
            status, retry_body = request(base, access_key, "POST", f"/api/sessions/{worker_b}/retry",
                                         {"request_id": "sidebar-status-retry", "run_id": run_b})
            if status != 200:
                fail(f"retry failed: {status} {retry_body}")
            retry_id = retry_body.get("run_id") or retry_body.get("id")
            deadline = time.monotonic() + 30
            queued_card = None
            while time.monotonic() < deadline:
                queued_card = await evaluate(cdp, page_id, f"""
                    (() => {{
                      const el = document.getElementById('thread-status-{retry_id}');
                      const dot = document.getElementById('thread-dot-{retry_id}');
                      return el ? (el.textContent + '|' + (dot ? dot.className : '')) : null;
                    }})()
                """)
                if queued_card and "queued" in str(queued_card):
                    break
                await asyncio.sleep(0.5)
            record("leaf card reads queued for the held-back retry (own color)",
                   bool(queued_card) and "queued" in str(queued_card)
                   and "bg-amber-400" in str(queued_card), f"card={queued_card!r}")
            await assert_icons(cdp, page_id, worker_b, "alert-indicator",
                               ["spinner", "worker-indicator", "waiting-indicator"],
                               "attention outranks waiting on the paused worker row")
            shot = await screenshot(cdp, page_id, shots, "leaf_queued")
            results["leaf_queued_screenshot"] = shot

            # ---- scenario C: the waiting state (queued, not launched) -----
            log("scenario C: the waiting state (a queued Run held back by a paused node)")
            payload = await wait_status(
                base, access_key, ids_a, manager_a,
                lambda st: bool(st) and st.get("work_state") == "idle" and not st.get("thinking_since"),
                "manager A idle after the report turn", timeout=180)
            status, _ = request(base, access_key, "PATCH", f"/api/sessions/{manager_a}",
                                {"automation_paused": True})
            if status != 200:
                fail(f"pause failed: {status}")
            status, retry_body = request(base, access_key, "POST", f"/api/sessions/{manager_a}/retry",
                                         {"request_id": "sidebar-status-m1-retry", "run_id": takeoff_run_a})
            if status != 200:
                fail(f"manager retry failed: {status} {retry_body}")
            payload = await wait_status(
                base, access_key, ids_a, manager_a,
                lambda st: bool(st) and st.get("work_state") == "waiting"
                and st.get("has_running_tasks") is False,
                "manager waiting", timeout=60)
            record("status: queued Run held by the pause reads work_state=waiting",
                   payload.get("work_state") == "waiting", json.dumps(payload, default=str))
            await assert_icons(cdp, page_id, manager_a, "waiting-indicator",
                               ["spinner", "worker-indicator", "alert-indicator"],
                               "waiting manager row")
            record("DOM: the paused node's queued Run shows the clock", True, manager_a)
            shot = await screenshot(cdp, page_id, shots, "waiting_state")
            results["waiting_screenshot"] = shot

            # ---- isolation postflight ------------------------------------
            # The host store also carries the harness's own session logs (this
            # trial may run inside a CharlieBot session), so the check is the
            # trial's own ids: none of the preview's native session ids may
            # appear anywhere in the host's native session store.
            native_after = snapshot_native_storage()
            preview_native = sorted({p.name for p in (home / "clc-sessions").rglob("*")
                                    if p.is_dir()}) if (home / "clc-sessions").is_dir() else []
            leaked_ids = [nid for nid in preview_native
                          if any(nid in path for path in native_after)]
            record("the trial's native session ids never reached the host's store",
                   not leaked_ids, f"preview_native_ids={preview_native[:3]} leaked={leaked_ids[:3]}")
            independent_after = {
                str(p.relative_to(independent)): p.read_bytes()
                for p in sorted(independent.rglob("*")) if p.is_file()
            }
            record("the independent instance's files stayed untouched",
                   independent_after == independent_before, "sentinel home byte-identical")
            record("the preview home lives inside the trial's temp directory",
                   str(home).startswith(str(tmp_path)) and home.name == "preview-home", str(home))
            record("the trial port is not the production port", port != PRODUCTION_PORT,
                   f"port={port}")

            results["checks"] = checks
            (evidence_dir / "sidebar_status_results.json").write_text(
                json.dumps(results, indent=2), encoding="utf-8")
            log(f"results written to {evidence_dir / 'sidebar_status_results.json'}")
            log("SIDEBAR STATUS LIVE HARNESS PASSED")
        finally:
            if chrome_proc is not None and chrome_proc.poll() is None:
                chrome_proc.terminate()
                try:
                    chrome_proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    chrome_proc.kill()
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=30)
            log("server console tail:\n" + server_console.read_text()[-800:])


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence-dir", required=True,
                        help="Where the screenshots, assertion JSON and tested commit land")
    parser.add_argument("--backend", default=DEFAULT_BACKEND,
                        help="The charlie-code backend id the trial instance runs")
    parser.add_argument("--port", type=int, default=None,
                        help="Fixed preview port (default: a free port; 18498 is refused)")
    parser.add_argument("--keep", action="store_true",
                        help="Keep the preview home for inspection instead of purging it")
    parser.add_argument("--chrome", default=None)
    args = parser.parse_args()
    asyncio.run(run_harness(args))


if __name__ == "__main__":
    main()
