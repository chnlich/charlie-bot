"""The switch command (scripts/switch-server.sh) against fake servers.

Each test builds its own runtime clone, home, release root and log directory under tmp_path, so nothing touches
~/.charliebot, ~/charliebot-releases, the default tmux server or the live port. FAKE_SERVER is one program that stands
in for the release binary: the switch reads the server's argv to tell a release from any other process. Its behavior
comes from a behavior.json beside it. Each installed release has a matching commit in the runtime clone, and the
switch moves the clone to the target release commit in either direction.

The probe turn's cases run its functions in a shell against a fake server of their own, with one-second waits, so each
stays under the unit budget. The delegation check's cases run delegation_check the probe's way, against a fake server
that also answers the delegate, task and archive routes. That fake server adds real worktrees to a git repository, so
each case can check that the check left no worktree and no branch behind. The restore case runs switch_forward with its
stop, pane, server and probe steps stubbed out.

The preflight, the adoption check, the escalation and the pane-driven switch moved to Rust integration tests in
crates/app/script-tests, which run the same script against stub commands.
"""

from __future__ import annotations

import dataclasses
import hashlib
import json
import os
import pathlib
import re
import shutil
import socket
import subprocess
import time
import urllib.error
import urllib.request
import uuid
from collections.abc import Callable, Iterator

import pytest
import systemd_stubs
import yaml

ROOT = pathlib.Path(__file__).resolve().parents[1]
SWITCH = ROOT / "scripts" / "switch-server.sh"
LAUNCHER = ROOT / "scripts" / "start-server.sh"
# A target sha for a shell that runs one switch step: the step parses it, and no git check resolves it.
STEP_TARGET_SHA = hashlib.sha1(b"step target under test").hexdigest()
ACCESS_KEY = "switch-test-key"
# The fake server's probe run: run_state is its state before a stop; output is none, message (a committed assistant
# message) or tool (a tool call in the open draft); output_after_polls counts the bootstrap reads that see no output;
# cancel is settles (a stop ends the run) or hangs (it does not); followup is none or nudge, which starts a second run
# in the probe session at the first archive, as the web reply audit does after a turn, and that archive answers 409.
# The delegation check's task: delegate is accepts or refused (the server answers 400); task_state is the state of the
# task's run; worktree is created (the server makes the worktree) or missing; task_error is the error the run logs, or
# empty.
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
    "archive_runs_delay_after": 0,
    "ignore_sigterm": False,
    "delegate": "accepts",
    "task_state": "success",
    "worktree": "created",
    "task_error": "",
}
GIT_ENV = {
    **os.environ,
    "GIT_AUTHOR_NAME": "switch-test",
    "GIT_AUTHOR_EMAIL": "switch-test@example.invalid",
    "GIT_COMMITTER_NAME": "switch-test",
    "GIT_COMMITTER_EMAIL": "switch-test@example.invalid",
}
DIRECT_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))

