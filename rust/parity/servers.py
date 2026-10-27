"""Process isolation for the parity servers: the temporary trees, the synthetic config and the traced processes.

Each server owns one temporary tree. The tree is the server's ``HOME``; ``home/`` inside it is its
``CHARLIEBOT_HOME`` and ``tmp/`` its ``TMPDIR``. A server runs with a sanitized ``PATH`` (the tree's
``fake-bin``, the Nsight fake's ``bin``, plus ``/usr/bin:/bin``) whose agent CLI names are scripted fakes, and with
every proxy variable pointed at a closed loopback port, so no request leaves the host. Every process under test (the
fixture generator and each server) runs inside sandbox.py's Landlock ruleset and under ``strace``; guard.py reads the
trace for writes outside each tree.
"""

import ast
import contextlib
import ctypes
import dataclasses
import fcntl
import json
import os
import pathlib
import random
import re
import shlex
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from collections.abc import Callable, Iterator

import yaml

# The fixed synthetic access key both servers accept. It guards nothing but the two temporary servers.
ACCESS_KEY = "parity-access-key-7c1e4b"
PORT_RANGE = range(18600, 18700)
# One lock file per port in PORT_RANGE; a run holds a port's flock while its server uses the port.
PORT_LOCK_DIR = pathlib.Path("/tmp/charliebot-parity-ports")
HOST = "127.0.0.1"
# The agent CLI names a server could launch. Each stand-in prints one line and exits 1.
STAND_IN_NAMES = ("claude", "codex", "opencode", "charlie-code", "gemini", "agy")
FAKE_AGENT = pathlib.Path(__file__).resolve().with_name("fake_agent.py")
# A loopback port with no listener: every proxied request a server makes is refused at once.
CLOSED_PROXY = "http://127.0.0.1:9"
# The tree entries the harness itself writes; the file comparison skips them.
HARNESS_ENTRIES = (".opencode", "fake-agent", "fake-bin", "fixture-manifest.json", "logs")
# The name prefix of the headless Chrome profile that each server makes under its tree's tmp/ (its TMPDIR).
HEADLESS_PROFILE_PREFIX = "headless-render-"
# The tmux a tree's server resolves on its PATH unless --tmux names another one.
TMUX_BIN = pathlib.Path("/usr/bin/tmux")
# Syscalls that change a file or directory. openat/open/creat count only with a write flag (guard.py checks).
TRACED_SYSCALLS = (
    "open", "openat", "openat2", "creat", "mkdir", "mkdirat", "mknod", "mknodat", "rename", "renameat", "renameat2",
    "link", "linkat", "symlink", "symlinkat", "unlink", "unlinkat", "rmdir", "truncate", "chmod", "fchmod", "fchmodat",
    "fchmodat2", "chown", "fchown", "lchown", "fchownat", "utime", "utimes", "utimensat", "futimesat", "setxattr",
    "lsetxattr", "fsetxattr", "removexattr", "lremovexattr", "fremovexattr", "bind", "chdir", "fchdir", "clone",
    "clone3", "fork", "vfork")
READY_TIMEOUT_SECONDS = 60.0
STOP_TIMEOUT_SECONDS = 20.0
PROCESS_POOL_STOP_TIMEOUT_SECONDS = 5.0
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
  socket_path = tree.tmp / f"tmux-{os.getuid()}" / "charliebot"
  if len(os.fsencode(socket_path)) > 107:
    raise RuntimeError(f"tmux socket path exceeds 107 bytes: {socket_path}")
  for name in STAND_IN_NAMES:
    wrapper = tree.fake_bin / name
    command = f"exec /usr/bin/python3 -I {shlex.quote(str(FAKE_AGENT))} {shlex.quote(name)} \"$@\""
    wrapper.write_text("\n".join(("#!/bin/sh", command)) + "\n", encoding="utf-8")
    wrapper.chmod(0o755)
  opencode_wrapper = tree.root / ".opencode" / "bin" / "opencode"
  opencode_wrapper.parent.mkdir(parents=True)
  command = f"exec /usr/bin/python3 -I {shlex.quote(str(FAKE_AGENT))} opencode \"$@\""
  opencode_wrapper.write_text("\n".join(("#!/bin/sh", command)) + "\n", encoding="utf-8")
  opencode_wrapper.chmod(0o755)
  return tree


