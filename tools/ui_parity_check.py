#!/usr/bin/env python3
"""Layout parity check: main against this branch on the whole chat page.

The production page may change only by an explicit list of intended
differences. This check renders the whole page of both checkouts from identical
session files, at desktop and phone widths, after load, under the archived
sidebar filter, and after a click on every tab button, and fails on any outline
difference outside that list and on any script error the branch side raises
that main does not.

How it works:

- Main side. A temporary detached worktree at --main-ref (default: the merge
  base of HEAD and origin/main). A worktree checkout has no build artifacts
  and the check builds nothing, so when ``src/runtime/agent_process/_vfkspawn.c``
  is identical on both sides the check copies the branch tree's built
  extension into the main worktree; otherwise it stops and names the
  difference.
- Servers. Each side serves its own ``server.app`` from its own checkout in a
  child process (``serve`` mode below) with uvicorn ``lifespan="off"`` (no
  scheduler, recovery or trigger scans) on a free local port, against a fresh
  temporary CHARLIEBOT_HOME with a synthetic access key; the tmux server that
  the Terminal tab starts keeps its socket in a directory of the side's own
  (TMUX_TMPDIR) and is killed when the side stops.
- Data. The same metadata.json files are written into both homes: three
  groups, one root with a child session and a worker, two archived workers and
  plain sessions. Every row carries the schema_version 2 profile the
  server requires, and the task rows nest the same on both sides.
- Browser. The first of google-chrome, google-chrome-stable, chromium,
  chromium-browser on PATH, headless, over CDP with a private profile; pass
  --chrome to name a binary. Each view exports the DOM outline of
  ``document.body`` (tag, id, classes, own text) after the whitelist below
  and every invisible subtree drop their nodes — the check compares the
  rendered page of each view — and the two outlines must be identical.
- Views. Each width renders the page after load (``all``), after the archived
  sidebar filter (``archived``), and after a click on each button inside
  ``#tab-buttons`` (``tab-<button id>``). The buttons come from the main
  side's loaded DOM in page order, never from a list in this tool, so the
  views follow the page's own button set and order.
- Script errors. Per side and view the check collects uncaught exceptions
  (Runtime.exceptionThrown), console errors (Runtime.consoleAPICalled with
  type error) and log errors (Log.entryAdded with level error), normalized so
  the side's own port, stack positions and asset version stamps (?v=, ?t=) do
  not register. The check fails when the branch side has an error message the
  main side lacks; the report lists the errors of both sides.

Whitelist (the tree-UI nodes the outline drops on both sides):

- nested-row: a session row whose parent's row is in the same list (both sides
  nest the same child rows, so the all view compares the root rows only).
- tree-container, tree-toggle: the subtree wrapper and the expand chevron.
- worker-icon: the leaf or delivered-check icon on a worker row.
- new-child-action, task-context-action: the two hover row actions.
- xterm-screen: the terminal's live screen rows (.xterm-rows and its subtree):
  a real shell's prompt and the tmux status clock, whose row count and
  timestamps differ between two loads of the same tree. The terminal's layout
  (#tab-terminal, the .xterm wrapper and viewport) stays in the outline.
- xterm-width-cache: the off-screen holder (a div at top -50000px under body)
  that xterm's DOM renderer measures glyph widths in. Its spans hold the last
  characters the terminal measured, which follow the order a real shell's
  output paints in, so two loads of the same tree differ (``<`` against ``t``).
The preview cap counting only root rows is the one intended difference the data
does not reach: every group holds at most five rows even when flat.

Stable content: a node whose text changes between two loads of the same tree
(a relative time, a connection state, a clock) compares as a placeholder; see
PLACEHOLDER_RULES, where each rule names its node and the reason. Intended
differences beyond that go to the --expected file, one entry per line:

  <view> | <outline line, as printed in the diff without the +/- prefix>

For example, a landing that adds a release banner inside the header lists:

  1440px/all |   div#release-banner "Release 12 is out"

A diff line with no entry fails the check, and an entry that matches no diff
line fails the check, so the file stays exact. Lines starting with # are
comments.

Run:  uv run python tools/ui_parity_check.py [--main-ref REF] [--chrome BIN]
        [--expected FILE] [--evidence-dir DIR]
On host3 (Playwright's Chromium; /usr/bin/chromium-browser there is a snap
wrapper that cannot run headless on a temporary profile):
  uv run python tools/ui_parity_check.py --main-ref origin/main \
    --chrome ~/.cache/ms-playwright/chromium-1228/chrome-linux/chrome
Exit 0: no difference outside the whitelist and the --expected file, and no
new script error. Exit 1: differences or new script errors, printed per view.
Exit 2: the check could not run.
"""

