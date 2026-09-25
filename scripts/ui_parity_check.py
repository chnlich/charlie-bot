#!/usr/bin/env python3
"""Layout parity check: main against this branch on the same synthetic sessions.

The task-tree UI may change the production layout only by an explicit list of
intended differences. This check renders the sidebar and the tab bar of both
checkouts from identical session files, at desktop and phone widths, and fails
on any difference outside that list, so a stray layout change is caught before
anyone looks at the trial instance.

How it works:

- Main side. A temporary detached worktree at --main-ref (default: the merge
  base of HEAD and origin/main, the main this branch last absorbed).
- Servers. Each side serves its own ``server.app`` from its own checkout in a
  child process (``serve`` mode below) with uvicorn ``lifespan="off"`` (no
  scheduler, recovery or trigger scans) on a free local port, against a fresh
  temporary CHARLIEBOT_HOME with a synthetic access key. Neither side runs in
  preview mode, so the preview-only differences (the preview marker, the hidden
  Terminal tab and the hidden scheduled-task "+") stay out of the comparison
  and are checked by eye on the trial instance.
- Data. The same metadata.json files are written into both homes: three
  groups, one root with a child session and a worker, two archived workers and
  plain sessions. Main ignores the task fields and lists every session flat.
- Browser. System google-chrome, headless, over CDP with a private profile.
  Each view exports the DOM outline of ``aside#sidebar`` and ``main > header``
  (tag, id, classes, own text, visibility) after the whitelist below drops its
  nodes, and the two outlines must be identical.

Whitelist (the intended visible differences of the task-tree layout):

- nested-row: a session row whose parent's row is in the same list (the branch
  nests it under the parent; main shows it flat).
- tree-container, tree-toggle: the subtree wrapper and the expand chevron.
- worker-icon: the leaf or delivered-check icon on a worker row.
- new-child-action, task-context-action: the two hover row actions.
- workers-tab: the Workers tab button, removed on the branch.
The preview cap counting only root rows is the one intended difference the data
does not reach: every group holds at most five rows even when flat. Apart from
the whitelist, the logo's build label (served commit and date) is compared as a
placeholder.

Run:  uv run python scripts/ui_parity_check.py [--main-ref REF] [--chrome BIN]
        [--evidence-dir DIR]
Exit 0: no difference outside the whitelist. Exit 1: differences, printed as a
unified diff per view. Exit 2: the check could not run.
"""

from __future__ import annotations

import argparse
import asyncio
import difflib
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

SCRIPT_REPO = Path(__file__).resolve().parent.parent
if str(SCRIPT_REPO) not in sys.path:
    sys.path.insert(0, str(SCRIPT_REPO))

from scripts.browser_harness_session_tree import CDP, pick_free_port  # noqa: E402
from src.core.process import terminate_and_wait  # noqa: E402

READY_PREFIX = "PARITY SERVE READY "
WIDTHS = ((1440, 900, False), (390, 844, True))
FILTERS = ("all", "archived")
ACTIVE_ID = "00000000-0000-4000-8000-0000000000a0"
# The logo's build label names the served commit and its date, so it differs
# between any two commits; both sides compare it as one placeholder.
BUILD_LABEL = re.compile(r"\b[0-9a-f]{7,40} \u00b7 \d{2}-\d{2}\b")


def fail(message: str) -> None:
    print(f"UI PARITY CHECK COULD NOT RUN: {message}", file=sys.stderr, flush=True)
    raise SystemExit(2)


# ---------------------------------------------------------------------------
# Synthetic sessions: one list, written identically into both homes.
# ---------------------------------------------------------------------------


def _sid(n: int) -> str:
    return f"00000000-0000-4000-8000-{n:012x}"


