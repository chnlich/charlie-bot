"""Direct worker spawner — creates a task, enriches the prompt, and runs the worker."""

import asyncio
import time
import traceback
from datetime import datetime, timezone
from zoneinfo import ZoneInfo
from pathlib import Path
from typing import Optional

import structlog

from src.agents.master_cc import run_message
from src.agents.worker import WORKER_COMMAND, QuotaExhaustedException, Worker
from src.core.models import BackendOption, SessionMetadata, ThreadMetadata, ThreadStatus
from src.core.ndjson import parse_ndjson_file
from src.core.sessions import SessionManager
from src.core.streaming import streaming_manager
from src.core.threads import ThreadManager
from src.core.config import CharlieBotConfig

log = structlog.get_logger()


def _build_worker_prompt(description: str, repo_path: Path, base_branch: str, branch_name: str, wt_path: str) -> str:
  """Build the full worker prompt including worktree workflow instructions."""
  return (
      f"## Worktree Workflow\n"
      f"You MUST isolate your work in a git worktree. Follow these steps exactly:\n"
      f"1. Create worktree: `git worktree add -b {branch_name} {wt_path} {base_branch}`\n"
      f"2. `cd {wt_path}` — do ALL your work inside this worktree.\n"
      f"3. Commit your changes with descriptive messages.\n\n"
      f"STOP here. Do NOT rebase, merge, or remove the worktree. A reviewer will handle that.\n\n"
      f"## Task\n{description}")


def _build_review_prompt(description: str, branch_name: str, wt_path: str, repo_path: Path, base_branch: str) -> str:
  """Build the prompt for a review worker."""
  return (
      f"## Code Review\n"
      f"You are reviewing another worker's code changes.\n\n"
      f"Original task: {description}\n\n"
      f"The work is on branch `{branch_name}` in worktree `{wt_path}`.\n\n"
      f"1. `cd {wt_path}`\n"
      f"2. Review the changes: `git diff {base_branch}...{branch_name}`\n"
      f"3. Check for: correctness, bugs, style violations (Google Style, 2-space indent, 120-col), missing edge cases.\n"
      f"4. If you find issues, fix them and commit with descriptive messages.\n"
      f"5. Rebase onto base: `git rebase {base_branch}` — resolve any conflicts.\n"
      f"6. Merge: `cd {repo_path} && git merge --ff-only {branch_name}`\n"
      f"7. Clean up: `git worktree remove {wt_path}`")


async def _git_current_branch(repo_path: Path) -> str:
  """Get the current branch of the repo."""
  proc = await asyncio.create_subprocess_exec(
      "git",
      "rev-parse",
      "--abbrev-ref",
      "HEAD",
      cwd=str(repo_path),
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
  )
  stdout, stderr = await proc.communicate()
  if proc.returncode != 0:
    err_msg = stderr.decode().strip()
    if 'unknown revision' in err_msg:
      log.warning('git_empty_repo_fallback', repo=str(repo_path), detail='no commits yet, defaulting to main')
      return 'main'
    raise RuntimeError(f'git rev-parse failed: {err_msg}')
  return stdout.decode().strip()


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
      "type": "worker_summary",
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
  option = next((opt for opt in cfg.backend_options if opt.id == backend_id), None)
  if option is None:
    raise ValueError(f"resolved backend '{backend_id}' is not configured")
  return BackendOption(id=option.id, label=option.label, type=option.type, model=model)


async def resolve_session_subagent_backend_model(
    session_id: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
) -> tuple[str, str]:
  """Resolve backend+model from the session default, with strict validation."""
  session_meta = await session_mgr.get_session(session_id)
  if session_meta is None:
    raise ValueError(f"session '{session_id}' not found")
  backend_id = session_meta.backend
  if not backend_id:
    raise ValueError(f"session '{session_id}' has no backend configured")
  option = next((opt for opt in cfg.backend_options if opt.id == backend_id), None)
  if option is None:
    raise ValueError(f"session backend '{backend_id}' is not in backend_options")
  if not option.model:
    raise ValueError(f"session backend '{backend_id}' has no default model")
  return option.id, option.model


