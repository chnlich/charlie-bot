"""The run scratch trees under /tmp: the owner record, the keep marker and the start sweep.

Each run creates one ``/tmp/charliebot-parity-*`` tree (parity.py keeps the servers on homes under /tmp,
whatever TMPDIR says) and records its owning process in it. At start, a run removes every scratch tree of
our user whose owning process is gone and that holds no keep marker; a tree from before this change has no
record, and the sweep removes it when it is older than six hours. The port lock directory holds flocks of
live runs and no owner record, so the sweep never touches it.
"""

import json
import os
import pathlib
import shutil
import time

SCRATCH_PREFIX = "charliebot-parity-"
SCRATCH_ROOT = pathlib.Path("/tmp")
# An empty file in a kept tree; the sweep leaves every tree that holds it.
KEEP_MARKER = "keep"
# The owning process of a tree: its pid and its /proc start time, so a recycled pid still reads as gone.
OWNER_RECORD = "owner.json"
# The scratch root's map from each run directory's short name to its scenario, direction and tree paths.
RUNS_MANIFEST = "runs.json"
# A tree without an owner record is from before this change; the sweep removes it once it is this old.
UNRECORDED_MAX_AGE_SECONDS = 6 * 60 * 60
PORT_LOCK_DIR = SCRATCH_ROOT / (SCRATCH_PREFIX + "ports")


def process_start_time(pid: int) -> int | None:
  """The process's /proc start time in clock ticks, or None when no such process exists."""
  try:
    stat_text = pathlib.Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
  except (FileNotFoundError, ProcessLookupError, PermissionError):
    return None
  # The comm field may hold spaces and parentheses; the numeric fields start after its closing paren.
  return int(stat_text.rsplit(")", 1)[1].split()[19])


def record_owner(scratch: pathlib.Path) -> None:
  """Write this process's pid and /proc start time into the tree."""
  pid = os.getpid()
  start_time = process_start_time(pid)
  if start_time is None:
    raise RuntimeError(f"cannot read the /proc start time of pid {pid}")
  record = {"pid": pid, "start_time": start_time}
  # A temp name and a rename: a crash mid-write leaves no partial file for the sweep to trip on.
  temp = scratch / (OWNER_RECORD + ".tmp")
  temp.write_text(json.dumps(record) + "\n", encoding="utf-8")
  os.replace(temp, scratch / OWNER_RECORD)


def sweep(root: pathlib.Path = SCRATCH_ROOT, now: float | None = None) -> None:
  """Remove every scratch tree of our user that no live process owns and that no keep marker saves."""
  now = time.time() if now is None else now
  for path in sorted(root.glob(SCRATCH_PREFIX + "*")):
    if path.name == PORT_LOCK_DIR.name or not path.is_dir():
      continue
    try:
      info = path.stat()
      if info.st_uid != os.getuid():
        continue
      if (path / KEEP_MARKER).exists():
        continue
      owner = _read_owner(path, path)
      if owner is not None:
        if process_start_time(owner["pid"]) == owner["start_time"]:
          continue
      elif now - info.st_mtime < UNRECORDED_MAX_AGE_SECONDS:
        continue
      shutil.rmtree(path)
    except FileNotFoundError:
      # Two runs can start together and both sweep the same stale tree; the one that comes second finds it gone.
      continue


def _read_owner(scratch: pathlib.Path, tree: pathlib.Path) -> dict | None:
  """The tree's owner record, or None when it has none; a record that cannot parse stops the sweep."""
  try:
    record_text = (scratch / OWNER_RECORD).read_text(encoding="utf-8")
  except FileNotFoundError:
    return None
  try:
    record = json.loads(record_text)
    pid = record["pid"]
    start_time = record["start_time"]
  except (json.JSONDecodeError, KeyError, TypeError):
    raise RuntimeError(
        f"parity: scratch tree {tree} holds an owner record that cannot be parsed; "
        "check that no run is writing it, then delete the tree") from None
  if not isinstance(pid, int) or not isinstance(start_time, int):
    raise RuntimeError(
        f"parity: scratch tree {tree} holds an owner record whose pid or start time is not an integer; "
        "check that no run is writing it, then delete the tree")
  return {"pid": pid, "start_time": start_time}


def finalize(scratch: pathlib.Path, keep: bool, clean: bool, runs: list[dict]) -> None:
  """End a run: keep the tree only when --keep says so, and point a failed run's reader at --keep."""
  if keep:
    (scratch / KEEP_MARKER).touch()
    (scratch / RUNS_MANIFEST).write_text(json.dumps(runs, indent=2) + "\n", encoding="utf-8")
    print(f"\ntrees and logs kept in {scratch}; {RUNS_MANIFEST} maps each run to its scenario and trees")
    return
  shutil.rmtree(scratch)
  if not clean:
    print("\nthe trees are gone; rerun with --keep to inspect them")
