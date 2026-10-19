"""Own one side's fake services, their local TLS proxy and their traffic log."""

import asyncio
import collections
import importlib
import logging
import pathlib
import threading
import time
from typing import Any

from . import proxy as proxy_module
from . import recorder

LOGGER = logging.getLogger(__name__)
FAKE_READY_TIMEOUT_SECONDS = 30.0
FAKE_START_TIMEOUT_SECONDS = 10.0
FAKE_STOP_TIMEOUT_SECONDS = 10.0
FAKE_PUSH_TIMEOUT_SECONDS = 10.0
_IMMEDIATE_READY_FAKES = frozenset({"gemini", "muse", "openai_compatible"})


class FakeSet:
  """One server side's fake services, request recorder and loopback proxy."""

  def __init__(
      self,
      tree_root: pathlib.Path,
      seeds: dict[str, dict],
      certificate: pathlib.Path,
      private_key: pathlib.Path,
  ) -> None:
    self.tree_root = tree_root
    self.seeds = seeds
    self.recorder = recorder.Recorder(tree_root / "logs" / "fakes.jsonl")
    self._certificate = certificate
    self._private_key = private_key
    self._loop: asyncio.AbstractEventLoop | None = None
    self._thread: threading.Thread | None = None
    self._loop_started = threading.Event()
    self._ready = {name: threading.Event() for name in seeds}
    self._refused: collections.Counter[str] = collections.Counter()
    self._refused_lock = threading.Lock()
    self._connection_counts: collections.Counter[str] = collections.Counter()
    self._fakes: dict[str, Any] = {}
    self._proxy: proxy_module.LocalHTTPSProxy | None = None
    self.proxy_url: str | None = None

  def start(self) -> None:
    self._thread = threading.Thread(target=self._run_loop, name="parity-fake-services", daemon=False)
    self._thread.start()
    if not self._loop_started.wait(FAKE_START_TIMEOUT_SECONDS):
      raise RuntimeError("fake-service event loop did not start")
    if self._loop is None:
      raise RuntimeError("fake-service event loop is missing")
    future = asyncio.run_coroutine_threadsafe(self._start_async(), self._loop)
    future.result(FAKE_START_TIMEOUT_SECONDS)

  def wait_ready(self, names: tuple[str, ...], timeout_seconds: float) -> list[str]:
    deadline = time.monotonic() + timeout_seconds
    for name in names:
      self._ready[name].wait(max(0.0, deadline - time.monotonic()))
    return [name for name in names if not self._ready[name].is_set()]

  def mark(self) -> int:
    return self.recorder.mark()

  def records_since(self, mark: int) -> list[dict]:
    return self.recorder.since(mark)

  def refused_hosts(self) -> dict[str, int]:
    with self._refused_lock:
      return dict(self._refused)

  def push(self, fake: str, action: str, payload: dict, timeout_seconds: float = FAKE_PUSH_TIMEOUT_SECONDS) -> None:
    if self._loop is None or fake not in self._fakes:
      raise RuntimeError(f"fake {fake!r} is not running for {self.tree_root}")
    future = asyncio.run_coroutine_threadsafe(self._fakes[fake].push(action, payload), self._loop)
    future.result(timeout_seconds)

  def stop(self) -> None:
    if self._loop is None or self._thread is None:
      return
    future = asyncio.run_coroutine_threadsafe(self._stop_async(), self._loop)
    future.result(FAKE_STOP_TIMEOUT_SECONDS)
    self._loop.call_soon_threadsafe(self._loop.stop)
    self._thread.join(FAKE_STOP_TIMEOUT_SECONDS)
    if self._thread.is_alive():
      raise RuntimeError(f"fake-service thread did not stop within {FAKE_STOP_TIMEOUT_SECONDS}s")
    self._loop = None
    self._thread = None

  async def _start_async(self) -> None:
    self._fakes = {}
    for name, seed in self.seeds.items():
      module = importlib.import_module(f".{name}", __name__)
      fake = module.Fake(seed, self.recorder, lambda name=name: self._ready[name].set())
      self._fakes[name] = fake
      if name in _IMMEDIATE_READY_FAKES:
        self._ready[name].set()
    self._proxy = proxy_module.LocalHTTPSProxy(
        self._certificate,
        self._private_key,
        frozenset(self.seeds),
        self.recorder,
        self._handle_http,
        self._handle_websocket,
        self._record_refusal,
        self._next_connection_id,
    )
    port = await self._proxy.start()
    self.proxy_url = f"http://127.0.0.1:{port}"

  async def _stop_async(self) -> None:
    if self._proxy is not None:
      await self._proxy.close()
    await asyncio.sleep(0.05)

  async def _handle_websocket(self, fake: str, connection: proxy_module.WebSocketConnection) -> None:
    await self._fakes[fake].websocket(connection)

  def _handle_http(self, fake: str, request: proxy_module.Request) -> proxy_module.HTTPResponse:
    return self._fakes[fake].http(request)

  def _next_connection_id(self, fake: str) -> str:
    self._connection_counts[fake] += 1
    return f"{fake}-{self._connection_counts[fake]}"

  def _record_refusal(self, target: str) -> None:
    with self._refused_lock:
      self._refused[target] += 1
    self.recorder.record({"kind": "refused", "host": target})

  def _run_loop(self) -> None:
    loop = asyncio.new_event_loop()
    self._loop = loop
    asyncio.set_event_loop(loop)
    self._loop_started.set()
    try:
      loop.run_forever()
    finally:
      pending = [task for task in asyncio.all_tasks(loop) if not task.done()]
      for task in pending:
        task.cancel()
      results = loop.run_until_complete(asyncio.gather(*pending, return_exceptions=True))
      for result in results:
        if isinstance(result, BaseException) and not isinstance(result, asyncio.CancelledError):
          LOGGER.error("fake-service task failed during shutdown", exc_info=result)
      loop.close()


__all__ = ["FAKE_READY_TIMEOUT_SECONDS", "FakeSet"]