# The stand-in release binary. Its routes answer the switch's readback and probe; at startup it logs a recovery line for
# every live Run record (when it adopts) and server_ready, as JSON lines like the Rust server. Each request past the
# version probe is appended to calls.log beside the program, as "METHOD PATH".
FAKE_SERVER = r'''#!/usr/bin/env python3
"""Stand-in CharlieBot server: the routes and log lines the switch reads, behavior from behavior.json."""
import datetime
import http.server
import json
import os
import pathlib
import re
import signal
import subprocess
import sys
import time

HERE = pathlib.Path(__file__).resolve().parent
BEHAVIOR = json.loads((HERE / "behavior.json").read_text(encoding="utf-8"))
HOME = pathlib.Path(os.environ["CHARLIEBOT_HOME"])
CONFIG = (HOME / "config.yaml").read_text(encoding="utf-8")
PORT = int(re.search(r"^\s*port:\s*(\d+)", CONFIG, re.MULTILINE).group(1))
KEY = re.search(r"access_key:\s*(\S+)", (HOME / "credentials.yaml").read_text(encoding="utf-8")).group(1)
PROBE_SESSION = "probe-session"
# The delegation check's parent session, the child session its task runs in, and the task's run. TASK holds the run
# record that the delegate call created.
PARENT_SESSION = "delegation-session"
CHILD_SESSION = "delegation-child"
TASK_RUN = "task-run"
TASK = {}
# The user messages the parent session has received: the server delegates only from a chain that holds one.
OPENED = []
TERMINAL = {"success", "failed", "interrupted", "stopped"}
# The session's runs, oldest first. The probe's message starts the probe run in behavior.run_state; behavior.followup
# "nudge" adds the follow-up run "nudge-run" at the first archive. READS counts the bootstrap reads and the run
# listings of the archive step.
RUNS = []
READS = {"bootstrap": 0, "runs": 0}


def log(event, **fields):
  stamp = datetime.datetime.now(datetime.UTC).isoformat()
  record = {"event": event, **fields, "level": "info", "timestamp": stamp}
  print(json.dumps(record, separators=(",", ":")), flush=True)


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
    if self.path.startswith(("/api/internal/delegate", f"/api/chat/{PARENT_SESSION}", f"/api/sessions/{PARENT_SESSION}",
                             f"/api/sessions/{CHILD_SESSION}")):
      return self._delegation(method, request_json)
    if method == "POST" and self.path == "/api/sessions/":
      if request_json.get("name") == "switch delegation":
        return self._reply(200, {"id": PARENT_SESSION})
      return self._reply(200, {"id": PROBE_SESSION})
    if method == "POST" and re.fullmatch(r"/api/chat/[^/]+/message", self.path):
      if not request_json.get("request_id"):
        return self._reply(422, {"detail": "request_id is required"})
      if BEHAVIOR["message_status"] != 202:
        return self._reply(BEHAVIOR["message_status"], {"detail": "message refused"})
      RUNS.append({"id": "probe-run", "state": BEHAVIOR["run_state"]})
      return self._reply(202, {"input_event_id": "probe-input"})
    if method == "GET" and re.fullmatch(r"/api/sessions/[^/]+/runs\?.*", self.path):
      # The archive step's run listing stalls once archive_runs_delay_after listings have been answered fast.
      if "order=desc" not in self.path:
        READS["runs"] += 1
        if READS["runs"] > BEHAVIOR["archive_runs_delay_after"]:
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

  def _delegation(self, method, request_json):
    path = self.path.split("?")[0]
    if method == "POST" and path == f"/api/chat/{PARENT_SESSION}/message":
      OPENED.append(request_json["content"])
      return self._reply(202, {"input_event_id": "open-input"})
    if method == "POST" and path == "/api/internal/delegate":
      if BEHAVIOR["delegate"] == "refused":
        return self._reply(400, {"detail": "backend router failed: no backend is available"})
      if not OPENED:  # the server's rule: a delegation needs a user message on its task chain
        return self._reply(403, {"detail": "Delegation blocked: no user request on the task chain from task "
                                           f"{PARENT_SESSION} to its root."})
      TASK.clear()
      TASK.update(self._start_task(request_json["repo_path"]))
      return self._reply(200, {"session_id": CHILD_SESSION, "parent_session_id": request_json["session_id"],
                               "run_id": TASK_RUN, "thread_id": TASK_RUN, "description": request_json["description"]})
    if method == "GET" and path == f"/api/sessions/{CHILD_SESSION}/runs":
      return self._reply(200, {"items": [dict(TASK)] if TASK else [], "next_cursor": None})
    if method == "POST" and path == f"/api/sessions/{CHILD_SESSION}/runs/{TASK_RUN}/cancel":
      if BEHAVIOR["cancel"] == "settles" and TASK["state"] not in TERMINAL:
        TASK["state"] = "interrupted"
      return self._reply(200, {"run_id": TASK_RUN, "stop_requested": True, "outcome": None})
    if method == "GET" and path == f"/api/sessions/{PARENT_SESSION}/runs":
      return self._reply(200, {"items": [], "next_cursor": None})
    if method == "POST" and path == f"/api/sessions/{PARENT_SESSION}/archive":
      return self._reply(200, {"archived": [PARENT_SESSION]})
    return self._reply(404, {"detail": "not found"})

  def _start_task(self, repo):
    """The task's run record. A created worktree is a real git worktree of the repository, as the server makes one; a
    task error goes to the run's event log, where the server writes its launch failures."""
    task = {"id": TASK_RUN, "state": BEHAVIOR["task_state"], "repo_path": repo, "branch_name": None,
            "worktree_path": None}
    if BEHAVIOR["worktree"] == "created":
      worktree = HOME / "worktrees" / "task-switch-check"
      worktree.parent.mkdir(parents=True, exist_ok=True)
      subprocess.run(["git", "-C", repo, "worktree", "add", "-q", "-b", "charliebot/task-switch-check", str(worktree),
                      "main"], check=True)
      task.update(branch_name="charliebot/task-switch-check", worktree_path=str(worktree.resolve()))
    if BEHAVIOR["task_error"]:
      run_dir = HOME / "sessions" / CHILD_SESSION / "data" / "runs" / TASK_RUN
      run_dir.mkdir(parents=True, exist_ok=True)
      (run_dir / "events.jsonl").write_text(
          json.dumps({"type": "error", "message": BEHAVIOR["task_error"]}) + "\n", encoding="utf-8")
    return task

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

# A uv stand-in: `uv sync` fails while the clone holds a fail-sync marker.
FAKE_UV = """\
#!/usr/bin/env python3
import pathlib
import sys

if sys.argv[1] == "sync":
  sys.exit(1 if pathlib.Path("fail-sync").exists() else 0)
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
  initial_head: str  # the runtime clone's HEAD before any switch
  pre_sha: str  # the build before the crate grouping: the agents crate layout
  old_sha: str  # the release current names: the runtime/agents layout, OpenCode and agy cannot re-attach
  rust_sha: str  # the target release: one crate per backend, OpenCode and agy can re-attach
  new_sha: str  # the release commit used by the forward and back switch test: agy cannot re-attach
  port: int
  live: list[subprocess.Popen[bytes]] = dataclasses.field(default_factory=list)


def _git(cwd: pathlib.Path, *args: str) -> str:
  return subprocess.run(["git", "-C", str(cwd), *args], capture_output=True, text=True, env=GIT_ENV,
                        check=True).stdout.strip()


def _write_behavior(directory: pathlib.Path, **overrides: object) -> None:
  (directory / "behavior.json").write_text(json.dumps({**BEHAVIOR_DEFAULTS, **overrides}), encoding="utf-8")


def _sha256(path: pathlib.Path) -> str:
  return hashlib.sha256(path.read_bytes()).hexdigest()


def _install_release(releases: pathlib.Path, sha: str) -> None:
  """A release directory with its binary, library and checksums."""
  release = releases / sha
  release.mkdir(parents=True)
  binary = release / "charliebot-server"
  binary.write_text(FAKE_SERVER, encoding="utf-8")
  binary.chmod(0o755)
  (release / "libfake.so").write_bytes(b"fake shared object")
  sums = [f"{_sha256(release / name)}  {name}\n" for name in ("charliebot-server", "libfake.so")]
  (release / "sha256sums.txt").write_text("".join(sums), encoding="utf-8")
  _write_behavior(release)


def _write_backend_registration(
    clone: pathlib.Path, module: str, backend_type: str, *, enabled: bool, layout: str = "old") -> None:
  """Write one backend's registration in the layout that build has: `pre` keeps it in the agents crate, `old` in the
  runtime/agents crate, `new` in the backend's own crate."""
  if layout == "new":
    path = clone / "rust/crates/backends" / module / "src/lib.rs"
  elif layout == "pre":
    path = clone / "rust/crates/charliebot-agents/src/backends" / module / "mod.rs"
  else:
    path = clone / "rust/crates/runtime/agents/src/backends" / module / "mod.rs"
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(
      f'''pub fn register() {{
  BackendTypeRegistration {{
    type_name: "{backend_type}",
    traits: BackendTraits {{
      restart_reattach: {str(enabled).lower()},
    }},
  }};
}}
''',
      encoding="utf-8")


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
  # The first build predates the crate grouping: one module per backend under the agents crate.
  for module, backend_type, enabled in (("opencode", "opencode", False), ("antigravity", "antigravity", False),
                                        ("claude_code", "cc-claude", True)):
    _write_backend_registration(clone, module, backend_type, enabled=enabled, layout="pre")
  _git(clone, "init", "-q", "-b", "main")
  _git(clone, "add", "scripts", "rust")
  _git(clone, "commit", "-q", "-m", "fake runtime clone")
  pre_sha = _git(clone, "rev-parse", "HEAD")
  # The grouping moves the agents crate under runtime/; the registrations keep their module layout.
  for module in ("opencode", "antigravity", "claude_code"):
    (clone / "rust/crates/charliebot-agents/src/backends" / module / "mod.rs").unlink()
  _write_backend_registration(clone, "opencode", "opencode", enabled=False)
  _write_backend_registration(clone, "antigravity", "antigravity", enabled=False)
  _write_backend_registration(clone, "claude_code", "cc-claude", enabled=True)
  _git(clone, "add", "rust")
  _git(clone, "commit", "-q", "-m", "move the crates into group directories")
  old_sha = _git(clone, "rev-parse", "HEAD")
  # The split moves each registration into its own crate, and OpenCode and agy start re-attaching.
  for module in ("opencode", "antigravity", "claude_code"):
    (clone / "rust/crates/runtime/agents/src/backends" / module / "mod.rs").unlink()
  _write_backend_registration(clone, "opencode", "opencode", enabled=True, layout="new")
  _write_backend_registration(clone, "antigravity", "antigravity", enabled=True, layout="new")
  _write_backend_registration(clone, "claude_code", "cc-claude", enabled=True, layout="new")
  _git(clone, "add", "rust")
  _git(clone, "commit", "-q", "-m", "move the backends into their own crates")
  rust_sha = _git(clone, "rev-parse", "HEAD")
  # A later build turns agy's re-attachment back off inside its own crate, so a switch away from a new-layout build
  # is read there too.
  _write_backend_registration(clone, "antigravity", "antigravity", enabled=False, layout="new")
  _git(clone, "add", "rust")
  _git(clone, "commit", "-q", "-m", "new release")
  new_sha = _git(clone, "rev-parse", "HEAD")
  subprocess.run(["git", "clone", "-q", "--bare", str(clone), str(root / "origin.git")], env=GIT_ENV, check=True)
  _git(clone, "remote", "add", "origin", str(root / "origin.git"))
  _git(clone, "checkout", "-q", old_sha)
  _install_release(releases, old_sha)
  _install_release(releases, pre_sha)
  _install_release(releases, rust_sha)
  _install_release(releases, new_sha)
  (releases / "current").symlink_to(old_sha)

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
      # A long TMPDIR puts the sandbox's private TMUX_TMPDIR past the 108-byte unix socket path limit.
      "TMUX_TMPDIR": "/tmp",
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
      initial_head=old_sha,
      pre_sha=pre_sha,
      old_sha=old_sha,
      rust_sha=rust_sha,
      new_sha=new_sha,
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


def _report(world: World, proc: subprocess.CompletedProcess[str]) -> str:
  logs = sorted(world.logs.glob("switch_*.log"))
  switch_log = logs[-1].read_text(encoding="utf-8") if logs else "(no switch log)"
  return f"rc={proc.returncode}\nstdout:\n{proc.stdout}\nstderr:\n{proc.stderr}\nswitch log:\n{switch_log}"


def _spawn_live(world: World) -> subprocess.Popen[bytes]:
  proc = subprocess.Popen(["sleep", "300"], start_new_session=True)
  world.live.append(proc)
  return proc


def _wait_until(predicate: Callable[[], bool], timeout_s: float) -> None:
  deadline = time.monotonic() + timeout_s
  while not predicate():
    if time.monotonic() > deadline:
      pytest.fail(f"condition not met within {timeout_s}s")
    time.sleep(0.05)


def _version_status(port: int) -> int:
  request = urllib.request.Request(
      f"http://127.0.0.1:{port}/api/internal/version", headers={"Authorization": f"Bearer {ACCESS_KEY}"})
  try:
    with DIRECT_OPENER.open(request, timeout=5) as response:
      return response.status
  except urllib.error.HTTPError as error:
    return error.code


def _switch_forward(world: World,
                    run_pid: int,
                    launch_output: str,
                    delegation: str = ":") -> subprocess.CompletedProcess[str]:
  """The switch's forward path, from the scan through the readback to the exit, with its stop, pane, server and probe
  steps stubbed out. DELEGATION is the shell command that stands in for the delegation check.

  The scan records the live run, and the stub stop kills its process, as a stop that does not spare a run would. A
  restore touches a marker file and exits 2, so a test tells a restore from the switch's own exit. OLD_IMPL names the
  running build, as the preflight left it.
  """
  launch_log = world.root / "server.log"
  launch_log.write_text(launch_output, encoding="utf-8")
  work = world.root / "work"
  work.mkdir(exist_ok=True)
  env = {
      **world.env,
      "CLONE": str(world.clone),
      "RUN_PID": str(run_pid),
      "RESTORED": str(world.root / "restored"),
      "RUNS_FILE": str(world.root / "switch.runs"),
      "WANT": f"rust {world.rust_sha}",
      "PORT": str(world.port),
      "NEW_LOG": str(launch_log),
      "NEW_OFFSET": "0",
      "WORK": str(work),
      "DELEGATION": delegation,
  }
  script = """
source "$1" rust "$2"
REPO_ROOT="$CLONE"
OLD_IMPL="$3"
stop_old() { kill -KILL "$RUN_PID"; }
start_in_pane() { :; }
wait_ready() { :; }
serving() { :; }
probe_turn() { :; }
delegation_check() { $DELEGATION; }
restore() { touch "$RESTORED"; exit 2; }
switch_forward
"""
  return subprocess.run(
      ["bash", "-c", script, "bash", str(SWITCH), world.rust_sha, f"rust {world.old_sha}"],
      env=env,
      capture_output=True,
      text=True,
      timeout=30,
      check=False)


ProbeRun = Callable[..., tuple[subprocess.CompletedProcess[str], list[str]]]
PROBE_BOOTSTRAP = "GET /api/sessions/probe-session/bootstrap"
PROBE_CANCEL = "POST /api/sessions/probe-session/runs/probe-run/cancel"
PROBE_ARCHIVE = "POST /api/sessions/probe-session/archive"


def _answers_version(port: int) -> bool:
  try:
    return _version_status(port) == 200
  except OSError:
    return False


def _start_fake(
    tmp_path: pathlib.Path, home: pathlib.Path, servers: list[subprocess.Popen[bytes]], port: int,
    behavior: dict[str, object]) -> pathlib.Path:
  """Starts one stand-in server on PORT over HOME with the behavior. Returns its directory, where calls.log lands."""
  server_dir = tmp_path / "server"
  server_dir.mkdir()
  binary = server_dir / "charliebot-server"
  binary.write_text(FAKE_SERVER, encoding="utf-8")
  binary.chmod(0o755)
  _write_behavior(server_dir, **behavior)
  env = {**os.environ, "CHARLIEBOT_HOME": str(home)}
  servers.append(subprocess.Popen([str(binary)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
  _wait_until(lambda: _answers_version(port), 10)
  return server_dir


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
    server_dir = _start_fake(tmp_path, home, servers, port, behavior)
    env = {
        **os.environ,
        "CHARLIEBOT_HOME": str(home),
        "CHARLIEBOT_SWITCH_PROBE_WAIT_S": "1",
        "CHARLIEBOT_SWITCH_PROBE_CANCEL_WAIT_S": "1",
    }
    work = tmp_path / "work"
    work.mkdir()
    probe_env = {**env, "PORT": str(port), "KEY": ACCESS_KEY, "WORK": str(work)}
    probe_cmd = ["bash", "-c", f'source "$1" rust {STEP_TARGET_SHA} && probe_turn', "bash", str(SWITCH)]
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
  """The probe run ends success, then the server starts a follow-up run. The archive's 409 makes the probe stop it."""
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
  """A run listing that answers only after END reports the wait timeout, not the listing's timed-out status."""
  start = time.monotonic()
  proc, calls = probe(run_state="success", output="none", archive_runs_delay=2)
  elapsed = time.monotonic() - start
  assert proc.returncode == 1, proc.stdout + proc.stderr
  assert elapsed < 1.8
  assert any(line.startswith("GET /api/sessions/probe-session/runs?limit=500") for line in calls)
  assert re.search(r"probe: archive did not finish within 1s session=probe-session run=- elapsed=\d+s", proc.stdout)
  _assert_stage_lines(proc)


def test_probe_archive_names_the_run_when_its_listing_answers_after_the_wait(probe: ProbeRun) -> None:
  """The wait ends while the run listing that follows the stop request is in flight: the probe reports the settle
  timeout with the run it waited for, as the clock check at the top of a pass does."""
  proc, calls = probe(
      run_state="running", output="message", cancel="hangs", archive_runs_delay=2, archive_runs_delay_after=1)
  assert proc.returncode == 1, proc.stdout + proc.stderr
  assert PROBE_CANCEL in calls
  assert calls.count("GET /api/sessions/probe-session/runs?limit=500") == 2
  assert re.search(r"probe: run did not settle within 1s session=probe-session run=probe-run elapsed=\d+s", proc.stdout)
  _assert_stage_lines(proc)


DelegationRun = Callable[..., tuple[subprocess.CompletedProcess[str], list[str], pathlib.Path]]
DELEGATE = "POST /api/internal/delegate"
DELEGATION_CANCEL = "POST /api/sessions/delegation-child/runs/task-run/cancel"
DELEGATION_ARCHIVE = "POST /api/sessions/delegation-session/archive"


@pytest.fixture
def delegation(tmp_path: pathlib.Path) -> Iterator[DelegationRun]:
  """The switch's delegation check, run in a shell against a fake server that answers the delegate, task and archive
  routes with the behavior the test gives. The runtime clone is a git repository with a main branch, so the fake server
  adds real worktrees to it. The runner returns the check's result, the requests the fake served, and the clone."""
  home = tmp_path / "home"
  home.mkdir()
  port = _free_port()
  (home / "config.yaml").write_text(f"server:\n  host: 127.0.0.1\n  port: {port}\n", encoding="utf-8")
  (home / "credentials.yaml").write_text(f"charliebot:\n  access_key: {ACCESS_KEY}\n", encoding="utf-8")
  clone = tmp_path / "clone"
  clone.mkdir()
  _git(clone, "init", "-q", "-b", "main")
  (clone / "trial.txt").write_text("before\n", encoding="utf-8")
  _git(clone, "add", "trial.txt")
  _git(clone, "commit", "-q", "-m", "baseline")
  servers: list[subprocess.Popen[bytes]] = []

  def run(**behavior: object) -> tuple[subprocess.CompletedProcess[str], list[str], pathlib.Path]:
    server_dir = _start_fake(tmp_path, home, servers, port, behavior)
    work = tmp_path / "work"
    work.mkdir()
    env = {
        **os.environ,
        "CHARLIEBOT_HOME": str(home),
        "CHARLIEBOT_SWITCH_DELEGATION_WAIT_S": "1",
        "CHARLIEBOT_SWITCH_PROBE_CANCEL_WAIT_S": "1",
        "PORT": str(port),
        "KEY": ACCESS_KEY,
        "WORK": str(work),
    }
    script = 'source "$1" rust "$2" && REPO_ROOT="$3" && delegation_check'
    proc = subprocess.run(
        ["bash", "-c", script, "bash", str(SWITCH), STEP_TARGET_SHA,
         str(clone)],
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
        check=False)
    calls_log = server_dir / "calls.log"
    calls = calls_log.read_text(encoding="utf-8").splitlines() if calls_log.exists() else []
    return proc, calls, clone

  yield run
  for server in servers:
    server.kill()
    server.wait()


def _left_behind(clone: pathlib.Path) -> list[str]:
  """The worktrees beyond the clone and the branches beyond main that the clone still holds."""
  worktrees = _git(clone, "worktree", "list", "--porcelain").splitlines()
  extra = [line for line in worktrees if line.startswith("worktree ") and line != f"worktree {clone.resolve()}"]
  branches = [name for name in _git(clone, "branch", "--format=%(refname:short)").splitlines() if name != "main"]
  return extra + branches


def test_delegation_passes_when_the_run_succeeds_and_its_worktree_exists(delegation: DelegationRun) -> None:
  proc, calls, clone = delegation(task_state="success", worktree="created")
  assert proc.returncode == 0, proc.stdout + proc.stderr
  assert "delegation: passed: run task-run ended success; worktree " in proc.stdout
  assert calls.count(DELEGATE) == 1
  assert not any(line.endswith("/cancel") for line in calls)  # a run that ended needs no stop
  assert calls[-1] == DELEGATION_ARCHIVE
  assert _left_behind(clone) == []


def test_delegation_fails_with_the_run_error_and_leaves_nothing_behind(delegation: DelegationRun) -> None:
  error = "RuntimeError: git worktree add timed out after 0.3s for charliebot/task-switch-check"
  proc, calls, clone = delegation(task_state="failed", worktree="missing", task_error=error)
  assert proc.returncode == 1, proc.stdout + proc.stderr
  assert f"delegation: step run failed: run task-run ended failed; the server's error: {error}" in proc.stdout
  assert calls[-1] == DELEGATION_ARCHIVE
  assert _left_behind(clone) == []


def test_delegation_refused_by_the_server_names_the_server_answer(delegation: DelegationRun) -> None:
  proc, calls, clone = delegation(delegate="refused")
  assert proc.returncode == 1, proc.stdout + proc.stderr
  assert "delegation: step delegate failed: the server answered 400:" in proc.stdout
  assert "backend router failed" in proc.stdout
  assert calls[-1] == DELEGATION_ARCHIVE
  assert _left_behind(clone) == []


@pytest.mark.integration
def test_delegation_times_out_then_stops_the_run_and_removes_its_worktree(delegation: DelegationRun) -> None:
  proc, calls, clone = delegation(task_state="running", worktree="created", cancel="settles")
  assert proc.returncode == 1, proc.stdout + proc.stderr
  assert "delegation: step run failed: run task-run did not end within 1s" in proc.stdout
  assert DELEGATION_CANCEL in calls
  assert calls[-1] == DELEGATION_ARCHIVE
  assert _left_behind(clone) == []


def test_switch_restores_and_exits_2_when_the_delegation_check_fails(world: World) -> None:
  """A failed delegation check takes the path of a failed probe turn, so the switch restores the previous release."""
  proc = _switch_forward(world, _spawn_live(world).pid, "", delegation="return 1")
  assert proc.returncode == 2, _report(world, proc)
  assert (world.root / "restored").exists(), _report(world, proc)
  assert "switch: step failed: the delegation check failed" in proc.stdout
  assert "SWITCH COMPLETE" not in proc.stdout
