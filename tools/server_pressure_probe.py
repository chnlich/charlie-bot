#!/usr/bin/env python3
"""Run a bounded pressure probe against an isolated CharlieBot session copy.

The public command runs memory, delayed-read, first-tree, and composition
measurements. The server and pressure workers use private entry points in this
file so systemd can place each process in the bounded user slice.
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import ctypes
import datetime
import errno
import itertools
import json
import math
import os
import pathlib
import platform
import random
import re
import shutil
import signal
import socket
import stat
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid
from collections.abc import Callable
from typing import Any

REPO_ROOT = pathlib.Path(__file__).resolve().parents[1]
LIVE_SESSIONS = pathlib.Path.home() / ".charliebot" / "sessions"
LIVE_CREDENTIALS = pathlib.Path.home() / ".charliebot" / "credentials.yaml"
LIVE_LOG = pathlib.Path("/tmp/charliebot-logs/server-latest.log")
DEFAULT_RESULTS_DIR = pathlib.Path.home() / "scripts" / "20261023_server_protection" / "pressure"
CGROUP_ROOT = pathlib.Path("/sys/fs/cgroup")
PRESSURE_SLICE = "charliebot-pressure-test.slice"
PRESSURE_AGENTS_SLICE = "charliebot-pressure-test-agents.slice"
PRESSURE_REPAIR_SLICE = "charliebot-pressure-test-repair.slice"
LIVE_URL = "http://127.0.0.1:18498/api/internal/version"
BOOT_MARKER = "2026-10-23T13:09:49.351645+00:00"
GIB = 1024**3
MIB = 1024**2
PARENT_MEMORY_MAX = 8 * GIB
PARENT_SWAP_MAX = 512 * MIB
SERVER_MEMORY_MAX = 3 * GIB
COMPOSITION_MEMORY_MAX = 2 * GIB
MEMAVAILABLE_FLOOR = 12 * GIB
GUARD_PROBE_INTERVAL = 1.0
GUARD_SLOW_SECONDS = 1.0
GUARD_FAST_THAW_SECONDS = 30.0
LIVE_PROBE_TIMEOUT_SECONDS = 10.0
MAX_SESSION_COUNT = 64
MAX_COMPOSITION_SESSION_COUNT = 16
DEFAULT_COMPOSITION_SESSION_COUNT = 9
MAX_SAMPLE_COUNT = 8
MAX_MEMORY_PRESSURE_GIB = 7.0
MEMORY_PRESSURE_SWAP_EXCESS_BYTES = 384 * MIB
AGENTS_MEMORY_HIGH = int(3.5 * GIB)
AGENTS_MEMORY_MAX = 4 * GIB
REPAIR_MEMORY_MAX = 768 * MIB
REPAIR_PROBE_BYTES = 256 * MIB
REPAIR_PROBE_RUNS = 8
VM_SWAP_SAMPLE_INTERVAL_S = 1.0
PRESSURE_WINDOW_START_TIMEOUT_S = 600.0
PRESSURE_KILL_OBSERVATION_TIMEOUT_S = 2400.0
KILL_FILL_MAX_BYTES = 2 * GIB
KILL_FILL_DIR = pathlib.Path("/dev/shm")
MAX_KILL_RESTARTS = 64
MAX_DISK_DELAY_MS = 1.0
MAX_LIVE_EVENT_GIB = 1.2
MAX_COPIED_SESSIONS = 128
COLD_STANDARD_P95_RATIO = 2.0
COLD_STANDARD_LONGEST_SLACK_MS = 5000.0
SERVER_PATH = "/usr/bin:/bin"
AGENT_CLI_NAMES = ("antigravity", "charlie-code", "claude", "codex", "gemini", "opencode")


def utc_now() -> str:
  return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds")


def flush_json(path: pathlib.Path, value: object) -> None:
  path.parent.mkdir(parents=True, exist_ok=True)
  temporary = path.with_name(path.name + ".tmp")
  with temporary.open("w", encoding="utf-8") as stream:
    json.dump(value, stream, indent=2, sort_keys=True)
    stream.write("\n")
    stream.flush()
    os.fsync(stream.fileno())
  os.replace(temporary, path)
  directory_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
  try:
    os.fsync(directory_fd)
  finally:
    os.close(directory_fd)


def assert_linked_files_unwritable(
    linked_files: list[pathlib.Path], open_path: Callable[[pathlib.Path, int], int] | None = None) -> None:
  """Raise before serving when any hard-linked session file opens for writing."""
  opener = os.open if open_path is None else open_path
  for path in linked_files:
    try:
      fd = opener(path, os.O_WRONLY | os.O_APPEND | os.O_CLOEXEC)
    except OSError as exc:
      if exc.errno in (errno.EACCES, errno.EPERM, errno.EROFS):
        continue
      raise
    os.close(fd)
    raise RuntimeError(f"test server can open a linked session file for writing: {path}")


def read_memavailable_bytes() -> int:
  with pathlib.Path("/proc/meminfo").open(encoding="utf-8") as stream:
    for line in stream:
      name, separator, value = line.partition(":")
      if separator and name == "MemAvailable":
        return int(value.split()[0]) * 1024
  raise RuntimeError("/proc/meminfo has no MemAvailable field")


def live_http_probe(access_key: str) -> int:
  request = urllib.request.Request(
      LIVE_URL,
      headers={"Authorization": f"Bearer {access_key}"},
      method="GET",
  )
  try:
    with urllib.request.urlopen(request, timeout=LIVE_PROBE_TIMEOUT_SECONDS) as response:
      return response.status
  except urllib.error.HTTPError as exc:
    return exc.code


class PressureGuard:
  """Probe the live server and freeze only the private test cgroup when it slows."""

  def __init__(
      self,
      *,
      probe: Callable[[], int],
      memory_available: Callable[[], int],
      freeze: Callable[[], None],
      thaw: Callable[[], None],
      stop_slice: Callable[[], None],
      clock: Callable[[], float] = time.monotonic,
      wall_clock: Callable[[], str] = utc_now,
      floor_bytes: int = MEMAVAILABLE_FLOOR,
      slow_seconds: float = GUARD_SLOW_SECONDS,
      fast_thaw_seconds: float = GUARD_FAST_THAW_SECONDS,
  ) -> None:
    self._probe = probe
    self._memory_available = memory_available
    self._freeze = freeze
    self._thaw = thaw
    self._stop_slice = stop_slice
    self._clock = clock
    self._wall_clock = wall_clock
    self._floor_bytes = floor_bytes
    self._slow_seconds = slow_seconds
    self._fast_thaw_seconds = fast_thaw_seconds
    self.stop_event = threading.Event()
    self.failure: Exception | None = None
    self.samples: list[dict[str, Any]] = []
    self.freeze_intervals: list[dict[str, Any]] = []
    self._state_lock = threading.Lock()
    self._frozen = False
    self._freeze_started: float | None = None
    self._freeze_started_at: str | None = None
    self._fast_since: float | None = None
    self._minimum_available: int | None = None

  def check_once(self) -> dict[str, Any]:
    available_before = self._memory_available()
    self._record_available(available_before)
    if available_before < self._floor_bytes:
      self._stop_for("MemAvailable fell below 12 GiB")
      raise RuntimeError("pressure run stopped because MemAvailable fell below 12 GiB")

    started = self._clock()
    status_code: int | None = None
    probe_error: str | None = None
    slow_timer = threading.Timer(self._slow_seconds, self._freeze_if_needed)
    slow_timer.daemon = True
    slow_timer.start()
    try:
      status_code = self._probe()
    except Exception as exc:
      probe_error = f"{type(exc).__name__}: {exc}"
    finally:
      slow_timer.cancel()
      slow_timer.join()
    finished = self._clock()
    elapsed = max(0.0, finished - started)
    available_after = self._memory_available()
    self._record_available(available_after)
    sample = {
        "at": self._wall_clock(),
        "duration_s": round(elapsed, 6),
        "http_status": status_code,
        "probe_error": probe_error,
        "memavailable_bytes": available_after,
    }
    self.samples.append(sample)

    if elapsed > self._slow_seconds:
      self._freeze_if_needed()
    else:
      self._thaw_if_ready(finished)

    if available_after < self._floor_bytes:
      self._stop_for("MemAvailable fell below 12 GiB")
      raise RuntimeError("pressure run stopped because MemAvailable fell below 12 GiB")
    if probe_error is not None:
      self._stop_for(f"live probe returned no HTTP status: {probe_error}")
      raise RuntimeError(f"live probe failed: {probe_error}")
    return sample

  def run(self) -> None:
    try:
      while not self.stop_event.is_set():
        started = self._clock()
        self.check_once()
        remaining = GUARD_PROBE_INTERVAL - (self._clock() - started)
        if self.stop_event.wait(max(0.0, remaining)):
          return
    except Exception as exc:
      self.failure = exc
      self.stop_event.set()

  def stop(self) -> None:
    self.stop_event.set()

  def close(self) -> None:
    with self._state_lock:
      if not self._frozen:
        return
      if self.failure is None:
        self._thaw()
        self._finish_freeze(self._clock(), "run_completed")
        return
      self._finish_freeze(self._clock(), "slice_stopped")

  def snapshot(self) -> dict[str, Any]:
    return {
        "probe_url": LIVE_URL,
        "probe_interval_s": GUARD_PROBE_INTERVAL,
        "slow_probe_threshold_s": self._slow_seconds,
        "fast_probe_thaw_s": self._fast_thaw_seconds,
        "sample_count": len(self.samples),
        "slow_sample_count": sum(sample["duration_s"] > self._slow_seconds for sample in self.samples),
        "max_probe_duration_s": max((sample["duration_s"] for sample in self.samples), default=0.0),
        "minimum_memavailable_bytes": self._minimum_available,
        "freeze_intervals": list(self.freeze_intervals),
        "samples": list(self.samples),
        "failure": None if self.failure is None else f"{type(self.failure).__name__}: {self.failure}",
    }

  def raise_if_failed(self) -> None:
    if self.failure is not None:
      raise RuntimeError(f"pressure guard failed: {self.failure}") from self.failure

  def _freeze_if_needed(self) -> None:
    with self._state_lock:
      self._fast_since = None
      if self._frozen:
        return
      self._freeze()
      now = self._clock()
      self._frozen = True
      self._freeze_started = now
      self._freeze_started_at = self._wall_clock()

  def _thaw_if_ready(self, now: float) -> None:
    with self._state_lock:
      if not self._frozen:
        return
      if self._fast_since is None:
        self._fast_since = now
        return
      if now - self._fast_since < self._fast_thaw_seconds:
        return
      self._thaw()
      self._finish_freeze(self._clock(), "30_seconds_of_fast_probes")

  def _record_available(self, available: int) -> None:
    if self._minimum_available is None:
      self._minimum_available = available
    else:
      self._minimum_available = min(self._minimum_available, available)

  def _finish_freeze(self, now: float, reason: str) -> None:
    if self._freeze_started is None or self._freeze_started_at is None:
      raise AssertionError("freeze interval has no start")
    self.freeze_intervals.append(
        {
            "started_at": self._freeze_started_at,
            "ended_at": self._wall_clock(),
            "duration_s": round(max(0.0, now - self._freeze_started), 6),
            "thaw_reason": reason,
        })
    self._frozen = False
    self._freeze_started = None
    self._freeze_started_at = None
    self._fast_since = None

  def _stop_for(self, reason: str) -> None:
    if self.failure is None:
      self.failure = RuntimeError(reason)
    self._stop_slice()
    self.stop_event.set()


def _landlock_syscall(number: int, *args: object) -> int:
  libc = ctypes.CDLL(None, use_errno=True)
  libc.syscall.restype = ctypes.c_long
  result = libc.syscall(ctypes.c_long(number), *args)
  if result < 0:
    error_number = ctypes.get_errno()
    raise OSError(error_number, os.strerror(error_number))
  return int(result)


class _LandlockRulesetAttr(ctypes.Structure):
  _fields_ = [("handled_access_fs", ctypes.c_uint64)]


class _LandlockPathBeneathAttr(ctypes.Structure):
  _fields_ = [
      ("allowed_access", ctypes.c_uint64),
      ("parent_fd", ctypes.c_int32),
      ("reserved", ctypes.c_uint32),
  ]


def apply_landlock_write_allowlist(writable_dirs: list[pathlib.Path]) -> dict[str, int]:
  """Deny filesystem writes except beneath the copy-created session and control directories."""
  if platform.machine() not in {"aarch64", "riscv64", "x86_64"}:
    raise RuntimeError(f"Landlock syscall numbers are not defined for {platform.machine()}")
  abi = _landlock_syscall(444, ctypes.c_void_p(0), ctypes.c_size_t(0), ctypes.c_uint(1))
  if abi < 1:
    raise RuntimeError(f"Landlock ABI {abi} does not support filesystem restrictions")
  access = (1 << 1) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 8) | (1 << 9) | (1 << 10) | (1 << 11) | (
      1 << 12)
  if abi >= 2:
    access |= 1 << 13
  if abi >= 3:
    access |= 1 << 14
  if abi >= 5:
    access |= 1 << 15
  ruleset_attr = _LandlockRulesetAttr(handled_access_fs=access)
  ruleset_fd = _landlock_syscall(
      444,
      ctypes.byref(ruleset_attr),
      ctypes.c_size_t(ctypes.sizeof(ruleset_attr)),
      ctypes.c_uint(0),
  )
  try:
    for directory in writable_dirs:
      directory_fd = os.open(directory, os.O_PATH | os.O_CLOEXEC)
      try:
        rule = _LandlockPathBeneathAttr(allowed_access=access, parent_fd=directory_fd, reserved=0)
        _landlock_syscall(445, ctypes.c_int(ruleset_fd), ctypes.c_int(1), ctypes.byref(rule), ctypes.c_uint(0))
      finally:
        os.close(directory_fd)
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(38, 1, 0, 0, 0) != 0:
      error_number = ctypes.get_errno()
      raise OSError(error_number, os.strerror(error_number))
    _landlock_syscall(446, ctypes.c_int(ruleset_fd), ctypes.c_uint(0))
  finally:
    os.close(ruleset_fd)
  return {"abi": abi, "handled_access_fs": access}


def _session_directory(source_sessions: pathlib.Path, session_id: str) -> pathlib.Path:
  if not session_id or session_id in {".", ".."
                                     } or pathlib.PurePath(session_id).name != session_id or "\x00" in session_id:
    raise ValueError(f"session id must be a direct child name: {session_id!r}")
  source = source_sessions / session_id
  try:
    source_mode = source.lstat().st_mode
  except FileNotFoundError as exc:
    raise FileNotFoundError(f"selected session is missing: {source}") from exc
  if not stat.S_ISDIR(source_mode):
    raise RuntimeError(f"selected session is not a non-symlink directory: {source}")
  return source


def _source_names_once(source_sessions: pathlib.Path, count: int, seed: int) -> list[str]:
  with os.scandir(source_sessions) as entries:
    names = sorted(entry.name for entry in entries if entry.is_dir(follow_symlinks=False))
  if count > len(names):
    raise ValueError(f"requested {count} sessions, but the source lists {len(names)} names")
  random.Random(seed).shuffle(names)
  return names[:count]


def _link_session_tree(source: pathlib.Path, target: pathlib.Path, linked_files: list[pathlib.Path]) -> int:
  source_stat = source.lstat()
  if stat.S_ISDIR(source_stat.st_mode):
    target.mkdir(mode=stat.S_IMODE(source_stat.st_mode))
    total_bytes = 0
    with os.scandir(source) as entries:
      for entry in entries:
        total_bytes += _link_session_tree(source / entry.name, target / entry.name, linked_files)
    return total_bytes
  if not stat.S_ISREG(source_stat.st_mode):
    raise RuntimeError(f"selected session contains a non-regular file: {source}")
  os.link(source, target, follow_symlinks=False)
  target_stat = target.stat()
  if (source_stat.st_dev, source_stat.st_ino) != (target_stat.st_dev, target_stat.st_ino):
    raise AssertionError(f"session file is not a hardlink in the copy: {target}")
  linked_files.append(target)
  return source_stat.st_size


def build_session_copy(
    *,
    copy_root: pathlib.Path,
    source_sessions: pathlib.Path,
    selected_ids: list[str],
    event_byte_limit: int,
) -> dict[str, Any]:
  copy_root.mkdir(parents=True, exist_ok=False)
  home = copy_root / "home"
  copied_sessions = home / "sessions"
  copied_sessions.mkdir(parents=True)
  (home / "state").mkdir()
  linked_files: list[pathlib.Path] = []
  linked_bytes = 0
  event_bytes = 0
  selected_session_ids: list[str] = []
  pending_ids = list(selected_ids)
  seen_ids: set[str] = set()
  while pending_ids:
    session_id = pending_ids.pop(0)
    if session_id in seen_ids:
      continue
    seen_ids.add(session_id)
    if len(seen_ids) > MAX_COPIED_SESSIONS:
      raise RuntimeError(f"selected sessions and their ancestors exceed {MAX_COPIED_SESSIONS} sessions")
    source = _session_directory(source_sessions, session_id)
    target = copied_sessions / session_id
    linked_bytes += _link_session_tree(source, target, linked_files)
    selected_session_ids.append(session_id)
    metadata_path = target / "metadata.json"
    if metadata_path.is_file():
      from src.infra import models

      meta = models.validate_session_metadata(metadata_path.read_text(encoding="utf-8"), str(metadata_path))
      if meta.task_parent_id is not None:
        pending_ids.append(meta.task_parent_id)
    event_path = source / "data" / "chat_events.jsonl"
    if event_path.is_file():
      event_bytes += event_path.stat().st_size
  if event_bytes > event_byte_limit:
    raise RuntimeError(
        f"selected live histories total {event_bytes} bytes, above the {event_byte_limit}-byte copy limit")
  manifest = {
      "session_ids": selected_session_ids,
      "linked_files": [str(path) for path in linked_files],
      "linked_file_count": len(linked_files),
      "linked_file_bytes": linked_bytes,
      "live_event_bytes": event_bytes,
  }
  flush_json(copy_root / "copy_manifest.json", manifest)
  return {"home": home, **manifest}


def write_minimal_config(home: pathlib.Path, port: int) -> None:
  import yaml

  config = {"server": {"host": "127.0.0.1", "port": port}}
  with (home / "config.yaml").open("w", encoding="utf-8") as stream:
    yaml.safe_dump(config, stream, sort_keys=True)
    stream.flush()
    os.fsync(stream.fileno())


def write_synthetic_sessions(home: pathlib.Path, session_ids: list[str]) -> list[pathlib.Path]:
  from src.infra import models

  writable_dirs = []
  for index, session_id in enumerate(session_ids):
    session_dir = home / "sessions" / session_id
    (session_dir / "data").mkdir(parents=True)
    metadata = models.SessionMetadata(
        id=session_id,
        name=f"pressure-probe-{index}",
        backend="claude",
        profile="manager",
    )
    (session_dir / "metadata.json").write_text(metadata.model_dump_json(indent=2) + "\n", encoding="utf-8")
    (session_dir / "data" / "chat_events.jsonl").write_text("", encoding="utf-8")
    writable_dirs.append(session_dir)
  return writable_dirs


def load_live_access_key() -> str:
  import yaml

  payload = yaml.safe_load(LIVE_CREDENTIALS.read_text(encoding="utf-8"))
  if not isinstance(payload, dict):
    raise TypeError(f"credentials file is not a mapping: {LIVE_CREDENTIALS}")
  charliebot = payload["charliebot"]
  if not isinstance(charliebot, dict):
    raise TypeError("credentials.yaml charliebot section is not a mapping")
  access_key = charliebot["access_key"]
  if not isinstance(access_key, str) or not access_key:
    raise TypeError("credentials.yaml charliebot.access_key is not a non-empty string")
  return access_key


def _read_proc_status(pid: int) -> dict[str, int]:
  values: dict[str, int] = {}
  with pathlib.Path(f"/proc/{pid}/status").open(encoding="utf-8") as stream:
    for line in stream:
      key, separator, value = line.partition(":")
      if separator and key in {"VmRSS", "VmHWM", "VmSwap", "RssAnon", "RssFile"}:
        values[key] = int(value.split()[0])
  required = {"VmRSS", "VmHWM", "VmSwap", "RssAnon", "RssFile"}
  if values.keys() != required:
    raise RuntimeError(f"missing process status fields for pid {pid}: {sorted(required - values.keys())}")
  return values


def compute_memory_pressure_target(
    *,
    server_rss_anon_bytes: int,
    slice_memory_max_bytes: int,
    slice_swap_max_bytes: int,
    swap_excess_bytes: int,
    probe_reserve_bytes: int,
    cap_bytes: int,
) -> int:
  """Size the pressure so the slice exceeds MemoryMax by an excess the swap cap can hold.

  The repair probe runs inside the same slice under pressure, so the reserve keeps
  the slice's peak overshoot at swap_excess_bytes even while a probe is resident.
  """
  if server_rss_anon_bytes <= 0:
    raise RuntimeError(f"server RssAnon reading is not positive: {server_rss_anon_bytes}")
  if server_rss_anon_bytes >= slice_memory_max_bytes:
    raise RuntimeError(
        f"server RssAnon {server_rss_anon_bytes} alone fills the slice MemoryMax {slice_memory_max_bytes}")
  if not 0 < swap_excess_bytes < slice_swap_max_bytes:
    raise RuntimeError(f"swap excess {swap_excess_bytes} must stay inside the slice swap cap {slice_swap_max_bytes}")
  if not 0 < probe_reserve_bytes < swap_excess_bytes:
    raise RuntimeError(
        f"probe reserve {probe_reserve_bytes} must leave a steady-state excess below {swap_excess_bytes}")
  target = slice_memory_max_bytes - server_rss_anon_bytes + swap_excess_bytes - probe_reserve_bytes
  if target > cap_bytes:
    raise RuntimeError(
        f"computed pressure target {target} bytes exceeds the {cap_bytes}-byte cap "
        f"(server RssAnon {server_rss_anon_bytes}, swap excess {swap_excess_bytes})")
  return target


def _self_cgroup_directory() -> pathlib.Path:
  with pathlib.Path("/proc/self/cgroup").open(encoding="ascii") as stream:
    for line in stream:
      hierarchy, separator, relative = line.partition("::")
      if separator and hierarchy == "0":
        return CGROUP_ROOT / relative.strip().lstrip("/")
  raise RuntimeError("process has no unified cgroup entry")


def _unit_property(unit: str, property_name: str) -> str:
  result = subprocess.run(
      ["/usr/bin/systemctl", "--user", "show", "--value", f"--property={property_name}", unit],
      check=True,
      capture_output=True,
      text=True,
  )
  return result.stdout.strip()


def _configure_unit(unit: str, properties: list[str]) -> None:
  subprocess.run(
      ["/usr/bin/systemctl", "--user", "set-property", "--runtime", unit, *properties],
      check=True,
      capture_output=True,
      text=True,
  )


def _cgroup_for_unit(unit: str) -> pathlib.Path:
  control_group = _unit_property(unit, "ControlGroup")
  if not control_group.startswith("/"):
    raise RuntimeError(f"systemd returned an invalid ControlGroup for {unit}: {control_group!r}")
  path = CGROUP_ROOT / control_group.lstrip("/")
  if not path.is_dir():
    raise RuntimeError(f"systemd cgroup path is missing for {unit}: {path}")
  return path


def _cgroup_pids(cgroup: pathlib.Path) -> set[int]:
  pids = set()
  procs_path = cgroup / "cgroup.procs"
  if procs_path.exists():
    pids.update(int(value) for value in procs_path.read_text(encoding="utf-8").split())
  for child in cgroup.iterdir():
    if child.is_dir() and (child / "cgroup.procs").is_file():
      pids.update(_cgroup_pids(child))
  return pids


def _wait_for_cgroup(unit: str, command_process: subprocess.Popen[str], timeout: float = 30.0) -> pathlib.Path:
  deadline = time.monotonic() + timeout
  while time.monotonic() < deadline:
    if command_process.poll() is not None:
      raise RuntimeError(f"systemd-run exited before {unit} became active: {command_process.returncode}")
    try:
      path = _cgroup_for_unit(unit)
    except (subprocess.CalledProcessError, RuntimeError):
      time.sleep(0.05)
      continue
    if _cgroup_pids(path):
      return path
    time.sleep(0.05)
  raise TimeoutError(f"systemd scope {unit} did not start within {timeout:g}s")


def _systemd_run_command(
    *,
    unit: str,
    slice_name: str,
    properties: list[str],
    command: list[str],
    home: pathlib.Path | None = None,
    stdout: Any = None,
    stderr: Any = None,
) -> subprocess.Popen[str]:
  systemd_run = shutil.which("systemd-run")
  if systemd_run is None:
    raise FileNotFoundError("systemd-run is missing from PATH")
  args = [
      systemd_run,
      "--user",
      "--scope",
      "--collect",
      "--quiet",
      f"--unit={unit}",
      f"--slice={slice_name}",
      "--working-directory=" + str(REPO_ROOT),
  ]
  args.extend(f"--property={item}" for item in properties)
  args.extend(("--setenv=PATH=" + SERVER_PATH, "--setenv=PYTHONDONTWRITEBYTECODE=1"))
  if home is not None:
    args.append(f"--setenv=CHARLIEBOT_HOME={home}")
    args.append(f"--setenv=PYTHONPATH={REPO_ROOT}")
  args.extend(("--", *command))
  return subprocess.Popen(
      args,
      cwd=REPO_ROOT,
      stdout=stdout,
      stderr=stderr,
      text=True,
      start_new_session=True,
  )


def _remove_pressure_slice(cgroup: pathlib.Path | None) -> None:
  if cgroup is None:
    return
  if not cgroup.exists():
    return
  if _cgroup_pids(cgroup):
    kill_path = cgroup / "cgroup.kill"
    if not kill_path.is_file():
      raise RuntimeError(f"cgroup.kill is unavailable for {cgroup}")
    kill_path.write_text("1", encoding="ascii")
    deadline = time.monotonic() + 5
    while _cgroup_pids(cgroup) and time.monotonic() < deadline:
      time.sleep(0.05)
    if _cgroup_pids(cgroup):
      raise RuntimeError(f"pressure slice still has processes after cgroup.kill: {cgroup}")
  subprocess.run(
      ["/usr/bin/systemctl", "--user", "stop", PRESSURE_SLICE],
      check=True,
      capture_output=True,
      text=True,
  )


def _freeze_cgroup(cgroup: pathlib.Path) -> None:
  freeze_path = cgroup / "cgroup.freeze"
  if not freeze_path.is_file():
    raise RuntimeError(f"cgroup.freeze is unavailable for {cgroup}")
  freeze_path.write_text("1", encoding="ascii")
  deadline = time.monotonic() + 10.0
  while time.monotonic() < deadline:
    events = (cgroup / "cgroup.events").read_text(encoding="ascii")
    if "frozen 1" in events.splitlines():
      return
    time.sleep(0.01)
  raise TimeoutError(f"cgroup did not freeze within 10 seconds: {cgroup}")


def _thaw_cgroup(cgroup: pathlib.Path) -> None:
  freeze_path = cgroup / "cgroup.freeze"
  if not freeze_path.is_file():
    raise RuntimeError(f"cgroup.freeze is unavailable for {cgroup}")
  freeze_path.write_text("0", encoding="ascii")
  deadline = time.monotonic() + 10.0
  while time.monotonic() < deadline:
    events = (cgroup / "cgroup.events").read_text(encoding="ascii")
    if "frozen 0" in events.splitlines():
      return
    time.sleep(0.01)
  raise TimeoutError(f"cgroup did not thaw within 10 seconds: {cgroup}")


class ResultRecorder:
  """Rewrite one result JSON atomically after every finished stage."""

  def __init__(self, path: pathlib.Path, payload: dict[str, Any]) -> None:
    self.path = path
    self.payload = payload
    self.payload["stages"] = []
    self.payload["guard_runs"] = []
    self._write()

  def finish_checkpoint(self, name: str, result: dict[str, Any]) -> None:
    self.payload["stages"].append({"name": name, "finished_at": utc_now(), **result})
    self._write()

  def record_guard_run(self, guard: dict[str, Any]) -> None:
    self.payload["guard_runs"].append(guard)
    self.payload["guard_freeze_intervals"] = [
        {
            "run": index,
            **interval
        } for index, run in enumerate(self.payload["guard_runs"]) for interval in run["freeze_intervals"]
    ]
    self._write()

  def set_run_summary(self, result: dict[str, Any]) -> None:
    self.payload["run_summary"] = result
    self._write()

  def set_reference_verdicts(self, verdicts: dict[str, Any]) -> None:
    self.payload["reference_verdicts"] = verdicts
    self._write()

  def fail(self, error: Exception) -> None:
    self.payload["failure"] = f"{type(error).__name__}: {error}"
    self._write()

  def _write(self) -> None:
    self.payload["updated_at"] = utc_now()
    flush_json(self.path, self.payload)


def _read_link_manifest(copy_root: pathlib.Path) -> tuple[list[pathlib.Path], list[str]]:
  manifest = json.loads((copy_root / "copy_manifest.json").read_text(encoding="utf-8"))
  return [pathlib.Path(value) for value in manifest["linked_files"]], list(manifest["session_ids"])


def _install_child_process_block() -> Callable[[], list[str]]:
  import asyncio as asyncio_module

  attempts: list[str] = []

  def blocked(name: str) -> Callable[..., Any]:

    def reject(*args: object, **kwargs: object) -> Any:
      attempts.append(name)
      raise RuntimeError(f"test server attempted a child process through {name}")

    return reject

  subprocess.Popen = blocked("subprocess.Popen")  # type: ignore[assignment]
  os.fork = blocked("os.fork")  # type: ignore[assignment]
  os.posix_spawn = blocked("os.posix_spawn")  # type: ignore[assignment]
  os.posix_spawnp = blocked("os.posix_spawnp")  # type: ignore[assignment]
  asyncio_module.create_subprocess_exec = blocked("asyncio.create_subprocess_exec")  # type: ignore[assignment]
  asyncio_module.create_subprocess_shell = blocked("asyncio.create_subprocess_shell")  # type: ignore[assignment]
  return lambda: list(attempts)


def _assert_agent_clis_off_path() -> None:
  if os.environ.get("PATH") != SERVER_PATH:
    raise RuntimeError(f"test server PATH is not the restricted value {SERVER_PATH!r}")
  for name in AGENT_CLI_NAMES:
    if shutil.which(name, path=SERVER_PATH) is not None:
      raise RuntimeError(f"agent CLI {name!r} is available on the test server PATH")


def _wait_for_file(path: pathlib.Path, timeout: float) -> None:
  deadline = time.monotonic() + timeout
  while not path.is_file() and time.monotonic() < deadline:
    time.sleep(0.02)
  if not path.is_file():
    raise TimeoutError(f"control file did not appear within {timeout:g}s: {path}")


def _zero_read_delay_count() -> int:
  return 0


def _delay_session_reads(session_root: pathlib.Path, delay_seconds: float) -> Callable[[], int]:
  import builtins
  import io

  count = {"value": 0}
  original_builtin_open = builtins.open
  original_io_open = io.open
  original_os_open = os.open
  root = str(session_root.resolve()) + os.sep

  def should_delay(path: object, read_only: bool) -> bool:
    if not read_only:
      return False
    try:
      candidate = os.path.abspath(os.fspath(path))
    except TypeError:
      return False
    return candidate.startswith(root)

  def delayed_builtin_open(file: object, mode: str = "r", *args: object, **kwargs: object) -> Any:
    read_only = "r" in mode and not any(flag in mode for flag in ("+", "w", "a", "x"))
    if should_delay(file, read_only):
      count["value"] += 1
      time.sleep(delay_seconds)
    return original_builtin_open(file, mode, *args, **kwargs)

  def delayed_io_open(file: object, mode: str = "r", *args: object, **kwargs: object) -> Any:
    read_only = "r" in mode and not any(flag in mode for flag in ("+", "w", "a", "x"))
    if should_delay(file, read_only):
      count["value"] += 1
      time.sleep(delay_seconds)
    return original_io_open(file, mode, *args, **kwargs)

  def delayed_os_open(path: object, flags: int, *args: object, **kwargs: object) -> int:
    read_only = not flags & (os.O_WRONLY | os.O_RDWR)
    if should_delay(path, read_only):
      count["value"] += 1
      time.sleep(delay_seconds)
    return original_os_open(path, flags, *args, **kwargs)

  builtins.open = delayed_builtin_open
  io.open = delayed_io_open
  os.open = delayed_os_open
  return lambda: count["value"]


def _serve_main(argv: list[str]) -> None:
  parser = argparse.ArgumentParser()
  parser.add_argument("--home", type=pathlib.Path, required=True)
  parser.add_argument("--port", type=int, required=True)
  parser.add_argument("--linked-manifest", type=pathlib.Path, required=True)
  parser.add_argument("--control-dir", type=pathlib.Path, required=True)
  parser.add_argument("--start-file", type=pathlib.Path, required=True)
  parser.add_argument("--stop-file", type=pathlib.Path, required=True)
  parser.add_argument("--ready-file", type=pathlib.Path, required=True)
  parser.add_argument("--writable-dir", type=pathlib.Path, action="append", default=[])
  parser.add_argument("--disk-delay-ms", type=float, default=0.0)
  args = parser.parse_args(argv)
  if pathlib.Path(os.environ["CHARLIEBOT_HOME"]).resolve() != args.home.resolve():
    raise RuntimeError("CHARLIEBOT_HOME does not match the selected copy")
  if args.port == 18498:
    raise RuntimeError("test server port must differ from the live server port")
  _assert_agent_clis_off_path()
  linked_manifest = json.loads(args.linked_manifest.read_text(encoding="utf-8"))
  linked_files = [pathlib.Path(value) for value in linked_manifest["linked_files"]]
  writable_dirs = [*args.writable_dir, args.control_dir]
  landlock = apply_landlock_write_allowlist(writable_dirs)
  assert_linked_files_unwritable(linked_files)
  blocked_child_attempts = _install_child_process_block()
  read_delay_count = _zero_read_delay_count
  if args.disk_delay_ms:
    read_delay_count = _delay_session_reads(args.home / "sessions", args.disk_delay_ms / 1000.0)
  _wait_for_file(args.start_file, timeout=120.0)

  from src.app import registrations

  registrations.register_all()
  import uvicorn

  import server

  config = uvicorn.Config(
      server.app,
      host="127.0.0.1",
      port=args.port,
      log_level="info",
      access_log=False,
      lifespan="off",
  )
  uvicorn_server = uvicorn.Server(config)

  async def serve() -> None:
    task = asyncio.create_task(uvicorn_server.serve())
    deadline = time.monotonic() + 30
    while not uvicorn_server.started:
      if task.done():
        raise RuntimeError(f"test server failed before listening: {task.exception()!r}")
      if time.monotonic() >= deadline:
        raise TimeoutError("test server did not listen within 30 seconds")
      await asyncio.sleep(0.02)
    args.ready_file.write_text(json.dumps({"pid": os.getpid(), "landlock": landlock}), encoding="utf-8")
    while not task.done() and not args.stop_file.exists():
      await asyncio.sleep(0.02)
    if not task.done():
      uvicorn_server.should_exit = True
    await task

  try:
    asyncio.run(serve())
  finally:
    (args.control_dir / "child_process_attempts.json").write_text(
        json.dumps(blocked_child_attempts()), encoding="utf-8")
    (args.control_dir / "read_delay_count.json").write_text(
        json.dumps({"delayed_read_opens": read_delay_count()}), encoding="utf-8")
  attempts = blocked_child_attempts()
  if attempts:
    raise RuntimeError(f"test server attempted child processes: {attempts}")


def _pressure_main(argv: list[str]) -> None:
  parser = argparse.ArgumentParser()
  parser.add_argument("--size-bytes", type=int, required=True)
  parser.add_argument("--start-file", type=pathlib.Path, required=True)
  parser.add_argument("--ready-file", type=pathlib.Path, required=True)
  parser.add_argument("--kill-fill-file", type=pathlib.Path)
  parser.add_argument("--kill-fill-delay-s", type=float, default=45.0)
  args = parser.parse_args(argv)
  _wait_for_file(args.start_file, timeout=120.0)
  stopped = threading.Event()
  signal.signal(signal.SIGTERM, lambda _signum, _frame: stopped.set())
  signal.signal(signal.SIGINT, lambda _signum, _frame: stopped.set())
  started = time.monotonic()
  blocks: list[bytearray] = []
  remaining = args.size_bytes
  block_size = 64 * MIB
  while remaining and not stopped.is_set():
    if args.kill_fill_file is not None and time.monotonic() - started > args.kill_fill_delay_s:
      # Pinned between MemoryHigh and MemoryMax: user faults throttle to about
      # 1 MiB/s, so cross MemoryMax with kernel-context charges instead. Buffered
      # writes leave dirty page cache the kernel cannot drop, and the memcg OOM
      # kill then ends this process, which the supervisor records and restarts.
      _kill_fill(args.kill_fill_file, stopped)
      return
    size = min(block_size, remaining)
    block = bytearray(size)
    for offset in range(0, size, 4096):
      block[offset] = 1
    blocks.append(block)
    remaining -= size
  if stopped.is_set():
    return
  flush_json(args.ready_file, {"pid": os.getpid(), "allocated_bytes": args.size_bytes})
  # Rewrite every page in a loop so reclaim under the slice limit finds the pressure
  # pages hot and picks the test server's colder pages instead.
  while not stopped.is_set():
    for block in blocks:
      if stopped.is_set():
        break
      for offset in range(0, len(block), 4096):
        block[offset] = (block[offset] + 1) & 0x7F
  if sum(len(block) for block in blocks) != args.size_bytes:
    raise AssertionError("pressure allocation changed while resident")


def _kill_fill(kill_fill_file: pathlib.Path, stopped: threading.Event) -> None:
  # Small chunks keep each write's balance_dirty_pages stall short, so a guard
  # freeze of the slice does not wait long on a task inside this loop.
  chunk = b"x" * (16 * MIB)
  written = 0
  with kill_fill_file.open("wb") as stream:
    while not stopped.is_set():
      stream.write(chunk)
      written += len(chunk)
      if written > KILL_FILL_MAX_BYTES:
        raise RuntimeError(
            f"kill-fill wrote {written} bytes without the kernel ending the process; "
            "the agents slice limits are not in effect")


def _repair_probe_main(argv: list[str]) -> None:
  """Allocate and touch a fixed block, read one small file of the copy, and exit."""
  parser = argparse.ArgumentParser()
  parser.add_argument("--touch-bytes", type=int, required=True)
  parser.add_argument("--read-file", type=pathlib.Path, required=True)
  args = parser.parse_args(argv)
  started = time.perf_counter()
  block = bytearray(args.touch_bytes)
  for offset in range(0, len(block), 4096):
    block[offset] = 1
  read_bytes = len(args.read_file.read_bytes())
  if read_bytes == 0:
    raise RuntimeError(f"repair probe read file is empty: {args.read_file}")
  elapsed_ms = (time.perf_counter() - started) * 1000
  print(
      json.dumps(
          {
              "pid": os.getpid(),
              "probe_ms": round(elapsed_ms, 3),
              "touched_bytes": args.touch_bytes,
              "read_bytes": read_bytes,
          }))


def _deep_size(value: object, seen: set[int] | None = None) -> int:
  if seen is None:
    seen = set()
  identity = id(value)
  if identity in seen:
    return 0
  seen.add(identity)
  size = sys.getsizeof(value)
  if isinstance(value, dict):
    size += sum(_deep_size(key, seen) + _deep_size(item, seen) for key, item in value.items())
  elif isinstance(value, (list, tuple, set, frozenset)):
    size += sum(_deep_size(item, seen) for item in value)
  return size


def _memo_snapshot(tree: Any) -> dict[str, dict[str, int]]:
  cache = tree.session_events.chat_events._events_cache

  def parsed_event_bytes(event_lists: list[list[dict[str, Any]]]) -> int:
    seen: set[int] = set()
    return sum(_deep_size(events, seen) for events in event_lists)

  output: dict[str, dict[str, int]] = {
      "events_cache":
          {
              "entry_count": len(cache),
              "parsed_event_count": sum(len(events) for events in cache.values()),
              "parsed_event_bytes": parsed_event_bytes(list(cache.values())),
          }
  }
  owners = {
      "_facts_memo": lambda cell: cell[0],
      "_outcomes_memo": lambda cell: cell[0],
      "_activity_memo": lambda cell: cell[1],
      "_state_tail_memo": lambda _cell: None,
  }
  for name, events_of in owners.items():
    memo = getattr(tree, name)
    event_lists = [events_of(cell) for cell in memo.values()]
    event_lists = [events for events in event_lists if isinstance(events, list)]
    output[name] = {
        "entry_count": len(memo),
        "parsed_event_count": sum(len(events) for events in event_lists),
        "parsed_event_bytes": parsed_event_bytes(event_lists),
    }
  return output


def _read_rss_anon_kib() -> int:
  values = _read_proc_status(os.getpid())
  return values["RssAnon"]


def _composition_main(argv: list[str]) -> None:
  parser = argparse.ArgumentParser()
  parser.add_argument("--home", type=pathlib.Path, required=True)
  parser.add_argument("--copy-root", type=pathlib.Path, required=True)
  parser.add_argument("--control-dir", type=pathlib.Path, required=True)
  parser.add_argument("--start-file", type=pathlib.Path, required=True)
  parser.add_argument("--marker", default=BOOT_MARKER)
  parser.add_argument("--live-log-reference", default="server_20261023_144751.log:748")
  args = parser.parse_args(argv)
  if pathlib.Path(os.environ["CHARLIEBOT_HOME"]).resolve() != args.home.resolve():
    raise RuntimeError("CHARLIEBOT_HOME does not match the selected composition copy")
  _assert_agent_clis_off_path()
  linked_files, _selected_ids = _read_link_manifest(args.copy_root)
  landlock = apply_landlock_write_allowlist([args.control_dir])
  assert_linked_files_unwritable(linked_files)
  _wait_for_file(args.start_file, timeout=120.0)
  blocked_child_attempts = _install_child_process_block()

  from src.app import registrations

  registrations.register_all()
  import httpx

  import server
  from src.runtime import task_execution, task_recovery

  tree = task_execution.task_manager()
  marker = datetime.datetime.fromisoformat(args.marker)
  if marker.tzinfo is None:
    raise ValueError("boot marker must include a timezone")

  async def measure() -> dict[str, Any]:
    index = await tree._get_index(force=True)
    paths: dict[str, str | None] = {}
    for session_id, meta in sorted(index.metas.items()):
      reason = task_recovery._boot_skip_reason(
          tree,
          session_id,
          args.home / "sessions" / session_id,
          meta,
          marker,
      )
      paths[session_id] = reason
    cache_after_boot = set(tree.session_events.chat_events._events_cache)
    full_check_count = sum(reason is not None for reason in paths.values())
    node_count = len(index.metas)
    after_boot = {
        "rss_anon_kib": _read_rss_anon_kib(),
        "events_and_memos": _memo_snapshot(tree),
        "index_node_count": len(index.metas),
        "boot_path_counts":
            {
                "full_check": full_check_count,
                "tail_read": sum(reason is None for reason in paths.values()),
            },
        "boot_path_mix":
            {
                "observed_full_fraction": full_check_count / node_count,
                "reference_full_fraction": 612 / 5924,
                "absolute_fraction_difference": abs(full_check_count / node_count - 612 / 5924),
            },
        "boot_path_reasons": {
            session_id: reason for session_id, reason in paths.items() if reason is not None
        },
    }
    flush_json(args.control_dir / "composition_after_boot.json", after_boot)

    import yaml

    credentials = yaml.safe_load((args.home / "credentials.yaml").read_text(encoding="utf-8"))
    access_key = credentials["charliebot"]["access_key"]
    transport = httpx.ASGITransport(app=server.app)
    async with httpx.AsyncClient(
        transport=transport,
        base_url="http://composition.local",
        headers={"Authorization": f"Bearer {access_key}"},
    ) as client:
      response = await client.get("/api/sessions/tree")
      if response.status_code != 200:
        raise RuntimeError(f"first tree_page returned {response.status_code}: {response.text[:300]}")
    cache_after_tree = set(tree.session_events.chat_events._events_cache)
    added = sorted(cache_after_tree - cache_after_boot)
    added_by_path = {"full_check": [], "tail_read": []}
    for session_id in added:
      bucket = "full_check" if paths[session_id] is not None else "tail_read"
      added_by_path[bucket].append(session_id)
    after_tree = {
        "rss_anon_kib": _read_rss_anon_kib(),
        "events_and_memos": _memo_snapshot(tree),
        "first_tree_status": response.status_code,
        "events_cache_added_session_ids": added,
        "events_cache_added_by_boot_path": added_by_path,
        "events_cache_added_count": len(added),
    }
    flush_json(args.control_dir / "composition_after_tree.json", after_tree)
    if blocked_child_attempts():
      raise RuntimeError(f"composition process attempted child processes: {blocked_child_attempts()}")
    return {
        "marker": args.marker,
        "marker_source": args.live_log_reference,
        "lifespan": "off (ASGITransport does not run lifespan handlers)",
        "landlock": landlock,
        "after_boot_reads": after_boot,
        "after_first_tree_page": after_tree,
        "blocked_child_process_attempts": blocked_child_attempts(),
    }

  result = asyncio.run(measure())
  cgroup = _self_cgroup_directory()
  result["scope_memory_max_bytes"] = _read_cgroup_bound(cgroup, "memory.max")
  result["scope_memory_swap_max_bytes"] = _read_cgroup_bound(cgroup, "memory.swap.max")
  result["scope_peak_memory_bytes"] = int((cgroup / "memory.peak").read_text(encoding="ascii"))
  flush_json(args.control_dir / "composition_result.json", result)


def _parse_measurement_args(argv: list[str] | None) -> argparse.Namespace:
  parser = argparse.ArgumentParser(
      description=(
          "Run bounded pressure measurements on a read-only hard-linked copy of selected CharlieBot sessions. "
          "Cases: memory, disk, cold, composition, or all. Memory and disk compare pressured requests with "
          "a no-pressure baseline on the same copy. Cold measures the first tree request from a fresh server. "
          "Disk adds a fixed delay to each session-file read and is approximate. Endpoints are session status, "
          "one session, the session tree, web reply, and web status.\n\n"
          "Memory layouts: the pressure target is computed per run as the 8 GiB slice MemoryMax minus the "
          "test server's RssAnon after its warm-up, plus a 384 MiB excess the 512 MiB slice swap cap can "
          "hold, so the kernel swaps server pages out instead of killing a process. Current: server and "
          "pressure share the parent slice and the server may use up to 512 MiB of swap. Protected: the "
          "server scope keeps its 3 GiB bound with MemorySwapMax=0; the pressure worker runs in the agents "
          "child slice charliebot-pressure-test-agents.slice (MemoryHigh=3.5G, MemoryMax=4G, no swap), the "
          "measured window starts when the share crosses its MemoryHigh, and because this kernel pins a "
          "throttled workload below MemoryMax the worker switches to buffered dirty page-cache writes after "
          "45 s to bring the kernel's MemoryMax OOM kill; the supervisor records and restarts every kill "
          "until the window ends; a repair child slice "
          "charliebot-pressure-test-repair.slice (MemoryMax=768M, no swap) holds the repair probe. The "
          "repair probe allocates and touches 256 MiB, reads one small file of the copy, and exits; it runs "
          "8 times without pressure and 8 times under pressure in the repair child (protected) or the test "
          "slice (current). Every test server and pressure worker runs under charliebot-pressure-test.slice "
          "with MemoryMax=8 GiB. Test-server peak RSS must stay at or below 3 GiB. Composition runs in a "
          "separate process scope with MemoryMax=2 GiB. Runs stop below 12 GiB MemAvailable. A live-server "
          "guard probes once per second, freezes the test slice after a probe over 1 second, and thaws it "
          "after 30 seconds of fast probes."),
      formatter_class=argparse.RawDescriptionHelpFormatter,
  )
  parser.add_argument(
      "--case",
      choices=("all", "memory", "disk", "cold", "composition"),
      default="all",
      help=
      "memory compares current/protected layouts; disk is approximate; cold measures a fresh first tree and judges the concurrent burst against the same burst without tree requests; composition records boot and tree cache sizes",
  )
  parser.add_argument(
      "--layout",
      choices=("current", "protected"),
      default="current",
      help="memory layout for --case memory; --case all runs both layouts",
  )
  parser.add_argument("--results-dir", type=pathlib.Path, default=DEFAULT_RESULTS_DIR)
  parser.add_argument(
      "--session-count",
      type=int,
      default=MAX_SESSION_COUNT,
      help=f"live sessions to hard-link per pressure copy (maximum {MAX_SESSION_COUNT})",
  )
  parser.add_argument(
      "--composition-session-count",
      type=int,
      default=DEFAULT_COMPOSITION_SESSION_COUNT,
      help=f"live sessions to sample for the two composition checkpoints (maximum {MAX_COMPOSITION_SESSION_COUNT})",
  )
  parser.add_argument("--seed", type=int, default=1792795452, help="stable selector seed")
  parser.add_argument(
      "--sample-count",
      type=int,
      default=MAX_SAMPLE_COUNT,
      help=f"requests per endpoint for latency percentiles (maximum {MAX_SAMPLE_COUNT})",
  )
  parser.add_argument(
      "--max-live-event-gib",
      type=float,
      default=MAX_LIVE_EVENT_GIB,
      help=f"fail before serving if selected live histories exceed this size (maximum {MAX_LIVE_EVENT_GIB} GiB)",
  )
  parser.add_argument(
      "--memory-pressure-gib",
      type=float,
      default=MAX_MEMORY_PRESSURE_GIB,
      help=f"cap for the computed pressure target (slice MemoryMax minus server RssAnon after warm-up plus the "
      f"swap excess); maximum {MAX_MEMORY_PRESSURE_GIB} GiB",
  )
  parser.add_argument(
      "--disk-delay-ms",
      type=float,
      default=MAX_DISK_DELAY_MS,
      help=f"fixed approximate delay for session file reads (maximum {MAX_DISK_DELAY_MS} ms)",
  )
  parser.add_argument("--boot-marker", default=BOOT_MARKER, help="UTC marker used by the boot skip decision")
  parser.add_argument("--reset-selection", action="store_true", help="select a new deterministic session subset")
  return parser.parse_args(argv)


def _quantile(values: list[float], percentile: float) -> float:
  ordered = sorted(values)
  if not ordered:
    raise ValueError("cannot compute a quantile from no measurements")
  position = (len(ordered) - 1) * percentile
  lower = math.floor(position)
  upper = math.ceil(position)
  if lower == upper:
    return ordered[lower]
  fraction = position - lower
  return ordered[lower] + (ordered[upper] - ordered[lower]) * fraction


def _request_definitions(session_ids: list[str]) -> dict[str, tuple[str, str, dict[str, str] | None]]:
  reply_id, status_id = session_ids
  return {
      "GET /api/sessions/status": ("GET", f"/api/sessions/status?ids={reply_id},{status_id}", None),
      "GET /api/sessions/{id}": ("GET", f"/api/sessions/{reply_id}", None),
      "GET /api/sessions/tree": ("GET", "/api/sessions/tree", None),
      "POST /api/internal/web/reply":
          (
              "POST",
              "/api/internal/web/reply",
              {
                  "session_id": reply_id,
                  "text": "Pressure probe reply."
              },
          ),
      "POST /api/internal/web/status":
          (
              "POST",
              "/api/internal/web/status",
              {
                  "session_id": status_id,
                  "text": "{\"goal\":\"pressure probe\",\"now\":\"sample\"}"
              },
          ),
  }


async def _timed_request(client: Any, method: str, path: str, payload: dict[str, str] | None) -> dict[str, Any]:
  started = time.perf_counter()
  response = await client.request(method, path, json=payload)
  elapsed_ms = (time.perf_counter() - started) * 1000
  if response.status_code != 200:
    raise RuntimeError(f"{method} {path} returned {response.status_code}: {response.text[:300]}")
  return {"duration_ms": elapsed_ms, "status": response.status_code}


async def _measure_warm_batch(base_url: str, access_key: str, session_ids: list[str], samples: int) -> dict[str, Any]:
  import httpx

  definitions = _request_definitions(session_ids)
  values: dict[str, list[float]] = {name: [] for name in definitions}
  statuses: dict[str, list[int]] = {name: [] for name in definitions}
  async with httpx.AsyncClient(
      base_url=base_url,
      headers={"Authorization": f"Bearer {access_key}"},
      timeout=120.0,
      limits=httpx.Limits(max_connections=max(samples * len(definitions), 10)),
  ) as client:
    for _ in range(samples):
      responses = await asyncio.gather(
          *(_timed_request(client, method, path, payload) for method, path, payload in definitions.values()))
      for name, response in zip(definitions, responses, strict=True):
        values[name].append(response["duration_ms"])
        statuses[name].append(response["status"])
  return {
      name:
          {
              "count": len(durations),
              "p50_ms": round(_quantile(durations, 0.50), 3),
              "p95_ms": round(_quantile(durations, 0.95), 3),
              "longest_ms": round(max(durations), 3),
              "statuses": statuses[name],
          } for name, durations in values.items()
  }


async def _measure_cold_burst(
    base_url: str,
    access_key: str,
    session_ids: list[str],
    samples: int,
    *,
    concurrent: bool,
    include_tree: bool = True,
) -> dict[str, Any]:
  import httpx

  definitions = _request_definitions(session_ids)
  tree_name = "GET /api/sessions/tree"
  tree_method, tree_path, tree_payload = definitions[tree_name]
  async with httpx.AsyncClient(
      base_url=base_url,
      headers={"Authorization": f"Bearer {access_key}"},
      timeout=180.0,
      limits=httpx.Limits(max_connections=max(samples * len(definitions), 10)),
  ) as client:
    first_tree = (
        asyncio.create_task(_timed_request(client, tree_method, tree_path, tree_payload)) if include_tree else None)
    if concurrent:
      await asyncio.sleep(0.01)
      other_requests: list[tuple[str, asyncio.Task[dict[str, Any]]]] = []
      for name, (method, path, payload) in definitions.items():
        count = samples
        if name == tree_name:
          count = max(0, samples - 1) if include_tree else 0
        other_requests.extend(
            (name, asyncio.create_task(_timed_request(client, method, path, payload))) for _ in range(count))
      first_result = None if first_tree is None else await first_tree
      responses = await asyncio.gather(*(task for _, task in other_requests))
      measured_names = list(definitions) if include_tree else [name for name in definitions if name != tree_name]
      values: dict[str, list[float]] = {name: [] for name in measured_names}
      statuses: dict[str, list[int]] = {name: [] for name in measured_names}
      if first_result is not None:
        values[tree_name].append(first_result["duration_ms"])
        statuses[tree_name].append(first_result["status"])
      for (name, _task), response in zip(other_requests, responses, strict=True):
        values[name].append(response["duration_ms"])
        statuses[name].append(response["status"])
      metrics = {
          name:
              {
                  "count": len(durations),
                  "p50_ms": round(_quantile(durations, 0.50), 3),
                  "p95_ms": round(_quantile(durations, 0.95), 3),
                  "longest_ms": round(max(durations), 3),
                  "statuses": statuses[name],
              } for name, durations in values.items()
      }
      cold_first_tree_ms = None if first_result is None else round(first_result["duration_ms"], 3)
      return {"cold_first_tree_ms": cold_first_tree_ms, "endpoints": metrics}
    first_result = await first_tree
  warm_metrics = await _measure_warm_batch(base_url, access_key, session_ids, samples)
  warm_metrics[tree_name] = {
      "count": 1,
      "p50_ms": round(first_result["duration_ms"], 3),
      "p95_ms": round(first_result["duration_ms"], 3),
      "longest_ms": round(first_result["duration_ms"], 3),
      "statuses": [first_result["status"]],
  }
  return {"cold_first_tree_ms": round(first_result["duration_ms"], 3), "endpoints": warm_metrics}


def _judge_against_reference(concurrent: dict[str, Any], reference: dict[str, Any]) -> dict[str, Any]:
  """Judge each non-tree endpoint of the cold burst against the same burst without tree requests.

  The plan's standard: p95 at most twice the reference p95, and the longest request at most the
  reference longest plus 5 s. The tree endpoint is reported by its own time and is not judged here.
  """
  tree_name = "GET /api/sessions/tree"
  verdicts: dict[str, dict[str, Any]] = {}
  for name, measured in concurrent.items():
    if name == tree_name:
      continue
    base = reference[name]
    p95_limit = COLD_STANDARD_P95_RATIO * base["p95_ms"]
    longest_limit = base["longest_ms"] + COLD_STANDARD_LONGEST_SLACK_MS
    verdicts[name] = {
        "reference_p95_ms": base["p95_ms"],
        "concurrent_p95_ms": measured["p95_ms"],
        "p95_limit_ms": round(p95_limit, 3),
        "reference_longest_ms": base["longest_ms"],
        "concurrent_longest_ms": measured["longest_ms"],
        "longest_limit_ms": round(longest_limit, 3),
        "passed": measured["p95_ms"] <= p95_limit and measured["longest_ms"] <= longest_limit,
    }
  return {
      "standard": "p95 <= 2 x reference p95 and longest <= reference longest + 5000 ms",
      "endpoints": verdicts,
      "passed": all(verdict["passed"] for verdict in verdicts.values()),
  }


def _choose_port() -> int:
  with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
    listener.bind(("127.0.0.1", 0))
    port = listener.getsockname()[1]
  if port == 18498:
    raise RuntimeError("ephemeral port selection returned the live server port")
  return port


def _wait_for_server_ready(path: pathlib.Path, scope: subprocess.Popen[str], timeout: float = 45.0) -> dict[str, Any]:
  deadline = time.monotonic() + timeout
  while time.monotonic() < deadline:
    if scope.poll() is not None:
      raise RuntimeError(f"test server scope exited before listening: {scope.returncode}")
    if path.is_file():
      return json.loads(path.read_text(encoding="utf-8"))
    time.sleep(0.05)
  raise TimeoutError(f"test server did not listen within {timeout:g}s")


def _assert_zero_children(pid: int) -> list[int]:
  children_path = pathlib.Path(f"/proc/{pid}/task/{pid}/children")
  if not children_path.is_file():
    raise RuntimeError(f"test server pid disappeared before its child-process check: {pid}")
  children = [int(value) for value in children_path.read_text(encoding="ascii").split()]
  if children:
    raise RuntimeError(f"test server pid {pid} has child processes: {children}")
  return children


def _probe_thread(guard: PressureGuard) -> threading.Thread:
  thread = threading.Thread(target=guard.run, name="charliebot-pressure-live-guard", daemon=True)
  thread.start()
  return thread


def _copy_info(copy: dict[str, Any], synthetic_count: int) -> dict[str, Any]:
  return {
      "session_count": len(copy["session_ids"]) + synthetic_count,
      "linked_live_session_count": len(copy["session_ids"]),
      "synthetic_session_count": synthetic_count,
      "linked_file_count": copy["linked_file_count"],
      "linked_file_bytes": copy["linked_file_bytes"],
      "live_event_bytes": copy["live_event_bytes"],
  }


def _base_bounds(layout: str, disk_delay_ms: float) -> dict[str, Any]:
  bounds = {
      "user_slice": PRESSURE_SLICE,
      "slice_memory_max_bytes": PARENT_MEMORY_MAX,
      "slice_memory_swap_max_bytes": PARENT_SWAP_MAX,
      "test_server_memory_max_bytes": SERVER_MEMORY_MAX,
      "test_server_memory_swap_max_bytes": 0 if layout == "protected" else PARENT_SWAP_MAX,
      "memory_pressure_cap_bytes": int(MAX_MEMORY_PRESSURE_GIB * GIB),
      "memory_swap_excess_bytes": MEMORY_PRESSURE_SWAP_EXCESS_BYTES,
      "repair_probe_touch_bytes": REPAIR_PROBE_BYTES,
      "repair_probe_runs": REPAIR_PROBE_RUNS,
      "disk_read_delay_ms": disk_delay_ms,
      "disk_result_approximate": True,
      "memavailable_stop_bytes": MEMAVAILABLE_FLOOR,
      "server_peak_rss_limit_bytes": SERVER_MEMORY_MAX,
      "lifespan": "off",
  }
  if layout == "protected":
    bounds.update(
        {
            "agents_child_slice": PRESSURE_AGENTS_SLICE,
            "agents_child_memory_high_bytes": AGENTS_MEMORY_HIGH,
            "agents_child_memory_max_bytes": AGENTS_MEMORY_MAX,
            "agents_child_memory_swap_max_bytes": 0,
            "repair_child_slice": PRESSURE_REPAIR_SLICE,
            "repair_child_memory_max_bytes": REPAIR_MEMORY_MAX,
            "repair_child_memory_swap_max_bytes": 0,
        })
  return bounds


class LiveLogWindow:
  """Read only the request-log bytes appended during the pressure run."""

  def __init__(self, path: pathlib.Path) -> None:
    self.path = path
    self.stream = path.open("rb")
    self.identity = (os.fstat(self.stream.fileno()).st_dev, os.fstat(self.stream.fileno()).st_ino)
    self.offset = os.fstat(self.stream.fileno()).st_size

  def read_new(self) -> bytes:
    current = self.path.stat()
    if (current.st_dev, current.st_ino) != self.identity:
      raise RuntimeError("server-latest.log changed identity during the pressure run")
    self.stream.seek(self.offset)
    return self.stream.read()

  def close(self) -> None:
    self.stream.close()


def _parse_live_latency(data: bytes, guard_runs: list[dict[str, Any]]) -> dict[str, Any]:
  pattern = re.compile(r"^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}).*http_request.*duration_ms=(\d+(?:\.\d+)?)")
  entries = []
  per_minute: dict[str, list[float]] = {}
  for raw_line in data.decode("utf-8", errors="replace").splitlines():
    match = pattern.search(raw_line)
    if match is None:
      continue
    local = datetime.datetime.strptime(match.group(1), "%Y-%m-%d %H:%M:%S").astimezone()
    duration_ms = float(match.group(2))
    minute = local.strftime("%Y-%m-%d %H:%M")
    per_minute.setdefault(minute, []).append(duration_ms)
    entries.append({"at": local.astimezone(datetime.timezone.utc), "duration_ms": duration_ms})
  freeze_times = [
      datetime.datetime.fromisoformat(interval["started_at"]).astimezone(datetime.timezone.utc)
      for run in guard_runs
      for interval in run["freeze_intervals"]
  ]
  slow = []
  for entry in entries:
    if entry["duration_ms"] <= 10_000:
      continue
    started = entry["at"].timestamp() - entry["duration_ms"] / 1000
    followed = any(started <= frozen.timestamp() <= started + 2 for frozen in freeze_times)
    slow.append({"duration_ms": round(entry["duration_ms"], 3), "freeze_followed_within_2s": followed})
  long_gaps = []
  for previous, current in itertools.pairwise(entries):
    gap_seconds = (current["at"] - previous["at"]).total_seconds()
    if gap_seconds <= 10:
      continue
    gap_start = previous["at"].timestamp()
    followed = any(gap_start <= frozen.timestamp() <= gap_start + 2 for frozen in freeze_times)
    long_gaps.append(
        {
            "started_at": previous["at"].isoformat(),
            "ended_at": current["at"].isoformat(),
            "gap_seconds": round(gap_seconds, 3),
            "freeze_followed_within_2s": followed,
        })
  minute_summary = {
      minute:
          {
              "request_count": len(values),
              "p50_ms": round(_quantile(values, 0.50), 3),
              "p95_ms": round(_quantile(values, 0.95), 3),
              "longest_ms": round(max(values), 3),
          } for minute, values in sorted(per_minute.items())
  }
  unguarded = [entry for entry in slow if not entry["freeze_followed_within_2s"]]
  unguarded_gaps = [entry for entry in long_gaps if not entry["freeze_followed_within_2s"]]
  return {
      "source":
          str(LIVE_LOG),
      "observed_request_count":
          len(entries),
      "per_minute_request_latency":
          minute_summary,
      "requests_over_10s":
          slow,
      "unguarded_requests_over_10s":
          unguarded,
      "request_gaps_over_10s":
          long_gaps,
      "unguarded_request_gaps_over_10s":
          unguarded_gaps,
      "longest_request_log_gap_s":
          round(
              max(
                  (
                      (current["at"] - previous["at"]).total_seconds()
                      for previous, current in itertools.pairwise(entries)),
                  default=0.0), 3),
      "passed":
          bool(entries) and not unguarded and not unguarded_gaps,
  }


def _test_server_log_gaps(path: pathlib.Path) -> dict[str, Any]:
  timestamp = re.compile(r"^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})")
  times = []
  for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
    match = timestamp.match(line)
    if match is not None:
      times.append(datetime.datetime.strptime(match.group(1), "%Y-%m-%d %H:%M:%S").astimezone(datetime.timezone.utc))
  gaps = [(second - first).total_seconds() for first, second in itertools.pairwise(times)]
  return {
      "timestamped_line_count": len(times),
      "longest_gap_s": round(max(gaps), 3) if gaps else None,
  }


def _make_guard(access_key: str, slice_cgroup: pathlib.Path) -> PressureGuard:
  return PressureGuard(
      probe=lambda: live_http_probe(access_key),
      memory_available=read_memavailable_bytes,
      freeze=lambda: _freeze_cgroup(slice_cgroup),
      thaw=lambda: _thaw_cgroup(slice_cgroup),
      stop_slice=lambda: _remove_pressure_slice(slice_cgroup),
  )


def _read_cgroup_bound(path: pathlib.Path, name: str) -> int | str:
  value = (path / name).read_text(encoding="ascii").strip()
  return "max" if value == "max" else int(value)


def _read_cgroup_event_counters(path: pathlib.Path, file_name: str, required: set[str]) -> dict[str, int]:
  counters = {}
  for line in (path / file_name).read_text(encoding="ascii").splitlines():
    key, separator, value = line.partition(" ")
    if separator:
      counters[key] = int(value)
  missing = required - counters.keys()
  if missing:
    raise RuntimeError(f"{path / file_name} lacks the counters {sorted(missing)}: {counters}")
  return counters


def _read_memory_events(cgroup: pathlib.Path) -> dict[str, dict[str, int]]:
  return {
      "memory_events": _read_cgroup_event_counters(cgroup, "memory.events", {"high", "max", "oom_kill"}),
      "memory_swap_events": _read_cgroup_event_counters(cgroup, "memory.swap.events", {"high", "max", "fail"}),
  }


class VmSwapSampler:
  """Sample the test server's VmSwap once per second and keep the peak."""

  def __init__(
      self,
      *,
      pid: int,
      read_status: Callable[[int], dict[str, int]] = _read_proc_status,
      interval_s: float = VM_SWAP_SAMPLE_INTERVAL_S,
      wall_clock: Callable[[], str] = utc_now,
  ) -> None:
    self._pid = pid
    self._read_status = read_status
    self._interval_s = interval_s
    self._wall_clock = wall_clock
    self.stop_event = threading.Event()
    self.samples: list[dict[str, Any]] = []
    self.peak_kib = 0
    self.error: Exception | None = None

  def run(self) -> None:
    while not self.stop_event.is_set():
      try:
        swap_kib = self._read_status(self._pid)["VmSwap"]
      except (OSError, RuntimeError) as exc:
        self.error = exc
        return
      self.peak_kib = max(self.peak_kib, swap_kib)
      self.samples.append({"at": self._wall_clock(), "vm_swap_kib": swap_kib})
      if self.stop_event.wait(self._interval_s):
        return

  def stop(self) -> None:
    self.stop_event.set()

  def snapshot(self) -> dict[str, Any]:
    return {
        "sample_interval_s": self._interval_s,
        "sample_count": len(self.samples),
        "peak_kib": self.peak_kib,
        "samples": list(self.samples),
    }


