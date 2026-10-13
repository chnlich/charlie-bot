"""Shared helpers for the repo's browser harnesses.

This module is a library, not an entry point: the scenario scripts beside it
(tools/ui_parity_check.py, tools/chat_scroll_regression.py,
tools/live_smoke_task_tree.py) import these names and own their scenarios,
evidence files and CLI parsing. The helpers:

- Credentials and evidence. ``mint_access_key`` plus ``write_credentials_yaml``
  seed a trial home's operator key (the ``charliebot.access_key`` secret shape
  src/infra/credentials.py reads); ``open_evidence_dir`` creates the run's
  evidence directory and returns the checkout's HEAD it documents.
- Process and port plumbing. ``pick_free_port`` binds one unused loopback
  port; ``stop_child`` stops a harness child (SIGTERM, grace wait, then
  SIGKILL and reap); ``log`` and ``fail`` are the shared print and failure
  exit.
- CDP mini-client. ``CDP`` speaks the Chrome DevTools Protocol over the
  browser websocket: request/response matching, event collection, console
  error capture, session-list fetch counts and mutation tracking.
- Chrome launch and page wire-up. ``resolve_chrome`` finds the binary,
  ``launch_chrome`` starts headless chrome with a CDP endpoint and a private
  profile (``DESKTOP_CAPTURE_FLAGS`` is the shared flag set),
  ``devtools_ws_url`` reads the endpoint off chrome's stderr, ``connect_cdp``
  wraps it in ``CDP``, and ``open_cdp_page`` creates one attached page target.
- Page evaluation. ``evaluate`` runs one JS expression on the page;
  ``wait_for`` polls one until truthy, and its timeout diagnostic dumps
  ``DIAGNOSTIC_SNAPSHOT``.

Isolation contract the callers keep: the app under test is the real shipped
application (``server.app``) served by uvicorn with ``lifespan="off"`` — no
crash recovery, scheduler, trigger scans or global cleanup — on one explicit
unused local port, with a fresh temporary CHARLIEBOT_HOME carrying the config
and a synthetic operator key; inherited production credentials are cleared
from the harness environment, and the production service is never started,
stopped or contacted.
"""

from __future__ import annotations

import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
  sys.path.insert(0, str(REPO_ROOT))

import asyncio  # noqa: E402
import json  # noqa: E402
import os  # noqa: E402
import shutil  # noqa: E402
import socket  # noqa: E402
import subprocess  # noqa: E402
import time  # noqa: E402
from collections.abc import Callable  # noqa: E402


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

  @property
  def events(self) -> list[dict]:
    """The raw protocol events received so far, for harnesses that filter by
    sessionId and method themselves. The live list: a length taken earlier is
    a mark, and events[mark:] is what arrived since."""
    return self._events

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
