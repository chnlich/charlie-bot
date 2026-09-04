#!/usr/bin/env python3
"""Trace-only scroll acceptance probe for the CharlieBot web UI.

Problem this solves: globally styling scrollbars with ``::-webkit-scrollbar``
rules makes Chrome move every scroller's scrolling onto the renderer main
thread, so every frame's scroll offset waits for main-thread idle and any
main-thread busyness (WebSocket events, usage-bar repaints, hover transitions)
turns directly into scroll jank. The repo therefore styles scrollers with the
standard ``scrollbar-width`` / ``scrollbar-color`` properties instead, which
keeps scrolling on the compositor thread. This probe is the acceptance
instrument for that contract: it drives wheel gestures over the session list
and the chat body of a live session page and asserts, from a Chrome trace
alone, that no scroll frame is accounted as ``SCROLL_MAIN_THREAD``.

The page under test runs untouched: the only injected code is the pre-load
access-key bootstrap (localStorage + cookie), ``performance.mark`` leg
markers, and an optional ``--inject-css`` style element added after the page
settles. No rAF loop and no PerformanceObserver — an injected per-frame
instrument would itself perturb the main thread it measures (its rAF loop
promotes compositor animations onto the main thread). All metrics are read
from the trace after the run.

Usage:
  python scripts/web_scroll_probe.py --session <id> [--url URL] [--out DIR] \\
      [--inject-css FILE] [--cpu-throttle N]
  python scripts/web_scroll_probe.py --report TRACE.json

Drive mode opens a headless Chrome (binary from config key
``headless_chrome_bin``) against the running server (default
``http://127.0.0.1:<server_port>``; the access key comes from config), runs
the legs below, and writes ``<out>/trace_<tag>.json`` (raw trace) and
``<out>/metrics_<tag>.json``; tag is the first 8 chars of the session id plus
the injected CSS file stem when present. --report mode re-reads an existing
trace, prints the same per-leg table, and re-applies the gate.

Legs: ``idle_baseline`` (quiet 2.5s window, no gesture — recorded as a
non-scroll leg), ``list_wheel_hover``, ``list_wheel_hover_repeat``,
``chat_wheel_control``, ``list_wheel_hover_cpu<N>x`` (the list gesture wrapped
in ``Emulation.setCPUThrottlingRate``). Each scroll leg hovers the scroller
midpoint and runs ``Input.synthesizeScrollGesture`` with yDistance -600 twice
then +600 twice at speed 3000 (the same input pipeline as a physical mouse
wheel), recording scrollTop after each burst as ``scroll_top_path``.

Metrics per leg, sliced from the trace by the leg's user_timing marks (the
renderer main thread is the ``CrRendererMain`` thread of the pid that owns the
marks):
  scrollable                     whether the leg's scroller overflows; false
                                 legs have null metrics and are excluded from
                                 the gate (a short page's chat body not
                                 scrolling is not a mechanism failure)
  span_ms                        leg duration between the performance.mark edges
  frames                         compositor frames (``PipelineReporter``
                                 begin events in the leg window)
  scroll_state                   frame counts per
                                 ``frame_reporter.scroll_state``;
                                 ``SCROLL_RASTER`` = compositor scrolled and
                                 re-rastered the frame, ``SCROLL_MAIN_THREAD``
                                 = the frame's scroll offset was computed on
                                 the main thread — the failure signature this
                                 probe exists to catch
  dropped_frames                 frames the compositor reports ``STATE_DROPPED``
  dropped_affecting_smoothness   dropped frames with ``affects_smoothness``
  main_busy_ms                   main-thread ``RunTask`` total in the leg window
  main_ms_per_frame              main_busy_ms / frames
  paint_count / style_recalc_count
                                 main-thread ``Paint`` / ``UpdateLayoutTree``
                                 events with a duration
  animation_recalc_count         ``StyleRecalcInvalidationTracking`` events
                                 with reason ``Animation``
  scroll_top_path                scrollTop after each gesture burst (proof the
                                 gesture actually scrolled)

Exit codes: 0 = every scrollable scroll leg has zero ``SCROLL_MAIN_THREAD``
frames; 1 = any such frame exists; 2 = precondition failure (chrome binary
not configured or missing, server unreachable, session page without a session
list, unreadable or non-probe trace in --report mode).
"""
from __future__ import annotations

