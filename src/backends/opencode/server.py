"""The opencode 2.0 server CharlieBot starts and shares across its turns: one per proxy_url.

The server runs ``opencode serve --port 0`` with the profile's XDG store, the headless config,
a random password and the file index off. One global event stream feeds every turn: each event goes
to the turn registered for its session, and an execution that no turn claims is interrupted. Each
turn claims its directory while it runs; when a turn ends, the server releases every loaded
directory no turn claims. With no turn and no claim for OPENCODE_SERVER_HEAVY_CHECK_SECONDS, the server
stops if its RSS is above OPENCODE_SERVER_HEAVY_RSS_MB; otherwise it stops after
OPENCODE_SERVER_IDLE_EXIT_SECONDS with no turn and no claim.
When its event stream or its process fails, every running turn ends first (the turn-end steps),
then the process stops, then each turn gets a ServerLostError.
"""

from __future__ import annotations

import asyncio
import collections
import contextlib
import json
import os
import pathlib
import re
import secrets
import signal
import subprocess
from collections.abc import Coroutine

import httpx

from src.backends.opencode import opencode_limits, options
from src.infra import config, constants, home, log_once, process
from src.runtime.agent_process import base, spawn

log = log_once.LazyStructlogLogger()

OPENCODE_BIN = str(pathlib.Path.home() / ".opencode" / "bin" / "opencode")
# The shell join script the server's session environment points BASH_ENV at.
JOIN_SCRIPT = pathlib.Path(__file__).with_name("cgroup_join.sh")

_LISTEN_RE = re.compile(r"server listening on (http://\S+)")
_VMRSS_RE = re.compile(r"^VmRSS:\s+(\d+) kB$", re.MULTILINE)
# The newest bytes the server wrote to stdout or stderr, quoted in the error that ends a turn.
_OUTPUT_TAIL_BYTES = 64 * 1024
_READ_CHUNK_BYTES = 64 * 1024


class ServerLostError(RuntimeError):
  """The shared server ended while this turn was running; the message names why."""


def process_rss_mb(pid: int) -> float:
  """The resident memory of process *pid* alone, in MiB; its child processes are not counted."""
  status = pathlib.Path(f"/proc/{pid}/status").read_text()
  match = _VMRSS_RE.search(status)
  if match is None:
    raise RuntimeError(f"/proc/{pid}/status has no VmRSS line")
  return int(match.group(1)) / 1024


def require_binary() -> None:
  """Refuse when no opencode 2.0 binary sits at ``OPENCODE_BIN``.

  Both the shared server and a one-shot run execute that path, so each asks
  here before its own work starts.
  """
  if not pathlib.Path(OPENCODE_BIN).is_file():
    raise RuntimeError(f"opencode 2.0 binary not found at {OPENCODE_BIN}")


def headless_config() -> dict:
  """The server's config: broad auto-approval, minus the three tools a CharlieBot turn must not use.

  ``question`` has no UI to answer it; ``subagent`` would start a child session that does not
  carry the turn's environment; ``opencode_session_move`` would move the session out of the
  turn's directory. ``shell`` is pinned to bash so the BASH_ENV join script runs for every command.
  """
  return {
      "permission": {
          "*": "allow",
          "question": "deny",
          "subagent": "deny",
          "opencode_session_move": "deny",
      },
      "shell": "bash",
      "default_agent": "charliebot",
      "agent": {
          "charliebot": {
              "mode": "primary"
          },
      },
      **options.model_overrides_config(config.get_config().backends.options),
  }


