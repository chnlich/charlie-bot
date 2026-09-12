"""Warm headless-Chrome renderer for the page-height measurement.

A fresh ``chrome --dump-dom`` process pays ~0.55 s of browser startup per measurement against
~25 ms of render; one Chrome process stays warm per OS process for the process lifetime and
serves every measurement over the DevTools websocket (``websockets.sync``, already a
dependency) — the trade is one idle browser's resident set. Lazy launch, one lock, atexit
close (a CLI invocation must never orphan the browser), relaunch after a crash; a failure
raises ``ValueError`` loudly with the browser's stderr tail, and closes the renderer.
"""

import atexit
import json
import shutil
import subprocess
import tempfile
import threading
import time
from pathlib import Path

from websockets.sync import client as _ws_client

from src.core.timeouts import HEADLESS_LAUNCH_TIMEOUT, HEADLESS_RENDER_TIMEOUT, HEADLESS_TEARDOWN_WAIT


class _WarmRenderer:
  """One warm Chrome process serving page-height renders over the DevTools websocket."""

  def __init__(self, chrome_bin: Path) -> None:
    self._chrome_bin = chrome_bin
    self._proc = None
    self._ws = None
    self._udd: Path | None = None
    self._stderr_path: Path | None = None
    self._session: str | None = None
    self._msg_id = 0
    self._pending: list[dict] = []

  def render_height(self, probe_uri: str) -> int:
    try:
      if self._proc is None or self._proc.poll() is not None or self._ws is None:
        self.close()
        self._launch()
      self._deadline = time.monotonic() + HEADLESS_RENDER_TIMEOUT
      return self._render_once(probe_uri)
    except Exception as e:
      tail = self._stderr_tail()
      self.close()
      if isinstance(e, ValueError):
        raise
      if isinstance(e, TimeoutError):
        raise ValueError(
            f"headless renderer timed out after {HEADLESS_RENDER_TIMEOUT}s while measuring the plan page height{tail}"
        ) from e
      raise ValueError(f"headless renderer failed while measuring the plan page height: {e}{tail}") from e

  def _render_once(self, probe_uri: str) -> int:
    self._pending.clear()  # per-render scratch: the previous render's drained events are dead
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
    self._stderr_path = self._udd / "stderr.log"
    self._deadline = time.monotonic() + HEADLESS_LAUNCH_TIMEOUT
    try:
      # Our own handle closes with the with-block; the child keeps its inherited fd.
      with self._stderr_path.open("wb") as stderr_log:
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
            stderr=stderr_log)
    except OSError as e:
      tail = self._stderr_tail()
      self.close()
      raise ValueError(f"headless renderer could not be launched: {self._chrome_bin} ({e}){tail}") from e
    port_file = self._udd / "DevToolsActivePort"
    while not port_file.is_file():
      if self._proc.poll() is not None or time.monotonic() > self._deadline:
        tail = self._stderr_tail()
        self.close()
        raise ValueError(
            f"headless renderer could not be launched: {self._chrome_bin} "
            f"(no DevToolsActivePort){tail}")
      time.sleep(0.05)
    port, path = port_file.read_text().splitlines()[:2]
    try:
      self._ws = _ws_client.connect(f"ws://127.0.0.1:{port}{path}", open_timeout=HEADLESS_LAUNCH_TIMEOUT)
    except Exception as e:
      tail = self._stderr_tail()
      self.close()
      raise ValueError(f"headless renderer could not be launched: {self._chrome_bin} ({e}){tail}") from e
    target_id = self._cmd("Target.createTarget", url="about:blank")["result"]["targetId"]
    self._session = self._cmd("Target.attachToTarget", targetId=target_id, flatten=True)["result"]["sessionId"]

  def _stderr_tail(self) -> str:
    # The browser's last stderr bytes for the failure messages; empty while nothing was written.
    if self._stderr_path is None or not self._stderr_path.is_file():
      return ""
    data = self._stderr_path.read_bytes()[-400:].decode("utf-8", errors="replace").strip()
    return f": {data}" if data else ""

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
    # The iframe's load handler writes the marker before the outer page's load event, but a
    # fast file:// load can land inside _cmd's own drain, so pending frames are scanned first.
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
    if (remaining := self._deadline - time.monotonic()) <= 0:
      raise TimeoutError
    return json.loads(self._ws.recv(timeout=remaining))

  def close(self) -> None:
    """Idempotent teardown; render-path callers hold the module lock, the atexit close runs at exit."""
    if self._ws is not None:
      self._ws.close()
      self._ws = None
    if self._proc is not None:
      self._proc.kill()
      self._proc.wait(HEADLESS_TEARDOWN_WAIT)
      self._proc = None
    if self._udd is not None:
      shutil.rmtree(self._udd, ignore_errors=True)
      self._udd = None
      self._stderr_path = None
    self._session = None
    self._pending = []


_renderer: _WarmRenderer | None = None
_module_lock = threading.Lock()


def render_height(chrome_bin: Path, probe_uri: str) -> int:
  global _renderer
  with _module_lock:
    if _renderer is None or _renderer._chrome_bin != chrome_bin:
      if _renderer is not None:
        _renderer.close()
      _renderer = _WarmRenderer(chrome_bin)
    return _renderer.render_height(probe_uri)


atexit.register(lambda: _renderer.close() if _renderer is not None else None)
