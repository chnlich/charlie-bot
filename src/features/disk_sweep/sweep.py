"""Select old task worktrees and scratch directories for removal.

Worktree age begins at the latest close among every task that owns a recorded
worktree path. Scratch age uses the newest mtime anywhere under one top-level
directory. A worktree archive contains tracked.patch and each unignored
untracked file under untracked/.
"""

import collections
import dataclasses
import datetime
import errno
import io
import json
import os
import pathlib
import shutil
import stat
import subprocess
import tarfile
import tempfile
from collections.abc import Iterable

from src.infra import config, event_types, git, log_once, models
from src.runtime import chat_events, run_identity, runs

log = log_once.LazyStructlogLogger()

# Delivered worktrees usually disappear immediately; three days retain other closed work briefly.
CLOSED_WORKTREE_DAYS = 3
# A manually retained worktree stays available for two weeks after task closure.
KEPT_WORKTREE_DAYS = 14
# Scratch directories have no task owner, so two quiet weeks provide the retention window.
SCRATCH_DAYS = 14
# Daily profile backups should not absorb an untracked file larger than 10 MiB.
MAX_UNTRACKED_BYTES = 10 * 1024 * 1024

_LIFECYCLE_TYPES = frozenset({event_types.CLONE_START, event_types.TASK_CLOSED, event_types.TASK_REOPENED})
_GIT_TIMEOUT_SECONDS = 60
_PROC_GONE_ERRNOS = frozenset({errno.ENOENT, errno.ESRCH})


@dataclasses.dataclass
class SweepRow:
  """One candidate's TSV result."""

  kind: str
  path: pathlib.Path
  freed_bytes: int = 0
  action: str = "skip"
  reason: str = ""
  repo_path: str | None = None
  branch_name: str | None = None
  owner_run: models.RunRecord | None = None
  owner_session_id: str | None = None
  archive_path: pathlib.Path | None = None

  def to_tsv(self) -> str:
    """Return the row in the command's tab-separated output format."""
    fields = (self.kind, str(self.path), str(self.freed_bytes), self.action, self.reason)
    return "\t".join(_tsv_field(field) for field in fields)

  def skip(self, reason: str) -> None:
    """Keep the row out of the sweep, recording *reason* and freeing no bytes.

    A row can hold bytes counted when it was selected for deletion and then be
    skipped by a later preflight, so a skip clears them; the report's total
    counts only deleted rows.
    """
    self.action = "skip"
    self.reason = reason
    self.freed_bytes = 0


@dataclasses.dataclass(frozen=True)
class SweepFailure:
  """An archive or deletion failure surfaced by an applying sweep."""

  path: pathlib.Path
  reason: str


@dataclasses.dataclass
class SweepReport:
  """Rows and totals for one sweep."""

  rows: list[SweepRow]
  deleted_count: int
  freed_bytes: int
  skipped_count: int
  unreadable_proc_count: int
  failures: list[SweepFailure]

  def total_line(self) -> str:
    """Return the final TSV totals line."""
    return (f"total\t{self.deleted_count}\t{self.freed_bytes}\t{self.skipped_count}\t{self.unreadable_proc_count}")


def _tsv_field(value: str) -> str:
  """Keep one output value on one tab-separated line."""
  return value.replace("\t", " ").replace("\r", " ").replace("\n", " ")


def _run_git(repo_path: pathlib.Path, *args: str, input_bytes: bytes | None = None) -> bytes:
  """Run a local git command without allowing status to refresh its index."""
  env = os.environ.copy()
  env["GIT_OPTIONAL_LOCKS"] = "0"
  proc = subprocess.run(
      ["git", *args],
      cwd=repo_path,
      env=env,
      input=input_bytes,
      stdout=subprocess.PIPE,
      stderr=subprocess.PIPE,
      timeout=_GIT_TIMEOUT_SECONDS,
      check=False)
  if proc.returncode != 0:
    stderr = proc.stderr.decode(errors="replace").strip()
    raise RuntimeError(f"git {' '.join(args)} failed in {repo_path}: {stderr}")
  return proc.stdout