class Turn:
  """One turn's claim on a session of the shared server: its events and its turn-end steps."""

  def __init__(self, server: OpenCodeServer, session_id: str, directory: str) -> None:
    self.session_id = session_id
    self.directory = directory
    # Event dicts from the global stream, and at most one ServerLostError.
    self.events: asyncio.Queue[dict | ServerLostError] = asyncio.Queue()
    self._server = server
    self._finished = False

  def drop_earlier_interrupts(self) -> None:
    """Drop the queued session.execution.interrupted events of interrupts sent before this turn's prompt."""
    kept: list[dict | ServerLostError] = []
    while not self.events.empty():
      item = self.events.get_nowait()
      if isinstance(item, dict) and item["type"] == "session.execution.interrupted":
        continue
      kept.append(item)
    for item in kept:
      self.events.put_nowait(item)

  async def finish(self) -> None:
    """The turn-end steps, in order: unregister, interrupt the session, delete its shells.

    Idempotent. Skipped when the server process is gone, because nothing is left to interrupt.
    """
    if self._finished:
      return
    self._finished = True
    self._server.unregister(self)
    if not self._server.process_alive:
      log.info("opencode_turn_end_steps_skipped", session_id=self.session_id)
      return
    await self._server.interrupt(self.session_id)
    await self._server.delete_shells(self.session_id, self.directory)


