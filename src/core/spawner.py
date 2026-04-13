"""Direct worker spawner — creates a task, enriches the prompt, and runs the worker."""

import asyncio
import time
import traceback
from datetime import datetime, timezone
from zoneinfo import ZoneInfo
from pathlib import Path
from typing import Optional

import structlog

from src.api.message_utils import extract_text_from_message
from src.agents.backends.claude_code import BASE_COMMAND
from src.agents.worker import QuotaExhaustedException, Worker
from src.core import event_types as ET
from src.core.models import BackendOption, SessionMetadata, SpawnRequest, ThreadMetadata, ThreadStatus
from src.core.ndjson import parse_ndjson_file
from src.core.git import git_current_branch, git_create_worktree, git_worktree_prune, git_worktree_remove
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.config import CharlieBotConfig, get_scheduled_tasks
from src.core.notifications import send_telegram
from src.core.review import maybe_spawn_reviewer

log = structlog.get_logger()

_CODING_PRINCIPLES = (
    "## Coding Principles\n"
    "The codebase has a single user. Apply these principles:\n"
    "- **Fail fast**: surface errors immediately. Do NOT add fallbacks, defaults, or silent recovery.\n"
    "- **No swallowed exceptions**: always log or re-raise. Never use bare `except: pass`.\n"
    "- **No defensive programming**: do not add guards for scenarios that cannot happen.\n")


def _build_worker_prompt(
    description: str,
    repo_path: Path,
    base_branch: str,
    branch_name: str,
    wt_path: str,
    session_meta: SessionMetadata,
    improve_dir: Optional[str] = None,
    iteration_number: Optional[int] = None,
    is_continuation: bool = False,
) -> str:
  """Build the task-specific worker prompt (session info + worktree workflow + task)."""
  session_info = (f"## Session Info\n"
                  f"- Session: {session_meta.name}\n")

  if is_continuation:
    worktree_section = (
        f"## Worktree Workflow\n"
        f"You are continuing work in an existing worktree from a previous iteration. "
        f"Review previous iteration changes before starting.\n"
        f"- Branch: `{branch_name}` (from `{base_branch}`)\n"
        f"- Worktree: `{wt_path}`\n"
        f"- Repo: `{repo_path}`\n\n"
        f"Follow these steps exactly:\n"
        f"1. `cd {wt_path}` — do ALL your work inside this worktree.\n"
        f"2. Commit your changes with descriptive messages.\n"
        f"   Use structured commit messages: first line is a short summary, then a blank line, "
        f"then a \"Why:\" line explaining the business reason for the change.\n\n"
        f"STOP here. Do NOT rebase, merge, or remove the worktree. A reviewer will handle that.\n\n"
        f"## Task\n{description}")
  else:
    worktree_section = (
        f"## Worktree Workflow\n"
        f"A dedicated git worktree is already created for you.\n"
        f"- Branch: `{branch_name}` (from `{base_branch}`)\n"
        f"- Worktree: `{wt_path}`\n"
        f"- Repo: `{repo_path}`\n\n"
        f"Follow these steps exactly:\n"
        f"1. `cd {wt_path}` — do ALL your work inside this worktree.\n"
        f"2. Commit your changes with descriptive messages.\n"
        f"   Use structured commit messages: first line is a short summary, then a blank line, "
        f"then a \"Why:\" line explaining the business reason for the change.\n\n"
        f"STOP here. Do NOT rebase, merge, or remove the worktree. A reviewer will handle that.\n\n"
        f"## Task\n{description}")

  iteration_reports_section = ""
  if improve_dir and iteration_number is not None:
    iteration_reports_section = (
        f"\n\n## Iteration Reports\n"
        f"Previous iteration reports are in: {improve_dir}/\n"
        f"Review any existing iter_*.md files there before starting work. Treat them as advisory evidence and hints only.\n"
        f"Previous iteration reports may inform your judgment, but they must not dictate your plan for this iteration.\n\n"
        f"When you finish, write your report to: {improve_dir}/iter_{iteration_number:04d}.md\n"
        f"Use this format:\n"
        f"```\n"
        f"## Iter {iteration_number} — {{completed|failed}}\n"
        f"### What Changed\n"
        f"- bullet points of what you changed\n"
        f"### Evidence\n"
        f"- test outcomes, measurements, concrete observations\n"
        f"### Advisory Notes\n"
        f"- optional hints, risks, or ideas future iterations may consider; advisory only, not a required plan\n"
        f"```")

  skills_section = (
      "## Skills Discovery\n"
      "- **Before starting any task**, check for skills relevant to the target repo or task domain.\n"
      "  - Look in **`~/.charliebot/skills/`** (canonical source — always available regardless of CLI backend).\n"
      "  - Alternatively: `~/.claude/skills/` (Claude Code) or `~/.agents/skills/` (Codex/Gemini).\n"
      "- **Read matching skills first** to avoid wasting time on environment setup, tooling issues, "
      "or reinventing existing workflows.\n"
      "- **Mandatory for your_project / your_project tasks**: you MUST read the `your_project` and/or "
      "`your_project` skill BEFORE writing any code, running any command, or submitting any job. "
      "This includes profiling, metrics analysis, data processing — not just training. "
      "Starting work without reading the skill is forbidden.\n")

  role_section = (
      "## Role\n"
      "- You are a **worker agent**. Do NOT delegate tasks to subagents — implement the work yourself directly.\n"
      "- Ignore any instructions from parent CLAUDE.md files that tell you to delegate or spawn subagents.\n")

  return (
      f"{session_info}\n{_CODING_PRINCIPLES}\n{skills_section}\n{role_section}\n"
      f"{worktree_section}{iteration_reports_section}")