def _read_lifecycle(session_dir: pathlib.Path) -> dict | None:
  """Read the latest lifecycle event across archived and live chat events."""
  event_paths = sorted(chat_events.chat_event_archives_dir(session_dir).glob(chat_events.ARCHIVE_FILE_GLOB))
  live_path = chat_events.chat_events_path(session_dir)
  if live_path.is_file():
    event_paths.append(live_path)
  latest = None
  for event_path in event_paths:
    with event_path.open(encoding="utf-8") as stream:
      for line_number, line in enumerate(stream, start=1):
        if not line.strip():
          continue
        try:
          event = json.loads(line)
        except json.JSONDecodeError as error:
          raise RuntimeError(f"invalid chat event at {event_path}:{line_number}: {error}") from error
        if event.get("type") in _LIFECYCLE_TYPES:
          latest = event
  return latest


def _task_keep_worktree(cfg: config.CharlieBotConfig, session_id: str) -> bool | None:
  """Return a task's keep-worktree setting, or None when its owner is unknown."""
  metadata_path = cfg.sessions_dir / session_id / run_identity.SESSION_METADATA_NAME
  if not metadata_path.is_file():
    return None
  try:
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
  except json.JSONDecodeError as error:
    raise RuntimeError(f"invalid session metadata at {metadata_path}: {error}") from error
  task_data = metadata.get("task")
  if not isinstance(task_data, dict):
    return None
  task = models.TaskSpec.model_validate(task_data)
  return task.keep_worktree


