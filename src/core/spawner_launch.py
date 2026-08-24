"""Worker construction — worktree creation, prompt build, backend resolution, Worker setup."""

import time
import uuid
from pathlib import Path

import structlog

from src.agents.worker import Worker
from src.core import spawner_backends, spawner_prompt
from src.core.config import CharlieBotConfig
from src.core.git import (
  git_create_worktree,
  git_remote_default_branch,
  git_worktree_dir_name,
)
from src.core.models import (
  SpawnRequest,
  TaskType,
  ThreadMetadata,
)
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.verify_trailer import VERIFY_RESULT_TRAILER_EXPECTED

log = structlog.get_logger()


async def _construct_worker(
    session_id: str,
    thread: ThreadMetadata,
    working_dir: Path,
    worker_prompt: str,
    cfg: CharlieBotConfig,
    thread_mgr: ThreadManager,
    request: SpawnRequest,
) -> Worker:
  """Resolve the request's backend option onto the thread, persist it, and build its Worker."""
  backend_option = spawner_backends.resolve_backend_option(cfg, request.resolved_backend, request.resolved_model)
  thread.backend = backend_option.id
  thread.model = backend_option.model
  if request.task_type == TaskType.VERIFY and backend_option.id not in thread.tried_backends:
    thread.tried_backends.append(backend_option.id)
  if backend_option.type == "cc-claude" and thread.claude_session_id is None:
    thread.claude_session_id = str(uuid.uuid4())
  await thread_mgr.save_metadata(thread)
  events_log = await thread_mgr.get_events_log_path(session_id, thread.id)
  return Worker(
      thread,
      working_dir,
      events_log,
      worker_prompt,
      cfg,
      backend_option=backend_option,
      on_spawned=thread_mgr.save_metadata,
  )


async def _create_worktree_and_process(
    session_id: str,
    thread: ThreadMetadata,
    description: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
    resolved_repo: Path,
    request: SpawnRequest,
) -> Worker:
  """Create worktree, build prompt, resolve backend, and construct Worker."""
  if request.prompt_override:
    worker_prompt = request.prompt_override
    if not thread.worktree_path:
      log.error(
          "spawn_worker_missing_worktree_path",
          session=session_id,
          thread_id=thread.id,
          detail="prompt_override requires persisted worktree_path",
      )
      raise RuntimeError("thread metadata missing worktree_path")
    worktree_path = Path(thread.worktree_path).resolve()
  else:
    # An unattended launch has nobody present to reconcile a local/remote
    # disagreement, so a request that names no base starts from the remote's
    # published default branch; the local checkout's refs never enter the decision.
    base_branch = request.base_branch or f"origin/{await git_remote_default_branch(resolved_repo)}"

    branch_name = request.branch_name_override or f"charliebot/task-{int(time.time())}-{thread.id[:8]}"

    canonical_branch = base_branch
    is_continuation = request.is_continuation
    start_point: str | None = None

    if request.worktree_path_override:
      # Reuse an existing worktree (e.g. improve loop iterations sharing a single worktree).
      wt_path = Path(request.worktree_path_override)
      thread.skip_cleanup = request.skip_cleanup
    else:
      wt_path = Path(cfg.worktree_dir) / git_worktree_dir_name(branch_name)

      Path(cfg.worktree_dir).mkdir(parents=True, exist_ok=True)
      resolution = await git_create_worktree(resolved_repo, base_branch, branch_name, wt_path)
      canonical_branch = resolution.canonical
      is_continuation = False
      start_point = resolution.start_point

    thread.branch_name = branch_name
    thread.repo_path = str(resolved_repo)
    thread.worktree_path = str(wt_path)
    thread.base_branch = canonical_branch
    thread.keep_worktree = request.keep_worktree
    thread.context = request.context

    session_meta = await spawner_backends._require_session(session_mgr, session_id)
    worker_prompt = spawner_prompt._build_worker_prompt(
        description,
        resolved_repo,
        canonical_branch,
        branch_name,
        str(wt_path),
        session_meta,
        cfg,
        task_type=request.task_type,
        loop_dir=request.loop_dir,
        iteration_number=request.iteration_number,
        is_continuation=is_continuation,
        keep_worktree=request.keep_worktree,
        start_point=start_point)
    worktree_path = wt_path.resolve()

  if worktree_path == resolved_repo:
    log.error(
        "spawn_worker_repo_root_cwd_detected",
        session=session_id,
        thread_id=thread.id,
        repo=str(resolved_repo),
        worktree=str(worktree_path),
    )
    raise RuntimeError("refusing to run subagent in repo root; worktree isolation required")

  return await _construct_worker(
      session_id, thread, worktree_path, worker_prompt, cfg, thread_mgr, request)


async def _create_repoless_process(
    session_id: str,
    thread: ThreadMetadata,
    description: str,
    cfg: CharlieBotConfig,
    thread_mgr: ThreadManager,
    request: SpawnRequest,
) -> Worker:
  """Create a repo-less worker for prompt-only tasks (no worktree, no git)."""
  if request.task_type == TaskType.VERIFY:
    sections = spawner_prompt._load_prompt_sections(
        cfg.charlie_bot_repo / "prompts" / "verify.md",
        spawner_prompt._REQUIRED_VERIFY_PROMPT_SECTIONS,
        extraction="verify-prompt")
    contract = spawner_prompt._substitute_tokens(
        "\n".join(sections[section_id].strip("\n") for section_id in spawner_prompt._REQUIRED_VERIFY_PROMPT_SECTIONS), {
            "{{result_trailer_expected}}": VERIFY_RESULT_TRAILER_EXPECTED,
            "{{canonical_template_path}}": str((cfg.charlie_bot_repo / "prompts" / "plan_template.html").resolve()),
        })
    spawner_prompt._require_tokens_resolved(contract, prompt="verify")
    worker_prompt = f"{contract}\n\n{description}"
  elif request.task_type in spawner_prompt._WORKFLOW_PROMPT_SECTION:
    worker_prompt = request.prompt_override or description
  else:
    raise ValueError(f"unsupported task_type: {request.task_type!r}")
  thread_dir = thread_mgr.thread_dir(session_id, thread.id)

  # Repo-less tasks cannot produce branch/worktree review artifacts.
  thread.branch_name = None
  thread.repo_path = None
  thread.worktree_path = str(thread_dir)
  thread.require_review = False
  thread.context = request.context

  return await _construct_worker(session_id, thread, thread_dir, worker_prompt, cfg, thread_mgr, request)