import argparse
import asyncio
import json
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from collections import Counter
from pathlib import Path
from typing import NoReturn

_REPO_ROOT = Path(__file__).resolve().parents[1]
if str(_REPO_ROOT) not in sys.path:
  sys.path.insert(0, str(_REPO_ROOT))

import websockets  # noqa: E402

from src.core.config import CharlieBotConfig, get_config  # noqa: E402

LIST_SELECTOR = "#session-list"
CHAT_SELECTOR_CANDIDATES = ("#messages", "#chat-messages", "#message-list", "main .overflow-y-auto")
TRACE_CATEGORIES = [
    "devtools.timeline",
    "disabled-by-default-devtools.timeline",
    "disabled-by-default-devtools.timeline.frame",
    "disabled-by-default-devtools.timeline.invalidationTracking",
    "blink.user_timing",
    "cc",
    "blink",
    "blink.animations",
    "input",
    "latencyInfo",
    "benchmark",
]
IDLE_LEG = "idle_baseline"
IDLE_LEG_S = 2.5
PAGE_READY_TIMEOUT_S = 120.0
PAGE_SETTLE_S = 6.0
HOVER_SETTLE_S = 0.3
BURST_SETTLE_S = 0.25
GESTURE_BURSTS = (-600, -600, 600, 600)
GESTURE_SPEED = 3000
READY_EXPR = (
    "(() => { const l = document.getElementById('session-list');"
    " return JSON.stringify({list: !!l, rows: l ? l.querySelectorAll('a[id^=session-]').length : 0,"
    " readyState: document.readyState}); })()")


def fail(message: str) -> NoReturn:
  """Report a precondition failure and exit 2."""
  print(f"error: {message}", file=sys.stderr)
  raise SystemExit(2)


def http_json(url: str) -> dict:
  """GET a JSON document."""
  with urllib.request.urlopen(url, timeout=5) as resp:
    return json.load(resp)


def free_port() -> int:
  """Grab a free TCP port for the Chrome devtools endpoint."""
  with socket.socket() as s:
    s.bind(("127.0.0.1", 0))
    return s.getsockname()[1]


class CDP:
  """Minimal CDP client over one WebSocket: request/response plus event handlers."""

  def __init__(self, ws) -> None:
    self.ws = ws
    self.mid = 0
    self.pending: dict[int, asyncio.Future] = {}
    self.handlers: dict[str, object] = {}

  async def send(self, method: str, session_id: str | None = None, **params: object) -> dict:
    self.mid += 1
    msg: dict = {"id": self.mid, "method": method, "params": params}
    if session_id:
      msg["sessionId"] = session_id
    fut = asyncio.get_running_loop().create_future()
    self.pending[self.mid] = fut
    await self.ws.send(json.dumps(msg))
    return await fut

  async def recv_loop(self) -> None:
    async for raw in self.ws:
      m = json.loads(raw)
      if "id" in m and m["id"] in self.pending:
        fut = self.pending.pop(m["id"])
        if "error" in m:
          fut.set_exception(RuntimeError(f"CDP {m.get('error')}"))
        else:
          fut.set_result(m.get("result", {}))
      elif "method" in m:
        handler = self.handlers.get(m["method"])
        if handler:
          handler(m)


async def evaluate_json(cdp: CDP, sid: str, expression: str) -> object:
  """Evaluate an expression in the page and return its by-value result."""
  r = await cdp.send("Runtime.evaluate", sid, expression=expression, returnByValue=True)
  return r["result"].get("value")


