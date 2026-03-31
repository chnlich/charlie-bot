"""Review pipeline — builds prompts, selects backends, and spawns review workers."""

import asyncio
from pathlib import Path
from typing import Optional

import structlog

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, ThreadMetadata
from src.core.ndjson import parse_ndjson_file
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.tasks import create_logged_task

log = structlog.get_logger()


def build_review_prompt(
    branch_name: str,
    wt_path: str,
    repo_path: Path,
    base_branch: str,
    session_id: str,
    original_thread_id: str,
    sessions_dir: Path,
    context: Optional[str] = None,
    user_request: Optional[str] = None,
    worker_summary: Optional[str] = None,
) -> str:
  """Build the prompt for a review worker."""
  from src.core.spawner import _CODING_PRINCIPLES

  context_hint = context or '(none provided)'
  context_lines: list[str] = []
  if user_request or worker_summary:
    if user_request:
      context_lines.append(f"**User request:** {user_request}")
    if worker_summary:
      context_lines.append(f"**Worker summary:** {worker_summary}")
  else:
    context_lines.append("*(Log extraction unavailable — review based on delegator hint and diff only.)*")
  context_lines.append(f"**Delegator hint:** {context_hint}")
  context_section = "\n".join(context_lines)
  chat_log_path = sessions_dir / session_id / "data" / "chat_events.jsonl"
  worker_log_path = sessions_dir / session_id / "threads" / original_thread_id / "data" / "events.jsonl"
  return (
      f"## Code Review\n"
      f"You are reviewing another worker's code changes.\n\n"
      f"## Context\n"
      f"{context_section}\n\n"
      f"If the summary above is insufficient or you are unsure about intent, "
      f"read the full logs: Session: `{chat_log_path}`, Worker: `{worker_log_path}`\n\n"
      f"{_CODING_PRINCIPLES}\n"
      f"## Review Checklist\n"
      f"IMPORTANT: Make minimal changes. Prefer approving the worker's code as-is. "
      f"Only fix clear bugs, correctness issues, or scope violations. "
      f"Do not refactor, restyle, or improve code that is functionally correct.\n\n"
      f"The work is on branch `{branch_name}` in worktree `{wt_path}`.\n\n"
      f"IMPORTANT: NEVER run git push from the worktree. All pushes must happen from `{repo_path}` after merging.\n\n"
      f"1. `cd {wt_path}`\n"
      f"2. Review the changes: `git diff {base_branch}...{branch_name}`\n"
      f"3. Verify the changes address the user's actual intent (from context research above).\n"
      f"4. **Scope check**: Flag any changes NOT requested in the task — extra flags, altered defaults,\n"
      f"   new parameters, behavioral changes. Workers must only do what was asked.\n"
      f"5. **Think divergently**: Beyond the diff, consider what could go wrong.\n"
      f"   - Do changed values make sense? Cross-check against existing defaults and conventions.\n"
      f"   - Are there edge cases, regressions, or interactions with other code the worker missed?\n"
      f"   - Would this change surprise someone reading the code for the first time?\n"
      f"6. Check for: correctness, bugs, unintended side effects, missing edge cases.\n"
      f"7. Style: Google Style, 2-space indent, 120-col (only flag if egregious — YAPF handles most).\n"
      f"8. If you find issues, fix them and commit with descriptive messages.\n"
      f"9. Stash untracked/modified files: `git stash --include-untracked`\n"
      f"10. Rebase onto base branch: `git rebase {base_branch}`\n"
      f"11. Merge: `cd {repo_path} && git merge --ff-only {branch_name}`\n"
      f"12. Push to remote: `cd {repo_path} && git pull --ff-only && git push origin {base_branch}`\n"
      f"13. Verify: `git log --oneline -1 {base_branch}` and `git log --oneline -1 origin/{base_branch}` must show the same commit."
  )


