"""Warm headless-Chrome renderer for the page-height measurement.

A fresh ``chrome --dump-dom`` process pays browser startup on every measurement — ~0.55 s
on this host's build against ~25 ms of render. One Chrome process stays warm per OS process
for the process lifetime and serves every measurement over the DevTools websocket
(``websockets.sync``, the websocket dependency the server already carries) — the trade for
the wall win is one idle browser's resident set. Lazy launch, one lock serializing renders,
atexit close so a CLI invocation never orphans the browser, relaunch after a crash; a
failure raises ``ValueError`` loudly and closes the renderer so the next call starts fresh.
"""

import atexit
import json
import shutil
import subprocess
import tempfile
import threading
import time
from pathlib import Path

from websockets.sync.client import connect as _ws_connect

_RENDER_TIMEOUT_S = 60
_LAUNCH_TIMEOUT_S = 15


class _WarmRenderer:
  """One warm Chrome process serving page-height renders over the DevTools websocket."""

  def __init__(self, chrome_bin: Path) -> None:
    self._chrome_bin = chrome_bin
    self._proc = None
    self._ws = None
    self._udd: Path | None = None
    self._session: str | None = None
    self._msg_id = 0
    self._deadline = 0.0
    self._pending: list[dict] = []

  def render_height(self, probe_uri: str) -> int:
    try:
      if self._proc is None or self._proc.poll() is not None or self._ws is None:
        self.close()
        self._launch()
      self._deadline = time.monotonic() + _RENDER_TIMEOUT_S
      return self._render_once(probe_uri)
    except Exception as e:
      self.close()
      if isinstance(e, ValueError):
        raise
      if isinstance(e, TimeoutError):
        raise ValueError(
            f"headless renderer timed out after {_RENDER_TIMEOUT_S}s while measuring the plan page height") from e
      raise ValueError(f"headless renderer failed while measuring the plan page height: {e}") from e

  def _render_once(self, probe_uri: str) -> int:
    self._cmd("Page.enable")
    self._cmd("Page.navigate", url=probe_uri)
    self._wait_load()
    marker = "document.getElementById('page-height')?.textContent ?? null"
    while True:
      reply = self._cmd("Runtime.evaluate", expression=marker, returnByValue=True)
      value = reply["result"]["result"].get("value")
      if value is not None:
        return int(value)
      if time.monotonic() > self._deadline:
        raise ValueError("headless renderer output carried no page-height marker; cannot measure the plan page")
      time.sleep(0.005)

  def _launch(self) -> None:
    self._udd = Path(tempfile.mkdtemp(prefix="headless-render-"))
    self._deadline = time.monotonic() + _LAUNCH_TIMEOUT_S
    try:
      self._proc = subprocess.Popen(
          [
              str(self._chrome_bin),
              "--headless",
              "--disable-gpu",
              "--no-sandbox",
              "--allow-file-access-from-files",
              "--remote-debugging-port=0",
              f"--user-data-dir={self._udd}",
              "about:blank",
          ],
          stdout=subprocess.DEVNULL,
          stderr=subprocess.DEVNULL)
    except OSError as e:
      self.close()
      raise ValueError(f"headless renderer could not be launched: {self._chrome_bin} ({e})") from e
    port_file = self._udd / "DevToolsActivePort"
    while not port_file.is_file():
      if self._proc.poll() is not None or time.monotonic() > self._deadline:
        self.close()
        raise ValueError(f"headless renderer could not be launched: {self._chrome_bin} (no DevToolsActivePort)")
      time.sleep(0.05)
    port, path = port_file.read_text().splitlines()[:2]
    try:
      self._ws = _ws_connect(f"ws://127.0.0.1:{port}{path}", open_timeout=_LAUNCH_TIMEOUT_S)
    except Exception as e:
      self.close()
      raise ValueError(f"headless renderer could not be launched: {self._chrome_bin} ({e})") from e
    target_id = self._cmd("Target.createTarget", url="about:blank")["result"]["targetId"]
    self._session = self._cmd("Target.attachToTarget", targetId=target_id, flatten=True)["result"]["sessionId"]

  def _cmd(self, method: str, **params: object) -> dict:
    """One CDP command; events and foreign replies stash in _pending until this reply arrives."""
    self._msg_id += 1
    msg: dict = {"id": self._msg_id, "method": method, "params": params}
    if self._session is not None:
      msg["sessionId"] = self._session
    self._ws.send(json.dumps(msg))
    while True:
      reply = self._next_message()
      if reply.get("id") == msg["id"]:
        if "error" in reply:
          raise RuntimeError(f"{method}: {reply['error']}")
        return reply
      self._pending.append(reply)

  def _wait_load(self) -> None:
    # The marker's write races nothing (the iframe's load handler runs before the outer
    # page's load event), but a fast file:// load can land inside _cmd's own drain.
    while True:
      for i, msg in enumerate(self._pending):
        if msg.get("method") == "Page.loadEventFired":
          del self._pending[i]
          return
      msg = self._next_message()
      if msg.get("method") == "Page.loadEventFired":
        return
      self._pending.append(msg)

  def _next_message(self) -> dict:
    remaining = self._deadline - time.monotonic()
    if remaining <= 0:
      raise TimeoutError
    return json.loads(self._ws.recv(timeout=remaining))

  def close(self) -> None:
    """Idempotent teardown; every caller holds the module lock."""
    if self._ws is not None:
      self._ws.close()
      self._ws = None
    if self._proc is not None:
      self._proc.kill()
      self._proc.wait(5)
      self._proc = None
    if self._udd is not None:
      shutil.rmtree(self._udd, ignore_errors=True)
      self._udd = None
    self._session = None
    self._pending = []


_renderer: _WarmRenderer | None = None
_module_lock = threading.Lock()


def render_height(chrome_bin: Path, probe_uri: str) -> int:
  """Render *probe_uri* through the process-wide warm Chrome, launching it on the first call."""
  global _renderer
  with _module_lock:
    if _renderer is None or _renderer._chrome_bin != chrome_bin:
      if _renderer is not None:
        _renderer.close()
      _renderer = _WarmRenderer(chrome_bin)
    return _renderer.render_height(probe_uri)


atexit.register(lambda: _renderer.close() if _renderer is not None else None)
