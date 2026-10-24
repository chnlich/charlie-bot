#!/usr/bin/env python3
"""Real-browser regression for chat reading positions (scroll-up history reads).

Executable repo-owned recipe (not a prose runbook). It reproduces the reported
"scroll up, then the page jumps to the bottom" family on the code it runs
inside, and holds every confirmed reading-position defect of the scroll chain:
pagination past the old scroll range, the cross-page turn merge, the
pagination hint's removal, live appends and stream drafts during a read,
manual turn collapse and reopen, resizes, hide/show, the transcript repaint,
pagination failure retry, and the stale response after a session switch.
Isolation contract:

- App under test. The real shipped application (``server.app``) served by
  uvicorn with ``lifespan="off"`` from THIS checkout — the browser exercises
  exactly the static files and routes this worktree ships. One explicit
  unused local port; the production service is never started, stopped or
  contacted. The harness verifies the address answers before any browser work
  ("check the server address").
- Synthetic home. A fresh temporary CHARLIEBOT_HOME carries the config and a
  synthetic operator key; inherited production credentials are cleared from
  the harness environment. The key is written in the credentials loader's own
  shape and read back through the existing config entry
  (``configured_access_key``) — the same gate the server enforces. No host
  credential, private session, or personal path enters the repo: everything
  lives in the run's temp dir and the evidence dir.
- Synthetic data. Seeded chat events (user/assistant turns) through
  ``SessionEvents.save_chat_event`` / ``persist_and_broadcast`` — the same
  owner the APIs serve, in this process only. No chat message is ever sent to
  a model: a scripted backend that never launches is the only configured
  backend, and the live-message scenarios ride the server's own broadcast.
- Browser. One muted headless chrome with a private ``--user-data-dir``
  profile inside the harness temp dir; ``--chrome`` wins over the config's
  ``headless_chrome_bin``, then the google-chrome installs — an absent binary
  is an explicit failure. The process is logged with its PID and stopped
  before exit; the user's own browser is untouched. Wheel actions are real CDP
  scroll gestures; approach scrolls are plain position writes.
- Evidence. Per-scenario assertion rows — anchor message id, its
  viewport-relative Y right after the update and once rendering settles —
  plus screenshots and the exact tested commit land in --evidence-dir
  (default: a host temp directory, never in git).

Run:  uv run python tools/chat_scroll_regression.py \
        [--evidence-dir DIR] [--keep] [--chrome BIN]

Reproducing the pre-fix failure: run this script from a worktree at the base
commit (the script serves the checkout it lives in); every scenario that fails
there and passes here is the regression evidence.
"""

from __future__ import annotations

import pathlib
import sys

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
  sys.path.insert(0, str(REPO_ROOT))

import argparse  # noqa: E402
import asyncio  # noqa: E402
import base64  # noqa: E402
import json  # noqa: E402
import os  # noqa: E402
import shutil  # noqa: E402
import tempfile  # noqa: E402
import time  # noqa: E402
import urllib.request  # noqa: E402

from tools import browser_harness_session_tree  # noqa: E402

ANCHOR_TOLERANCE_PX = 2.0
SETTLE_S = 1.2
RESULTS_NAME = "chat_scroll_regression_results.json"

# --- page-side helpers ------------------------------------------------------
# One snapshot of the whole reading state: scroll geometry, the engine's pin
# intent and window, the jump button, the pagination hint, the active session.
SNAP_JS = """(() => {
  const c = document.getElementById('messages');
  if (!c) return null;
  const e = globalThis.Chat && Chat.TurnEngine ? Chat.activeTurnEngineFor(c) : null;
  const btn = document.getElementById('scroll-to-bottom');
  const sent = document.getElementById('load-more-sentinel');
  return {
    top: c.scrollTop, height: c.scrollHeight, viewport: c.clientHeight,
    bottom: c.scrollHeight - c.clientHeight - c.scrollTop,
    pinned: e ? e.pinnedIntent : null,
    window: e && e.window ? {start: e.window.start, end: e.window.end} : null,
    jumpBtn: btn ? !btn.classList.contains('hidden') : null,
    sentinel: sent ? sent.getAttribute('data-state') : null,
    sessionId: (typeof SESSION_ID !== 'undefined') ? SESSION_ID : null,
  };
})()"""

# Viewport-relative Y of one message plus the gap to its previous sibling —
# the spacing a restore must conserve.
ANCHOR_Y_JS = """((id) => {
  const c = document.getElementById('messages');
  const el = c.querySelector('[data-message-id="' + id + '"]');
  if (!el) return null;
  const ct = c.getBoundingClientRect().top;
  const prev = el.previousElementSibling;
  return {
    y: el.getBoundingClientRect().top - ct,
    gap: prev ? (el.getBoundingClientRect().top - prev.getBoundingClientRect().top
                 - prev.getBoundingClientRect().height) : null,
  };
})"""

# The message the reader is looking at: the first node at or just below the
# container's visible top.
TOP_VISIBLE_JS = """(() => {
  const c = document.getElementById('messages');
  const ct = c.getBoundingClientRect().top;
  for (const el of c.querySelectorAll('[data-message-id]')) {
    const y = el.getBoundingClientRect().top - ct;
    if (y >= -4) return {id: el.getAttribute('data-message-id'), y};
  }
  const nodes = c.querySelectorAll('[data-message-id]');
  const last = nodes[nodes.length - 1];
  return last ? {id: last.getAttribute('data-message-id'), y: null} : null;
})()"""

# The turn wrap holding one message (a manual collapse may fold the message
# away; the wrap row is the identity that survives the fold).
WRAP_OF_JS = """((id) => {
  const c = document.getElementById('messages');
  const el = c.querySelector('[data-message-id="' + id + '"]');
  const wrap = el ? el.closest('[data-turn-key]') : null;
  return wrap ? {key: wrap.getAttribute('data-turn-key'),
                 y: wrap.getBoundingClientRect().top - c.getBoundingClientRect().top} : null;
})"""

# The engine's own reading anchor: the identity the engine held the place
# with, and the one a preserved place must keep across display-mode switches.
ENGINE_ANCHOR_JS = """(() => {
  const e = Chat.TurnEngine && Chat.activeTurnEngineFor(document.getElementById('messages'));
  return e && e.readingAnchor ? {kind: e.readingAnchor.kind,
      key: e.readingAnchor.key || null, id: e.readingAnchor.id || null,
      offset: e.readingAnchor.offset} : null;
})()"""

# The turn wrap / fold row of one turn key: the identity that survives the
# turn's fold, reached through data-turn-key. ``inView`` means it intersects
# the viewport — a row holding a negative offset legitimately pokes above it.
ROW_OF_JS = """((key) => {
  const c = document.getElementById('messages');
  const row = key ? c.querySelector('[data-turn-key="' + key + '"]') : null;
  if (!row) return null;
  const top = row.getBoundingClientRect().top - c.getBoundingClientRect().top;
  return {key, y: top, open: row.dataset.turnOpen,
          inView: top <= c.clientHeight
              && row.getBoundingClientRect().bottom >= c.getBoundingClientRect().top};
})"""

# The engine's record of the last restore the browser overran: the requested
# write and the position the scroll range actually allowed. Read per step and
# cleared before the next, so every row carries its own clamp evidence.
READ_CLAMP_JS = """(() => {
  const e = Chat.TurnEngine && Chat.activeTurnEngineFor(document.getElementById('messages'));
  return e && e.lastRestoreClamp ? Object.assign({}, e.lastRestoreClamp) : null;
})()"""

CLEAR_CLAMP_JS = """(() => {
  const e = Chat.TurnEngine && Chat.activeTurnEngineFor(document.getElementById('messages'));
  if (e) e.lastRestoreClamp = null;
  return true;
})()"""

# The reference row for one wheel notch: the message or turn row nearest the
# viewport center, remembered as a selector the notch's before/after reads
# share.
PICK_CENTER_ROW_JS = """(() => {
  const c = document.getElementById('messages');
  const cr = c.getBoundingClientRect();
  const mid = cr.top + c.clientHeight / 2;
  let best = null;
  for (const el of c.querySelectorAll('[data-message-id], [data-turn-key]')) {
    const r = el.getBoundingClientRect();
    if (r.height < 4 || r.bottom < cr.top || r.top > cr.bottom) continue;
    const d = Math.abs((r.top + Math.min(r.height, 40) / 2) - mid);
    if (!best || d < best.d) best = {el, d};
  }
  if (!best) return null;
  const el = best.el;
  const sel = el.hasAttribute('data-message-id')
    ? '[data-message-id="' + el.getAttribute('data-message-id') + '"]'
    : '[data-turn-key="' + el.getAttribute('data-turn-key') + '"]';
  window.__wheelRef = sel;
  return sel;
})()"""