def _short_desc(description: str, limit: int = 120) -> str:
  """First line of description, truncated."""
  first_line = description.split('\n', 1)[0].strip()
  if len(first_line) > limit:
    return first_line[:limit] + '...'
  return first_line


def _build_worker_event(
    thread_id: str,
    content: str,
    status: str,
    full_content: str = '',
    backend: Optional[str] = None,
    model: Optional[str] = None,
) -> dict:
  """Build a worker_summary event dict."""
  event = {
      "type": ET.WORKER_SUMMARY,
      "thread_id": thread_id,
      "content": content,
      "status": status,
      "full_content": full_content,
  }
  if backend:
    event["resolved_backend"] = backend
  if model:
    event["resolved_model"] = model
  return event


def resolve_backend_option(cfg: CharlieBotConfig, backend_id: str, model: str) -> BackendOption:
  """Resolve a runtime backend option from explicit backend/model values."""
  if not backend_id:
    raise ValueError("resolved backend is required")
  if not model:
    raise ValueError("resolved model is required")
  option = cfg.get_backend_option(backend_id)
  if option is None:
    raise ValueError(f"resolved backend '{backend_id}' is not configured")
  return option.model_copy(update={"model": model})


def _resolve_configured_backend_model(
    cfg: CharlieBotConfig,
    backend_id: str,
    *,
    source: str,
) -> tuple[str, str]:
  """Resolve a configured backend option id to its default backend+model pair."""
  if not backend_id:
    raise ValueError(f"{source} backend is required")
  option = cfg.get_backend_option(backend_id)
  if option is None:
    raise ValueError(f"{source} backend '{backend_id}' is not in backend_options")
  if not option.model:
    raise ValueError(f"{source} backend '{backend_id}' has no default model")
  return option.id, option.model


async def resolve_session_subagent_backend_model(
    session_id: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
) -> tuple[str, str]:
  """Resolve backend+model from the session default, with strict validation."""
  session_meta = await session_mgr.get_session(session_id)
  if session_meta is None:
    raise ValueError(f"session '{session_id}' not found")
  return _resolve_configured_backend_model(cfg, session_meta.backend, source="session")