def _load_run_groups(cfg: config.CharlieBotConfig) -> dict[pathlib.Path, dict[str, list[models.RunRecord]]]:
  """Group recorded worktree Runs by canonical path and session owner."""
  groups: dict[pathlib.Path,
               dict[str, list[models.RunRecord]]] = collections.defaultdict(lambda: collections.defaultdict(list))
  for metadata_path in sorted(
      cfg.sessions_dir.glob(f"*/{runs.DATA_DIR_NAME}/{runs.RUNS_DIR_NAME}/*/{runs.RUN_METADATA_NAME}")):
    try:
      record = models.RunRecord.model_validate_json(metadata_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
      raise RuntimeError(f"unreadable run metadata at {metadata_path}: {error}") from error
    if record.worktree_path is None:
      continue
    recorded_session_id = metadata_path.parents[3].name
    recorded_run_id = metadata_path.parent.name
    if record.session_id != recorded_session_id or record.id != recorded_run_id:
      raise RuntimeError(
          f"run metadata at {metadata_path} names session/run {record.session_id!r}/{record.id!r}, "
          f"but its directory is {recorded_session_id!r}/{recorded_run_id!r}")
    path = pathlib.Path(record.worktree_path).expanduser().absolute()
    groups[path][record.session_id].append(record)
  return groups


def _latest_closed_at(event: dict | None) -> datetime.datetime | None:
  """Return the timestamp when the latest lifecycle fact is a task close."""
  if event is None or event.get("type") != event_types.TASK_CLOSED:
    return None
  timestamp = event.get("timestamp")
  if not isinstance(timestamp, str):
    raise ValueError("task_closed event has no timestamp")
  closed_at = datetime.datetime.fromisoformat(timestamp.replace("Z", "+00:00"))
  if closed_at.tzinfo is None:
    raise ValueError(f"task_closed timestamp has no timezone: {timestamp}")
  return closed_at.astimezone(datetime.UTC)


def _choose_owner_run(owner_groups: dict[str, list[models.RunRecord]]) -> tuple[str, models.RunRecord] | None:
  """Choose the earliest complete Run as the worktree's archive and removal owner."""
  complete_runs: list[models.RunRecord] = []
  for _session_id, records in sorted(owner_groups.items()):
    complete_runs.extend(record for record in records if record.repo_path and record.branch_name)
  if not complete_runs:
    return None
  complete_runs.sort(
      key=lambda record: (record.started_at or datetime.datetime.min.replace(tzinfo=datetime.UTC), record.id))
  record = complete_runs[0]
  return record.session_id, record


def _worktree_rows(cfg: config.CharlieBotConfig, now: datetime.datetime) -> list[SweepRow]:
  """Select old closed-task worktrees and retain open or unknown owners as skipped rows."""
  worktree_root = pathlib.Path(cfg.paths.worktree_dir).absolute()
  rows = []
  for path, owner_groups in sorted(_load_run_groups(cfg).items(), key=lambda item: str(item[0])):
    if not path.is_dir():
      continue
    row = SweepRow(kind="worktree", path=path)
    if path.is_symlink():
      row.reason = "worktree-symlink"
      rows.append(row)
      continue
    if not path.is_relative_to(worktree_root):
      row.reason = "outside-worktree-root"
      rows.append(row)
      continue
    owner_closes: list[datetime.datetime] = []
    any_keep = False
    owner_missing = False
    owner_open = False
    for session_id in sorted(owner_groups):
      keep = _task_keep_worktree(cfg, session_id)
      if keep is None:
        owner_missing = True
        continue
      any_keep = any_keep or keep
      closed_at = _latest_closed_at(_read_lifecycle(cfg.sessions_dir / session_id))
      if closed_at is None:
        owner_open = True
      else:
        owner_closes.append(closed_at)
    if owner_missing:
      row.reason = "unknown-owner"
      rows.append(row)
      continue
    if owner_open:
      row.reason = "task-open"
      rows.append(row)
      continue
    if len(owner_closes) != len(owner_groups):
      row.reason = "unknown-owner"
      rows.append(row)
      continue
    wait_days = KEPT_WORKTREE_DAYS if any_keep else CLOSED_WORKTREE_DAYS
    if now - max(owner_closes) < datetime.timedelta(days=wait_days):
      continue
    selected_owner = _choose_owner_run(owner_groups)
    if selected_owner is None:
      row.reason = "unknown-owner"
      rows.append(row)
      continue
    owner_session_id, run = selected_owner
    row.owner_run = run
    row.owner_session_id = owner_session_id
    row.repo_path = run.repo_path
    row.branch_name = run.branch_name
    row.archive_path = cfg.sessions_dir / owner_session_id / runs.DATA_DIR_NAME / runs.RUNS_DIR_NAME / run.id / (
        "worktree-changes.tar")
    row.action = "delete"
    row.reason = "closed-task"
    rows.append(row)
  return rows


def _raise_walk_error(error: OSError) -> None:
  """Stop a tree walk when an entry cannot be inspected."""
  raise error


def _tree_latest_mtime(path: pathlib.Path) -> float:
  """Return the newest lstat mtime in a directory tree without following symlinks."""
  latest = path.lstat().st_mtime
  for root, directories, filenames in os.walk(path, followlinks=False, onerror=_raise_walk_error):
    for name in [*directories, *filenames]:
      entry = pathlib.Path(root) / name
      latest = max(latest, entry.lstat().st_mtime)
  return latest


def _nested_repositories(path: pathlib.Path) -> list[pathlib.Path]:
  """Find git repositories within one scratch candidate, pruning their metadata dirs."""
  repositories = []
  for root, directories, filenames in os.walk(path, followlinks=False, onerror=_raise_walk_error):
    if ".git" in directories or ".git" in filenames:
      repositories.append(pathlib.Path(root))
      directories[:] = [name for name in directories if name != ".git"]
    directories[:] = [name for name in directories if not (pathlib.Path(root) / name).is_symlink()]
  return repositories


def _repo_has_unsaved_changes(repo_path: pathlib.Path) -> bool:
  """Return whether a nested repository has edits or commits outside remote refs."""
  status = _run_git(repo_path, "status", "--porcelain")
  if status:
    return True
  unpushed = _run_git(repo_path, "rev-list", "--all", "--not", "--remotes")
  return bool(unpushed)


def _scratch_rows(cfg: config.CharlieBotConfig, now: datetime.datetime) -> list[SweepRow]:
  """Select old top-level scratch directories and skip any containing unsaved repositories."""
  scratch_root = pathlib.Path(cfg.paths.scratch_dir)
  if not scratch_root.is_dir():
    return []
  rows = []
  cutoff = now.timestamp() - SCRATCH_DAYS * 24 * 60 * 60
  for path in sorted(scratch_root.iterdir()):
    if not path.is_dir():
      continue
    row = SweepRow(kind="scratch", path=path.absolute())
    if path.is_symlink():
      row.reason = "scratch-symlink"
      rows.append(row)
      continue
    try:
      latest_mtime = _tree_latest_mtime(path)
    except OSError as error:
      row.reason = f"unreadable-path: {error}"
      rows.append(row)
      continue
    if latest_mtime >= cutoff:
      continue
    if any(_repo_has_unsaved_changes(repo_path) for repo_path in _nested_repositories(path)):
      row.reason = "has-unsaved-repo"
      rows.append(row)
      continue
    row.action = "delete"
    row.reason = "old-scratch"
    rows.append(row)
  return rows


def _read_link(path: pathlib.Path) -> pathlib.Path | None:
  """Resolve a process symlink target when it names a filesystem path."""
  try:
    target = os.readlink(path)
  except OSError as error:
    if error.errno in _PROC_GONE_ERRNOS:
      return None
    raise
  target = target.removesuffix(" (deleted)")
  target_path = pathlib.Path(target)
  if not target_path.is_absolute():
    return None
  return target_path.resolve()


def _maps_paths(path: pathlib.Path) -> list[pathlib.Path]:
  """Read absolute file paths from a process maps file."""
  result = []
  try:
    with path.open(encoding="utf-8", errors="replace") as stream:
      for line in stream:
        fields = line.rstrip("\n").split(maxsplit=5)
        if len(fields) != 6:
          continue
        target = fields[5].removesuffix(" (deleted)")
        target_path = pathlib.Path(target)
        if target_path.is_absolute():
          result.append(target_path.resolve())
  except OSError as error:
    if error.errno not in _PROC_GONE_ERRNOS:
      raise
  return result


def _cmdline_paths(cmdline: bytes, cwd: pathlib.Path | None) -> list[pathlib.Path]:
  """Resolve absolute and cwd-relative command-line arguments that name paths."""
  result = []
  for raw_argument in cmdline.split(b"\0"):
    if not raw_argument:
      continue
    argument = os.fsdecode(raw_argument)
    values = [argument]
    if "=" in argument:
      values.append(argument.split("=", 1)[1])
    for value in values:
      value = value.removesuffix(" (deleted)")
      path = pathlib.Path(value).expanduser()
      if path.is_absolute():
        result.append(path.resolve())
      elif cwd is not None:
        result.append((cwd / path).resolve())
  return result


def _process_paths(proc_root: pathlib.Path) -> tuple[list[tuple[int, list[pathlib.Path]]], set[int]]:
  """Read cwd, root, exe, fd, maps and cmdline paths for each readable user process."""
  own_uid = os.getuid()
  processes: list[tuple[int, list[pathlib.Path]]] = []
  unreadable: set[int] = set()
  for proc_dir in sorted(proc_root.iterdir(), key=lambda entry: entry.name):
    if not proc_dir.name.isdecimal():
      continue
    pid = int(proc_dir.name)
    try:
      if proc_dir.stat().st_uid != own_uid:
        unreadable.add(pid)
        continue
    except OSError as error:
      if error.errno in _PROC_GONE_ERRNOS:
        continue
      unreadable.add(pid)
      continue
    paths: list[pathlib.Path] = []
    cwd = None
    process_unreadable = False
    for link_name in ("cwd", "root", "exe"):
      try:
        target = _read_link(proc_dir / link_name)
      except OSError:
        process_unreadable = True
        continue
      if target is not None:
        paths.append(target)
        if link_name == "cwd":
          cwd = target
    fd_dir = proc_dir / "fd"
    try:
      fd_entries = sorted(fd_dir.iterdir(), key=lambda entry: entry.name)
    except OSError as error:
      if error.errno not in _PROC_GONE_ERRNOS:
        process_unreadable = True
      fd_entries = []
    for fd_entry in fd_entries:
      try:
        target = _read_link(fd_entry)
      except OSError:
        process_unreadable = True
        continue
      if target is not None:
        paths.append(target)
    try:
      paths.extend(_maps_paths(proc_dir / "maps"))
    except OSError:
      process_unreadable = True
    try:
      cmdline = (proc_dir / "cmdline").read_bytes()
    except OSError as error:
      if error.errno not in _PROC_GONE_ERRNOS:
        process_unreadable = True
      cmdline = b""
    paths.extend(_cmdline_paths(cmdline, cwd))
    if process_unreadable:
      unreadable.add(pid)
    processes.append((pid, paths))
  return processes, unreadable


def _is_within(path: pathlib.Path, root: pathlib.Path) -> bool:
  """Whether a resolved path is the root or one of its descendants."""
  return path == root or path.is_relative_to(root)


def _in_use_paths(
    candidates: Iterable[SweepRow],
    processes: list[tuple[int, list[pathlib.Path]]],
) -> set[pathlib.Path]:
  """Return candidate paths referenced by any process path."""
  used = set()
  for candidate in candidates:
    if candidate.action == "skip":
      continue
    candidate_path = candidate.path.resolve()
    if any(_is_within(proc_path, candidate_path) for _pid, paths in processes for proc_path in paths):
      used.add(candidate.path)
  return used


def _git_status(repo_path: pathlib.Path) -> bytes:
  """Read the complete worktree status, including submodule contents."""
  return _run_git(repo_path, "status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignore-submodules=none")


def _submodule_paths(repo_path: pathlib.Path) -> set[str]:
  """Return paths recorded as gitlinks in the repository index."""
  entries = _run_git(repo_path, "ls-files", "--stage", "-z").split(b"\0")
  result = set()
  for entry in entries:
    if not entry:
      continue
    metadata, path = entry.split(b"\t", 1)
    mode = metadata.split(b" ", 1)[0]
    if mode == b"160000":
      result.add(os.fsdecode(path))
  return result


def _has_dirty_submodule(repo_path: pathlib.Path, status: bytes) -> bool:
  """Detect modified gitlinks and changes inside initialized submodules."""
  submodule_paths = _submodule_paths(repo_path)
  if submodule_paths:
    for entry in status.split(b"\0"):
      if len(entry) < 4:
        continue
      status_code = entry[:2]
      changed_path = os.fsdecode(entry[3:])
      if changed_path in submodule_paths and status_code != b"  ":
        return True
    submodule_status = _run_git(repo_path, "submodule", "status", "--recursive").decode(errors="replace")
    if any(line.startswith(("+", "U")) or "-dirty" in line for line in submodule_status.splitlines()):
      return True
  return False


def _expanded_untracked_paths(repo_path: pathlib.Path) -> list[pathlib.Path]:
  """Expand untracked directories to their non-ignored files, excluding nested git metadata."""
  listed = _run_git(repo_path, "ls-files", "--others", "--exclude-standard", "-z")
  candidates: set[pathlib.Path] = set()
  for raw_path in listed.split(b"\0"):
    if not raw_path:
      continue
    path = repo_path / pathlib.Path(os.fsdecode(raw_path))
    mode = path.lstat().st_mode
    if stat.S_ISDIR(mode) and not stat.S_ISLNK(mode):
      for root, directories, filenames in os.walk(path, followlinks=False, onerror=_raise_walk_error):
        directories[:] = [name for name in directories if name != ".git"]
        for name in [*directories, *filenames]:
          entry = pathlib.Path(root) / name
          if entry.name != ".git" and (entry.is_symlink() or not entry.is_dir()):
            candidates.add(entry)
    else:
      candidates.add(path)
  if not candidates:
    return []
  relative_paths = sorted(path.relative_to(repo_path) for path in candidates)
  input_bytes = b"\0".join(os.fsencode(path.as_posix()) for path in relative_paths) + b"\0"
  env = os.environ.copy()
  env["GIT_OPTIONAL_LOCKS"] = "0"
  proc = subprocess.run(
      ["git", "check-ignore", "--no-index", "-z", "--stdin"],
      cwd=repo_path,
      env=env,
      input=input_bytes,
      stdout=subprocess.PIPE,
      stderr=subprocess.PIPE,
      timeout=_GIT_TIMEOUT_SECONDS,
      check=False)
  if proc.returncode not in (0, 1):
    stderr = proc.stderr.decode(errors="replace").strip()
    raise RuntimeError(f"git check-ignore failed in {repo_path}: {stderr}")
  ignored = {pathlib.Path(os.fsdecode(raw)) for raw in proc.stdout.split(b"\0") if raw}
  return [repo_path / path for path in relative_paths if path not in ignored]


def _inspect_worktree(row: SweepRow) -> tuple[bytes, list[pathlib.Path], str | None]:
  """Return tracked status, archiveable untracked files and a skip reason."""
  status = _git_status(row.path)
  if _has_dirty_submodule(row.path, status):
    return status, [], "dirty-submodule"
  untracked = _expanded_untracked_paths(row.path)
  for path in untracked:
    if path.lstat().st_size > MAX_UNTRACKED_BYTES:
      return status, untracked, "large-untracked"
  return status, untracked, None


def _worktree_preflight(row: SweepRow) -> None:
  """Set the worktree action after checking submodules and large untracked files."""
  status, _untracked, skip_reason = _inspect_worktree(row)
  if skip_reason is not None:
    row.skip(skip_reason)
    return
  if status:
    row.action = "archive+delete"
    row.reason = "uncommitted-changes"
  else:
    row.action = "delete"
    row.reason = "closed-task"


def _scratch_preflight(row: SweepRow, now: datetime.datetime) -> None:
  """Recheck scratch age and nested repository state immediately before deletion."""
  if _tree_latest_mtime(row.path) >= now.timestamp() - SCRATCH_DAYS * 24 * 60 * 60:
    row.skip("too-young")
    return
  if any(_repo_has_unsaved_changes(repo_path) for repo_path in _nested_repositories(row.path)):
    row.skip("has-unsaved-repo")


def _archive_worktree(row: SweepRow, untracked: list[pathlib.Path]) -> pathlib.Path:
  """Write a run-scoped tar with the tracked patch and all unignored untracked files."""
  if row.archive_path is None:
    raise RuntimeError(f"worktree {row.path} has no owning run directory")
  archive_path = row.archive_path
  if not archive_path.parent.is_dir():
    raise RuntimeError(f"run directory is missing for archive: {archive_path.parent}")
  patch = _run_git(row.path, "diff", "--binary", "HEAD")
  temp_fd, temp_name = tempfile.mkstemp(prefix=".worktree-changes-", suffix=".tar", dir=archive_path.parent)
  os.close(temp_fd)
  temp_path = pathlib.Path(temp_name)
  try:
    with tarfile.open(temp_path, mode="w") as archive:
      patch_info = tarfile.TarInfo("tracked.patch")
      patch_info.size = len(patch)
      patch_info.mtime = datetime.datetime.now(datetime.UTC).timestamp()
      archive.addfile(patch_info, io.BytesIO(patch))
      for path in untracked:
        relative = path.relative_to(row.path)
        archive.add(path, arcname=(pathlib.Path("untracked") / relative).as_posix(), recursive=False)
    os.replace(temp_path, archive_path)
  finally:
    temp_path.unlink(missing_ok=True)
  return archive_path


def _inode_blocks(paths: Iterable[pathlib.Path]) -> dict[pathlib.Path, int]:
  """Attribute each fully removed inode's blocks to the first selected directory containing it."""
  inodes: dict[tuple[int, int], list[int | pathlib.Path]] = {}
  for path in paths:
    for root, _directories, _filenames in os.walk(path, followlinks=False, onerror=_raise_walk_error):
      for name in os.listdir(root):
        entry = pathlib.Path(root) / name
        result = entry.lstat()
        key = (result.st_dev, result.st_ino)
        record = inodes.get(key)
        if record is None:
          inodes[key] = [result.st_nlink, result.st_blocks * 512, 1, path]
        else:
          record[2] = int(record[2]) + 1
    # os.walk does not count the candidate root itself, matching reclaim.py's accounting.
  freed: dict[pathlib.Path, int] = collections.defaultdict(int)
  for link_count, blocks, seen_count, first_path in inodes.values():
    if int(seen_count) >= int(link_count) or int(link_count) == 0:
      freed[pathlib.Path(first_path)] += int(blocks)
  return dict(freed)


def _mark_in_use(rows: list[SweepRow], proc_root: pathlib.Path) -> set[int]:
  """Mark eligible rows in use and return the unique unreadable process ids."""
  processes, unreadable = _process_paths(proc_root)
  for path in _in_use_paths(rows, processes):
    row = next(row for row in rows if row.path == path)
    row.skip("in-use")
  return unreadable


def _process_uses_path(path: pathlib.Path, proc_root: pathlib.Path) -> tuple[bool, set[int]]:
  """Recheck one candidate's in-use state immediately before its deletion."""
  processes, unreadable = _process_paths(proc_root)
  used = _in_use_paths([SweepRow(kind="candidate", path=path, action="delete")], processes)
  return path in used, unreadable


def _fresh_worktree_action(row: SweepRow) -> tuple[bytes, list[pathlib.Path]]:
  """Refresh worktree status and action before archive or removal."""
  status, untracked, skip_reason = _inspect_worktree(row)
  if skip_reason is not None:
    row.skip(skip_reason)
    return status, untracked
  if status:
    row.action = "archive+delete"
    row.reason = "uncommitted-changes"
  else:
    row.action = "delete"
    row.reason = "closed-task"
  return status, untracked


async def _remove_worktree(row: SweepRow, cfg: config.CharlieBotConfig) -> str | None:
  """Remove one worktree through the shared reporting helper."""
  if row.owner_run is None:
    raise RuntimeError(f"worktree {row.path} has no owner run")
  return await git.git_worktree_remove_reporting(
      row.repo_path,
      str(row.path),
      row.branch_name,
      row.owner_run.id,
      pathlib.Path(cfg.paths.worktree_dir),
      log_fields={
          "run_id": row.owner_run.id,
          "session": row.owner_session_id
      },
      label="Disk sweep worktree",
      fail_event="disk_sweep_remove_failed",
      remove_failed_event="disk_sweep_remove_failed")


async def _apply_rows(
    rows: list[SweepRow],
    cfg: config.CharlieBotConfig,
    proc_root: pathlib.Path,
    now: datetime.datetime,
) -> tuple[int, int, list[SweepFailure], set[int]]:
  """Archive and delete eligible rows, returning actual counts and failures."""
  failures = []
  unreadable: set[int] = set()
  deleted_count = 0
  freed_bytes = 0
  for row in rows:
    if row.action == "skip":
      continue
    used, unreadable_now = _process_uses_path(row.path, proc_root)
    unreadable.update(unreadable_now)
    if used:
      row.skip("in-use")
      continue
    try:
      if row.kind == "worktree":
        status, untracked = _fresh_worktree_action(row)
        if row.action == "skip":
          continue
        archive_path = None
        if status:
          archive_path = _archive_worktree(row, untracked)
          used_after_archive, unreadable_now = _process_uses_path(row.path, proc_root)
          unreadable.update(unreadable_now)
          if used_after_archive:
            row.skip("in-use")
            continue
        remove_error = await _remove_worktree(row, cfg)
        if remove_error is not None:
          raise RuntimeError(remove_error)
      else:
        _scratch_preflight(row, now)
        if row.action == "skip":
          continue
        used, unreadable_now = _process_uses_path(row.path, proc_root)
        unreadable.update(unreadable_now)
        if used:
          row.skip("in-use")
          continue
        archive_path = None
        shutil.rmtree(row.path)
      deleted_count += 1
      log.info(
          "disk_sweep_removed",
          kind=row.kind,
          path=str(row.path),
          freed_bytes=row.freed_bytes,
          archive=str(archive_path) if archive_path is not None else None)
      freed_bytes += row.freed_bytes
    except Exception as error:
      row.skip("apply-failed")
      failures.append(SweepFailure(path=row.path, reason=str(error)))
  return deleted_count, freed_bytes, failures, unreadable


async def run_sweep(
    cfg: config.CharlieBotConfig,
    *,
    apply: bool,
    now: datetime.datetime | None = None,
    proc_root: pathlib.Path = pathlib.Path("/proc"),
) -> SweepReport:
  """Run a read-only listing or an archive-and-delete sweep."""
  if now is None:
    now = datetime.datetime.now(datetime.UTC)
  elif now.tzinfo is None:
    raise ValueError("disk-sweep time must be timezone-aware")
  now = now.astimezone(datetime.UTC)
  rows = _worktree_rows(cfg, now) + _scratch_rows(cfg, now)
  worktree_root = pathlib.Path(cfg.paths.worktree_dir).absolute()
  scratch_root = pathlib.Path(cfg.paths.scratch_dir).resolve()
  for row in rows:
    if row.action != "skip":
      if row.kind == "worktree" and row.path == worktree_root:
        row.skip("unsafe-root")
      elif row.kind == "scratch" and row.path == scratch_root:
        row.skip("unsafe-root")
  unreadable = _mark_in_use(rows, proc_root)
  for row in rows:
    if row.action == "skip" or row.reason == "in-use":
      continue
    if row.kind == "worktree":
      _worktree_preflight(row)
  eligible_paths = [row.path for row in rows if row.action != "skip"]
  freed_by_path = _inode_blocks(eligible_paths)
  for row in rows:
    if row.action != "skip":
      row.freed_bytes = freed_by_path.get(row.path, 0)
  if apply:
    deleted_count, freed_bytes, failures, unreadable_after = await _apply_rows(rows, cfg, proc_root, now)
    unreadable.update(unreadable_after)
    skipped_count = sum(row.action == "skip" for row in rows)
  else:
    deleted_count = sum(row.action != "skip" for row in rows)
    freed_bytes = sum(row.freed_bytes for row in rows if row.action != "skip")
    failures = []
    skipped_count = sum(row.action == "skip" for row in rows)
  return SweepReport(
      rows=rows,
      deleted_count=deleted_count,
      freed_bytes=freed_bytes,
      skipped_count=skipped_count,
      unreadable_proc_count=len(unreadable),
      failures=failures)


async def run_scheduled() -> str:
  """Run the scheduled handler in apply mode and return its totals line."""
  report = await run_sweep(config.get_config(), apply=True)
  if report.failures:
    failures = "; ".join(f"{failure.path}: {failure.reason}" for failure in report.failures)
    raise RuntimeError(f"disk_sweep failed: {failures}")
  return report.total_line()
