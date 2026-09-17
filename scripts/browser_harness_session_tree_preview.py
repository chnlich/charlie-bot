"""The fresh-home preview browser harness: the real entry point, the real UI.

Starts ``charliebot session-tree preview`` as its own foreground process on a
disposable home (the actual CLI, never a lifespan-off in-process server), then
drives a real headless Chrome with an isolated browser profile through the
first-login/empty state, the creation toolbar (the wide blue New Session button
with the compact model dropdown beside it in one row), the one-click New Session
flow (real control click from the welcome screen and from a non-Chat tab: no
creation form, Chat with a focused composer, drafts kept, one node per pending
action, the dropdown's selected model carried on the create), explicit
child/worker creation through the existing modal, goal and rule editing, node
switching with draft preservation, the real GLM manager turn from the first
message, Context and run history, the completion/refusal/reopen and move/pause
controls, a reload with selection, a narrow viewport, the live activity
feedback on real Run paths (a manager turn spinning its own row, a worker turn
spinning its row plus its ancestors' delegated-work gear while collapsed, a
stop clearing both — observed through the tree's WebSocket updates, never a
painted fake row), and — for every backend listed in --backends — one task
created through a real dropdown selection of that model with its own bounded
first message and Run evidence (backend, model and native location matching
the selection). Every refusal is a recorded failure; no scenario is scripted
to pass.

The cues' motion itself is proven temporally on the live rows: each window
samples the animation timeline at unequal intervals and requires distinct
transforms, currentTime advancing at the page's own wall-clock pace and a
constant startTime (element generations are counted, so a legal fact-driven
repaint is distinguishable from constant restarts; any restart or stall
inside a window breaks the advance-equals-wall equality) in the unemulated
page, under emulated prefers-reduced-motion (the user-restored behavior: the
spinner and the delegated gear keep rotating under reduce too; only the
pulse cues stop), on desktop and at the narrow viewport, and across an
ordinary mid-run reload. The windows ride real Runs: the unemulated and
reduced windows on the worker Run, the reload and narrow windows plus the
stop on a fresh bounded child-manager turn (managers never auto-complete, so
the row and the interrupted-state label stay observable). A short clipped
frame sequence of the live row is kept beside the samples for visual glyph
inspection.

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
import traceback  # noqa: E402
import urllib.error  # noqa: E402
import urllib.request  # noqa: E402

from scripts.browser_harness_session_tree import (  # noqa: E402
    CDP,
    evaluate,
    log,
    pick_free_port,
    wait_for,
)

RESEARCH_DEFAULT = Path("/home/chaoli/.charliebot/sessions/8a7964a3-8e53-4fa3-9145-893bac307ddc/research")

# The dropdown's id->label map, read from the live page after login.
BACKEND_LABELS: dict[str, str] = {}

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


TREE_ROW_ACTIVITY_SNIPPET = """
    (() => {
      const id = "__NODE_ID__";
      const row = document.getElementById('tree-node-' + id);
      if (!row) return null;
      // A cue with a nonzero rect can still be scrolled out of the sidebar's
      // clip: bring the row into view first, then require the cue's rect to
      // intersect both the viewport and the scroll container, so "visible"
      // means actually on screen.
      row.scrollIntoView({block: 'nearest'});
      const spin = row.querySelector('svg[id="spinner-' + id + '"]');
      const gear = row.querySelector('svg[id="worker-indicator-' + id + '"]');
      const unread = row.querySelector('span[id="unread-' + id + '"]');
      const scroller = row.closest('.overflow-y-auto, .overflow-auto, #session-list');
      const visible = (el) => {
        if (!el || el.classList.contains('hidden')) return false;
        const r = el.getBoundingClientRect();
        if (r.width <= 2 || r.height <= 2) return false;
        const left = Math.max(r.left, scroller ? scroller.getBoundingClientRect().left : 0, 0);
        const top = Math.max(r.top, scroller ? scroller.getBoundingClientRect().top : 0, 0);
        const right = Math.min(r.right, scroller ? scroller.getBoundingClientRect().right : innerWidth, innerWidth);
        const bottom = Math.min(r.bottom, scroller ? scroller.getBoundingClientRect().bottom : innerHeight, innerHeight);
        return right - left > 2 && bottom - top > 2;
      };
      return {spinner: visible(spin), gear: visible(gear), unread: visible(unread),
              label: row.textContent || ''};
    })()
"""


async def tree_row_activity(cdp: CDP, session_id: str, node_id: str) -> dict:
    """One tree row's live activity facts: which shared cue is visible and its state label."""
    expr = TREE_ROW_ACTIVITY_SNIPPET.replace("__NODE_ID__", node_id)
    value = await evaluate(cdp, session_id, expr)
    if value is None:
        raise RuntimeError(f"tree row for {node_id} is not rendered")
    return value


async def tree_row_activity_tolerant(cdp: CDP, session_id: str, node_id: str) -> dict | None:
    """Like tree_row_activity, but a node whose task left the open tree (its
    automation completed it mid-scenario) reads as None instead of aborting."""
    try:
        return await tree_row_activity(cdp, session_id, node_id)
    except RuntimeError:
        return None


async def wait_row_activity(cdp: CDP, session_id: str, node_id: str, want: dict,
                            timeout: float, label: str, samples: list[str] | None = None) -> dict:
    """Bounded wait for one row's cue state; every distinct state label seen on the
    way is kept so the observed transition sequence is reported, not assumed."""
    deadline = time.monotonic() + timeout
    last = {}
    while time.monotonic() < deadline:
        last = await tree_row_activity(cdp, session_id, node_id)
        if samples is not None:
            state = last.get("label", "")
            if not samples or samples[-1] != state:
                samples.append(state)
        if all(last.get(k) == v for k, v in want.items()):
            return last
        await asyncio.sleep(0.4)
    raise TimeoutError(f"{label}: last row state {last}")


async def open_task_tab(cdp: CDP, session_id: str, tab: str) -> None:
    await evaluate(cdp, session_id, f"switchTab({json.dumps(tab)})")
    await wait_for(cdp, session_id,
                   f"!document.getElementById('tab-{tab}').classList.contains('hidden')",
                   timeout=10, label=f"tab {tab} visible")


# Motion observation: one temporal window proves a cue actually moves. The
# intervals are unequal on purpose (a uniform cadence could alias with the
# animation period), and every window is verdict-checked: any restart or
# stall inside it breaks either the constant startTime, the strictly
# advancing currentTime, or the advance-equals-wall-clock equality.
MOTION_INTERVALS_S = [0.05, 0.45, 0.20, 0.75, 0.85]
RELOAD_INTERVALS_S = [0.05, 0.50, 0.75]

MOTION_TIMELINE_SNIPPET = """
    (() => {
      globalThis.__motionProbeSeq = globalThis.__motionProbeSeq || 0;
      const sample = (kind, id) => {
        const el = document.getElementById(id);
        const pageNow = performance.now();
        if (!el) return {kind, id, pageNow, missing: true};
        const hidden = el.classList.contains('hidden');
        if (!hidden && !el.dataset.motionProbe) {
          el.dataset.motionProbe = String(++globalThis.__motionProbeSeq);
        }
        const anims = hidden ? [] : el.getAnimations();
        const a = anims[0] || null;
        const s = getComputedStyle(el);
        return {kind, id, pageNow, hidden, stamp: el.dataset.motionProbe || null,
                transform: s.transform, animationName: s.animationName,
                time: a ? a.currentTime : null, start: a ? a.startTime : null,
                state: a ? a.playState : null};
      };
      return [sample('spinner', __SPIN_ID__), sample('gear', __GEAR_ID__)];
    })()
"""


async def sample_motion_timeline(cdp: CDP, session_id: str, spinner_id: str, gear_id: str,
                                 intervals: list[float]) -> list[list[dict]]:
    """Sample two cues' animation timelines at the given unequal spacing.

    pageNow is read inside the page so CDP latency cancels in the
    advance-versus-wall comparison; the dataset stamp marks element identity so
    a tree repaint (which rebuilds the row) is observable as a generation
    change.
    """
    expr = (MOTION_TIMELINE_SNIPPET
            .replace('__SPIN_ID__', json.dumps(spinner_id))
            .replace('__GEAR_ID__', json.dumps(gear_id)))
    samples: list[list[dict]] = []
    for delay in intervals:
        await asyncio.sleep(delay)
        samples.append(await evaluate(cdp, session_id, expr))
    return samples