from __future__ import annotations

import argparse
import asyncio
import difflib
import json
import os
import pathlib
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

SCRIPT_REPO = pathlib.Path(__file__).resolve().parent.parent
if str(SCRIPT_REPO) not in sys.path:
  sys.path.insert(0, str(SCRIPT_REPO))

from src.infra import constants  # noqa: E402
from tools import browser_harness_session_tree  # noqa: E402

READY_PREFIX = "PARITY SERVE READY "
WIDTHS = ((1440, 900, False), (390, 844, True))
ACTIVE_ID = "00000000-0000-4000-8000-0000000000a0"
CHROME_CANDIDATES = ("google-chrome", "google-chrome-stable", "chromium", "chromium-browser")
VFKSPAWN_C = pathlib.Path("src/runtime/agent_process/_vfkspawn.c")
# Stack frames and code positions inside an error message: the same error on
# both sides must compare equal, so its location does not register.
LINE_COL = re.compile(r":\d+:\d+")
# Query stamps that differ between any two loads or commits: the
# ?v=<commit>-<date>-<hash> that every static asset URL (so every stack frame)
# carries.
CACHE_BUSTER = re.compile(r"([?&]v)=[^\s&:)\"'\\]+")

# Placeholder rules: each entry names one node whose own text changes between
# two loads of the same tree, with the reason; outline_lines rewrites the
# matched text before comparing, so only the varying part disappears.
PLACEHOLDER_RULES = (
    # The logo's build label names the served commit and its date, so it
    # differs between any two commits.
    ("BUILD_LABEL", re.compile(r"\b[0-9a-f]{7,40} · \d{2}-\d{2}\b"), "<build label>"),)


def fail(message: str) -> None:
  print(f"UI PARITY CHECK COULD NOT RUN: {message}", file=sys.stderr, flush=True)
  raise SystemExit(2)


# ---------------------------------------------------------------------------
# Synthetic sessions: one list, written identically into both homes.
# ---------------------------------------------------------------------------


def _sid(n: int) -> str:
  return f"00000000-0000-4000-8000-{n:012x}"


def synthetic_sessions() -> list[dict]:
  """(id, name, group, status, parent, profile, starred) rows, newest first.

  Every row carries the schema_version 2 profile the server requires: the
  independent roots are managers, and the root's descendants keep their
  manager and worker profiles.
  """
  root, child, worker = _sid(1), _sid(2), _sid(3)
  rows = [
      (ACTIVE_ID, "Release notes", "alpha", "active", None, "manager", False),
      (root, "Rework auth", "alpha", "active", None, "manager", False),
      (child, "Token store", "alpha", "active", root, "manager", False),
      (worker, "Implement the parser", "alpha", "active", root, "worker", False),
      (_sid(4), "Benchmark sweep", "beta", "active", None, "manager", True),
      (_sid(5), "Loader cleanup", "beta", "active", None, "manager", False),
      (_sid(6), "Onboarding doc", "gamma", "active", None, "manager", False),
      (_sid(7), "Scratchpad", None, "active", None, "manager", False),
      (_sid(8), "Weekly review", None, "active", None, "manager", False),
      (_sid(9), "Implement the cache", "alpha", "archived", root, "worker", False),
      (_sid(10), "Implement the retry", "alpha", "archived", root, "worker", False),
  ]
  out = []
  for index, (sid, name, group, status, parent, profile, starred) in enumerate(rows):
    # Fixed times months back, one hour apart: the list order and every
    # rendered time label come out the same on both sides.
    stamp = f"2026-01-15T{20 - index:02d}:00:00Z"
    meta = {
        "id": sid,
        "name": name,
        "status": status,
        "backend": "fake-scripted",
        "group": group,
        "starred": starred,
        "created_at": stamp,
        "updated_at": stamp,
        "schema_version": 2,
        "profile": profile,
        "task_parent_id": parent,
    }
    out.append(meta)
  return out


