"""Review pipeline — builds prompts, selects backends, and spawns review workers."""

import asyncio
from dataclasses import dataclass
from pathlib import Path

import structlog

from src.core import event_types as ET
from src.core import finalize_effects
from src.core.chat_events import chat_events_path
from src.core.config import CharlieBotConfig
from src.core.git import (
  git_current_branch,
  git_worktree_dir_name,
  git_worktree_prune,
  git_worktree_remove,
)
from src.core.master_trigger import trigger_master
from src.core.message_aggregator import extract_text_from_message
from src.core.models import (
  BackendOption,
  SpawnRequest,
  ThreadMetadata,
  backend_type_allows_missing_model,
)
from src.core.ndjson import parse_ndjson_file
from src.core.sessions import SessionManager
from src.core.tasks import create_logged_task
from src.core.threads import ThreadManager, thread_events_log_path

log = structlog.get_logger()


async def _trigger_master_judged(
    session_id: str,
    summary: str,
    thread_id: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
) -> None:
  """trigger_master behind the wake idempotency judgment.

  A rerun of the finalize chain (startup reconcile completing a crashed
  finalize) must not wake the master twice for the same worker summary. The
  judgment is effect-keyed: any master output event AFTER this thread's last
  terminal summary means "woke" — a hard kill between summary persist and the
  trigger call therefore still wakes (summary alone is not the key), and a
  rerun after a successful wake skips.
  """
  chat_events = session_mgr.load_chat_events_sync(session_id)
  if finalize_effects.master_woke_after_summary(chat_events, thread_id):
    log.info("master_wake_skip_already_woke", session=session_id, thread=thread_id)
    return
  await trigger_master(session_id, summary, cfg, session_mgr)


async def finalize_review_chain(
    session_id: str,
    original_thread: ThreadMetadata,
    worktree_parent: Path,
) -> str | None:
  """Idempotently remove the worktree shared by the original worker + its reviewer(s).

  Runs only on the review success path. Returns an error message when the cleanup
  fails so the caller (which holds session_mgr) can broadcast it; returns None on
  success or when there is nothing to clean.
  """
  if not original_thread.repo_path or not original_thread.worktree_path:
    return None
  if original_thread.keep_worktree:
    return None
  wt = Path(original_thread.worktree_path)
  if not wt.exists():
    return None
  if not original_thread.branch_name:
    raise RuntimeError(f"thread {original_thread.id} has worktree_path but no branch_name")
  try:
    removed = await git_worktree_remove(
        original_thread.repo_path,
        wt,
        original_thread.id,
        allowed_parent=worktree_parent,
        expected_residue_name=git_worktree_dir_name(original_thread.branch_name),
    )
  except Exception as e:
    log.exception(
        "review_chain_cleanup_failed",
        session=session_id,
        thread_id=original_thread.id,
        worktree=str(wt),
        error=str(e))
    return f"Review worktree cleanup failed for {wt}: {e}"
  if not removed:
    log.error("review_chain_cleanup_remove_failed", session=session_id, thread_id=original_thread.id, worktree=str(wt))
    return f"Review worktree cleanup failed for {wt}: git worktree remove reported failure"
  await git_worktree_prune(original_thread.repo_path, original_thread.id)
  return None


