"""Scripted agent used by the isolated parity harness."""

import asyncio
import base64
import ctypes
import fcntl
import hashlib
import hmac
import json
import os
import pathlib
import re
import signal
import sys
import threading
import time
import urllib.parse

_ARGV_VALUE = re.compile(r"\$\{argv:(?P<flag>[^}]+)\}")
_PDEATHSIG_OPTION = 2
# The SIGTERM dispositions a `sigterm` step names; scenario_file.py accepts the same two words.
_SIGTERM_DISPOSITIONS = {"ignore": signal.SIG_IGN, "default": signal.SIG_DFL}
_STDOUT_LOCK = threading.Lock()
# How long a stream-json run waits for stdin to close after its steps. The server closes stdin on the turn's result.
STDIN_EOF_TIMEOUT_SECONDS = 5.0


class MissingArgvFlag(ValueError):  # noqa: N818
  """An emit value names a command-line flag the backend did not pass."""


class AgentDirGone(RuntimeError):  # noqa: N818
  """The fake-agent directory was removed while a gate waited for its open marker."""


def _proc_identity(pid: int) -> tuple[str, str]:
  stat = pathlib.Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
  fields = stat.rsplit(")", 1)[1].split()
  return fields[0], fields[19]


def _access_key(credentials: pathlib.Path) -> str:
  in_charliebot = False
  for line in credentials.read_text(encoding="utf-8").splitlines():
    if line and not line[0].isspace():
      in_charliebot = line.strip() == "charliebot:"
    elif in_charliebot and line.strip().startswith("access_key:"):
      value = line.strip().split(":", 1)[1].strip()
      if value[:1] in ("'", '"') and value[-1:] == value[:1]:
        value = value[1:-1]
      if not value:
        raise ValueError(f"empty charliebot.access_key in {credentials}")
      return value
  raise ValueError(f"charliebot.access_key is absent from {credentials}")


def _run_token_record() -> tuple[dict | None, bool]:
  token = os.environ.get("CHARLIEBOT_RUN_TOKEN")
  if token is None:
    return None, False
  payload_b64, signature_b64 = token.split(".", 1)
  padding = "=" * (-len(payload_b64) % 4)
  payload = json.loads(base64.urlsafe_b64decode(payload_b64 + padding))
  if not isinstance(payload, dict):
    raise ValueError("CHARLIEBOT_RUN_TOKEN payload is not an object")
  signature_padding = "=" * (-len(signature_b64) % 4)
  signature = base64.urlsafe_b64decode(signature_b64 + signature_padding)
  credentials = pathlib.Path(os.environ["CHARLIEBOT_HOME"]) / "credentials.yaml"
  key = _access_key(credentials)
  expected = hmac.new(key.encode("utf-8"), payload_b64.encode("ascii"), hashlib.sha256).digest()
  claims_valid = all(
      isinstance(payload.get(field), str) and payload[field] for field in ("session_id", "run_id", "agent"))
  return payload, claims_valid and hmac.compare_digest(signature, expected)


def _environment() -> dict[str, str]:
  names = {
      "CHARLIEBOT_SESSION_ID", "CHARLIEBOT_HOME", "CHARLIEBOT_VENV_BIN", "PATH", "GIT_CEILING_DIRECTORIES",
      "PYDANTIC_DISABLE_PLUGINS", "CLAUDECODE"
  }
  names.update(name for name in os.environ if name.startswith("CLAUDE_CODE_"))
  return {name: os.environ[name] for name in sorted(names) if name in os.environ}


def _stdin_text(index: int, stdin_bytes: bytes) -> str:
  try:
    return stdin_bytes.decode("utf-8")
  except UnicodeDecodeError as error:
    raise ValueError(f"fake-agent spawn record {index}: stdin is not valid UTF-8: {error}") from error