async def resolve_requested_subagent_backend_model(
    session_id: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    requested_backend: Optional[str] = None,
) -> tuple[str, str]:
  """Resolve backend+model from an explicit configured backend or the session default."""
  session_meta = await session_mgr.get_session(session_id)
  if session_meta is None:
    raise ValueError(f"session '{session_id}' not found")
  if requested_backend is not None:
    return _resolve_configured_backend_model(cfg, requested_backend, source="requested")
  return _resolve_configured_backend_model(cfg, session_meta.backend, source="session")


def _require_thread_backend_model(thread: ThreadMetadata) -> tuple[str, str]:
  """Return backend+model from thread metadata or raise."""
  if not thread.backend:
    raise ValueError(f"thread '{thread.id}' missing backend metadata")
  if not thread.model:
    raise ValueError(f"thread '{thread.id}' missing model metadata")
  return thread.backend, thread.model


async def _create_worktree_and_process(
    session_id: str,
    thread: ThreadMetadata,
    description: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
    resolved_repo: Path,
    req: SpawnRequest,
) -> Worker:
  """Create worktree, build prompt, resolve backend, and construct Worker."""
  worktree_path: Optional[Path] = None

  if req.prompt_override:
    worker_prompt = req.prompt_override
    if not thread.worktree_path:
      log.error(
          "spawn_worker_missing_worktree_path",
          session=session_id,
          thread_id=thread.id,
          detail="prompt_override requires persisted worktree_path",
      )
      raise RuntimeError("thread metadata missing worktree_path")
    worktree_path = Path(thread.worktree_path).resolve()
  elif req.worktree_path_override:
    # Reuse an existing worktree (e.g. improve loop iterations sharing a single worktree).
    base_branch = req.base_branch or await git_current_branch(resolved_repo)
    branch_name = req.branch_name_override or f"charliebot/task-{int(time.time())}-{thread.id[:8]}"
    wt_path = Path(req.worktree_path_override)

    thread.branch_name = branch_name
    thread.repo_path = str(resolved_repo)
    thread.worktree_path = str(wt_path)
    thread.base_branch = base_branch
    thread.skip_cleanup = req.skip_cleanup
    thread.context = req.context

    session_meta = await session_mgr.get_session(session_id)
    worker_prompt = _build_worker_prompt(
        description,
        resolved_repo,
        base_branch,
        branch_name,
        str(wt_path),
        session_meta,
        improve_dir=req.improve_dir,
        iteration_number=req.iteration_number,
        is_continuation=req.is_continuation)
    worktree_path = wt_path.resolve()
  else:
    # Get current branch as the base for the worktree
    base_branch = req.base_branch or await git_current_branch(resolved_repo)

    # Compute branch name and worktree path
    ts = int(time.time())
    branch_name = req.branch_name_override or f"charliebot/task-{ts}-{thread.id[:8]}"
    wt_path = Path(cfg.worktree_dir) / branch_name.replace("/", "-")

    # Ensure worktree parent dir exists and create worktree before launch.
    Path(cfg.worktree_dir).mkdir(parents=True, exist_ok=True)
    await git_create_worktree(resolved_repo, base_branch, branch_name, wt_path)

    # Store branch_name, repo_path, worktree path, and optional context on thread metadata.
    thread.branch_name = branch_name
    thread.repo_path = str(resolved_repo)
    thread.worktree_path = str(wt_path)
    thread.base_branch = base_branch
    thread.context = req.context

    # Build enriched prompt with worktree workflow instructions
    session_meta = await session_mgr.get_session(session_id)
    worker_prompt = _build_worker_prompt(
        description,
        resolved_repo,
        base_branch,
        branch_name,
        str(wt_path),
        session_meta,
        improve_dir=req.improve_dir,
        iteration_number=req.iteration_number)
    worktree_path = wt_path.resolve()

  if worktree_path is None:
    raise RuntimeError("worktree path was not resolved")
  if worktree_path == resolved_repo:
    log.error(
        "spawn_worker_repo_root_cwd_detected",
        session=session_id,
        thread_id=thread.id,
        repo=str(resolved_repo),
        worktree=str(worktree_path),
    )
    raise RuntimeError("refusing to run subagent in repo root; worktree isolation required")

  backend_option = resolve_backend_option(cfg, req.resolved_backend, req.resolved_model)
  thread.backend = backend_option.id
  thread.model = backend_option.model
  await thread_mgr.save_metadata(thread)

  # Build Worker
  events_log = await thread_mgr.get_events_log_path(session_id, thread.id)
  return Worker(
      thread,
      worktree_path,
      events_log,
      worker_prompt,
      cfg,
      backend_option=backend_option,
      on_spawned=thread_mgr.save_metadata,
  )


