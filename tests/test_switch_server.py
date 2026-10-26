"""The switch command (scripts/switch-server.sh) against fake servers.

Each test builds its own runtime clone, home, release root and log directory under tmp_path, so nothing touches
~/.charliebot, ~/charliebot-releases, the default tmux server or the live port. FAKE_SERVER is one program that stands
in for both implementations: as the clone's server.py it is the Python server, as a release's charliebot-server it is
the Rust binary, and the switch tells them apart by argv. Its behavior comes from a behavior.json beside it.

The preflight refusals stop before any pane is looked up, so they need no tmux. The steps after the preflight drive a
pane on a private tmux server (tmux -L). The repo caps integration tests at 50 and already uses all 50, so those steps
share one test. The probe turn's cases run its functions in a shell against a fake server of their own, with one-second
waits, so each stays under the unit budget. The escalation cases run escalate_pane the same way, against stand-in
servers under a shell that plays the pane, so they need no tmux either.
"""

from __future__ import annotations

import ast
import contextlib
import dataclasses
import datetime
import hashlib
import json
import os
import pathlib
import re
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid
from collections.abc import Callable, Iterator

import pytest
import systemd_stubs
import yaml

from src.infra import config
from src.runtime.hooks import backend_type_registration

ROOT = pathlib.Path(__file__).resolve().parents[1]
SWITCH = ROOT / "scripts" / "switch-server.sh"
LAUNCHER = ROOT / "scripts" / "start-server.sh"
RUST_SHA = hashlib.sha1(b"rust-backend release under test").hexdigest()
ACCESS_KEY = "switch-test-key"
SESSION = "11111111-2222-3333-4444-555555555555"
# The fake server's probe run: run_state is its state before a stop; output is none, message (a committed assistant
# message) or tool (a tool call in the open draft); output_after_polls counts the bootstrap reads that see no output;
# cancel is settles (a stop ends the run) or hangs (it does not); followup is none or nudge, which starts a second run
# in the probe session at the first archive, as the web reply audit does after a turn, and that archive answers 409.
BEHAVIOR_DEFAULTS = {
    "check_config": "ok",
    "adopt": True,
    "run_state": "running",
    "output": "message",
    "output_after_polls": 0,
    "cancel": "settles",
    "followup": "none",
    "message_status": 202,
    "archive_runs_delay": 0,
    "ignore_sigterm": False,
}
GIT_ENV = {
    **os.environ,
    "GIT_AUTHOR_NAME": "switch-test",
    "GIT_AUTHOR_EMAIL": "switch-test@example.invalid",
    "GIT_COMMITTER_NAME": "switch-test",
    "GIT_COMMITTER_EMAIL": "switch-test@example.invalid",
}
DIRECT_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))

