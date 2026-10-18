"""Process isolation for the parity servers: the temporary trees, the synthetic config and the traced processes.

Each server owns one temporary tree. The tree is the server's ``HOME``; ``home/`` inside it is its
``CHARLIEBOT_HOME`` and ``tmp/`` its ``TMPDIR``. A server runs with a sanitized ``PATH`` (the tree's
``fake-bin`` plus ``/usr/bin:/bin``) whose agent CLI names are scripted fakes, and with every proxy variable
pointed at a closed loopback port, so no request leaves the host. Every process under test (the fixture generator
and each server) runs inside sandbox.py's Landlock ruleset and under ``strace``; guard.py reads the trace for
writes outside each tree.
"""

import dataclasses
import os
import pathlib
import signal
import shlex
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

import yaml

# The fixed synthetic access key both servers accept. It guards nothing but the two temporary servers.
ACCESS_KEY = "parity-access-key-7c1e4b"
PORT_RANGE = range(18600, 18700)
HOST = "127.0.0.1"
# The agent CLI names a server could launch. Each stand-in prints one line and exits 1.
STAND_IN_NAMES = ("claude", "codex", "opencode", "charlie-code", "gemini", "agy")
FAKE_AGENT = pathlib.Path(__file__).resolve().with_name("fake_agent.py")
# A loopback port with no listener: every proxied request a server makes is refused at once.
CLOSED_PROXY = "http://127.0.0.1:9"
# The tree entries the harness itself writes; the file comparison skips them.
HARNESS_ENTRIES = ("fake-agent", "fake-bin", "fixture-manifest.json", "logs")
# Syscalls that change a file or directory. openat/open/creat count only with a write flag (guard.py checks).
TRACED_SYSCALLS = (
    "open", "openat", "openat2", "creat", "mkdir", "mkdirat", "mknod", "mknodat", "rename", "renameat", "renameat2",
    "link", "linkat", "symlink", "symlinkat", "unlink", "unlinkat", "rmdir", "truncate", "chmod", "fchmod", "fchmodat",
    "fchmodat2", "chown", "fchown", "lchown", "fchownat", "utime", "utimes", "utimensat", "futimesat", "setxattr",
    "lsetxattr", "fsetxattr", "removexattr", "lremovexattr", "fremovexattr", "bind", "chdir", "fchdir", "clone",
    "clone3", "fork", "vfork")
READY_TIMEOUT_SECONDS = 60.0
STOP_TIMEOUT_SECONDS = 20.0
POLL_INTERVAL_SECONDS = 0.02


@dataclasses.dataclass(frozen=True)
class Tree:
  """One server's temporary tree; ``root`` is the server's HOME."""

  label: str
  root: pathlib.Path

  @property
  def home(self) -> pathlib.Path:
    return self.root / "home"

  @property
  def tmp(self) -> pathlib.Path:
    return self.root / "tmp"

  @property
  def fake_bin(self) -> pathlib.Path:
    return self.root / "fake-bin"

  @property
  def workspace(self) -> pathlib.Path:
    return self.root / "workspace"

  @property
  def worktrees(self) -> pathlib.Path:
    return self.root / "worktrees"


def make_tree(label: str, root: pathlib.Path) -> Tree:
  """Lay out one empty tree with its fake-bin stand-ins."""
  tree = Tree(label, root)
  for path in (tree.home, tree.tmp, tree.fake_bin, tree.workspace, tree.worktrees, tree.root / "logs",
               tree.root / "fake-agent"):
    path.mkdir(parents=True)
  for name in STAND_IN_NAMES:
    wrapper = tree.fake_bin / name
    command = f"exec /usr/bin/python3 -I {shlex.quote(str(FAKE_AGENT))} {shlex.quote(name)} \"$@\""
    wrapper.write_text("\n".join(("#!/bin/sh", command)) + "\n", encoding="utf-8")
    wrapper.chmod(0o755)
  return tree