async def extract_review_context(
    session_id: str,
    thread_id: str,
    sessions_dir: Path,
) -> tuple[Optional[str], Optional[str]]:
  """Extract user request and worker summary from JSONL logs for review context.

  Returns (user_request, worker_summary). Falls back to (None, None) if extraction is incomplete.
  """
  user_request: Optional[str] = None
  worker_summary: Optional[str] = None
  has_user_request = False
  has_worker_summary = False

  try:
    chat_log = sessions_dir / session_id / "data" / "chat_events.jsonl"
    events = await asyncio.to_thread(parse_ndjson_file, chat_log)
    for ev in events:
      if ev.get("type") == ET.TASK_DELEGATED and ev.get("thread_id") == thread_id:
        user_request_value = ev.get("description")
        if isinstance(user_request_value, str):
          normalized_request = user_request_value.strip()
          if normalized_request:
            user_request = normalized_request
            has_user_request = True
        break
  except Exception as e:
    log.warning("review_context_chat_events_failed", session=session_id, thread=thread_id, error=str(e))
  if not has_user_request:
    log.warning("review_context_user_request_unavailable", session=session_id, thread=thread_id)

  try:
    worker_log = sessions_dir / session_id / "threads" / thread_id / "data" / "events.jsonl"
    events = await asyncio.to_thread(parse_ndjson_file, worker_log)
    for ev in reversed(events):
      if ev.get("type") == ET.RESULT:
        worker_summary_value = ev.get("result")
        if isinstance(worker_summary_value, str):
          normalized_summary = worker_summary_value.strip()
          if normalized_summary:
            worker_summary = normalized_summary
            has_worker_summary = True
        break
  except Exception as e:
    log.warning("review_context_worker_events_failed", session=session_id, thread=thread_id, error=str(e))
  if not has_worker_summary:
    log.warning("review_context_worker_summary_unavailable", session=session_id, thread=thread_id)

  if not has_user_request or not has_worker_summary:
    return None, None

  return user_request, worker_summary


def validate_review_prerequisites(
    original_thread: ThreadMetadata,
    session_id: str,
) -> Optional[tuple[Path, str, str]]:
  """Validate that original_thread has repo_path, branch_name, and worktree_path.

  Returns (repo_path, branch_name, worktree_path) or None if validation fails.
  """
  if not original_thread.repo_path:
    log.warning(
        "spawn_review_no_repo_path",
        session=session_id,
        thread=original_thread.id,
        detail="original thread missing repo_path (repoless worker — review not applicable)")
    return None
  if not original_thread.branch_name:
    log.error(
        "spawn_review_no_branch_name",
        session=session_id,
        thread=original_thread.id,
        detail="original thread missing branch_name")
    return None
  if not original_thread.worktree_path:
    log.error(
        "spawn_review_no_worktree_path",
        session=session_id,
        thread=original_thread.id,
        detail="original thread missing worktree_path")
    return None
  return Path(original_thread.repo_path), original_thread.branch_name, original_thread.worktree_path


def _resolve_preference_option(cfg: CharlieBotConfig, option_id: str) -> BackendOption:
  """Resolve a model_preference entry to its BackendOption with default model.

  Raises ValueError if the option_id is not in backend_options or has no model.
  """
  option = cfg.get_backend_option(option_id)
  if option is None:
    raise ValueError(f"model_preference entry '{option_id}' not in backend_options")
  if not option.model:
    raise ValueError(f"model_preference entry '{option_id}' has no default model")
  return option


def select_reviewer_backend(
    cfg: CharlieBotConfig,
    worker_backend: str,
    worker_model: str,
    tried_backends: list[str],
) -> Optional[tuple[str, str, list[str]]]:
  """Select a reviewer backend via model_preference, skipping already-tried backends.

  Returns (resolved_backend, resolved_model, updated_tried_backends) or None if exhausted.
  """
  resolved_backend, resolved_model = worker_backend, worker_model

  for pref_id in cfg.model_preference:
    if pref_id == worker_backend:
      log.debug("reviewer_skip_same_backend", preference=pref_id)
      continue
    if pref_id in tried_backends:
      log.debug("reviewer_skip_tried_backend", preference=pref_id)
      continue
    try:
      pref_option = _resolve_preference_option(cfg, pref_id)
      log.info(
          "reviewer_backend_selected",
          preference=pref_id,
          worker_backend=worker_backend,
          reviewer_model=pref_option.model,
          retry_attempt=len(tried_backends),
      )
      resolved_backend = pref_option.id
      resolved_model = pref_option.model
      break
    except Exception as e:
      log.warning("reviewer_preference_failed", preference=pref_id, error=str(e))
  else:
    if cfg.model_preference:
      if worker_backend not in tried_backends:
        log.info("reviewer_fallback_to_worker_backend", worker_backend=worker_backend, tried=tried_backends)
      else:
        log.warning("reviewer_all_backends_exhausted", tried=tried_backends)
        return None

  return resolved_backend, resolved_model, tried_backends + [resolved_backend]