def motion_timeline_verdict(samples: list[list[dict]], kind: str,
                            min_span_s: float, max_generations: int = 2) -> tuple[bool, str]:
    """Temporal motion proof for one cue across a sampled window.

    Passing requires: the cue rendered and live from the window's start; one
    element generation (at most one rebuild, the legal fact-driven repaint)
    spanning at least min_span_s; within it a constant startTime (an animation
    restart moves it), strictly advancing currentTime that matches the page's
    own wall clock (a restart or stall falls behind), running playState, and
    at least two distinct transforms.
    """
    rows = [entry for sample in samples for entry in sample if entry.get('kind') == kind]
    if len(rows) < 3:
        return False, f'{kind}: fewer than 3 samples: {rows}'
    if rows[0].get('missing') or rows[0].get('hidden'):
        return False, f'{kind}: cue not live at window start: {rows[0]}'
    generations: list[list[dict]] = []
    current: list[dict] = []
    for r in rows:
        if r.get('missing') or r.get('hidden'):
            if current:
                generations.append(current)
                current = []
            continue
        if current and current[-1].get('stamp') != r.get('stamp'):
            generations.append(current)
            current = []
        current.append(r)
    if current:
        generations.append(current)
    if len(generations) > max_generations:
        return False, (f'{kind}: {len(generations)} element generations in one window '
                       f'(constant restarts): {[g[0].get("stamp") for g in generations]}')
    best = max(generations, key=lambda g: g[-1]['pageNow'] - g[0]['pageNow'])
    wall = best[-1]['pageNow'] - best[0]['pageNow']
    if wall < min_span_s * 1000:
        return False, f'{kind}: continuous window {wall:.0f}ms < {min_span_s}s: {best}'
    starts = {g.get('start') for g in best}
    if len(starts) != 1 or None in starts:
        return False, f'{kind}: startTime moved inside one generation (animation restart): {best}'
    times = [g.get('time') for g in best]
    if (any(not isinstance(t, (int, float)) for t in times)
            or any(times[i + 1] <= times[i] for i in range(len(times) - 1))):
        return False, f'{kind}: currentTime not strictly advancing: {times}'
    advance = times[-1] - times[0]
    if abs(advance - wall) > 250:
        return False, (f'{kind}: animation advanced {advance:.0f}ms while the page clock moved '
                       f'{wall:.0f}ms (restart or stall)')
    if any(g.get('state') != 'running' for g in best):
        return False, f'{kind}: playState not running throughout: {[g.get("state") for g in best]}'
    if any('spin' not in str(g.get('animationName')) for g in best):
        return False, f'{kind}: animationName lost mid-window: {[g.get("animationName") for g in best]}'
    transforms = {g.get('transform') for g in best}
    if len(transforms) < 2:
        return False, f'{kind}: transform never changed: {transforms}'
    return True, (f'{kind}: {len(best)} samples over {wall:.0f}ms, advance {advance:.0f}ms, '
                  f'{len(transforms)} transforms, startTime constant, '
                  f'{len(generations)} generation(s)')


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