# The reference row's viewport-relative Y plus the scroll position: one
# notch's before/after pair.
WHEEL_REF_STATE_JS = """(() => {
  const c = document.getElementById('messages');
  const el = window.__wheelRef ? c.querySelector(window.__wheelRef) : null;
  return {y: el ? el.getBoundingClientRect().top - c.getBoundingClientRect().top : null,
          top: c.scrollTop};
})()"""

# Two animation frames: the settle the jump-to-bottom scenario allows.
TWO_FRAMES_JS = "new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)))"

# Per-notch settle: pump animation frames for ``ms`` milliseconds. Pending
# rAF work keeps headless frame production alive, so a mouseWheel event's
# scroll is processed within a frame; without it the event pends and
# coalesces with the next notch into one doubled scroll.
NOTCH_PUMP_JS = """((ms) => new Promise((resolve) => {
  const t0 = performance.now();
  const step = () => {
    if (performance.now() - t0 < ms) requestAnimationFrame(step);
    else resolve(true);
  };
  requestAnimationFrame(step);
}))"""

# The last turn's key — the row a jump to the bottom must show.
LAST_TURN_KEY_JS = """(() => {
  const e = Chat.TurnEngine && Chat.activeTurnEngineFor(document.getElementById('messages'));
  if (!e) return null;
  const turns = e.segments.filter(s => s.kind === 'turn');
  return turns.length ? turns[turns.length - 1].key : null;
})()"""

# The bottom spacer's style height: 0 when the window reaches the last segment.
BOTTOM_SPACER_JS = """(() => {
  const e = Chat.TurnEngine && Chat.activeTurnEngineFor(document.getElementById('messages'));
  return e && e.bottomSpacer ? parseFloat(e.bottomSpacer.style.height) : null;
})()"""

# Engine-level instrumentation: every reproject's reason/pin/position and every
# scrollTop write, so each scenario's evidence carries the trace that explains
# its outcome.
INSTALL_INSTRUMENT_JS = """(() => {
  window.__projLog = [];
  window.__writeLog = [];
  window.__restoreLog = [];
  const wire = () => {
    const c = document.getElementById('messages');
    const e = globalThis.Chat && Chat.TurnEngine ? Chat.activeTurnEngineFor(c) : null;
    if (!e || e.__instrumented) return false;
    e.__instrumented = true;
    const orig = e.reproject.bind(e);
    e.reproject = (reason, ...args) => {
      const before = {top: c.scrollTop, h: c.scrollHeight, vp: c.clientHeight};
      const ret = orig(reason, ...args);
      window.__projLog.push({
        reason, before,
        after: {top: c.scrollTop, h: c.scrollHeight},
        pinned: e.pinnedIntent,
        pending: e.pendingAnchor ? (e.pendingAnchor.kind + ':' + (e.pendingAnchor.id || e.pendingAnchor.key)) : null,
        reading: e.readingAnchor ? (e.readingAnchor.kind + ':' + (e.readingAnchor.id || e.readingAnchor.key)) : null,
        window: {start: e.window.start, end: e.window.end},
      });
      if (window.__projLog.length > 400) window.__projLog.shift();
      return ret;
    };
    if (typeof e.restoreReadingAnchor !== 'function') return true;
    const origRestore = e.restoreReadingAnchor.bind(e);
    e.restoreReadingAnchor = (target) => {
      const c = document.getElementById('messages');
      const before = c.scrollTop;
      const ok = origRestore(target);
      window.__restoreLog.push({t: Math.round(performance.now()), kind: target.kind,
        id: String(target.id || target.key).slice(0, 18), offset: Math.round(target.offset * 10) / 10,
        topBefore: before, topAfter: c.scrollTop, applied: ok});
      return ok;
    };
    const d = Object.getOwnPropertyDescriptor(Element.prototype, 'scrollTop');
    Object.defineProperty(c, 'scrollTop', {
      get() { return d.get.call(c); },
      set(v) {
        const from = d.get.call(c);
        d.set.call(c, v);
        window.__writeLog.push({from, to: d.get.call(c), h: c.scrollHeight,
                                t: Math.round(performance.now())});
        if (window.__writeLog.length > 400) window.__writeLog.shift();
      },
      configurable: true,
    });
    return true;
  };
  if (!wire()) {
    const obs = new MutationObserver(() => { if (wire()) obs.disconnect(); });
    obs.observe(document, {childList: true, subtree: true});
  }
  return true;
})()"""

# The page's fetch stub. Only this session's history pages and transcript
# reads are intercepted — every other request (bootstrap, WS handshake,
# static) rides the real server. The stub serves deterministic synthetic
# pages, can fail one request with a status, and can delay one response so a
# response outlives the session that asked for it.
INSTALL_HELPERS_JS = """(() => {
  window.__fetchLog = [];
  window.__stubPages = [];
  window.__failNextStatus = null;
  window.__delayNextMs = 0;
  window.__transcriptPayload = null;
  window.__transcriptReads = 0;
  const orig = window.fetch;
  window.fetch = async (url, opts) => {
    const u = String(url);
    const m = u.match(/\\/api\\/sessions\\/([^/]+)\\/events\\?/);
    if (m && m[1] === window.__sessionId) {
      window.__fetchLog.push(u);
      if (window.__failNextStatus) {
        const status = window.__failNextStatus;
        window.__failNextStatus = null;
        return new Response(JSON.stringify({error: 'stubbed failure'}), {status});
      }
      const delay = window.__delayNextMs;
      if (delay) { window.__delayNextMs = 0; await new Promise(r => setTimeout(r, delay)); }
      const page = window.__stubPages.shift();
      if (page) {
        return new Response(JSON.stringify(page), {status: 200,
            headers: {'Content-Type': 'application/json'}});
      }
    }
    if (window.__transcriptPayload && u.includes('/transcript?')) {
      window.__transcriptReads += 1;
      return new Response(JSON.stringify(window.__transcriptPayload), {status: 200,
          headers: {'Content-Type': 'application/json'}});
    }
    return orig(url, opts);
  };
  return true;
})()"""


def stub_message(role: str, msg_id: str, text: str, index: int) -> dict:
  """One synthetic history message in the shape the renderer consumes."""
  return {
      "role": role,
      "id": msg_id,
      "content": text,
      "timestamp": "2026-10-04T10:00:00Z",
      "event_index": 10000 + index,
  }


def stub_turn(page: list[dict], prefix: str, i: int, closed: bool) -> None:
  """One round in the wire shape: question, answer, and — when the round
  finished — the separator message the aggregator projects from master_done.
  A page whose last round is left open reproduces the real boundary case: the
  round's separator only exists once the next round ends, so a page cut at a
  round line carries an unclosed span that the engine merges into the mounted
  head's leading turn.
  """
  page.append(stub_message("user", f"{prefix}_{i:02d}u", f"{prefix} question {i:02d}", len(page)))
  page.append(stub_message("assistant", f"{prefix}_{i:02d}a", f"{prefix} answer {i:02d}", len(page)))
  if closed:
    sep = stub_message("separator", f"{prefix}_{i:02d}s", "", len(page))
    sep["thinking_seconds"] = 2
    page.append(sep)


def page_payload(messages: list[dict], has_more: bool, next_before: int = -1000) -> dict:
  """One pagination page in the wire shape loadOlderMessages consumes."""
  return {"messages": messages, "has_more": has_more, "next_before": next_before}


def build_stub_pages() -> tuple[list[dict], list[dict]]:
  """The two synthetic older pages.

  Page one holds twenty closed rounds plus one trailing round left open —
  its span merges across the page line into the mounted head's leading turn.
  At ~40 one-line messages per mounted head and a ~700px viewport the page is
  also taller than the old scrollable range, which is the reported jump's
  mechanism: the compensation write exceeded the old maximum, the browser
  clamped it, and the clamp echo read back as a user scroll parked at the
  bottom.

  Page two closes the history (has_more false), which removes the pagination
  hint — the hint's height above the reader must be compensated too.
  """
  page1: list[dict] = []
  for i in range(24):
    stub_turn(page1, "pg1", i, closed=True)
  stub_turn(page1, "pg1", 24, closed=False)
  page2: list[dict] = []
  for i in range(8):
    stub_turn(page2, "pg2", i, closed=True)
  return page1, page2


async def ev(cdp: browser_harness_session_tree.CDP, session_id: str, expression: str):
  """One Runtime.evaluate with returnByValue; errors fail the run loudly."""
  return await browser_harness_session_tree.evaluate(cdp, session_id, expression)


async def snap(cdp: browser_harness_session_tree.CDP, session_id: str) -> dict:
  return await ev(cdp, session_id, SNAP_JS)


async def settle() -> None:
  """Give rAF, the engine's idle slices, and the WS round-trip time to land."""
  await asyncio.sleep(SETTLE_S)