async def spawn_review_worker(
    session_id: str,
    original_thread,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
    tried_backends: Optional[list[str]] = None,
) -> bool:
  """Spawn a review worker for a completed worker's branch.

  Returns True if a reviewer was spawned, False if all backends are exhausted.
  """
  from src.core.spawner import (
      _git_current_branch,
      _require_thread_backend_model,
      _short_desc,
      spawn_worker,
  )

  if tried_backends is None:
    tried_backends = []

  # Max retries guard: at most len(model_preference) retries after the initial spawn.
  if len(tried_backends) > len(cfg.model_preference):
    log.warning("reviewer_max_retries_exceeded", tried=tried_backends, max=len(cfg.model_preference))
    return False

  prerequisites = validate_review_prerequisites(original_thread, session_id)
  if prerequisites is None:
    return False
  repo_path, branch_name, wt_path = prerequisites

  base_branch = original_thread.base_branch or await _git_current_branch(repo_path)
  worker_backend, worker_model = _require_thread_backend_model(original_thread)

  backend_result = select_reviewer_backend(cfg, worker_backend, worker_model, tried_backends)
  if backend_result is None:
    return False
  resolved_backend, resolved_model, tried_backends = backend_result

  user_request, worker_summary = await extract_review_context(session_id, original_thread.id, cfg.sessions_dir)

  review_prompt = build_review_prompt(
      branch_name,
      wt_path,
      repo_path,
      base_branch,
      session_id=session_id,
      original_thread_id=original_thread.id,
      sessions_dir=cfg.sessions_dir,
      context=original_thread.context,
      user_request=user_request,
      worker_summary=worker_summary)

  session_meta = await session_mgr.get_session(session_id)
  review_thread = await thread_mgr.create_thread(
      session_meta,
      f"Review: {original_thread.context or _short_desc(original_thread.description)}",
      review_of=original_thread.id,
  )
  review_thread.branch_name = branch_name
  review_thread.repo_path = str(repo_path)
  review_thread.worktree_path = wt_path
  review_thread.tried_backends = tried_backends
  await thread_mgr.save_metadata(review_thread)

  create_logged_task(
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
  return True


async def maybe_spawn_reviewer(
    session_id: str,
    thread,
    exit_code: int,
    events_summary: str,
    full_summary: str,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    cfg: CharlieBotConfig,
) -> None:
  """Handle review spawning logic and trigger master when appropriate."""
  from src.core.spawner import _read_events_summary, _trigger_master

  # Re-read thread metadata to get review_of field
  thread_meta = await thread_mgr.get_thread(session_id, thread.id)

  if exit_code == 0 and not thread_meta.review_of:
    if not thread_meta.require_review:
      # No review needed — trigger master directly
      await _trigger_master(session_id, full_summary, cfg, session_mgr)
      return
    # Successful worker needing review -> spawn reviewer
    await spawn_review_worker(session_id, thread_meta, cfg, session_mgr, thread_mgr)
    return

  if thread_meta.review_of:
    # This IS a reviewer thread.
    if exit_code != 0:
      # Reviewer failed — attempt retry with next untried backend.
      original_thread = await thread_mgr.get_thread(session_id, thread_meta.review_of)
      if original_thread:
        retried = await spawn_review_worker(
            session_id,
            original_thread,
            cfg,
            session_mgr,
            thread_mgr,
            tried_backends=list(thread_meta.tried_backends),
        )
        if retried:
          log.info(
              "reviewer_retry_spawned",
              session=session_id,
              failed_review=thread_meta.id,
              tried=thread_meta.tried_backends,
          )
          return
        log.info(
            "reviewer_retries_exhausted",
            session=session_id,
            failed_review=thread_meta.id,
            tried=thread_meta.tried_backends,
        )
      else:
        log.warning(
            "reviewer_retry_original_not_found",
            session=session_id,
            review_of=thread_meta.review_of,
        )

    # Review done (success or retries exhausted) -> combine summaries, trigger master.
    original_events = await _read_events_summary(session_id, thread_meta.review_of, thread_mgr)
    combined = f"**Original worker result:**\n{original_events}\n\n**Review result:**\n{events_summary}"
    await _trigger_master(session_id, combined, cfg, session_mgr)
    return

  # Failed/cancelled worker -> trigger master immediately
  await _trigger_master(session_id, full_summary, cfg, session_mgr)