def _require_thread_backend_model(thread: ThreadMetadata) -> tuple[str, str]:
  """Return backend+model from thread metadata or raise."""
  if not thread.backend:
    raise ValueError(f"thread '{thread.id}' missing backend metadata")
  if not thread.model:
    raise ValueError(f"thread '{thread.id}' missing model metadata")
  return thread.backend, thread.model


async def broadcast_and_persist(session_id: str, event: dict, session_mgr: SessionManager) -> None:
  """Broadcast an event to the session WebSocket channel and persist it to NDJSON."""
  await streaming_manager.broadcast(f"session:{session_id}", event)
  await session_mgr.save_chat_event(session_id, event)


async def spawn_worker(
    session_id: str,
    description: str,
    thread_id: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
    repo_path: Optional[str] = None,
    prompt_override: Optional[str] = None,
    resolved_backend: str = "",
    resolved_model: str = "",
) -> None:
  """Spawn a Claude Code worker for the given thread. Fire-and-forget via asyncio.create_task()."""
  try:
    thread = await thread_mgr.get_thread(session_id, thread_id)
    if not thread:
      log.error("spawn_worker_thread_missing", session=session_id, thread_id=thread_id)
      return

    if repo_path is None:
      repos = cfg.discover_repos()
      if not repos:
        log.error("spawn_worker_no_repo", session=session_id, detail="no repos found in workspace_dirs")
        return
      repo_path = repos[0]["path"]
      log.info("spawn_worker_repo_defaulted", session=session_id, repo=repo_path)

    resolved_repo = Path(repo_path)

    if prompt_override:
      worker_prompt = prompt_override
    else:
      # Get current branch as the base for the worktree
      base_branch = await _git_current_branch(resolved_repo)

      # Compute branch name and worktree path
      ts = int(time.time())
      branch_name = f"charliebot/task-{ts}-{thread.id[:8]}"
      wt_path = str(Path(cfg.worktree_dir) / branch_name.replace("/", "-"))

      # Store branch_name and repo_path on thread metadata
      thread.branch_name = branch_name
      thread.repo_path = str(resolved_repo)
      await thread_mgr._save_metadata(thread)

      # Build enriched prompt with worktree workflow instructions
      worker_prompt = _build_worker_prompt(description, resolved_repo, base_branch, branch_name, wt_path)

    # Ensure worktree parent dir exists
    Path(cfg.worktree_dir).mkdir(parents=True, exist_ok=True)

    backend_option = resolve_backend_option(cfg, resolved_backend, resolved_model)
    thread.backend = backend_option.id
    thread.model = backend_option.model
    await thread_mgr._save_metadata(thread)

    # Build and run Worker
    events_log = await thread_mgr.get_events_log_path(session_id, thread_id)
    worker = Worker(
        thread,
        resolved_repo,
        events_log,
        worker_prompt,
        cfg,
        backend_option=backend_option,
        on_spawned=thread_mgr._save_metadata,
    )

    # Mark RUNNING
    thread.cli_command = " ".join(WORKER_COMMAND + [description])
    thread.status = ThreadStatus.RUNNING
    thread.started_at = datetime.now(timezone.utc)
    await thread_mgr._save_metadata(thread)
    log.info("worker_running", thread_id=thread.id, session=session_id)

    now = datetime.now(ZoneInfo('America/New_York')).strftime('%m/%d %H:%M')
    started_event = _build_worker_event(
        thread.id,
        f'Worker `{thread.id[:8]}` started ({now}): {_short_desc(description)}',
        'running',
        backend=thread.backend,
        model=thread.model,
    )
    await broadcast_and_persist(session_id, started_event, session_mgr)

    try:
      exit_code = await worker.run()
      if exit_code == 0:
        await thread_mgr.update_status(session_id, thread.id, ThreadStatus.COMPLETED, exit_code=0)
        log.info("worker_completed", thread_id=thread.id)
      else:
        await thread_mgr.update_status(session_id, thread.id, ThreadStatus.FAILED, exit_code=exit_code)
        log.warning("worker_failed_nonzero", thread_id=thread.id, exit_code=exit_code)

      await _notify_completion(session_id, description, thread, exit_code, thread_mgr, session_mgr, cfg)

    except QuotaExhaustedException:
      await worker.terminate()
      await thread_mgr.update_status(session_id, thread.id, ThreadStatus.FAILED)
      log.warning("worker_quota_exhausted", thread_id=thread.id)
      await _notify_completion(session_id, description, thread, -1, thread_mgr, session_mgr, cfg, quota_exhausted=True)

    except Exception as e:
      await worker.terminate()
      await thread_mgr.update_status(session_id, thread.id, ThreadStatus.FAILED, exit_code=-1)
      log.error("worker_failed", thread_id=thread.id, error=str(e), traceback=traceback.format_exc())
      await _notify_completion(session_id, description, thread, -1, thread_mgr, session_mgr, cfg, error=str(e))

  except Exception as e:
    log.error("spawn_worker_setup_failed", session=session_id, error=str(e), traceback=traceback.format_exc())