def write_home(home: pathlib.Path, port: int, access_key: str, sessions: list[dict]) -> None:
  home.mkdir(parents=True)
  config = {
      "server": {
          "port": port,
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
                      }
                  ],
              "pool": ["fake-scripted"]
          },
      "paths": {
          "worktree_dir": str(home / "worktrees")
      },
  }
  (home / "config.yaml").write_text(json.dumps(config, indent=2), encoding="utf-8")
  browser_harness_session_tree.write_credentials_yaml(home, access_key)
  for meta in sessions:
    session_dir = home / "sessions" / meta["id"]
    session_dir.mkdir(parents=True)
    (session_dir / "metadata.json").write_text(json.dumps(meta, indent=2), encoding="utf-8")


# ---------------------------------------------------------------------------
# serve mode: one side's own server.app, run from that side's checkout.
# ---------------------------------------------------------------------------


def serve(repo: pathlib.Path, port: int) -> None:
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

  def __init__(self, name: str, repo: pathlib.Path, tmp: pathlib.Path, sessions: list[dict]) -> None:
    self.name = name
    self.repo = repo
    self.port = browser_harness_session_tree.pick_free_port()
    self.access_key = browser_harness_session_tree.mint_access_key(f"parity-{name}-")
    self.home = tmp / f"home-{name}"
    write_home(self.home, self.port, self.access_key, sessions)
    self.log_path = tmp / f"serve-{name}.log"
    # The Terminal tab starts a login shell in a tmux server that outlives the
    # web server. A socket directory of the side's own keeps that server off the
    # host's shared one, and stop() kills it with the side.
    self.tmux_dir = tmp / f"tmux-{name}"
    self.tmux_dir.mkdir()
    self.proc: subprocess.Popen | None = None

  @property
  def base(self) -> str:
    return f"http://127.0.0.1:{self.port}"

  def start(self) -> None:
    env = {
        k: v
        for k, v in os.environ.items()
        if k not in (constants.SESSION_ID_ENV_VAR, "CHARLIEBOT_ACCESS_KEY", "VIRTUAL_ENV")
    }
    env.update({"CHARLIEBOT_HOME": str(self.home), "PYTHONPATH": str(self.repo), "TMUX_TMPDIR": str(self.tmux_dir)})
    log = self.log_path.open("w", encoding="utf-8")
    self.proc = subprocess.Popen(
        [
            sys.executable,
            str(pathlib.Path(__file__).resolve()), "serve", "--repo",
            str(self.repo), "--port",
            str(self.port)
        ],
        cwd=self.repo,
        env=env,
        stdout=subprocess.PIPE,
        stderr=log,
        text=True)
    line = self.proc.stdout.readline()
    if not line.startswith(READY_PREFIX):
      # The side's own error travels in the message: a copied log is evidence,
      # not the only place the failure shows.
      tail = self.log_path.read_text(encoding="utf-8", errors="replace")[-1500:]
      fail(f"{self.name} server did not start; log {self.log_path}: {line.strip()}\n{tail}")
    modules = json.loads(line[len(READY_PREFIX):])
    paths = [modules["server"], *modules["src"]]
    if not all(pathlib.Path(p).resolve().is_relative_to(self.repo.resolve()) for p in paths):
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
    browser_harness_session_tree.stop_child(self.proc, grace_s=10, kill_reap_s=10)
    for socket_path in self.tmux_dir.glob("tmux-*/*"):
      subprocess.run(["tmux", "-S", str(socket_path), "kill-server"], check=True)