async def _create_repoless_process(
    session_id: str,
    thread: ThreadMetadata,
    description: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
    req: SpawnRequest,
) -> Worker:
  """Create a repo-less worker for prompt-only tasks (no worktree, no git)."""
  worker_prompt = req.prompt_override or description
  thread_dir = cfg.sessions_dir / session_id / 'threads' / thread.id

  # Repo-less tasks cannot produce branch/worktree review artifacts.
  thread.branch_name = None
  thread.repo_path = None
  thread.worktree_path = str(thread_dir)
  thread.require_review = False
  thread.context = req.context

  backend_option = resolve_backend_option(cfg, req.resolved_backend, req.resolved_model)
  thread.backend = backend_option.id
  thread.model = backend_option.model
  await thread_mgr.save_metadata(thread)

  events_log = await thread_mgr.get_events_log_path(session_id, thread.id)
  return Worker(
      thread,
      thread_dir,
      events_log,
      worker_prompt,
      cfg,
      backend_option=backend_option,
      on_spawned=thread_mgr.save_metadata,
  )


async def _stream_worker_events(
    worker: Worker,
    session_id: str,
    description: str,
    thread: ThreadMetadata,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
) -> tuple[int, bool, str]:
  """Mark worker running, broadcast start event, run worker.

  Returns (exit_code, quota_exhausted, error_message).
  """
  thread.cli_command = " ".join(BASE_COMMAND + [description])
  thread.status = ThreadStatus.RUNNING
  thread.started_at = datetime.now(timezone.utc)
  await thread_mgr.save_metadata(thread)
  log.info("worker_running", thread_id=thread.id, session=session_id)

  now = datetime.now(ZoneInfo('America/Los_Angeles')).strftime('%m/%d %H:%M')
  started_event = _build_worker_event(
      thread.id,
      f'Worker `{thread.id[:8]}` started ({now}): {_short_desc(description)}',
      'running',
      backend=thread.backend,
      model=thread.model,
  )
  await session_mgr.persist_and_broadcast(session_id, started_event)

  try:
    exit_code = await worker.run()
    return exit_code, False, ""
  except QuotaExhaustedException:
    await worker.terminate()
    log.warning("worker_quota_exhausted", thread_id=thread.id)
    return -1, True, ""
  except Exception as e:
    await worker.terminate()
    log.error("worker_failed", thread_id=thread.id, error=str(e), traceback=traceback.format_exc())
    return -1, False, str(e)


async def _cleanup_worker_directory(thread: ThreadMetadata, skip_cleanup: bool) -> None:
  """Remove the worker's worktree or temp directory after it finishes."""
  if skip_cleanup:
    return

  # Git worktree removal for repo-based workers.
  if getattr(thread, 'worktree_path', None) and getattr(thread, 'repo_path', None):
    wt = Path(thread.worktree_path)
    if wt.exists():
      try:
        removed = await git_worktree_remove(thread.repo_path, wt, thread.id)
        if not removed:
          return
        await git_worktree_prune(thread.repo_path, thread.id)
      except Exception as wt_err:
        log.warning("worktree_cleanup_error", thread_id=thread.id, error=str(wt_err))
    return


