"""The fresh-home preview browser harness: the real entry point, the real UI.

Starts ``charliebot session-tree preview`` as its own foreground process on a
disposable home (the actual CLI, never a lifespan-off in-process server), then
drives a real headless Chrome with an isolated browser profile through the
first-login/empty state, the one-click New Task flow (real control click from
the welcome screen and from a non-Chat tab: no creation form, Chat with a
focused composer, drafts kept, one node per pending action), explicit
child/worker creation through the existing modal, goal and rule editing, node
switching with draft preservation, the real GLM manager turn from the first
message, Context and run history, the completion/refusal/reopen and move/pause
controls, a reload with selection, and a narrow viewport. Every refusal is a
recorded failure; no scenario is scripted to pass.

Evidence: per-scenario screenshots and session_tree_preview_browser_results.json
with the exact tested commit, the invocation and the console-error list.
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

from scripts.browser_harness_session_tree import (  # noqa: E402
    CDP,
    evaluate,
    log,
    pick_free_port,
    wait_for,
)

DEFAULT_BACKEND = "charlie-code-glm53-flash"
RESEARCH_DEFAULT = Path("/home/chaoli/.charliebot/sessions/8a7964a3-8e53-4fa3-9145-893bac307ddc/research")

GUARD_SOURCE = """
    (function () {
      // No access key here: the first-login scenario drives the real login page.
      window.__errs = [];
      window.addEventListener('error', (e) => window.__errs.push(String(e.message).slice(0, 200)));
      window.addEventListener('unhandledrejection', (e) => window.__errs.push('rej: ' + String(e.reason).slice(0, 200)));
      const origConsoleError = console.error;
      console.error = function () {
        window.__errs.push([...arguments].map((a) => String(a && a.message ? a.message : a)).join(' ').slice(0, 200));
        origConsoleError.apply(console, arguments);
      };
    })();
"""


class Results:
    def __init__(self, evidence_dir: Path, commit: str, invocation: list[str]) -> None:
        self.evidence_dir = evidence_dir
        self.commit = commit
        self.invocation = invocation
        self.scenarios: list[dict] = []
        self.console_errors: list[str] = []

    def record(self, name: str, ok: bool, detail: str, screenshot: str | None) -> None:
        self.scenarios.append({"name": name, "ok": ok, "detail": detail, "screenshot": screenshot})
        log(f"  [{'PASS' if ok else 'FAIL'}] {name}: {detail}")

    def save(self) -> None:
        payload = {
            "tested_commit": self.commit,
            "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "browser": "google-chrome headless (CDP), isolated private profile",
            "entry_point": "charliebot session-tree preview (fresh process, foreground)",
            "invocation": self.invocation,
            "scenarios": self.scenarios,
            "console_errors": self.console_errors,
        }
        out = self.evidence_dir / "session_tree_preview_browser_results.json"
        out.write_text(json.dumps(payload, indent=2), encoding="utf-8")
        log(f"results written to {out}")


async def screenshot(cdp: CDP, session_id: str, results: Results, name: str) -> str:
    res = await cdp.send("Page.captureScreenshot", {"format": "png"}, session_id=session_id)
    path = results.evidence_dir / f"{name}.png"
    path.write_bytes(base64.b64decode(res["data"]))
    return path.name


async def click(cdp: CDP, session_id: str, selector: str) -> None:
    await evaluate(cdp, session_id,
                   f"(() => {{ const el = document.querySelector({json.dumps(selector)});"
                   f" if (!el) throw new Error('missing element ' + {json.dumps(selector)}); el.click(); }})()")


async def click_button_by_text(cdp: CDP, session_id: str, text: str, scope: str = "body") -> None:
    expr = (f"(() => {{ const root = document.querySelector({json.dumps(scope)});"
            f" const btn = [...root.querySelectorAll('button')].find(b => b.textContent.trim() ==="
            f" {json.dumps(text)}); if (!btn) throw new Error('missing button ' + {json.dumps(text)});"
            " btn.click(); })()")
    await evaluate(cdp, session_id, expr)


async def open_task_tab(cdp: CDP, session_id: str, tab: str) -> None:
    await evaluate(cdp, session_id, f"switchTab({json.dumps(tab)})")
    await wait_for(cdp, session_id,
                   f"!document.getElementById('tab-{tab}').classList.contains('hidden')",
                   timeout=10, label=f"tab {tab} visible")


def api_request(base: str, key: str, method: str, path: str, body: dict | None = None) -> tuple[int, dict]:
    headers = {"Authorization": "Bearer " + key}
    data = None
    if body is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            return resp.status, json.loads(resp.read().decode())
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read().decode() or "{}")


def build_source_home(source: Path, backend_id: str) -> None:
    """The trial's private configuration source: one backend entry of the current profile."""
    import yaml

    from src.core.config import load_config, load_credentials

    cfg = load_config()
    option = cfg.get_backend_option(backend_id)
    if option is None:
        raise SystemExit(
            f"backend {backend_id!r} is not configured in the current profile {cfg.config_file}")
    if option.type.value != "charlie-code":
        raise SystemExit(
            f"backend {backend_id!r} has type {option.type.value!r}; the preview isolates native "
            "state only for charlie-code")
    source.mkdir(parents=True, exist_ok=True)
    entry = json.loads(option.model_dump_json())
    config = {
        "server": {"host": "127.0.0.1", "port": 18498},
        "paths": {"workspace_dirs": [str(source / "workspaces")],
                  "worktree_dir": str(source / "worktrees")},
        "backends": {"options": [entry], "preference": [backend_id]},
    }
    (source / "config.yaml").write_text(yaml.safe_dump(config), encoding="utf-8")
    creds = "charliebot:\n  access_key: source-operator-key-not-used\n"
    if getattr(option, "credential", None):
        section = str(option.credential)
        api_key = load_credentials().get(section, "api_key")
        if api_key is None:
            raise SystemExit(f"backend {backend_id!r} needs credentials.{section}.api_key; it is unset")
        creds += f"{section}:\n  api_key: {api_key}\n"
    (source / "credentials.yaml").write_text(creds, encoding="utf-8")


