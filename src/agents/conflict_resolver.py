"""Conflict Resolution Worker — handles git merge conflicts between branches."""

from pathlib import Path

import structlog

from src.core.config import CharliBotConfig
from src.core.git import GitManager
from src.core.models import Priority, SessionMetadata, Task
from src.core.threads import ThreadManager

log = structlog.get_logger()


class ConflictResolver:
  """Spawns a specialized Worker to resolve merge conflicts."""

  def __init__(self, cfg: CharliBotConfig, git_manager: GitManager, thread_manager: ThreadManager):
    self._cfg = cfg
    self._git = git_manager
    self._thread_manager = thread_manager

  async def resolve(
    self,
    session_meta: SessionMetadata,
    target_branch: str,
    source_branch: str,
  ) -> bool:
    """
    Attempt to resolve merge conflicts between two branches.
    Returns True if resolved, False if manual intervention is required.
    """
    bare_path = self._cfg.repos_dir / f"{session_meta.id}.git"

    # Gather context for the Worker
    try:
      target_log = await self._git.get_commit_log(bare_path, target_branch, n=10)
      source_log = await self._git.get_commit_log(bare_path, source_branch, n=10)
      diff = await self._git.get_diff(bare_path, target_branch, source_branch)
    except Exception as e:
      log.error("conflict_context_failed", error=str(e))
      return False

    task_description = self._build_conflict_task(
      target_branch=target_branch,
      source_branch=source_branch,
      target_log=target_log,
      source_log=source_log,
      diff=diff[:8000],  # Truncate very large diffs
    )

    conflict_task = Task(
      priority=Priority.P0,
      description=task_description,
      context={
        "target_branch": target_branch,
        "source_branch": source_branch,
        "type": "conflict_resolution",
      },
    )

    thread = await self._thread_manager.create_thread(
      session_meta=session_meta,
      task=conflict_task,
      is_conflict_resolver=True,
    )

    # Import Worker here to avoid circular imports
    from src.agents.worker import Worker

    worktree_path = await self._thread_manager.get_worktree_path(session_meta.id, thread.id)
    events_log = await self._thread_manager.get_events_log_path(session_meta.id, thread.id)

    worker = Worker(
      thread_metadata=thread,
      worktree_path=worktree_path,
      events_log_path=events_log,
      task_description=task_description,
    )

    try:
      exit_code = await worker.run()
      success = exit_code == 0
      log.info("conflict_resolution_done", thread=thread.id, success=success)
      return success
    except Exception as e:
      log.error("conflict_resolution_failed", thread=thread.id, error=str(e))
      return False

  def _build_conflict_task(
    self,
    target_branch: str,
    source_branch: str,
    target_log: list[str],
    source_log: list[str],
    diff: str,
  ) -> str:
    target_log_str = "\n".join(target_log) or "(no commits)"
    source_log_str = "\n".join(source_log) or "(no commits)"

    return f"""## Merge Conflict Resolution Task

You are resolving a merge conflict between two branches.

**Target branch** (merge INTO): `{target_branch}`
Recent commits:
{target_log_str}

**Source branch** (merge FROM): `{source_branch}`
Recent commits:
{source_log_str}

**Diff between branches:**
```diff
{diff}
```

## Instructions

1. Examine the conflicted files (look for `<<<<<<<`, `=======`, `>>>>>>>` markers)
2. Understand the intent of each branch's changes from the commit history
3. Resolve each conflict by choosing the semantically correct combination:
   - If changes are independent: keep both
   - If one clearly supersedes the other: keep the better one
   - If they conflict in logic: merge them intelligently
4. Remove all conflict markers
5. Run tests if available to verify correctness
6. Commit the resolution with message: `fix: resolve merge conflict between {source_branch} and {target_branch}`
7. Output a brief explanation of each conflict and how you resolved it
"""
