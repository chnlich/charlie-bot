"""Outside-write guard: list every file a process under test changed, or tried to change, outside its tree.

Each process under test runs under ``strace -f -y`` (servers.traced_argv) inside sandbox.py's Landlock ruleset.
The guard reads the trace and resolves every file-changing syscall to an absolute path: the path strace
prints for the returned descriptor (``-y``), the directory descriptor plus the relative path for the ``*at``
calls, or the calling thread's working directory for the plain calls, tracked through chdir, fchdir and every
clone, fork and vfork. A completed change outside the allowed roots is a violation and fails the run. A change
the sandbox refused (EACCES or EPERM) changed nothing; the guard lists it as refused, for the record.

A call strace prints as ``???`` (its syscall number was unreadable, typically a thread that died inside the call)
names no path, so the guard counts it as undecodable and judges nothing about it. The sandbox refuses every write
outside the tree, so such a call cannot have changed a file there.

Why a syscall trace and not a filesystem scan: the host is shared, so a before-and-after scan of HOME or /tmp
sees every other process's writes (the live server, other worktrees, editors) and cannot attribute them, while
inotify cannot watch a whole filesystem and fanotify needs CAP_SYS_ADMIN. The trace attributes every change to
the process tree under test, child processes included, and records the refused attempts as well. The sandbox
turns a write the guard might misparse into a refused one, so a parser gap cannot become damage.
"""

import dataclasses
import os
import pathlib
import re

# Device nodes a process may write: writing them changes no file. sandbox.py allows the same set.
EXEMPT_PATHS = frozenset(
    {"/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom", "/dev/tty", "/dev/ptmx"})
EXEMPT_PREFIXES = ("/dev/pts/",)
REFUSED_ERRNOS = frozenset({"EACCES", "EPERM", "EROFS"})
_WRITE_OPEN_FLAGS = ("O_WRONLY", "O_RDWR", "O_CREAT", "O_TRUNC")

_LINE = re.compile(r"^(?P<pid>\d+)\s+(?P<rest>.*)$")
_SHARED_MEMORY_SEMAPHORE = re.compile(r"/dev/shm/sem\.[^/]+")
# Chromium's anonymous shared memory: a file in /dev/shm that Chromium unlinks as soon as it is made.
_CHROMIUM_SHARED_MEMORY = re.compile(r"/dev/shm/\.org\.chromium\.Chromium\.[A-Za-z0-9]{6}")
_UNFINISHED = re.compile(r"^(?P<call>.*) <unfinished \.\.\.>$")
_RESUMED = re.compile(r"^<\.\.\. (?P<name>\w+|\?\?\?) resumed>(?P<tail>.*)$")
_CALL = re.compile(r"^(?P<name>\w+|\?\?\?)\((?P<args>.*)\)\s+= (?P<ret>.*)$", re.DOTALL)
_ANNOTATED_FD = re.compile(r"^(?P<fd>-?\d+|AT_FDCWD)<(?P<path>.*)>$", re.DOTALL)
_RETURN_FD_PATH = re.compile(r"^\d+<(?P<path>.*)>$", re.DOTALL)
_RETURN_ERROR = re.compile(r"^-1 (?P<errno>E[A-Z0-9]+)\b")
_CLONE_FLAGS = re.compile(r"flags=(?P<flags>[A-Z0-9_|]+)")
_SUN_PATH = re.compile(r'sun_path="(?P<path>[^"]*)"')

# The argument positions that name the changed path(s) of each syscall: ("path", i) is a plain path argument
# resolved against the cwd, ("at", d, i) a directory-descriptor argument d plus path argument i, ("fd", i) a
# descriptor argument annotated with its path.
_PATH_ARGS: dict[str, tuple[tuple, ...]] = {
    "creat": (("path", 0),),
    "mkdir": (("path", 0),),
    "mkdirat": (("at", 0, 1),),
    "mknod": (("path", 0),),
    "mknodat": (("at", 0, 1),),
    "rename": (("path", 0), ("path", 1)),
    "renameat": (("at", 0, 1), ("at", 2, 3)),
    "renameat2": (("at", 0, 1), ("at", 2, 3)),
    "link": (("path", 1),),
    "linkat": (("at", 2, 3),),
    "symlink": (("path", 1),),
    "symlinkat": (("at", 1, 2),),
    "unlink": (("path", 0),),
    "unlinkat": (("at", 0, 1),),
    "rmdir": (("path", 0),),
    "truncate": (("path", 0),),
    "chmod": (("path", 0),),
    "fchmod": (("fd", 0),),
    "fchmodat": (("at", 0, 1),),
    "fchmodat2": (("at", 0, 1),),
    "chown": (("path", 0),),
    "lchown": (("path", 0),),
    "fchown": (("fd", 0),),
    "fchownat": (("at", 0, 1),),
    "utime": (("path", 0),),
    "utimes": (("path", 0),),
    "utimensat": (("at", 0, 1),),
    "futimesat": (("at", 0, 1),),
    "setxattr": (("path", 0),),
    "lsetxattr": (("path", 0),),
    "fsetxattr": (("fd", 0),),
    "removexattr": (("path", 0),),
    "lremovexattr": (("path", 0),),
    "fremovexattr": (("fd", 0),),
}