def _prepare_home(copy: dict[str, Any], port: int, synthetic_ids: list[str]) -> tuple[list[pathlib.Path], pathlib.Path]:
  home = copy["home"]
  write_minimal_config(home, port)
  writable_dirs = write_synthetic_sessions(home, synthetic_ids)
  control = pathlib.Path(copy["copy_root"]) / "control" / uuid.uuid4().hex
  control.mkdir(parents=True)
  return writable_dirs, control


def _start_server(
    *,
    copy: dict[str, Any],
    port: int,
    layout: str,
    disk_delay_ms: float,
    writable_dirs: list[pathlib.Path],
    control: pathlib.Path,
    server_log: pathlib.Path,
    run_name: str,
) -> tuple[subprocess.Popen[str], pathlib.Path, pathlib.Path, str]:
  linked_manifest = pathlib.Path(copy["copy_root"]) / "copy_manifest.json"
  start_file = control / "start"
  ready_file = control / "ready.json"
  unit = f"charliebot-pressure-server-{run_name}-{uuid.uuid4().hex[:8]}.scope"
  properties = [f"MemoryMax={SERVER_MEMORY_MAX}", f"MemorySwapMax={0 if layout == 'protected' else PARENT_SWAP_MAX}"]
  command = [
      sys.executable,
      str(pathlib.Path(__file__).resolve()),
      "_serve",
      "--home",
      str(copy["home"]),
      "--port",
      str(port),
      "--linked-manifest",
      str(linked_manifest),
      "--control-dir",
      str(control),
      "--start-file",
      str(start_file),
      "--stop-file",
      str(control / "stop"),
      "--ready-file",
      str(ready_file),
      "--disk-delay-ms",
      str(disk_delay_ms),
  ]
  command.extend(value for directory in writable_dirs for value in ("--writable-dir", str(directory)))
  server_log.parent.mkdir(parents=True, exist_ok=True)
  log_fd = server_log.open("w", encoding="utf-8")
  unit_process = _systemd_run_command(
      unit=unit,
      slice_name=PRESSURE_SLICE,
      properties=properties,
      command=command,
      home=copy["home"],
      stdout=log_fd,
      stderr=subprocess.STDOUT,
  )
  cgroup = _wait_for_cgroup(unit, unit_process)
  setattr(unit_process, "_pressure_log_fd", log_fd)
  return unit_process, cgroup, start_file, unit