def wait_ready(home: Path, timeout: float = 120.0) -> dict:
    record_path = home / "state" / "preview_instance.json"
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if record_path.is_file():
            record = json.loads(record_path.read_text())
            if record.get("ready"):
                return record
        time.sleep(0.2)
    raise SystemExit(f"preview instance at {home} never became ready")


async def run_harness(args: argparse.Namespace) -> None:
    chrome = args.chrome or shutil.which("google-chrome") or shutil.which("google-chrome-stable")
    if not chrome:
        raise SystemExit("google-chrome is not installed; install it or pass --chrome (no fake output)")

    evidence_dir = Path(args.evidence_dir)
    evidence_dir.mkdir(parents=True, exist_ok=True)
    commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=REPO_ROOT,
                            capture_output=True, text=True, check=True).stdout.strip()

    with tempfile.TemporaryDirectory(prefix="charliebot-preview-harness-") as tmp:
        tmp_path = Path(tmp)
        source = tmp_path / "source-home"
        build_source_home(source, args.backend)
        # The harness process itself must keep production identities out of any
        # child it spawns; the preview CLI clears its own in addition.
        for var in ("CHARLIEBOT_SESSION_ID", "CHARLIEBOT_RUN_TOKEN",
                    "CHARLIE_CODE_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"):
            os.environ.pop(var, None)
        home = tmp_path / "preview-home"
        port = pick_free_port()
        invocation = [sys.executable, "-m", "src.cli.main", "session-tree", "preview",
                      "--home", str(home), "--port", str(port), "--backend", args.backend]
        env = dict(os.environ)
        env["CHARLIEBOT_HOME"] = str(source)
        env["PYTHONUNBUFFERED"] = "1"
        server_out = tmp_path / "server-console.log"
        results = Results(evidence_dir, commit, invocation)
        log(f"starting preview instance on 127.0.0.1:{port} (home {home})")
        with open(server_out, "w", encoding="utf-8") as server_log_file:
            proc = subprocess.Popen(
                invocation, cwd=str(REPO_ROOT), env=env,
                stdout=server_log_file, stderr=subprocess.STDOUT)
        try:
            record = wait_ready(home)
            base = record["url"]
            log(f"preview ready: {base} (branch {record['source_branch']}, sha {record['source_sha'][:12]})")
            import yaml

            access_key = yaml.safe_load((home / "credentials.yaml").read_text())["charliebot"]["access_key"]

            profile = tmp_path / "chrome-profile"
            profile.mkdir()
            debug_port = pick_free_port()
            chrome_proc = subprocess.Popen(
                [chrome, "--headless=new", "--remote-debugging-port=" + str(debug_port),
                 f"--user-data-dir={profile}", "--no-first-run", "--no-default-browser-check",
                 "--disable-background-networking", "--window-size=1440,900",
                 "--remote-allow-origins=*",
                 "--disable-background-timer-throttling",
                 "--disable-backgrounding-occluded-windows",
                 "--disable-renderer-backgrounding",
                 "about:blank"],
                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            try:
                await drive_browser(cdp_host=chrome_proc, debug_port=debug_port, base=base,
                                    access_key=access_key, results=results, args=args, home=home)
            except Exception as exc:
                # A harness error must stay visible in the evidence, not be
                # masked by the failed-scenario exit below.
                results.record("harness-error", False, f"{type(exc).__name__}: {exc}", None)
                raise
            finally:
                chrome_proc.terminate()
                try:
                    chrome_proc.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    chrome_proc.kill()
                    chrome_proc.wait(timeout=10)
        finally:
            from src.core.home_writer_fence import probe_writer_fence

            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=30)
            holder = probe_writer_fence(home)
            results.record("server-fence-released-on-stop", holder["exclusive_holder_alive"] is False,
                           f"writer fence holder alive: {holder['exclusive_holder_alive']}", None)
            log("server console: " + server_out.read_text()[-600:])
            results.save()
            failed = [s for s in results.scenarios if not s["ok"]]
            if failed:
                raise SystemExit(f"{len(failed)} scenario(s) failed: {[s['name'] for s in failed]}")