async def rect(cdp: CDP, sid: str, selector: str) -> dict | None:
  """Bounding box plus scroll metrics for the first element matching selector."""
  value = await evaluate_json(
      cdp, sid, f"""(() => {{
      const el = document.querySelector({json.dumps(selector)}); if (!el) return null;
      const b = el.getBoundingClientRect();
      return {{x: b.x, y: b.y, w: b.width, h: b.height, sh: el.scrollHeight, ch: el.clientHeight, st: el.scrollTop}};
    }})()""")
  return value if isinstance(value, dict) else None


async def mark(cdp: CDP, sid: str, leg: str, edge: str) -> None:
  """Drop a performance.mark leg marker; blink.user_timing carries it into the trace."""
  await cdp.send("Runtime.evaluate", sid, expression=f"performance.mark('leg:{leg}:{edge}')", returnByValue=True)


async def scroll_leg(cdp: CDP, sid: str, name: str, selector: str, x: float, y: float) -> dict:
  """Hover the scroller midpoint, run the wheel gesture bursts, record the scrollTop path."""
  await mark(cdp, sid, name, "start")
  await cdp.send("Input.dispatchMouseEvent", sid, type="mouseMoved", x=x, y=y)
  await asyncio.sleep(HOVER_SETTLE_S)
  path = [(await rect(cdp, sid, selector))["st"]]
  for dist in GESTURE_BURSTS:
    await cdp.send(
        "Input.synthesizeScrollGesture",
        sid,
        x=x,
        y=y,
        xDistance=0,
        yDistance=dist,
        repeatCount=0,
        speed=GESTURE_SPEED,
        gestureSourceType="mouse")
    await asyncio.sleep(BURST_SETTLE_S)
    path.append((await rect(cdp, sid, selector))["st"])
  # A small second hover shift so a different row sits under the pointer.
  await cdp.send("Input.dispatchMouseEvent", sid, type="mouseMoved", x=x, y=y + 7)
  await asyncio.sleep(HOVER_SETTLE_S)
  path.append((await rect(cdp, sid, selector))["st"])
  await mark(cdp, sid, name, "end")
  return {"leg": name, "scrollable": True, "scroll_top_path": path}


async def wait_page_ready(cdp: CDP, sid: str, page_url: str) -> int:
  """Poll until the session page is complete with a non-empty #session-list; return row count."""
  deadline = time.monotonic() + PAGE_READY_TIMEOUT_S
  last = ""
  while time.monotonic() < deadline:
    await asyncio.sleep(0.5)
    raw = await evaluate_json(cdp, sid, READY_EXPR)
    if not isinstance(raw, str):
      continue  # mid-navigation: no execution context to evaluate in yet
    last = raw
    state = json.loads(raw)
    if state["readyState"] == "complete" and not state["list"]:
      fail(f"page at {page_url} has no #session-list — wrong page, auth wall, or broken template")
    if state["readyState"] == "complete" and state["rows"] > 0:
      return int(state["rows"])
  fail(f"session page at {page_url} did not become ready within {PAGE_READY_TIMEOUT_S:.0f}s (last state: {last})")


def _leg_marks(trace: list[dict]) -> dict[str, dict[str, float]]:
  """Collect leg:<name>:start/end user-timing marks, keyed by leg name."""
  marks: dict[str, dict[str, float]] = {}
  for e in trace:
    if not str(e.get("cat", "")).startswith("blink.user_timing"):
      continue
    name = e.get("name", "")
    if not name.startswith("leg:"):
      continue
    _, leg, edge = name.split(":")
    marks.setdefault(leg, {})[edge] = e["ts"]
  return marks