def link_tmux(tree: Tree, tmux: pathlib.Path) -> None:
  """Put *tmux* on the tree's PATH as ``tmux``, and nothing else from its directory.

  A host may carry tmux only as a wrapper outside /usr/bin (a tools directory, say), so the harness links the
  executable the option names into the tree's fake-bin, which comes first on the server's sanitized PATH. The server
  then resolves ``tmux`` to that file and never sees the other programs beside it.
  """
  (tree.fake_bin / "tmux").symlink_to(tmux)


def write_home_config(
    tree: Tree, port: int, config_overlay: dict | None = None, credentials_overlay: dict | None = None) -> None:
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
  config = _deep_merge(config, config_overlay or {})
  (tree.home / "config.yaml").write_text(yaml.safe_dump(config, sort_keys=False), encoding="utf-8")
  credentials = tree.home / "credentials.yaml"
  credential_data = _deep_merge({"charliebot": {"access_key": ACCESS_KEY}}, credentials_overlay or {})
  credentials.write_text(yaml.safe_dump(credential_data, sort_keys=False), encoding="utf-8")
  credentials.chmod(0o600)


def _deep_merge(base: dict, overlay: dict) -> dict:
  merged = dict(base)
  for key, value in overlay.items():
    if isinstance(merged.get(key), dict) and isinstance(value, dict):
      merged[key] = _deep_merge(merged[key], value)
    else:
      merged[key] = value
  return merged