def _spawn_record(index: int, name: str, scripted: bool, stdin_bytes: bytes) -> dict:
  pid = os.getpid()
  _, pid_start = _proc_identity(pid)
  pdeathsig = ctypes.c_int()
  libc = ctypes.CDLL(None, use_errno=True)
  result = libc.prctl(_PDEATHSIG_OPTION, ctypes.byref(pdeathsig), 0, 0, 0)
  if result != 0:
    error_number = ctypes.get_errno()
    raise OSError(error_number, os.strerror(error_number))
  claims, token_valid = _run_token_record()
  stdout_path = os.readlink("/proc/self/fd/1")
  if name == "opencode" and sys.argv[2:3] == ["serve"]:
    stdout_path = str(pathlib.Path.home() / "fake-agent" / "agent.raw")
    pathlib.Path(stdout_path).touch()
    pathlib.Path(stdout_path).with_name("agent.raw.cursor").write_text("0", encoding="utf-8")
  return {
      "index": index,
      "name": name,
      "scripted": scripted,
      "argv": [name, *sys.argv[2:]],
      "cwd": os.getcwd(),
      "pid": pid,
      "pid_start": pid_start,
      "pgid": os.getpgrp(),
      "stdout_path": stdout_path,
      "stdin_bytes": len(stdin_bytes),
      "stdin": _stdin_text(index, stdin_bytes),
      # ``nice`` is sampled where this record is written, after the end of stdin in ``main``. A server sends a fake a
      # stdin prompt only after it raised the turn-tree nice parent-side: Python spawns at
      # src/runtime/agent_process/base.py:999, applies the limits at :1010 and starts the prompt writer at :1016-1017;
      # Rust spawns at rust/crates/charliebot-agents/src/transport/spawn.rs:129, applies the limits at :130 and its
      # caller writes the prompt afterwards. Spawns that get no stdin prompt, a one-shot argv spawn
      # (base.py:834, spawn.rs:147-150) and the OpenCode server, have no input for this sample to order against.
      "nice": os.getpriority(os.PRIO_PROCESS, 0),
      "own_session": os.getsid(0) == pid,
      "pdeathsig": pdeathsig.value,
      "env": _environment(),
      "run_token_claims": claims,
      "token_valid": token_valid,
  }


def _read_stdin_line() -> bytes:
  return b"" if sys.stdin.isatty() else sys.stdin.buffer.readline()


def _read_stdin_rest() -> bytes:
  return b"" if sys.stdin.isatty() else sys.stdin.buffer.read()


def _is_stream_json(first_line: bytes) -> bool:
  """Whether stdin is a stream-json session: its first line is a JSON user message, not a plain prompt."""
  try:
    message = json.loads(first_line)
  except ValueError:
    return False
  return isinstance(message, dict) and message.get("type") == "user"


def _emit_line(text: str) -> None:
  """Write one stdout line; the stdin echo thread and the scripted steps share stdout."""
  with _STDOUT_LOCK:
    sys.stdout.write(text + "\n")
    sys.stdout.flush()


def _echo_user_line(line: bytes) -> None:
  """Echo a stdin user message as Claude's replay of it, the line the server matches against its own write."""
  message = json.loads(line)
  if message.get("type") != "user":
    raise ValueError(f"stream-json stdin line is not a user message: {line[:80]!r}")
  replay = {"type": "user", "isReplay": True, "uuid": message["uuid"], "message": message.get("message")}
  _emit_line(json.dumps(replay, ensure_ascii=False, separators=(",", ":")))


class _StdinEcho:
  """Echoes each stream-json user line as it arrives, so a turn can take input while its steps run."""

  def __init__(self) -> None:
    self._thread = threading.Thread(target=self._run, daemon=True)

  def start(self) -> None:
    self._thread.start()

  def _run(self) -> None:
    for line in sys.stdin.buffer:
      _echo_user_line(line)

  def join(self) -> None:
    # The server closes stdin when the turn's result arrives, so the reader reaches EOF after the steps.
    self._thread.join(timeout=STDIN_EOF_TIMEOUT_SECONDS)