# One program for both implementations. Its routes answer the switch's readback and probe; at startup it logs a recovery
# line for every live Run record (when it adopts) and server_ready, in the format of the implementation it stands in
# for: console lines for server.py, JSON lines for the charliebot-server binary. Each request past the version probe is
# appended to calls.log beside the program, as "METHOD PATH".
FAKE_SERVER = r'''#!/usr/bin/env python3
"""Stand-in CharlieBot server: the routes and log lines the switch reads, behavior from behavior.json."""
import datetime
import http.server
import json
import os
import pathlib
import re
import signal
import sys
import time

HERE = pathlib.Path(__file__).resolve().parent
BEHAVIOR = json.loads((HERE / "behavior.json").read_text(encoding="utf-8"))
RUST_LOGS = pathlib.Path(__file__).name == "charliebot-server"
HOME = pathlib.Path(os.environ["CHARLIEBOT_HOME"])
CONFIG = (HOME / "config.yaml").read_text(encoding="utf-8")
PORT = int(re.search(r"^\s*port:\s*(\d+)", CONFIG, re.MULTILINE).group(1))
KEY = re.search(r"access_key:\s*(\S+)", (HOME / "credentials.yaml").read_text(encoding="utf-8")).group(1)
PROBE_SESSION = "probe-session"
TERMINAL = {"success", "failed", "interrupted", "stopped"}
# The session's runs, oldest first. The probe's message starts the probe run in behavior.run_state; behavior.followup
# "nudge" adds the follow-up run "nudge-run" at the first archive. READS counts the bootstrap reads.
RUNS = []
READS = {"bootstrap": 0}


def log(event, **fields):
  if RUST_LOGS:
    stamp = datetime.datetime.now(datetime.UTC).isoformat()
    record = {"event": event, **fields, "level": "info", "timestamp": stamp}
    print(json.dumps(record, separators=(",", ":")), flush=True)
  else:
    stamp = datetime.datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    detail = "".join(f" {key}={value}" for key, value in fields.items())
    print(f"{stamp} [info     ] {event}{detail}", flush=True)


if "--check-config" in sys.argv:
  if BEHAVIOR["check_config"] == "fail":
    print("config error: unknown key server.bogus", file=sys.stderr)
    sys.exit(1)
  sys.exit(0)

if BEHAVIOR["ignore_sigterm"]:
  signal.signal(signal.SIGTERM, signal.SIG_IGN)


class Handler(http.server.BaseHTTPRequestHandler):
  def do_GET(self):
    self._route("GET")

  def do_POST(self):
    self._route("POST")

  def do_DELETE(self):
    self._route("DELETE")

  def _route(self, method):
    request_body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
    request_json = json.loads(request_body) if request_body else {}
    if self.headers.get("Authorization") != f"Bearer {KEY}":
      return self._reply(401, {"detail": "unauthorized"})
    if method == "GET" and self.path == "/api/internal/version":
      return self._reply(200, {"ok": True})
    with (HERE / "calls.log").open("a", encoding="utf-8") as calls:
      calls.write(f"{method} {self.path}\n")
    if method == "POST" and self.path == "/api/sessions/":
      return self._reply(200, {"id": PROBE_SESSION})
    if method == "POST" and re.fullmatch(r"/api/chat/[^/]+/message", self.path):
      if not request_json.get("request_id"):
        return self._reply(422, {"detail": "request_id is required"})
      if BEHAVIOR["message_status"] != 202:
        return self._reply(BEHAVIOR["message_status"], {"detail": "message refused"})
      RUNS.append({"id": "probe-run", "state": BEHAVIOR["run_state"]})
      return self._reply(202, {"input_event_id": "probe-input"})
    if method == "GET" and re.fullmatch(r"/api/sessions/[^/]+/runs\?.*", self.path):
      if "order=desc" not in self.path:
        time.sleep(BEHAVIOR["archive_runs_delay"])
      items = list(reversed(RUNS)) if "order=desc" in self.path else list(RUNS)
      return self._reply(200, {"items": items, "next_cursor": None})
    if method == "GET" and self.path == f"/api/sessions/{PROBE_SESSION}/bootstrap":
      return self._reply(200, self._bootstrap())
    stop = re.fullmatch(rf"/api/sessions/{PROBE_SESSION}/runs/([^/]+)/cancel", self.path)
    if method == "POST" and stop:
      run = next((run for run in RUNS if run["id"] == stop[1]), None)
      if run is None:
        return self._reply(404, {"detail": "run not found"})
      if BEHAVIOR["cancel"] == "settles" and run["state"] not in TERMINAL:
        run["state"] = "interrupted"
      return self._reply(200, {"run_id": run["id"], "stop_requested": True, "outcome": None})
    if method == "POST" and self.path == f"/api/sessions/{PROBE_SESSION}/archive":
      if not request_json.get("request_id"):
        return self._reply(422, {"detail": "request_id is required"})
      if BEHAVIOR["followup"] == "nudge" and not any(run["id"] == "nudge-run" for run in RUNS):
        RUNS.append({"id": "nudge-run", "state": "running"})
      if any(run["state"] not in TERMINAL for run in RUNS):
        return self._reply(409, {"detail": "a run of the session is unfinished"})
      return self._reply(200, {"archived": [PROBE_SESSION]})
    return self._reply(404, {"detail": "not found"})

  def _bootstrap(self):
    READS["bootstrap"] += 1
    messages = [{"role": "user", "content": "Reply with the single word HELLO.", "id": "probe-user"}]
    draft = None
    if READS["bootstrap"] > BEHAVIOR["output_after_polls"]:
      if BEHAVIOR["output"] == "message":
        messages.append({"role": "assistant", "content": "HELLO", "id": "probe-reply"})
      elif BEHAVIOR["output"] == "tool":
        draft = {"role": "assistant", "content": "", "tools": [{"name": "Bash"}]}
    return {"messages": messages, "pending_draft": draft, "event_count": len(messages), "oldest_message_ordinal": 0,
            "has_more": False}

  def _reply(self, status, body):
    data = json.dumps(body).encode("utf-8")
    self.send_response(status)
    self.send_header("Content-Type", "application/json")
    self.send_header("Content-Length", str(len(data)))
    self.end_headers()
    self.wfile.write(data)

  def log_message(self, format, *args):
    pass


for meta_path in sorted(HOME.glob("sessions/*/data/runs/*/metadata.json")):
  meta = json.loads(meta_path.read_text(encoding="utf-8"))
  if meta["ended_at"] is None:
    if BEHAVIOR["adopt"]:
      log("task_recovery_resume", session_id=meta["session_id"], run_id=meta["id"])
    # Another event that names the run: the server's master log does, and it must not count as adoption.
    log("master_cc_resuming", session_id=meta["session_id"], run_id=meta["id"])
server = http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
log("server_ready", ready_in_ms=1)
server.serve_forever()
'''

# A uv stand-in: `uv sync` fails while the clone holds a fail-sync marker; `uv run CMD` runs CMD in place.
FAKE_UV = """\
#!/usr/bin/env python3
import os
import pathlib
import sys

if sys.argv[1] == "sync":
  sys.exit(1 if pathlib.Path("fail-sync").exists() else 0)
if sys.argv[1] == "run":
  os.execvp(sys.argv[2], sys.argv[2:])
sys.exit(2)
"""


@dataclasses.dataclass
class World:
  """One test's throwaway runtime clone, home, release root, log directory and tmux socket."""

  root: pathlib.Path
  clone: pathlib.Path
  home: pathlib.Path
  releases: pathlib.Path
  logs: pathlib.Path
  socket: str
  env: dict[str, str]
  base_head: str  # the runtime clone's HEAD before any switch; the PORT_BASELINE commit is one ahead
  baseline: str
  port: int
  live: list[subprocess.Popen[bytes]] = dataclasses.field(default_factory=list)