async def _finalize_worker(
    session_id: str,
    description: str,
    thread: ThreadMetadata,
    exit_code: int,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    cfg: CharlieBotConfig,
    quota_exhausted: bool = False,
    error: str = "",
    skip_notify: bool = False,
) -> None:
  """Update thread status and notify completion."""
  # Re-read from disk: the cancel endpoint may have already set CANCELLED.
  current = await thread_mgr.get_thread(session_id, thread.id)
  if current and current.status == ThreadStatus.CANCELLED:
    # Cancel endpoint already set the status; don't overwrite.
    log.info("worker_already_cancelled", thread_id=thread.id)
  elif quota_exhausted:
    await thread_mgr.update_status(session_id, thread.id, ThreadStatus.FAILED)
  elif error:
    await thread_mgr.update_status(session_id, thread.id, ThreadStatus.FAILED, exit_code=-1)
  elif exit_code == 0:
    await thread_mgr.update_status(session_id, thread.id, ThreadStatus.COMPLETED, exit_code=0)
    log.info("worker_completed", thread_id=thread.id)
  else:
    await thread_mgr.update_status(session_id, thread.id, ThreadStatus.FAILED, exit_code=exit_code)
    log.warning("worker_failed_nonzero", thread_id=thread.id, exit_code=exit_code)

  # Clean up the thread's worktree/temp directory.
  # Skip cleanup if a reviewer will be spawned — it needs the worktree.
  # Also skip if the thread was marked skip_cleanup (e.g. improve loop shared worktree).
  if getattr(thread, 'skip_cleanup', False):
    skip_cleanup = True
  else:
    can_spawn_reviewer = all(getattr(thread, attr, None) for attr in ('repo_path', 'branch_name', 'worktree_path'))
    skip_cleanup = (
        exit_code == 0 and getattr(thread, 'require_review', False) and not getattr(thread, 'review_of', None) and
        can_spawn_reviewer)
  await _cleanup_worker_directory(thread, skip_cleanup)

  if not skip_notify:
    await _notify_completion(
        session_id,
        description,
        thread,
        exit_code,
        thread_mgr,
        session_mgr,
        cfg,
        quota_exhausted=quota_exhausted,
        error=error)


class DelegationBlockedError(Exception):
  """Raised when the takeoff gate rejects a delegation attempt."""


def _check_takeoff_gate(session_id: str, session_mgr: SessionManager) -> None:
  """Verify the last real user message contains 'take off'. Raises DelegationBlockedError if not."""
  events = session_mgr.load_chat_events_sync(session_id)
  if not events:
    raise DelegationBlockedError(
        'Delegation blocked: no chat events found for session. '
        'Show the plan and wait for the user to say "take off" before delegating.')

  # Scan backwards for the last user event with real text (not a tool_result)
  for event in reversed(events):
    if event.get("type") != "user":
      continue
    content = event.get("content")
    # Real user text is a string; tool_results come as list-of-dict with type=tool_result
    if isinstance(content, str):
      if "take off" not in content.lower():
        raise DelegationBlockedError(
            'Delegation blocked: the last user message does not contain "take off". '
            'Show the plan and wait for the user to say "take off" before delegating.')
      return

  raise DelegationBlockedError(
      'Delegation blocked: no user message found in chat history. '
      'Show the plan and wait for the user to say "take off" before delegating.')