def synthetic_sessions() -> list[dict]:
    """(id, name, group, status, parent, profile, starred) rows, newest first."""
    root, child, worker = _sid(1), _sid(2), _sid(3)
    rows = [
        (ACTIVE_ID, "Release notes", "alpha", "active", None, None, False),
        (root, "Rework auth", "alpha", "active", None, "manager", False),
        (child, "Token store", "alpha", "active", root, "manager", False),
        (worker, "Implement the parser", "alpha", "active", root, "worker", False),
        (_sid(4), "Benchmark sweep", "beta", "active", None, None, True),
        (_sid(5), "Loader cleanup", "beta", "active", None, None, False),
        (_sid(6), "Onboarding doc", "gamma", "active", None, None, False),
        (_sid(7), "Scratchpad", None, "active", None, None, False),
        (_sid(8), "Weekly review", None, "active", None, None, False),
        (_sid(9), "Implement the cache", "alpha", "archived", root, "worker", False),
        (_sid(10), "Implement the retry", "alpha", "archived", root, "worker", False),
    ]
    out = []
    for index, (sid, name, group, status, parent, profile, starred) in enumerate(rows):
        # Fixed times months back, one hour apart: the list order and every
        # rendered time label come out the same on both sides.
        stamp = f"2026-01-15T{20 - index:02d}:00:00Z"
        meta = {
            "id": sid, "name": name, "status": status, "backend": "fake-scripted",
            "group": group, "starred": starred, "created_at": stamp, "updated_at": stamp,
        }
        if profile:
            meta.update({"schema_version": 2, "profile": profile, "task_parent_id": parent})
        out.append(meta)
    return out


def write_home(home: Path, port: int, access_key: str, sessions: list[dict]) -> None:
    home.mkdir(parents=True)
    config = {
        "server": {"port": port, "host": "127.0.0.1"},
        "backends": {"options": [{
            "id": "fake-scripted", "label": "Scripted (never launches)",
            "type": "cc-claude", "model": "scripted-model",
        }], "preference": ["fake-scripted"]},
        "paths": {"worktree_dir": str(home / "worktrees")},
    }
    (home / "config.yaml").write_text(json.dumps(config, indent=2), encoding="utf-8")
    (home / "credentials.yaml").write_text(f"charliebot:\n  access_key: {access_key}\n", encoding="utf-8")
    for meta in sessions:
        session_dir = home / "sessions" / meta["id"]
        session_dir.mkdir(parents=True)
        (session_dir / "metadata.json").write_text(json.dumps(meta, indent=2), encoding="utf-8")


# ---------------------------------------------------------------------------
# serve mode: one side's own server.app, run from that side's checkout.
# ---------------------------------------------------------------------------


def serve(repo: Path, port: int) -> None:
    sys.path.insert(0, str(repo))
    import uvicorn

    import server
    import src

    # The ready line names the imported modules, so the parent can assert that
    # this side really serves its own checkout.
    print(READY_PREFIX + json.dumps({"server": server.__file__, "src": list(src.__path__)}), flush=True)
    uvicorn.run(server.app, host="127.0.0.1", port=port, log_level="error", lifespan="off")


class Side:
    """One checkout served from a temporary home by a child process."""

    def __init__(self, name: str, repo: Path, tmp: Path, sessions: list[dict]) -> None:
        self.name = name
        self.repo = repo
        self.port = pick_free_port()
        self.access_key = f"parity-{name}-" + os.urandom(8).hex()
        self.home = tmp / f"home-{name}"
        write_home(self.home, self.port, self.access_key, sessions)
        self.log_path = tmp / f"serve-{name}.log"
        self.proc: subprocess.Popen | None = None

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def start(self) -> None:
        env = {k: v for k, v in os.environ.items()
               if k not in ("CHARLIEBOT_SESSION_ID", "CHARLIEBOT_ACCESS_KEY", "VIRTUAL_ENV")}
        env.update({"CHARLIEBOT_HOME": str(self.home), "PYTHONPATH": str(self.repo)})
        log = self.log_path.open("w", encoding="utf-8")
        self.proc = subprocess.Popen(
            [sys.executable, str(Path(__file__).resolve()), "serve", "--repo", str(self.repo),
             "--port", str(self.port)],
            cwd=self.repo, env=env, stdout=subprocess.PIPE, stderr=log, text=True)
        line = self.proc.stdout.readline()
        if not line.startswith(READY_PREFIX):
            fail(f"{self.name} server did not start; log {self.log_path}: {line.strip()}")
        modules = json.loads(line[len(READY_PREFIX):])
        paths = [modules["server"], *modules["src"]]
        if not all(Path(p).resolve().is_relative_to(self.repo.resolve()) for p in paths):
            fail(f"{self.name} server imported code outside {self.repo}: {paths}")
        deadline = time.monotonic() + 30
        while True:
            try:
                with socket.create_connection(("127.0.0.1", self.port), timeout=1):
                    return
            except OSError:
                if self.proc.poll() is not None or time.monotonic() > deadline:
                    fail(f"{self.name} server is not listening; log {self.log_path}")
                time.sleep(0.1)

    def stop(self) -> None:
        if self.proc:
            terminate_and_wait(self.proc, term_timeout_s=10, kill_timeout_s=10)


