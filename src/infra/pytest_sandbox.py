"""A pytest plugin that restricts every test run to the processes it started.

Registered as a pytest11 entry point, so any pytest run through this project's
venv loads it before conftests and before collection. With
CHARLIEBOT_USE_TEST_SANDBOX=1 (the default) the plugin, in the main thread and
before any test thread starts:

- drops TMUX, XDG_RUNTIME_DIR and DBUS_SESSION_BUS_ADDRESS from the environment,
  so bare tmux and `systemctl --user` calls cannot reach the live server's pane,
  default tmux server, or systemd user manager;
- points TMUX_TMPDIR at a short private directory (a unix socket path holds at
  most 108 bytes) deleted when the run ends;
- enters a fresh user namespace (skipped when the process is already inside an
  unmapped one — a nested pytest cannot create a second one there), sets
  PR_SET_NO_NEW_PRIVS, and applies a Landlock ruleset scoped to signals, so a
  kill aimed outside this run's own descendants fails with EPERM.

Any failed step makes pytest exit non-zero before collection; there is no
fallback. Set the switch to 0 to run unsandboxed.
"""
import atexit
import ctypes
import os
import shutil
import sys
import tempfile

import pytest

SWITCH = "CHARLIEBOT_USE_TEST_SANDBOX"
_LANDLOCK_CREATE_RULESET = 444
_LANDLOCK_ADD_RULE = 445
_LANDLOCK_RESTRICT_SELF = 446
_LANDLOCK_CREATE_RULESET_VERSION = 1
_LANDLOCK_SCOPE_SIGNAL = 1 << 1
_MIN_ABI = 6
_PR_SET_NO_NEW_PRIVS = 38
_DAEMON_ENV_VARS = ("TMUX", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS")


class _RulesetAttr(ctypes.Structure):
  """struct landlock_ruleset_attr as ABI 6 sees it: handled fs, net, and scope masks."""

  _fields_ = [("fs", ctypes.c_uint64), ("net", ctypes.c_uint64), ("scoped", ctypes.c_uint64)]


_libc = ctypes.CDLL(None, use_errno=True)
_libc.syscall.restype = ctypes.c_long
# Set once the sandbox applies; pytest_report_header reads it for the run log.
abi: int | None = None


class _SandboxError(Exception):
  """A failed sandbox step: the call that failed (or the ABI finding) and its errno."""

  def __init__(self, call: str, num: int) -> None:
    super().__init__(call)
    self.call = call
    self.num = num


def landlock_abi() -> int:
  """The kernel's Landlock ABI version, via landlock_create_ruleset(2)'s version probe."""
  version = _libc.syscall(_LANDLOCK_CREATE_RULESET, None, 0, _LANDLOCK_CREATE_RULESET_VERSION)
  if version < 0:
    raise _SandboxError("landlock_create_ruleset(ABI probe)", ctypes.get_errno())
  return version


def _uid_map_empty() -> bool:
  """Whether this process already sits in a user namespace without a uid mapping.

  A process in such a namespace cannot create a nested one (unshare(CLONE_NEWUSER)
  fails with EPERM), so the namespace step is skipped for nested pytest runs.
  """
  with open("/proc/self/uid_map", encoding="ascii") as f:
    return f.read().strip() == ""


def _apply() -> None:
  global abi
  for name in _DAEMON_ENV_VARS:
    os.environ.pop(name, None)
  try:
    tmpdir = tempfile.mkdtemp(prefix="cbpytest-")
  except OSError as e:
    raise _SandboxError("tempfile.mkdtemp", e.errno or 0) from e
  atexit.register(shutil.rmtree, tmpdir)
  os.environ["TMUX_TMPDIR"] = tmpdir
  try:
    uid_map_empty = _uid_map_empty()
  except OSError as e:
    raise _SandboxError("read /proc/self/uid_map", e.errno or 0) from e
  if not uid_map_empty:
    try:
      os.unshare(os.CLONE_NEWUSER)
    except OSError as e:
      raise _SandboxError("os.unshare(CLONE_NEWUSER)", e.errno or 0) from e
  if _libc.prctl(_PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0:
    raise _SandboxError("prctl(PR_SET_NO_NEW_PRIVS)", ctypes.get_errno())
  version = landlock_abi()
  if version < _MIN_ABI:
    raise _SandboxError(f"landlock abi {version} below {_MIN_ABI}", 0)
  abi = version
  ruleset = _RulesetAttr(0, 0, _LANDLOCK_SCOPE_SIGNAL)
  fd = _libc.syscall(_LANDLOCK_CREATE_RULESET, ctypes.byref(ruleset), ctypes.sizeof(ruleset), 0)
  if fd < 0:
    raise _SandboxError("landlock_create_ruleset", ctypes.get_errno())
  try:
    if _libc.syscall(_LANDLOCK_RESTRICT_SELF, fd, 0) != 0:
      raise _SandboxError("landlock_restrict_self", ctypes.get_errno())
  finally:
    os.close(fd)


@pytest.hookimpl(tryfirst=True)
def pytest_load_initial_conftests(early_config: pytest.Config, parser: pytest.Parser, args: list[str]) -> None:
  """Apply the sandbox before any conftest loads or any thread starts."""
  if os.environ.get(SWITCH, "1") != "1":
    return
  try:
    _apply()
  except _SandboxError as e:
    detail = f" (errno {e.num}, {os.strerror(e.num)})" if e.num else ""
    sys.stderr.write(f"pytest sandbox: {e.call} failed{detail}; "
                     f"set {SWITCH}=0 to run without the sandbox\n")
    raise SystemExit(2) from e


def pytest_report_header(config: pytest.Config) -> str | None:
  """Show the Landlock ABI the run restricted itself under."""
  if abi is None:
    return None
  return f"landlock abi: {abi}"