def write_home_config(tree: Tree, port: int) -> None:
  """Write the synthetic config.yaml and credentials.yaml into the tree's home.

  One backend option, no integration, the session cgroups and the worker disk-headroom check off, and both
  path settings inside the tree.
  """
  config = {
      "server":
          {
              "host": HOST,
              "port": port,
              "session_memory_max_mb": 0,
              "session_tasks_max": 0,
              "min_free_disk_gib": 0,
          },
      "paths": {
          "workspace_dirs": [str(tree.workspace)],
          "worktree_dir": str(tree.worktrees),
      },
      "backends":
          {
              "pool": [],
              "options":
                  [{
                      "id": "claude-sonnet",
                      "label": "CC · Sonnet 5",
                      "type": "cc-claude",
                      "model": "claude-sonnet-5",
                  }],
          },
  }
  (tree.home / "config.yaml").write_text(yaml.safe_dump(config, sort_keys=False), encoding="utf-8")
  credentials = tree.home / "credentials.yaml"
  credentials.write_text(yaml.safe_dump({"charliebot": {"access_key": ACCESS_KEY}}), encoding="utf-8")
  credentials.chmod(0o600)


def server_env(tree: Tree, repo_root: pathlib.Path) -> dict[str, str]:
  """The whole environment of a process under test: nothing is inherited from the harness."""
  proxies = dict.fromkeys(("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"), CLOSED_PROXY)
  proxies.update({name.lower(): value for name, value in proxies.items()})
  return {
      "HOME": str(tree.root),
      "CHARLIEBOT_HOME": str(tree.home),
      "TMPDIR": str(tree.tmp),
      "PATH": f"{tree.fake_bin}:/usr/bin:/bin",
      "CHARLIEBOT_VENV_BIN": str(repo_root / ".venv" / "bin"),
      "CHARLIEBOT_REPO_ROOT": str(repo_root),
      "LANG": "C.UTF-8",
      "LC_ALL": "C.UTF-8",
      "TZ": "UTC",
      "PYTHONUNBUFFERED": "1",
      # Bytecode caches would land in the worktree, outside both trees.
      "PYTHONDONTWRITEBYTECODE": "1",
      "NO_PROXY": f"{HOST},localhost",
      "no_proxy": f"{HOST},localhost",
      **proxies,
  }


def pick_ports(count: int) -> list[int]:
  """Pick *count* distinct ports in PORT_RANGE that accept a bind right now."""
  ports: list[int] = []
  for port in PORT_RANGE:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
      try:
        probe.bind((HOST, port))
      except OSError:
        continue
    ports.append(port)
    if len(ports) == count:
      return ports
  raise RuntimeError(f"fewer than {count} free ports in {PORT_RANGE.start}-{PORT_RANGE.stop - 1}")


SANDBOX = pathlib.Path(__file__).resolve().parent / "sandbox.py"


def traced_argv(argv: list[str], tree: Tree, trace_log: pathlib.Path) -> list[str]:
  """*argv* inside the tree's write sandbox, under strace: every file-changing syscall of the process tree,
  completed or refused, lands in *trace_log*."""
  return [
      "/usr/bin/strace", "-f", "-qq", "--seccomp-bpf", "-y", "-s", "4096", "-e", "trace=" + ",".join(TRACED_SYSCALLS),
      "-o",
      str(trace_log), "--", sys.executable,
      str(SANDBOX), "--allow-write",
      str(tree.root), "--", *argv
  ]


def run_traced(
    argv: list[str], *, tree: Tree, repo_root: pathlib.Path, trace_log: pathlib.Path, log_path: pathlib.Path) -> None:
  """Run one traced process under test to completion; a non-zero exit raises with its log tail."""
  with open(log_path, "wb") as log_file:
    proc = subprocess.run(
        traced_argv(argv, tree, trace_log),
        env=server_env(tree, repo_root),
        cwd=repo_root,
        stdin=subprocess.DEVNULL,
        stdout=log_file,
        stderr=subprocess.STDOUT,
        check=False)
  if proc.returncode != 0:
    raise RuntimeError(f"{argv[0]} exited {proc.returncode}; log tail:\n{log_tail(log_path)}")


def log_tail(path: pathlib.Path, lines: int = 40) -> str:
  return "\n".join(path.read_text(encoding="utf-8", errors="replace").splitlines()[-lines:])