# ---------------------------------------------------------------------------
# Browser: CDP helpers and the outline export.
# ---------------------------------------------------------------------------


# Walks one root and returns [depth, tag, id, classes, text, visible] rows,
# skipping each whitelisted node with its subtree and counting the rule hits.
OUTLINE_JS = r"""
(function (parentOf) {
  const present = new Set([...document.querySelectorAll('#session-list [id^="session-"]')]
    .map((el) => el.id.slice('session-'.length)));
  const hits = {};
  function rule(el) {
    const id = el.id || '';
    if (id.startsWith('session-')) {
      const parent = parentOf[id.slice('session-'.length)];
      if (parent && present.has(parent)) return 'nested-row';
    }
    if (el.classList.contains('tree-subtree') || el.classList.contains('tree-children')) return 'tree-container';
    if (el.hasAttribute('data-tree-toggle')) return 'tree-toggle';
    const onclick = el.getAttribute('onclick') || '';
    if (onclick.includes('createChildSession(')) return 'new-child-action';
    if (onclick.includes('openTaskContextModal(')) return 'task-context-action';
    if (id === 'btn-workers') return 'workers-tab';
    if (el.tagName.toLowerCase() === 'svg' && (el.getAttribute('title') || '').startsWith('Worker (')) return 'worker-icon';
    return null;
  }
  function ownText(el) {
    let text = '';
    for (const node of el.childNodes) if (node.nodeType === 3) text += node.textContent;
    return text.replace(/\s+/g, ' ').trim();
  }
  function visible(el) {
    return el.getClientRects().length > 0 && getComputedStyle(el).visibility !== 'hidden';
  }
  const rows = [];
  function walk(el, depth) {
    const hit = rule(el);
    if (hit) { hits[hit] = (hits[hit] || 0) + 1; return; }
    const classes = [...el.classList].sort().join('.');
    rows.push([depth, el.tagName.toLowerCase(), el.id || '', classes, ownText(el), visible(el)]);
    for (const child of el.children) walk(child, depth + 1);
  }
  for (const selector of ['aside#sidebar', 'main > header']) {
    const root = document.querySelector(selector);
    if (root) walk(root, 0); else rows.push([0, 'missing', selector, '', '', false]);
  }
  return {rows, hits};
})
"""


async def evaluate(cdp: CDP, session_id: str, expression: str):
    result = await cdp.send("Runtime.evaluate", {
        "expression": expression, "returnByValue": True, "awaitPromise": True}, session_id=session_id)
    if "exceptionDetails" in result:
        raise RuntimeError(str(result["exceptionDetails"])[:500])
    return result.get("result", {}).get("value")


async def wait_for(cdp: CDP, session_id: str, expression: str, what: str, timeout: float = 20) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if await evaluate(cdp, session_id, expression):
            return
        await asyncio.sleep(0.2)
    fail(f"timed out waiting for {what}")


def outline_lines(rows: list[list]) -> list[str]:
    lines = []
    for depth, tag, el_id, classes, text, shown in rows:
        head = tag + (f"#{el_id}" if el_id else "") + (f".{classes}" if classes else "")
        text = BUILD_LABEL.sub("<build label>", text)
        lines.append("  " * depth + head + (f" {json.dumps(text)}" if text else "") + ("" if shown else " [hidden]"))
    return lines