def _wait_scope_exit(
    process: subprocess.Popen[str],
    unit: str,
    cgroup: pathlib.Path,
    *,
    start_file: pathlib.Path | None = None,
    stop_file: pathlib.Path | None = None,
    allow_sigterm: bool = False,
    allow_sigkill: bool = False,
    wait_timeout_s: float = 30.0,
) -> int:
  if process.poll() is None:
    if start_file is not None and stop_file is not None:
      start_file.touch(exist_ok=True)
      stop_file.touch(exist_ok=True)
    else:
      subprocess.run(["/usr/bin/systemctl", "--user", "stop", unit], check=True, capture_output=True, text=True)
  try:
    code = process.wait(timeout=wait_timeout_s)
  except subprocess.TimeoutExpired:
    raise TimeoutError(f"systemd-run client for {unit} did not exit after stop")
  if cgroup.exists() and _cgroup_pids(cgroup):
    raise RuntimeError(f"scope {unit} retains processes after shutdown")
  log_fd = getattr(process, "_pressure_log_fd", None)
  if log_fd is not None:
    log_fd.close()
  accepted_codes = {0}
  if allow_sigterm:
    accepted_codes.add(-signal.SIGTERM)
  if allow_sigkill:
    accepted_codes.add(-signal.SIGKILL)
  if code not in accepted_codes:
    raise RuntimeError(f"scope {unit} exited with status {code}")
  return code