class ServerProcess:
  """One traced server process bound to one tree and port."""

  def __init__(
      self, label: str, argv: list[str], tree: Tree, port: int, repo_root: pathlib.Path, process_number: int) -> None:
    self.label = label
    self.argv = argv
    self.tree = tree
    self.port = port
    self.repo_root = repo_root
    self.trace_log = tree.root / "logs" / f"{tree.label}.server-{process_number}.strace"
    self.log_path = tree.root / "logs" / f"{tree.label}.server-{process_number}.log"
    self._proc: subprocess.Popen | None = None
    self._server_identity: tuple[int, str] | None = None

  @property
  def base_url(self) -> str:
    return f"http://{HOST}:{self.port}"

  def start(self) -> None:
    log_file = open(self.log_path, "wb")
    try:
      self._proc = subprocess.Popen(
          traced_argv(self.argv, self.tree, self.trace_log),
          env=server_env(self.tree, self.repo_root),
          cwd=self.repo_root,
          stdin=subprocess.DEVNULL,
          stdout=log_file,
          stderr=subprocess.STDOUT,
          start_new_session=True)
    finally:
      log_file.close()

  def wait_ready(self) -> None:
    """Poll the public auth-status route until it answers 200; a dead server raises with its log tail."""
    deadline = time.monotonic() + READY_TIMEOUT_SECONDS
    url = self.base_url + "/api/auth/status"
    while time.monotonic() < deadline:
      if self._proc.poll() is not None:
        raise RuntimeError(
            f"{self.label} server exited {self._proc.returncode} before it was ready; log tail:\n"
            f"{log_tail(self.log_path)}")
      try:
        with urllib.request.urlopen(url, timeout=2) as response:
          if response.status == 200:
            self._server_identity = self._capture_identity()
            return
      except (urllib.error.URLError, ConnectionError, TimeoutError):
        pass
      time.sleep(0.1)
    raise RuntimeError(
        f"{self.label} server not ready after {READY_TIMEOUT_SECONDS}s; log tail:\n"
        f"{log_tail(self.log_path)}")

  def _server_pid(self) -> int | None:
    """The traced server's pid: strace's one direct child (the sandbox launcher execs into the server)."""
    children = pathlib.Path(f"/proc/{self._proc.pid}/task/{self._proc.pid}/children")
    try:
      pids = children.read_text(encoding="utf-8").split()
    except OSError:
      return None
    return int(pids[0]) if pids else None

  def _capture_identity(self) -> tuple[int, str]:
    """The traced server's pid and process start time, so a later stop cannot signal a reused pid."""
    server_pid = self._server_pid()
    if server_pid is None:
      raise RuntimeError(f"{self.label} server has no pid under its strace")
    identity = process_identity(server_pid)
    if identity is None:
      raise RuntimeError(f"{self.label} server pid {server_pid} exited")
    return server_pid, identity[1]

  def stop_server(self) -> None:
    """SIGTERM the server and wait for its pid to exit, without waiting for traced children."""
    if self._proc is None or self._proc.poll() is not None:
      return
    if self._server_identity is None:
      self._server_identity = self._capture_identity()
    server_pid, pid_start = self._server_identity
    identity = process_identity(server_pid)
    if identity is not None and identity[1] == pid_start and identity[0] != "Z":
      try:
        os.kill(server_pid, signal.SIGTERM)
      except ProcessLookupError:
        pass
    deadline = time.monotonic() + STOP_TIMEOUT_SECONDS
    while time.monotonic() < deadline:
      identity = process_identity(server_pid)
      if identity is None or identity[0] == "Z" or identity[1] != pid_start:
        return
      time.sleep(POLL_INTERVAL_SECONDS)
    if self._proc.poll() is None:
      os.killpg(self._proc.pid, signal.SIGKILL)
      self._proc.wait()
    raise RuntimeError(f"{self.label} server ignored SIGTERM for {STOP_TIMEOUT_SECONDS}s and was killed")

  def wait_trace(self) -> None:
    """Wait for this server's strace process after its traced children have exited."""
    if self._proc is None or self._proc.poll() is not None:
      return
    try:
      self._proc.wait(timeout=STOP_TIMEOUT_SECONDS)
    except subprocess.TimeoutExpired:
      os.killpg(self._proc.pid, signal.SIGKILL)
      self._proc.wait()
      raise RuntimeError(f"{self.label} strace did not exit within {STOP_TIMEOUT_SECONDS}s and was killed") from None

  def stop(self) -> None:
    """Stop the server and wait for strace after the server's children exit."""
    self.stop_server()
    self.wait_trace()

  def kill(self) -> None:
    """Kill the whole process group (the cleanup path after a harness error)."""
    if self._proc is not None and self._proc.poll() is None:
      os.killpg(self._proc.pid, signal.SIGKILL)
      self._proc.wait()


def process_identity(pid: int) -> tuple[str, str] | None:
  """Read process state and field 22 of ``/proc/<pid>/stat``; return None after the process exits."""
  try:
    stat = pathlib.Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
  except FileNotFoundError:
    return None
  fields = stat.rsplit(")", 1)[1].split()
  return fields[0], fields[19]