def _renderer_main_thread(trace: list[dict]) -> tuple[int, int]:
  """(pid, tid) of the CrRendererMain thread of the pid owning the blink.user_timing marks."""
  mark_pid = Counter(e["pid"] for e in trace if str(e.get("cat", "")).startswith("blink.user_timing"))
  if not mark_pid:
    fail("trace contains no blink.user_timing events; not a web_scroll_probe trace")
  pid = mark_pid.most_common(1)[0][0]
  threads = {(e["pid"], e["tid"]): e["args"]["name"] for e in trace if e.get("name") == "thread_name"}
  for t_pid, t_tid in threads:
    if t_pid == pid and threads[(t_pid, t_tid)] == "CrRendererMain":
      return t_pid, t_tid
  fail(f"trace has no CrRendererMain thread for pid {pid} that owns the leg marks")


def trace_leg_metrics(trace: list[dict]) -> dict[str, dict]:
  """Per-leg metric dicts keyed by leg name, computed from the trace alone."""
  marks = _leg_marks(trace)
  if not marks:
    fail("trace contains no 'leg:<name>:<start|end>' user_timing marks; not a web_scroll_probe trace")
  main_pid, main_tid = _renderer_main_thread(trace)
  main_events = [e for e in trace if e.get("pid") == main_pid and e.get("tid") == main_tid]
  reporters = [
      e for e in trace if e.get("name") == "PipelineReporter" and e.get("ph") == "b" and e.get("pid") == main_pid
  ]
  metrics: dict[str, dict] = {}
  for leg, m in marks.items():
    if "start" not in m or "end" not in m:
      print(f"warning: leg '{leg}' is missing a start/end mark; skipping", file=sys.stderr)
      continue
    a, b = m["start"], m["end"]
    in_window = [e for e in main_events if a <= e.get("ts", 0) <= b]
    busy_ms = sum(e["dur"] for e in in_window if e.get("name") == "RunTask" and e.get("dur")) / 1000
    pr = [e for e in reporters if a <= e.get("ts", 0) <= b]
    frames = len(pr)
    states = Counter(e["args"]["frame_reporter"]["state"] for e in pr)
    scroll_states = Counter(e["args"]["frame_reporter"]["scroll_state"] for e in pr)
    metrics[leg] = {
        "span_ms":
            round((b - a) / 1000, 1),
        "frames":
            frames,
        "scroll_state":
            dict(scroll_states),
        "dropped_frames":
            states.get("STATE_DROPPED", 0),
        "dropped_affecting_smoothness":
            sum(
                1 for e in pr if e["args"]["frame_reporter"]["state"] == "STATE_DROPPED" and
                e["args"]["frame_reporter"]["affects_smoothness"]),
        "main_busy_ms":
            round(busy_ms, 1),
        "main_ms_per_frame":
            round(busy_ms / max(frames, 1), 2),
        "paint_count":
            sum(1 for e in in_window if e.get("name") == "Paint" and e.get("dur")),
        "style_recalc_count":
            sum(1 for e in in_window if e.get("name") == "UpdateLayoutTree" and e.get("dur")),
        "animation_recalc_count":
            sum(
                1 for e in in_window if e.get("name") == "StyleRecalcInvalidationTracking" and
                e.get("args", {}).get("data", {}).get("reason") == "Animation"),
    }
  return metrics


def _null_metrics() -> dict:
  return {
      "span_ms": None,
      "frames": None,
      "scroll_state": None,
      "dropped_frames": None,
      "dropped_affecting_smoothness": None,
      "main_busy_ms": None,
      "main_ms_per_frame": None,
      "paint_count": None,
      "style_recalc_count": None,
      "animation_recalc_count": None,
  }


def make_row(leg: str, scrollable: bool, computed: dict | None, scroll_top_path: list[int] | None) -> dict:
  """Assemble one metrics row; non-scrollable legs carry null metrics and skip the gate."""
  row = {"leg": leg, "scrollable": scrollable, **_null_metrics(), "scroll_top_path": None}
  if scrollable:
    if computed is None:
      fail(f"leg '{leg}' is scrollable but its trace marks are missing")
    row.update(computed)
    row["scroll_top_path"] = scroll_top_path
  return row