async def capture(cdp: CDP, session_id: str, side: Side, width: int, height: int, mobile: bool,
                  view_filter: str, parent_of: dict) -> dict:
    await cdp.send("Emulation.setDeviceMetricsOverride", {
        "width": width, "height": height, "deviceScaleFactor": 1, "mobile": mobile}, session_id=session_id)
    await cdp.send("Network.setCookie", {
        "name": "charliebot_access_key", "value": side.access_key, "url": side.base}, session_id=session_id)
    await cdp.send("Page.navigate", {"url": f"{side.base}/?session={ACTIVE_ID}"}, session_id=session_id)
    await wait_for(cdp, session_id, f"!!document.getElementById('session-{ACTIVE_ID}')",
                   f"the {side.name} session list")
    if mobile:
        # The phone layout keeps the sidebar in a closed drawer; open it so the
        # rows are compared as rendered, not as hidden nodes.
        await evaluate(cdp, session_id, "toggleMobileSidebar()")
        await asyncio.sleep(0.5)
    if view_filter != "all":
        await evaluate(cdp, session_id, f"switchSidebarFilter({json.dumps(view_filter)})")
        await wait_for(cdp, session_id,
                       "!!document.querySelector('#session-list [id^=\"session-\"]:not(#session-"
                       + ACTIVE_ID + ")') && !document.getElementById('session-" + ACTIVE_ID + "')",
                       f"the {side.name} {view_filter} list")
    # Let the first status poll and any deferred paint settle before reading.
    await asyncio.sleep(2.0)
    return await evaluate(cdp, session_id, OUTLINE_JS + f"({json.dumps(parent_of)})")