async def wheel(cdp: browser_harness_session_tree.CDP, session_id: str, dy: int) -> None:
  """One real scroll gesture over the message container's center.

  ``dy`` is the distance the reader travels UP (positive scrolls up), matching
  the gesture's yDistance convention.
  """
  box = await ev(
      cdp, session_id, """(() => {
    const r = document.getElementById('messages').getBoundingClientRect();
    return {x: r.x + r.width / 2, y: r.y + r.height / 2};
  })()""")
  params = {**box, "yDistance": dy, "xDistance": 0, "speed": 1600, "gestureSourceType": "mouse"}
  await cdp.send("Input.synthesizeScrollGesture", params, session_id=session_id)
  await asyncio.sleep(0.35)


async def wheel_notch(cdp: browser_harness_session_tree.CDP, session_id: str, box: dict, delta_y: int = -100) -> None:
  """One real mouse-wheel notch: a CDP mouseWheel event with a 100px delta,
  the distance one notch travels on the reader's machine."""
  await cdp.send(
      "Input.dispatchMouseEvent", {
          "type": "mouseWheel",
          "x": box["x"],
          "y": box["y"],
          "deltaX": 0,
          "deltaY": delta_y,
      },
      session_id=session_id)


async def wheel_until_trigger(cdp: browser_harness_session_tree.CDP, session_id: str, max_gestures: int = 6) -> None:
  """Wheel up in real gestures until the pagination trigger fires.

  A synthesized gesture's realized travel varies between chromium builds, so
  the crossing is detected by the stub's fetch log rather than assumed from
  one gesture's distance.
  """
  for _ in range(max_gestures):
    await wheel(cdp, session_id, 600)
    fired = await ev(cdp, session_id, "window.__fetchLog.length >= 1")
    if fired:
      return
  browser_harness_session_tree.fail(
      "pagination trigger never fired within "
      f"{max_gestures} gestures (fetch log empty)")


async def wait_reader_settled(cdp: browser_harness_session_tree.CDP, session_id: str, timeout_s: float = 30.0) -> None:
  """Wait until the reader and the document are both at rest: no genuine scroll
  event for 400ms and the pre-render queue empty.

  A synthesized gesture's realized travel trickles in past the CDP call returning,
  and a big jump leaves idle slices materializing the new window, so a place
  pinned on a fixed sleep can describe a reader who is still moving or a height
  model that is still growing. The engine's own lastScrollAgeMs and queueLength
  are the truth; programmatic writes never refresh the scroll age.
  """
  probe = (
      "(() => {const c = document.getElementById('messages');"
      "const e = Chat.TurnEngine && Chat.activeTurnEngineFor(c);"
      "if (!e) return null;"
      "return {age: Math.round(performance.now() - e.lastScrollTs),"
      "        queue: Chat.TurnEngine.debug(c).queueLength};})()")
  deadline = time.monotonic() + timeout_s
  last = None
  while time.monotonic() < deadline:
    last = await ev(cdp, session_id, probe)
    # A null age is a never-scrolled engine (the age is infinite, which JSON
    # cannot carry): trivially at rest.
    if last and (last["age"] is None or last["age"] >= 400) and last["queue"] == 0:
      return
    await asyncio.sleep(0.25)
  browser_harness_session_tree.fail(f"the reader never settled within {timeout_s}s (last probe {last})")


async def scroll_write(cdp: browser_harness_session_tree.CDP, session_id: str, top: int) -> None:
  """A plain position write: the reader parks at an absolute scrollTop.

  The engine reads any non-echo write as user motion, so the approach scroll
  behaves like a drag; the pagination trigger stays above 80px, so the parked
  position never fetches on its own.
  """
  await ev(cdp, session_id, f"document.getElementById('messages').scrollTop = {top}")
  await asyncio.sleep(0.6)


async def anchor_y(cdp: browser_harness_session_tree.CDP, session_id: str, anchor_id: str) -> dict | None:
  return await ev(cdp, session_id, ANCHOR_Y_JS + f'("{anchor_id}")')


async def screenshot(
    cdp: browser_harness_session_tree.CDP, session_id: str, evidence_dir: pathlib.Path, name: str) -> str:
  res = await cdp.send("Page.captureScreenshot", {"format": "png"}, session_id=session_id)
  path = evidence_dir / f"{name}.png"
  path.write_bytes(base64.b64decode(res["data"]))
  return path.name


class Scenario:
  """One assertion row plus the evidence numbers behind it."""

  def __init__(self, name: str) -> None:
    self.name = name
    self.ok = True
    self.checks: list[dict] = []
    self.data: dict = {}

  def check(self, cond: bool, message: str) -> bool:
    self.checks.append({"check": message, "ok": bool(cond)})
    if not cond:
      self.ok = False
    return cond

  async def finish(
      self, cdp: browser_harness_session_tree.CDP, cs: str, results: list[dict], screenshot_name: str | None) -> None:
    """Record the row with the engine trace tails as its diagnosis."""
    try:
      self.data["proj_tail"] = (await ev(cdp, cs, "window.__projLog || []"))[-40:]
      self.data["write_tail"] = (await ev(cdp, cs, "window.__writeLog || []"))[-40:]
      self.data["restore_tail"] = (await ev(cdp, cs, "window.__restoreLog || []"))[-40:]
    except Exception as exc:  # the trace is evidence, never a failure source
      self.data["trace_error"] = repr(exc)
    detail = "; ".join(("PASS: " if c["ok"] else "FAIL: ") + c["check"] for c in self.checks)
    results.append(
        {
            "name": self.name,
            "ok": self.ok,
            "detail": detail,
            "data": self.data,
            "screenshot": screenshot_name,
        })
    browser_harness_session_tree.log(f"  [{'PASS' if self.ok else 'FAIL'}] {self.name}: {detail[:400]}")


def pos_holds(before: dict | None, after: dict | None, tol=ANCHOR_TOLERANCE_PX) -> bool:
  """The reader's place survived: same viewport offset, same spacing above."""
  return (
      before is not None and after is not None and abs(before["y"] - after["y"]) <= tol and
      (before["gap"] is None or after["gap"] is None or abs(before["gap"] - after["gap"]) <= tol))


def anchor_held(engine_anchor: dict | None, expected: dict | None) -> bool:
  """The engine kept the reading identity: same kind and same content id/key."""
  return (
      engine_anchor is not None and expected is not None and engine_anchor["kind"] == expected["kind"] and
      engine_anchor.get("id") == expected.get("id") and engine_anchor.get("key") == expected.get("key"))


async def pin_reading_place(cdp: browser_harness_session_tree.CDP, session_id: str) -> dict:
  """Pin the reader's place before a walk: the visible message with its offset
  and spacing, its turn wrap, and the engine's own anchor with that anchor's
  wrap. Every later assertion reads these identities, never whatever the engine
  re-selects after a switch."""
  top_visible = await ev(cdp, session_id, TOP_VISIBLE_JS)
  ay = await anchor_y(cdp, session_id, top_visible["id"]) if top_visible else None
  wrap = None
  if top_visible:
    wrap = await ev(cdp, session_id, WRAP_OF_JS + f'("{top_visible["id"]}")')
  engine_anchor = await ev(cdp, session_id, ENGINE_ANCHOR_JS)
  engine_wrap = None
  if engine_anchor and engine_anchor["kind"] == "message" and engine_anchor.get("id"):
    engine_wrap = await ev(cdp, session_id, WRAP_OF_JS + f'("{engine_anchor["id"]}")')
  elif engine_anchor and engine_anchor.get("key"):
    engine_wrap = await ev(cdp, session_id, ROW_OF_JS + f'("{engine_anchor["key"]}")')
  return {"msg": top_visible, "ay": ay, "wrap": wrap, "engine_anchor": engine_anchor, "engine_wrap": engine_wrap}


async def fold_step_state(
    cdp: browser_harness_session_tree.CDP, session_id: str, msg_id: str, wrap_key: str | None) -> dict:
  """One measurement point of a collapse/reopen step: the pinned message's
  place, the read turn's row, the engine's anchor and clamp record, and the
  scroll snapshot."""
  return {
      "msg": await anchor_y(cdp, session_id, msg_id),
      "row": await ev(cdp, session_id, ROW_OF_JS + f'("{wrap_key}")') if wrap_key else None,
      "engine_anchor": await ev(cdp, session_id, ENGINE_ANCHOR_JS),
      "clamp": await ev(cdp, session_id, READ_CLAMP_JS),
      "snap": await snap(cdp, session_id),
  }


# The reader opens the oldest turns of the mounted head by clicking their fold
# rows — the reader path to a history one can park inside. Every row sits in
# the DOM at mount (the folded head fits in the engine's window), so one pass
# clicks them all.
OPEN_OLDEST_TURNS_JS = """((n) => {
  const e = Chat.activeTurnEngineFor(document.getElementById('messages'));
  const keys = e.segments.filter(s => s.kind === 'turn').map(s => s.key);
  const rows = keys.slice(0, n)
      .map(k => document.querySelector('[data-turn-key="' + k + '"] .turn-row'))
      .filter(Boolean);
  rows.forEach(r => r.click());
  return {wanted: n, clicked: rows.length};
})"""


