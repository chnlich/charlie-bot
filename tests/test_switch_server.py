"""The switch command (scripts/switch-server.sh) against fake servers.

Each test builds its own runtime clone, home, release root and log directory under tmp_path, so nothing touches
~/.charliebot, ~/charliebot-releases, the default tmux server or the live port. FAKE_SERVER is one program that stands
in for both implementations: as the clone's server.py it is the Python server, as a release's charliebot-server it is
the Rust binary, and the switch tells them apart by argv. Its behavior comes from a behavior.json beside it.

The preflight refusals stop before any pane is looked up, so they need no tmux. The steps after the preflight drive a
pane on a private tmux server (tmux -L). The repo caps integration tests at 50 and already uses 49, so those steps
share one test.
"""

from __future__ import annotations

import dataclasses
import datetime
import hashlib
import json
import os
import pathlib
import re
import shlex
import shutil
import socket
import subprocess
import time
import urllib.error
import urllib.request
import uuid
from collections.abc import Callable, Iterator

import pytest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SWITCH = ROOT / "scripts" / "switch-server.sh"
LAUNCHER = ROOT / "scripts" / "start-server.sh"
RUST_SHA = hashlib.sha1(b"rust-backend release under test").hexdigest()
ACCESS_KEY = "switch-test-key"
SESSION = "11111111-2222-3333-4444-555555555555"
BEHAVIOR_DEFAULTS = {"check_config": "ok", "adopt": True, "probe": "success", "ignore_sigterm": False}
GIT_ENV = {
    **os.environ,
    "GIT_AUTHOR_NAME": "switch-test",
    "GIT_AUTHOR_EMAIL": "switch-test@example.invalid",
    "GIT_COMMITTER_NAME": "switch-test",
    "GIT_COMMITTER_EMAIL": "switch-test@example.invalid",
}
DIRECT_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))

# One program for both implementations. Its routes answer the switch's readback and probe; it logs the recovery line
# for every Run record without ended_at and then server_ready, as the real server does on startup.
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

HERE = pathlib.Path(__file__).resolve().parent
BEHAVIOR = json.loads((HERE / "behavior.json").read_text(encoding="utf-8"))
HOME = pathlib.Path(os.environ["CHARLIEBOT_HOME"])
CONFIG = (HOME / "config.yaml").read_text(encoding="utf-8")
PORT = int(re.search(r"^\s*port:\s*(\d+)", CONFIG, re.MULTILINE).group(1))
KEY = re.search(r"access_key:\s*(\S+)", (HOME / "credentials.yaml").read_text(encoding="utf-8")).group(1)


def log(event, **fields):
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

  def _route(self, method):
    self.rfile.read(int(self.headers.get("Content-Length") or 0))
    if self.headers.get("Authorization") != f"Bearer {KEY}":
      return self._reply(401, {"detail": "unauthorized"})
    if method == "GET" and self.path == "/api/internal/version":
      return self._reply(200, {"ok": True})
    if method == "POST" and self.path == "/api/sessions/":
      return self._reply(200, {"id": "probe-session"})
    if method == "POST" and re.fullmatch(r"/api/chat/[^/]+/message", self.path):
      return self._reply(202, {"input_event_id": "probe-input"})
    if method == "GET" and re.fullmatch(r"/api/sessions/[^/]+/runs\?.*", self.path):
      return self._reply(200, {"items": [{"id": "probe-run", "state": BEHAVIOR["probe"]}]})
    return self._reply(404, {"detail": "not found"})

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
  if meta["ended_at"] is None and BEHAVIOR["adopt"]:
    log("task_recovery_resume", session=meta["session_id"], run_id=meta["id"])
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
  port = _free_port()
  (home / "config.yaml").write_text(f"server:\n  host: 127.0.0.1\n  port: {port}\n", encoding="utf-8")
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


def _seed_live_run(world: World, run_id: str, backend: str) -> None:
  """A Run record whose process is alive, as the server left it before a restart."""
  proc = _spawn_live(world)
  record = {
      "id": run_id,
      "session_id": SESSION,
      "kind": "work",
      "backend": backend,
      "pid": proc.pid,
      "pid_start": _pid_start(proc.pid),
      "started_at": datetime.datetime.now(datetime.UTC).isoformat(),
      "ended_at": None,
  }
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


@pytest.mark.parametrize("case", ["checksum", "check-config", "opencode-run", "improve-loop", "dirty-clone"])
def test_preflight_refusal_exits_1_and_changes_nothing(world: World, case: str) -> None:
  """Each preflight check refuses before the switch stops anything: exit 1 and no change."""
  release = world.releases / RUST_SHA
  if case == "checksum":
    (release / "charliebot-server").write_text(FAKE_SERVER + "# tampered\n", encoding="utf-8")
  elif case == "check-config":
    _write_behavior(release, check_config="fail")
  elif case == "opencode-run":
    _seed_live_run(world, "run-opencode", "opencode")
  elif case == "improve-loop":
    _seed_improve_loop(world, _spawn_live(world).pid)
  elif case == "dirty-clone":
    (world.clone / "server.py").write_text("# local edit\n", encoding="utf-8")

  proc = _switch(world, "rust", RUST_SHA)

  assert proc.returncode == 1, _report(world, proc)
  assert not (world.releases / "current").is_symlink()
  assert _git(world.clone, "rev-parse", "HEAD") == world.base_head


@pytest.mark.integration
def test_switch_on_a_real_pane_restores_then_round_trips_then_exits_3(world: World) -> None:
  """Every pane-driven exit in one tmux server: exit 2 after a failed readback, exit 0 each way, exit 3 on sync failure.

  The steps share one test because the integration cap leaves a single slot. Each step builds on the state the
  previous one leaves, so the order is the test.
  """
  pane = _start_pane(world)
  _start_server(world, pane)
  _seed_live_run(world, "run-live", "claude")

  # The Rust readback cannot adopt the live run: the switch fails after it stopped Python and restores Python.
  _write_behavior(world.releases / RUST_SHA, adopt=False)
  proc = _switch(world, "rust", RUST_SHA)
  assert proc.returncode == 2, _report(world, proc)
  _assert_untouched(world)

  # With a passing readback the live run is adopted, and the clone fast-forwards to the PORT_BASELINE commit.
  _write_behavior(world.releases / RUST_SHA)
  proc = _switch(world, "rust", RUST_SHA)
  assert proc.returncode == 0, _report(world, proc)
  assert os.readlink(world.releases / "current") == RUST_SHA
  assert _git(world.clone, "rev-parse", "HEAD") == world.baseline
  assert _serves_release(world, RUST_SHA)
  assert _version_status(world.port) == 200
  assert "run_id=run-live" in _latest_log(world)

  # The way back needs no fast-forward: the clone stays at the baseline and current is removed.
  proc = _switch(world, "python")
  assert proc.returncode == 0, _report(world, proc)
  assert not (world.releases / "current").is_symlink()
  assert _git(world.clone, "rev-parse", "HEAD") == world.baseline
  assert _serves_python(world)

  # A sync that fails again during the restore leaves the server down, which is exit 3.
  (world.clone / "fail-sync").write_text("", encoding="utf-8")
  proc = _switch(world, "rust", RUST_SHA)
  assert proc.returncode == 3, _report(world, proc)
  assert _listener_pids(world.port) == set()