class OpenCodeServer:
  """One opencode 2.0 server process, its HTTP client, its event stream, and the turns it serves."""

  def __init__(self, proxy_url: str | None) -> None:
    self._proxy_url = proxy_url
    self._lock = asyncio.Lock()
    # Guards the claim counts and the release: a claim taken during a release waits for it.
    self._release_lock = asyncio.Lock()
    # Turns running in each directory; a directory with a claim is never released.
    self._claims: collections.Counter[str] = collections.Counter()
    self._proc: spawn.SpawnedProcess | None = None
    self._client: httpx.AsyncClient | None = None
    self._connected = asyncio.Event()
    self._reader: asyncio.Task | None = None
    self._idle: asyncio.Task | None = None
    self._tasks: set[asyncio.Task] = set()
    self._turns: dict[str, Turn] = {}
    # Interrupts sent for each session whose session.execution.interrupted has not arrived yet.
    self._pending: collections.Counter[str] = collections.Counter()
    self._settled: dict[str, asyncio.Event] = {}
    self._output_tail = bytearray()
    self._stopping = False
    self.version: str | None = None

  @property
  def process_alive(self) -> bool:
    return self._proc is not None and self._proc.returncode is None

  @property
  def pid(self) -> int | None:
    return self._proc.pid if self._proc is not None else None

  def server_env(self, password: str) -> dict[str, str]:
    """The server process environment: CharlieBot's own minus any run identity, plus the store, password and config."""
    env = dict(os.environ)
    # A turn's run identity belongs to that turn's processes only; the shared server carries none.
    env.pop(constants.RUN_TOKEN_ENV, None)
    env.pop(constants.SESSION_ID_ENV_VAR, None)
    env.update(
        {
            "XDG_DATA_HOME": str(home.opencode_data_dir()),
            "XDG_STATE_HOME": str(home.opencode_state_dir()),
            "XDG_CACHE_HOME": str(home.opencode_cache_dir()),
            "OPENCODE_SERVER_PASSWORD": password,
            "OPENCODE_CONFIG_CONTENT": json.dumps(headless_config()),
            # Read only from the environment. The file index costs ~62 MiB per git directory; no turn uses it.
            "OPENCODE_DISABLE_FFF": "1",
        })
    if self._proxy_url is not None:
      base.apply_proxy_env(env, self._proxy_url)
    return env

  async def ensure_running(self) -> None:
    """Start the server unless it is running; concurrent callers share one start."""
    async with self._lock:
      if self.process_alive:
        return
      await self._start()

  async def stop(self) -> None:
    """Stop the server process now; the next ensure_running starts a new one."""
    async with self._lock:
      if self._proc is not None:
        await self._shutdown()

  def register(self, session_id: str, directory: str) -> Turn:
    """Claim *session_id* for a turn; from now on its executions are that turn's."""
    if session_id in self._turns:
      raise RuntimeError(f"opencode session {session_id} already has a running turn")
    turn = Turn(self, session_id, directory)
    self._turns[session_id] = turn
    self._cancel_idle()
    return turn

  def unregister(self, turn: Turn) -> None:
    if self._turns.get(turn.session_id) is turn:
      del self._turns[turn.session_id]
    if not self._turns and self.process_alive:
      self._arm_idle()

  async def interrupt(self, session_id: str) -> bool:
    """POST the session's interrupt. Returns interrupted; an interrupted execution's event is awaited."""
    self._add_pending(session_id)
    try:
      response = await self.client.post(f"/api/session/{session_id}/interrupt")
      response.raise_for_status()
      interrupted = bool(response.json()["interrupted"])
    except BaseException:
      self._settle_one(session_id)
      raise
    if not interrupted:
      self._settle_one(session_id)
    return interrupted

  async def wait_interrupts_settled(self, session_id: str) -> None:
    """Wait until every interrupt sent before this call has its session.execution.interrupted event."""
    try:
      await asyncio.wait_for(
          self._settled_event(session_id).wait(), timeout=opencode_limits.OPENCODE_INTERRUPT_SETTLE_TIMEOUT)
    except TimeoutError as e:
      raise RuntimeError(
          f"earlier interrupt of opencode session {session_id} did not settle within "
          f"{opencode_limits.OPENCODE_INTERRUPT_SETTLE_TIMEOUT:g} s") from e

  async def delete_shells(self, session_id: str, directory: str) -> None:
    """Remove the running shell commands that *session_id*'s turn started in *directory*."""
    response = await self.client.get("/api/shell", params={"location[directory]": directory})
    response.raise_for_status()
    for shell in response.json()["data"]:
      if shell.get("metadata", {}).get("sessionID") != session_id:
        continue
      removed = await self.client.delete(f"/api/shell/{shell['id']}", params={"location[directory]": directory})
      removed.raise_for_status()

  async def claim(self, directory: str) -> None:
    """Hold *directory* for a turn: no release unloads it until the turn's claim is dropped."""
    async with self._release_lock:
      self._claims[directory] += 1

  async def release_claim(self, directory: str) -> None:
    """Drop a turn's claim on *directory*, then unload every loaded directory no turn claims."""
    async with self._release_lock:
      self._claims[directory] -= 1
      if self._claims[directory] == 0:
        del self._claims[directory]
      if (not self._claims and not self._turns and not self._stopping and self._client is not None and
          self.process_alive and self._idle is not None and self._idle.done()):
        self._arm_idle()
      # A stopping, exited or still starting server has no loaded directories to release.
      if self._stopping or self._client is None or not self.process_alive:
        return
      response = await self.client.get("/api/debug/location")
      response.raise_for_status()
      for location in response.json():
        loaded = location["directory"]
        if loaded in self._claims:
          continue
        released = await self.client.delete("/api/debug/location", params={"location[directory]": loaded})
        if not released.is_success:
          raise RuntimeError(f"opencode release of directory {loaded} returned HTTP {released.status_code}")

  @property
  def client(self) -> httpx.AsyncClient:
    assert self._client is not None, "the opencode server client exists only while the server runs"
    return self._client

  async def _start(self) -> None:
    require_binary()
    if self._proc is not None:
      # The previous process exited; release what it left behind before starting again.
      await self._shutdown()
    self._stopping = False
    self._connected = asyncio.Event()
    self._pending.clear()
    self._settled.clear()
    for path in (home.opencode_data_dir(), home.opencode_state_dir(), home.opencode_cache_dir()):
      path.mkdir(parents=True, exist_ok=True)
    password = secrets.token_urlsafe(32)
    try:
      self._proc = await spawn.spawn_subprocess(
          OPENCODE_BIN,
          "serve",
          "--port",
          "0",
          cwd=str(home.opencode_store_dir()),
          env=self.server_env(password),
          stdin=subprocess.DEVNULL,
          stdout=subprocess.PIPE,
          stderr=subprocess.PIPE,
          limit=base.DEFAULT_BUFFER_LIMIT,
          start_new_session=True,
          pdeathsig=True,
      )
      self._track(self._drain(self._proc.stderr))
      url = await self._read_listening_url()
      self._track(self._drain(self._proc.stdout))
      self._client = httpx.AsyncClient(
          base_url=url, auth=("opencode", password), timeout=opencode_limits.OPENCODE_HTTP_API_TIMEOUT)
      await self._check_version()
      self._reader = self._track(self._read_events())
      self._track(self._watch_exit())
      await self._await_connected()
      self._arm_idle()
      log.info("opencode_server_started", pid=self._proc.pid, url=url, version=self.version)
    except BaseException:
      await self._shutdown()
      raise

  async def _read_listening_url(self) -> str:
    assert self._proc is not None and self._proc.stdout is not None
    loop = asyncio.get_running_loop()
    deadline = loop.time() + opencode_limits.OPENCODE_SERVER_START_TIMEOUT
    while True:
      try:
        raw = await asyncio.wait_for(self._proc.stdout.readline(), timeout=deadline - loop.time())
      except TimeoutError as e:
        raise RuntimeError(
            f"{OPENCODE_BIN} serve did not print its listening address within "
            f"{opencode_limits.OPENCODE_SERVER_START_TIMEOUT:g} s") from e
      if not raw:
        raise RuntimeError(
            f"{OPENCODE_BIN} serve exited before printing its listening address: "
            f"{self._tail_text()}")
      self._append_tail(raw)
      match = _LISTEN_RE.search(raw.decode("utf-8", errors="replace"))
      if match:
        return match.group(1)

  async def _check_version(self) -> None:
    response = await self.client.get("/api/info")
    response.raise_for_status()
    self.version = response.json()["version"]
    if self.version.split(".", 1)[0] != "2":
      raise RuntimeError(f"{OPENCODE_BIN} reports opencode {self.version}; CharlieBot needs the 2.x server")

  async def _await_connected(self) -> None:
    waiter = asyncio.ensure_future(self._connected.wait())
    try:
      done, _ = await asyncio.wait(
          {waiter, self._reader},
          timeout=opencode_limits.OPENCODE_SERVER_START_TIMEOUT,
          return_when=asyncio.FIRST_COMPLETED)
    finally:
      waiter.cancel()
    if waiter not in done:
      if self._reader in done:
        raise RuntimeError(f"opencode event stream ended before server.connected: {self._reader.result()}")
      raise RuntimeError("opencode event stream did not report server.connected in time")

  async def _read_events(self) -> str:
    """Read the one global event stream until it ends; a failed or ended stream ends the server."""
    assert self._client is not None
    try:
      async with self._client.stream("GET", "/api/event", timeout=None) as response:
        response.raise_for_status()
        async for line in response.aiter_lines():
          if line.startswith("data:"):
            self._dispatch(json.loads(line[len("data:"):]))
      reason = "opencode event stream closed"
    except Exception as e:
      reason = f"opencode event stream failed: {e}"
    # An ended stream is usually the server dying: wait one grace for its exit
    # so the failure names the exit code, not the symptom. wait()'s shield
    # keeps the shared exit future pending for the cleanup path's waiter.
    if self._proc is not None and self._proc.returncode is None:
      with contextlib.suppress(TimeoutError):
        await asyncio.wait_for(self._proc.wait(), timeout=opencode_limits.OPENCODE_STREAM_EXIT_GRACE)
    if self._proc is not None and self._proc.returncode is not None:
      reason = f"opencode serve exited with code {self._proc.returncode}: {self._tail_text()}"
    self._track(self._lose(reason))
    return reason

  async def _watch_exit(self) -> None:
    assert self._proc is not None
    code = await self._proc.wait()
    self._track(self._lose(f"opencode serve exited with code {code}: {self._tail_text()}"))

  def _dispatch(self, event: dict) -> None:
    event_type = event.get("type")
    if event_type == "server.connected":
      self._connected.set()
      return
    session_id = event.get("data", {}).get("sessionID")
    if session_id is None:
      return
    if event_type == "session.execution.interrupted":
      self._settle_one(session_id)
    turn = self._turns.get(session_id)
    if turn is not None:
      turn.events.put_nowait(event)
    elif event_type == "session.execution.started":
      log.warning("opencode_unclaimed_execution_interrupted", session_id=session_id)
      self._track(self.interrupt(session_id))

  async def _lose(self, reason: str) -> None:
    """End the server after a failure: the running turns' steps, then the process, then ServerLostError."""
    async with self._lock:
      if self._proc is None or self._stopping:
        return
      self._stopping = True
      turns = list(self._turns.values())
      for turn in turns:
        try:
          await turn.finish()
        except Exception:
          log.exception("opencode_turn_end_failed", session_id=turn.session_id)
      await self._shutdown()
      for turn in turns:
        turn.events.put_nowait(ServerLostError(reason))
      log.error("opencode_server_lost", reason=reason, turns=len(turns))

  async def _stop_when_idle(self) -> None:
    await asyncio.sleep(opencode_limits.OPENCODE_SERVER_HEAVY_CHECK_SECONDS)
    async with self._lock:
      if not self._turns and not self._claims and self.process_alive:
        rss_mb = process_rss_mb(self._proc.pid)
        if rss_mb > opencode_limits.OPENCODE_SERVER_HEAVY_RSS_MB:
          log.info("opencode_server_heavy_exit", pid=self._proc.pid, rss_mb=round(rss_mb, 1))
          self._stopping = True
          await self._shutdown()
          return
    rest = opencode_limits.OPENCODE_SERVER_IDLE_EXIT_SECONDS - opencode_limits.OPENCODE_SERVER_HEAVY_CHECK_SECONDS
    await asyncio.sleep(rest)
    async with self._lock:
      if self._turns or self._claims or not self.process_alive:
        return
      log.info("opencode_server_idle_exit", pid=self._proc.pid)
      self._stopping = True
      await self._shutdown()

  async def _shutdown(self) -> None:
    """Stop the process, cancel every task this server started, close the client. Caller holds the lock."""
    self._stopping = True
    proc = self._proc
    if proc is not None and proc.returncode is None:
      process.kill_process_group(proc.pid, signal.SIGTERM)
      try:
        await asyncio.wait_for(proc.wait(), timeout=opencode_limits.OPENCODE_SERVER_STOP_TIMEOUT)
      except TimeoutError:
        log.warning("opencode_server_stop_timeout", pid=proc.pid)
        process.kill_process_group(proc.pid, signal.SIGKILL)
        await proc.wait()
    current = asyncio.current_task()
    tasks = [task for task in self._tasks if task is not current]
    for task in tasks:
      task.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)
    if self._client is not None:
      await self._client.aclose()
    self._client = None
    self._proc = None
    self._reader = None

  def _track(self, coro: Coroutine) -> asyncio.Task:
    """Run *coro* as one of this server's tasks; a failure is logged when the task ends."""
    task = asyncio.ensure_future(coro)
    self._tasks.add(task)
    task.add_done_callback(self._task_done)
    return task

  def _task_done(self, task: asyncio.Task) -> None:
    self._tasks.discard(task)
    if not task.cancelled() and task.exception() is not None:
      log.error("opencode_server_task_failed", error=str(task.exception()))

  def _arm_idle(self) -> None:
    self._cancel_idle()
    self._idle = self._track(self._stop_when_idle())

  def _cancel_idle(self) -> None:
    if self._idle is not None:
      self._idle.cancel()
      self._idle = None

  async def _drain(self, stream: asyncio.StreamReader) -> None:
    while chunk := await stream.read(_READ_CHUNK_BYTES):
      self._append_tail(chunk)

  def _append_tail(self, data: bytes) -> None:
    self._output_tail += data
    del self._output_tail[:-_OUTPUT_TAIL_BYTES]

  def _tail_text(self) -> str:
    return bytes(self._output_tail).decode("utf-8", errors="replace").strip()

  def _add_pending(self, session_id: str) -> None:
    self._pending[session_id] += 1
    self._settled_event(session_id).clear()

  def _settle_one(self, session_id: str) -> None:
    # Clamped at zero: an execution can end interrupted without this server having asked for it.
    if self._pending[session_id] > 0:
      self._pending[session_id] -= 1
    if self._pending[session_id] == 0:
      self._settled_event(session_id).set()

  def _settled_event(self, session_id: str) -> asyncio.Event:
    event = self._settled.get(session_id)
    if event is None:
      event = self._settled[session_id] = asyncio.Event()
      event.set()
    return event


_servers: dict[str | None, OpenCodeServer] = {}


def server_for(proxy_url: str | None) -> OpenCodeServer:
  """The shared server for *proxy_url*: one per distinct proxy setting, created on first use."""
  server = _servers.get(proxy_url)
  if server is None:
    server = _servers[proxy_url] = OpenCodeServer(proxy_url)
  return server