# --- seeding ----------------------------------------------------------------
async def seed_sessions() -> tuple[str, str, list[dict], object, object]:
  """Create the two regression sessions and their synthetic chat events.

  The main session carries 30 closed turns: the bootstrap's 40-message tail
  then starts mid-history with an assistant reply, which is what lets the
  first stubbed page's trailing user message merge across the page boundary.
  The events persist through the same owner the APIs serve; nothing reaches a
  model. Returns (main session id, second session id, bootstrap messages,
  session store block, session events block).
  """
  from src.infra import config
  from src.runtime import (
      run_token,
      session_anchors,
      session_events,
      session_fork,
      session_lifecycle,
      session_listing,
      session_sidebar,
      session_store,
      session_successor,
      task_sessions,
  )
  from src.runtime.api import message_utils

  cfg = config.get_config()
  store = session_store.SessionStore(cfg)
  sidebar = session_sidebar.SessionSidebar(cfg, store)
  events = session_events.SessionEvents(cfg, store)
  tree = task_sessions.TaskTreeManager(
      cfg, store, events, sidebar, session_listing.SessionListing(cfg, store, sidebar),
      session_lifecycle.SessionLifecycle(cfg, store, events), session_fork.SessionFork(cfg, store, events),
      session_anchors.SessionAnchors(cfg, store, events), session_successor.SessionSuccessor(cfg, store, events))
  operator = run_token.CallerIdentity(kind="operator")
  main = await tree.create_task(
      request_id="seed-main",
      task_parent_id=None,
      profile="manager",
      task=None,
      name="Reading-position regression",
      backend="fake-scripted",
      caller=operator)
  sid = main.id
  # One real round per turn: the question, the answer, and the round end —
  # the aggregator projects master_done into the separator message that closes
  # a turn, exactly as a completed chat session's history looks.
  for i in range(40):
    await events.save_chat_event(sid, {"type": "user", "content": f"history question {i:02d}"})
    await events.save_chat_event(
        sid, {
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": f"history answer {i:02d}"
                }]
            }
        })
    await events.save_chat_event(sid, {"type": "master_done"})
  other = await tree.create_task(
      request_id="seed-second",
      task_parent_id=None,
      profile="manager",
      task=None,
      name="Reading-position second",
      backend="fake-scripted",
      caller=operator)
  sid_b = other.id
  for i in range(3):
    await events.save_chat_event(sid_b, {"type": "user", "content": f"Session B opening question {i}"})
    await events.save_chat_event(
        sid_b, {
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": f"Session B answer {i}"
                }]
            }
        })
    await events.save_chat_event(sid_b, {"type": "master_done"})
  # The wheel-step scenarios' session: same completed-round shape as the main
  # one; the scenarios grow it past four viewports with stub pages.
  wheel_session = await tree.create_task(
      request_id="seed-wheel",
      task_parent_id=None,
      profile="manager",
      task=None,
      name="Wheel-step regression",
      backend="fake-scripted",
      caller=operator)
  sid_c = wheel_session.id
  for i in range(40):
    await events.save_chat_event(sid_c, {"type": "user", "content": f"wheel history question {i:02d}"})
    await events.save_chat_event(
        sid_c, {
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": f"wheel history answer {i:02d}"
                }]
            }
        })
    await events.save_chat_event(sid_c, {"type": "master_done"})
  bootstrap = await message_utils.build_session_bootstrap_data(sid, store, events)
  return sid, sid_b, sid_c, bootstrap.messages, store, events


async def broadcast_user(events, sid: str, text: str) -> None:
  """One real live message: persisted, then broadcast through the aggregator."""
  await events.persist_and_broadcast(sid, {"type": "user", "content": text})


async def broadcast_assistant_draft(events, sid: str, text: str) -> None:
  """One real stream delta: the assistant event buffers into the live draft."""
  await events.persist_and_broadcast(
      sid, {
          "type": "assistant",
          "message": {
              "content": [{
                  "type": "text",
                  "text": text
              }]
          }
      })


async def bootstrap_messages_now(sid: str, store, events) -> list[dict]:
  """The session's current committed messages, read in-process.

  The transcript-reset payload must carry what the view now holds (the seeded
  history plus the live messages the scenarios broadcast).
  """
  from src.runtime.api import message_utils
  bootstrap = await message_utils.build_session_bootstrap_data(sid, store, events)
  return bootstrap.messages


# --- scenarios --------------------------------------------------------------