def server_env(
    tree: Tree,
    repo_root: pathlib.Path,
    proxy_url: str | None = None,
    ca_file: pathlib.Path | None = None,
) -> dict[str, str]:
  """The whole environment of a process under test: nothing is inherited from the harness."""
  proxies = dict.fromkeys(("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"), CLOSED_PROXY)
  if proxy_url is not None:
    proxies["HTTPS_PROXY"] = proxy_url
  proxies.update({name.lower(): value for name, value in proxies.items()})
  environment = {
      "HOME": str(tree.root),
      "CHARLIEBOT_HOME": str(tree.home),
      "TMPDIR": str(tree.tmp),
      "TMUX_TMPDIR": str(tree.tmp),
      "PATH": f"{tree.fake_bin}:{repo_root / 'rust/parity/fakes/nsight/bin'}:/usr/bin:/bin",
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
  if ca_file is not None:
    environment["SSL_CERT_FILE"] = str(ca_file)
  return environment


@contextlib.contextmanager
def pick_ports(count: int) -> Iterator[list[int]]:
  """Reserve *count* ports of PORT_RANGE for the with block and yield them.

  A reserved port stays locked until the block exits, so a concurrent run skips it. The scan starts at a random
  offset, so concurrent runs try different ports first. A port whose lock is held, or that does not bind, is skipped.
  """
  PORT_LOCK_DIR.mkdir(parents=True, exist_ok=True)
  start = random.randrange(len(PORT_RANGE))
  ports: list[int] = []
  descriptors: list[int] = []
  try:
    for offset in range(len(PORT_RANGE)):
      if len(ports) == count:
        break
      port = PORT_RANGE[(start + offset) % len(PORT_RANGE)]
      descriptor = _lock_port(port)
      if descriptor is None:
        continue
      descriptors.append(descriptor)
      if not _port_binds(port):
        os.close(descriptor)
        descriptors.remove(descriptor)
        continue
      ports.append(port)
    if len(ports) < count:
      raise RuntimeError(f"fewer than {count} free ports in {PORT_RANGE.start}-{PORT_RANGE.stop - 1}")
    yield ports
  finally:
    for descriptor in descriptors:
      os.close(descriptor)


def _lock_port(port: int) -> int | None:
  """The open descriptor holding the port's exclusive flock, or None while another run holds it.

  The lock file is never deleted: a run that unlinked it could leave another run holding a lock on the old inode
  while a third run locks a new file for the same port.
  """
  descriptor = os.open(PORT_LOCK_DIR / f"{port}.lock", os.O_RDWR | os.O_CREAT, 0o644)
  try:
    fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
  except BlockingIOError:
    os.close(descriptor)
    return None
  except BaseException:
    os.close(descriptor)
    raise
  return descriptor


def _port_binds(port: int) -> bool:
  with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
    try:
      probe.bind((HOST, port))
    except OSError:
      return False
  return True


SANDBOX = pathlib.Path(__file__).resolve().parent / "sandbox.py"
# The strace that traces every process under test. parity.py's --strace option names another binary.
STRACE_BIN = pathlib.Path("/usr/bin/strace")


def traced_argv(argv: list[str], tree: Tree, trace_log: pathlib.Path, strace: pathlib.Path) -> list[str]:
  """*argv* inside the tree's write sandbox, under strace: every file-changing syscall of the process tree,
  completed or refused, lands in *trace_log*."""
  return [
      str(strace), "-f", "-qq", "--seccomp-bpf", "-y", "-s", "4096", "-e", "trace=" + ",".join(TRACED_SYSCALLS), "-o",
      str(trace_log), "--", sys.executable,
      str(SANDBOX), "--allow-write",
      str(tree.root), "--", *argv
  ]


def run_traced(
    argv: list[str],
    *,
    tree: Tree,
    repo_root: pathlib.Path,
    trace_log: pathlib.Path,
    log_path: pathlib.Path,
    strace: pathlib.Path,
) -> None:
  """Run one traced process under test to completion; a non-zero exit raises with its log tail."""
  with open(log_path, "wb") as log_file:
    proc = subprocess.run(
        traced_argv(argv, tree, trace_log, strace),
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
      self,
      label: str,
      argv: list[str],
      tree: Tree,
      port: int,
      repo_root: pathlib.Path,
      process_number: int,
      strace: pathlib.Path,
      proxy_url: str | None = None,
      ca_file: pathlib.Path | None = None,
  ) -> None:
    self.label = label
    self.argv = argv
    self.tree = tree
    self.port = port
    self.repo_root = repo_root
    self.strace = strace
    self.proxy_url = proxy_url
    self.ca_file = ca_file
    self.trace_log = tree.root / "logs" / f"{tree.label}.server-{process_number}.strace"
    self.log_path = tree.root / "logs" / f"{tree.label}.server-{process_number}.log"
    self._proc: subprocess.Popen | None = None
    self._server_identity: tuple[int, str] | None = None
    # The pid whose socket listens on the port, verified by wait_ready.
    self.owner_pid: int | None = None

  @property
  def base_url(self) -> str:
    return f"http://{HOST}:{self.port}"

  def start(self) -> None:
    log_file = open(self.log_path, "wb")
    try:
      self._proc = subprocess.Popen(
          traced_argv(self.argv, self.tree, self.trace_log, self.strace),
          env=server_env(self.tree, self.repo_root, self.proxy_url, self.ca_file),
          cwd=self.repo_root,
          stdin=subprocess.DEVNULL,
          stdout=log_file,
          stderr=subprocess.STDOUT,
          start_new_session=True)
    finally:
      log_file.close()

  def wait_ready(self) -> None:
    """Poll the public auth-status route until this server's own listener answers 200.

    A dead server raises with its log tail. A listener held by another process raises at once, naming that process,
    because the requests would reach it instead.
    """
    deadline = time.monotonic() + READY_TIMEOUT_SECONDS
    url = self.base_url + "/api/auth/status"
    while time.monotonic() < deadline:
      if self._proc.poll() is not None:
        raise RuntimeError(
            f"{self.label} server exited {self._proc.returncode} before it was ready; log tail:\n"
            f"{log_tail(self.log_path)}")
      owner = self._listener_owner()
      if owner is not None:
        try:
          with urllib.request.urlopen(url, timeout=2) as response:
            if response.status == 200:
              self.owner_pid = owner
              self._server_identity = self._capture_identity()
              return
        except (urllib.error.URLError, ConnectionError, TimeoutError):
          pass
      time.sleep(0.1)
    raise RuntimeError(
        f"{self.label} server not ready after {READY_TIMEOUT_SECONDS}s; log tail:\n"
        f"{log_tail(self.log_path)}")

  def exit_status(self) -> int | None:
    """The server's exit status once it has exited, else None. strace exits with its traced process's status."""
    return self._proc.poll()

  def _listener_owner(self) -> int | None:
    """The pid in this server's process tree that holds the listening socket on the port; None before the bind."""
    inodes = _listener_inodes(self.port)
    if not inodes:
      return None
    for pid in _process_tree(self._proc.pid):
      if _socket_inodes(pid) & inodes:
        return pid
    owner = _socket_owner(inodes)
    owner_text = "an unknown pid (no readable descriptor table holds it)" if owner is None else f"pid {owner}"
    raise RuntimeError(
        f"{self.label} port {self.port} is listened on by {owner_text}, outside the server's process tree")

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

  def stop_server(self) -> int:
    """Stop tmux, SIGTERM the server, then stop the spawn-pool workers and headless browsers it leaves behind.

    Returns the number of headless browsers that it stopped.
    """
    self._kill_tmux_server()
    if self._proc is None or self._proc.poll() is not None:
      return 0
    if self._server_identity is None:
      self._server_identity = self._capture_identity()
    server_pid, pid_start = self._server_identity
    identity = process_identity(server_pid)
    if identity is not None and identity[1] == pid_start and identity[0] != "Z":
      with contextlib.suppress(ProcessLookupError):
        os.kill(server_pid, signal.SIGTERM)
    deadline = time.monotonic() + STOP_TIMEOUT_SECONDS
    while time.monotonic() < deadline:
      identity = process_identity(server_pid)
      if identity is None or identity[0] == "Z" or identity[1] != pid_start:
        self._stop_spawn_pool_workers()
        return self._stop_headless_browsers()
      time.sleep(POLL_INTERVAL_SECONDS)
    if self._proc.poll() is None:
      os.killpg(self._proc.pid, signal.SIGKILL)
      self._proc.wait()
    raise RuntimeError(f"{self.label} server ignored SIGTERM for {STOP_TIMEOUT_SECONDS}s and was killed")

  def _stop_spawn_pool_workers(self) -> None:
    """Stop Python spawn workers left alive by the server's non-blocking executor shutdown."""
    if self._proc is None:
      return
    group = self._proc.pid

    def spawn_worker(pid: int, command: bytes) -> bool:
      return os.getpgid(pid) == group and b"multiprocessing.spawn" in command and b"--multiprocessing-fork" in command

    self._stop_processes(self._processes_matching(spawn_worker), "process-pool workers")

  def _stop_headless_browsers(self) -> int:
    """Stop the headless Chrome that a server left running after SIGTERM, so the server's strace can exit.

    Returns the number of browsers that it stopped.
    """
    profile = f"--user-data-dir={self.tree.tmp / HEADLESS_PROFILE_PREFIX}".encode()
    browsers = self._processes_matching(lambda pid, command: profile in command)
    self._stop_processes(browsers, "headless browsers")
    return len(browsers)

  def _processes_matching(self, matches: Callable[[int, bytes], bool]) -> dict[int, str]:
    """Live processes whose command line *matches* accepts, by pid and start time, so no signal reaches a reused pid."""
    found = {}
    for process in pathlib.Path("/proc").iterdir():
      if not process.name.isdecimal():
        continue
      pid = int(process.name)
      try:
        command = (process / "cmdline").read_bytes()
        if not matches(pid, command):
          continue
      except (FileNotFoundError, ProcessLookupError):
        continue
      identity = process_identity(pid)
      if identity is not None and identity[0] != "Z":
        found[pid] = identity[1]
    return found

  def _stop_processes(self, identities: dict[int, str], label: str) -> None:
    """SIGTERM the processes, then SIGKILL those still running; raise if any survive the SIGKILL."""
    for pid, start_time in identities.items():
      identity = process_identity(pid)
      if identity is not None and identity[0] != "Z" and identity[1] == start_time:
        with contextlib.suppress(ProcessLookupError):
          os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + PROCESS_POOL_STOP_TIMEOUT_SECONDS
    while time.monotonic() < deadline:
      active = _still_running(identities)
      if not active:
        return
      time.sleep(POLL_INTERVAL_SECONDS)
    for pid in active:
      with contextlib.suppress(ProcessLookupError):
        os.kill(pid, signal.SIGKILL)
    deadline = time.monotonic() + PROCESS_POOL_STOP_TIMEOUT_SECONDS
    while time.monotonic() < deadline:
      active = _still_running(identities)
      if not active:
        return
      time.sleep(POLL_INTERVAL_SECONDS)
    raise RuntimeError(f"{self.label} {label} remained after SIGKILL: {active}")

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
    self._kill_tmux_server()
    if self._proc is not None and self._proc.poll() is None:
      os.killpg(self._proc.pid, signal.SIGKILL)
      self._proc.wait()

  def _kill_tmux_server(self) -> None:
    socket_path = self.tree.tmp / f"tmux-{os.getuid()}" / "charliebot"
    if not socket_path.exists():
      return
    environment = dict(os.environ)
    environment["TMUX_TMPDIR"] = str(self.tree.tmp)
    result = subprocess.run(
        ["tmux", "-L", "charliebot", "kill-server"],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if result.returncode and "no server running" not in result.stderr.lower() and socket_path.exists():
      raise RuntimeError(f"{self.label} tmux cleanup failed: {result.stderr.strip() or result.stdout.strip()}")


def process_identity(pid: int) -> tuple[str, str] | None:
  """Read process state and field 22 of ``/proc/<pid>/stat``; return None after the process exits."""
  try:
    stat = pathlib.Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
  except (FileNotFoundError, ProcessLookupError):
    return None
  fields = stat.rsplit(")", 1)[1].split()
  return fields[0], fields[19]


def _still_running(identities: dict[int, str]) -> list[int]:
  """The pids in *identities* that are still the same process: not a zombie, same start time."""
  return [
      pid for pid, start_time in identities.items()
      if (identity := process_identity(pid)) is not None and identity[0] != "Z" and identity[1] == start_time
  ]


# prctl(2) option that makes the calling process the reaper of its orphaned descendants.
PR_SET_CHILD_SUBREAPER = 36


def become_child_subreaper() -> None:
  """Adopt the orphans of this process's descendants, so the harness reaps the fake agents its servers leave.

  A fake agent outlives the server that started it once the harness stops that server. Without this, the nearest
  subreaper above the harness adopts the fake, and the harness never reaps it.
  """
  libc = ctypes.CDLL(None, use_errno=True)
  if libc.prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) != 0:
    error_number = ctypes.get_errno()
    raise OSError(error_number, os.strerror(error_number))


# The /proc/net/tcp state column of a listening socket.
LISTEN_STATE = "0A"


def _listener_inodes(port: int) -> set[int]:
  """The inodes of the listening IPv4 and IPv6 TCP sockets on *port*."""
  inodes: set[int] = set()
  for table in ("/proc/net/tcp", "/proc/net/tcp6"):
    # The first line is the column header.
    for line in pathlib.Path(table).read_text(encoding="utf-8").splitlines()[1:]:
      fields = line.split()
      local_port = int(fields[1].rsplit(":", 1)[1], 16)
      if fields[3] == LISTEN_STATE and local_port == port:
        inodes.add(int(fields[9]))
  return inodes


def _process_tree(root: int) -> list[int]:
  """*root* and every descendant, read from each task's children list."""
  pids = [root]
  for pid in pids:  # the loop also visits the children appended below
    try:
      tasks = list(pathlib.Path(f"/proc/{pid}/task").iterdir())
    except FileNotFoundError:
      continue  # the process exited during the walk
    for task in tasks:
      try:
        children = (task / "children").read_text(encoding="utf-8").split()
      except FileNotFoundError:
        continue  # the task exited during the walk
      pids.extend(int(child) for child in children)
  return pids


def _socket_inodes(pid: int) -> set[int]:
  """The inodes of the sockets in *pid*'s descriptor table; empty once the process exits or its table is unreadable."""
  try:
    descriptors = list((pathlib.Path("/proc") / str(pid) / "fd").iterdir())
  except (FileNotFoundError, PermissionError):
    return set()  # the process exited, or another user's descriptor table is not readable here
  inodes: set[int] = set()
  for descriptor in descriptors:
    try:
      target = os.readlink(descriptor)
    except (FileNotFoundError, PermissionError):
      continue  # the descriptor closed during the scan, or it belongs to another user's process
    if target.startswith("socket:["):
      inodes.add(int(target[len("socket:["):-1]))
  return inodes


def _socket_owner(inodes: set[int]) -> int | None:
  """The pid with a readable descriptor table that holds one of *inodes*, or None when no readable table does."""
  for entry in pathlib.Path("/proc").iterdir():
    if entry.name.isdecimal() and _socket_inodes(int(entry.name)) & inodes:
      return int(entry.name)
  return None


# -- log events -------------------------------------------------------------

# structlog's console renderer, which the Python server logs with: "<date> <time> [<level>] <event> <key>=<value>...".
# A value is bare unless it holds a space, a tab, "=", a quote or a line break; such a value is repr()'d.
_CONSOLE_HEADER = re.compile(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} \[\w+\s*\] (?P<event>\S+)(?P<fields>.*)")
_CONSOLE_FIELD = re.compile(r"\s(?P<key>\w+)=(?P<value>'(?:[^'\\]|\\.)*'|\"(?:[^\"\\]|\\.)*\"|\S+)")
# The console renderer writes Python's spelling of a bool or None; the Rust server's JSON writes the JSON spelling.
_PYTHON_SPELLINGS = {"True": "true", "False": "false", "None": "null"}
# The keys the Rust server adds to each JSON line beside the event's own fields.
_RUST_LINE_KEYS = ("event", "level", "timestamp")


@dataclasses.dataclass(frozen=True)
class LogEvent:
  """One event line of a server log: its name and the text of each field."""

  name: str
  fields: dict[str, str]


def read_log_events(path: pathlib.Path) -> list[LogEvent]:
  """The events of one server log in order: a Rust JSON object per line, or a Python console header.

  A last line still being written has no newline, so it is left for a later read. Any other line, such as a
  traceback, names no event.
  """
  events = []
  for line in path.read_bytes().splitlines(keepends=True):
    if not line.endswith(b"\n"):
      continue
    text = line.decode("utf-8", errors="replace")
    event = _json_event(text) if text.startswith("{") else _console_event(text)
    if event is not None:
      events.append(event)
  return events


def _json_event(text: str) -> LogEvent | None:
  try:
    record = json.loads(text)
  except json.JSONDecodeError:
    return None  # a traceback line that opens with a brace is not a Rust event line
  if not isinstance(record, dict) or not isinstance(record.get("event"), str):
    return None
  return LogEvent(
      record["event"], {
          key: _json_text(value) for key, value in record.items() if key not in _RUST_LINE_KEYS
      })


def _json_text(value: object) -> str:
  """A string as itself; any other JSON scalar as JSON spells it (true, null, 3)."""
  return value if isinstance(value, str) else json.dumps(value)


def _console_event(text: str) -> LogEvent | None:
  header = _CONSOLE_HEADER.match(text)
  if header is None:
    return None
  fields = {field["key"]: _console_text(field["value"]) for field in _CONSOLE_FIELD.finditer(header["fields"])}
  return LogEvent(header["event"], fields)


def _console_text(value: str) -> str:
  if value[0] in "'\"":
    return ast.literal_eval(value)
  return _PYTHON_SPELLINGS.get(value, value)


def wait_for_log_events(
    logs: dict[str, pathlib.Path],
    names: tuple[str, ...],
    fields: dict[str, str],
    timeout_seconds: float,
) -> list[str]:
  """Poll until each side's log holds an event named in *names* with every field in *fields*.

  *logs* maps each side's label to that side's own server log, so one side's event never answers another side's wait.
  Returns one message per side that did not log a match within *timeout_seconds*, naming the side and the events.
  """
  deadline = time.monotonic() + timeout_seconds
  pending = dict(logs)
  while pending:
    pending = {label: path for label, path in pending.items() if not _has_match(read_log_events(path), names, fields)}
    if not pending or time.monotonic() >= deadline:
      break
    time.sleep(POLL_INTERVAL_SECONDS)
  wanted = " or ".join(names)
  if fields:
    wanted += " with " + ", ".join(f"{key}={value}" for key, value in fields.items())
  return [f"{label} server did not log {wanted} within {timeout_seconds}s" for label in pending]


def _has_match(events: list[LogEvent], names: tuple[str, ...], fields: dict[str, str]) -> bool:
  return any(
      event.name in names and all(event.fields.get(key) == value for key, value in fields.items()) for event in events)