def print_table(rows: list[dict]) -> None:
  """Print the per-leg metrics table (dash cells for non-scrollable legs)."""
  print(
      f"{'leg':28s} {'scr':>3s} {'span_ms':>8s} {'frames':>6s} {'drop':>5s} {'dropSm':>6s} "
      f"{'ms/fr':>6s} {'Paint':>5s} {'ULT':>5s} {'Anim':>6s}  scroll_state / scroll_top_path")
  for r in rows:
    if not r["scrollable"]:
      print(
          f"{r['leg']:28s}   no {'-':>8s} {'-':>6s} {'-':>5s} {'-':>6s} {'-':>6s} {'-':>5s} "
          f"{'-':>5s} {'-':>6s}  (no overflowing scroller under test)")
      continue
    print(
        f"{r['leg']:28s}  yes {r['span_ms']:8.1f} {r['frames']:6d} {r['dropped_frames']:5d} "
        f"{r['dropped_affecting_smoothness']:6d} {r['main_ms_per_frame']:6.2f} {r['paint_count']:5d} "
        f"{r['style_recalc_count']:5d} {r['animation_recalc_count']:6d}  "
        f"{r['scroll_state']}  {r['scroll_top_path']}")


def gate(rows: list[dict]) -> int:
  """0 = every scrollable scroll leg is compositor-scrolls; 1 = any SCROLL_MAIN_THREAD frame."""
  bad = [
      f"{r['leg']}={r['scroll_state']['SCROLL_MAIN_THREAD']}" for r in rows
      if r["scrollable"] and r["scroll_state"] and r["scroll_state"].get("SCROLL_MAIN_THREAD", 0) > 0
  ]
  if bad:
    print(f"GATE FAIL: SCROLL_MAIN_THREAD frames on scrollable legs: {', '.join(bad)}")
    return 1
  print("GATE PASS: no SCROLL_MAIN_THREAD frames on any scrollable scroll leg")
  return 0


def report_mode(args: argparse.Namespace) -> int:
  """Re-read an existing trace, print the per-leg table, re-apply the gate."""
  path = Path(args.report)
  if not path.is_file():
    fail(f"trace file not found: {path}")
  try:
    trace = json.loads(path.read_text(encoding="utf-8"))
  except (OSError, json.JSONDecodeError) as e:
    fail(f"cannot read trace {path}: {e}")
  computed = trace_leg_metrics(trace)
  rows = [
      make_row(leg, scrollable=not leg.startswith(IDLE_LEG), computed=computed.get(leg), scroll_top_path=None)
      for leg, _ in computed.items()
  ]
  print_table(rows)
  return gate(rows)