async def _trigger_master(
    session_id: str,
    summary: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
) -> None:
  """Best-effort trigger of the master agent to process a worker result."""
  try:
    session_meta = await session_mgr.get_session(session_id)
    if not session_meta or not session_meta.cc_session_id:
      log.debug("trigger_master_skipped", session=session_id, reason="no cc_session_id")
      return

    try:
      new_cc_session_id = await run_message(
          cfg,
          session_meta,
          summary,
          session_mgr.save_chat_event,
          session_mgr.save_metadata,
          mark_unread=session_mgr.mark_unread,
          skip_user_event=True,
      )
    except Exception as e:
      if not _is_resume_not_found_error(e):
        raise

      stale_cc_session_id = session_meta.cc_session_id
      log.warning(
          "trigger_master_invalid_resume_detected",
          session=session_id,
          cc_session_id=stale_cc_session_id,
          error=str(e),
      )

      retry_session_meta = session_meta.model_copy(deep=True)
      retry_session_meta.cc_session_id = None
      log.info(
          "trigger_master_retry_without_resume",
          session=session_id,
          stale_cc_session_id=stale_cc_session_id,
      )
      new_cc_session_id = await run_message(
          cfg,
          retry_session_meta,
          summary,
          session_mgr.save_chat_event,
          session_mgr.save_metadata,
          mark_unread=session_mgr.mark_unread,
          skip_user_event=True,
      )
      log.info(
          "trigger_master_resume_recovery_succeeded",
          session=session_id,
          stale_cc_session_id=stale_cc_session_id,
          recovered_cc_session_id=new_cc_session_id,
      )

    if new_cc_session_id and new_cc_session_id != session_meta.cc_session_id:
      await _persist_cc_session_id(session_id, new_cc_session_id, session_meta, session_mgr)
  except Exception as e:
    log.error("trigger_master_failed", session=session_id, error=str(e), traceback=traceback.format_exc())


async def _persist_cc_session_id(
    session_id: str,
    new_cc_session_id: str,
    fallback_meta: SessionMetadata,
    session_mgr: SessionManager,
) -> None:
  """Persist a refreshed cc_session_id without clobbering unrelated metadata fields."""
  fresh = await session_mgr.get_session(session_id)
  meta = fresh or fallback_meta
  meta.cc_session_id = new_cc_session_id
  await session_mgr.save_metadata(meta)


def _is_resume_not_found_error(error: Exception) -> bool:
  """Return True only for stale resume errors where session/conversation is missing."""
  message = str(error).lower()
  if "resume" not in message:
    return False

  has_conversation_not_found = "conversation" in message and "not found" in message
  has_session_not_found = "session" in message and "not found" in message
  return has_conversation_not_found or has_session_not_found