def build_source_home(source: Path, backend_ids: list[str]) -> None:
    """The trial's private configuration source: the selected backend entries of the current profile.

    The source read is scoped to the entries the trial consumes: the profile's raw
    YAML is parsed here and only the selected entries are validated against this
    branch's schema. A sibling entry carrying a field from a newer schema (the
    live profile is user-owned state that moves independently of this branch)
    must not block the trial; an entry actually copied still refuses loudly on
    anything this branch cannot interpret, and the seeded trial home validates
    strictly. The first entry is the trial's default; the rest are the
    explicitly selectable additions.
    """
    import yaml

    from src.core.config import CharlieBotConfig, charliebot_home_dir, load_credentials

    config_path = charliebot_home_dir() / "config.yaml"
    raw_options = (yaml.safe_load(config_path.read_text(encoding="utf-8")) or {}).get("backends", {}).get("options") or []
    entries = []
    written_sections: set[str] = set()
    credential_lines: list[str] = []
    for backend_id in backend_ids:
        raw_entry = next((e for e in raw_options if isinstance(e, dict) and e.get("id") == backend_id), None)
        if raw_entry is None:
            raise SystemExit(f"backend {backend_id!r} is not configured in the current profile {config_path}")
        try:
            # Schema-strict validation of exactly the entry the trial copies.
            option = CharlieBotConfig(backends={"options": [raw_entry]}).get_backend_option(backend_id)
        except Exception as e:
            raise SystemExit(
                f"backend {backend_id!r} in {config_path} is not interpretable by this branch's "
                f"config schema: {e}") from e
        assert option is not None
        if option.type.value != "charlie-code":
            raise SystemExit(
                f"backend {backend_id!r} has type {option.type.value!r}; the preview isolates native "
                "state only for charlie-code")
        entries.append(json.loads(option.model_dump_json()))
        if getattr(option, "credential", None):
            section = str(option.credential)
            api_key = load_credentials().get(section, "api_key")
            if api_key is None:
                raise SystemExit(f"backend {backend_id!r} needs credentials.{section}.api_key; it is unset")
            if section not in written_sections:
                written_sections.add(section)
                credential_lines.append(f"{section}:\n  api_key: {api_key}\n")
    source.mkdir(parents=True, exist_ok=True)
    config = {
        "server": {"host": "127.0.0.1", "port": 18498},
        "paths": {"workspace_dirs": [str(source / "workspaces")],
                  "worktree_dir": str(source / "worktrees")},
        "backends": {"options": entries, "preference": [backend_ids[0]]},
    }
    (source / "config.yaml").write_text(yaml.safe_dump(config), encoding="utf-8")
    creds = "charliebot:\n  access_key: source-operator-key-not-used\n" + "".join(credential_lines)
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
        backends = [b.strip() for b in args.backends.split(",") if b.strip()]
        if not backends:
            raise SystemExit("--backends names at least one backend id")
        build_source_home(source, backends)
        # The harness process itself must keep production identities out of any
        # child it spawns; the preview CLI clears its own in addition.
        for var in ("CHARLIEBOT_SESSION_ID", "CHARLIEBOT_RUN_TOKEN",
                    "CHARLIE_CODE_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"):
            os.environ.pop(var, None)
        home = tmp_path / "preview-home"
        port = pick_free_port()
        # The fresh multi-entry setup rides the real CLI flags: --backend names the
        # trial's default, every further selected entry arrives as --add-backend.
        invocation = [sys.executable, "-m", "src.cli.main", "session-tree", "preview",
                      "--home", str(home), "--port", str(port), "--backend", backends[0]]
        for extra_id in backends[1:]:
            invocation += ["--add-backend", extra_id]
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
                # masked by the failed-scenario exit below; the finally block's
                # SystemExit would otherwise swallow this traceback.
                log("harness exception traceback: " + traceback.format_exc())
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
    new_session_label = await evaluate(
        cdp, sid,
        "[...document.querySelectorAll('button span')].some(s => s.textContent.trim() === 'New Session')")
    empty_tree = await evaluate(
        cdp, sid,
        "(() => { const t = document.getElementById('session-list');"
        " return t ? /No task trees yet|Use .New [Ss]ession/i.test(t.textContent) : false; })()")
    overlay_gone = await evaluate(cdp, sid,
                                  "document.getElementById('auth-overlay').style.display === 'none'")
    toolbar = await evaluate(cdp, sid, """
        (() => {
          const btn = [...document.querySelectorAll('#sidebar button')].find(b => b.textContent.trim() === 'New Session');
          const sel = document.getElementById('new-session-backend');
          if (!btn || !sel) return {present: false};
          const b = btn.getBoundingClientRect(), s = sel.getBoundingClientRect();
          const span = btn.querySelector('span');
          const cs = getComputedStyle(sel);
          const ctx = document.createElement('canvas').getContext('2d');
          ctx.font = cs.font;
          const labels = [...sel.options].map(o => o.textContent.trim());
          const labelWidths = labels.map(t => Math.round(ctx.measureText(t).width));
          return {present: true, options: [...sel.options].map(o => o.value),
                  selected: sel.value, oneRow: Math.abs(b.top - s.top) < 4 && b.bottom <= s.bottom + 4,
                  buttonWidth: b.width, selectWidth: s.width,
                  buttonSpanClipped: span.scrollWidth > span.clientWidth + 0.5,
                  optionTexts: labels, labelWidths};
        })()""")
    toolbar_ok = (toolbar.get("present") and len(toolbar.get("options", [])) >= 1
                  and toolbar.get("oneRow") and not toolbar.get("buttonSpanClipped", True)
                  and all(toolbar.get("optionTexts", [])) and toolbar.get("selectWidth", 0) >= 140)
    ok = (indicator == "Preview — isolated trial instance" and tasks_first and new_session_label
          and empty_tree and overlay_gone and toolbar_ok)
    results.record("s01-first-login-empty-state", ok,
                   f"indicator={indicator!r} tasks-first={tasks_first} new-session-label={new_session_label} "
                   f"empty-tree={empty_tree} overlay-gone={overlay_gone} toolbar={toolbar}",
                   await screenshot(cdp, sid, results, "s01b-empty-preview") + "," + login_shot)
    # The unauthenticated first load logs one expected 401 from the sidebar's
    # initial fetch (the shipped app's pre-login behavior); the no-console-error
    # contract covers the authenticated flow from here on.
    cdp.console_errors = []
    await evaluate(cdp, sid, "window.__errs = [];")
    BACKEND_LABELS.update(await evaluate(cdp, sid,
        "(() => { const sel = document.getElementById('new-session-backend');"
        " const m = {}; for (const o of sel.options) m[o.value] = o.textContent.trim(); return m; })()"))
    log(f"  model dropdown entries: {list(BACKEND_LABELS)}")

    # --- S2: the user-facing New Session control creates the root in one click ---
    # A real click of the welcome screen's New Session button (never a direct
    # helper call): no creation form may appear, Chat must open with the
    # composer holding the cursor, the dropdown's selected model rides the
    # create, and the first typed message must be admitted through the normal
    # durable input path. The sidebar's control takes the same path (S6b/S6c
    # click it from an existing session).
    await click_button_by_text(cdp, sid, "New Session", "main")
    await wait_for(cdp, sid, "typeof SESSION_ID !== 'undefined' && SESSION_ID", timeout=20,
                   label="new root task selected")
    root_id = await evaluate(cdp, sid, "SESSION_ID")
    await wait_for(cdp, sid,
                   "(() => { const m = document.getElementById('task-child-modal');"
                   " const f = ['task-child-name', 'task-child-goal', 'task-child-profile',"
                   " 'task-child-backend'].map((id) => document.getElementById(id));"
                   " return !m && f.every((x) => !x); })()",
                   timeout=8, label="no creation form fields")
    composer_ready = await wait_for(
        cdp, sid,
        "!document.getElementById('tab-chat').classList.contains('hidden')"
        " && document.activeElement === document.getElementById('msg-input')"
        " && document.getElementById('msg-input').value === ''",
        timeout=10, label="chat open with a cursor-ready empty composer")
    await wait_for(cdp, sid,
                   f"!!document.getElementById('tree-node-{root_id}')", timeout=15,
                   label="root row in tree")
    panel_open = await evaluate(cdp, sid,
                                "!document.getElementById('btn-task').classList.contains('hidden')")
    create_posts = [m for m in cdp.mutations if m["method"] == "POST" and m["url"].endswith("/api/sessions/")]
    create_body = json.loads(create_posts[-1]["body"]) if create_posts else {}
    selected_default = await evaluate(cdp, sid, "document.getElementById('new-session-backend').value")
    create_ok = (create_body.get("profile") == "manager" and create_body.get("task_parent_id") is None
                 and (create_body.get("task") or {}).get("goal") == "" and bool(create_body.get("request_id"))
                 and create_body.get("backend") == selected_default)
    badge_label = await evaluate(cdp, sid, "document.getElementById('backend-badge').textContent")
    badge_ok = badge_label == (BACKEND_LABELS.get(selected_default) or selected_default)
    results.record("s02-one-click-root-create",
                   bool(root_id and composer_ready and panel_open and create_ok and badge_ok),
                   f"root={root_id[:8]} composer-ready={composer_ready} "
                   f"task-panel-open={panel_open} create-body-ok={create_ok} "
                   f"selected={selected_default} badge={badge_label!r} create-posts={len(create_posts)}",
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
    # The admitted input through the normal API: the durable events page must
    # carry the user's message (bounded poll — admission is synchronous, the
    # read projection may lag one invalidation behind the send).
    admitted = False
    events_status = None
    admit_deadline = time.monotonic() + 15
    while time.monotonic() < admit_deadline:
        events_status, page = api_request(base, access_key, "GET",
                                          f"/api/sessions/{root_id}/events?before=999999&limit=40")
        admitted = events_status == 200 and any(
            m.get("role") == "user" and first_message in str(m.get("content", ""))
            for m in ((page or {}).get("messages") or []))
        if admitted:
            break
        await asyncio.sleep(0.5)
    results.record("s02b-first-message-admitted-empty-goal", bool(admitted),
                   f"events-page={events_status} user-message-admitted={admitted}",
                   await screenshot(cdp, sid, results, "s02b-first-message"))

    # --- S2c: the manager turn spins its own tree row live -----------------
    # The dispatched manager_turn Run is real: the row must move queued/idle ->
    # running through the tree's WebSocket updates alone (no reload, no other
    # input). Every distinct state label seen on the way is reported.
    row_states: list[str] = []
    try:
        await wait_row_activity(cdp, sid, root_id, {"spinner": True}, 120,
                                "root row spinner during the manager turn", row_states)
        during_shot = await screenshot(cdp, sid, results, "s02c-running-row-desktop")
        results.record("s02c-manager-turn-row-spinner", True,
                       f"root row spinner visible without reload; observed label sequence: "
                       f"{[t[:28] for t in row_states]}",
                       during_shot)
    except TimeoutError as exc:
        results.record("s02c-manager-turn-row-spinner", False, str(exc)[:300],
                       await screenshot(cdp, sid, results, "s02c-fail"))
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

    # --- S6b: New Task from a non-Chat tab keeps the drafts and opens Chat ---
    # The Task tab is displaying; the operator has an unsaved message draft and
    # an unsaved task-edit draft on this node. One click of the user-facing
    # control creates a root, lands in Chat with the cursor ready, and both
    # drafts survive the switch.
    posts_before = len([m for m in cdp.mutations if m["method"] == "POST" and m["url"].endswith("/api/sessions/")])
    await evaluate(cdp, sid,
                   "(() => { const inp = document.getElementById('msg-input');"
                   "inp.value = 'worker message draft';"
                   "inp.dispatchEvent(new Event('input'));"
                   "const goal = document.getElementById('task-goal-input');"
                   "goal.value = 'unsaved before create';"
                   "goal.dispatchEvent(new Event('input')); })()")
    await asyncio.sleep(0.7)  # the editor's draft persistence window
    await click_button_by_text(cdp, sid, "New Session", "#sidebar")
    await wait_for(cdp, sid,
                   "typeof SESSION_ID !== 'undefined' && SESSION_ID !== "
                   + json.dumps(worker_id),
                   timeout=20, label="new root selected from a non-Chat tab")
    fresh_root_id = await evaluate(cdp, sid, "SESSION_ID")
    fresh_posts = [m for m in cdp.mutations if m["method"] == "POST" and m["url"].endswith("/api/sessions/")]
    create_delta = len(fresh_posts) - posts_before
    composer_ready = await wait_for(
        cdp, sid,
        "!document.getElementById('tab-chat').classList.contains('hidden')"
        " && document.activeElement === document.getElementById('msg-input')"
        " && document.getElementById('msg-input').value === ''",
        timeout=10, label="chat open with a cursor-ready composer from a non-Chat tab")
    await wait_for(cdp, sid, f"!!document.getElementById('tree-node-{fresh_root_id}')", timeout=15,
                   label="new root row in tree")
    # Both drafts come back with the worker. The wait is value-based: a stale
    # editor from the previous node stays in the DOM until this node's own
    # render replaces it, so element existence alone would read the wrong node.
    await evaluate(cdp, sid, f"switchSession({json.dumps(worker_id)})")
    await open_task_tab(cdp, sid, "task")
    await wait_for(cdp, sid,
                   "SESSION_ID === " + json.dumps(worker_id)
                   + " && (document.getElementById('task-goal-input')?.value === 'unsaved before create'"
                   " || document.getElementById('task-goal-input')?.value === "
                   "'Execute a small trial step (edited)')", timeout=15,
                   label="worker task editor with its own value")
    goal_draft_back = await evaluate(cdp, sid, "document.getElementById('task-goal-input').value")
    composer_back = await evaluate(cdp, sid, "document.getElementById('msg-input').value")
    await evaluate(cdp, sid,
                   "document.getElementById('task-goal-input').value = 'Execute a small trial step (edited)';"
                   "document.getElementById('task-goal-input').dispatchEvent(new Event('input'));")
    results.record("s06b-new-task-from-non-chat-tab",
                   bool(fresh_root_id and create_delta == 1 and composer_ready
                        and goal_draft_back == "unsaved before create"
                        and composer_back == "worker message draft"),
                   f"root={fresh_root_id[:8]} create-posts={create_delta} "
                   f"composer-ready={composer_ready} goal-draft={goal_draft_back!r} "
                   f"composer-draft={composer_back!r}",
                   await screenshot(cdp, sid, results, "s06b-new-task-from-task-tab"))

    # --- S6c: one pending create action absorbs rapid clicks -----------------
    # Two synchronous clicks of the real control in one JS tick: the second
    # lands while the first create-and-open is in flight.
    posts_before = len([m for m in cdp.mutations if m["method"] == "POST" and m["url"].endswith("/api/sessions/")])
    await evaluate(cdp, sid,
                   "(() => { const btn = [...document.querySelectorAll('#sidebar button')]"
                   ".find(b => b.textContent.trim() === 'New Session');"
                   " if (!btn) throw new Error('missing New Session button'); btn.click(); btn.click(); })()")
    await wait_for(cdp, sid,
                   "typeof SESSION_ID !== 'undefined' && SESSION_ID !== "
                   + json.dumps(worker_id),
                   timeout=20, label="one new task from the double click")
    rapid_root_id = await evaluate(cdp, sid, "SESSION_ID")
    rapid_posts = [m for m in cdp.mutations if m["method"] == "POST" and m["url"].endswith("/api/sessions/")]
    rapid_delta = len(rapid_posts) - posts_before
    rapid_ready = await wait_for(
        cdp, sid,
        "document.activeElement === document.getElementById('msg-input')",
        timeout=10, label="composer focused after the double click")
    await wait_for(cdp, sid, f"!!document.getElementById('tree-node-{rapid_root_id}')", timeout=15,
                   label="rapid root row in tree")
    results.record("s06c-rapid-clicks-one-create",
                   bool(rapid_root_id and rapid_delta == 1 and rapid_ready
                        and rapid_root_id != fresh_root_id),
                   f"root={rapid_root_id[:8]} create-posts={rapid_delta} composer-ready={rapid_ready}",
                   await screenshot(cdp, sid, results, "s06c-rapid-clicks"))

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
        # Success must clear the row's spinner through the same live path: an
        # open task whose Run finished reads idle, never perpetually running.
        try:
            after = await wait_row_activity(cdp, sid, root_id,
                                            {"spinner": False, "gear": False}, 30,
                                            "root row cleared after the manager turn")
            results.record("s08b-manager-turn-row-cleared",
                           "idle" in after.get("label", ""),
                           f"spinner cleared; row label reads: {after.get('label', '')[:60]!r}",
                           await screenshot(cdp, sid, results, "s08b-row-idle"))
        except TimeoutError as exc:
            results.record("s08b-manager-turn-row-cleared", False, str(exc)[:300],
                           await screenshot(cdp, sid, results, "s08b-fail"))
        # --- S9: Run history and stored Context on the launched run --------
        await open_task_tab(cdp, sid, "runs")
        # The panel renders after its own fetch; an immediate textContent read
        # races that fetch and would report a false empty history.
        run_visible = await wait_for(
            cdp, sid, f"document.getElementById('tab-runs').textContent.includes({json.dumps(run['id'][:8])})",
            timeout=10, label="run row in the runs panel")
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
    # The reload lands on the just-created one-click root (the requirement's
    # "new node appears in the task tree and remains selected after refresh").
    # A deep link to a node the user had expanded collapses that node by design
    # (revealNode: selecting a node does not force-open it), so the previously
    # expanded root is not the reload target; its subtree rows must still
    # survive the reload.
    await cdp.send("Page.navigate", {"url": f"{base}/?session={rapid_root_id}"}, session_id=sid)
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
        f"(() => {{ const row = document.getElementById('tree-node-{rapid_root_id}');"
        " return !!row && row.firstElementChild.classList.contains('bg-blue-600/20'); })()")
    missing = await evaluate(
        cdp, sid,
        f"[{json.dumps(rapid_root_id)}, {json.dumps(root_id)}, {json.dumps(child_id)}, {json.dumps(worker_id)}]"
        ".filter(id => !document.getElementById('tree-node-' + id))")
    results.record("s11-reload-persistence",
                   bool(still_there and reloaded_session == rapid_root_id and selected and not missing),
                   f"reloaded-session={str(reloaded_session)[:8]} expected={rapid_root_id[:8]} "
                   f"missing-rows={missing} created-node-selected-after-reload={selected}",
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
    # The creation toolbar keeps the button and the dropdown in one row when the
    # narrow sidebar is open.
    narrow_toolbar = await evaluate(cdp, sid, """
        (() => {
          const btn = [...document.querySelectorAll('#sidebar button')].find(b => b.textContent.trim() === 'New Session');
          const sel = document.getElementById('new-session-backend');
          if (!btn || !sel) return false;
          const b = btn.getBoundingClientRect(), s = sel.getBoundingClientRect();
          return Math.abs(b.top - s.top) < 4 && b.bottom <= s.bottom + 4;
        })()""")
    await screenshot(cdp, sid, results, "s12b-narrow-sidebar")
    await cdp.send("Emulation.clearDeviceMetricsOverride", session_id=sid)
    results.record("s12-narrow-viewport", bool(narrow_ok and narrow_toolbar),
                   f"task tree renders inside a 390px viewport; toolbar one-row={narrow_toolbar}",
                   None)

    # --- S14: delegated-work activity on real Run paths ---------------------
    # The trial's own controls resume the worker; real work turns run. Observed
    # live through the tree's WebSocket updates: a running node spins its own
    # row, its running ancestors show the delegated gear, and a real stop
    # clears the cues. Nothing paints a fake running row.
    await evaluate(cdp, sid, f"switchSession({json.dumps(worker_id)})")
    await wait_for(cdp, sid, f"SESSION_ID === {json.dumps(worker_id)}", timeout=15,
                   label="worker selected for the activity scenario")
    status, _meta = api_request(base, access_key, "PATCH", f"/api/sessions/{worker_id}",
                                {"automation_paused": False})
    resumed = status == 200
    results.record("s14a-worker-resumed", resumed, f"PATCH automation_paused=false -> {status}", None)

    toolbar_intact = await evaluate(cdp, sid, """
        (() => {
          const btn = [...document.querySelectorAll('#sidebar button')].find(b => b.textContent.trim() === 'New Session');
          const sel = document.getElementById('new-session-backend');
          if (!btn || !sel) return false;
          const b = btn.getBoundingClientRect(), s = sel.getBoundingClientRect();
          return Math.abs(b.top - s.top) < 4 && sel.options.length >= 1;
        })()""")

    # The worker's own real turn: one bounded first message through the
    # composer. The reply asks for real generated text so the Run stays
    # inspectable; the stop below preempts it before it can finish.
    # The generated text keeps the real Run alive long enough to hold the
    # temporal motion windows below (explicitly labeled prolongation through
    # the real Run owner; the stop below still preempts it).
    worker_message = ("Trial activity step. Write a 60-word story about a lighthouse, then a second "
                      "60-word paragraph about the sea, then a third 60-word paragraph about the sky, "
                      "then reply with exactly WORK-OK on the last line. Do not create subtasks.")
    await open_task_tab(cdp, sid, "chat")
    await wait_for(cdp, sid, "!!document.getElementById('msg-input')", timeout=10, label="worker composer")
    await evaluate(cdp, sid,
                   "(() => { const inp = document.getElementById('msg-input');"
                   f"inp.value = {json.dumps(worker_message)};"
                   "inp.dispatchEvent(new Event('input')); })()")
    await click(cdp, sid, "#send-btn")
    await wait_for(cdp, sid,
                   f"document.getElementById('tab-chat').textContent.includes({json.dumps(worker_message)})",
                   timeout=15, label="worker message rendered")

    # The worker's row must move to a live spinner on its own; the manager
    # ancestors (child, root) show the delegated gear while a descendant runs.
    worker_states: list[str] = []
    active_run_id = None
    try:
        await wait_row_activity(cdp, sid, worker_id, {"spinner": True}, 120,
                                "worker row spinner during its work Run", worker_states)
        root_during = await tree_row_activity(cdp, sid, root_id)
        child_during = await tree_row_activity(cdp, sid, child_id)
        # Pin the exact Run this visible spinner belongs to (never a fake row).
        status, runs_page = api_request(base, access_key, "GET",
                                        f"/api/sessions/{worker_id}/runs?order=desc&limit=1")
        runs = (runs_page.get("items") or []) if isinstance(runs_page, dict) else []
        if runs and runs[0].get("state") in ("running", "queued"):
            active_run_id = runs[0]["id"]
        geometry = await evaluate(cdp, sid, f"""
            (() => {{
              const row = document.getElementById('tree-node-{worker_id}');
              if (!row) return null;
              const inner = row.firstElementChild;
              const spin = row.querySelector('svg[id="spinner-{worker_id}"]');
              const name = row.querySelector('.session-name');
              if (!spin || !name) return null;
              const r = inner.getBoundingClientRect(), s = spin.getBoundingClientRect(),
                    n = name.getBoundingClientRect();
              return {{
                spinnerInRow: s.top >= r.top && s.bottom <= r.bottom + 1 && s.width > 8 && s.height > 8,
                spinnerLeftOfName: s.right <= n.left + 2,
                nameWidthFloor: n.width >= 0.5 * r.width,
                spinnerAnimation: getComputedStyle(spin).animationName,
              }};
            }})()""")
        geo_ok = bool(geometry) and (geometry.get("spinnerInRow") and geometry.get("spinnerLeftOfName")
                                     and geometry.get("nameWidthFloor")
                                     and "spin" in str(geometry.get("spinnerAnimation")))
        results.record("s14b-worker-run-activity-cues",
                       bool(active_run_id and root_during.get("gear") and toolbar_intact),
                       f"worker spinner live; root delegated gear={root_during.get('gear')} "
                       f"toolbar-intact={toolbar_intact} child state={child_during} "
                       f"active run={str(active_run_id)[:8]} "
                       f"run-state-at-check={runs[0].get('state') if runs else 'none'} "
                       f"worker labels={[t[:20] for t in worker_states]}",
                       await screenshot(cdp, sid, results, "s14b-during-desktop"))
        results.record("s14c-activity-geometry", geo_ok,
                       f"spinner inside the row, adjacent to a readable name, animated: {geometry}",
                       None)
    except TimeoutError as exc:
        results.record("s14b-worker-run-activity-cues", False, str(exc)[:300],
                       await screenshot(cdp, sid, results, "s14b-fail"))
        results.record("s14c-activity-geometry", False, "skipped: no running row observed", None)

    # --- Temporal motion proof on the live cues (the user-restored animation) ---
    # The worker row's spinner and the root row's delegated gear are genuinely
    # visible on a real Run here (never a painted fake row). Each window is
    # verdict-checked by motion_timeline_verdict; see that helper for what
    # counts as continuous motion versus constant restarts. The unemulated and
    # reduced-motion windows ride the worker Run (both complete well inside
    # even the shortest observed turn); the reload and narrow windows plus the
    # stop then ride a fresh bounded CHILD-MANAGER turn, because the worker
    # Run's lifetime is the model's own and its task auto-completes on success
    # (the documented pre-existing quirk), which removes the row
    # mid-scenario. Managers never auto-complete, so the row and the
    # interrupted-state label stay observable. Same real Run owners, same
    # visual structure: the node's own spinner plus the root's delegated gear.
    spin_el = f"spinner-{worker_id}"
    gear_el = f"worker-indicator-{root_id}"
    run_label = f"run {str(active_run_id)[:8]}" if active_run_id else "run already terminal"

    async def motion_pass(min_span: float, intervals: list[float] | None = None) -> dict[str, tuple[bool, str]]:
        samples = await sample_motion_timeline(cdp, sid, spin_el, gear_el, intervals or MOTION_INTERVALS_S)
        return {kind: motion_timeline_verdict(samples, kind, min_span) for kind in ("spinner", "gear")}

    async def active_run_of(session_id: str) -> str | None:
        status, page = api_request(base, access_key, "GET",
                                   f"/api/sessions/{session_id}/runs?order=desc&limit=1")
        items = (page.get("items") or []) if isinstance(page, dict) else []
        return items[0]["id"] if items and items[0].get("state") in ("running", "queued") else None

    try:
        desktop_verdicts = await motion_pass(2.1)
        frame_rect = await evaluate(cdp, sid, f"""
            (() => {{
              const row = document.getElementById('tree-node-{worker_id}');
              if (!row) return null;
              row.scrollIntoView({{block: 'nearest'}});
              const r = row.getBoundingClientRect();
              return {{x: Math.max(0, r.x), y: Math.max(0, r.y),
                      width: Math.min(r.width, 340), height: r.height}};
            }})()""")
        frame_names: list[str] = []
        if frame_rect:
            for i in range(6):
                shot = await cdp.send("Page.captureScreenshot",
                                      {"format": "png", "clip": dict(frame_rect, scale=3)},
                                      session_id=sid)
                fname = f"s14g-motion-frame-{i}.png"
                (results.evidence_dir / fname).write_bytes(base64.b64decode(shot["data"]))
                frame_names.append(fname)
                await asyncio.sleep(0.13)
        results.record(
            "s14g-motion-timeline-desktop",
            all(ok for ok, _ in desktop_verdicts.values()),
            f"{run_label}: " + "; ".join(detail for _, detail in desktop_verdicts.values())
            + f"; frames={frame_names}",
            await screenshot(cdp, sid, results, "s14g-motion-desktop"))

        await cdp.send("Emulation.setEmulatedMedia",
                       {"features": [{"name": "prefers-reduced-motion", "value": "reduce"}]},
                       session_id=sid)
        await asyncio.sleep(0.3)
        reduced_verdicts = await motion_pass(2.1)
        reduced_scoped = await evaluate(cdp, sid, f"""
            (() => {{
              const row = document.getElementById('tree-node-{worker_id}');
              if (!row) return null;
              const dot = row.querySelector('span[id^="unread-"]');
              const meta = row.querySelector('.tree-meta-row');
              const work = meta && meta.children[1] ? meta.children[1] : null;
              const badge = work && work.firstElementChild;
              return {{unreadDot: dot ? getComputedStyle(dot).animationName : null,
                       badgeDot: badge ? getComputedStyle(badge).animationName : null}};
            }})()""")
        await cdp.send("Emulation.setEmulatedMedia", {"features": []}, session_id=sid)
        reduced_ok = (all(ok for ok, _ in reduced_verdicts.values())
                      and bool(reduced_scoped)
                      and reduced_scoped.get("unreadDot") == "none"
                      and reduced_scoped.get("badgeDot") == "none")
        results.record(
            "s14h-motion-timeline-reduced-motion", reduced_ok,
            f"{run_label} under prefers-reduced-motion: reduce: "
            + "; ".join(detail for _, detail in reduced_verdicts.values())
            + f"; pulse cues under reduce: {reduced_scoped}",
            await screenshot(cdp, sid, results, "s14h-motion-reduced"))
    except (TimeoutError, RuntimeError) as exc:
        results.record("s14g-motion-timeline-desktop", False, f"{run_label}: {str(exc)[:280]}", None)
        results.record("s14h-motion-timeline-reduced-motion", False,
                       "skipped: the desktop window failed", None)

    # --- Late live windows and the stop ride a bounded child-manager turn ----
    # The worker Run's lifetime is the model's own (observed ~17-45s for a
    # bounded turn) and its task auto-completes on run success, removing the
    # row. A fresh bounded manager turn never auto-completes, so the reload
    # window, the narrow window and the stop stay observable on it.
    spin_el = f"spinner-{child_id}"
    gear_el = f"worker-indicator-{root_id}"
    await evaluate(cdp, sid, f"switchSession({json.dumps(child_id)})")
    await wait_for(cdp, sid, f"SESSION_ID === {json.dumps(child_id)}", timeout=15,
                   label="child manager selected for the late motion windows")
    await open_task_tab(cdp, sid, "chat")
    await wait_for(cdp, sid, "!!document.getElementById('msg-input')", timeout=10, label="child composer")
    child_message = ("Trial stop step. Write a 60-word story about a harbor, then a second 60-word "
                     "paragraph about a storm, then reply with exactly STOP-OK on the last line. "
                     "Do not create subtasks.")
    await evaluate(cdp, sid,
                   "(() => { const inp = document.getElementById('msg-input');"
                   f"inp.value = {json.dumps(child_message)};"
                   "inp.dispatchEvent(new Event('input')); })()")
    await click(cdp, sid, "#send-btn")
    child_run_id = None
    late_label = "child turn"
    try:
        await wait_row_activity(cdp, sid, child_id, {"spinner": True}, 120,
                                "child manager turn live for the late motion windows")
        child_run_id = await active_run_of(child_id)
        late_label = f"child run {str(child_run_id)[:8]}" if child_run_id else "child turn (live spinner)"

        # Ordinary refresh mid-run: a real reload re-renders the tree from
        # server facts over the reconnect path; the same live Run must still
        # animate afterwards (launch through ongoing work across a refresh).
        await cdp.send("Page.reload", {}, session_id=sid)
        await wait_for(cdp, sid,
                       f"!!document.getElementById('tree-node-{child_id}')"
                       f" && !document.getElementById('spinner-{child_id}')"
                       ".classList.contains('hidden')",
                       timeout=45, label="live spinner re-rendered after reload")
        await asyncio.sleep(0.3)
        reload_verdicts = await motion_pass(1.2, RELOAD_INTERVALS_S)
        results.record(
            "s14i-motion-timeline-after-reload",
            all(ok for ok, _ in reload_verdicts.values()),
            f"{late_label} across an ordinary mid-run reload: "
            + "; ".join(detail for _, detail in reload_verdicts.values()),
            await screenshot(cdp, sid, results, "s14i-motion-after-reload"))
    except (TimeoutError, RuntimeError) as exc:
        results.record("s14i-motion-timeline-after-reload", False,
                       f"{late_label}: {str(exc)[:280]}", None)

    # Narrow viewport: the same live cues at 390px, names still readable.
    await cdp.send("Emulation.setDeviceMetricsOverride",
                   {"width": 390, "height": 844, "deviceScaleFactor": 2, "mobile": True},
                   session_id=sid)
    await asyncio.sleep(0.6)
    # The drawer may have been left open by S12: open it only when closed, so
    # the scenario asserts the sidebar state it needs instead of a toggle parity.
    await evaluate(cdp, sid,
                   "(() => { const sb = document.getElementById('sidebar');"
                   " if (sb && !sb.classList.contains('open')) toggleMobileSidebar(); })()")
    await asyncio.sleep(0.4)
    narrow_child = await tree_row_activity_tolerant(cdp, sid, child_id)
    narrow_root = await tree_row_activity_tolerant(cdp, sid, root_id)
    if narrow_child is None:
        results.record("s14d-narrow-during-activity", False,
                       "the child row left the open tree before the narrow check; not "
                       "painted as a pass",
                       await screenshot(cdp, sid, results, "s14d-child-row-gone"))
    else:
        narrow_readability = await evaluate(cdp, sid, f"""
            (() => {{
              const row = document.getElementById('tree-node-{child_id}');
              const inner = row.firstElementChild;
              const name = row.querySelector('.session-name');
              const r = inner.getBoundingClientRect(), n = name.getBoundingClientRect();
              return {{rowWidth: r.width, nameWidth: n.width,
                       nameVisible: n.width > 60 && n.left >= r.left && n.right <= r.right + 1}};
            }})()""")
        results.record("s14d-narrow-during-activity",
                       bool(narrow_child.get("spinner") and narrow_readability.get("nameVisible")),
                       f"narrow during: child spinner={narrow_child.get('spinner')} "
                       f"root gear={None if narrow_root is None else narrow_root.get('gear')} "
                       f"readability={narrow_readability}",
                       await screenshot(cdp, sid, results, "s14d-during-narrow"))

    # The same live cues keep their timeline at the narrow viewport too.
    try:
        narrow_verdicts = await motion_pass(2.1)
        results.record(
            "s14j-motion-timeline-narrow",
            all(ok for ok, _ in narrow_verdicts.values()),
            f"{late_label} at 390px: " + "; ".join(detail for _, detail in narrow_verdicts.values()),
            await screenshot(cdp, sid, results, "s14j-motion-narrow"))
    except (TimeoutError, RuntimeError) as exc:
        results.record("s14j-motion-timeline-narrow", False, f"{late_label}: {str(exc)[:280]}", None)

    # The real stop: durable request, signal, observed exit -> interrupted, on
    # the child manager's own bounded turn (a finished worker run would
    # honestly report stop_requested=False; a manager never auto-completes, so
    # the interrupted row and its label stay observable).
    stop_target = await active_run_of(child_id)
    if stop_target is None and child_run_id is not None:
        stop_target = child_run_id
    if stop_target is None:
        results.record("s14e-stop-clears-activity", False,
                       "no active child run remained to stop (the bounded turn reached a "
                       "terminal state first); stop clearing not evidenced this round", None)
    else:
        status, cancel = api_request(base, access_key, "POST",
                                     f"/api/sessions/{child_id}/runs/{stop_target}/cancel",
                                     {"request_id": "trial-stop-" + stop_target[:8]})
        try:
            after = await wait_row_activity(cdp, sid, child_id,
                                            {"spinner": False, "gear": False}, 60,
                                            "row cleared after the stop")
            root_after = await tree_row_activity_tolerant(cdp, sid, root_id)
            label = after.get("label", "")
            w_status, w_page = api_request(base, access_key, "GET",
                                           f"/api/sessions/{worker_id}/runs?order=desc&limit=1")
            w_runs = (w_page.get("items") or []) if isinstance(w_page, dict) else []
            worker_row_after = await tree_row_activity_tolerant(cdp, sid, worker_id)
            results.record("s14e-stop-clears-activity",
                           bool(cancel.get("stop_requested")) and "attention" in label,
                           f"stopped child run {stop_target[:8]}: cancel={dict(cancel)} "
                           f"row label={label[:40]!r} "
                           f"root gear after={None if root_after is None else root_after.get('gear')}; "
                           f"worker run now={w_runs[0].get('state') if w_runs else 'none'}, "
                           f"worker row={'gone (auto-completed on its own success)' if worker_row_after is None else 'rendered'}",
                           await screenshot(cdp, sid, results, "s14e-after-stop-narrow"))
        except TimeoutError as exc:
            results.record("s14e-stop-clears-activity", False, str(exc)[:300],
                           await screenshot(cdp, sid, results, "s14e-fail"))
        except RuntimeError as exc:
            # The row left the open tree before the cleared state could be
            # read: recorded honestly, never painted as a pass.
            results.record("s14e-stop-clears-activity", False,
                           f"stop request sent to {child_id[:8]} ({dict(cancel)}), but the row "
                           f"left the open tree before the cleared state could be observed: "
                           f"{str(exc)[:200]}",
                           await screenshot(cdp, sid, results, "s14e-fail"))
    await cdp.send("Emulation.clearDeviceMetricsOverride", session_id=sid)
    await asyncio.sleep(0.4)
    results.record("s14f-desktop-after-stop",
                   True, "cleared device override; final desktop state recorded",
                   await screenshot(cdp, sid, results, "s14f-after-desktop"))

    # --- S15: unread-reply feedback on the real writer path -----------------
    # The unread writer is the finalize chain's summary delivery
    # (SessionManager.mark_unread inside _persist_worker_summary_once). The two
    # pristine roots from S6b/S6c (no history, no pending automation, root rows
    # always rendered) each get one real bounded manager turn: the row spins
    # while the run is live, shows the familiar unread dot once the summary
    # lands on the idle row, clears in BOTH clients when the task is opened
    # (the mark-read broadcast) while the other root stays unread. Manager
    # turns never auto-complete their task and the second client deep-links to
    # the neutral worker (a full page load marks the deep-linked session read
    # through the SSR bootstrap), so neither unread root is ever opened by the
    # rig. No model catalog is re-validated here.
    async def screenshot_tolerant(cdp_session: str, name: str) -> str | None:
        try:
            return await screenshot(cdp, cdp_session, results, name)
        except Exception as exc:  # a dead tab must not mask the scenario's own failure
            log(f"screenshot {name} failed: {exc!r}")
            return None

    await wait_for(cdp, sid,
                   f"!!document.getElementById('tree-node-{fresh_root_id}')"
                   f" && !!document.getElementById('tree-node-{rapid_root_id}')", timeout=15,
                   label="both trial root rows rendered")
    one_before = await tree_row_activity(cdp, sid, fresh_root_id)
    two_before = await tree_row_activity(cdp, sid, rapid_root_id)
    results.record("s15a-before-unread",
                   bool(one_before.get("unread") is False and two_before.get("unread") is False),
                   f"before any reply both dots are hidden: root6b unread={one_before.get('unread')} "
                   f"({one_before.get('label', '')[:24]!r}), root6c unread={two_before.get('unread')} "
                   f"({two_before.get('label', '')[:24]!r})",
                   await screenshot(cdp, sid, results, "s15a-before-unread"))

    trial_message = ("Trial unread step. Reply with exactly UNREAD-OK, then stop. "
                     "Do not create subtasks. Do not complete or close the task.")

    async def send_manager_turn(node_id: str) -> None:
        await evaluate(cdp, sid, f"switchSession({json.dumps(node_id)})")
        await wait_for(cdp, sid, f"SESSION_ID === {json.dumps(node_id)}", timeout=15,
                       label="trial root selected for its turn")
        await open_task_tab(cdp, sid, "chat")
        await wait_for(cdp, sid, "!!document.getElementById('msg-input')", timeout=10, label="composer")
        await evaluate(cdp, sid,
                       "(() => { const inp = document.getElementById('msg-input');"
                       f"inp.value = {json.dumps(trial_message)};"
                       "inp.dispatchEvent(new Event('input')); })()")
        await click(cdp, sid, "#send-btn")

    await send_manager_turn(fresh_root_id)

    # The row spins live; its own dot stays hidden behind the activity.
    try:
        await wait_row_activity(cdp, sid, fresh_root_id, {"spinner": True, "unread": False}, 120,
                                "trial root spinner during its manager turn")
        results.record("s15b-spinner-hides-unread",
                       True,
                       "the trial row spins live; the dot stays hidden while work runs",
                       await screenshot(cdp, sid, results, "s15b-during-spinner"))
    except TimeoutError as exc:
        results.record("s15b-spinner-hides-unread", False, str(exc)[:300],
                       await screenshot_tolerant(sid, "s15b-fail"))

    # Reduced-motion scope after the user's icon-motion correction: with the
    # OS preference emulated the running badge pulse and the unread dot pulse
    # stop, while the spinner and the delegated gear keep their original
    # rotation (the temporal motion proof under reduce lives in S14 on
    # genuinely visible cues; this row-level read pins the computed cascade on
    # a second real run). The badge pulse only exists while the row runs, so
    # its unemulated value is accepted as pulse-or-idle (the label decides),
    # never as a silent skip.
    probe_template = """
        (() => {
          const out = {};
          const anim = (rowId, selector) => {
            const row = document.getElementById('tree-node-' + rowId);
            if (!row) return null;
            const el = row.querySelector(selector);
            return el ? getComputedStyle(el).animationName : null;
          };
          out.spinner = anim('__SPIN__', 'svg[id^="spinner-"]');
          out.unreadDot = anim('__UNREAD__', 'span[id^="unread-"]');
          out.gear = anim('__GEAR__', 'svg[id^="worker-indicator-"]');
          out.badgeDot = null;
          const metaEl = document.getElementById('tree-node-' + '__SPIN__');
          const meta = metaEl ? metaEl.querySelector('.tree-meta-row') : null;
          const work = meta && meta.children[1] ? meta.children[1] : null;
          out.badgeDot = work && work.firstElementChild
            ? getComputedStyle(work.firstElementChild).animationName : null;
          out.spinLabel = metaEl ? (metaEl.textContent || '') : '';
          return out;
        })()
    """
    animation_probe = await evaluate(cdp, sid, probe_template
                                     .replace("__SPIN__", fresh_root_id)
                                     .replace("__UNREAD__", rapid_root_id)
                                     .replace("__GEAR__", rapid_root_id))
    badge_live = (animation_probe or {}).get("badgeDot") == "pulse" or (
        "idle" in str((animation_probe or {}).get("spinLabel", "")))
    results.record("s15b2-motion-present-unemulated",
                   bool(animation_probe) and animation_probe.get("spinner") == "spin"
                   and animation_probe.get("unreadDot") == "pulse-dot"
                   and animation_probe.get("gear") == "spin" and badge_live,
                   f"computed animations without emulation: {animation_probe}", None)
    await cdp.send("Emulation.setEmulatedMedia",
                   {"features": [{"name": "prefers-reduced-motion", "value": "reduce"}]}, session_id=sid)
    await asyncio.sleep(0.3)
    reduced = await evaluate(cdp, sid, probe_template
                             .replace("__SPIN__", fresh_root_id)
                             .replace("__UNREAD__", rapid_root_id)
                             .replace("__GEAR__", rapid_root_id))
    await cdp.send("Emulation.setEmulatedMedia", {"features": []}, session_id=sid)
    reduced_ok = (bool(reduced) and reduced.get("spinner") == "spin" and reduced.get("gear") == "spin"
                  and reduced.get("unreadDot") == "none" and reduced.get("badgeDot") == "none")
    results.record("s15h-reduced-motion-spinners-keep-motion-pulses-stop",
                   reduced_ok,
                   f"computed animations under prefers-reduced-motion: reduce -> {reduced}",
                   await screenshot_tolerant(sid, "s15h-reduced-motion"))

    async def wait_terminal_run(node_id: str, bound: float = 240.0) -> dict | None:
        deadline = time.monotonic() + bound
        while time.monotonic() < deadline:
            status, page = api_request(base, access_key, "GET",
                                       f"/api/sessions/{node_id}/runs?order=desc&limit=1")
            runs = (page.get("items") or []) if isinstance(page, dict) else []
            if runs and runs[0].get("state") in ("success", "failed", "stopped", "interrupted"):
                return runs[0]
            await asyncio.sleep(2)
        return None

    one_run = await wait_terminal_run(fresh_root_id)
    try:
        await wait_row_activity(cdp, sid, fresh_root_id, {"spinner": False, "unread": True}, 90,
                                "trial root idle with its unread dot after the summary landed")
        one_state = await tree_row_activity(cdp, sid, fresh_root_id)
        # Terminal transition of the activity cue: the spinner is gone and the
        # unread dot is the row's live pulse again.
        dot_anim = await evaluate(cdp, sid,
                                  f"(() => {{ const d = document.getElementById('unread-{fresh_root_id}');"
                                  " return d ? getComputedStyle(d).animationName : null; })()")
        results.record("s15c-unread-dot-after-turn",
                       bool(one_run and one_run.get("state") == "success"
                            and "idle" in one_state.get("label", "") and dot_anim == "pulse-dot"),
                       f"turn run {str(one_run and one_run.get('id'))[:8]} "
                       f"({one_run and one_run.get('state')}): the summary writer marked the "
                       f"session unread and the idle row shows the dot ({one_state.get('label', '')[:32]!r}, "
                       f"dot animation {dot_anim!r})",
                       await screenshot(cdp, sid, results, "s15c-unread"))
    except TimeoutError as exc:
        results.record("s15c-unread-dot-after-turn", False,
                       f"{str(exc)[:240]}; run terminal: {one_run and one_run.get('state')}",
                       await screenshot_tolerant(sid, "s15c-fail"))

    # The second trial root gets its own real reply: two unread tasks at once.
    await send_manager_turn(rapid_root_id)
    two_run = await wait_terminal_run(rapid_root_id)
    try:
        await wait_row_activity(cdp, sid, rapid_root_id, {"spinner": False, "unread": True}, 90,
                                "second trial root idle with its unread dot")
        await wait_row_activity(cdp, sid, fresh_root_id, {"unread": True}, 20,
                                "the first root's dot is still visible")
        results.record("s15d-two-unread-tasks",
                       bool(two_run and two_run.get("state") == "success"),
                       f"second turn run {str(two_run and two_run.get('id'))[:8]} "
                       f"({two_run and two_run.get('state')}): two idle rows, two unread dots",
                       await screenshot(cdp, sid, results, "s15d-two-unread"))
    except TimeoutError as exc:
        results.record("s15d-two-unread-tasks", False,
                       f"{str(exc)[:240]}; run terminal: {two_run and two_run.get('state')}",
                       await screenshot_tolerant(sid, "s15d-fail"))

    # Second client: a real second tab on the same profile (its own WebSocket).
    # It deep-links to the neutral worker: the deep link marks THAT session
    # read (the SSR bootstrap's mark-read), never the two unread roots.
    target2 = await cdp.send("Target.createTarget", {"url": f"{base}/?session={worker_id}"})
    attached2 = await cdp.send("Target.attachToTarget", {"targetId": target2["targetId"], "flatten": True})
    sid2 = attached2["sessionId"]
    await cdp.send("Page.enable", session_id=sid2)
    await cdp.send("Runtime.enable", session_id=sid2)
    await cdp.send("Page.addScriptToEvaluateOnNewDocument", {"source": GUARD_SOURCE}, session_id=sid2)
    try:
        await wait_for(cdp, sid2, "!!document.getElementById('preview-indicator')", timeout=25,
                       label="second client logged in")
        await wait_for(cdp, sid2,
                       f"!!document.getElementById('tree-node-{fresh_root_id}')"
                       f" && !!document.getElementById('tree-node-{rapid_root_id}')", timeout=20,
                       label="second client renders both trial rows")
        two_one = await tree_row_activity(cdp, sid2, fresh_root_id)
        two_two = await tree_row_activity(cdp, sid2, rapid_root_id)
        results.record("s15e-second-client-unread",
                       bool(two_one.get("unread") and two_two.get("unread")),
                       f"second client rows: root6b unread={two_one.get('unread')} "
                       f"({two_one.get('label', '')[:24]!r}), root6c unread={two_two.get('unread')} "
                       f"({two_two.get('label', '')[:24]!r})",
                       await screenshot(cdp, sid2, results, "s15e-second-client-unread"))
    except (TimeoutError, AssertionError) as exc:
        results.record("s15e-second-client-unread", False, str(exc)[:300],
                       await screenshot_tolerant(sid2, "s15e-fail"))

    # Opening one trial root clears exactly its dot everywhere; the other
    # root's dot survives (another task remaining unread).
    try:
        await evaluate(cdp, sid, f"switchSession({json.dumps(fresh_root_id)})")
        await wait_for(cdp, sid, f"SESSION_ID === {json.dumps(fresh_root_id)}", timeout=15,
                       label="first client opened the first trial root")
        await wait_row_activity(cdp, sid, fresh_root_id, {"unread": False}, 20,
                                "opened task's dot cleared in the first client")
        await wait_row_activity(cdp, sid2, fresh_root_id, {"unread": False}, 20,
                                "opened task's dot cleared in the second client (read broadcast)")
        await wait_row_activity(cdp, sid2, rapid_root_id, {"unread": True}, 20,
                                "the other root remains unread in the second client")
        results.record("s15f-open-clears-one-keeps-other",
                       True,
                       "opening the first trial root cleared its dot in both clients; "
                       "the second root remains unread",
                       await screenshot(cdp, sid2, results, "s15f-opened-read"))
    except TimeoutError as exc:
        results.record("s15f-open-clears-one-keeps-other", False, str(exc)[:300],
                       await screenshot_tolerant(sid, "s15f-fail"))
    finally:
        tab2_errs = await evaluate(cdp, sid2, "window.__errs || []")
        cdp.console_errors = list(cdp.console_errors or []) + [f"tab2: {e}" for e in (tab2_errs or [])]
        await cdp.send("Target.closeTarget", {"targetId": target2["targetId"]})

    # --- S13: every selected model is a real choice with a real Run ---------
    # For each non-default dropdown entry: a real selection change, a real New
    # Session click, the create carrying that backend, the node metadata and
    # the header badge agreeing, and one bounded first message whose
    # manager_turn Run records the same backend and a native session inside the
    # preview home. Provider/network failures are recorded, never retried
    # forever and never scripted into a pass.
    for model_id, model_label in BACKEND_LABELS.items():
        if model_id == selected_default:
            continue
        previous_session_id = await evaluate(cdp, sid, "SESSION_ID")
        await evaluate(cdp, sid,
                       "(() => { const sel = document.getElementById('new-session-backend');"
                       f" sel.value = {json.dumps(model_id)};"
                       " sel.dispatchEvent(new Event('change')); })()")
        await click_button_by_text(cdp, sid, "New Session", "#sidebar")
        # The create-and-open switches asynchronously: wait until the view actually
        # moved off the previous node before reading anything about the new one.
        await wait_for(cdp, sid,
                       f"typeof SESSION_ID !== 'undefined' && SESSION_ID && SESSION_ID !== {json.dumps(previous_session_id)}",
                       timeout=20, label=f"root switched for {model_id}")
        model_root_id = await evaluate(cdp, sid, "SESSION_ID")
        model_posts = [m for m in cdp.mutations
                       if m["method"] == "POST" and m["url"].endswith("/api/sessions/")]
        model_body = json.loads(model_posts[-1]["body"]) if model_posts else {}
        meta = await evaluate(cdp, sid,
                              "fetch('/api/sessions/' + SESSION_ID, {cache: 'no-store'})"
                              ".then(r => r.json())")
        badge_label = await evaluate(cdp, sid, "document.getElementById('backend-badge').textContent")
        selection_ok = (model_body.get("backend") == model_id
                        and (meta or {}).get("backend") == model_id
                        and badge_label == model_label)
        results.record(f"s13-select-{model_id}",
                       bool(model_root_id and selection_ok),
                       f"root={str(model_root_id)[:8]} create-backend={model_body.get('backend')!r} "
                       f"metadata-backend={(meta or {}).get('backend')!r} badge={badge_label!r}",
                       await screenshot(cdp, sid, results, f"s13-select-{model_id}"))
        # One bounded first message on the model's own root.
        message = (f"Model check for {model_id}. Reply with exactly: MODEL-OK, then stop. "
                   "Do not create subtasks.")
        await wait_for(cdp, sid,
                       f"SESSION_ID === {json.dumps(model_root_id)} && !!document.getElementById('msg-input')",
                       timeout=15, label=f"composer on {model_id}")
        await evaluate(cdp, sid,
                       "(() => { const inp = document.getElementById('msg-input');"
                       f"inp.value = {json.dumps(message)};"
                       "inp.dispatchEvent(new Event('input')); })()")
        await click(cdp, sid, "#send-btn")
        await wait_for(cdp, sid,
                       f"document.getElementById('tab-chat').textContent.includes({json.dumps(message)})",
                       timeout=15, label=f"first message rendered on {model_id}")
        log(f"  waiting for the real {model_label} manager turn (bounded)")
        run_deadline = time.monotonic() + 180
        model_run = None
        while time.monotonic() < run_deadline:
            status, page = api_request(base, access_key,
                                       "GET", f"/api/sessions/{model_root_id}/runs?limit=5")
            runs = (page.get("items") or []) if isinstance(page, dict) else (page or [])
            terminal = [r for r in runs if r.get("kind") == "manager_turn" and r.get("state") in
                        ("success", "failed", "stopped", "interrupted")]
            if terminal:
                model_run = terminal[0]
                break
            await asyncio.sleep(2)
        native_entries = sorted(p.name for p in (home / "clc-sessions").iterdir())
        if model_run is None:
            results.record(f"s13-live-{model_id}", False,
                           "no terminal manager_turn run within 180s (provider/network failure "
                           "is an explicit failed live check)",
                           await screenshot(cdp, sid, results, f"s13-fail-{model_id}"))
        else:
            # Same live-claim bar as the default model's s08: only a successful
            # turn with the selected backend and a native session inside the
            # preview home passes; a failed terminal run is a failed check.
            run_ok = (model_run.get("state") == "success"
                      and model_run.get("backend") == model_id
                      and bool(model_run.get("native_session_id"))
                      and len(native_entries) > 0)
            results.record(f"s13-live-{model_id}", run_ok,
                           f"run={model_run['id'][:8]} state={model_run.get('state')} "
                           f"backend={model_run.get('backend')!r} "
                           f"native={model_run.get('native_session_id')} "
                           f"native-dir-entries={len(native_entries)}",
                           await screenshot(cdp, sid, results, f"s13-live-{model_id}"))

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
    parser.add_argument("--backends", required=True,
                        help="Comma-separated charlie-code backend ids: the first is the "
                             "trial's default, the rest become the selectable model "
                             "dropdown entries. Required: the trial's model catalog is "
                             "always named by the invocation, never baked in.")
    parser.add_argument("--chrome", default=None, help="Chrome binary (default: google-chrome)")
    args = parser.parse_args()
    asyncio.run(run_harness(args))


if __name__ == "__main__":
    main()