def _start_index(agent_dir: pathlib.Path, name: str, stdin_bytes: bytes) -> tuple[int, bool, dict | None]:
  lock_path = agent_dir / "lock"
  spawns_path = agent_dir / "spawns.jsonl"
  with lock_path.open("a+") as lock_file:
    fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
    starts_count = sum(1 for line in spawns_path.read_text(encoding="utf-8").splitlines()
                       if line.strip()) if spawns_path.exists() else 0
    script_path = agent_dir / "script.json"
    script = json.loads(script_path.read_text(encoding="utf-8")) if script_path.exists() else None
    starts = script["starts"] if script is not None else []
    entry = starts[starts_count] if starts_count < len(starts) else None
    scripted = entry is not None and entry["name"] == name
    record = _spawn_record(starts_count, name, scripted, stdin_bytes)
    with spawns_path.open("a", encoding="utf-8") as spawns_file:
      # Readers split on str.splitlines, which also breaks at U+2028 and U+0085; ASCII escapes keep one record per line.
      spawns_file.write(json.dumps(record, separators=(",", ":")) + "\n")
      spawns_file.flush()
    return starts_count, scripted, entry


def _argv_value(flag: str) -> str:
  args = sys.argv[2:]
  for index, arg in enumerate(args):
    if arg == flag:
      if index + 1 == len(args):
        raise MissingArgvFlag(f"missing argv value after flag {flag!r}")
      return args[index + 1]
  raise MissingArgvFlag(f"missing argv flag {flag!r}")


def _substitute(value: object) -> object:
  if isinstance(value, str):
    match = _ARGV_VALUE.fullmatch(value)
    return _argv_value(match["flag"]) if match else value
  if isinstance(value, list):
    return [_substitute(item) for item in value]
  if isinstance(value, dict):
    return {key: _substitute(item) for key, item in value.items()}
  return value


def _run_steps(entry: dict, agent_dir: pathlib.Path, index: int) -> int:
  for step in entry["steps"]:
    if "emit" in step:
      event = _substitute(step["emit"])
      if not isinstance(event, dict):
        raise ValueError("emit step must resolve to a JSON object")
      _emit_line(json.dumps(event, ensure_ascii=False, separators=(",", ":")))
    elif "gate" in step:
      gate = step["gate"]
      (agent_dir / f"at-{gate}").write_text(str(index), encoding="utf-8")
      while not (agent_dir / f"open-{gate}").exists():
        if not agent_dir.is_dir():
          raise AgentDirGone(f"fake-agent directory {agent_dir} is gone while gate {gate!r} waits")
        time.sleep(0.02)
    elif "sigterm" in step:
      signal.signal(signal.SIGTERM, _SIGTERM_DISPOSITIONS[step["sigterm"]])
    else:
      raise ValueError(f"unknown fake-agent step: {step!r}")
  return entry["exit_code"]


def _write_exit(agent_dir: pathlib.Path, index: int, exit_code: int) -> None:
  (agent_dir / f"exit-{index}").write_text(str(exit_code), encoding="utf-8")