def _git(cwd: pathlib.Path, *args: str) -> str:
  return subprocess.run(["git", "-C", str(cwd), *args], capture_output=True, text=True, env=GIT_ENV,
                        check=True).stdout.strip()


def _write_behavior(directory: pathlib.Path, **overrides: object) -> None:
  (directory / "behavior.json").write_text(json.dumps({**BEHAVIOR_DEFAULTS, **overrides}), encoding="utf-8")


def _sha256(path: pathlib.Path) -> str:
  return hashlib.sha256(path.read_bytes()).hexdigest()


def _install_release(releases: pathlib.Path, baseline: str) -> None:
  """A release directory in the shape the host's build job leaves: binary, library, sums and PORT_BASELINE."""
  release = releases / RUST_SHA
  release.mkdir(parents=True)
  binary = release / "charliebot-server"
  binary.write_text(FAKE_SERVER, encoding="utf-8")
  binary.chmod(0o755)
  (release / "libfake.so").write_bytes(b"fake shared object")
  sums = [f"{_sha256(release / name)}  {name}\n" for name in ("charliebot-server", "libfake.so")]
  (release / "sha256sums.txt").write_text("".join(sums), encoding="utf-8")
  (release / "PORT_BASELINE").write_text(f"{baseline}\n", encoding="utf-8")
  _write_behavior(release)


def _free_port() -> int:
  with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    return sock.getsockname()[1]


def _build_world(root: pathlib.Path) -> World:
  clone = root / "clone"
  home = root / "home"
  releases = root / "releases"
  logs = root / "logs"
  bin_dir = root / "bin"
  for path in (clone / "scripts", home / "sessions", releases, logs, bin_dir):
    path.mkdir(parents=True)
  shutil.copy2(LAUNCHER, clone / "scripts" / "start-server.sh")
  shutil.copy2(SWITCH, clone / "scripts" / "switch-server.sh")
  # The launcher's preflight reads its expected limits from this directory.
  shutil.copytree(ROOT / "scripts" / "systemd", clone / "scripts" / "systemd")
  (clone / "server.py").write_text(FAKE_SERVER, encoding="utf-8")
  _git(clone, "init", "-q", "-b", "main")
  _git(clone, "add", "scripts", "server.py")
  _git(clone, "commit", "-q", "-m", "fake server")
  base_head = _git(clone, "rev-parse", "HEAD")
  _git(clone, "checkout", "-q", "-b", "baseline")
  _git(clone, "commit", "-q", "--allow-empty", "-m", "baseline")
  baseline = _git(clone, "rev-parse", "HEAD")
  _git(clone, "checkout", "-q", "main")
  _write_behavior(clone)
  _install_release(releases, baseline)

  (bin_dir / "uv").write_text(FAKE_UV, encoding="utf-8")
  (bin_dir / "uv").chmod(0o755)
  # The launcher's slice preflight and its systemd-run scope, without the host's user manager.
  systemd_stubs.install(bin_dir)
  port = _free_port()
  (home / "config.yaml").write_text(
      yaml.safe_dump(
          {
              "server": {
                  "host": "127.0.0.1",
                  "port": port
              },
              "backends":
                  {
                      "options":
                          [
                              {
                                  "id": "oc-deepseek-flash",
                                  "type": "opencode"
                              },
                              {
                                  "id": "agy-gemini-flash",
                                  "type": "antigravity"
                              },
                              {
                                  "id": "claude-opus",
                                  "type": "cc-claude"
                              },
                          ]
                  },
          }),
      encoding="utf-8")
  (home / "credentials.yaml").write_text(f"charliebot:\n  access_key: {ACCESS_KEY}\n", encoding="utf-8")

  env = {
      **os.environ,
      "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
      "CHARLIEBOT_HOME": str(home),
      "CHARLIEBOT_RELEASES_DIR": str(releases),
      "CHARLIEBOT_LOG_DIR": str(logs),
      "CHARLIEBOT_TMUX_SOCKET": f"switch-test-{uuid.uuid4().hex[:8]}",
      "CHARLIEBOT_SWITCH_STOP_WAIT_S": "3",
      "CHARLIEBOT_SWITCH_READY_WAIT_S": "30",
  }
  env.pop("TMUX", None)
  env.pop("SYSTEMD_RUN_REFUSE_ALL", None)
  return World(
      root=root,
      clone=clone,
      home=home,
      releases=releases,
      logs=logs,
      socket=env["CHARLIEBOT_TMUX_SOCKET"],
      env=env,
      base_head=base_head,
      baseline=baseline,
      port=port)


@pytest.fixture
def world(tmp_path: pathlib.Path) -> Iterator[World]:
  built = _build_world(tmp_path)
  yield built
  subprocess.run(["tmux", "-L", built.socket, "kill-server"], env=built.env, capture_output=True, check=False)
  # tmux leaves the socket file behind after kill-server; with the server gone it is an empty stale file.
  socket_path = pathlib.Path(built.env.get("TMUX_TMPDIR", "/tmp")) / f"tmux-{os.getuid()}" / built.socket
  socket_path.unlink(missing_ok=True)
  for proc in built.live:
    proc.kill()
    proc.wait()