async def run_browser(chrome: str, sides: list[Side], parent_of: dict) -> dict:
    import websockets

    profile = Path(tempfile.mkdtemp(prefix="ui-parity-chrome-"))
    debug_port = pick_free_port()
    proc = subprocess.Popen(
        [chrome, "--headless=new", f"--remote-debugging-port={debug_port}", f"--user-data-dir={profile}",
         "--no-first-run", "--no-default-browser-check", "--disable-background-networking",
         "--remote-allow-origins=*", "about:blank"],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    try:
        ws_url = None
        deadline = time.monotonic() + 20
        while ws_url is None and time.monotonic() < deadline:
            line = proc.stderr.readline()
            if "DevTools listening on ws://" in line:
                ws_url = line.strip().split()[-1]
        if ws_url is None:
            fail("chrome devtools endpoint did not come up")
        async with websockets.connect(ws_url, max_size=50 * 1024 * 1024) as ws:
            cdp = CDP(ws)
            captures: dict = {}
            for width, height, mobile in WIDTHS:
                for view_filter in FILTERS:
                    view = f"{width}px/{view_filter}"
                    captures[view] = {}
                    for side in sides:
                        target = await cdp.send("Target.createTarget", {"url": "about:blank"})
                        attached = await cdp.send("Target.attachToTarget",
                                                  {"targetId": target["targetId"], "flatten": True})
                        session_id = attached["sessionId"]
                        await cdp.send("Page.enable", session_id=session_id)
                        await cdp.send("Network.enable", session_id=session_id)
                        await cdp.send("Page.addScriptToEvaluateOnNewDocument", {"source": (
                            f"try {{ localStorage.setItem('charliebot_access_key', "
                            f"{json.dumps(side.access_key)}); }} catch (e) {{}}")}, session_id=session_id)
                        captures[view][side.name] = await capture(
                            cdp, session_id, side, width, height, mobile, view_filter, parent_of)
                        await cdp.send("Target.closeTarget", {"targetId": target["targetId"]})
            return captures
    finally:
        terminate_and_wait(proc, term_timeout_s=5, kill_timeout_s=5)
        shutil.rmtree(profile, ignore_errors=True)


# ---------------------------------------------------------------------------
# Orchestration
# ---------------------------------------------------------------------------


def git(*args: str) -> str:
    return subprocess.run(["git", *args], cwd=SCRIPT_REPO, capture_output=True, text=True,
                          check=True).stdout.strip()


def resolve_main_ref(ref: str | None) -> str:
    try:
        return git("rev-parse", "--verify", (ref or git("merge-base", "HEAD", "origin/main")) + "^{commit}")
    except subprocess.CalledProcessError as exc:
        fail(f"cannot resolve the main reference {ref or 'merge-base HEAD origin/main'}: {exc.stderr.strip()}")


def check(args: argparse.Namespace) -> int:
    chrome = args.chrome or shutil.which("google-chrome") or shutil.which("google-chrome-stable")
    if not chrome:
        fail("google-chrome is not installed; pass --chrome")
    main_sha = resolve_main_ref(args.main_ref)
    branch_sha = git("rev-parse", "HEAD")
    evidence = Path(args.evidence_dir or tempfile.mkdtemp(prefix="ui-parity-evidence-"))
    evidence.mkdir(parents=True, exist_ok=True)
    sessions = synthetic_sessions()
    parent_of = {m["id"]: m["task_parent_id"] for m in sessions if m.get("task_parent_id")}
    print(f"main {main_sha[:10]} vs branch {branch_sha[:10]} (working tree of {SCRIPT_REPO})", flush=True)

    with tempfile.TemporaryDirectory(prefix="ui-parity-") as tmp_name:
        tmp = Path(tmp_name)
        main_tree = tmp / "main-checkout"
        git("worktree", "add", "--detach", "--quiet", str(main_tree), main_sha)
        sides = [Side("main", main_tree, tmp, sessions), Side("branch", SCRIPT_REPO, tmp, sessions)]
        try:
            for side in sides:
                side.start()
            captures = asyncio.run(run_browser(chrome, sides, parent_of))
        finally:
            for side in sides:
                side.stop()
            for side in sides:
                shutil.copy(side.log_path, evidence / side.log_path.name)
            git("worktree", "remove", "--force", str(main_tree))

    failures = 0
    report = {"main": main_sha, "branch": branch_sha, "views": {}}
    for view, by_side in captures.items():
        main_lines = outline_lines(by_side["main"]["rows"])
        branch_lines = outline_lines(by_side["branch"]["rows"])
        diff = list(difflib.unified_diff(main_lines, branch_lines, "main", "branch", lineterm="", n=2))
        hits = {side: by_side[side]["hits"] for side in by_side}
        stem = view.replace("/", "-")
        (evidence / f"outline-{stem}-main.txt").write_text("\n".join(main_lines) + "\n", encoding="utf-8")
        (evidence / f"outline-{stem}-branch.txt").write_text("\n".join(branch_lines) + "\n", encoding="utf-8")
        report["views"][view] = {"hits": hits, "diff": diff, "main_nodes": len(main_lines),
                                 "branch_nodes": len(branch_lines)}
        state = "differs" if diff else "identical"
        print(f"{view}: {state} ({len(main_lines)} nodes compared; whitelist hits "
              f"main {hits['main']}, branch {hits['branch']})", flush=True)
        if diff:
            failures += 1
            print("\n".join(diff), flush=True)
    (evidence / "ui_parity_report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(f"evidence: {evidence}", flush=True)
    if failures:
        print(f"UI PARITY CHECK FAILED: {failures} of {len(captures)} views differ outside the whitelist",
              flush=True)
        return 1
    print("UI PARITY CHECK PASSED", flush=True)
    return 0


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="mode")
    serve_parser = sub.add_parser("serve", help="internal: serve one checkout (spawned by the check)")
    serve_parser.add_argument("--repo", type=Path, required=True)
    serve_parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--main-ref", default=None, help="main commit (default: merge-base HEAD origin/main)")
    parser.add_argument("--chrome", default=None, help="Chrome binary (default: google-chrome on PATH)")
    parser.add_argument("--evidence-dir", default=None, help="report and server logs (default: a new temp dir)")
    args = parser.parse_args()
    if args.mode == "serve":
        serve(args.repo, args.port)
        return
    raise SystemExit(check(args))


if __name__ == "__main__":
    main()