async def drive_mode(args: argparse.Namespace) -> int:
  """Drive the legs against the live server and gate the resulting trace."""
  cfg: CharlieBotConfig = get_config()
  chrome_bin = cfg.headless_chrome_bin
  if not chrome_bin:
    fail("headless_chrome_bin is not configured; set it in config.yaml to run the probe")
  if not Path(chrome_bin).exists():
    fail(f"headless_chrome_bin does not exist: {chrome_bin}")
  if args.inject_css and not Path(args.inject_css).is_file():
    fail(f"--inject-css file not found: {args.inject_css}")
  base_url = (args.url or f"http://127.0.0.1:{cfg.server_port}").rstrip("/")
  key = cfg.charliebot_access_key
  try:
    with urllib.request.urlopen(f"{base_url}/", timeout=5) as resp:
      resp.read(1)
  except urllib.error.HTTPError:
    pass  # any HTTP response, including 401, proves the server is reachable
  except OSError as e:
    fail(f"charliebot server unreachable at {base_url}: {e}")

  out_dir = Path(args.out)
  out_dir.mkdir(parents=True, exist_ok=True)
  tag = args.session[:8] + (f"_{Path(args.inject_css).stem}" if args.inject_css else "")
  trace_path = out_dir / f"trace_{tag}.json"
  metrics_path = out_dir / f"metrics_{tag}.json"

  debug_port = free_port()
  chunks: list[dict] = []
  page: dict = {}
  records: list[dict] = []
  with tempfile.TemporaryDirectory(prefix="charliebot-scroll-probe-") as profile:
    proc = subprocess.Popen(
        [
            chrome_bin, "--headless=new", "--no-sandbox", f"--remote-debugging-port={debug_port}",
            f"--user-data-dir={profile}", "--window-size=1600,900", "--force-device-scale-factor=1", "--no-first-run",
            "--disable-extensions", "about:blank"
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL)
    try:
      devtools = f"http://127.0.0.1:{debug_port}"
      ver = None
      deadline = time.monotonic() + 30
      while time.monotonic() < deadline:
        if proc.poll() is not None:
          fail(f"headless chrome exited early (code {proc.returncode}); binary: {chrome_bin}")
        try:
          ver = http_json(f"{devtools}/json/version")
          break
        except OSError:
          time.sleep(0.2)
      if ver is None:
        fail(f"headless chrome devtools endpoint did not come up within 30s: {devtools}")
      print("CHROME", ver.get("Browser"))

      async with websockets.connect(ver["webSocketDebuggerUrl"], max_size=None) as ws:
        cdp = CDP(ws)
        recv = asyncio.create_task(cdp.recv_loop())
        try:
          tid = (await cdp.send("Target.createTarget", url="about:blank"))["targetId"]
          sid = (await cdp.send("Target.attachToTarget", targetId=tid, flatten=True))["sessionId"]
          for dom in ("Page", "Runtime"):
            await cdp.send(f"{dom}.enable", sid)
          if key:
            # Pre-load bootstrap so the first navigation lands authenticated, like a signed-in browser.
            await cdp.send(
                "Page.addScriptToEvaluateOnNewDocument",
                sid,
                source=(
                    f"try{{localStorage.setItem('charliebot_access_key','{key}');}}catch(e){{}};"
                    f"document.cookie='charliebot_access_key={key}; path=/; SameSite=Strict';"))
          cdp.handlers["Tracing.dataCollected"] = lambda m: chunks.extend(m["params"].get("value", []))
          done = asyncio.get_running_loop().create_future()
          cdp.handlers["Tracing.tracingComplete"] = lambda m: (not done.done()) and done.set_result(True)
          await cdp.send("Tracing.start", sid, traceConfig={"includedCategories": TRACE_CATEGORIES})
          page_url = f"{base_url}/?session={args.session}"
          await cdp.send("Page.navigate", sid, url=page_url)
          await wait_page_ready(cdp, sid, page_url)
          await asyncio.sleep(PAGE_SETTLE_S)  # let chat history render and polls settle
          if args.inject_css:
            css = Path(args.inject_css).read_text(encoding="utf-8")
            await cdp.send(
                "Runtime.evaluate",
                sid,
                returnByValue=True,
                expression=(
                    "(() => { const st = document.createElement('style'); st.textContent = " + json.dumps(css) +
                    "; document.head.appendChild(st); return 'css injected'; })()"))
            await asyncio.sleep(1.0)
          page = json.loads(
              await evaluate_json(
                  cdp, sid, (
                      "JSON.stringify({url: location.href, view: innerWidth + 'x' + innerHeight,"
                      " dpr: devicePixelRatio,"
                      " rows: document.querySelectorAll('#session-list a[id^=session-]').length,"
                      " page_nodes: document.getElementsByTagName('*').length})")))
          page["chrome"] = ver.get("Browser")
          page["injected_css"] = Path(args.inject_css).name if args.inject_css else None
          print("PAGE", json.dumps(page))

          lst = await rect(cdp, sid, LIST_SELECTOR)
          print("LIST", json.dumps(lst))
          list_scrollable = lst["sh"] > lst["ch"]
          lx, ly = lst["x"] + lst["w"] / 2, lst["y"] + min(lst["h"] / 2, 200)
          chat_sel = None
          for sel in CHAT_SELECTOR_CANDIDATES:
            rr = await rect(cdp, sid, sel)
            if rr and rr["sh"] > rr["ch"] + 50:
              chat_sel = sel
              break
          print("CHAT scroller", chat_sel)

          await mark(cdp, sid, IDLE_LEG, "start")
          await asyncio.sleep(IDLE_LEG_S)
          await mark(cdp, sid, IDLE_LEG, "end")
          records.append({"leg": IDLE_LEG, "scrollable": False, "scroll_top_path": None})
          list_legs = [("list_wheel_hover", lx, ly), ("list_wheel_hover_repeat", lx, ly)]
          if list_scrollable:
            for name, gx, gy in list_legs:
              records.append(await scroll_leg(cdp, sid, name, LIST_SELECTOR, gx, gy))
          else:
            records.extend({"leg": name, "scrollable": False, "scroll_top_path": None} for name, _, _ in list_legs)
          if chat_sel:
            cr = await rect(cdp, sid, chat_sel)
            cx, cy = cr["x"] + cr["w"] / 2, cr["y"] + cr["h"] / 2
            records.append(await scroll_leg(cdp, sid, "chat_wheel_control", chat_sel, cx, cy))
          else:
            records.append({"leg": "chat_wheel_control", "scrollable": False, "scroll_top_path": None})
          cpu_leg = f"list_wheel_hover_cpu{args.cpu_throttle}x"
          if list_scrollable:
            await cdp.send("Emulation.setCPUThrottlingRate", sid, rate=args.cpu_throttle)
            records.append(await scroll_leg(cdp, sid, cpu_leg, LIST_SELECTOR, lx, ly))
            await cdp.send("Emulation.setCPUThrottlingRate", sid, rate=1)
          else:
            records.append({"leg": cpu_leg, "scrollable": False, "scroll_top_path": None})

          await cdp.send("Tracing.end", sid)
          await asyncio.wait_for(done, timeout=60)
        finally:
          recv.cancel()
    finally:
      # Fully reap chrome before the TemporaryDirectory cleanup reads the profile dir.
      proc.terminate()
      try:
        proc.wait(timeout=10)
      except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()

  with trace_path.open("w") as f:
    json.dump(chunks, f)
  computed = trace_leg_metrics(chunks)
  rows = [make_row(r["leg"], r["scrollable"], computed.get(r["leg"]), r["scroll_top_path"]) for r in records]
  with metrics_path.open("w") as f:
    json.dump({"page": page, "legs": rows}, f, indent=1)
  print(f"trace: {trace_path} ({len(chunks)} events)")
  print(f"metrics: {metrics_path} (rows: {page['rows']})")
  print_table(rows)
  return gate(rows)


def parse_args() -> argparse.Namespace:
  p = argparse.ArgumentParser(description="Trace-only scroll acceptance probe (see module docstring).")
  mode = p.add_mutually_exclusive_group(required=True)
  mode.add_argument("--session", help="session id to drive the probe against (drive mode)")
  mode.add_argument("--report", help="re-read an existing trace JSON and re-apply the gate (report mode)")
  p.add_argument("--url", help="server base URL (default http://127.0.0.1:<server_port> from config)")
  p.add_argument(
      "--out",
      default="/tmp/charliebot-scroll-probe/",
      help="output directory for trace and metrics (default %(default)s)")
  p.add_argument("--inject-css", help="CSS file appended as a <style> element after the page settles")
  p.add_argument(
      "--cpu-throttle", type=int, default=4, help="CPU throttling rate for the final list leg (default %(default)s)")
  return p.parse_args()


def main() -> int:
  args = parse_args()
  if args.report:
    return report_mode(args)
  return asyncio.run(drive_mode(args))


if __name__ == "__main__":
  sys.exit(main())