class PressureWorker:
  """One systemd scope that allocates the pressure target and rewrites its pages."""

  def __init__(
      self,
      *,
      process: subprocess.Popen[str],
      unit: str,
      cgroup: pathlib.Path,
      log_fd: Any,
      ready_file: pathlib.Path,
      target_bytes: int,
      scope_bounds: dict[str, Any] | None = None,
      slice_bounds: dict[str, Any] | None = None,
  ) -> None:
    self.process = process
    self.unit = unit
    self.cgroup = cgroup
    self.log_fd = log_fd
    self.ready_file = ready_file
    self.target_bytes = target_bytes
    self.allocated = False
    self.scope_bounds = scope_bounds
    self.slice_bounds = slice_bounds

  def is_running(self) -> bool:
    return self.process.poll() is None

  def exit_code(self) -> int:
    return self.process.returncode

  def poll_allocation(self) -> bool:
    """Check the worker's ready file once; a worker killed mid-allocation never reports it."""
    if self.allocated:
      return True
    if not self.ready_file.is_file():
      return False
    allocation = json.loads(self.ready_file.read_text(encoding="utf-8"))
    if int(allocation["allocated_bytes"]) != self.target_bytes:
      raise RuntimeError(f"pressure worker allocation did not reach its target: {allocation}")
    self.allocated = True
    return True

  def close_log(self) -> None:
    if not self.log_fd.closed:
      self.log_fd.close()

  def stop(self) -> int:
    # A worker crawling under MemoryHigh reclaim answers SIGTERM slowly, so systemd
    # escalates to SIGKILL; a scope we stopped ourselves counts as stopped either way.
    code = _wait_scope_exit(
        self.process,
        self.unit,
        self.cgroup,
        allow_sigterm=True,
        allow_sigkill=True,
        wait_timeout_s=120.0,
    )
    self.close_log()
    return code