async def run_scenarios(
    cdp: browser_harness_session_tree.CDP, page: str, sid: str, sid_b: str, sid_c: str, store, events,
    evidence_dir: pathlib.Path, results: list[dict]) -> None:
  """The ordered scenario flow; each scenario starts from a state it sets.

  ``page`` is the CDP page session the browser calls ride; ``sid``/``sid_b``/
  ``sid_c`` are the chat sessions the app serves. Scenario rows append to
  *results* as they finish, so an aborted run keeps the rows it already
  produced.
  """
  page1, page2 = build_stub_pages()
  cs = page  # every browser call below targets the one page session

  # -- setup: engine mounted in outline; the reader opens the turns their
  # later reads park in by clicking the fold rows --------------------------
  sc = Scenario("setup_engine_mounted")
  await browser_harness_session_tree.wait_for(
      cdp,
      cs, "Chat.TurnEngine && document.getElementById('messages') "
      "&& Chat.activeTurnEngineFor(document.getElementById('messages')) "
      "&& document.querySelector('#messages [data-message-id]')",
      label="turn engine mounted with rendered messages")
  head_first_id = await ev(
      cdp, cs, "document.querySelector('#messages [data-message-id]').getAttribute('data-message-id')")
  await ev(cdp, cs, INSTALL_HELPERS_JS)
  await ev(cdp, cs, INSTALL_INSTRUMENT_JS)
  await ev(cdp, cs, "window.__sessionId = " + json.dumps(sid))
  opened = await ev(cdp, cs, OPEN_OLDEST_TURNS_JS + "(5)")
  await settle()
  # The opens grew the history above a pinned reader; the jump button is the
  # reader's own gesture back to the present, and it re-arms the follow.
  await ev(cdp, cs, "document.getElementById('scroll-to-bottom').click()")
  await asyncio.sleep(0.4)
  state = await snap(cdp, cs)
  sc.check(opened["clicked"] == 5, f"the reader opened five turns from their rows ({opened})")
  sc.check(state["pinned"] is True, "engine mounted bottom-pinned")
  sc.check(state["sentinel"] == "idle", "pagination hint present (has_more)")
  sc.check(state["height"] > state["viewport"] * 2, "mounted head with the opened turns is scrollable")
  sc.data.update({"snap": state, "head_first_id": head_first_id, "opened": opened})
  await sc.finish(cdp, cs, results, await screenshot(cdp, cs, evidence_dir, "setup_outline"))

  # -- S1: one wheel notch up stops the follow ------------------------------
  sc = Scenario("wheel_up_one_notch_stops_follow")
  await ev(cdp, cs, "document.getElementById('scroll-to-bottom').click()")
  await asyncio.sleep(0.4)
  before = await snap(cdp, cs)
  await wheel(cdp, cs, 100)
  after = await snap(cdp, cs)
  sc.check(before["pinned"] is True, "started pinned at the bottom")
  sc.check(after["top"] < before["top"] - 60, f"the wheel moved the view up ({before['top']} -> {after['top']})")
  sc.check(after["pinned"] is False, "pin intent cleared by the upward scroll")
  sc.check(after["jumpBtn"] is True, "jump button shown")
  sc.data.update({"before": before, "after": after})
  await sc.finish(cdp, cs, results, None)

  # -- S2: a live message while reading must not yank the reader ------------
  sc = Scenario("live_message_while_reading_holds_position")
  anchor = await ev(cdp, cs, TOP_VISIBLE_JS)
  ay_before = await anchor_y(cdp, cs, anchor["id"])
  await broadcast_user(events, sid, "live message arriving during a read")
  await settle()
  after = await snap(cdp, cs)
  ay_after = await anchor_y(cdp, cs, anchor["id"])
  sc.check(pos_holds(ay_before, ay_after), f"anchor {anchor['id']} holds its viewport offset")
  sc.check(after["bottom"] > 0, "reader not yanked to the bottom")
  sc.check(after["jumpBtn"] is True, "jump button still shown")
  sc.data.update({"anchor": anchor, "ay_before": ay_before, "ay_after": ay_after, "after": after})
  await sc.finish(cdp, cs, results, None)

  # -- S3: a growing stream draft must not yank the reader ------------------
  sc = Scenario("stream_draft_while_reading_holds_position")
  ay_before = await anchor_y(cdp, cs, anchor["id"])
  snap_before = await snap(cdp, cs)
  await broadcast_assistant_draft(events, sid, "a streaming draft grows below while the reader reads")
  await settle()
  after = await snap(cdp, cs)
  ay_after = await anchor_y(cdp, cs, anchor["id"])
  sc.check(pos_holds(ay_before, ay_after), f"anchor {anchor['id']} holds its viewport offset")
  sc.check(abs(after["top"] - snap_before["top"]) <= ANCHOR_TOLERANCE_PX, "scrollTop unchanged")
  sc.data.update({"ay_before": ay_before, "ay_after": ay_after, "after": after})
  await sc.finish(cdp, cs, results, None)

  # -- S4: returning to the bottom re-arms the follow -----------------------
  sc = Scenario("return_to_bottom_then_follow")
  await ev(cdp, cs, "document.getElementById('scroll-to-bottom').click()")
  await asyncio.sleep(0.4)
  at_bottom = await snap(cdp, cs)
  sc.check(at_bottom["bottom"] <= ANCHOR_TOLERANCE_PX, "jump button returned to the bottom")
  sc.check(at_bottom["pinned"] is True, "pin intent re-armed")
  sc.check(at_bottom["jumpBtn"] is False, "jump button hidden again")
  await broadcast_user(events, sid, "first message after returning to the bottom")
  await settle()
  followed = await snap(cdp, cs)
  sc.check(followed["bottom"] <= ANCHOR_TOLERANCE_PX, "new message followed at the bottom")
  sc.check(followed["pinned"] is True, "still pinned after following")
  sc.data.update({"at_bottom": at_bottom, "followed": followed})
  await sc.finish(cdp, cs, results, None)

  # -- S5: pagination whose new history exceeds the old scroll range --------
  sc = Scenario("pagination_new_history_exceeds_old_range_holds_anchor")
  # The response is delayed so the reader's post-gesture place is observable
  # before the prepend lands: the wheel's own travel is the user's reading
  # move, and the assertion holds the engine to the place the wheel produced.
  await ev(cdp, cs, "window.__delayNextMs = 1500")
  await ev(cdp, cs, "window.__stubPages = [" + json.dumps(page_payload(page1, has_more=True)) + "]")
  # Park the reader 450px from the top (no trigger above 80px), then cross the
  # trigger with real wheel gestures.
  await scroll_write(cdp, cs, 450)
  pre = await snap(cdp, cs)
  await wheel_until_trigger(cdp, cs)
  lead_key = await ev(cdp, cs, "Chat.activeTurnEngineFor(document.getElementById('messages')).segments[0].key")
  anchor = await ev(cdp, cs, TOP_VISIBLE_JS)
  ay_before = await anchor_y(cdp, cs, anchor["id"])
  await browser_harness_session_tree.wait_for(cdp, cs, "window.__fetchLog.length >= 1", label="pagination fetch fired")
  await browser_harness_session_tree.wait_for(
      cdp,
      cs, "Chat.activeTurnEngineFor(document.getElementById('messages'))"
      ".entries.some(e => e.msg && e.msg.id === 'pg1_00u')",
      label="older page ingested")
  immediate = await snap(cdp, cs)
  ay_immediate = await anchor_y(cdp, cs, anchor["id"])
  await settle()
  settled = await snap(cdp, cs)
  ay_settled = await anchor_y(cdp, cs, anchor["id"])
  fetches = await ev(cdp, cs, "window.__fetchLog.length")
  sc.check(fetches == 1, f"exactly one page fetched (got {fetches})")
  sc.check(pos_holds(ay_before, ay_settled), f"anchor {anchor['id']} holds its viewport offset once rendering settles")
  sc.check(settled["bottom"] > 150, f"reader not parked at the new bottom (bottom={settled['bottom']})")
  sc.check(settled["pinned"] is False, "pin intent stays cleared")
  # The reported jump's precondition: the compensation write for the height
  # added above the reader lands past the old maximum scroll, where the old
  # engine's clamp echo read back as a user scroll.
  growth = settled["height"] - pre["height"]
  old_max = pre["height"] - pre["viewport"]
  sc.check(
      pre["top"] + growth > old_max,
      f"the compensation write crosses the old maximum (top {pre['top']} + growth {growth} vs old max {old_max})")
  merge_state = await ev(
      cdp, cs, (
          "(() => {const e = Chat.activeTurnEngineFor(document.getElementById('messages'));"
          "const seg = e.segments.find(sg => sg.entries.some(en => en.msg && en.msg.id === 'pg1_24u'));"
          "return {merged: seg ? seg.key || ('flat#' + e.segments.indexOf(seg)) : null,"
          "rederives: e.stats.rederivesOfSettledTurns};})()"))
  # The page's open trailing span derives under the shared span rule against
  # the head's leading turn: one fused turn whose conclusion and separator are
  # the head-leading turn's, with the seam round's head message on the front.
  expected_merged = 'pg1_24u|' + '|'.join(lead_key.split('|')[1:]) if lead_key else None
  sc.check(
      merge_state["merged"] == expected_merged, f"the page's trailing span fused with the head's leading turn "
      f"({merge_state}, expected {expected_merged})")
  sc.check(
      settled["bottom"] > 150 or pos_holds(ay_before, ay_immediate, tol=40),
      "already right after the update the reader is not thrown toward the bottom")
  sc.data.update(
      {
          "anchor": anchor,
          "ay_before": ay_before,
          "ay_immediate": ay_immediate,
          "ay_settled": ay_settled,
          "pre": pre,
          "immediate": immediate,
          "settled": settled,
          "fetches": fetches,
          "merge": merge_state,
          "lead_key": lead_key,
      })
  await sc.finish(cdp, cs, results, await screenshot(cdp, cs, evidence_dir, "pagination_hold"))

  # -- S6: a stale pagination response after a session switch is dropped ----
  sc = Scenario("session_switch_drops_stale_response")
  stale_page = {
      "messages": [stub_message("user", "stale_marker_u", "STALE-PAGE-MARKER", 0)],
      "has_more": False,
      "next_before": -1000
  }
  await ev(cdp, cs, "window.__stubPages = [" + json.dumps(stale_page) + "]")
  await ev(cdp, cs, "window.__delayNextMs = 2500")
  await ev(cdp, cs, "window.__fetchLog = []")
  await scroll_write(cdp, cs, 0)
  await browser_harness_session_tree.wait_for(cdp, cs, "window.__fetchLog.length >= 1", label="stale fetch in flight")
  await ev(cdp, cs, "switchSession(" + json.dumps(sid_b) + ")")
  await browser_harness_session_tree.wait_for(
      cdp, cs, "document.body.innerText.includes('Session B opening question 0')", label="session B rendered")
  await asyncio.sleep(3.0)
  body_text = await ev(cdp, cs, "document.body.innerText.includes('STALE-PAGE-MARKER')")
  stale_node = await ev(
      cdp, cs, "(Chat.activeTurnEngineFor(document.getElementById('messages'))||{entries:[]})"
      ".entries.some(e => e.msg && e.msg.id === 'stale_marker_u')")
  active = await snap(cdp, cs)
  mounted_b = await ev(cdp, cs, "!!(Chat.TurnEngine && Chat.activeTurnEngineFor(document.getElementById('messages')))")
  sc.check(active["sessionId"] == sid_b, "session B is the active view")
  sc.check(not body_text and not stale_node, "the stale page's content never rendered")
  sc.check(mounted_b, "engine mounted for session B")
  sc.data.update({"active": active, "stale_text_in_dom": body_text, "stale_node": stale_node})
  await sc.finish(cdp, cs, results, None)

  # -- S7: pagination failure surfaces retry; retry loads the last page ------
  sc = Scenario("pagination_failure_retry_and_last_page")
  await ev(cdp, cs, "switchSession(" + json.dumps(sid) + ")")
  await browser_harness_session_tree.wait_for(
      cdp,
      cs,
      "Chat.TurnEngine && Chat.activeTurnEngineFor(document.getElementById('messages'))",
      label="session A re-rendered")
  await settle()
  await ev(cdp, cs, "window.__sessionId = " + json.dumps(sid))
  await ev(cdp, cs, OPEN_OLDEST_TURNS_JS + "(5)")
  await settle()
  await ev(cdp, cs, "window.__failNextStatus = 500")
  await ev(cdp, cs, "window.__stubPages = [" + json.dumps(page_payload(page2, has_more=False)) + "]")
  await scroll_write(cdp, cs, 450)
  await wheel_until_trigger(cdp, cs)
  # The failed fetch changed nothing, so the reader's post-gesture place is
  # already stable; record it before the retry's prepend must hold it.
  anchor = await ev(cdp, cs, TOP_VISIBLE_JS)
  ay_before = await anchor_y(cdp, cs, anchor["id"])
  await browser_harness_session_tree.wait_for(
      cdp,
      cs,
      "(document.getElementById('load-more-sentinel')||{dataset:{}}).dataset.state === 'failed'",
      label="sentinel entered the failed state")
  await settle()
  failed = await snap(cdp, cs)
  sc.check(failed["sentinel"] == "failed", "failed pagination shows the retry hint")
  await ev(cdp, cs, "document.getElementById('load-more-sentinel').click()")
  await browser_harness_session_tree.wait_for(
      cdp, cs, "!document.getElementById('load-more-sentinel')", label="sentinel removed")
  await settle()
  settled = await snap(cdp, cs)
  ay_settled = await anchor_y(cdp, cs, anchor["id"])
  sc.check(settled["sentinel"] is None, "last page removed the pagination hint")
  sc.check(pos_holds(ay_before, ay_settled), f"anchor {anchor['id']} holds through the retry and the hint's removal")
  sc.check(settled["bottom"] > 150, "reader not parked at the bottom")
  sc.data.update({"failed": failed, "settled": settled, "ay_before": ay_before, "ay_settled": ay_settled})
  await sc.finish(cdp, cs, results, await screenshot(cdp, cs, evidence_dir, "pagination_retry"))

  # -- S8: the transcript repaint keeps the reader's place -------------------
  sc = Scenario("transcript_reset_repaint_keeps_position")
  # S7's retry left synthetic pages on top of this view; a reset payload
  # carries only the session's real transcript, so the reader is parked on a
  # fresh view whose every message the payload holds — an anchor the repaint
  # can genuinely keep. The reader opened the oldest turns from their rows to
  # read inside them; the remount's fresh registry folds them back to one row
  # per turn, and the read turn's fold row takes the reading offset — the
  # same rule the manual collapse follows.
  await ev(cdp, cs, "switchSession(" + json.dumps(sid_b) + ")")
  await browser_harness_session_tree.wait_for(
      cdp, cs, "document.body.innerText.includes('Session B opening question 0')", label="session B rendered")
  await ev(cdp, cs, "switchSession(" + json.dumps(sid) + ")")
  await browser_harness_session_tree.wait_for(
      cdp,
      cs,
      "Chat.TurnEngine && Chat.activeTurnEngineFor(document.getElementById('messages'))",
      label="session A re-rendered")
  await ev(cdp, cs, INSTALL_HELPERS_JS)
  await ev(cdp, cs, INSTALL_INSTRUMENT_JS)
  await ev(cdp, cs, "window.__sessionId = " + json.dumps(sid))
  await ev(cdp, cs, OPEN_OLDEST_TURNS_JS + "(5)")
  await settle()
  await ev(cdp, cs, "document.getElementById('scroll-to-bottom').click()")
  await asyncio.sleep(0.4)
  await wheel(cdp, cs, 600)
  await asyncio.sleep(0.5)
  place = await pin_reading_place(cdp, cs)
  engine_anchor0 = place["engine_anchor"]
  wrap_key = place["engine_wrap"]["key"] if place["engine_wrap"] else None
  if engine_anchor0 is None or engine_anchor0["kind"] != "message" or wrap_key is None:
    sc.check(cond=False, message=f"no message anchor inside an open turn to repaint around ({engine_anchor0})")
    sc.data.update({"place": place})
    await sc.finish(cdp, cs, results, await screenshot(cdp, cs, evidence_dir, "transcript_reset"))
    return
  reset_messages = await bootstrap_messages_now(sid, store, events)
  payload = {"reset": True, "messages": reset_messages, "revision": "regression-reset", "total": len(reset_messages)}
  await ev(cdp, cs, "window.__transcriptPayload = " + json.dumps(payload))
  await ev(cdp, cs, "setWorkerTranscriptMode({sessionId: SESSION_ID})")
  await browser_harness_session_tree.wait_for(cdp, cs, "window.__transcriptReads >= 1", label="transcript poll fetched")
  await settle()
  await ev(cdp, cs, "stopPageTimer('transcript-poll'); setWorkerTranscriptMode(null)")
  await ev(cdp, cs, "window.__transcriptPayload = null")
  settled = await snap(cdp, cs)
  remounted = await ev(cdp, cs, "!!(Chat.TurnEngine && Chat.activeTurnEngineFor(document.getElementById('messages')))")
  engine_anchor_after = await ev(cdp, cs, ENGINE_ANCHOR_JS)
  ay_settled = await anchor_y(cdp, cs, engine_anchor0["id"])
  row_settled = await ev(cdp, cs, ROW_OF_JS + f'("{wrap_key}")')
  clamp = await ev(cdp, cs, READ_CLAMP_JS)
  sc.check(remounted, "the live engine survived the reset")
  sc.check(
      anchor_held(engine_anchor_after, engine_anchor0),
      f"the repaint kept the reading identity ({engine_anchor_after} vs {engine_anchor0})")
  sc.check(ay_settled is None, "the remount folded the opened turns back to rows")
  row_holds = (row_settled is not None and abs(row_settled["y"] - engine_anchor0["offset"]) <= ANCHOR_TOLERANCE_PX)
  clamp_consistent = clamp is not None and abs(clamp["limitedScrollTop"] - settled["top"]) <= ANCHOR_TOLERANCE_PX
  sc.check(
      row_holds or clamp_consistent, f"the read turn's fold row carries the reading offset across the repaint "
      f"(row {row_settled}, offset {engine_anchor0['offset']}, clamp {clamp})")
  sc.check(settled["bottom"] > 150, "reader not thrown to the bottom by the repaint")
  sc.data.update(
      {
          "place": place,
          "engine_anchor": engine_anchor0,
          "wrap_key": wrap_key,
          "row_settled": row_settled,
          "settled": settled
      })
  await sc.finish(cdp, cs, results, await screenshot(cdp, cs, evidence_dir, "transcript_reset"))

  # -- S9: a viewport resize keeps the reading position ----------------------
  sc = Scenario("viewport_resize_keeps_reading_position")
  # Park the reader inside the open newest turn: the wheel's capture anchors
  # the engine on a rendered message, the identity the resize must hold. The
  # 900px override first normalizes the viewport under emulation semantics:
  # the headless window's natural innerHeight is smaller than an explicit
  # 900px override (window chrome), so the size checks must compare
  # measurements taken under the same semantics.
  await cdp.send(
      "Emulation.setDeviceMetricsOverride", {
          "width": 1440,
          "height": 900,
          "deviceScaleFactor": 1,
          "mobile": False
      },
      session_id=cs)
  await asyncio.sleep(0.4)
  await ev(cdp, cs, "document.getElementById('scroll-to-bottom').click()")
  await asyncio.sleep(0.4)
  await wheel(cdp, cs, 200)
  await wait_reader_settled(cdp, cs)
  anchor = await ev(cdp, cs, TOP_VISIBLE_JS)
  ay_before = await anchor_y(cdp, cs, anchor["id"])
  before = await snap(cdp, cs)
  await cdp.send(
      "Emulation.setDeviceMetricsOverride", {
          "width": 1440,
          "height": 600,
          "deviceScaleFactor": 1,
          "mobile": False
      },
      session_id=cs)
  await browser_harness_session_tree.wait_for(
      cdp,
      cs,
      f"document.getElementById('messages').clientHeight < {before['viewport'] - 100}",
      label="viewport shrank")
  await asyncio.sleep(0.4)
  resized = await snap(cdp, cs)
  ay_resized = await anchor_y(cdp, cs, anchor["id"])
  await cdp.send(
      "Emulation.setDeviceMetricsOverride", {
          "width": 1440,
          "height": 900,
          "deviceScaleFactor": 1,
          "mobile": False
      },
      session_id=cs)
  await browser_harness_session_tree.wait_for(
      cdp,
      cs,
      f"document.getElementById('messages').clientHeight === {before['viewport']}",
      label="viewport returned to its size")
  await asyncio.sleep(0.4)
  restored = await snap(cdp, cs)
  ay_restored = await anchor_y(cdp, cs, anchor["id"])
  sc.check(resized["viewport"] < before["viewport"] - 100, "the viewport actually shrank")
  sc.check(restored["viewport"] == before["viewport"], "the viewport returned to its size")
  sc.check(pos_holds(ay_before, ay_resized), "anchor holds across the shrink")
  sc.check(pos_holds(ay_before, ay_restored), "anchor holds across the restore")
  sc.data.update({"ay_before": ay_before, "ay_resized": ay_resized, "ay_restored": ay_restored})
  await sc.finish(cdp, cs, results, None)

  # -- S10: hiding the chat tab and returning keeps the reading position ----
  sc = Scenario("hide_and_reappear_keeps_reading_position")
  ay_before = await anchor_y(cdp, cs, anchor["id"])
  # The tab switch's DOM effect on the chat column, without the terminal tab's
  # PTY mount: the column hides, the engine sees a zero-height container, and
  # the return rebuilds it.
  await ev(cdp, cs, "document.getElementById('tab-chat').classList.add('hidden')")
  await asyncio.sleep(1.0)
  hidden = await snap(cdp, cs)
  await ev(cdp, cs, "document.getElementById('tab-chat').classList.remove('hidden')")
  await settle()
  back = await snap(cdp, cs)
  ay_back = await anchor_y(cdp, cs, anchor["id"])
  mounted = await ev(cdp, cs, "!!(Chat.TurnEngine && Chat.activeTurnEngineFor(document.getElementById('messages')))")
  sc.check(hidden["viewport"] == 0, "the chat column actually hid")
  sc.check(back["viewport"] > 0, "the chat column is visible again")
  sc.check(mounted, "engine still mounted after hide/show")
  sc.check(pos_holds(ay_before, ay_back), f"anchor {anchor['id']} recovered at its viewport offset")
  sc.data.update({"hidden": hidden, "back": back, "ay_before": ay_before, "ay_back": ay_back})
  await sc.finish(cdp, cs, results, None)

  # -- S11: single-turn collapse and reopen keep the reading content ---------
  # The identity rule for the manual path: collapsing the turn the reader is
  # inside parks its fold row at the reading offset (or records the clamp),
  # and reopening returns the message to the offset it held before.
  sc = Scenario("single_turn_fold_expand_keeps_reading_content")
  await ev(cdp, cs, "document.getElementById('scroll-to-bottom').click()")
  await asyncio.sleep(0.4)
  await ev(cdp, cs, OPEN_OLDEST_TURNS_JS + "(5)")
  await settle()
  await wheel(cdp, cs, 600)
  await wait_reader_settled(cdp, cs)
  place = await pin_reading_place(cdp, cs)
  engine_anchor0 = place["engine_anchor"]
  if not engine_anchor0 or engine_anchor0["kind"] != "message" or place["engine_wrap"] is None:
    sc.check(cond=False, message=f"no message anchor to fold around ({engine_anchor0})")
    sc.data.update({"place": place})
    await sc.finish(cdp, cs, results, await screenshot(cdp, cs, evidence_dir, "single_turn_fold"))
    return
  anchor = place["msg"]
  ay_before = place["ay"]
  wrap_key = place["engine_wrap"]["key"]
  steps = []
  await ev(cdp, cs, CLEAR_CLAMP_JS)
  await ev(cdp, cs, f"document.querySelector('[data-turn-key=\"{wrap_key}\"] .turn-collapse').click()")
  folded_now = await fold_step_state(cdp, cs, engine_anchor0["id"], wrap_key)
  await settle()
  folded_settled = await fold_step_state(cdp, cs, engine_anchor0["id"], wrap_key)
  steps.append({"step": "collapse", "immediate": folded_now, "settled": folded_settled})
  sc.check(
      anchor_held(folded_now["engine_anchor"], engine_anchor0) and
      anchor_held(folded_settled["engine_anchor"], engine_anchor0), f"the collapse kept the reading identity "
      f"({folded_now['engine_anchor']} vs {engine_anchor0})")
  sc.check(folded_now["msg"] is None and folded_settled["msg"] is None, "the collapse hid the reading message")
  clamp = folded_now["clamp"]
  row_holds = (
      folded_now["row"] is not None and abs(folded_now["row"]["y"] - engine_anchor0["offset"]) <= ANCHOR_TOLERANCE_PX)
  clamp_consistent = clamp is not None and abs(
      clamp["limitedScrollTop"] - folded_now["snap"]["top"]) <= ANCHOR_TOLERANCE_PX
  sc.check(
      row_holds or clamp_consistent, f"the fold row carries the reading offset or the clamp says why "
      f"(row {folded_now['row']}, offset {engine_anchor0['offset']}, clamp {clamp})")

  await ev(cdp, cs, CLEAR_CLAMP_JS)
  await ev(cdp, cs, f"document.querySelector('[data-turn-key=\"{wrap_key}\"] .turn-row').click()")
  reopened_now = await fold_step_state(cdp, cs, engine_anchor0["id"], wrap_key)
  await settle()
  reopened_settled = await fold_step_state(cdp, cs, engine_anchor0["id"], wrap_key)
  steps.append({"step": "reopen", "immediate": reopened_now, "settled": reopened_settled})
  sc.check(
      anchor_held(reopened_now["engine_anchor"], engine_anchor0) and
      anchor_held(reopened_settled["engine_anchor"], engine_anchor0), f"the reopen kept the reading identity "
      f"({reopened_now['engine_anchor']} vs {engine_anchor0})")
  restore_target = {"y": engine_anchor0["offset"], "gap": None}
  sc.check(
      pos_holds(restore_target, reopened_now["msg"]) and pos_holds(restore_target, reopened_settled["msg"]),
      f"the reopen put the reading message back at its offset "
      f"(immediate {reopened_now['msg']}, settled {reopened_settled['msg']} "
      f"vs {engine_anchor0['offset']})")
  sc.data.update(
      {
          "anchor": anchor,
          "ay_before": ay_before,
          "engine_anchor": engine_anchor0,
          "wrap_key": wrap_key,
          "steps": steps
      })
  await sc.finish(cdp, cs, results, await screenshot(cdp, cs, evidence_dir, "single_turn_fold"))

  # -- S12: from the bottom, each wheel notch moves the content one notch ---
  # The defect this locks out: the height model counted no row gap, so every
  # row crossing the window's top edge shifted the content 12px — a 100px
  # notch moved the screen anywhere from 52 to 176px.
  sc = Scenario("wheel_steps_from_bottom_equal_notch")
  wheel_pages: list[list[dict]] = []
  for prefix in ("wh1", "wh2", "wh3"):
    wheel_page: list[dict] = []
    for i in range(25):
      stub_turn(wheel_page, prefix, i, closed=True)
    wheel_pages.append(wheel_page)
  await ev(cdp, cs, "window.__sessionId = " + json.dumps(sid_c))
  # Distinct next_before cursors: an unchanged cursor with has_more trips the
  # client's no-progress guard and stops pagination early.
  await ev(
      cdp, cs, "window.__stubPages = [" + ", ".join(
          json.dumps(page_payload(page, has_more=page_i < len(wheel_pages) - 1, next_before=-1000 - page_i))
          for page_i, page in enumerate(wheel_pages)) + "]")
  await ev(cdp, cs, "switchSession(" + json.dumps(sid_c) + ")")
  await browser_harness_session_tree.wait_for(
      cdp,
      cs, "Chat.TurnEngine && Chat.activeTurnEngineFor(document.getElementById('messages')) "
      "&& document.querySelector('#messages [data-message-id]')",
      label="wheel session rendered")
  await wait_reader_settled(cdp, cs)
  # Paginate the stub pages in (a short head may already have auto-filled
  # some); the notches below then run over more than four viewports of folded
  # rows.
  while await ev(cdp, cs, "window.__stubPages.length > 0"):
    await ev(cdp, cs, "window.__fetchLog = []")
    await scroll_write(cdp, cs, 450)
    await wheel_until_trigger(cdp, cs)
    await wait_reader_settled(cdp, cs)
  loaded = await snap(cdp, cs)
  sc.check(loaded["sentinel"] is None, "every history page loaded (sentinel gone)")
  sc.check(
      loaded["height"] > 4 * loaded["viewport"],
      f"more than four viewports of folded rows ({loaded['height']} vs {loaded['viewport']})")
  # Start at the bottom through the jump-to-bottom button.
  await ev(cdp, cs, "document.getElementById('scroll-to-bottom').click()")
  await wait_reader_settled(cdp, cs)
  start = await snap(cdp, cs)
  sc.check(start["bottom"] <= ANCHOR_TOLERANCE_PX, f"the jump button parked at the bottom ({start['bottom']})")
  sc.check(start["pinned"] is True, "pin intent armed at the bottom")
  box = await ev(
      cdp, cs, "(() => {const r = document.getElementById('messages').getBoundingClientRect();"
      " return {x: r.x + r.width / 2, y: r.y + r.height / 2};})()")
  steps = []
  for i in range(15):
    ref = await ev(cdp, cs, PICK_CENTER_ROW_JS)
    before = await ev(cdp, cs, WHEEL_REF_STATE_JS)
    pump = asyncio.create_task(ev(cdp, cs, NOTCH_PUMP_JS + "(700)"))
    await asyncio.sleep(0.05)  # the pump is producing frames before the event lands
    await wheel_notch(cdp, cs, box)
    await pump
    after = await ev(cdp, cs, WHEEL_REF_STATE_JS)
    step = None
    if before and after and before["y"] is not None and after["y"] is not None:
      step = after["y"] - before["y"]
    steps.append({"notch": i, "ref": ref, "before": before, "after": after, "step": step})
  measured = [s for s in steps if s["step"] is not None]
  sc.check(len(measured) == len(steps), f"every notch kept its reference row ({len(measured)}/{len(steps)})")
  for s in measured:
    sc.check(abs(s["step"] - 100) <= 1.0, f"notch {s['notch']} moved the reference row 100±1px (got {s['step']:.2f})")
  sc.data.update({"loaded": loaded, "start": start, "steps": steps})
  await sc.finish(cdp, cs, results, await screenshot(cdp, cs, evidence_dir, "wheel_steps"))

  # -- S13: jump to bottom from deep history shows the last turn at once -----
  # The jump projects the pinned range before it snaps the scroll position, so
  # the last turn's row is in the DOM and in view with the bottom spacer at
  # zero height.
  sc = Scenario("jump_to_bottom_from_history_shows_last_turn")
  await scroll_write(cdp, cs, 0)
  await wait_reader_settled(cdp, cs)
  top = await snap(cdp, cs)
  sc.check(top["top"] <= 80, f"reader parked at the top of the loaded history ({top['top']})")
  sc.check(
      top["height"] > 4 * top["viewport"],
      f"more than four viewports of loaded history ({top['height']} vs {top['viewport']})")
  await ev(cdp, cs, "document.getElementById('scroll-to-bottom').click()")
  await ev(cdp, cs, TWO_FRAMES_JS)
  last_key = await ev(cdp, cs, LAST_TURN_KEY_JS)
  row = await ev(cdp, cs, ROW_OF_JS + f'("{last_key}")') if last_key else None
  spacer = await ev(cdp, cs, BOTTOM_SPACER_JS)
  after = await snap(cdp, cs)
  sc.check(row is not None and row["inView"], f"the last turn's row intersects the viewport ({row})")
  sc.check(spacer == 0, f"the bottom spacer is 0px tall (got {spacer})")
  sc.data.update({"top": top, "last_key": last_key, "row": row, "bottom_spacer": spacer, "after": after})
  await sc.finish(cdp, cs, results, await screenshot(cdp, cs, evidence_dir, "jump_to_bottom"))