def _switch(world: World, *args: str) -> subprocess.CompletedProcess[str]:
  """Run the clone's own switch command, as the operator does from the runtime clone."""
  return subprocess.run(
      ["bash", str(world.clone / "scripts" / "switch-server.sh"), *args],
      env=world.env,
      capture_output=True,
      text=True,
      timeout=120,
      check=False)


def _report(world: World, proc: subprocess.CompletedProcess[str]) -> str:
  logs = sorted(world.logs.glob("switch_*.log"))
  switch_log = logs[-1].read_text(encoding="utf-8") if logs else "(no switch log)"
  return f"rc={proc.returncode}\nstdout:\n{proc.stdout}\nstderr:\n{proc.stderr}\nswitch log:\n{switch_log}"


def _spawn_live(world: World) -> subprocess.Popen[bytes]:
  proc = subprocess.Popen(["sleep", "300"], start_new_session=True)
  world.live.append(proc)
  return proc


def _pid_start(pid: int) -> str:
  """Field 22 of /proc/<pid>/stat, the start time that pins one process instance."""
  stat = pathlib.Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
  return stat.rsplit(")", 1)[1].split()[19]


def _seed_live_run(world: World, run_id: str, backend: str | None) -> None:
  """A Run record whose process is alive, as the server left it before a restart."""
  proc = _spawn_live(world)
  record = {
      "id": run_id,
      "session_id": SESSION,
      "kind": "work",
      "pid": proc.pid,
      "pid_start": _pid_start(proc.pid),
      "started_at": datetime.datetime.now(datetime.UTC).isoformat(),
      "ended_at": None,
  }
  if backend is not None:
    record["backend"] = backend
  run_dir = world.home / "sessions" / SESSION / "data" / "runs" / run_id
  run_dir.mkdir(parents=True)
  (run_dir / "metadata.json").write_text(json.dumps(record), encoding="utf-8")


def _seed_improve_loop(world: World, controller_pid: int) -> None:
  """An improve loop marked running under a live controller process."""
  loop_dir = world.home / "sessions" / SESSION / "loops" / "1"
  loop_dir.mkdir(parents=True)
  (loop_dir / "state.json").write_text(
      json.dumps({
          "status": "running",
          "server_pid": controller_pid
      }), encoding="utf-8")


def _tmux(world: World, *args: str) -> str:
  return subprocess.run(["tmux", "-L", world.socket, *args], env=world.env, capture_output=True, text=True,
                        check=True).stdout.strip()


def _start_pane(world: World) -> str:
  return _tmux(
      world, "new-session", "-d", "-s", "switch", "-c", str(world.clone), "-P", "-F", "#{pane_id}",
      "bash --noprofile --norc")


def _latest_log(world: World) -> str:
  latest = world.logs / "server-latest.log"
  return latest.read_text(encoding="utf-8", errors="replace") if latest.exists() else ""


def _wait_until(predicate: Callable[[], bool], timeout_s: float) -> None:
  deadline = time.monotonic() + timeout_s
  while not predicate():
    if time.monotonic() > deadline:
      pytest.fail(f"condition not met within {timeout_s}s")
    time.sleep(0.05)


def _start_server(world: World, pane: str) -> None:
  """Start the server in the pane through the clone's launcher, as the operator does."""
  command = (
      f"cd {shlex.quote(str(world.clone))} && env CHARLIEBOT_HOME={shlex.quote(str(world.home))} "
      f"CHARLIEBOT_RELEASES_DIR={shlex.quote(str(world.releases))} "
      f"CHARLIEBOT_LOG_DIR={shlex.quote(str(world.logs))} ./scripts/start-server.sh")
  _tmux(world, "send-keys", "-t", pane, command, "Enter")
  _wait_until(lambda: "server_ready" in _latest_log(world), 30)


def _listener_pids(port: int) -> set[int]:
  out = subprocess.run(["ss", "-Hltnp", f"sport = :{port}"], capture_output=True, text=True, check=True).stdout
  return {int(pid) for pid in re.findall(r"pid=(\d+)", out)}


def _listener_argv(world: World) -> list[pathlib.Path]:
  pids = _listener_pids(world.port)
  assert len(pids) == 1, f"expected one listener on port {world.port}, found {pids}"
  raw = pathlib.Path(f"/proc/{pids.pop()}/cmdline").read_bytes()
  return [pathlib.Path(arg.decode()) for arg in raw.split(b"\0") if arg]


def _serves_python(world: World) -> bool:
  return any(arg.name == "server.py" for arg in _listener_argv(world))


def _serves_release(world: World, sha: str) -> bool:
  binary = (world.releases / sha / "charliebot-server").resolve()
  return any(arg.resolve() == binary for arg in _listener_argv(world))


def _version_status(port: int) -> int:
  request = urllib.request.Request(
      f"http://127.0.0.1:{port}/api/internal/version", headers={"Authorization": f"Bearer {ACCESS_KEY}"})
  try:
    with DIRECT_OPENER.open(request, timeout=5) as response:
      return response.status
  except urllib.error.HTTPError as error:
    return error.code