class PressureSupervisor:
  """Restart the pressure worker until the measured window ends, recording kernel kills.

  The measured window starts when a worker reaches its allocation target or when
  the kernel ends the first worker, whichever comes first; the agents child slice
  in the protected layout can never hold the full target, so the kill starts it.
  """

  def __init__(
      self,
      *,
      start_worker: Callable[[], PressureWorker],
      kill_total: Callable[[], int | None],
      wall_clock: Callable[[], str] = utc_now,
      poll_interval_s: float = 0.05,
  ) -> None:
    self._start_worker = start_worker
    self._kill_total = kill_total
    self._wall_clock = wall_clock
    self._poll_interval_s = poll_interval_s
    self.stop_event = threading.Event()
    self.first_activity = threading.Event()
    self.kills: list[dict[str, Any]] = []
    self.worker_starts: list[str] = []
    self.worker_exits: list[int] = []
    self.active: PressureWorker | None = None
    self.failure: Exception | None = None
    self._last_kill_total: int | None = None
    self._cumulative_kills = 0

  def run(self) -> None:
    try:
      self._run_loop()
    except Exception as exc:
      self.failure = exc
      self.stop_event.set()
      self.first_activity.set()

  def _run_loop(self) -> None:
    while not self.stop_event.is_set():
      worker = self._start_worker()
      self.active = worker
      self.worker_starts.append(worker.unit)
      while worker.is_running() and not self.stop_event.is_set():
        if worker.poll_allocation():
          self.first_activity.set()
        self.stop_event.wait(self._poll_interval_s)
      if worker.poll_allocation():
        self.first_activity.set()
      if worker.is_running():
        return
      code = worker.exit_code()
      self.worker_exits.append(code)
      if code in (0, -signal.SIGTERM):
        return
      total = self._kill_total()
      # A wiped and recreated slice restarts its counter at zero, so a total
      # below the previous one means the fresh generation's count is all new.
      if total is not None and self._last_kill_total is not None and total < self._last_kill_total:
        self._cumulative_kills += total
      elif total is not None and self._last_kill_total is not None:
        self._cumulative_kills += total - self._last_kill_total
      elif total is not None:
        self._cumulative_kills += total
      self._last_kill_total = total
      self.kills.append(
          {
              "at": self._wall_clock(),
              "worker_exit_code": code,
              "oom_kill_total": total,
              "cumulative_kills": self._cumulative_kills,
          })
      self.first_activity.set()
      if len(self.kills) > MAX_KILL_RESTARTS:
        raise RuntimeError(f"the kernel ended the pressure worker more than {MAX_KILL_RESTARTS} times")
      if self.stop_event.is_set():
        return

  def stop(self) -> None:
    self.stop_event.set()