def build_review_prompt(
    branch_name: str,
    wt_path: str,
    base_branch: str,
    cfg: CharlieBotConfig,
    session_id: str,
    original_thread_id: str,
    sessions_dir: Path,
    context: str | None = None,
    user_request: str | None = None,
    worker_summary: str | None = None,
) -> str:
  """Build the prompt for a review worker."""
  from src.core.spawner import load_worker_prompt_sections

  coding_principles = load_worker_prompt_sections(cfg)["coding_principles"]

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
  session_dir = sessions_dir / session_id
  chat_log_path = chat_events_path(session_dir)
  worker_log_path = thread_events_log_path(session_dir, original_thread_id)
  return (
      f"## Code Review\n"
      f"You are reviewing another worker's code changes.\n\n"
      f"## Context\n"
      f"{context_section}\n\n"
      f"If the summary above is insufficient or you are unsure about intent, "
      f"read the full logs: Session: `{chat_log_path}`, Worker: `{worker_log_path}`\n\n"
      f"{coding_principles}\n"
      f"## Review Checklist\n"
      f"IMPORTANT: Make minimal changes. Prefer approving the worker's code as-is. "
      f"Only fix clear bugs, correctness issues, or scope violations. "
      f"Do not refactor, restyle, or improve code that is functionally correct.\n\n"
      f"If the user request contains task spec sections, read every path listed under `## Source Files` "
      f"before judging the diff. Apply the task spec's `## Reviewer Checklist`. For control-flow or "
      f"state-machine tasks, verify the implementation against `## Required Behavior`; do not rely only "
      f"on tests.\n\n"
      f"The work is on branch `{branch_name}` in worktree `{wt_path}`. All git operations below run from the worktree.\n\n"
      f"1. `cd {wt_path}`\n"
      f"2. Fetch the latest base branch: `git fetch origin {base_branch}`\n"
      f"3. Review the changes: `git diff origin/{base_branch}...{branch_name}`\n"
      f"4. Verify the changes address the user's actual intent (from context research above).\n"
      f"5. **Scope check**: Flag any changes NOT requested in the task — extra flags, altered defaults,\n"
      f"   new parameters, behavioral changes. Workers must only do what was asked.\n"
      f"6. **Think divergently**: Beyond the diff, consider what could go wrong.\n"
      f"   - Do changed values make sense? Cross-check against existing defaults and conventions.\n"
      f"   - Are there edge cases, regressions, or interactions with other code the worker missed?\n"
      f"   - Would this change surprise someone reading the code for the first time?\n"
      f"7. Check for: correctness, bugs, unintended side effects, missing edge cases.\n"
      f"8. Style: Google Style, 2-space indent, 120-col (only flag if egregious — YAPF handles most).\n"
      f"9. If you find issues, fix them and commit with descriptive messages.\n"
      f"10. Stash untracked/modified files: `git stash --include-untracked`\n"
      f"11. Fetch the latest base branch: `git fetch origin {base_branch}`\n"
      f"12. Rebase onto the remote base: `git rebase origin/{base_branch}`\n"
      f"13. Push to remote base branch from the worktree: `git push origin HEAD:{base_branch}`\n"
      f"14. Verify: `git log --oneline -1 HEAD` and `git log --oneline -1 origin/{base_branch}` must show the same commit."
  )


async def extract_review_context(
    session_id: str,
    thread_id: str,
    sessions_dir: Path,
) -> tuple[str | None, str | None]:
  """Extract user request and worker summary from JSONL logs for review context.

  Returns (user_request, worker_summary); each may independently be None when extraction fails.
  The caller's prompt builder handles partial context.
  """
  user_request: str | None = None
  worker_summary: str | None = None
  has_user_request = False
  has_worker_summary = False
  session_dir = sessions_dir / session_id

  try:
    chat_log = chat_events_path(session_dir)
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
    worker_log = thread_events_log_path(session_dir, thread_id)
    events = await asyncio.to_thread(parse_ndjson_file, worker_log)
    result_text: str | None = None
    assistant_text: str | None = None
    for ev in reversed(events):
      ev_type = ev.get("type")
      if result_text is None and ev_type == ET.RESULT:
        val = ev.get("result")
        if isinstance(val, str):
          stripped = val.strip()
          if stripped:
            result_text = stripped
            break
        # empty / non-string: fall through to look for assistant text
        continue
      if assistant_text is None and ev_type == ET.ASSISTANT:
        msg = ev.get("message") if isinstance(ev.get("message"), dict) else None
        text = extract_text_from_message(msg).strip()
        if text:
          assistant_text = text
          break
    chosen = result_text or assistant_text
    if chosen:
      worker_summary = chosen
      has_worker_summary = True
  except Exception as e:
    log.warning("review_context_worker_events_failed", session=session_id, thread=thread_id, error=str(e))
  if not has_worker_summary:
    log.warning("review_context_worker_summary_unavailable", session=session_id, thread=thread_id)

  return user_request, worker_summary


def validate_review_prerequisites(
    original_thread: ThreadMetadata,
    session_id: str,
) -> tuple[Path, str, str] | None:
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
  if not Path(original_thread.worktree_path).is_dir():
    log.error(
        "spawn_review_worktree_missing",
        session=session_id,
        thread=original_thread.id,
        worktree=original_thread.worktree_path)
    return None
  return Path(original_thread.repo_path), original_thread.branch_name, original_thread.worktree_path