def _assert_untouched(world: World) -> None:
  """Nothing changed: no current link, the clone where it was, and the Python server still answering."""
  assert not (world.releases / "current").is_symlink()
  assert _git(world.clone, "rev-parse", "HEAD") == world.base_head
  assert _serves_python(world)
  assert _version_status(world.port) == 200


def _assert_server_scope_records(world: World, expected_count: int) -> None:
  records = systemd_stubs.read_systemd_run_records(world.root / "bin")
  server_records = [record for record in records if str(record["unit_name"]).startswith("charliebot-server-")]
  names = [str(record["unit_name"]) for record in server_records]
  assert len(names) == expected_count
  assert len(set(names)) == len(names)
  for record in server_records:
    name = str(record["unit_name"])
    assert re.fullmatch(r"charliebot-server-\d{8}_\d{6}-\d+", name)
    arguments = record["arguments"]
    assert isinstance(arguments, list)
    assert f"--unit={name}" in arguments
    assert "--slice=charliebot.slice" in arguments
    assert arguments[arguments.index("-p") + 1] == "MemorySwapMax=0"


def _home_helper(home: pathlib.Path, mode: str) -> subprocess.CompletedProcess[str]:
  return subprocess.run(
      ["bash", "-c", 'source "$1" python && home_helper "$2"', "bash",
       str(SWITCH), mode],
      env={
          **os.environ, "CHARLIEBOT_HOME": str(home)
      },
      capture_output=True,
      text=True,
      timeout=5,
      check=True)


@pytest.mark.parametrize("server", [{"host": "127.0.0.1"}, None], ids=["host-only", "absent"])
def test_home_helper_config_uses_server_default(world: World, server: dict[str, str] | None) -> None:
  (world.home / "config.yaml").write_text(
      yaml.safe_dump({"server": server} if server is not None else {}), encoding="utf-8")

  proc = _home_helper(world.home, "config")

  assert int(proc.stdout.splitlines()[0]) == config.ServerConfig().port


def test_home_helper_config_uses_explicit_port(world: World) -> None:
  proc = _home_helper(world.home, "config")

  assert int(proc.stdout.splitlines()[0]) == world.port


def test_home_helper_no_reattach_matches_registered_traits() -> None:
  declaration = re.search(r"^NO_REATTACH = (.+)$", SWITCH.read_text(encoding="utf-8"), re.MULTILINE)
  assert declaration is not None
  assert ast.literal_eval(declaration[1]) == {
      backend_type for backend_type, registration in backend_type_registration.registrations().items()
      if not registration.traits.restart_reattach
  }


@pytest.mark.parametrize(
    "case", [
        "checksum", "check-config", "opencode-run", "antigravity-run", "unknown-backend-run", "missing-backend-run",
        "improve-loop", "dirty-clone", "launcher-preflight"
    ])
def test_preflight_refusal_exits_1_and_changes_nothing(world: World, case: str) -> None:
  """Each preflight check refuses before the switch stops anything: exit 1 and no change."""
  release = world.releases / RUST_SHA
  if case == "checksum":
    (release / "charliebot-server").write_text(FAKE_SERVER + "# tampered\n", encoding="utf-8")
  elif case == "check-config":
    _write_behavior(release, check_config="fail")
  elif case in {"opencode-run", "antigravity-run", "unknown-backend-run", "missing-backend-run"}:
    backend = {
        "opencode-run": "oc-deepseek-flash",
        "antigravity-run": "agy-gemini-flash",
        "unknown-backend-run": "oc-retired",
        "missing-backend-run": None,
    }[case]
    _seed_live_run(world, "run-blocked", backend)
  elif case == "improve-loop":
    _seed_improve_loop(world, _spawn_live(world).pid)
  elif case == "dirty-clone":
    (world.clone / "server.py").write_text("# local edit\n", encoding="utf-8")
  elif case == "launcher-preflight":
    world.env["SYSTEMD_RUN_REFUSE_ALL"] = "1"

  home_before = {path.relative_to(world.home): path.read_bytes() for path in world.home.rglob("*") if path.is_file()}
  current = world.releases / "current"
  current_before = os.readlink(current) if current.is_symlink() else None

  proc = _switch(world, "rust", RUST_SHA)

  assert proc.returncode == 1, _report(world, proc)
  assert not (world.releases / "current").is_symlink()
  assert (os.readlink(current) if current.is_symlink() else None) == current_before
  assert _git(world.clone, "rev-parse", "HEAD") == world.base_head
  assert {
      path.relative_to(world.home): path.read_bytes() for path in world.home.rglob("*") if path.is_file()
  } == home_before
  assert all(live.poll() is None for live in world.live)
  if case == "launcher-preflight":
    assert "launcher preflight" in proc.stdout + proc.stderr, _report(world, proc)
  if case.endswith("-run"):
    assert "a restart would lose live work:" in proc.stdout, _report(world, proc)
    lines = _home_helper(world.home, "scan").stdout.splitlines()
    assert [line for line in lines if line.startswith("live\t")
           ] == [f"live\t{SESSION}\trun-blocked\t{backend}\t{world.live[0].pid}"]
    blocks = [line for line in lines if line.startswith("block\t")]
    assert len(blocks) == 1
    assert "run-blocked" in blocks[0]
    assert str(backend) in blocks[0]
    if case in {"opencode-run", "antigravity-run"}:
      assert case.removesuffix("-run") in blocks[0]
      assert str(world.live[0].pid) in blocks[0]