@dataclasses.dataclass(frozen=True)
class Change:
  """One file-changing syscall outside the allowed roots."""

  trace: str
  pid: int
  syscall: str
  path: str
  outcome: str  # "done", or the errno that refused it

  def describe(self) -> str:
    return f"{self.syscall} {self.path} ({self.outcome}; pid {self.pid} in {self.trace})"


@dataclasses.dataclass
class Report:
  violations: list[Change] = dataclasses.field(default_factory=list)
  refused: list[Change] = dataclasses.field(default_factory=list)
  # The trace file path of each undecodable call, one entry per call.
  undecodable: list[str] = dataclasses.field(default_factory=list)
  traces: int = 0


def _merged_calls(lines: list[str], trace_log: pathlib.Path) -> list[tuple[int, str]]:
  """(pid, "name(args) = ret") per completed syscall, in the order the calls started.

  A call another thread interrupted prints as ``<unfinished ...>`` and later ``<... name resumed>``; the halves
  are joined and the call keeps the position of its start, so a clone's child pid is known before the
  child's first call.
  """
  calls: list[tuple[int, str] | None] = []
  pending: dict[int, tuple[int, str]] = {}
  for line in lines:
    match = _LINE.match(line)
    if not match:
      raise ValueError(f"unparsed strace line in {trace_log}: {line!r}")
    pid, rest = int(match["pid"]), match["rest"]
    if rest.startswith(("---", "+++")):
      continue
    if rest == "???( <detached ...>":
      # strace detached from a thread killed inside a call whose number it could not read: an undecodable call.
      pending.pop(pid, None)
      calls.append((pid, "???()  = ?"))
      continue
    unfinished = _UNFINISHED.match(rest)
    if unfinished:
      pending[pid] = (len(calls), unfinished["call"])
      calls.append(None)
      continue
    resumed = _RESUMED.match(rest)
    if resumed:
      if pid not in pending:
        raise ValueError(f"resumed call without its start in {trace_log}: {line!r}")
      slot, head = pending.pop(pid)
      calls[slot] = (pid, head + resumed["tail"])
      continue
    calls.append((pid, rest))
  # A call still unfinished when the trace ended never returned (the process was killed inside it).
  return [call for call in calls if call is not None]


def _split_args(args: str) -> list[str]:
  """Top-level comma-separated arguments; quoted strings, braces, brackets and fd annotations stay whole."""
  parts: list[str] = []
  depth = 0
  quoted = False
  escaped = False
  start = 0
  for index, char in enumerate(args):
    if quoted:
      if escaped:
        escaped = False
      elif char == "\\":
        escaped = True
      elif char == '"':
        quoted = False
      continue
    if char == '"':
      quoted = True
    elif char in "{[(<":
      depth += 1
    elif char in "}])>":
      depth -= 1
    elif char == "," and depth == 0:
      parts.append(args[start:index].strip())
      start = index + 1
  parts.append(args[start:].strip())
  return parts


def _unquote(arg: str) -> str:
  """A strace string argument as the path the kernel saw."""
  if not (arg.startswith('"') and arg.endswith('"')):
    raise ValueError(f"expected a quoted path argument, got {arg!r}")
  return arg[1:-1].encode("latin-1").decode("unicode_escape").encode("latin-1").decode("utf-8", "surrogateescape")


def _annotated_path(arg: str) -> str:
  match = _ANNOTATED_FD.match(arg)
  if not match:
    raise ValueError(f"descriptor argument without a -y path: {arg!r}")
  return match["path"]


class _Cwds:
  """Each thread's working directory. Threads created with CLONE_FS share one cell; other children copy it.

  strace can print a child's first calls before the line of the clone, fork or vfork that created it (a vfork
  parent stays blocked while its child runs), so the creating calls are read in a first pass and a child takes
  its parent's directory at its first call.
  """

  def __init__(self, first_pid: int, cwd: str, parents: dict[int, tuple[int, bool]]) -> None:
    self._cells: dict[int, list[str]] = {first_pid: [cwd]}
    self._parents = parents

  def _cell(self, pid: int) -> list[str]:
    if pid not in self._cells:
      if pid not in self._parents:
        raise ValueError(f"pid {pid} has no known working directory (its creating call is missing from the trace)")
      parent, shares_fs = self._parents[pid]
      cell = self._cell(parent)
      self._cells[pid] = cell if shares_fs else [cell[0]]
    return self._cells[pid]

  def get(self, pid: int) -> str:
    return self._cell(pid)[0]

  def chdir(self, pid: int, path: str) -> None:
    self._cell(pid)[0] = path