async def spawn_worker(
    session_id: str,
    description: str,
    thread_id: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
    request: Optional[SpawnRequest] = None,
) -> None:
  """Spawn a Claude Code worker for the given thread. Fire-and-forget via asyncio.create_task()."""
  req = request or SpawnRequest()
  if req.require_takeoff:
    await asyncio.to_thread(_check_takeoff_gate, session_id, session_mgr)

  thread = None
  worker = None
  exit_code = -1
  quota_exhausted = False
  error_msg = ""
  try:
    thread = await thread_mgr.get_thread(session_id, thread_id)
    if not thread:
      log.error("spawn_worker_thread_missing", session=session_id, thread_id=thread_id)
      return

    if req.repo_path is None:
      # Repo-less worker: run prompt directly without worktree
      worker = await _create_repoless_process(session_id, thread, description, cfg, session_mgr, thread_mgr, req)
    else:
      resolved_repo = Path(req.repo_path).resolve()
      worker = await _create_worktree_and_process(
          session_id, thread, description, cfg, session_mgr, thread_mgr, resolved_repo, req)

    exit_code, quota_exhausted, error_msg = await _stream_worker_events(
        worker, session_id, description, thread, thread_mgr, session_mgr)

  except asyncio.CancelledError:
    log.warning("spawn_worker_cancelled", session=session_id, thread_id=thread_id)
    if worker:
      await worker.terminate()
    raise
  except Exception as e:
    log.error("spawn_worker_setup_failed", session=session_id, error=str(e), traceback=traceback.format_exc())
    error_msg = str(e)
  finally:
    if thread is not None:
      try:
        await _finalize_worker(
            session_id,
            description,
            thread,
            exit_code,
            thread_mgr,
            session_mgr,
            cfg,
            quota_exhausted=quota_exhausted,
            error=error_msg,
            skip_notify=req.skip_notify)
      except Exception as e:
        log.error("spawn_worker_finalize_failed", session=session_id, traceback=traceback.format_exc())
        try:
          await session_mgr.persist_and_broadcast(
              session_id, {
                  "type": ET.ERROR,
                  "content": f"Worker finalization failed: {e}"
              })
        except Exception:
          log.warning("spawn_worker_finalize_broadcast_failed", session=session_id, exc_info=True)


async def _broadcast_completion(
    session_id: str,
    description: str,
    thread,
    exit_code: int,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    quota_exhausted: bool = False,
    error: str = "",
) -> tuple[str, str]:
  """Build and broadcast the worker_summary event. Returns (events_summary, full_summary)."""
  # Update last_run_status for scheduled sessions
  session_meta = await session_mgr.get_session(session_id)
  if session_meta and session_meta.scheduled_task:
    session_meta.last_run_status = "success" if exit_code == 0 else "failed"
    session_meta.updated_at = datetime.now(timezone.utc)
    await session_mgr.save_metadata(session_meta)

  events_summary = await _read_events_summary(session_id, thread.id, thread_mgr)

  # Re-read to pick up cancel endpoint's status
  current_thread = await thread_mgr.get_thread(session_id, thread.id)
  cancelled = current_thread and current_thread.status == ThreadStatus.CANCELLED

  status = 'cancelled' if cancelled else ('completed' if exit_code == 0 else 'failed')
  now = datetime.now(ZoneInfo('America/Los_Angeles')).strftime('%m/%d %H:%M')
  chat_summary = f'Worker `{thread.id[:8]}` finished ({now}): {_short_desc(description)}'
  full_summary = f"**Worker finished: {description}**\n\n{events_summary}"

  suffix = ""
  if cancelled:
    suffix = "\n\n*Cancelled by user.*"
  elif quota_exhausted:
    suffix = "\n\n*Worker stopped: API quota exhausted.*"
  elif error:
    suffix = f"\n\n*Worker error: {error}*"
  elif exit_code != 0:
    suffix = f"\n\n*Worker exited with code {exit_code}.*"
  chat_summary += suffix
  full_summary += suffix

  worker_event = _build_worker_event(
      thread.id,
      chat_summary,
      status,
      full_content=full_summary,
      backend=thread.backend,
      model=thread.model,
  )
  await session_mgr.mark_unread(session_id)
  await session_mgr.persist_and_broadcast(session_id, worker_event)
  log.info("worker_summary_sent", session=session_id, thread=thread.id)
  return events_summary, full_summary