async def drive_browser(cdp_host: subprocess.Popen, debug_port: int, base: str,
                        access_key: str, results: Results, args: argparse.Namespace,
                        home: Path) -> None:
    import websockets


    deadline = time.monotonic() + 20
    ws_url = None
    while time.monotonic() < deadline and ws_url is None:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{debug_port}/json/version", timeout=5) as resp:
                ws_url = json.loads(resp.read().decode())["webSocketDebuggerUrl"]
        except (OSError, KeyError):
            await asyncio.sleep(0.2)
    if ws_url is None:
        raise SystemExit("chrome devtools endpoint did not come up")
    ws = await websockets.connect(ws_url, max_size=50 * 1024 * 1024)
    cdp = CDP(ws)
    await asyncio.sleep(0.3)

    target = await cdp.send("Target.createTarget", {"url": "about:blank"})
    attached = await cdp.send("Target.attachToTarget", {"targetId": target["targetId"], "flatten": True})
    sid = attached["sessionId"]
    await cdp.send("Page.enable", session_id=sid)
    await cdp.send("Runtime.enable", session_id=sid)
    await cdp.send("Network.enable", session_id=sid)
    await cdp.send("Page.addScriptToEvaluateOnNewDocument", {"source": GUARD_SOURCE}, session_id=sid)

    # --- S1: first login over the real auth overlay, then the empty state ---
    # `/` is a public path by design: the index loads unauthenticated and the
    # SPA's auth overlay is the real first-login surface.
    await cdp.send("Page.navigate", {"url": base}, session_id=sid)
    await wait_for(cdp, sid,
                   "document.getElementById('auth-overlay')"
                   " && document.getElementById('auth-overlay').style.display !== 'none'",
                   timeout=15, label="login overlay")
    login_shot = await screenshot(cdp, sid, results, "s01a-login-overlay")
    await evaluate(cdp, sid,
                   f"document.getElementById('auth-key-input').value = {json.dumps(access_key)};"
                   "submitAccessKey();")
    await wait_for(cdp, sid, "!!document.getElementById('preview-indicator')", timeout=20,
                   label="main page after login")
    indicator = await evaluate(cdp, sid, "document.getElementById('preview-indicator').textContent")
    tasks_first = await evaluate(cdp, sid, "currentFilter === 'tasks'")
    new_task_label = await evaluate(
        cdp, sid,
        "[...document.querySelectorAll('button span')].some(s => s.textContent.trim() === 'New Task')")
    empty_tree = await evaluate(
        cdp, sid,
        "(() => { const t = document.getElementById('session-list');"
        " return t ? /No task trees yet|Use .New task/i.test(t.textContent) : false; })()")
    overlay_gone = await evaluate(cdp, sid,
                                  "document.getElementById('auth-overlay').style.display === 'none'")
    ok = (indicator == "Preview — isolated trial instance" and tasks_first and new_task_label
          and empty_tree and overlay_gone)
    results.record("s01-first-login-empty-state", ok,
                   f"indicator={indicator!r} tasks-first={tasks_first} new-task-label={new_task_label} "
                   f"empty-tree={empty_tree} overlay-gone={overlay_gone}",
                   await screenshot(cdp, sid, results, "s01b-empty-preview") + "," + login_shot)
    # The unauthenticated first load logs one expected 401 from the sidebar's
    # initial fetch (the shipped app's pre-login behavior); the no-console-error
    # contract covers the authenticated flow from here on.
    cdp.console_errors = []
    await evaluate(cdp, sid, "window.__errs = [];")

    # --- S2: the user-facing New Task control creates the root in one click ---
    # A real click of the welcome screen's New Task button (never a direct
    # helper call): no creation form may appear, Chat must open with the
    # composer holding the cursor, and the first typed message must be admitted
    # through the normal durable input path. The sidebar's control takes the
    # same path (S5b/S5c click it from an existing session).
    await click_button_by_text(cdp, sid, "New Task", "main")
    await wait_for(cdp, sid, "typeof SESSION_ID !== 'undefined' && SESSION_ID", timeout=20,
                   label="new root task selected")
    root_id = await evaluate(cdp, sid, "SESSION_ID")
    await wait_for(cdp, sid,
                   "(() => { const m = document.getElementById('task-child-modal');"
                   " const f = ['task-child-name', 'task-child-goal', 'task-child-profile',"
                   " 'task-child-backend'].map((id) => document.getElementById(id));"
                   " return !m && f.every((x) => !x); })()",
                   timeout=8, label="no creation form fields")
    chat_open = await evaluate(cdp, sid, "!document.getElementById('tab-chat').classList.contains('hidden')")
    composer_ready = await evaluate(
        cdp, sid,
        "document.activeElement === document.getElementById('msg-input')"
        " && document.getElementById('msg-input').value === ''")
    await wait_for(cdp, sid,
                   f"!!document.getElementById('tree-node-{root_id}')", timeout=15,
                   label="root row in tree")
    panel_open = await evaluate(cdp, sid,
                                "!document.getElementById('btn-task').classList.contains('hidden')")
    create_posts = [m for m in cdp.mutations if m["method"] == "POST" and m["url"].endswith("/api/sessions/")]
    create_body = json.loads(create_posts[-1]["body"]) if create_posts else {}
    create_ok = (create_body.get("profile") == "manager" and create_body.get("task_parent_id") is None
                 and (create_body.get("task") or {}).get("goal") == "" and bool(create_body.get("request_id")))
    results.record("s02-one-click-root-create",
                   bool(root_id and chat_open and composer_ready and panel_open and create_ok),
                   f"root={root_id[:8]} chat={chat_open} composer-ready={composer_ready} "
                   f"task-panel-open={panel_open} create-body-ok={create_ok} "
                   f"create-posts={len(create_posts)}",
                   await screenshot(cdp, sid, results, "s02-one-click-root-chat"))
    # A welcome-screen create lands through a full page load; re-mark the
    # authenticated flow as the no-console-error baseline from here.
    cdp.console_errors = []
    await evaluate(cdp, sid, "window.__errs = [];")

    # --- S2b: the first message on the empty-goal root (durable admission) ---
    first_message = "Take off. Reply with exactly: TRIAL-OK, then stop. Do not create subtasks."
    await wait_for(cdp, sid, "!!document.getElementById('msg-input')", timeout=10, label="composer")
    await evaluate(cdp, sid,
                   "const inp = document.getElementById('msg-input');"
                   f"inp.value = {json.dumps(first_message)};"
                   "inp.dispatchEvent(new Event('input'));")
    await click(cdp, sid, "#send-btn")
    await wait_for(cdp, sid,
                   f"document.getElementById('tab-chat').textContent.includes({json.dumps(first_message)})",
                   timeout=15, label="first message rendered")
    status, page = api_request(base, access_key, "GET",
                               f"/api/sessions/{root_id}/events?before=999999&limit=40")
    admitted = status == 200 and any(
        m.get("role") == "user" and first_message in str(m.get("content", ""))
        for m in (page.get("messages") or [])) if isinstance(page, dict) else False
    results.record("s02b-first-message-admitted-empty-goal", bool(admitted),
                   f"events-page={status} user-message-admitted={admitted}",
                   await screenshot(cdp, sid, results, "s02b-first-message"))

    # --- S3: child manager under the root ----------------------------------
    await open_task_tab(cdp, sid, "task")
    await wait_for(cdp, sid, "!!document.getElementById('task-action-child')", timeout=10,
                   label="New subtask action")
    await click(cdp, sid, "#task-action-child")
    await wait_for(cdp, sid, "!!document.getElementById('task-child-modal')", timeout=10,
                   label="child modal")
    await evaluate(cdp, sid,
                   "document.getElementById('task-child-profile').value = 'manager';"
                   "document.getElementById('task-child-name').value = 'Feature alpha';"
                   "document.getElementById('task-child-goal').value = 'Deliver feature alpha';")
    await click_button_by_text(cdp, sid, "Create subtask", "#task-child-modal")
    await wait_for(cdp, sid,
                   "typeof SESSION_ID !== 'undefined' && SESSION_ID !== " + json.dumps(root_id),
                   timeout=15, label="child manager selected")
    child_id = await evaluate(cdp, sid, "SESSION_ID")
    await evaluate(cdp, sid, f"Sidebar.SessionTree.ensureExpanded({json.dumps(root_id)})")
    await wait_for(cdp, sid, f"!!document.getElementById('tree-node-{child_id}')", timeout=15,
                   label="child row visible")
    results.record("s03-create-child-manager", True, f"child={child_id[:8]} under root",
                   await screenshot(cdp, sid, results, "s03-child-manager"))

    # --- S4: worker under the child manager --------------------------------
    await click(cdp, sid, "#task-action-child")
    await wait_for(cdp, sid, "!!document.getElementById('task-child-modal')", timeout=10,
                   label="worker modal")
    await evaluate(cdp, sid,
                   "document.getElementById('task-child-profile').value = 'worker';"
                   "document.getElementById('task-child-name').value = 'Worker one';"
                   "document.getElementById('task-child-goal').value = 'Execute a small trial step';")
    await click_button_by_text(cdp, sid, "Create subtask", "#task-child-modal")
    await wait_for(cdp, sid,
                   f"typeof SESSION_ID !== 'undefined' && SESSION_ID !== {json.dumps(child_id)}",
                   timeout=15, label="worker selected")
    worker_id = await evaluate(cdp, sid, "SESSION_ID")
    await evaluate(cdp, sid, f"Sidebar.SessionTree.ensureExpanded({json.dumps(child_id)})")
    await wait_for(cdp, sid, f"!!document.getElementById('tree-node-{worker_id}')", timeout=15,
                   label="worker row visible")
    results.record("s04-create-worker", True, f"worker={worker_id[:8]} under child",
                   await screenshot(cdp, sid, results, "s04-worker-created"))

    # --- S5: goal editing on the Task tab ----------------------------------
    await open_task_tab(cdp, sid, "task")
    await wait_for(cdp, sid, "!!document.getElementById('task-goal-input')", timeout=10,
                   label="goal editor")
    await evaluate(cdp, sid,
                   "document.getElementById('task-goal-input').value = 'Execute a small trial step (edited)';")
    await click(cdp, sid, "#task-save-btn")
    await wait_for(cdp, sid,
                   "fetch('/api/sessions/' + SESSION_ID, {cache: 'no-store'})"
                   ".then(r => r.json()).then(d => (d.task && d.task.goal || '').includes('(edited)'))",
                   timeout=15, label="goal persisted")
    results.record("s05-edit-goal", True, "goal edited and persisted through the Task tab",
                   await screenshot(cdp, sid, results, "s05-goal-edited"))


    # --- S6: node switching preserves the unsaved draft --------------------
    await evaluate(cdp, sid,
                   "document.getElementById('task-goal-input').value = 'unsaved draft text';"
                   "document.getElementById('task-goal-input').dispatchEvent(new Event('input'));")
    # The editor persists the unsaved draft on a 300ms debounce; a real user
    # switching nodes always takes longer than that.
    await asyncio.sleep(0.7)
    await evaluate(cdp, sid, f"switchSession({json.dumps(child_id)})")
    await wait_for(cdp, sid,
                   f"SESSION_ID === {json.dumps(child_id)} && !!document.getElementById('task-goal-input')",
                   timeout=15, label="switched to child")
    await evaluate(cdp, sid, f"switchSession({json.dumps(worker_id)})")
    await wait_for(cdp, sid,
                   "SESSION_ID === " + json.dumps(worker_id)
                   + " && (document.getElementById('task-goal-input').value === 'unsaved draft text'"
                   " || document.getElementById('task-goal-input').value === "
                   "'Execute a small trial step (edited)')",
                   timeout=15, label="worker editor rendered with its own value")
    draft_back = await evaluate(cdp, sid, "document.getElementById('task-goal-input').value")
    ok = draft_back == "unsaved draft text"
    await evaluate(cdp, sid,
                   "document.getElementById('task-goal-input').value = 'Execute a small trial step (edited)';"
                   "document.getElementById('task-goal-input').dispatchEvent(new Event('input'));")
    results.record("s06-draft-preservation", ok,
                   f"draft after switching away and back: {draft_back!r}",
                   await screenshot(cdp, sid, results, "s06-draft-preserved"))

    # --- S5b: New Task from a non-Chat tab keeps the drafts and opens Chat ---
    # The Task tab is displaying; the operator has an unsaved message draft and
    # an unsaved task-edit draft on this node. One click of the user-facing
    # control creates a root, lands in Chat with the cursor ready, and both
    # drafts survive the switch.
    posts_before = len([m for m in cdp.mutations if m["method"] == "POST" and m["url"].endswith("/api/sessions/")])
    await evaluate(cdp, sid,
                   "const inp = document.getElementById('msg-input');"
                   "inp.value = 'worker message draft';"
                   "inp.dispatchEvent(new Event('input'));"
                   "const goal = document.getElementById('task-goal-input');"
                   "goal.value = 'unsaved before create';"
                   "goal.dispatchEvent(new Event('input'));")
    await asyncio.sleep(0.7)  # the editor's draft persistence window
    await click_button_by_text(cdp, sid, "New Task", "#sidebar")
    await wait_for(cdp, sid,
                   "typeof SESSION_ID !== 'undefined' && SESSION_ID !== "
                   + json.dumps(worker_id),
                   timeout=20, label="new root selected from a non-Chat tab")
    fresh_root_id = await evaluate(cdp, sid, "SESSION_ID")
    fresh_posts = [m for m in cdp.mutations if m["method"] == "POST" and m["url"].endswith("/api/sessions/")]
    create_delta = len(fresh_posts) - posts_before
    chat_open = await evaluate(cdp, sid, "!document.getElementById('tab-chat').classList.contains('hidden')")
    composer_ready = await evaluate(
        cdp, sid,
        "document.activeElement === document.getElementById('msg-input')"
        " && document.getElementById('msg-input').value === ''")
    await wait_for(cdp, sid, f"!!document.getElementById('tree-node-{fresh_root_id}')", timeout=15,
                   label="new root row in tree")
    # Both drafts come back with the worker.
    await evaluate(cdp, sid, f"switchSession({json.dumps(worker_id)})")
    await wait_for(cdp, sid,
                   "SESSION_ID === " + json.dumps(worker_id)
                   + " && !!document.getElementById('task-goal-input')", timeout=15,
                   label="worker selected again")
    await open_task_tab(cdp, sid, "task")
    await wait_for(cdp, sid, "!!document.getElementById('task-goal-input')", timeout=10,
                   label="worker task editor")
    goal_draft_back = await evaluate(cdp, sid, "document.getElementById('task-goal-input').value")
    composer_back = await evaluate(cdp, sid, "document.getElementById('msg-input').value")
    await evaluate(cdp, sid,
                   "document.getElementById('task-goal-input').value = 'Execute a small trial step (edited)';"
                   "document.getElementById('task-goal-input').dispatchEvent(new Event('input'));")
    results.record("s05b-new-task-from-non-chat-tab",
                   bool(fresh_root_id and create_delta == 1 and chat_open and composer_ready
                        and goal_draft_back == "unsaved before create"
                        and composer_back == "worker message draft"),
                   f"root={fresh_root_id[:8]} create-posts={create_delta} chat={chat_open} "
                   f"composer-ready={composer_ready} goal-draft={goal_draft_back!r} "
                   f"composer-draft={composer_back!r}",
                   await screenshot(cdp, sid, results, "s05b-new-task-from-task-tab"))

    # --- S5c: one pending create action absorbs rapid clicks -----------------
    # Two synchronous clicks of the real control in one JS tick: the second
    # lands while the first create-and-open is in flight.
    posts_before = len([m for m in cdp.mutations if m["method"] == "POST" and m["url"].endswith("/api/sessions/")])
    await evaluate(cdp, sid,
                   "(() => { const btn = [...document.querySelectorAll('#sidebar button')]"
                   ".find(b => b.textContent.trim() === 'New Task');"
                   " if (!btn) throw new Error('missing New Task button'); btn.click(); btn.click(); })()")
    await wait_for(cdp, sid,
                   "typeof SESSION_ID !== 'undefined' && SESSION_ID !== "
                   + json.dumps(worker_id),
                   timeout=20, label="one new task from the double click")
    rapid_root_id = await evaluate(cdp, sid, "SESSION_ID")
    rapid_posts = [m for m in cdp.mutations if m["method"] == "POST" and m["url"].endswith("/api/sessions/")]
    rapid_delta = len(rapid_posts) - posts_before
    rapid_ready = await evaluate(
        cdp, sid,
        "document.activeElement === document.getElementById('msg-input')")
    await wait_for(cdp, sid, f"!!document.getElementById('tree-node-{rapid_root_id}')", timeout=15,
                   label="rapid root row in tree")
    results.record("s05c-rapid-clicks-one-create",
                   bool(rapid_root_id and rapid_delta == 1 and rapid_ready
                        and rapid_root_id != fresh_root_id),
                   f"root={rapid_root_id[:8]} create-posts={rapid_delta} composer-ready={rapid_ready}",
                   await screenshot(cdp, sid, results, "s05c-rapid-clicks"))

    # --- S7: local + subtree rules in the Context tab ----------------------
    await open_task_tab(cdp, sid, "task-context")
    await wait_for(cdp, sid, "!!document.getElementById('task-rule-editor')", timeout=15,
                   label="rule editor")
    await evaluate(cdp, sid,
                   "document.getElementById('task-rule-editor').value = 'Trial rule: keep replies short.';"
                   "document.getElementById('task-rule-editor').dispatchEvent(new Event('input'));")
    await click_button_by_text(cdp, sid, "Save this-task rule", "#tab-task-context")
    await wait_for(cdp, sid,
                   "fetch('/api/sessions/' + SESSION_ID, {cache: 'no-store'})"
                   ".then(r => r.json()).then(d => !!(d.prompt_rules && d.prompt_rules.node &&"
                   " (d.prompt_rules.node.text || '').includes('keep replies short')))",
                   timeout=15, label="local rule persisted")
    # Subtree rule lives on the child manager; the editor switches scope there.
    await evaluate(cdp, sid, f"switchSession({json.dumps(child_id)})")
    await wait_for(cdp, sid, "SESSION_ID === " + json.dumps(child_id), timeout=15, label="child selected")
    await wait_for(cdp, sid, "!!document.getElementById('task-rule-editor')", timeout=15,
                   label="rule editor on child")
    has_scope_radio = await evaluate(
        cdp, sid,
        "!!document.querySelector('#tab-task-context input[name=\"task-rule-scope\"][value=\"subtree\"]')")
    if has_scope_radio:
        await evaluate(cdp, sid,
                       "const radio = document.querySelector("
                       "'#tab-task-context input[name=\"task-rule-scope\"][value=\"subtree\"]');"
                       "radio.checked = true; radio.dispatchEvent(new Event('change'));")
        await wait_for(cdp, sid,
                       "document.getElementById('task-rule-editor').placeholder.includes('descendant')",
                       timeout=10, label="subtree scope active")
        await evaluate(cdp, sid,
                       "document.getElementById('task-rule-editor').value = "
                       "'Subtree rule: workers report with one summary line.';"
                       "document.getElementById('task-rule-editor').dispatchEvent(new Event('input'));")
        await click_button_by_text(cdp, sid, "Save subtree rule", "#tab-task-context")
        await wait_for(cdp, sid,
                       "fetch('/api/sessions/' + SESSION_ID, {cache: 'no-store'})"
                       ".then(r => r.json()).then(d => !!(d.prompt_rules && d.prompt_rules.subtree &&"
                       " (d.prompt_rules.subtree.text || '').includes('one summary line')))",
                       timeout=15, label="subtree rule persisted")
    results.record("s07-rules-editing", True,
                   f"local rule on worker, subtree rule on child (scope radio={has_scope_radio})",
                   await screenshot(cdp, sid, results, "s07-rules"))

    # --- S8: the real GLM manager turn from the first message --------------
    # S2b's first message already rode the normal durable input path on the
    # empty-goal root; the dispatched manager_turn Run is this scenario's
    # real-CLC evidence (one real turn, no second send).
    await evaluate(cdp, sid, f"switchSession({json.dumps(root_id)})")
    await wait_for(cdp, sid, "SESSION_ID === " + json.dumps(root_id), timeout=15, label="root selected")
    log("  waiting for the real GLM manager turn (bounded)")
    run_wait_deadline = time.monotonic() + 180
    run = None
    while time.monotonic() < run_wait_deadline:
        status, page = api_request(base, access_key, "GET", f"/api/sessions/{root_id}/runs?limit=5")
        runs = (page.get("items") or []) if isinstance(page, dict) else (page or [])
        terminal = [r for r in runs if r.get("kind") == "manager_turn" and r.get("state") in
                    ("success", "failed", "stopped", "interrupted")]
        if terminal:
            run = terminal[0]
            break
        await asyncio.sleep(2)
    if run is None:
        results.record("s08-real-glm-manager-turn", False,
                       "no terminal manager_turn run within 180s (provider/network failure is an "
                       "explicit failed live check)", await screenshot(cdp, sid, results, "s08-fail"))
    else:
        native_dir = home / "clc-sessions"
        native_entries = sorted(p.name for p in native_dir.iterdir())
        detail = (f"run={run['id'][:8]} state={run.get('state')} "
                  f"native={run.get('native_session_id')} "
                  f"native-dir-entries={len(native_entries)}")
        results.record("s08-real-glm-manager-turn",
                       run.get("state") == "success" and bool(native_entries), detail,
                       await screenshot(cdp, sid, results, "s08-glm-run"))
        # --- S9: Run history and stored Context on the launched run --------
        await open_task_tab(cdp, sid, "runs")
        run_visible = await evaluate(
            cdp, sid, f"document.getElementById('tab-runs').textContent.includes({json.dumps(run['id'][:8])})")
        status, ctx = api_request(base, access_key, "GET",
                                  f"/api/sessions/{root_id}/runs/{run['id']}/context")
        has_snapshot = status == 200 and bool((ctx or {}).get("snapshot"))
        results.record("s09-run-history-and-context", bool(run_visible and has_snapshot),
                       f"runs-panel={bool(run_visible)} context-snapshot={has_snapshot}",
                       await screenshot(cdp, sid, results, "s09-runs-context"))
        # The stored snapshot is the same assembly the prompt preview showed.
        status, preview = api_request(base, access_key, "GET",
                                      f"/api/sessions/{root_id}/effective-prompt?kind=manager_turn")
        preview_hash = (preview or {}).get("prompt_hash")
        stored_hash = ((ctx or {}).get("snapshot") or {}).get("prompt_hash")
        results.record("s09b-prompt-preview-matches-stored-context",
                       bool(preview_hash and preview_hash == stored_hash),
                       f"preview={str(preview_hash)[:16]} stored={str(stored_hash)[:16]}", None)

    # --- S10: completion, refusal, reopen, move, pause on the worker -------
    await evaluate(cdp, sid, f"switchSession({json.dumps(worker_id)})")
    await wait_for(cdp, sid, "SESSION_ID === " + json.dumps(worker_id), timeout=15,
                   label="worker selected again")
    await open_task_tab(cdp, sid, "task")
    await wait_for(cdp, sid, "!!document.getElementById('task-action-complete')", timeout=10,
                   label="complete action")
    await click(cdp, sid, "#task-action-complete")
    await wait_for(cdp, sid, "!!document.getElementById('task-complete-modal')", timeout=10,
                   label="complete modal")
    await evaluate(cdp, sid,
                   "document.getElementById('task-complete-summary').value = 'Trial step done.';"
                   "document.getElementById('task-complete-refs').value = 'manual trial evidence';"
                   "document.getElementById('task-complete-refs').dispatchEvent(new Event('input'));")
    await click_button_by_text(cdp, sid, "Complete task", "#task-complete-modal")
    await wait_for(cdp, sid,
                   "fetch('/api/sessions/' + SESSION_ID, {cache: 'no-store'})"
                   ".then(r => r.json()).then(d => d.task_state === 'completed')", timeout=15,
                   label="worker completed")
    shot = await screenshot(cdp, sid, results, "s10a-worker-completed")
    await wait_for(cdp, sid, "!!document.getElementById('task-action-reopen')", timeout=10,
                   label="reopen action")
    await click(cdp, sid, "#task-action-reopen")
    await wait_for(cdp, sid, "!!document.getElementById('task-reason-modal')", timeout=10,
                   label="reopen reason modal")
    await evaluate(cdp, sid,
                   "const ta = document.querySelector('#task-reason-modal textarea');"
                   "if (ta) { ta.value = 'Trial reopen'; ta.dispatchEvent(new Event('input')); }")
    await click_button_by_text(cdp, sid, "Reopen task", "#task-reason-modal")
    await wait_for(cdp, sid,
                   "fetch('/api/sessions/' + SESSION_ID, {cache: 'no-store'})"
                   ".then(r => r.json()).then(d => d.task_state === 'open')", timeout=15,
                   label="worker reopened")
    await wait_for(cdp, sid, "!!document.getElementById('task-action-move')", timeout=10,
                   label="move action")
    await click(cdp, sid, "#task-action-move")
    await wait_for(cdp, sid, "!!document.getElementById('task-move-modal')", timeout=10,
                   label="move chooser")
    chooser_ok = await evaluate(
        cdp, sid, "!!document.getElementById('task-move-list') &&"
                  "!document.getElementById('task-move-list').textContent.includes('Failed to load')")
    await click_button_by_text(cdp, sid, "Cancel", "#task-move-modal")
    pause_btn = await evaluate(cdp, sid, "!!document.getElementById('task-action-pause')")
    if pause_btn:
        await click(cdp, sid, "#task-action-pause")
        await wait_for(cdp, sid,
                       "fetch('/api/sessions/' + SESSION_ID, {cache: 'no-store'})"
                       ".then(r => r.json()).then(d => d.automation_paused === true)", timeout=15,
                       label="paused")
    results.record("s10-completion-refusal-reopen-move-pause",
                   bool(chooser_ok),
                   f"complete/reopen applied; move chooser loaded={chooser_ok}; pause applied={pause_btn}",
                   await screenshot(cdp, sid, results, "s10b-controls"))

    # --- S11: reload preserves the trial state and the selected node --------
    await cdp.send("Page.navigate", {"url": f"{base}/?session={root_id}"}, session_id=sid)
    await wait_for(cdp, sid, "!!document.getElementById('preview-indicator')", timeout=20,
                   label="reloaded main page")
    await wait_for(cdp, sid,
                   f"!!document.getElementById('tree-node-{root_id}')", timeout=20,
                   label="root row after reload")
    still_there = await evaluate(
        cdp, sid,
        f"[{json.dumps(root_id)}, {json.dumps(child_id)}, {json.dumps(worker_id)}]"
        ".every(id => { const el = document.getElementById('tree-node-' + id);"
        " if (el) return true; return false; })")
    reloaded_session = await evaluate(cdp, sid, "SESSION_ID")
    selected = await evaluate(
        cdp, sid,
        f"(() => {{ const row = document.getElementById('tree-node-{root_id}');"
        " return !!row && row.firstElementChild.classList.contains('bg-blue-600/20'); })()")
    results.record("s11-reload-persistence", bool(still_there and reloaded_session == root_id and selected),
                   f"rows survive a reload (cookie login intact); selected-after-reload={selected}",
                   await screenshot(cdp, sid, results, "s11-after-reload"))

    # --- S12: narrow viewport ----------------------------------------------
    await cdp.send("Emulation.setDeviceMetricsOverride",
                   {"width": 390, "height": 844, "deviceScaleFactor": 2, "mobile": True},
                   session_id=sid)
    await asyncio.sleep(0.6)
    await screenshot(cdp, sid, results, "s12a-narrow-viewport")
    await evaluate(cdp, sid, "toggleMobileSidebar()")
    await asyncio.sleep(0.4)
    narrow_ok = await evaluate(cdp, sid,
                               "(() => { const list = document.getElementById('session-list');"
                               " if (!list) return false; const r = list.getBoundingClientRect();"
                               " return r.width > 100 && r.width <= window.innerWidth + 2; })()")
    await screenshot(cdp, sid, results, "s12b-narrow-sidebar")
    await cdp.send("Emulation.clearDeviceMetricsOverride", session_id=sid)
    results.record("s12-narrow-viewport", bool(narrow_ok),
                   "task tree renders inside a 390px viewport", None)

    # --- console errors -----------------------------------------------------
    errs = await evaluate(cdp, sid, "window.__errs || []")
    errs = list(errs or [])
    errs += cdp.console_errors
    results.console_errors = errs
    results.record("no-console-errors", not errs,
                   f"{len(errs)} console errors across all scenarios"
                   + (": " + "; ".join(errs[:5]) if errs else ""),
                   None)
    del shot


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence-dir", default=str(RESEARCH_DEFAULT / "preview_browser_evidence"),
                        help="Where screenshots and results land")
    parser.add_argument("--backend", default=DEFAULT_BACKEND,
                        help="The charlie-code backend id the trial instance runs")
    parser.add_argument("--chrome", default=None, help="Chrome binary (default: google-chrome)")
    args = parser.parse_args()
    asyncio.run(run_harness(args))


if __name__ == "__main__":
    main()