def provision_main_extension(main_tree: pathlib.Path) -> None:
  """Give the detached main worktree the compiled _vfkspawn extension.

  The worktree checkout has no build artifacts and the check builds nothing,
  so the main side borrows the branch tree's built extension — sound only
  while the C source is identical on both sides.
  """
  main_c = (main_tree / VFKSPAWN_C).read_text(encoding="utf-8")
  branch_c = (SCRIPT_REPO / VFKSPAWN_C).read_text(encoding="utf-8")
  if main_c != branch_c:
    diff = list(difflib.unified_diff(branch_c.splitlines(), main_c.splitlines(), "branch", "main", lineterm="",
                                     n=1))[:30]
    fail(
        f"{VFKSPAWN_C} differs between the branch and the main checkout, so the main side cannot "
        "borrow the branch's built extension; pass a --main-ref whose _vfkspawn.c matches the "
        "branch, or build the extension in a main checkout (uv sync):\n" + "\n".join(diff))
  built = sorted(SCRIPT_REPO.glob(str(VFKSPAWN_C.parent / "_vfkspawn*.so")))
  if not built:
    fail(f"no built _vfkspawn extension under {SCRIPT_REPO / VFKSPAWN_C.parent}; run `uv sync` in the branch checkout")
  for so in built:
    shutil.copy(so, main_tree / VFKSPAWN_C.parent / so.name)


# ---------------------------------------------------------------------------
# Browser: CDP helpers and the outline export.
# ---------------------------------------------------------------------------

# Walks document.body and returns [depth, tag, id, classes, text, visible]
# rows for the rendered page: each whitelisted node drops with its subtree,
# and so does every invisible node — an inactive tab's panel or a closed
# modal belongs to the views that show it. script/style/template/link/meta
# carry code, not layout, so they drop too: an inline script's text would
# diff the two checkouts' source rather than the page. Rule hits are counted
# either way.
OUTLINE_JS = r"""
(function (parentOf) {
  const present = new Set([...document.querySelectorAll('#session-list [id^="session-"]')]
    .map((el) => el.id.slice('session-'.length)));
  const hits = {};
  function rule(el) {
    const tag = el.tagName.toLowerCase();
    if (['script', 'style', 'template', 'link', 'meta'].includes(tag)) return 'non-layout-tag';
    const id = el.id || '';
    if (id.startsWith('session-')) {
      const parent = parentOf[id.slice('session-'.length)];
      if (parent && present.has(parent)) return 'nested-row';
    }
    if (el.classList.contains('tree-subtree') || el.classList.contains('tree-children')) return 'tree-container';
    if (el.hasAttribute('data-tree-toggle')) return 'tree-toggle';
    const onclick = el.getAttribute('onclick') || '';
    if (onclick.includes('createChildSession')) return 'new-child-action';
    if (onclick.includes('openTaskContextModal(')) return 'task-context-action';
    if (tag === 'svg' && (el.getAttribute('title') || '').startsWith('Worker (')) return 'worker-icon';
    // .xterm-rows: the terminal's live screen. Its row count and text follow a
    // real shell and the tmux status clock, so two loads of one tree differ.
    if (el.classList.contains('xterm-rows')) return 'xterm-screen';
    // body > div at top -50000px: where xterm's DOM renderer measures glyph
    // widths. Its spans hold the last characters measured, which follow the
    // order a real shell's output paints in, so two loads of one tree differ.
    if (el.parentElement === document.body && el.style.top === '-50000px') return 'xterm-width-cache';
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
    // Only the rendered page compares: an invisible subtree (another tab's
    // panel, a closed modal) belongs to the views that show it.
    if (!visible(el)) { hits['not-rendered'] = (hits['not-rendered'] || 0) + 1; return; }
    const classes = [...el.classList].sort().join('.');
    rows.push([depth, el.tagName.toLowerCase(), el.id || '', classes, ownText(el), true]);
    for (const child of el.children) walk(child, depth + 1);
  }
  walk(document.body, 0);
  return {rows, hits};
})
"""