def _kill_fill_path(run_name: str) -> pathlib.Path:
  """Keep the kill fill in tmpfs: kernel-context charges with no writeback IO."""
  return KILL_FILL_DIR / f"charliebot-pressure-kill-fill-{run_name}.bin"


def _start_pressure_worker(
    *,
    layout: str,
    target_bytes: int,
    control_dir: pathlib.Path,
    parent_slice_cgroup: pathlib.Path,
    log_path: pathlib.Path,
    kill_fill_file: pathlib.Path | None = None,
) -> PressureWorker:
  worker_id = uuid.uuid4().hex[:8]
  slice_name = PRESSURE_SLICE if layout == "current" else PRESSURE_AGENTS_SLICE
  unit = f"charliebot-pressure-memory-{worker_id}.scope"
  ready_file = control_dir / f"pressure_ready_{worker_id}.json"
  start_file = control_dir / f"pressure_start_{worker_id}"
  properties = ["MemorySwapMax=0"] if layout == "protected" else []
  command = [
      sys.executable,
      str(pathlib.Path(__file__).resolve()),
      "_pressure",
      "--size-bytes",
      str(target_bytes),
      "--start-file",
      str(start_file),
      "--ready-file",
      str(ready_file),
  ]
  if layout == "protected":
    command.extend(("--kill-fill-file", str(kill_fill_file), "--kill-fill-delay-s", "45.0"))
  log_path.parent.mkdir(parents=True, exist_ok=True)
  log_fd = log_path.open("w", encoding="utf-8")
  process = _systemd_run_command(
      unit=unit,
      slice_name=slice_name,
      properties=properties,
      command=command,
      stdout=log_fd,
      stderr=subprocess.STDOUT,
  )
  cgroup = _wait_for_cgroup(unit, process)
  slice_bounds: dict[str, Any] | None = None
  if layout == "protected":
    # systemd wipes every member of a slice on a memcg OOM and the transient
    # slice then unloads, so each worker generation recreates it with default
    # limits. Re-apply the layout before the worker starts allocating, and use
    # a per-worker start file so no allocation runs ahead of the configuration.
    agents_slice_cgroup = parent_slice_cgroup / PRESSURE_AGENTS_SLICE
    _configure_unit(
        PRESSURE_AGENTS_SLICE,
        [f"MemoryHigh={AGENTS_MEMORY_HIGH}", f"MemoryMax={AGENTS_MEMORY_MAX}", "MemorySwapMax=0"],
    )
    slice_bounds = {
        "memory_high_bytes": _read_cgroup_bound(agents_slice_cgroup, "memory.high"),
        "memory_max_bytes": _read_cgroup_bound(agents_slice_cgroup, "memory.max"),
        "memory_swap_max_bytes": _read_cgroup_bound(agents_slice_cgroup, "memory.swap.max"),
    }
    if slice_bounds != {
        "memory_high_bytes": AGENTS_MEMORY_HIGH,
        "memory_max_bytes": AGENTS_MEMORY_MAX,
        "memory_swap_max_bytes": 0,
    }:
      raise RuntimeError(f"agents child slice bounds differ from the requested values: {slice_bounds}")
  start_file.touch()
  scope_bounds = {
      "unit": unit,
      "slice": slice_name,
      "memory_max_bytes": _read_cgroup_bound(cgroup, "memory.max"),
      "memory_swap_max_bytes": _read_cgroup_bound(cgroup, "memory.swap.max"),
  }
  # Return without waiting for allocation: in the protected layout the worker
  # crawls through MemoryHigh reclaim for minutes before the kernel ends it, so
  # the supervisor polls the ready file instead of blocking here.
  return PressureWorker(
      process=process,
      unit=unit,
      cgroup=cgroup,
      log_fd=log_fd,
      ready_file=ready_file,
      target_bytes=target_bytes,
      scope_bounds=scope_bounds,
      slice_bounds=slice_bounds,
  )


def _start_keeper_scope(*, slice_name: str, log_path: pathlib.Path) -> tuple[subprocess.Popen[str], str, pathlib.Path]:
  """Hold a transient user slice open so its bounds and memory.events stay readable."""
  sleep = shutil.which("sleep", path=SERVER_PATH)
  if sleep is None:
    raise FileNotFoundError("sleep is missing from the restricted test PATH")
  unit = f"charliebot-pressure-keeper-{uuid.uuid4().hex[:8]}.scope"
  log_fd = log_path.open("w", encoding="utf-8")
  process = _systemd_run_command(
      unit=unit,
      slice_name=slice_name,
      properties=[],
      command=[sleep, "3600"],
      stdout=log_fd,
      stderr=subprocess.STDOUT,
  )
  cgroup = _wait_for_cgroup(unit, process)
  return process, unit, cgroup


def _stop_keeper_scope(process: subprocess.Popen[str], unit: str, cgroup: pathlib.Path) -> None:
  _wait_scope_exit(process, unit, cgroup, allow_sigterm=True)


def _repair_probe_read_file(copy: dict[str, Any]) -> pathlib.Path:
  for session_id in copy["session_ids"]:
    path = pathlib.Path(copy["home"]) / "sessions" / session_id / "metadata.json"
    if path.is_file() and 0 < path.stat().st_size <= MIB:
      return path
  raise RuntimeError("the copy holds no small linked metadata file for the repair probe")


def _run_repair_probes(
    *,
    count: int,
    slice_name: str,
    read_file: pathlib.Path,
    log_dir: pathlib.Path,
    stage_name: str,
) -> dict[str, Any]:
  runs = []
  for index in range(count):
    unit = f"charliebot-pressure-repair-{uuid.uuid4().hex[:8]}.scope"
    log_path = log_dir / f"repair_{stage_name}_{index}.log"
    with log_path.open("w", encoding="utf-8") as log_fd:
      started = time.perf_counter()
      process = _systemd_run_command(
          unit=unit,
          slice_name=slice_name,
          properties=[],
          command=[
              sys.executable,
              str(pathlib.Path(__file__).resolve()),
              "_repair_probe",
              "--touch-bytes",
              str(REPAIR_PROBE_BYTES),
              "--read-file",
              str(read_file),
          ],
          stdout=subprocess.PIPE,
          stderr=log_fd,
      )
      stdout, _ = process.communicate()
      wall_ms = (time.perf_counter() - started) * 1000
      log_fd.write(stdout)
    code = process.returncode
    if code != 0:
      raise RuntimeError(f"repair probe {unit} exited with status {code}; see {log_path}")
    probe = json.loads(stdout)
    runs.append(
        {
            "unit": unit,
            "wall_ms": round(wall_ms, 3),
            "probe_ms": probe["probe_ms"],
            "read_bytes": probe["read_bytes"],
            "touched_bytes": probe["touched_bytes"],
            "exit_code": code,
            "killed": False,
        })
  probe_times = [run["probe_ms"] for run in runs]
  return {
      "slice": slice_name,
      "run_count": count,
      "runs": runs,
      "probe_p50_ms": round(_quantile(probe_times, 0.50), 3),
      "probe_p95_ms": round(_quantile(probe_times, 0.95), 3),
      "longest_probe_ms": round(max(probe_times), 3),
      "killed_run_count": sum(run["killed"] for run in runs),
  }