ProbeRun = Callable[..., tuple[subprocess.CompletedProcess[str], list[str]]]
PROBE_BOOTSTRAP = "GET /api/sessions/probe-session/bootstrap"
PROBE_CANCEL = "POST /api/sessions/probe-session/runs/probe-run/cancel"
PROBE_ARCHIVE = "POST /api/sessions/probe-session/archive"


def _answers_version(port: int) -> bool:
  try:
    return _version_status(port) == 200
  except OSError:
    return False


@pytest.fixture
def probe(tmp_path: pathlib.Path) -> Iterator[ProbeRun]:
  """The switch's probe turn, run in a shell against a fake server that answers with the behavior the test gives.

  Each call starts its own fake server, so a test calls the runner once. The runner returns the probe's result and the
  requests the fake served, as "METHOD PATH" lines. Both probe waits are one second.
  """
  home = tmp_path / "home"
  home.mkdir()
  port = _free_port()
  (home / "config.yaml").write_text(f"server:\n  host: 127.0.0.1\n  port: {port}\n", encoding="utf-8")
  (home / "credentials.yaml").write_text(f"charliebot:\n  access_key: {ACCESS_KEY}\n", encoding="utf-8")
  servers: list[subprocess.Popen[bytes]] = []

  def run(**behavior: object) -> tuple[subprocess.CompletedProcess[str], list[str]]:
    server_dir = tmp_path / "server"
    server_dir.mkdir()
    (server_dir / "server.py").write_text(FAKE_SERVER, encoding="utf-8")
    _write_behavior(server_dir, **behavior)
    env = {
        **os.environ,
        "CHARLIEBOT_HOME": str(home),
        "CHARLIEBOT_SWITCH_PROBE_WAIT_S": "1",
        "CHARLIEBOT_SWITCH_PROBE_CANCEL_WAIT_S": "1",
    }
    servers.append(
        subprocess.Popen(
            [sys.executable, str(server_dir / "server.py")],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL))
    _wait_until(lambda: _answers_version(port), 10)
    work = tmp_path / "work"
    work.mkdir()
    probe_env = {**env, "PORT": str(port), "KEY": ACCESS_KEY, "WORK": str(work)}
    probe_cmd = ["bash", "-c", 'source "$1" python && probe_turn', "bash", str(SWITCH)]
    proc = subprocess.run(probe_cmd, env=probe_env, capture_output=True, text=True, timeout=30, check=False)
    calls_log = server_dir / "calls.log"
    calls = calls_log.read_text(encoding="utf-8").splitlines() if calls_log.exists() else []
    return proc, calls

  yield run
  for server in servers:
    server.kill()
    server.wait()


def _assert_stage_lines(proc: subprocess.CompletedProcess[str]) -> None:
  """Each probe stage logs one line that names its session and its run and gives the elapsed seconds."""
  stage_lines = [line for line in proc.stdout.splitlines() if "switch: probe: " in line and " session=" in line]
  assert stage_lines, proc.stdout
  for line in stage_lines:
    assert re.search(r"probe: .* session=probe-session run=\S+ elapsed=\d+s$", line), line


def test_probe_passes_on_first_output_then_stops_the_run_and_archives(probe: ProbeRun) -> None:
  """Output on the second read: the probe stops the run, the stop settles it, and the session is archived."""
  proc, calls = probe(run_state="running", output="message", output_after_polls=1, cancel="settles")
  assert proc.returncode == 0, proc.stdout + proc.stderr
  assert PROBE_CANCEL in calls
  assert calls[-1] == PROBE_ARCHIVE
  _assert_stage_lines(proc)


def test_probe_passes_without_a_cancel_when_the_run_ends_success(probe: ProbeRun) -> None:
  proc, calls = probe(run_state="success", output="none")
  assert proc.returncode == 0, proc.stdout + proc.stderr
  assert not any(line.endswith("/cancel") for line in calls)
  assert calls[-1] == PROBE_ARCHIVE
  _assert_stage_lines(proc)


def test_probe_fails_at_once_when_the_run_fails_before_any_output(probe: ProbeRun) -> None:
  proc, calls = probe(run_state="failed", output="none")
  assert proc.returncode == 1, proc.stdout + proc.stderr
  assert calls.count(PROBE_BOOTSTRAP) == 1  # the failed run ends the wait on its first read
  assert not any(line.endswith("/cancel") for line in calls)
  assert calls[-1] == PROBE_ARCHIVE
  _assert_stage_lines(proc)


def test_probe_fails_and_stops_the_run_when_no_output_comes_within_the_wait(probe: ProbeRun) -> None:
  proc, calls = probe(run_state="running", output="none")
  assert proc.returncode == 1, proc.stdout + proc.stderr
  assert PROBE_CANCEL in calls
  assert calls[-1] == PROBE_ARCHIVE
  _assert_stage_lines(proc)