async def evaluate(cdp: browser_harness_session_tree.CDP, session_id: str, expression: str):
  result = await cdp.send(
      "Runtime.evaluate", {
          "expression": expression,
          "returnByValue": True,
          "awaitPromise": True
      },
      session_id=session_id)
  if "exceptionDetails" in result:
    raise RuntimeError(str(result["exceptionDetails"])[:500])
  return result.get("result", {}).get("value")


async def wait_for(
    cdp: browser_harness_session_tree.CDP, session_id: str, expression: str, what: str, timeout: float = 20) -> None:
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
    for _, pattern, replacement in PLACEHOLDER_RULES:
      text = pattern.sub(replacement, text)
    lines.append("  " * depth + head + (f" {json.dumps(text)}" if text else "") + ("" if shown else " [hidden]"))
  return lines


def collect_errors(events: list[dict], mark: int, session_id: str, side: Side) -> list[str]:
  """Normalized script-error messages the page raised since *mark* (a length
  of the CDP event log taken before the view's interaction)."""
  messages = set()
  for event in events[mark:]:
    if event.get("sessionId") != session_id:
      continue
    method = event.get("method", "")
    params = event.get("params", {})
    text = None
    if method == "Runtime.exceptionThrown":
      details = params.get("exceptionDetails", {})
      exception = details.get("exception", {})
      text = f"{details.get('text', '')} {exception.get('description') or exception.get('value') or ''}"
    elif method == "Runtime.consoleAPICalled" and params.get("type") == "error":
      # An argument's objectId is a per-page handle; its value or description is the message.
      text = " ".join(str(arg["value"] if "value" in arg else arg.get("description", "")) for arg in params["args"])
    elif method == "Log.entryAdded" and params.get("entry", {}).get("level") == "error":
      entry = params["entry"]
      text = f"{entry.get('text', '')} {entry.get('url', '')}"
    if text is None:
      continue
    text = text.replace(side.base, "<side>").replace(str(side.port), "<port>")
    text = LINE_COL.sub(":<line>", text)
    text = CACHE_BUSTER.sub(r"\1=<stamp>", text)
    messages.add(re.sub(r"\s+", " ", text).strip()[:400])
  return sorted(messages)


async def snap(cdp: browser_harness_session_tree.CDP, session_id: str, side: Side, mark: int, parent_of: dict) -> dict:
  """One view's outline, whitelist hits and script errors since *mark*."""
  capture = await evaluate(cdp, session_id, OUTLINE_JS + f"({json.dumps(parent_of)})")
  capture["errors"] = collect_errors(cdp.events, mark, session_id, side)
  return capture