def _run_server_stage(
    *,
    copy: dict[str, Any],
    stage_name: str,
    results_dir: pathlib.Path,
    access_key: str,
    layout: str,
    sample_count: int,
    guard_callback: Callable[[dict[str, Any]], None],
    pressure_cap_bytes: int | None = None,
    disk_delay_ms: float = 0.0,
    cold_concurrent: bool = False,
    cold_without_tree: bool = False,
    cold_baseline: bool = False,
    run_repair_probes: bool = False,
) -> dict[str, Any]:
  is_pressure_stage = pressure_cap_bytes is not None
  synthetic_ids = [str(uuid.uuid4()), str(uuid.uuid4())]
  port = _choose_port()
  writable_dirs, control = _prepare_home(copy, port, synthetic_ids)
  run_name = f"{stage_name}-{uuid.uuid4().hex[:6]}"
  server_log = results_dir / "logs" / f"server_{run_name}.log"
  server_process, server_cgroup, start_file, server_unit = _start_server(
      copy=copy,
      port=port,
      layout=layout,
      disk_delay_ms=disk_delay_ms,
      writable_dirs=writable_dirs,
      control=control,
      server_log=server_log,
      run_name=run_name,
  )
  slice_cgroup = server_cgroup.parent
  guard: PressureGuard | None = None
  guard_thread: threading.Thread | None = None
  sampler: VmSwapSampler | None = None
  sampler_thread: threading.Thread | None = None
  supervisor: PressureSupervisor | None = None
  supervisor_thread: threading.Thread | None = None
  repair_keeper: tuple[subprocess.Popen[str], str, pathlib.Path] | None = None
  pressure_target_bytes: int | None = None
  server_rss_anon_kib_after_warmup: int | None = None
  window_started_by: str | None = None
  kill_fill_file: pathlib.Path | None = None
  try:
    if slice_cgroup.name != PRESSURE_SLICE:
      raise RuntimeError(f"test server did not enter {PRESSURE_SLICE}: {server_cgroup}")
    if _cgroup_pids(slice_cgroup) != _cgroup_pids(server_cgroup):
      raise RuntimeError(
          f"pressure slice contains processes outside the test server: {sorted(_cgroup_pids(slice_cgroup))}")
    expected_swap = 0 if layout == "protected" else PARENT_SWAP_MAX
    server_bounds = {
        "memory_max_bytes": _read_cgroup_bound(server_cgroup, "memory.max"),
        "memory_swap_max_bytes": _read_cgroup_bound(server_cgroup, "memory.swap.max"),
    }
    if server_bounds != {"memory_max_bytes": SERVER_MEMORY_MAX, "memory_swap_max_bytes": expected_swap}:
      raise RuntimeError(f"test server cgroup bounds differ from the requested layout: {server_bounds}")
    _configure_unit(PRESSURE_SLICE, [f"MemoryMax={PARENT_MEMORY_MAX}", f"MemorySwapMax={PARENT_SWAP_MAX}"])
    slice_bounds = {
        "memory_max_bytes": _read_cgroup_bound(slice_cgroup, "memory.max"),
        "memory_swap_max_bytes": _read_cgroup_bound(slice_cgroup, "memory.swap.max"),
    }
    if slice_bounds != {"memory_max_bytes": PARENT_MEMORY_MAX, "memory_swap_max_bytes": PARENT_SWAP_MAX}:
      raise RuntimeError(f"pressure slice bounds differ from the requested values: {slice_bounds}")
    guard = _make_guard(access_key, slice_cgroup)
    guard_thread = _probe_thread(guard)
    start_file.touch()
    ready = _wait_for_server_ready(control / "ready.json", server_process)
    pid = int(ready["pid"])
    if _cgroup_pids(server_cgroup) != {pid}:
      raise RuntimeError(f"test server scope has unexpected processes: {sorted(_cgroup_pids(server_cgroup))}")
    agents_slice_cgroup: pathlib.Path | None = None
    agents_slice_bounds: dict[str, int | str] | None = None
    repair_slice_cgroup: pathlib.Path | None = None
    repair_slice_bounds: dict[str, int | str] | None = None
    if layout == "protected":
      # The agents slice holds no keeper: an OOM in it wipes every member, so the
      # slice unloads and each worker generation recreates and reconfigures it.
      agents_slice_cgroup = slice_cgroup / PRESSURE_AGENTS_SLICE
      repair_keeper = _start_keeper_scope(
          slice_name=PRESSURE_REPAIR_SLICE, log_path=results_dir / "logs" / f"repair_keeper_{run_name}.log")
      repair_slice_cgroup = repair_keeper[2].parent
      _configure_unit(PRESSURE_REPAIR_SLICE, [f"MemoryMax={REPAIR_MEMORY_MAX}", "MemorySwapMax=0"])
      repair_slice_bounds = {
          "memory_max_bytes": _read_cgroup_bound(repair_slice_cgroup, "memory.max"),
          "memory_swap_max_bytes": _read_cgroup_bound(repair_slice_cgroup, "memory.swap.max"),
      }
      if repair_slice_bounds != {"memory_max_bytes": REPAIR_MEMORY_MAX, "memory_swap_max_bytes": 0}:
        raise RuntimeError(f"repair child slice bounds differ from the requested values: {repair_slice_bounds}")
    sampler = VmSwapSampler(pid=pid)
    sampler_thread = threading.Thread(target=sampler.run, name="charliebot-pressure-vm-swap-sampler", daemon=True)
    sampler_thread.start()
    base_url = f"http://127.0.0.1:{port}"
    if cold_baseline:
      cold_metrics = asyncio.run(
          _measure_cold_burst(base_url, access_key, synthetic_ids, sample_count, concurrent=False))
      metrics = cold_metrics["endpoints"]
      cold_first_tree_ms = cold_metrics["cold_first_tree_ms"]
    else:
      if stage_name.startswith("memory"):
        import httpx

        async def warm_cache() -> None:
          async with httpx.AsyncClient(
              base_url=base_url,
              headers={"Authorization": f"Bearer {access_key}"},
              timeout=120.0,
          ) as client:
            response = await client.get("/api/sessions/tree")
            if response.status_code != 200:
              raise RuntimeError(f"cache warmup returned {response.status_code}: {response.text[:300]}")

        asyncio.run(warm_cache())
      if is_pressure_stage:
        server_rss_anon_kib_after_warmup = _read_proc_status(pid)["RssAnon"]
        pressure_target_bytes = compute_memory_pressure_target(
            server_rss_anon_bytes=server_rss_anon_kib_after_warmup * 1024,
            slice_memory_max_bytes=PARENT_MEMORY_MAX,
            slice_swap_max_bytes=PARENT_SWAP_MAX,
            swap_excess_bytes=MEMORY_PRESSURE_SWAP_EXCESS_BYTES,
            probe_reserve_bytes=REPAIR_PROBE_BYTES,
            cap_bytes=pressure_cap_bytes,
        )
        kill_cgroup = agents_slice_cgroup if layout == "protected" else slice_cgroup

        def kill_total() -> int | None:
          # A memcg OOM in the agents slice wipes the whole slice and the
          # transient slice unloads, so the counter may already be gone; race
          # the unload briefly and report None when it is lost.
          deadline = time.monotonic() + 2.0
          while True:
            try:
              return _read_cgroup_event_counters(kill_cgroup, "memory.events", {"high", "max", "oom_kill"})["oom_kill"]
            except OSError:
              if time.monotonic() >= deadline:
                return None
              time.sleep(0.01)

        kill_fill_file = _kill_fill_path(run_name) if layout == "protected" else None

        def start_worker() -> PressureWorker:
          return _start_pressure_worker(
              layout=layout,
              target_bytes=pressure_target_bytes,
              control_dir=control,
              parent_slice_cgroup=slice_cgroup,
              log_path=results_dir / "logs" / f"pressure_{run_name}.log",
              kill_fill_file=kill_fill_file,
          )

        supervisor = PressureSupervisor(start_worker=start_worker, kill_total=kill_total)
        supervisor_thread = threading.Thread(
            target=supervisor.run, name="charliebot-pressure-worker-supervisor", daemon=True)
        supervisor_thread.start()
        window_started_by = None
        deadline = time.monotonic() + PRESSURE_WINDOW_START_TIMEOUT_S
        while not supervisor.first_activity.is_set():
          if (layout == "protected" and agents_slice_cgroup is not None and
              (agents_slice_cgroup / "memory.current").exists()):
            agents_current = int((agents_slice_cgroup / "memory.current").read_text(encoding="ascii"))
            if agents_current >= AGENTS_MEMORY_HIGH:
              window_started_by = "agents_share_over_memory_high"
              break
          if time.monotonic() >= deadline:
            raise TimeoutError(
                "pressure worker never reached its target, was never killed, and the "
                "agents share never crossed its MemoryHigh")
          time.sleep(0.25)
        if window_started_by is None:
          window_started_by = "worker_allocation_or_kill"
      if cold_concurrent:
        cold_metrics = asyncio.run(
            _measure_cold_burst(
                base_url,
                access_key,
                synthetic_ids,
                sample_count,
                concurrent=True,
                include_tree=not cold_without_tree,
            ))
        metrics = cold_metrics["endpoints"]
        cold_first_tree_ms = cold_metrics["cold_first_tree_ms"]
      else:
        metrics = asyncio.run(_measure_warm_batch(base_url, access_key, synthetic_ids, sample_count))
        cold_first_tree_ms = None
    repair_probe: dict[str, Any] | None = None
    if run_repair_probes:
      repair_probe = _run_repair_probes(
          count=REPAIR_PROBE_RUNS,
          slice_name=PRESSURE_REPAIR_SLICE if layout == "protected" else PRESSURE_SLICE,
          read_file=_repair_probe_read_file(copy),
          log_dir=results_dir / "logs",
          stage_name=run_name,
      )
    kill_observation: dict[str, Any] = {"observed": False, "kills": 0, "timed_out_without_kill": False}
    if is_pressure_stage and layout == "protected" and supervisor is not None:
      # The kernel pins a workload between MemoryHigh and MemoryMax instead of
      # reaching MemoryMax quickly, so keep the supervisor restarting workers
      # after the measured window until the first kill lands or the cap passes.
      kill_observation["observed"] = True
      kill_deadline = time.monotonic() + PRESSURE_KILL_OBSERVATION_TIMEOUT_S
      while not supervisor.kills and supervisor.failure is None:
        if not supervisor_thread.is_alive():
          break
        if time.monotonic() >= kill_deadline:
          kill_observation["timed_out_without_kill"] = True
          break
        time.sleep(1.0)
      kill_observation["kills"] = len(supervisor.kills)
    memory_event_groups = [(PRESSURE_SLICE, slice_cgroup), ("test_server_scope", server_cgroup)]
    if layout == "protected":
      # The agents slice only exists while a worker generation lives; after an
      # OOM it is wiped and unloaded, so record it when present.
      if agents_slice_cgroup is not None and agents_slice_cgroup.exists():
        memory_event_groups.append((PRESSURE_AGENTS_SLICE, agents_slice_cgroup))
      memory_event_groups.append((PRESSURE_REPAIR_SLICE, repair_slice_cgroup))
    if supervisor is not None and supervisor.active is not None and supervisor.active.is_running():
      memory_event_groups.append((supervisor.active.unit, supervisor.active.cgroup))
    memory_events_at_end = {name: _read_memory_events(cgroup) for name, cgroup in memory_event_groups}
    pressure_kills: list[dict[str, Any]] = []
    pressure_worker_exits: list[int] = []
    pressure_worker_starts: list[str] = []
    pressure_scope_bounds: dict[str, Any] | None = None
    if supervisor is not None:
      supervisor.stop()
      supervisor_thread.join(timeout=30)
      if supervisor_thread.is_alive():
        raise TimeoutError("pressure worker supervisor did not stop")
      active = supervisor.active
      if active is not None:
        pressure_scope_bounds = active.scope_bounds
        agents_slice_bounds = active.slice_bounds
        if active.is_running():
          active.stop()
        else:
          active.close_log()
      if supervisor.failure is not None:
        raise RuntimeError("pressure worker supervisor failed") from supervisor.failure
      pressure_kills = supervisor.kills
      pressure_worker_exits = supervisor.worker_exits
      pressure_worker_starts = supervisor.worker_starts
      if pressure_kills and layout != "protected":
        raise RuntimeError(f"the current layout recorded pressure worker kills: {pressure_kills}")
    _assert_zero_children(pid)
    process_status = _read_proc_status(pid)
    if process_status["VmHWM"] * 1024 > SERVER_MEMORY_MAX:
      raise RuntimeError(f"test server peak RSS exceeded 3 GiB: {process_status['VmHWM']} KiB")
    sampler.stop()
    sampler_thread.join(timeout=5)
    if sampler_thread.is_alive():
      raise TimeoutError("VmSwap sampler did not stop")
    if sampler.error is not None:
      raise RuntimeError("VmSwap sampler failed while the test server was still running") from sampler.error
    vm_swap = sampler.snapshot()
    guard.stop()
    guard_thread.join(timeout=5)
    if guard_thread.is_alive():
      raise TimeoutError("live-server guard did not stop")
    guard.close()
    guard.raise_if_failed()
    guard_snapshot = guard.snapshot()
    server_code = _wait_scope_exit(
        server_process,
        server_unit,
        server_cgroup,
        start_file=start_file,
        stop_file=control / "stop",
    )
    child_attempts = json.loads((control / "child_process_attempts.json").read_text(encoding="utf-8"))
    if child_attempts:
      raise RuntimeError(f"test server attempted child processes: {child_attempts}")
    read_delay = json.loads((control / "read_delay_count.json").read_text(encoding="utf-8"))
    scope_bounds = {
        "slice_memory_max_bytes": slice_bounds["memory_max_bytes"],
        "slice_memory_swap_max_bytes": slice_bounds["memory_swap_max_bytes"],
        "server_memory_max_bytes": server_bounds["memory_max_bytes"],
        "server_memory_swap_max_bytes": server_bounds["memory_swap_max_bytes"],
    }
    return {
        "port": port,
        "server_log": str(server_log),
        "test_server_log_gaps": _test_server_log_gaps(server_log),
        "server_pid": pid,
        "server_process_exit_code": server_code,
        "server_memory":
            {
                "vm_rss_kib_at_end": process_status["VmRSS"],
                "vm_hwm_kib_peak": process_status["VmHWM"],
                "vm_swap_kib_at_end": process_status["VmSwap"],
                "rss_anon_kib_at_end": process_status["RssAnon"],
                "rss_file_kib_at_end": process_status["RssFile"],
            },
        "bounds_readback": scope_bounds,
        "agents_slice_bounds_readback": agents_slice_bounds,
        "repair_slice_bounds_readback": repair_slice_bounds,
        "pressure_scope_bounds_readback": pressure_scope_bounds,
        "endpoints": metrics,
        "cold_first_tree_ms": cold_first_tree_ms,
        "server_rss_anon_kib_after_warmup": server_rss_anon_kib_after_warmup,
        "pressure_target_bytes": pressure_target_bytes,
        "pressure_swap_excess_bytes": MEMORY_PRESSURE_SWAP_EXCESS_BYTES if is_pressure_stage else 0,
        "pressure_probe_reserve_bytes": REPAIR_PROBE_BYTES if is_pressure_stage else 0,
        "pressure_kills": pressure_kills,
        "pressure_worker_starts": pressure_worker_starts,
        "pressure_worker_exit_codes": pressure_worker_exits,
        "pressure_window_started_by": window_started_by,
        "pressure_kill_observation": kill_observation if is_pressure_stage else None,
        "vm_swap": vm_swap,
        "memory_events_at_end": memory_events_at_end,
        "repair_probe": repair_probe,
        "disk_delay_ms": disk_delay_ms,
        "disk_result_approximate": bool(disk_delay_ms),
        "delayed_read_opens": int(read_delay["delayed_read_opens"]),
        "linked_file_write_assertion": "passed before server imported its app",
        "agent_cli_path": SERVER_PATH,
        "agent_cli_child_process_attempts": child_attempts,
        "server_child_pids_at_end": [],
        "guard": guard_snapshot,
    }
  finally:
    if guard is not None:
      guard.stop()
      if guard_thread is None:
        raise AssertionError("pressure guard thread is missing")
      guard_thread.join(timeout=5)
      if guard_thread.is_alive():
        raise TimeoutError("live-server guard did not stop during cleanup")
      guard.close()
      guard_callback(guard.snapshot())
    if sampler is not None and sampler_thread is not None:
      sampler.stop()
      sampler_thread.join(timeout=5)
    if supervisor is not None:
      supervisor.stop()
      if supervisor_thread is not None:
        supervisor_thread.join(timeout=30)
        if supervisor_thread.is_alive():
          raise TimeoutError("pressure worker supervisor did not stop during cleanup")
      active = supervisor.active
      if active is not None and active.is_running():
        active.stop()
      elif active is not None:
        active.close_log()
    try:
      if kill_fill_file is not None:
        with contextlib.suppress(FileNotFoundError):
          kill_fill_file.unlink()
      for keeper in (repair_keeper,):
        if keeper is not None:
          _stop_keeper_scope(*keeper)
    finally:
      try:
        if server_process.poll() is None:
          _wait_scope_exit(
              server_process,
              server_unit,
              server_cgroup,
              start_file=start_file,
              stop_file=control / "stop",
          )
      finally:
        _remove_pressure_slice(slice_cgroup)


def _run_composition(
    *,
    copy: dict[str, Any],
    results_dir: pathlib.Path,
    access_key: str,
    synthetic_key: str,
    boot_marker: str,
    stage_callback: Callable[[str, dict[str, Any]], None],
    guard_callback: Callable[[dict[str, Any]], None],
) -> dict[str, Any]:
  write_minimal_config(copy["home"], 18499)
  (copy["home"] / "credentials.yaml").write_text(
      "charliebot:\n  access_key: " + json.dumps(synthetic_key) + "\n", encoding="utf-8")
  control = pathlib.Path(copy["copy_root"]) / "composition-control"
  control.mkdir()
  start_file = control / "start"
  log_path = results_dir / "logs" / "composition.log"
  log_path.parent.mkdir(parents=True, exist_ok=True)
  unit = f"charliebot-pressure-composition-{uuid.uuid4().hex[:8]}.scope"
  command = [
      sys.executable,
      str(pathlib.Path(__file__).resolve()),
      "_composition",
      "--home",
      str(copy["home"]),
      "--copy-root",
      str(copy["copy_root"]),
      "--control-dir",
      str(control),
      "--start-file",
      str(start_file),
      "--marker",
      boot_marker,
  ]
  log_stream = log_path.open("w", encoding="utf-8")
  process: subprocess.Popen[str] | None = None
  cgroup: pathlib.Path | None = None
  slice_cgroup: pathlib.Path | None = None
  guard: PressureGuard | None = None
  guard_thread: threading.Thread | None = None
  checkpoint_paths = {
      "after_boot_reads": control / "composition_after_boot.json",
      "after_first_tree_page": control / "composition_after_tree.json",
  }
  emitted: set[str] = set()
  try:
    process = _systemd_run_command(
        unit=unit,
        slice_name=PRESSURE_SLICE,
        properties=[f"MemoryMax={COMPOSITION_MEMORY_MAX}", "MemorySwapMax=0", "CPUQuota=100%"],
        command=command,
        home=copy["home"],
        stdout=log_stream,
        stderr=subprocess.STDOUT,
    )
    cgroup = _wait_for_cgroup(unit, process)
    setattr(process, "_pressure_log_fd", log_stream)
    setattr(process, "_pressure_unit", unit)
    slice_cgroup = cgroup.parent
    if slice_cgroup.name != PRESSURE_SLICE:
      raise RuntimeError(f"composition process did not enter {PRESSURE_SLICE}: {cgroup}")
    if len(_cgroup_pids(cgroup)) != 1:
      raise RuntimeError(f"composition scope process set is not singular: {sorted(_cgroup_pids(cgroup))}")
    _configure_unit(PRESSURE_SLICE, [f"MemoryMax={PARENT_MEMORY_MAX}", f"MemorySwapMax={PARENT_SWAP_MAX}"])
    slice_bounds = {
        "memory_max_bytes": _read_cgroup_bound(slice_cgroup, "memory.max"),
        "memory_swap_max_bytes": _read_cgroup_bound(slice_cgroup, "memory.swap.max"),
    }
    if slice_bounds != {"memory_max_bytes": PARENT_MEMORY_MAX, "memory_swap_max_bytes": PARENT_SWAP_MAX}:
      raise RuntimeError(f"composition pressure slice bounds differ from requested values: {slice_bounds}")
    process_bounds = {
        "memory_max_bytes": _read_cgroup_bound(cgroup, "memory.max"),
        "memory_swap_max_bytes": _read_cgroup_bound(cgroup, "memory.swap.max"),
    }
    if process_bounds != {"memory_max_bytes": COMPOSITION_MEMORY_MAX, "memory_swap_max_bytes": 0}:
      raise RuntimeError(f"composition process bounds differ from requested values: {process_bounds}")
    guard = _make_guard(access_key, slice_cgroup)
    guard_thread = _probe_thread(guard)
    start_file.touch()
    result_path = control / "composition_result.json"
    deadline = time.monotonic() + 600
    while process.poll() is None:
      for stage_name, path in checkpoint_paths.items():
        if stage_name not in emitted and path.is_file():
          stage_callback(stage_name, json.loads(path.read_text(encoding="utf-8")))
          emitted.add(stage_name)
      if time.monotonic() >= deadline:
        raise TimeoutError("composition measurement did not finish within 600 seconds")
      time.sleep(0.05)
    for stage_name, path in checkpoint_paths.items():
      if stage_name not in emitted and path.is_file():
        stage_callback(stage_name, json.loads(path.read_text(encoding="utf-8")))
        emitted.add(stage_name)
    if process.returncode != 0:
      raise RuntimeError(f"composition scope exited {process.returncode}; see {log_path}")
    if emitted != set(checkpoint_paths):
      raise RuntimeError(f"composition scope omitted checkpoints: {sorted(set(checkpoint_paths) - emitted)}")
    guard.stop()
    guard_thread.join(timeout=5)
    if guard_thread.is_alive():
      raise TimeoutError("composition guard did not stop")
    guard.close()
    guard.raise_if_failed()
    result = json.loads(result_path.read_text(encoding="utf-8"))
    result["slice_bounds_readback"] = slice_bounds
    result["process_bounds_readback"] = process_bounds
    result["guard"] = guard.snapshot()
    result["process_log"] = str(log_path)
    return result
  finally:
    if guard is not None:
      guard.stop()
      if guard_thread is None:
        raise AssertionError("composition guard thread is missing")
      guard_thread.join(timeout=5)
      if guard_thread.is_alive():
        raise TimeoutError("composition guard did not stop during cleanup")
      guard.close()
      guard_callback(guard.snapshot())
    if process is not None and cgroup is not None and process.poll() is None:
      _wait_scope_exit(process, unit, cgroup)
    if not log_stream.closed:
      log_stream.close()
    if slice_cgroup is not None:
      _remove_pressure_slice(slice_cgroup)