def test_probe_fails_when_the_stop_does_not_settle_within_its_wait(probe: ProbeRun) -> None:
  proc, calls = probe(run_state="running", output="tool", cancel="hangs")
  assert proc.returncode == 1, proc.stdout + proc.stderr
  assert PROBE_CANCEL in calls
  _assert_stage_lines(proc)


NUDGE_CANCEL = "POST /api/sessions/probe-session/runs/nudge-run/cancel"


def test_probe_archive_stops_the_follow_up_run_that_refuses_the_archive(probe: ProbeRun) -> None:
  """The probe run ends success, then the server starts a follow-up run; the archive's 409 sends the probe to stop it."""
  proc, calls = probe(run_state="success", output="none", followup="nudge", cancel="settles")
  assert proc.returncode == 0, proc.stdout + proc.stderr
  assert calls.count(PROBE_ARCHIVE) == 2  # the first archive answers 409
  assert calls.index(NUDGE_CANCEL) > calls.index(PROBE_ARCHIVE)
  assert calls[-1] == PROBE_ARCHIVE
  assert "probe: archive answered 409" in proc.stdout
  assert re.search(r"probe: stop requested session=probe-session run=nudge-run elapsed=\d+s", proc.stdout)
  _assert_stage_lines(proc)


def test_probe_archive_fails_naming_the_follow_up_run_that_does_not_settle(probe: ProbeRun) -> None:
  """A follow-up run that the stop cannot end holds the archive: the probe fails and logs the run it waited for."""
  proc, calls = probe(run_state="success", output="none", followup="nudge", cancel="hangs")
  assert proc.returncode == 1, proc.stdout + proc.stderr
  assert calls.count(PROBE_ARCHIVE) == 1  # refused once; the stop timed out, so there is no second archive
  assert re.search(r"probe: run did not settle within 1s session=probe-session run=nudge-run elapsed=\d+s", proc.stdout)
  _assert_stage_lines(proc)


def test_probe_archive_does_not_cancel_an_empty_run_id(probe: ProbeRun) -> None:
  """A refused message leaves no run to stop, but the probe still archives the empty session."""
  proc, calls = probe(message_status=500)
  assert proc.returncode == 1, proc.stdout + proc.stderr
  assert calls[-1] == PROBE_ARCHIVE
  assert not any(line.endswith("/cancel") for line in calls)
  assert "probe: archived session=probe-session" in proc.stdout
  _assert_stage_lines(proc)


def test_probe_archive_bounds_a_slow_run_list_by_its_wait(probe: ProbeRun) -> None:
  """A stalled archive request times out within PROBE_CANCEL_WAIT_S."""
  start = time.monotonic()
  proc, calls = probe(run_state="success", output="none", archive_runs_delay=2)
  elapsed = time.monotonic() - start
  assert proc.returncode == 1, proc.stdout + proc.stderr
  assert elapsed < 1.8
  assert any(line.startswith("GET /api/sessions/probe-session/runs?limit=500") for line in calls)
  assert "probe: listing the runs answered 000" in proc.stdout
  _assert_stage_lines(proc)


# A stand-in for a server under a pane. The escalation finds it by its argv, which names server.py; stubborn mode
# ignores SIGTERM.
STAND_IN_SERVER = """\
import signal
import sys
import time

if sys.argv[1] == "stubborn":
  signal.signal(signal.SIGTERM, signal.SIG_IGN)
time.sleep(300)
"""


def _children(pid: int) -> list[int]:
  out = subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True, check=False).stdout
  return [int(child) for child in out.split()]


def _server_under(pid: int) -> int:
  """The stand-in server forked under the pane shell, once exec has run and its argv names server.py."""

  def exec_done() -> bool:
    return any(b"server.py" in pathlib.Path(f"/proc/{child}/cmdline").read_bytes() for child in _children(pid))

  _wait_until(exec_done, 10)
  return _children(pid)[0]


@pytest.fixture
def pane(tmp_path: pathlib.Path) -> Iterator[Callable[[str], subprocess.Popen[bytes]]]:
  """Starts a pane shell whose one child is a stand-in server in the given mode; teardown kills what is left."""
  shells: list[subprocess.Popen[bytes]] = []

  def start(mode: str) -> subprocess.Popen[bytes]:
    directory = tmp_path / f"pane{len(shells)}"
    directory.mkdir()
    (directory / "server.py").write_text(STAND_IN_SERVER, encoding="utf-8")
    shell = subprocess.Popen(
        ["bash", "-c", '"$1" "$2" "$3"; true', "bash", sys.executable,
         str(directory / "server.py"), mode])
    shells.append(shell)
    return shell

  yield start
  for shell in shells:
    for child in _children(shell.pid):
      with contextlib.suppress(ProcessLookupError):
        os.kill(child, signal.SIGKILL)
    shell.kill()
    shell.wait()