async def capture_width(
    cdp: browser_harness_session_tree.CDP, session_id: str, side: Side, width: int, height: int, mobile: bool,
    parent_of: dict, tab_buttons: list[str] | None) -> tuple[dict, list[str]]:
  """The view sequence for one side at one width; returns the views and the
  tab buttons in the side's own DOM (the caller keeps the main side's list)."""
  await cdp.send(
      "Emulation.setDeviceMetricsOverride", {
          "width": width,
          "height": height,
          "deviceScaleFactor": 1,
          "mobile": mobile
      },
      session_id=session_id)
  await cdp.send(
      "Network.setCookie", {
          "name": "charliebot_access_key",
          "value": side.access_key,
          "url": side.base
      },
      session_id=session_id)
  views: dict = {}
  mark = len(cdp.events)
  await cdp.send("Page.navigate", {"url": f"{side.base}/?session={ACTIVE_ID}"}, session_id=session_id)
  await wait_for(cdp, session_id, f"!!document.getElementById('session-{ACTIVE_ID}')", f"the {side.name} session list")
  if mobile:
    # The phone layout keeps the sidebar in a closed drawer; open it so the
    # rows are compared as rendered, not as hidden nodes.
    await evaluate(cdp, session_id, "toggleMobileSidebar()")
    await asyncio.sleep(0.5)
  # Let the first status poll and any deferred paint settle before reading.
  await asyncio.sleep(2.0)
  views[f"{width}px/all"] = await snap(cdp, session_id, side, mark, parent_of)
  buttons = await evaluate(
      cdp, session_id, "[...document.querySelectorAll('#tab-buttons button')].map((button) => button.id)")

  mark = len(cdp.events)
  await evaluate(cdp, session_id, "switchSidebarFilter('archived')")
  await wait_for(
      cdp, session_id, "!!document.querySelector('#session-list [id^=\"session-\"]:not(#session-" + ACTIVE_ID +
      ")') && !document.getElementById('session-" + ACTIVE_ID + "')", f"the {side.name} archived list")
  await asyncio.sleep(1.0)
  views[f"{width}px/archived"] = await snap(cdp, session_id, side, mark, parent_of)

  await evaluate(cdp, session_id, "switchSidebarFilter('all')")
  await wait_for(cdp, session_id, f"!!document.getElementById('session-{ACTIVE_ID}')", f"the {side.name} all list")
  for button_id in (tab_buttons if tab_buttons is not None else buttons):
    view = f"{width}px/tab/{button_id}"
    mark = len(cdp.events)
    clicked = await evaluate(
        cdp, session_id, f"(function () {{ const button = document.getElementById({json.dumps(button_id)});"
        " if (!button) return false; button.click(); return true; })()")
    # Let the tab's own refresh (plans, terminal) fetch and paint.
    await asyncio.sleep(1.5)
    if clicked:
      views[view] = await snap(cdp, session_id, side, mark, parent_of)
    else:
      views[view] = {
          "rows": [[0, "missing", button_id, "", "", False]],
          "hits": {},
          "errors": [f"tab button #{button_id} is not in the DOM"],
      }
  return views, buttons


async def run_browser(chrome: str, sides: list[Side], parent_of: dict) -> tuple[dict, list[str]]:
  import websockets

  profile = pathlib.Path(tempfile.mkdtemp(prefix="ui-parity-chrome-"))
  debug_port = browser_harness_session_tree.pick_free_port()
  # --no-sandbox: host3's kernel denies the sandbox's user namespaces (the
  # headless_render harness launches the same way); the pages rendered here
  # are the check's own synthetic ones.
  proc = browser_harness_session_tree.launch_chrome(
      chrome, profile, debug_port, [
          "--no-sandbox", "--no-first-run", "--no-default-browser-check", "--disable-background-networking",
          "--remote-allow-origins=*"
      ])
  try:
    ws_url = await browser_harness_session_tree.devtools_ws_url(proc, 20, fail)
    async with websockets.connect(ws_url, max_size=50 * 1024 * 1024) as ws:
      cdp = browser_harness_session_tree.CDP(ws)
      captures: dict = {}
      # The tab buttons come from the main side's DOM in page order; the same
      # click sequence then runs on both sides at every width.
      tab_buttons: list[str] | None = None
      for width, height, mobile in WIDTHS:
        for side in sides:
          session_id, target_id = await browser_harness_session_tree.open_cdp_page(
              cdp, ("Page", "Network", "Runtime", "Log"))
          await cdp.send(
              "Page.addScriptToEvaluateOnNewDocument", {
                  "source":
                      (
                          f"try {{ localStorage.setItem('charliebot_access_key', "
                          f"{json.dumps(side.access_key)}); }} catch (e) {{}}")
              },
              session_id=session_id)
          views, buttons = await capture_width(cdp, session_id, side, width, height, mobile, parent_of, tab_buttons)
          if side.name == "main" and tab_buttons is None:
            tab_buttons = buttons
          for view, capture in views.items():
            captures.setdefault(view, {})[side.name] = capture
          await cdp.send("Target.closeTarget", {"targetId": target_id})
      return captures, tab_buttons or []
  finally:
    browser_harness_session_tree.stop_child(proc, grace_s=5, kill_reap_s=5)
    shutil.rmtree(profile, ignore_errors=True)