class FakeOpenCodeServer:
  """The loopback OpenCode API driven by one fake-agent start entry."""

  def __init__(self, agent_dir: pathlib.Path, index: int, entry: dict) -> None:
    self.agent_dir = agent_dir
    self.index = index
    self.entry = entry
    self.sessions: dict[str, str] = {}
    self.shells: dict[str, dict] = {}
    self.loaded: set[str] = set()
    self.active: dict[str, asyncio.Event] = {}
    self.subscribers: list[asyncio.Queue] = []
    self.next_id = 0

  def new_id(self, prefix: str) -> str:
    self.next_id += 1
    return f"{prefix}_{self.next_id}"

  def emit(self, event: dict) -> None:
    for queue in self.subscribers:
      queue.put_nowait(event)

  async def handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    head = await reader.readuntil(b"\r\n\r\n")
    lines = head.decode("ascii").split("\r\n")
    method, target, _ = lines[0].split(" ", 2)
    headers = {
        key.strip().lower(): value.strip() for key, value in (line.split(":", 1) for line in lines[1:] if ":" in line)
    }
    length = int(headers.get("content-length", 0))
    body = json.loads(await reader.readexactly(length)) if length else None
    path, _, query = target.partition("?")
    params = urllib.parse.parse_qs(query)
    if method == "GET" and path == "/api/event":
      await self.stream(writer)
      return
    status, payload = await self.route(method, path, params, body)
    data = json.dumps(payload).encode("utf-8") if payload is not None else b""
    writer.write(
        f"HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {len(data)}\r\n"
        "Connection: close\r\n\r\n".encode("ascii") + data)
    await writer.drain()
    writer.close()
    await writer.wait_closed()

  async def route(self, method: str, path: str, params: dict, body: dict | None) -> tuple[int, object]:
    parts = path.strip("/").split("/")
    if method == "GET" and path == "/api/info":
      return 200, {"version": "2.0.32"}
    if method == "GET" and path == "/api/debug/location":
      return 200, [{"directory": directory} for directory in sorted(self.loaded)]
    if method == "DELETE" and path == "/api/debug/location":
      self.loaded.discard(params.get("location[directory]", [""])[0])
      return 204, None
    if method == "GET" and path == "/api/model":
      self.loaded.add(params.get("location[directory]", [""])[0])
      return 200, {
          "data": [{
              "id": "fake-model",
              "providerID": "fakeprov",
              "limit": {
                  "context": 200000,
                  "output": 8000
              }
          }]
      }
    if method == "POST" and path == "/api/session":
      session_id = self.new_id("ses")
      self.sessions[session_id] = body["location"]["directory"]
      self.loaded.add(body["location"]["directory"])
      return 200, {"data": {"id": session_id}}
    if len(parts) >= 3 and parts[:2] == ["api", "session"]:
      session_id = parts[2]
      if session_id not in self.sessions:
        return 404, {"_tag": "SessionNotFoundError", "sessionID": session_id}
      action = parts[3] if len(parts) > 3 else None
      if method == "GET" and action is None:
        return 200, {"data": {"id": session_id}}
      if method == "PUT" and action == "environment":
        return 204, None
      if method == "POST" and action == "prompt":
        asyncio.create_task(self.run_prompt(session_id, body))
        return 200, {"data": {"id": self.new_id("msg")}}
      if method == "POST" and action == "interrupt":
        cancel = self.active.pop(session_id, None)
        if cancel is None:
          return 200, {"interrupted": False}
        cancel.set()
        self.emit({"type": "session.execution.interrupted", "data": {"sessionID": session_id, "reason": "user"}})
        return 200, {"interrupted": True}
    if method == "POST" and path == "/api/location/reload":
      return 204, None
    if method == "GET" and path == "/api/shell":
      directory = params.get("location[directory]", [""])[0]
      self.loaded.add(directory)
      shells = [shell for shell in self.shells.values() if shell["directory"] == directory]
      return 200, {"location": {"directory": directory}, "data": shells}
    if method == "DELETE" and len(parts) == 3 and parts[:2] == ["api", "shell"]:
      self.shells.pop(parts[2], None)
      return 204, None
    return 404, {"error": f"no route {method} {path}"}

  async def run_prompt(self, session_id: str, body: dict) -> None:
    cancel = asyncio.Event()
    self.active[session_id] = cancel
    self.emit({"type": "session.execution.started", "data": {"sessionID": session_id}})
    for step in self.entry["steps"]:
      if "emit" in step:
        event = json.loads(json.dumps(step["emit"]))
        event.setdefault("data", {}).setdefault("sessionID", session_id)
        self.emit(event)
        if event.get("type") in ("session.execution.succeeded", "session.execution.failed",
                                 "session.execution.interrupted"):
          self.active.pop(session_id, None)
          return
      elif "gate" in step:
        gate = step["gate"]
        (self.agent_dir / f"at-{gate}").write_text(str(self.index), encoding="utf-8")
        while not (self.agent_dir / f"open-{gate}").exists() and not cancel.is_set():
          if not self.agent_dir.is_dir():
            raise AgentDirGone(f"fake-agent directory {self.agent_dir} is gone while gate {gate!r} waits")
          await asyncio.sleep(0.02)
        if cancel.is_set():
          return
      elif "sigterm" in step:
        signal.signal(signal.SIGTERM, _SIGTERM_DISPOSITIONS[step["sigterm"]])
      else:
        raise ValueError(f"unknown fake-agent step: {step!r}")
    self.active.pop(session_id, None)
    self.emit({"type": "session.execution.succeeded", "data": {"sessionID": session_id}})

  async def stream(self, writer: asyncio.StreamWriter) -> None:
    writer.write(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
    queue: asyncio.Queue = asyncio.Queue()
    self.subscribers.append(queue)
    queue.put_nowait({"type": "server.connected", "data": {}})
    try:
      while True:
        event = await queue.get()
        if event is None:
          break
        writer.write(f"data: {json.dumps(event, separators=(',', ':'))}\n\n".encode("utf-8"))
        await writer.drain()
    except (ConnectionError, asyncio.CancelledError):
      raise
    finally:
      self.subscribers.remove(queue)
      writer.close()

  async def serve(self) -> None:
    server = await asyncio.start_server(self.handle, "127.0.0.1", 0)
    port = server.sockets[0].getsockname()[1]
    print(f"server listening on http://127.0.0.1:{port}", flush=True)
    async with server:
      await server.serve_forever()


def main() -> int:
  name = sys.argv[1]
  agent_dir = pathlib.Path.home() / "fake-agent"
  agent_dir.mkdir(parents=True, exist_ok=True)
  # A stream-json stdin stays open until the turn's result, so its lines are read as they arrive; any other stdin is
  # one prompt that ends at EOF, read whole before the spawn is recorded.
  stdin_head = _read_stdin_line()
  stream_json = _is_stream_json(stdin_head)
  if stream_json:
    stdin_bytes = stdin_head
    _echo_user_line(stdin_head)
    stdin_echo = _StdinEcho()
    stdin_echo.start()
  else:
    stdin_bytes = stdin_head + _read_stdin_rest()
  index, scripted, entry = _start_index(agent_dir, name, stdin_bytes)
  if not scripted:
    print(f"{name}: parity stand-in, not an agent CLI", file=sys.stderr)
    exit_code = 1
  elif name == "opencode" and sys.argv[2:3] == ["serve"]:
    try:
      asyncio.run(FakeOpenCodeServer(agent_dir, index, entry).serve())
    except Exception as error:
      print(f"{name}: {type(error).__name__}: {error}", file=sys.stderr)
      _write_exit(agent_dir, index, 2)
      return 2
    return 0
  else:
    try:
      exit_code = _run_steps(entry, agent_dir, index)
    except AgentDirGone as error:
      # The directory is gone, so the exit marker has nowhere to go: the start ends here.
      print(f"{name}: {error}", file=sys.stderr)
      return 2
    except MissingArgvFlag as error:
      print(f"{name}: {error}", file=sys.stderr)
      exit_code = 2
    except Exception as error:
      print(f"{name}: {type(error).__name__}: {error}", file=sys.stderr)
      exit_code = 2
    if stream_json:
      stdin_echo.join()
  _write_exit(agent_dir, index, exit_code)
  return exit_code


if __name__ == "__main__":
  # Exit without interpreter finalization. A stream-json stdin thread can still be blocked in a read of the buffered
  # stdin, and finalization then aborts with "could not acquire lock for <stdin>" instead of the exit status.
  status = main()
  sys.stdout.flush()
  sys.stderr.flush()
  os._exit(status)
