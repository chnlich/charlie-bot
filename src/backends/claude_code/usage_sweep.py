"""Claude Code's part of the cold-storage sweep (src/features/usage/storage_cool.py): transcript directories.

``usage_logs.sweep()`` imports this module on its first call, so a usage capture and
``charliebot --help`` never load it. The sweep's one deletion rule is stated on ``SweepScope``
(src/runtime/hooks/usage_sources.py).

A transcript directory belongs to CharlieBot when the cwd encoding the CLI applies (separators,
dots and underscores become hyphens) leaves a session id in its name. Any other name encodes a
cwd that CharlieBot never handed a claude process, and the sweep never touches it.
"""

import os
import pathlib
import shutil

from src.backends.claude_code import login_dirs
from src.infra import config, log_once
from src.runtime import worktree_trash
from src.runtime.hooks import usage_sources

log = log_once.LazyStructlogLogger()

# Claude Code derives the transcript directory from the process cwd; every cwd
# CharlieBot hands a claude process is the session directory itself or a
# directory inside it, so the encoded name carries the session id after the
# encoded "sessions" path segment.
_SESSIONS_SEGMENT = "-sessions-"


def claude_project_dir_name(cwd: pathlib.Path) -> str:
  """The transcript directory name Claude Code derives from a process cwd.

  Path separators, dots and underscores each become a hyphen; every other
  character passes through.
  """
  return str(cwd).replace("/", "-").replace(".", "-").replace("_", "-")


def claude_projects_roots(cfg: config.CharlieBotConfig) -> list[pathlib.Path]:
  """Every ``projects`` tree a cc-claude backend may have written transcripts into.

  A transcript tree outlives the environment that created it, so the search set
  must not depend on the current one: the default home is seeded beside the
  ``claude_config_dir()`` answer for the environment and the configured pool
  accounts, mirroring how the codex sweep seeds its store.  Widening the
  searched trees does not widen what is deletable: a name that encodes neither a
  CharlieBot session id nor the worktree prefix stays untouched no matter which
  tree it sits in.
  """
  homes = {login_dirs.default_claude_dir(), login_dirs.claude_config_dir()}
  for account in cfg.accounts.claude:
    homes.add(pathlib.Path(account.config_dir).expanduser())
  return sorted(home / "projects" for home in homes)


def _encoded_session_id(dir_name: str) -> str | None:
  """The CharlieBot session id a transcript directory name encodes, or None.

  The encoded cwd of anything inside a session directory reads
  ``<prefix>-sessions-<session id>[-<more>]``, so the name is CharlieBot's own
  exactly when the tail after the last ``-sessions-`` starts with a full session
  id and either ends there or continues with another path segment.
  """
  start = dir_name.rfind(_SESSIONS_SEGMENT)
  if start < 0:
    return None
  tail = dir_name[start + len(_SESSIONS_SEGMENT):]
  match = usage_sources.SESSION_ID_RE.match(tail)
  if match is None:
    return None
  rest = tail[match.end():]
  if rest and not rest.startswith("-"):
    return None
  return match.group(0)


def _newest_mtime(path: pathlib.Path) -> float | None:
  """Newest file mtime under *path*, or the directory's own when it holds none."""
  newest: float | None = None
  for dirpath, _, filenames in os.walk(path):
    for filename in filenames:
      try:
        mtime = os.stat(os.path.join(dirpath, filename)).st_mtime
      except OSError:
        continue
      newest = mtime if newest is None else max(newest, mtime)
  if newest is None:
    try:
      newest = os.stat(path).st_mtime
    except OSError:
      return None
  return newest


def _idle_past_window(path: pathlib.Path, scope: usage_sources.SweepScope) -> bool:
  """Newest-mtime form of the idle judgment for a transcript directory; False (logged) on probe failure."""
  newest = _newest_mtime(path)
  if newest is None:
    log.warning("storage_cool_idle_probe_failed", path=str(path))
    return False
  return scope.idle_past(newest)


def _live_worktree_dir_names(cfg: config.CharlieBotConfig) -> set[str]:
  """Encoded cwd names of the worktrees currently on disk (their runs may still write)."""
  worktree_dir = pathlib.Path(cfg.paths.worktree_dir)
  if not worktree_dir.is_dir():
    return set()
  return {
      claude_project_dir_name(child)
      for child in worktree_dir.iterdir()
      if child.is_dir() and child.name != worktree_trash.TRASH_DIR_NAME
  }


def _delete_claude_project_dir(path: pathlib.Path, counter: usage_sources.SweepCounter, dry_run: bool) -> None:
  """Delete a transcript directory file by file, then its empty skeleton."""
  for file_path in sorted(path.rglob("*")):
    if file_path.is_file():
      counter.delete_file(file_path, dry_run, count=False)
  if dry_run:
    counter.count += 1
    return
  try:
    shutil.rmtree(path)
  except OSError as e:
    log.warning("storage_cool_dir_delete_failed", path=str(path), error=str(e))
    return
  counter.count += 1


def _sweep_claude_transcripts(scope: usage_sources.SweepScope, counter: usage_sources.SweepCounter) -> None:
  """Delete cold sessions' transcript directories and unreferenced orphan directories.

  A transcript directory belongs to CharlieBot when its encoded name carries a
  session id. That session being cold is the forward-computed deletion case; the
  session having no metadata left makes the directory an orphan, reclaimable once
  it has been idle past the safety window. Worktree-encoded directories follow the
  same orphan rule once the worktree itself is gone. Any other name encodes a cwd
  CharlieBot never handed a claude process and is never touched.

  The live-session counterpart is claude_accounts.retire_transcript_copies, which
  trims a still-live session's redundant pool copies to the newest two; the two
  deletion sets are disjoint (whole cold trees vs individual copies of a live
  transcript), so neither can delete what the other protects.
  """
  cfg = scope.cfg
  worktree_prefix = claude_project_dir_name(pathlib.Path(cfg.paths.worktree_dir)) + "-"
  live_worktrees = _live_worktree_dir_names(cfg) if scope.session_id is None else set()
  for entry in usage_sources.sweep_root_entries(claude_projects_roots(cfg), lambda root: root.iterdir()):
    if not entry.is_dir():
      continue
    encoded_session = _encoded_session_id(entry.name)
    if encoded_session is not None:
      owner = scope.facts.get(encoded_session)
      if owner is not None:
        # The session still exists: only the cold rule decides.
        if owner.cold:
          _delete_claude_project_dir(entry, counter, scope.dry_run)
      elif (cfg.sessions_dir / encoded_session).is_dir():
        # An unreadable session metadata file is not proof that the session
        # was deleted; only a missing session directory makes this an orphan.
        continue
      elif scope.session_id is None and _idle_past_window(entry, scope):
        # No metadata references it any more: orphan past the window.
        _delete_claude_project_dir(entry, counter, scope.dry_run)
    elif scope.session_id is None and entry.name.startswith(worktree_prefix) and entry.name not in live_worktrees:
      # A worktree's transcripts are orphans once the worktree is gone.
      if _idle_past_window(entry, scope):
        _delete_claude_project_dir(entry, counter, scope.dry_run)


def sweep(scope: usage_sources.SweepScope) -> usage_sources.SourceSweep:
  """Claude Code's part of the sweep: one ``claude-transcripts`` category, counted in directories."""
  counter = usage_sources.SweepCounter("claude-transcripts", "dirs")
  _sweep_claude_transcripts(scope, counter)
  return usage_sources.SourceSweep(categories=(counter.result(),), freelist=None)