def _resolve(spec: tuple, args: list[str], pid: int, cwds: _Cwds) -> str:
  if spec[0] == "path":
    path = _unquote(args[spec[1]])
    return os.path.normpath(os.path.join(cwds.get(pid), path))
  if spec[0] == "at":
    path_arg = args[spec[2]]
    if path_arg == "NULL":
      # utimensat(fd, NULL, ...) acts on the descriptor itself.
      return _annotated_path(args[spec[1]])
    path = _unquote(path_arg)
    if path.startswith("/"):
      return os.path.normpath(path)
    return os.path.normpath(os.path.join(_annotated_path(args[spec[1]]), path))
  return _annotated_path(args[spec[1]])


def _open_paths(name: str, args: list[str], ret: str, pid: int, cwds: _Cwds) -> list[str]:
  """The path a write-mode open touched, or nothing for a read-only open."""
  if name == "open":
    flags, spec = args[1], ("path", 0)
  elif name == "openat":
    flags, spec = args[2], ("at", 0, 1)
  else:  # openat2: the flags sit in the open_how struct.
    flags, spec = args[2], ("at", 0, 1)
  if not any(flag in flags for flag in _WRITE_OPEN_FLAGS):
    return []
  returned = _RETURN_FD_PATH.match(ret)
  return [returned["path"] if returned else _resolve(spec, args, pid, cwds)]


def _outcome(ret: str) -> str | None:
  """"done" for a completed call, the errno for a refused one, None for any other failure (nothing changed).

  A call whose result strace could not see (``= ?``: the process died inside it, or a signal will restart it)
  counts as "unknown" and is judged like a completed one.
  """
  if ret.startswith("?"):
    return "unknown"
  error = _RETURN_ERROR.match(ret)
  if not error:
    return "done"
  return error["errno"] if error["errno"] in REFUSED_ERRNOS else None


def _inside(path: str, roots: tuple[str, ...]) -> bool:
  # POSIX semaphores are process-pool coordination files, not application data; Chromium's shared memory lives
  # only until its unlink, so a headless browser's allocations leave no file behind.
  return path in EXEMPT_PATHS or path.startswith(EXEMPT_PREFIXES) or _SHARED_MEMORY_SEMAPHORE.fullmatch(
      path) is not None or _CHROMIUM_SHARED_MEMORY.fullmatch(path) is not None or any(
          path == root or path.startswith(root + "/") for root in roots)


def scan(trace_log: pathlib.Path, roots: tuple[pathlib.Path, ...], start_cwd: pathlib.Path, report: Report) -> None:
  """Add every change outside *roots* that *trace_log* records to *report*.

  *start_cwd* is the working directory the traced process started in.
  """
  report.traces += 1
  root_strings = tuple(os.path.realpath(root) for root in roots)
  calls = _merged_calls(trace_log.read_text(encoding="utf-8", errors="surrogateescape").splitlines(), trace_log)
  if not calls:
    raise ValueError(f"empty trace: {trace_log}")
  parsed: list[tuple[int, re.Match]] = []
  parents: dict[int, tuple[int, bool]] = {}
  for pid, call in calls:
    match = _CALL.match(call)
    if not match:
      raise ValueError(f"unparsed syscall in {trace_log}: {call!r}")
    parsed.append((pid, match))
    if match["name"] in ("clone", "clone3", "fork", "vfork") and _outcome(match["ret"]) == "done":
      flags = _CLONE_FLAGS.search(match["args"])
      parents[int(match["ret"].split()[0])] = (pid, bool(flags) and "CLONE_FS" in flags["flags"])
  cwds = _Cwds(calls[0][0], str(start_cwd), parents)
  for pid, match in parsed:
    name, ret = match["name"], match["ret"]
    if name == "???":
      report.undecodable.append(str(trace_log))
      continue
    if name in ("clone", "clone3", "fork", "vfork"):
      continue
    outcome = _outcome(ret)
    args = _split_args(match["args"])
    if name == "chdir":
      if outcome == "done":
        cwds.chdir(pid, _resolve(("path", 0), args, pid, cwds))
      continue
    if name == "fchdir":
      if outcome == "done":
        cwds.chdir(pid, _annotated_path(args[0]))
      continue
    if outcome is None:
      continue
    if name in ("open", "openat", "openat2"):
      paths = _open_paths(name, args, ret, pid, cwds)
    elif name == "bind":
      sun_path = _SUN_PATH.search(match["args"])
      # Only a filesystem AF_UNIX address creates a file; abstract names start with "@" in strace output.
      paths = [os.path.normpath(os.path.join(cwds.get(pid), sun_path["path"]))] if sun_path else []
    elif name in _PATH_ARGS:
      paths = [_resolve(spec, args, pid, cwds) for spec in _PATH_ARGS[name]]
    else:
      raise ValueError(f"traced syscall without a path rule: {name}")
    for path in paths:
      if _inside(path, root_strings):
        continue
      change = Change(trace_log.name, pid, name, path, outcome)
      (report.refused if outcome in REFUSED_ERRNOS else report.violations).append(change)