def _escalate(tmp_path: pathlib.Path, shell_pid: int) -> subprocess.CompletedProcess[str]:
  """The switch's escalate_pane against the pane shell, with a one-second SIGTERM wait."""
  # impl_of_pid reads RELEASES_REAL, which run_detached sets; a sourced copy of the script needs it set too.
  env = {
      **os.environ,
      "PANE": "%escalation",
      "PANE_PID": str(shell_pid),
      "RELEASES_REAL": str(tmp_path / "releases"),
      "CHARLIEBOT_SWITCH_STOP_WAIT_S": "1",
  }
  return subprocess.run(
      ["bash", "-c", 'source "$1" python && escalate_pane', "bash",
       str(SWITCH)],
      env=env,
      capture_output=True,
      text=True,
      timeout=30,
      check=False)


def _logged_with(output: str, signal_name: str, pid: int) -> bool:
  """Whether one escalation log line names the signal and the PID it was sent to."""
  return any(signal_name in line and re.search(rf"\b{pid}\b", line) for line in output.splitlines())


def test_escalation_stops_a_server_under_the_pane_with_sigterm(
    pane: Callable[[str], subprocess.Popen[bytes]], tmp_path: pathlib.Path) -> None:
  """A server that honors SIGTERM is stopped by SIGTERM alone, and the pane then goes idle."""
  shell = pane("honest")
  server = _server_under(shell.pid)
  proc = _escalate(tmp_path, shell.pid)
  assert proc.returncode == 0, proc.stdout + proc.stderr
  assert _logged_with(proc.stdout, "SIGTERM", server), proc.stdout
  assert "SIGKILL" not in proc.stdout
  assert shell.wait(timeout=5) == 0


def test_escalation_sigkills_a_survivor_and_spares_a_server_outside_the_pane(
    pane: Callable[[str], subprocess.Popen[bytes]], tmp_path: pathlib.Path) -> None:
  """A server that ignores SIGTERM gets SIGKILL after the SIGTERM wait; a server outside the pane gets no signal."""
  shell = pane("stubborn")
  server = _server_under(shell.pid)
  outside = tmp_path / "outside"
  outside.mkdir()
  (outside / "server.py").write_text(STAND_IN_SERVER, encoding="utf-8")
  bystander = subprocess.Popen([sys.executable, str(outside / "server.py"), "honest"])
  try:
    proc = _escalate(tmp_path, shell.pid)
    assert bystander.poll() is None, "a server outside the pane was signaled"
  finally:
    bystander.kill()
    bystander.wait()
  assert proc.returncode == 0, proc.stdout + proc.stderr
  assert _logged_with(proc.stdout, "SIGTERM", server), proc.stdout
  assert _logged_with(proc.stdout, "SIGKILL", server), proc.stdout
  assert shell.wait(timeout=5) == 0


@pytest.mark.integration
def test_switch_on_a_real_pane_restores_then_round_trips_then_exits_3(world: World) -> None:
  """Every pane-driven exit in one tmux server: exit 2 after a failed readback, exit 0 each way, exit 3 on sync failure,
  with a SIGKILL escalation on the last stop.

  The steps share one test because the integration cap leaves a single slot. Each step builds on the state the
  previous one leaves, so the order is the test.
  """
  # Stops wait 1 s here: the final switch stops a Python server that ignores SIGTERM, which exercises the SIGKILL
  # escalation within the budget.
  world.env["CHARLIEBOT_SWITCH_STOP_WAIT_S"] = "1"
  pane = _start_pane(world)
  _start_server(world, pane)
  _seed_live_run(world, "run-live", "claude-opus")

  # The Rust readback cannot adopt the live run: the switch fails after it stopped Python and restores Python.
  _write_behavior(world.releases / RUST_SHA, adopt=False)
  proc = _switch(world, "rust", RUST_SHA)
  assert proc.returncode == 2, _report(world, proc)
  _assert_server_scope_records(world, 3)
  _assert_untouched(world)

  # With a passing readback the live run is adopted, and the clone fast-forwards to the PORT_BASELINE commit.
  _write_behavior(world.releases / RUST_SHA)
  proc = _switch(world, "rust", RUST_SHA)
  assert proc.returncode == 0, _report(world, proc)
  _assert_server_scope_records(world, 4)
  assert os.readlink(world.releases / "current") == RUST_SHA
  assert _git(world.clone, "rev-parse", "HEAD") == world.baseline
  assert _serves_release(world, RUST_SHA)
  assert _version_status(world.port) == 200
  assert '"run_id":"run-live"' in _latest_log(world)

  # The way back needs no fast-forward: the clone stays at the baseline and current is removed. The Python server it
  # starts ignores SIGTERM, so the next switch has to SIGKILL it.
  _write_behavior(world.clone, ignore_sigterm=True)
  proc = _switch(world, "python")
  assert proc.returncode == 0, _report(world, proc)
  _assert_server_scope_records(world, 5)
  assert not (world.releases / "current").is_symlink()
  assert _git(world.clone, "rev-parse", "HEAD") == world.baseline
  assert _serves_python(world)

  # A sync that fails again during the restore leaves the server down, which is exit 3.
  (world.clone / "fail-sync").write_text("", encoding="utf-8")
  proc = _switch(world, "rust", RUST_SHA)
  assert proc.returncode == 3, _report(world, proc)
  _assert_server_scope_records(world, 5)
  assert _listener_pids(world.port) == set()