def _result_payload(
    *,
    copy: dict[str, Any],
    bounds: dict[str, Any],
    revision: str,
    label: str,
) -> dict[str, Any]:
  return {
      "schema_version": 1,
      "label": label,
      "created_at": utc_now(),
      "repo_revision": revision,
      "copy": _copy_info(copy, synthetic_count=2),
      "bounds": bounds,
      "stages": [],
      "guard_runs": [],
      "guard_freeze_intervals": [],
  }


def _git_revision() -> str:
  result = subprocess.run(
      ["git", "rev-parse", "HEAD"],
      cwd=REPO_ROOT,
      check=True,
      capture_output=True,
      text=True,
  )
  return result.stdout.strip()


def _select_ids(
    results_dir: pathlib.Path,
    count: int,
    seed: int,
    reset: bool,
    selection_name: str = "session_selection.json",
) -> list[str]:
  selection_path = results_dir / selection_name
  if reset and selection_path.exists():
    selection_path.unlink()
  if selection_path.exists():
    selection = json.loads(selection_path.read_text(encoding="utf-8"))
    if selection["session_count"] != count or selection["seed"] != seed:
      raise ValueError("saved selection differs from --session-count or --seed; pass --reset-selection to replace it")
    selected = list(selection["session_ids"])
    if len(selected) != count:
      raise ValueError("saved session selection has a different count than its manifest")
    for session_id in selected:
      _session_directory(LIVE_SESSIONS, session_id)
    return selected
  selected = _source_names_once(LIVE_SESSIONS, count, seed)
  flush_json(selection_path, {"session_count": count, "seed": seed, "session_ids": selected})
  return selected


def _assert_pressure_preflight() -> None:
  if os.getuid() == 0:
    raise RuntimeError("pressure probe must run as the unprivileged user that owns the live server")
  if not shutil.which("systemd-run") or not pathlib.Path("/usr/bin/systemctl").is_file():
    raise FileNotFoundError("systemd-run --user and /usr/bin/systemctl are required")
  subprocess.run(["/usr/bin/systemctl", "--user", "show-environment"], check=True, capture_output=True, text=True)
  if not (CGROUP_ROOT / "cgroup.controllers").is_file():
    raise RuntimeError("unified cgroup v2 is required")
  if read_memavailable_bytes() < MEMAVAILABLE_FLOOR:
    raise RuntimeError("MemAvailable is already below the 12 GiB pressure-run floor")
  if not LIVE_SESSIONS.is_dir():
    raise FileNotFoundError(f"live session directory is missing: {LIVE_SESSIONS}")
  if not LIVE_LOG.is_file():
    raise FileNotFoundError(f"live server log is missing: {LIVE_LOG}")
  probe_abi = _landlock_syscall(444, ctypes.c_void_p(0), ctypes.c_size_t(0), ctypes.c_uint(1))
  if probe_abi < 1:
    raise RuntimeError("Landlock filesystem restrictions are required")


def _run_public(args: argparse.Namespace) -> None:
  if not 2 <= args.session_count <= MAX_SESSION_COUNT:
    raise ValueError(f"--session-count must be between 2 and {MAX_SESSION_COUNT}")
  if not 2 <= args.composition_session_count <= MAX_COMPOSITION_SESSION_COUNT:
    raise ValueError(f"--composition-session-count must be between 2 and {MAX_COMPOSITION_SESSION_COUNT}")
  if not 2 <= args.sample_count <= MAX_SAMPLE_COUNT:
    raise ValueError(f"--sample-count must be between 2 and {MAX_SAMPLE_COUNT}")
  if not math.isfinite(args.memory_pressure_gib) or not 0 < args.memory_pressure_gib <= MAX_MEMORY_PRESSURE_GIB:
    raise ValueError(f"--memory-pressure-gib must be above zero and at most {MAX_MEMORY_PRESSURE_GIB}")
  if not math.isfinite(args.disk_delay_ms) or not 0 <= args.disk_delay_ms <= MAX_DISK_DELAY_MS:
    raise ValueError(f"--disk-delay-ms must be between zero and {MAX_DISK_DELAY_MS}")
  if not math.isfinite(args.max_live_event_gib) or not 0 < args.max_live_event_gib <= MAX_LIVE_EVENT_GIB:
    raise ValueError(f"--max-live-event-gib must be above zero and at most {MAX_LIVE_EVENT_GIB}")
  args.results_dir = args.results_dir.resolve()
  args.results_dir.mkdir(parents=True, exist_ok=True)
  _assert_pressure_preflight()
  access_key = load_live_access_key()
  log_window: LiveLogWindow | None = None
  scratch: pathlib.Path | None = None
  recorders: dict[str, ResultRecorder] = {}
  composition_recorder: ResultRecorder | None = None
  try:
    log_window = LiveLogWindow(LIVE_LOG)
    run_id = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "_" + uuid.uuid4().hex[:8]
    scratch = args.results_dir / ".scratch" / run_id
    scratch.mkdir(parents=True, exist_ok=False)
    selected_ids = _select_ids(args.results_dir, args.session_count, args.seed, args.reset_selection)
    event_limit = int(args.max_live_event_gib * GIB)
    main_copy = build_session_copy(
        copy_root=scratch / "main",
        source_sessions=LIVE_SESSIONS,
        selected_ids=selected_ids,
        event_byte_limit=event_limit,
    )
    main_copy["copy_root"] = scratch / "main"
    synthetic_key = "pressure-probe-" + uuid.uuid4().hex
    import yaml

    (main_copy["home"] / "credentials.yaml").write_text(
        yaml.safe_dump({"charliebot": {
            "access_key": synthetic_key
        }}, sort_keys=True), encoding="utf-8")
    bounds_current = _base_bounds("current", args.disk_delay_ms)
    bounds_protected = _base_bounds("protected", args.disk_delay_ms)
    revision = _git_revision()
    result_paths = {
        "memory_current": args.results_dir / "memory_current.json",
        "memory_protected": args.results_dir / "memory_protected.json",
        "disk": args.results_dir / "disk.json",
        "cold": args.results_dir / "cold.json",
        "composition": args.results_dir / "composition.json",
    }
    wants_memory = args.case in ("all", "memory")
    wants_disk = args.case in ("all", "disk")
    wants_cold = args.case in ("all", "cold")
    wants_composition = args.case in ("all", "composition")
    composition_ids = _select_ids(
        args.results_dir,
        args.composition_session_count,
        args.seed + 1,
        args.reset_selection,
        selection_name="composition_selection.json",
    ) if wants_composition else []
    memory_layouts = ("current", "protected") if args.case == "all" else (args.layout,)
    if wants_memory:
      for layout in memory_layouts:
        bounds = bounds_current if layout == "current" else bounds_protected
        recorders[f"memory_{layout}"] = ResultRecorder(
            result_paths[f"memory_{layout}"],
            _result_payload(copy=main_copy, bounds=bounds, revision=revision, label=f"memory {layout}"),
        )
    if wants_disk:
      recorders["disk"] = ResultRecorder(
          result_paths["disk"],
          _result_payload(copy=main_copy, bounds=bounds_current, revision=revision, label="approximate disk delay"),
      )
    if wants_cold:
      recorders["cold"] = ResultRecorder(
          result_paths["cold"],
          _result_payload(copy=main_copy, bounds=bounds_current, revision=revision, label="cold first tree"),
      )
    composition_copy: dict[str, Any] | None = None
    if wants_composition:
      composition_copy = build_session_copy(
          copy_root=scratch / "composition",
          source_sessions=LIVE_SESSIONS,
          selected_ids=composition_ids,
          event_byte_limit=event_limit,
      )
      composition_copy["copy_root"] = scratch / "composition"
      composition_payload = {
          "schema_version": 1,
          "label": "boot reads and first tree cache composition",
          "created_at": utc_now(),
          "repo_revision": revision,
          "copy": _copy_info(composition_copy, synthetic_count=0),
          "bounds":
              {
                  "user_slice": PRESSURE_SLICE,
                  "slice_memory_max_bytes": PARENT_MEMORY_MAX,
                  "slice_memory_swap_max_bytes": PARENT_SWAP_MAX,
                  "process_memory_max_bytes": COMPOSITION_MEMORY_MAX,
                  "process_memory_swap_max_bytes": 0,
                  "cpu_quota": "100%",
                  "memavailable_stop_bytes": MEMAVAILABLE_FLOOR,
                  "boot_reference_full_nodes": 612,
                  "boot_reference_total_nodes": 5924,
                  "boot_reference_full_fraction": 612 / 5924,
                  "boot_marker": args.boot_marker,
              },
          "stages": [],
          "guard_runs": [],
          "guard_freeze_intervals": [],
      }
      composition_recorder = ResultRecorder(result_paths["composition"], composition_payload)
    sample_count = args.sample_count
    pressure_cap_bytes = int(args.memory_pressure_gib * GIB)
    if args.case in ("all", "memory"):
      for layout in memory_layouts:
        recorder = recorders[f"memory_{layout}"]
        baseline = _run_server_stage(
            copy=main_copy,
            stage_name=f"memory-{layout}-baseline",
            results_dir=args.results_dir,
            access_key=synthetic_key,
            layout=layout,
            sample_count=sample_count,
            guard_callback=recorder.record_guard_run,
            run_repair_probes=True,
        )
        recorder.finish_checkpoint("baseline_no_pressure", {"metrics": baseline})
        stress = _run_server_stage(
            copy=main_copy,
            stage_name=f"memory-{layout}-pressure",
            results_dir=args.results_dir,
            access_key=synthetic_key,
            layout=layout,
            sample_count=sample_count,
            guard_callback=recorder.record_guard_run,
            pressure_cap_bytes=pressure_cap_bytes,
            run_repair_probes=True,
        )
        recorder.finish_checkpoint("memory_pressure", {"metrics": stress})
    if wants_disk or wants_cold:

      def save_concurrent_baseline_guard(snapshot: dict[str, Any]) -> None:
        if wants_disk:
          recorders["disk"].record_guard_run(snapshot)
        if wants_cold:
          recorders["cold"].record_guard_run(snapshot)

      concurrent_baseline = _run_server_stage(
          copy=main_copy,
          stage_name="disk-cold-concurrent-baseline",
          results_dir=args.results_dir,
          access_key=synthetic_key,
          layout="current",
          sample_count=sample_count,
          guard_callback=save_concurrent_baseline_guard,
          cold_concurrent=True,
      )
      if wants_disk:
        recorders["disk"].finish_checkpoint("baseline_no_delay", {"metrics": concurrent_baseline})
      if wants_cold:
        cold_alone = _run_server_stage(
            copy=main_copy,
            stage_name="cold-tree-alone-baseline",
            results_dir=args.results_dir,
            access_key=synthetic_key,
            layout="current",
            sample_count=sample_count,
            guard_callback=recorders["cold"].record_guard_run,
            cold_baseline=True,
        )
        recorders["cold"].finish_checkpoint("baseline_first_tree_alone", {"metrics": cold_alone})
        recorders["cold"].finish_checkpoint("first_tree_with_concurrent_endpoints", {"metrics": concurrent_baseline})
        reference = _run_server_stage(
            copy=main_copy,
            stage_name="cold-concurrent-without-tree-reference",
            results_dir=args.results_dir,
            access_key=synthetic_key,
            layout="current",
            sample_count=sample_count,
            guard_callback=recorders["cold"].record_guard_run,
            cold_concurrent=True,
            cold_without_tree=True,
        )
        recorders["cold"].finish_checkpoint("concurrent_endpoints_without_tree", {"metrics": reference})
        recorders["cold"].set_reference_verdicts(
            _judge_against_reference(concurrent_baseline["endpoints"], reference["endpoints"]))
      if wants_disk:
        disk = _run_server_stage(
            copy=main_copy,
            stage_name="disk-delay",
            results_dir=args.results_dir,
            access_key=synthetic_key,
            layout="current",
            sample_count=sample_count,
            guard_callback=recorders["disk"].record_guard_run,
            disk_delay_ms=args.disk_delay_ms,
            cold_concurrent=True,
        )
        recorders["disk"].finish_checkpoint("fixed_read_delay_approximate", {"metrics": disk})
    if composition_copy is not None and composition_recorder is not None:

      def save_composition_checkpoint(stage_name: str, checkpoint: dict[str, Any]) -> None:
        composition_recorder.finish_checkpoint(stage_name, {stage_name: checkpoint})

      composition = _run_composition(
          copy=composition_copy,
          results_dir=args.results_dir,
          access_key=access_key,
          synthetic_key=synthetic_key,
          boot_marker=args.boot_marker,
          stage_callback=save_composition_checkpoint,
          guard_callback=composition_recorder.record_guard_run,
      )
      composition_recorder.set_run_summary(composition)
    if log_window is None:
      raise AssertionError("live log window was not opened")
    all_recorders = list(recorders.values())
    if composition_recorder is not None:
      all_recorders.append(composition_recorder)
    live_latency = _parse_live_latency(
        log_window.read_new(),
        [guard for recorder in all_recorders for guard in recorder.payload["guard_runs"]],
    )
    for recorder in all_recorders:
      recorder.payload["live_server_latency_check"] = live_latency
      recorder._write()
    if not live_latency["passed"]:
      raise RuntimeError(f"live server latency check found an unguarded request over 10 seconds: {live_latency}")
  except Exception as exc:
    for recorder in recorders.values():
      recorder.fail(exc)
    if composition_recorder is not None:
      composition_recorder.fail(exc)
    raise
  finally:
    if log_window is not None:
      log_window.close()
    if scratch is not None and scratch.exists():
      shutil.rmtree(scratch)


def main(argv: list[str] | None = None) -> None:
  args = list(sys.argv[1:] if argv is None else argv)
  if args and args[0] == "_serve":
    _serve_main(args[1:])
    return
  if args and args[0] == "_pressure":
    _pressure_main(args[1:])
    return
  if args and args[0] == "_repair_probe":
    _repair_probe_main(args[1:])
    return
  if args and args[0] == "_composition":
    _composition_main(args[1:])
    return
  parsed = _parse_measurement_args(args)
  _run_public(parsed)


if __name__ == "__main__":
  main()
