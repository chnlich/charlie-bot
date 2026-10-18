"""Exec a command under a Landlock ruleset that allows filesystem writes only beneath the given paths.

Usage: python sandbox.py --allow-write DIR [--allow-write DIR ...] -- ARGV...

A process under test can only change files beneath its own temporary tree (and write to a few device nodes): a
write, create, remove or rename anywhere else fails with EACCES before it lands. It can open a TCP connection only
to the ports in CONNECT_PORTS, so a client that ignores the proxy variables cannot reach port 22, 80 or 443. Reads,
execution and bind stay unrestricted. Landlock does not cover metadata calls (chmod, chown, utimes, xattrs; guard.py's
trace covers them), UDP, Unix sockets, or the destination address of a connect: a connect to any host on a port of
1024 or more is allowed. The ruleset holds across exec and is inherited by every child, so it binds the whole process
tree.
"""

import argparse
import ctypes
import os
import pathlib
import struct
import sys

_SYS_LANDLOCK_CREATE_RULESET = 444
_SYS_LANDLOCK_ADD_RULE = 445
_SYS_LANDLOCK_RESTRICT_SELF = 446
_LANDLOCK_CREATE_RULESET_VERSION = 1
_LANDLOCK_RULE_PATH_BENEATH = 1
_LANDLOCK_RULE_NET_PORT = 2
_LANDLOCK_NET_ABI = 4  # the first ABI with the TCP access rights (Linux 6.7)
_LANDLOCK_ACCESS_NET_CONNECT_TCP = 1 << 1
_PR_SET_NO_NEW_PRIVS = 38

_WRITE_FILE = 1 << 1
_REMOVE_DIR = 1 << 4
_REMOVE_FILE = 1 << 5
_MAKE_CHAR = 1 << 6
_MAKE_DIR = 1 << 7
_MAKE_REG = 1 << 8
_MAKE_SOCK = 1 << 9
_MAKE_FIFO = 1 << 10
_MAKE_BLOCK = 1 << 11
_MAKE_SYM = 1 << 12
_REFER = 1 << 13  # ABI 2
_TRUNCATE = 1 << 14  # ABI 3

# Device nodes a process may open for writing: writes to them change no file.
_WRITABLE_DEVICES = ("/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom", "/dev/tty", "/dev/ptmx")
_WRITABLE_DEVICE_DIRS = ("/dev/pts",)
# ProcessPoolExecutor's spawn context creates POSIX semaphores here; guard.py checks every other shared-memory path.
_WRITABLE_PROCESS_POOL_DIRS = ("/dev/shm",)
# The TCP ports a process under test may connect to: 9 is servers.CLOSED_PROXY, and 1024-65535 are the ports an
# unprivileged process uses. Landlock cannot express a range, so each port gets its own rule.
CONNECT_PORTS = (9, *range(1024, 65536))


def _syscall(libc: ctypes.CDLL, number: int, *args: object) -> int:
  result = libc.syscall(number, *args)
  if result < 0:
    errno = ctypes.get_errno()
    raise OSError(errno, f"syscall {number} failed: {os.strerror(errno)}")
  return result


def _add_rule(libc: ctypes.CDLL, ruleset_fd: int, path: str, access: int) -> None:
  fd = os.open(path, os.O_PATH | os.O_CLOEXEC)
  try:
    # struct landlock_path_beneath_attr is packed: a u64 access mask, then the s32 parent fd.
    attr = ctypes.create_string_buffer(struct.pack("=Qi", access, fd), 12)
    _syscall(libc, _SYS_LANDLOCK_ADD_RULE, ruleset_fd, _LANDLOCK_RULE_PATH_BENEATH, attr, 0)
  finally:
    os.close(fd)