async def _spawn_review_worker(
    session_id: str,
    original_thread,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
) -> None:
  """Spawn a review worker for a successfully completed worker's branch."""
  if not original_thread.repo_path:
    log.error("spawn_review_no_repo_path", session=session_id, thread=original_thread.id,
              detail="original thread missing repo_path")
    return
  repo_path = Path(original_thread.repo_path)

  base_branch = await _git_current_branch(repo_path)
  branch_name = original_thread.branch_name
  wt_path = str(Path(cfg.worktree_dir) / branch_name.replace("/", "-"))

  review_prompt = _build_review_prompt(original_thread.description, branch_name, wt_path, repo_path, base_branch)
  resolved_backend, resolved_model = _require_thread_backend_model(original_thread)

  session_meta = await session_mgr.get_session(session_id)
  review_thread = await thread_mgr.create_thread(
      session_meta,
      f"Review: {original_thread.description}",
      review_of=original_thread.id,
  )

  asyncio.create_task(
      spawn_worker(
          session_id,
          review_thread.description,
          review_thread.id,
          cfg,
          session_mgr,
          thread_mgr,
          repo_path=str(repo_path),
          prompt_override=review_prompt,
          resolved_backend=resolved_backend,
          resolved_model=resolved_model,
      ))


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
    # Update last_run_status for scheduled sessions
    session_meta = await session_mgr.get_session(session_id)
    if session_meta and session_meta.scheduled_task:
      session_meta.last_run_status = "success" if exit_code == 0 else "failed"
      session_meta.updated_at = datetime.now(timezone.utc)
      await session_mgr.save_metadata(session_meta)

    events_summary = await _read_events_summary(session_id, thread.id, thread_mgr)

    status = "completed" if exit_code == 0 else "failed"
    now = datetime.now(ZoneInfo('America/New_York')).strftime('%m/%d %H:%M')
    chat_summary = f'Worker `{thread.id[:8]}` finished ({now}): {_short_desc(description)}'
    full_summary = f"**Worker finished: {description}**\n\n{events_summary}"

    suffix = ""
    if quota_exhausted:
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
    await broadcast_and_persist(session_id, worker_event, session_mgr)
    log.info("worker_summary_sent", session=session_id, thread=thread.id)

    # Re-read thread metadata to get review_of field
    thread_meta = await thread_mgr.get_thread(session_id, thread.id)

    if exit_code == 0 and not thread_meta.review_of:
      # Successful worker, not a review -> spawn reviewer, don't trigger master yet
      await _spawn_review_worker(session_id, thread_meta, cfg, session_mgr, thread_mgr)
      return

    if thread_meta.review_of:
      # This IS the review -> combine summaries, trigger master
      original_events = await _read_events_summary(session_id, thread_meta.review_of, thread_mgr)
      combined = f"**Original worker result:**\n{original_events}\n\n**Review result:**\n{events_summary}"
      await _trigger_master(session_id, combined, cfg, session_mgr)
      return

    # Failed/cancelled worker -> trigger master immediately
    await _trigger_master(session_id, full_summary, cfg, session_mgr)

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
      await broadcast_and_persist(session_id, fallback_event, session_mgr)
    except Exception as inner:
      log.error("fallback_notify_failed", thread_id=thread.id, error=str(inner), traceback=traceback.format_exc())


async def _read_events_summary(session_id: str, thread_id: str, thread_mgr: ThreadManager, max_lines: int = 80) -> str:
  """Read the last N lines from a thread's events.jsonl for summarization."""
  events_path = await thread_mgr.get_events_log_path(session_id, thread_id)
  events = parse_ndjson_file(events_path)
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
  if ev_type == "result":
    return str(ev.get("result", ""))[:500]

  if ev_type == "assistant":
    msg = ev.get("message", {})
    blocks = msg.get("content", []) if isinstance(msg, dict) else []
    texts = []
    for block in blocks if isinstance(blocks, list) else []:
      if isinstance(block, dict):
        if block.get("type") == "text":
          texts.append(block.get("text", ""))
        elif block.get("type") == "tool_use":
          texts.append(f"[tool_use: {block.get('name', '?')}]")
    return " ".join(texts)[:300] if texts else ""

  if ev_type == "rate_limit_event":
    rli = ev.get("rate_limit_info", {})
    status = rli.get("status", "unknown")
    rate_type = rli.get("rateLimitType", "unknown")
    return f"Rate limit {status} ({rate_type})"

  if ev_type in ("thinking", "error", "complete", "tool_result", "tool_use", "file_write"):
    content = ev.get("content", ev.get("message", ""))
    if isinstance(content, list):
      texts = [b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") == "text"]
      return " ".join(texts)[:200] if texts else ""
    return str(content)[:200]

  return ""
