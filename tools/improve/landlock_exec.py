#!/usr/bin/env python3
"""Run one improve round's command under a write-only Landlock boundary.

Usage: landlock_exec.py <direction> -- <command> [args...]

The boundary limits writes only: every read stays open, so the round reads the checkout, the
cargo registry, the mock data and the instructions file as before. Writes are allowed under the
round's own directories and the caches every direction shares, and denied everywhere else. The
live CharlieBot home, the runtime clone, the other directions' clones and the host worker's
trees therefore stay out of reach of a round that writes the wrong path or runs `rm`:

  writable for this direction only
    ~/improve/<direction>/charlie-bot/     the direction's clone
    ~/improve/<direction>/evidence/        the round's screenshots, logs and summaries
    ~/improve/<direction>/clc-sessions/    the CLC reviewer's sessions
    /tmp/improve-<direction>/              the round's trial trees and scratch
    /tmp/improve-<direction>-tmp/          TMPDIR
    ~/.cache/improve-target/<direction>    the cargo target directory
  writable for every direction
    ~/improve/mock-home/                   the generated mock data and its manifest
    ~/.cargo/registry, ~/.cargo/git, ~/.cargo/.package-cache
    ~/.claude-improve                      the worker's own Claude config directory
  no write rule, so read-only inside the boundary
    ~/improve/home/                        config.yaml and credentials.yaml
    ~/improve/launch/<launch id>/          the instructions file and the in-flight files list
    ~/improve/<direction>/memory-peaks.tsv

The device paths below carry write rules because tools write to them: /dev/null and the other
null and random devices, /dev/tty for an interactive tool, /dev/ptmx and /dev/pts for a new
terminal, and /dev/shm for a browser's shared memory.

The launcher creates each missing allow-list directory, because the rules bind to objects that
exist when it starts. It stops with an error when the kernel cannot enforce Landlock: a round
without the boundary would write anywhere on the host. ABI 3 is required for the truncate rule.
"""
import ctypes
import os
import re
import struct
import sys

SYS_CREATE, SYS_ADD, SYS_RESTRICT = 444, 445, 446
LANDLOCK_CREATE_RULESET_VERSION = 1
WRITE_FILE = 1 << 1
TRUNCATE = 1 << 14
# Every write class: WRITE_FILE, the removals and creations of each object type, REFER (ABI 2) and TRUNCATE (ABI 3).
WRITE_ACCESS = sum(1 << bit for bit in (1, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14))
FILE_ACCESS = WRITE_FILE | TRUNCATE
RULE_PATH_BENEATH = 1
PR_SET_NO_NEW_PRIVS = 38
REQUIRED_ABI = 3
USAGE = "usage: landlock_exec.py <direction> -- <command> [args...]"
DIRECTION_PATTERN = re.compile(r"^[a-z][a-z0-9_]*$")

libc = ctypes.CDLL(None, use_errno=True)

DEVICE_PATHS = (
  "/dev/null",
  "/dev/zero",
  "/dev/full",
  "/dev/random",
  "/dev/urandom",
  "/dev/tty",
  "/dev/ptmx",
  "/dev/pts",
  "/dev/shm",
)


def check(ret, what):
  if ret < 0:
    err = ctypes.get_errno()
    raise OSError(err, f"{what}: {os.strerror(err)}")
  return ret


def direction_paths(home, direction):
  """The writable paths of one direction, and the writable paths every direction shares."""
  round_paths = [
    f"{home}/improve/{direction}/charlie-bot",
    f"{home}/improve/{direction}/evidence",
    f"{home}/improve/{direction}/clc-sessions",
    f"/tmp/improve-{direction}",
    f"/tmp/improve-{direction}-tmp",
    f"{home}/.cache/improve-target/{direction}",
  ]
  shared_paths = [
    f"{home}/improve/mock-home",
    f"{home}/.cargo/registry",
    f"{home}/.cargo/git",
    f"{home}/.cargo/.package-cache",
    f"{home}/.claude-improve",
  ]
  return round_paths, shared_paths


def parse_argv(argv):
  if len(argv) < 4 or argv[2] != "--":
    raise SystemExit(USAGE)
  direction = argv[1]
  if not DIRECTION_PATTERN.match(direction):
    raise SystemExit(f"not an improve direction: {direction!r}")
  return direction, argv[3:]


def writable_directories(round_paths, shared_paths):
  """Create each missing allow-list directory; a rule needs an existing object.

  A device path must exist: a missing one would turn every write to it into a denial later.
  """
  for path in DEVICE_PATHS:
    if not os.path.exists(path):
      raise SystemExit(f"device path {path} does not exist")
  for path in round_paths + shared_paths:
    if not os.path.lexists(path):
      os.makedirs(path, exist_ok=True)


def landlock_abi():
  """Return the Landlock ABI version of this kernel, or stop when it has none."""
  ret = libc.syscall(SYS_CREATE, None, 0, LANDLOCK_CREATE_RULESET_VERSION)
  if ret < 0:
    err = ctypes.get_errno()
    raise SystemExit(f"this kernel cannot enforce Landlock ({os.strerror(err)}); the round would run unguarded")
  return ret


def restrict(write_allowed):
  abi = landlock_abi()
  if abi < REQUIRED_ABI:
    raise SystemExit(f"Landlock ABI {abi} cannot enforce truncate; ABI {REQUIRED_ABI} is required")
  attr = struct.pack("=Q", WRITE_ACCESS)
  ruleset = check(libc.syscall(SYS_CREATE, attr, len(attr), 0), "landlock_create_ruleset")
  for path in write_allowed:
    access = WRITE_ACCESS
    if not os.path.isdir(path):
      access &= FILE_ACCESS
    fd = os.open(path, os.O_PATH | os.O_CLOEXEC)
    rule = struct.pack("=Qi", access, fd)
    check(libc.syscall(SYS_ADD, ruleset, RULE_PATH_BENEATH, rule, 0), f"landlock_add_rule {path}")
    os.close(fd)
  check(libc.prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), "prctl NO_NEW_PRIVS")
  check(libc.syscall(SYS_RESTRICT, ruleset, 0), "landlock_restrict_self")
  return abi


def main(argv):
  direction, cmd = parse_argv(argv)
  home = os.path.expanduser("~")
  round_paths, shared_paths = direction_paths(home, direction)
  writable_directories(round_paths, shared_paths)
  write_allowed = sorted({os.path.realpath(path) for path in round_paths + shared_paths + list(DEVICE_PATHS)})
  abi = restrict(write_allowed)
  print(f"improve boundary: write-only Landlock ABI {abi}, {len(write_allowed)} writable paths, {direction}",
        file=sys.stderr)
  os.execvp(cmd[0], cmd)


if __name__ == "__main__":
  main(sys.argv)