def _add_connect_ports(libc: ctypes.CDLL, ruleset_fd: int) -> None:
  """Allow TCP connect to each port of CONNECT_PORTS; the kernel copies each attribute during its call."""
  attr = ctypes.create_string_buffer(16)
  for port in CONNECT_PORTS:
    # struct landlock_net_port_attr: the allowed network access mask, then the port in host byte order.
    struct.pack_into("=QQ", attr, 0, _LANDLOCK_ACCESS_NET_CONNECT_TCP, port)
    _syscall(libc, _SYS_LANDLOCK_ADD_RULE, ruleset_fd, _LANDLOCK_RULE_NET_PORT, attr, 0)


def restrict_writes(allowed_dirs: list[pathlib.Path]) -> None:
  """Allow writes only beneath *allowed_dirs* (and to the device nodes above) and TCP connects only to CONNECT_PORTS.

  The rules hold for this process and its children. Below Landlock ABI 4 the function exits instead, because an
  unrestricted connect would defeat the rule.
  """
  libc = ctypes.CDLL(None, use_errno=True)
  libc.syscall.restype = ctypes.c_long
  abi = _syscall(libc, _SYS_LANDLOCK_CREATE_RULESET, None, 0, _LANDLOCK_CREATE_RULESET_VERSION)
  if abi < _LANDLOCK_NET_ABI:
    sys.exit(
        f"sandbox.py needs Landlock ABI {_LANDLOCK_NET_ABI} (Linux 6.7 or newer) for the TCP connect rule; "
        f"this kernel reports ABI {abi}")
  dir_access = (
      _WRITE_FILE | _REMOVE_DIR | _REMOVE_FILE | _MAKE_CHAR | _MAKE_DIR | _MAKE_REG | _MAKE_SOCK | _MAKE_FIFO |
      _MAKE_BLOCK | _MAKE_SYM | _REFER | _TRUNCATE)
  file_access = _WRITE_FILE | _TRUNCATE
  # struct landlock_ruleset_attr at ABI 4: the handled filesystem access mask, then the handled network access mask.
  ruleset_attr = struct.pack("=QQ", dir_access, _LANDLOCK_ACCESS_NET_CONNECT_TCP)
  ruleset_fd = _syscall(
      libc, _SYS_LANDLOCK_CREATE_RULESET, ctypes.create_string_buffer(ruleset_attr), len(ruleset_attr), 0)
  try:
    for directory in allowed_dirs:
      _add_rule(libc, ruleset_fd, str(directory), dir_access)
    for device in _WRITABLE_DEVICES:
      if os.path.exists(device):
        _add_rule(libc, ruleset_fd, device, file_access)
    for device_dir in _WRITABLE_DEVICE_DIRS:
      if os.path.isdir(device_dir):
        _add_rule(libc, ruleset_fd, device_dir, file_access)
    for process_pool_dir in _WRITABLE_PROCESS_POOL_DIRS:
      if os.path.isdir(process_pool_dir):
        _add_rule(libc, ruleset_fd, process_pool_dir, dir_access)
    _add_connect_ports(libc, ruleset_fd)
    if libc.prctl(_PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0:
      errno = ctypes.get_errno()
      raise OSError(errno, f"prctl(PR_SET_NO_NEW_PRIVS) failed: {os.strerror(errno)}")
    _syscall(libc, _SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd, 0)
  finally:
    os.close(ruleset_fd)


def main() -> None:
  parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
  parser.add_argument("--allow-write", action="append", required=True, type=pathlib.Path)
  parser.add_argument("argv", nargs=argparse.REMAINDER)
  args = parser.parse_args()
  argv = args.argv[1:] if args.argv[:1] == ["--"] else args.argv
  if not argv:
    parser.error("no command given after --")
  for directory in args.allow_write:
    if not directory.is_absolute() or not directory.is_dir():
      parser.error(f"--allow-write needs an existing absolute directory: {directory}")
  restrict_writes(args.allow_write)
  os.execvp(argv[0], argv)


if __name__ == "__main__":
  sys.exit(main())