async def _notify_completion(
    session_id: str,
    description: str,
    thread,
    exit_code: int,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    cfg: CharlieBotConfig,
    quota_exhausted: bool = False,
    error: str = "",
) -> None:
  """Broadcast worker_summary event to the session WebSocket and trigger master agent."""
  try:
    events_summary, full_summary = await _broadcast_completion(
        session_id, description, thread, exit_code, thread_mgr, session_mgr, quota_exhausted, error)

    # Send Telegram notification if the session's scheduled task has notify='telegram'.
    try:
      session_meta = await session_mgr.get_session(session_id)
      if session_meta and session_meta.scheduled_task:
        for task in get_scheduled_tasks():
          if task.name == session_meta.scheduled_task and task.notify == 'telegram':
            await send_telegram(events_summary, cfg)
            break
    except Exception as tg_err:
      log.warning("telegram_notify_failed", session=session_id, error=str(tg_err))

    await maybe_spawn_reviewer(
        session_id, thread, exit_code, events_summary, full_summary, thread_mgr, session_mgr, cfg)
  except Exception as e:
    log.error("notify_completion_failed", thread_id=thread.id, error=str(e))
    try:
      fallback = f'Worker `{thread.id[:8]}` finished: {_short_desc(description)}\n\n*(summary unavailable: {e})*'
      fallback_event = _build_worker_event(
          thread.id,
          fallback,
          "completed" if exit_code == 0 else "failed",
          full_content=fallback,
          backend=thread.backend,
          model=thread.model,
      )
      await session_mgr.mark_unread(session_id)
      await session_mgr.persist_and_broadcast(session_id, fallback_event)
    except Exception as inner:
      log.error("fallback_notify_failed", thread_id=thread.id, error=str(inner), traceback=traceback.format_exc())


async def _read_events_summary(session_id: str, thread_id: str, thread_mgr: ThreadManager, max_lines: int = 80) -> str:
  """Read the last N lines from a thread's events.jsonl for summarization."""
  events_path = await thread_mgr.get_events_log_path(session_id, thread_id)
  events = await asyncio.to_thread(parse_ndjson_file, events_path)
  if not events:
    return "(no events recorded)"
  tail = events[-max_lines:]
  parts = []
  for ev in tail:
    ev_type = ev.get("type", "unknown")
    content = _extract_event_content(ev, ev_type)
    if content:
      parts.append(f"[{ev_type}] {content}")
  return "\n".join(parts) if parts else "(empty event log)"


def _extract_event_content(ev: dict, ev_type: str) -> str:
  """Extract human-readable content from a Claude Code stream-json event."""
  if ev_type == ET.RESULT:
    return str(ev.get("result", ""))[:500]

  if ev_type == ET.ASSISTANT:
    msg = ev.get("message") if isinstance(ev.get("message"), dict) else {}
    text = extract_text_from_message(msg)
    blocks = msg.get("content") or []
    tool_parts = [
        f"[tool_use: {b.get('name', '?')}]" for b in (blocks if isinstance(blocks, list) else [])
        if isinstance(b, dict) and b.get("type") == ET.TOOL_USE
    ]
    parts = ([text] if text else []) + tool_parts
    return " ".join(parts)[:300] if parts else ""

  if ev_type == "rate_limit_event":
    rli = ev.get("rate_limit_info", {})
    status = rli.get("status", "unknown")
    rate_type = rli.get("rateLimitType", "unknown")
    return f"Rate limit {status} ({rate_type})"

  if ev_type in (ET.THINKING, ET.ERROR, ET.COMPLETE, ET.TOOL_RESULT, ET.TOOL_USE, ET.FILE_WRITE):
    content = ev.get("content", ev.get("message", ""))
    if isinstance(content, list):
      text = extract_text_from_message({"content": content})
      return text[:200] if text else ""
    return str(content)[:200]

  return ""