# ---------------------------------------------------------------------------
# Orchestration
# ---------------------------------------------------------------------------


def git(*args: str) -> str:
  return subprocess.run(["git", *args], cwd=SCRIPT_REPO, capture_output=True, text=True, check=True).stdout.strip()


def resolve_main_ref(ref: str | None) -> str:
  try:
    return git("rev-parse", "--verify", (ref or git("merge-base", "HEAD", "origin/main")) + "^{commit}")
  except subprocess.CalledProcessError as exc:
    fail(f"cannot resolve the main reference {ref or 'merge-base HEAD origin/main'}: {exc.stderr.strip()}")


def load_expected(path: pathlib.Path) -> set[tuple[str, str]]:
  """The --expected file: (view, outline line) entries, one per line."""
  entries = set()
  for lineno, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
    line = raw.rstrip()
    if not line or line.startswith("#"):
      continue
    view, sep, text = line.partition(" | ")
    if not sep or not view.strip() or not text:
      fail(f"{path}:{lineno}: an entry is '<view> | <outline line>': {raw!r}")
    entries.add((view.strip(), text))
  return entries


def check(args: argparse.Namespace) -> int:
  chrome = args.chrome or next((c for c in (shutil.which(name) for name in CHROME_CANDIDATES) if c), None)
  if not chrome:
    fail(f"none of {', '.join(CHROME_CANDIDATES)} is on PATH; pass --chrome")
  main_sha = resolve_main_ref(args.main_ref)
  branch_sha = git("rev-parse", "HEAD")
  expected = load_expected(pathlib.Path(args.expected)) if args.expected else set()
  evidence = pathlib.Path(args.evidence_dir or tempfile.mkdtemp(prefix="ui-parity-evidence-"))
  evidence.mkdir(parents=True, exist_ok=True)
  sessions = synthetic_sessions()
  parent_of = {m["id"]: m["task_parent_id"] for m in sessions if m.get("task_parent_id")}
  print(f"main {main_sha[:10]} vs branch {branch_sha[:10]} (working tree of {SCRIPT_REPO})", flush=True)

  with tempfile.TemporaryDirectory(prefix="ui-parity-") as tmp_name:
    tmp = pathlib.Path(tmp_name)
    main_tree = tmp / "main-checkout"
    git("worktree", "add", "--detach", "--quiet", str(main_tree), main_sha)
    sides = [Side("main", main_tree, tmp, sessions), Side("branch", SCRIPT_REPO, tmp, sessions)]
    try:
      provision_main_extension(main_tree)
      for side in sides:
        side.start()
      captures, tab_buttons = asyncio.run(run_browser(chrome, sides, parent_of))
    finally:
      for side in sides:
        side.stop()
      for side in sides:
        # A side that failed to start never wrote its log; the failure message
        # already carries that side's own error.
        if side.log_path.exists():
          shutil.copy(side.log_path, evidence / side.log_path.name)
      git("worktree", "remove", "--force", str(main_tree))

  print(f"tab buttons from the main DOM in page order: {', '.join(tab_buttons)}", flush=True)
  problems: list[str] = []
  occurred: set[tuple[str, str]] = set()
  report = {"main": main_sha, "branch": branch_sha, "tab_buttons": tab_buttons, "views": {}}
  for view, by_side in captures.items():
    main_capture, branch_capture = by_side["main"], by_side["branch"]
    main_lines = outline_lines(main_capture["rows"])
    branch_lines = outline_lines(branch_capture["rows"])
    diff = list(difflib.unified_diff(main_lines, branch_lines, "main", "branch", lineterm="", n=2))
    diff_entries = {(view, line[1:]) for line in diff if line[:1] in ("+", "-") and not line.startswith(("+++", "---"))}
    uncovered = diff_entries - expected
    occurred |= diff_entries & expected
    new_errors = sorted(set(branch_capture["errors"]) - set(main_capture["errors"]))
    hits = {side: by_side[side]["hits"] for side in by_side}
    stem = view.replace("/", "-")
    (evidence / f"outline-{stem}-main.txt").write_text("\n".join(main_lines) + "\n", encoding="utf-8")
    (evidence / f"outline-{stem}-branch.txt").write_text("\n".join(branch_lines) + "\n", encoding="utf-8")
    report["views"][view] = {
        "hits": hits,
        "diff": diff,
        "main_nodes": len(main_lines),
        "branch_nodes": len(branch_lines),
        "errors": {
            "main": main_capture["errors"],
            "branch": branch_capture["errors"]
        },
        "new_errors": new_errors,
    }
    if not diff:
      state = "identical"
    elif uncovered:
      state = "differs"
    else:
      state = "expected differences only"
    print(
        f"{view}: {state} ({len(main_lines)} nodes compared; whitelist hits "
        f"main {hits['main']}, branch {hits['branch']}; script errors "
        f"main {len(main_capture['errors'])}, branch {len(branch_capture['errors'])})",
        flush=True)
    if uncovered:
      problems.append(f"{view}: {len(uncovered)} outline difference(s) outside the expected file")
      print("\n".join(diff), flush=True)
      print("differences outside the expected file, in --expected entry form:", flush=True)
      for _, text in sorted(uncovered):
        print(f"{view} | {text}", flush=True)
    if new_errors:
      problems.append(f"{view}: {len(new_errors)} script error(s) the main side does not raise")
      for message in new_errors:
        print(f"new script error: {message}", flush=True)
  stale = expected - occurred
  if stale:
    problems.append(f"{len(stale)} expected entrie(s) did not occur")
    print("expected entries that did not occur:", flush=True)
    for view, text in sorted(stale):
      print(f"{view} | {text}", flush=True)
  (evidence / "ui_parity_report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
  print(f"evidence: {evidence}", flush=True)
  if problems:
    print(f"UI PARITY CHECK FAILED: {'; '.join(problems)}", flush=True)
    return 1
  print("UI PARITY CHECK PASSED", flush=True)
  return 0


def main() -> None:
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  sub = parser.add_subparsers(dest="mode")
  serve_parser = sub.add_parser("serve", help="internal: serve one checkout (spawned by the check)")
  serve_parser.add_argument("--repo", type=pathlib.Path, required=True)
  serve_parser.add_argument("--port", type=int, required=True)
  parser.add_argument("--main-ref", default=None, help="main commit (default: merge-base HEAD origin/main)")
  parser.add_argument(
      "--chrome", default=None, help=f"Chrome binary (default: the first of {', '.join(CHROME_CANDIDATES)} on PATH)")
  parser.add_argument("--expected", default=None, help="file listing the intended outline differences")
  parser.add_argument("--evidence-dir", default=None, help="report and server logs (default: a new temp dir)")
  args = parser.parse_args()
  if args.mode == "serve":
    serve(args.repo, args.port)
    return
  raise SystemExit(check(args))


if __name__ == "__main__":
  main()