# --- harness shell ----------------------------------------------------------


async def run_harness(args: argparse.Namespace) -> None:
  evidence_dir = pathlib.Path(args.evidence_dir)
  commit = browser_harness_session_tree.open_evidence_dir(evidence_dir)

  # The run's whole footprint (home, profile) lives in one temp directory;
  # --keep names it for debugging instead of deleting it at the end.
  tmp_path = pathlib.Path(tempfile.mkdtemp(prefix="charliebot-scroll-regression-"))
  # The browser binary is host-local, so the synthetic home inherits the host
  # config's headless_chrome_bin — read through the existing config entry
  # BEFORE the env override (both caches hot-reload per file fingerprint).
  from src.app import registrations
  from src.infra import config
  registrations.register_all()
  host_chrome_bin = str(config.get_config().headless_chrome_bin or "")

  home = tmp_path / "charliebot-home"
  home.mkdir()
  profile = tmp_path / "chrome-profile"
  server_port = browser_harness_session_tree.pick_free_port()
  home_config = {
      "headless_chrome_bin": host_chrome_bin,
      "server": {
          "port": server_port,
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
              "pool": ["fake-scripted"],
          },
      "paths": {
          "worktree_dir": str(home / "worktrees")
      },
  }
  (home / "config.yaml").write_text(json.dumps(home_config, indent=2), encoding="utf-8")
  access_key = browser_harness_session_tree.mint_access_key("scroll-regression-key-")
  browser_harness_session_tree.write_credentials_yaml(home, access_key)
  for var in ("CHARLIEBOT_ACCESS_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"):
    os.environ.pop(var, None)
  os.environ["CHARLIEBOT_HOME"] = str(home)

  # The existing config entry provides the credential the browser will use,
  # and the config's own headless-chrome entry is the first browser candidate.
  from src.infra import credentials
  key = credentials.configured_access_key()
  if not key:
    browser_harness_session_tree.fail("configured_access_key() returned nothing for the synthetic home")
  chrome = browser_harness_session_tree.resolve_chrome(
      args.chrome or config.get_config().headless_chrome_bin or None, browser_harness_session_tree.fail)
  browser_harness_session_tree.log(f"chat scroll regression · commit {commit} · chrome {chrome}")

  sid, sid_b, sid_c, _bootstrap_messages, store, events = await seed_sessions()

  # Isolated server: the real app, lifespan disabled, this checkout's files.
  import uvicorn

  import server
  server_config = uvicorn.Config(server.app, host="127.0.0.1", port=server_port, log_level="error", lifespan="off")
  app_server = uvicorn.Server(server_config)
  serve_task = asyncio.get_running_loop().create_task(app_server.serve())
  deadline = time.monotonic() + 30
  while not app_server.started:
    if serve_task.done():
      browser_harness_session_tree.fail(f"isolated server failed to start: {serve_task.exception()}")
    if time.monotonic() > deadline:
      browser_harness_session_tree.fail("isolated server did not start within 30s")
    await asyncio.sleep(0.1)
  base = f"http://127.0.0.1:{server_port}"

  def _get(path: str, timeout: float) -> int:
    with urllib.request.urlopen(base + path, timeout=timeout) as resp:
      return resp.status

  try:
    # The first page render compiles templates and lists sessions — give the
    # cold address check room, and keep the loop live while it runs.
    status = await asyncio.to_thread(_get, "/", 30)
    if status >= 500:
      browser_harness_session_tree.fail(f"server address check failed: GET / returned {status}")
    browser_harness_session_tree.log(f"server address verified: {base}")
  except Exception as exc:
    browser_harness_session_tree.fail(f"server address check failed: {exc!r}")

  chrome_proc = browser_harness_session_tree.launch_chrome(
      chrome, profile, browser_harness_session_tree.pick_free_port(),
      browser_harness_session_tree.DESKTOP_CAPTURE_FLAGS)
  browser_harness_session_tree.log(f"test browser pid {chrome_proc.pid} (muted, private profile {profile})")
  results_payload: dict = {
      "tested_commit": commit,
      "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
      "browser": chrome,
      "browser_pid": chrome_proc.pid,
      "server_base": base,
      "entry_point": "tools/chat_scroll_regression.py",
      "invocation": sys.argv[1:],
      "scenarios": [],
      "console_errors": [],
  }
  out = evidence_dir / RESULTS_NAME

  def save() -> None:
    out.write_text(json.dumps(results_payload, indent=2, ensure_ascii=False), encoding="utf-8")

  ws_url = await browser_harness_session_tree.devtools_ws_url(chrome_proc, 30, browser_harness_session_tree.fail)
  cdp = await browser_harness_session_tree.connect_cdp(ws_url)
  try:
    page_session, _target = await browser_harness_session_tree.open_cdp_page(cdp, ("Page", "Runtime", "Network"))
    await cdp.send(
        "Network.setCookie", {
            "name": "charliebot_access_key",
            "value": key,
            "url": base + "/"
        },
        session_id=page_session)
    # The client's own auth gate reads localStorage before first paint; seed
    # it the same way a logged-in browser carries it, so the auth overlay
    # never covers the page and the input domain reaches the chat column.
    await cdp.send(
        "Page.addScriptToEvaluateOnNewDocument",
        {"source": "try { localStorage.setItem('charliebot_access_key', " + json.dumps(key) + "); } catch (e) {}"},
        session_id=page_session)
    await cdp.send("Page.navigate", {"url": f"{base}/?session={sid}"}, session_id=page_session)
    await run_scenarios(cdp, page_session, sid, sid_b, sid_c, store, events, evidence_dir, results_payload["scenarios"])
    results_payload["console_errors"] = list(cdp.console_errors)
  except Exception as exc:
    results_payload["aborted_by"] = repr(exc)
    raise
  finally:
    await cdp.close()
    browser_harness_session_tree.stop_child(chrome_proc, grace_s=5, kill_reap_s=5)
    browser_harness_session_tree.log(f"test browser pid {chrome_proc.pid} stopped")
    app_server.should_exit = True
    try:
      await asyncio.wait_for(serve_task, timeout=10)
    except Exception:
      serve_task.cancel()
    # Partial evidence still lands when a scenario raises: the failed rows and
    # the abort reason are the run's diagnosis.
    save()
    if not args.keep:
      shutil.rmtree(tmp_path, ignore_errors=True)

  failed = [s for s in results_payload["scenarios"] if not s["ok"]]
  browser_harness_session_tree.log(
      f"results written to {out}; "
      f"{len(results_payload['scenarios']) - len(failed)} passed, {len(failed)} failed")
  if failed:
    browser_harness_session_tree.fail(f"{len(failed)} scenario(s) failed: {[s['name'] for s in failed]}")


def main() -> None:
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  parser.add_argument(
      "--evidence-dir", default=None, help="where screenshots and the results JSON land (default: host temp dir)")
  parser.add_argument(
      "--chrome", default=None, help="chrome binary override (default: config headless_chrome_bin, then google-chrome)")
  parser.add_argument(
      "--keep", action="store_true", help="keep the temp home/profile for debugging (still stops the browser)")
  args = parser.parse_args()
  if args.evidence_dir is None:
    args.evidence_dir = str(
        pathlib.Path(tempfile.gettempdir()) /
        ("charliebot-scroll-regression-" + time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())))
  try:
    asyncio.run(run_harness(args))
  except SystemExit:
    raise
  except Exception as exc:
    browser_harness_session_tree.fail(repr(exc))


if __name__ == "__main__":
  main()