def _resolve_preference_option(cfg: CharlieBotConfig, option_id: str) -> BackendOption:
  """Resolve a model_preference entry to its BackendOption with default model.

  Raises ValueError if the option_id is not in backend_options or requires but lacks a model.
  """
  option = cfg.get_backend_option(option_id)
  if option is None:
    raise ValueError(f"model_preference entry '{option_id}' not in backend_options")
  if backend_type_allows_missing_model(option.type):
    return option.model_copy(update={"model": None})
  if not option.model:
    raise ValueError(f"model_preference entry '{option_id}' has no default model")
  return option


def select_reviewer_backend(
    cfg: CharlieBotConfig,
    worker_backend: str,
    worker_model: str | None,
    tried_backends: list[str],
) -> tuple[str, str | None, list[str]] | None:
  """Select a checking-role backend (reviewer, verify default) via model_preference, skipping already-tried backends.

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

  return resolved_backend, resolved_model, [*tried_backends, resolved_backend]


@dataclass(frozen=True)
class ReviewSpawnContext:
  """Resolved inputs needed to spawn a review worker for an original thread."""
  repo_path: Path
  branch_name: str
  wt_path: str
  base_branch: str
  resolved_backend: str
  resolved_model: str | None
  tried_backends: list[str]


async def _resolve_review_spawn_context(
    original_thread: ThreadMetadata,
    cfg: CharlieBotConfig,
    session_id: str,
    tried_backends: list[str] | None,
) -> ReviewSpawnContext | None:
  """Decide whether a reviewer can be spawned and resolve every input the spawn-side needs.

  Returns a `ReviewSpawnContext` on success, or None to short-circuit (retries
  exceeded, prerequisites missing, or all backends exhausted).
  """
  from src.core.spawner import require_thread_backend_model

  tried_backends = list(tried_backends) if tried_backends is not None else []

  # Max retries guard: at most len(model_preference) retries after the initial spawn.
  if len(tried_backends) > len(cfg.model_preference):
    log.warning("reviewer_max_retries_exceeded", tried=tried_backends, max=len(cfg.model_preference))
    return None

  prerequisites = validate_review_prerequisites(original_thread, session_id)
  if prerequisites is None:
    return None
  repo_path, branch_name, wt_path = prerequisites

  base_branch = original_thread.base_branch or await git_current_branch(repo_path)
  worker_backend, worker_model = require_thread_backend_model(original_thread, cfg)

  backend_result = select_reviewer_backend(cfg, worker_backend, worker_model, tried_backends)
  if backend_result is None:
    return None
  resolved_backend, resolved_model, tried_backends = backend_result

  return ReviewSpawnContext(
      repo_path=repo_path,
      branch_name=branch_name,
      wt_path=wt_path,
      base_branch=base_branch,
      resolved_backend=resolved_backend,
      resolved_model=resolved_model,
      tried_backends=tried_backends,
  )


def _short_desc(description: str) -> str:
  """First line of description, truncated to 120 chars."""
  first_line = description.split("\n", 1)[0].strip()
  if len(first_line) > 120:
    return first_line[:120] + "..."
  return first_line


async def spawn_review_worker(
    session_id: str,
    original_thread: ThreadMetadata,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
    tried_backends: list[str] | None = None,
    exclude_thread_id: str | None = None,
) -> bool:
  """Spawn a review worker for a completed worker's branch.

  Returns True if a reviewer now exists (spawned here or already present),
  False if all backends are exhausted.

  ``exclude_thread_id`` names the thread whose finalize is running: on the
  failed-reviewer retry path the failed reviewer itself matches the
  reviewer-exists judgment and must not block its own replacement.
  """
  from src.core.spawner import spawn_worker

  # Idempotency judgment: never derive a second reviewer for the same original
  # thread, so the merge happens exactly once (the reviewer pushes; the server
  # only removes the worktree, so no merge-side dedupe key is needed).
  session_threads = await thread_mgr.list_threads(session_id)
  if finalize_effects.reviewer_thread_exists(
      session_threads, original_thread.id, exclude_thread_id=exclude_thread_id):
    log.info(
        "reviewer_skip_already_exists",
        session=session_id,
        original_thread=original_thread.id,
        exclude=exclude_thread_id,
    )
    return True

  ctx = await _resolve_review_spawn_context(original_thread, cfg, session_id, tried_backends)
  if ctx is None:
    return False

  user_request, worker_summary = await extract_review_context(session_id, original_thread.id, cfg.sessions_dir)
  review_prompt = build_review_prompt(
      ctx.branch_name,
      ctx.wt_path,
      ctx.base_branch,
      cfg=cfg,
      session_id=session_id,
      original_thread_id=original_thread.id,
      sessions_dir=cfg.sessions_dir,
      context=original_thread.context,
      user_request=user_request,
      worker_summary=worker_summary)

  session_meta = await session_mgr.get_session(session_id)
  if session_meta is None:
    log.warning("review_session_missing", session=session_id)
    return False
  review_thread = await thread_mgr.create_thread(
      session_meta,
      f"Review: {original_thread.context or _short_desc(original_thread.description)}",
      review_of=original_thread.id,
  )
  review_thread.branch_name = ctx.branch_name
  review_thread.repo_path = str(ctx.repo_path)
  review_thread.worktree_path = ctx.wt_path
  review_thread.tried_backends = ctx.tried_backends
  await thread_mgr.save_metadata(review_thread)

  request = SpawnRequest(
      repo_path=str(ctx.repo_path),
      prompt_override=review_prompt,
      resolved_backend=ctx.resolved_backend,
      resolved_model=ctx.resolved_model,
  )
  create_logged_task(
      spawn_worker(
          session_id, review_thread.description, review_thread.id, cfg, session_mgr, thread_mgr, request=request))
  return True


async def _retry_failed_reviewer(
    session_id: str,
    thread_meta: ThreadMetadata,
    original_thread: ThreadMetadata | None,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
) -> bool:
  """Retry a failed reviewer on the next untried backend; finalize chain if exhausted.

  Returns True iff a fresh reviewer was spawned (caller should skip the trailing
  combined-summary trigger). Returns False when no original_thread is recoverable
  or when all backends have been exhausted.
  """
  if not original_thread:
    log.warning(
        "reviewer_retry_original_not_found",
        session=session_id,
        review_of=thread_meta.review_of,
    )
    return False
  retried = await spawn_review_worker(
      session_id,
      original_thread,
      cfg,
      session_mgr,
      thread_mgr,
      tried_backends=list(thread_meta.tried_backends),
      exclude_thread_id=thread_meta.id,
  )
  if retried:
    log.info(
        "reviewer_retry_spawned",
        session=session_id,
        failed_review=thread_meta.id,
        tried=thread_meta.tried_backends,
    )
    return True
  # Reviewer failed and all backends are exhausted: keep the shared worktree so the
  # failed review's state survives for debugging. The startup quarantine sweep reclaims
  # it later once it ages out.
  log.info(
      "reviewer_retries_exhausted",
      session=session_id,
      failed_review=thread_meta.id,
      tried=thread_meta.tried_backends,
  )
  return False


async def maybe_spawn_reviewer(
    session_id: str,
    thread: ThreadMetadata,
    exit_code: int,
    events_summary: str,
    full_summary: str,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    cfg: CharlieBotConfig,
) -> None:
  """Handle review spawning logic and trigger master when appropriate."""
  from src.core.spawner import read_events_summary

  # Re-read thread metadata to get review_of field
  thread_meta = await thread_mgr.get_thread(session_id, thread.id)

  if exit_code == 0 and not thread_meta.review_of:
    if not thread_meta.require_review:
      # No review needed — trigger master directly
      await _trigger_master_judged(session_id, full_summary, thread_meta.id, cfg, session_mgr)
      return
    # Successful worker needing review -> spawn reviewer
    await spawn_review_worker(
        session_id, thread_meta, cfg, session_mgr, thread_mgr, exclude_thread_id=thread_meta.id)
    return

  if thread_meta.review_of:
    # This IS a reviewer thread.
    original_thread = await thread_mgr.get_thread(session_id, thread_meta.review_of)
    if exit_code != 0:
      retried = await _retry_failed_reviewer(session_id, thread_meta, original_thread, cfg, session_mgr, thread_mgr)
      if retried:
        return

    # Review done (success or retries exhausted) -> combine summaries, trigger master.
    original_events = await read_events_summary(session_id, thread_meta.review_of, thread_mgr)
    combined = f"**Original worker result:**\n{original_events}\n\n**Review result:**\n{events_summary}"
    await _trigger_master_judged(session_id, combined, thread_meta.id, cfg, session_mgr)
    if exit_code == 0 and original_thread:
      cleanup_error = await finalize_review_chain(session_id, original_thread, Path(cfg.worktree_dir))
      if cleanup_error:
        await session_mgr.deliver_to_successor(session_id, {"type": ET.ERROR, "content": cleanup_error})
    return

  # Failed/cancelled worker -> trigger master immediately
  await _trigger_master_judged(session_id, full_summary, thread_meta.id, cfg, session_mgr)
