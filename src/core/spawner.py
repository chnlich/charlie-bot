"""Direct worker spawner — creates a task, enriches the prompt, and runs the worker."""

import asyncio
import shlex
import signal
import time
import traceback
import uuid
from collections.abc import Awaitable, Callable
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional
from zoneinfo import ZoneInfo

import structlog

from src.agents.backends.claude_code import BASE_COMMAND, ClaudeCodeBackend
from src.agents.worker import QuotaExhaustedException, Worker
from src.api.message_utils import extract_text_from_message
from src.core import event_types as ET
from src.core import finalize_effects, review, runs
from src.core.config import CharlieBotConfig, get_scheduled_tasks
from src.core.git import (
  git_create_worktree,
  git_remote_default_branch,
  git_worktree_dir_name,
  git_worktree_prune,
  git_worktree_remove,
)
from src.core.memory import assemble_worker
from src.core.models import (
  BackendOption,
  SessionMetadata,
  SpawnRequest,
  TaskType,
  ThreadMetadata,
  ThreadStatus,
  backend_type_allows_missing_model,
)
from src.core.ndjson import parse_ndjson_file
from src.core.notifications import send_telegram
from src.core.process import kill_process_group
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.verify_trailer import (
  VERIFY_RESULT_TRAILER_EXPECTED,
  read_verify_final_report,
  verify_result_trailer_error,
)

log = structlog.get_logger()

_PROMPT_SECTION_MARKER_PREFIX = "<!-- section: "
_PROMPT_SECTION_MARKER_SUFFIX = " -->"

_REQUIRED_WORKER_PROMPT_SECTIONS = (
    "session_info",
    "coding_principles",
    "skills_discovery",
    "role",
    "intro_new",
    "intro_continuation",
    "worktree_workflow_header",
    "workflow_implement",
    "workflow_quick_edit",
    "workflow_script_run",
    "task_spec_source_files",
    "task",
    "iteration_reports",
    "worktree_persistence",
    "memory",
)

_REQUIRED_VERIFY_PROMPT_SECTIONS = ("preamble", "scope")


def _parse_prompt_sections(text: str) -> dict[str, str]:
  """Split a prompt template's raw text on its `<!-- section: <id> -->` marker lines."""
  sections: dict[str, str] = {}
  current_id: Optional[str] = None
  current_lines: list[str] = []
  for line in text.split("\n"):
    if line.startswith(_PROMPT_SECTION_MARKER_PREFIX) and line.endswith(_PROMPT_SECTION_MARKER_SUFFIX):
      if current_id is not None:
        sections[current_id] = "\n".join(current_lines)
      current_id = line[len(_PROMPT_SECTION_MARKER_PREFIX):-len(_PROMPT_SECTION_MARKER_SUFFIX)]
      current_lines = []
      continue
    if current_id is not None:
      current_lines.append(line)
  if current_id is not None:
    sections[current_id] = "\n".join(current_lines)
  return sections


def _load_prompt_sections(path: Path, required: tuple[str, ...], *, extraction: str) -> dict[str, str]:
  """Read a marker-sectioned prompt template fresh and return its sections.

  Stateless and uncached: every call re-reads the file so an edit takes effect on the
  next spawn. Missing file or missing required section raises with the file's full
  path and the most likely cause -- the repo checkout predates the *extraction*
  commit that moved this prompt out of Python. No embedded-text fallback.
  """
  if not path.is_file():
    raise FileNotFoundError(
        f"prompt template not found at {path} — the repo checkout most likely "
        f"predates the {extraction} extraction commit")
  sections = _parse_prompt_sections(path.read_text(encoding="utf-8"))
  missing = [section_id for section_id in required if section_id not in sections]
  if missing:
    raise ValueError(
        f"{path} is missing required section(s): {', '.join(missing)} — the repo checkout "
        f"most likely predates the {extraction} extraction commit")
  return sections


def load_worker_prompt_sections(cfg: CharlieBotConfig) -> dict[str, str]:
  """Read prompts/worker.md fresh and split it into its required sections."""
  return _load_prompt_sections(
      cfg.charlie_bot_repo / "prompts" / "worker.md", _REQUIRED_WORKER_PROMPT_SECTIONS, extraction="worker-prompt")


def load_verify_prompt_sections(cfg: CharlieBotConfig) -> dict[str, str]:
  """Read prompts/verify.md fresh and split it into its required sections."""
  return _load_prompt_sections(
      cfg.charlie_bot_repo / "prompts" / "verify.md", _REQUIRED_VERIFY_PROMPT_SECTIONS, extraction="verify-prompt")


def _substitute_tokens(template: str, tokens: dict[str, str]) -> str:
  """Sequentially `str.replace` every `{{name}}` token (not `str.format` -- section text may
  contain literal single braces)."""
  result = template
  for token, value in tokens.items():
    result = result.replace(token, value)
  return result


def _build_verify_repoless_prompt(description: str, cfg: CharlieBotConfig) -> str:
  """Build the full prompt for a repo-less VERIFY task (preamble + scope contract + task)."""
  sections = load_verify_prompt_sections(cfg)
  contract = _substitute_tokens(
      "\n".join(sections[section_id].strip("\n") for section_id in _REQUIRED_VERIFY_PROMPT_SECTIONS), {
          "{{result_trailer_expected}}": VERIFY_RESULT_TRAILER_EXPECTED,
          "{{canonical_template_path}}": str((cfg.charlie_bot_repo / "prompts" / "plan_template.html").resolve()),
      })
  if "{{" in contract:
    raise ValueError("verify prompt assembly left an unresolved {{token}} in the output")
  return f"{contract}\n\n{description}"


def _build_worker_prompt(
    description: str,
    repo_path: Path,
    base_branch: str,
    branch_name: str,
    wt_path: str,
    session_meta: SessionMetadata,
    cfg: CharlieBotConfig,
    task_type: TaskType,
    loop_dir: Optional[str] = None,
    iteration_number: Optional[int] = None,
    is_continuation: bool = False,
    keep_worktree: bool = False,
    start_point: Optional[str] = None,
) -> str:
  """Build the task-specific worker prompt (session info + worktree workflow + task)."""
  sections = load_worker_prompt_sections(cfg)

  session_info = sections["session_info"].replace("{{session_name}}", session_meta.name)

  intro_line = sections["intro_continuation"] if is_continuation else sections["intro_new"]

  branch_origin = f"`{base_branch}`" + (f" @ `{start_point}`" if start_point else "")
  branch_tokens = {
      "{{branch_name}}": branch_name,
      "{{base_branch_origin}}": branch_origin,
      "{{wt_path}}": wt_path,
      "{{repo_path}}": str(repo_path),
  }

  if task_type == TaskType.IMPLEMENT:
    workflow_body = _substitute_tokens(sections["workflow_implement"], {"{{intro_line}}": intro_line, **branch_tokens})
  elif task_type == TaskType.QUICK_EDIT:
    workflow_body = _substitute_tokens(sections["workflow_quick_edit"], {"{{intro_line}}": intro_line, **branch_tokens})
  elif task_type == TaskType.SCRIPT_RUN:
    workflow_body = _substitute_tokens(sections["workflow_script_run"], branch_tokens)
  else:
    raise ValueError(f"unsupported task_type: {task_type!r}")

  task_spec_section = sections["task_spec_source_files"]
  task_section = sections["task"].replace("{{description}}", description)

  worktree_section = (
      f"{sections['worktree_workflow_header']}\n{workflow_body}\n\n"
      f"{task_spec_section}\n{task_section}")

  iteration_reports_section = ""
  if loop_dir and iteration_number is not None:
    iteration_body = _substitute_tokens(
        sections["iteration_reports"], {
            "{{loop_dir}}": loop_dir,
            "{{iteration_number_padded}}": f"{iteration_number:04d}",
            "{{iteration_number}}": str(iteration_number),
        })
    iteration_reports_section = f"\n\n{iteration_body}"

  skills_section = sections["skills_discovery"]
  role_section = sections["role"]

  memory_section = ""
  memory_block = assemble_worker(cfg.memory_dir, repo_path.name)
  if memory_block:
    memory_section = "\n" + sections["memory"].replace("{{memory_block}}", memory_block)

  keep_worktree_section = ""
  if keep_worktree:
    keep_worktree_section = f"\n\n{sections['worktree_persistence']}"

  result = (
      f"{session_info}\n{sections['coding_principles']}\n{skills_section}\n{role_section}{memory_section}\n"
      f"{worktree_section}{iteration_reports_section}{keep_worktree_section}")

  if "{{" in result:
    raise ValueError("worker prompt assembly left an unresolved {{token}} in the output")

  return result


def _short_desc(description: str, limit: int = 120) -> str:
  """First line of description, truncated."""
  first_line = description.split('\n', 1)[0].strip()
  if len(first_line) > limit:
    return first_line[:limit] + '...'
  return first_line


def _worker_summary_timestamp() -> str:
  return datetime.now(ZoneInfo('America/Los_Angeles')).strftime('%Y-%m-%d %H:%M %Z')


def _worker_locator_summary(thread_id: str, status: str, timestamp: str) -> str:
  short_id = thread_id[:8]
  return (
      f"Worker `{short_id}` | thread `{thread_id}` | status: {status} | time: {timestamp} | "
      f"find in Workers panel by thread ID")


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


def _thread_worker_event(thread: ThreadMetadata, status: str, full_content: str = '') -> dict:
  """Build a worker_summary event whose chat content is the thread's locator summary."""
  return _build_worker_event(
      thread.id,
      _worker_locator_summary(thread.id, status, _worker_summary_timestamp()),
      status,
      full_content=full_content,
      backend=thread.backend,
      model=thread.model,
  )


def _resolve_routing_model(option: BackendOption, model: Optional[str], *, source: str) -> Optional[str]:
  if backend_type_allows_missing_model(option.type):
    return None
  if not model:
    raise ValueError(f"{source} model is required")
  return model


def resolve_backend_option(cfg: CharlieBotConfig, backend_id: str, model: Optional[str]) -> BackendOption:
  """Resolve a runtime backend option from explicit backend/model values."""
  if not backend_id:
    raise ValueError("resolved backend is required")
  option = cfg.get_backend_option(backend_id)
  if option is None:
    raise ValueError(f"resolved backend '{backend_id}' is not configured")
  resolved_model = _resolve_routing_model(option, model, source="resolved")
  return option.model_copy(update={"model": resolved_model})


def _option_default_backend_model(option: BackendOption, *, source: str) -> tuple[str, Optional[str]]:
  """Pair a configured option with its own default model, or raise when it needs one and has none."""
  if backend_type_allows_missing_model(option.type):
    return option.id, None
  if not option.model:
    raise ValueError(f"{source} backend '{option.id}' has no default model")
  return option.id, option.model


def _resolve_configured_backend_model(
    cfg: CharlieBotConfig,
    backend_id: str,
    *,
    source: str,
) -> tuple[str, Optional[str]]:
  """Resolve a configured backend option id to its default backend+model pair."""
  if not backend_id:
    raise ValueError(f"{source} backend is required")
  option = cfg.get_backend_option(backend_id)
  if option is None:
    raise ValueError(f"{source} backend '{backend_id}' is not in backend_options")
  return _option_default_backend_model(option, source=source)


def _resolve_session_default_backend_model(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    session_id: str,
) -> tuple[str, Optional[str]]:
  """Resolve backend+model from a session's default.

  A session with no backend recorded takes cfg.backend_options[0] as its default. A session
  pinned to an id that cfg.backend_options no longer defines (e.g. after a config.yaml rename)
  is a hard error: substituting a different backend would run the worker on another model and
  another account without the operator knowing. Raises when cfg.backend_options is empty, when
  the pinned id is unknown, or when the selected option has no default model.
  """
  option = cfg.get_backend_option(session_meta.backend) if session_meta.backend else None
  if option is None:
    if not cfg.backend_options:
      raise ValueError("session backend resolution requires a configured backend_options entry")
    if session_meta.backend:
      log.error(
          "session_backend_unresolved",
          stored=session_meta.backend,
          refused_substitute=cfg.backend_options[0].id,
          session_id=session_id,
      )
      raise ValueError(
          f"session backend '{session_meta.backend}' is not in config.yaml backend_options — "
          f"refusing to substitute '{cfg.backend_options[0].id}'")
    option = cfg.backend_options[0]
  return _option_default_backend_model(option, source="session")


async def resolve_session_subagent_backend_model(
    session_id: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
) -> tuple[str, Optional[str]]:
  """Resolve backend+model from the session's recorded default, ignoring any caller preference."""
  return await resolve_requested_subagent_backend_model(session_id, cfg, session_mgr)


async def resolve_requested_subagent_backend_model(
    session_id: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    requested_backend: Optional[str] = None,
) -> tuple[str, Optional[str]]:
  """Resolve backend+model from an explicit configured backend or the session default.

  Both paths are strict: an unknown explicit `requested_backend` (a typo in user input) and a
  session pinned to an id config.yaml no longer defines both raise. Only an empty session
  backend defaults, and it defaults to cfg.backend_options[0].
  """
  session_meta = await session_mgr.get_session(session_id)
  if session_meta is None:
    raise ValueError(f"session '{session_id}' not found")
  if requested_backend is not None:
    return _resolve_configured_backend_model(cfg, requested_backend, source="requested")
  return _resolve_session_default_backend_model(cfg, session_meta, session_id)


async def _select_verify_quota_retry_backend(
    session_id: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread: ThreadMetadata,
) -> Optional[tuple[str, Optional[str], list[str]]]:
  """Select the next untried checking-role backend after verifier quota exhaustion."""
  current_backend, _ = _require_thread_backend_model(thread, cfg)
  checked_backend, checked_model = await resolve_session_subagent_backend_model(session_id, cfg, session_mgr)
  tried_backends = list(thread.tried_backends)
  retry = review.select_reviewer_backend(cfg, checked_backend, checked_model, tried_backends)
  if retry is None or retry[0] == current_backend:
    log.warning(
        "verify_quota_retry_backend_unavailable",
        thread_id=thread.id,
        current_backend=current_backend,
        tried=tried_backends,
    )
    return None
  return retry


def _require_thread_backend_model(thread: ThreadMetadata, cfg: CharlieBotConfig) -> tuple[str, Optional[str]]:
  """Return backend+model from thread metadata or raise."""
  if not thread.backend:
    raise ValueError(f"thread '{thread.id}' missing backend metadata")
  if thread.model:
    return thread.backend, thread.model
  option = cfg.get_backend_option(thread.backend)
  if option is None:
    raise ValueError(f"thread '{thread.id}' backend '{thread.backend}' is not in backend_options")
  if backend_type_allows_missing_model(option.type):
    return thread.backend, None
  raise ValueError(f"thread '{thread.id}' missing model metadata")


def _prepare_thread_backend_metadata(
    thread: ThreadMetadata,
    backend_option: BackendOption,
    description: str,
) -> None:
  if backend_option.type != "cc-claude":
    return
  if thread.claude_session_id is None:
    thread.claude_session_id = str(uuid.uuid4())
  backend = ClaudeCodeBackend(
      model=backend_option.model,
      effort=backend_option.effort,
      cli_binary=backend_option.cli_binary,
      fast_mode=backend_option.fast_mode,
      claude_session_id=thread.claude_session_id,
  )
  thread.cli_command = shlex.join(backend._build_command(description) + [description])


async def _apply_backend_option(
    thread: ThreadMetadata,
    description: str,
    cfg: CharlieBotConfig,
    thread_mgr: ThreadManager,
    req: SpawnRequest,
) -> BackendOption:
  """Resolve the request's backend option onto the thread, persist it, and return the option."""
  backend_option = resolve_backend_option(cfg, req.resolved_backend, req.resolved_model)
  thread.backend = backend_option.id
  thread.model = backend_option.model
  if req.task_type == TaskType.VERIFY and backend_option.id not in thread.tried_backends:
    thread.tried_backends.append(backend_option.id)
  _prepare_thread_backend_metadata(thread, backend_option, description)
  await thread_mgr.save_metadata(thread)
  return backend_option


async def _construct_worker(
    session_id: str,
    thread: ThreadMetadata,
    description: str,
    working_dir: Path,
    worker_prompt: str,
    cfg: CharlieBotConfig,
    thread_mgr: ThreadManager,
    req: SpawnRequest,
) -> Worker:
  """Apply the request's backend onto the thread and build its Worker around working_dir."""
  backend_option = await _apply_backend_option(thread, description, cfg, thread_mgr, req)
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
    req: SpawnRequest,
) -> Worker:
  """Create worktree, build prompt, resolve backend, and construct Worker."""
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
  else:
    # An unattended launch has nobody present to reconcile a local/remote
    # disagreement, so a request that names no base starts from the remote's
    # published default branch; the local checkout's refs never enter the decision.
    base_branch = req.base_branch or f"origin/{await git_remote_default_branch(resolved_repo)}"

    # Compute branch name; the fresh-worktree branch derives the path from it.
    branch_name = req.branch_name_override or f"charliebot/task-{int(time.time())}-{thread.id[:8]}"

    canonical_branch = base_branch
    is_continuation = req.is_continuation
    start_point: Optional[str] = None

    if req.worktree_path_override:
      # Reuse an existing worktree (e.g. improve loop iterations sharing a single worktree).
      wt_path = Path(req.worktree_path_override)
      thread.skip_cleanup = req.skip_cleanup
    else:
      wt_path = Path(cfg.worktree_dir) / branch_name.replace("/", "-")

      # Ensure worktree parent dir exists and create worktree before launch.
      Path(cfg.worktree_dir).mkdir(parents=True, exist_ok=True)
      resolution = await git_create_worktree(resolved_repo, base_branch, branch_name, wt_path)
      canonical_branch = resolution.canonical
      is_continuation = False
      start_point = resolution.start_point

    # Store branch_name, repo_path, worktree path, and optional context on thread metadata.
    thread.branch_name = branch_name
    thread.repo_path = str(resolved_repo)
    thread.worktree_path = str(wt_path)
    thread.base_branch = canonical_branch
    thread.keep_worktree = req.keep_worktree
    thread.context = req.context

    # Build enriched prompt with worktree workflow instructions
    session_meta = await session_mgr.get_session(session_id)
    if session_meta is None:
      raise ValueError(f"session '{session_id}' not found")
    worker_prompt = _build_worker_prompt(
        description,
        resolved_repo,
        canonical_branch,
        branch_name,
        str(wt_path),
        session_meta,
        cfg,
        task_type=req.task_type,
        loop_dir=req.loop_dir,
        iteration_number=req.iteration_number,
        is_continuation=is_continuation,
        keep_worktree=req.keep_worktree,
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

  return await _construct_worker(session_id, thread, description, worktree_path, worker_prompt, cfg, thread_mgr, req)


async def _create_repoless_process(
    session_id: str,
    thread: ThreadMetadata,
    description: str,
    cfg: CharlieBotConfig,
    thread_mgr: ThreadManager,
    req: SpawnRequest,
) -> Worker:
  """Create a repo-less worker for prompt-only tasks (no worktree, no git)."""
  if req.task_type == TaskType.VERIFY:
    worker_prompt = _build_verify_repoless_prompt(description, cfg)
  elif req.task_type in (TaskType.IMPLEMENT, TaskType.QUICK_EDIT, TaskType.SCRIPT_RUN):
    worker_prompt = req.prompt_override or description
  else:
    raise ValueError(f"unsupported task_type: {req.task_type!r}")
  thread_dir = cfg.sessions_dir / session_id / 'threads' / thread.id

  # Repo-less tasks cannot produce branch/worktree review artifacts.
  thread.branch_name = None
  thread.repo_path = None
  thread.worktree_path = str(thread_dir)
  thread.require_review = False
  thread.context = req.context

  return await _construct_worker(session_id, thread, description, thread_dir, worker_prompt, cfg, thread_mgr, req)


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
  if thread.cli_command is None:
    thread.cli_command = " ".join(BASE_COMMAND + [description])
  thread.status = ThreadStatus.RUNNING
  thread.started_at = datetime.now(timezone.utc)
  await thread_mgr.save_metadata(thread)
  log.info("worker_running", thread_id=thread.id, session=session_id)

  await session_mgr.persist_and_broadcast(session_id, _thread_worker_event(thread, 'running'))

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


async def _maybe_override_exit_code_from_result(
    exit_code: int,
    session_id: str,
    thread: ThreadMetadata,
    thread_mgr: ThreadManager,
) -> int:
  """Override a non-zero exit_code to 0 when the last result event in events.jsonl is a success.

  Workers killed by SIGTERM (exit 143) after emitting a success result event should not be
  treated as failures. Any failure reading events.jsonl is logged and the original exit_code
  is returned -- this path must never raise.
  """
  if exit_code == 0:
    return exit_code
  try:
    events_path = await thread_mgr.get_events_log_path(session_id, thread.id)
    events = await asyncio.to_thread(parse_ndjson_file, events_path)
  except Exception as e:
    log.warning("worker_exit_override_read_failed", thread_id=thread.id, error=str(e))
    return exit_code

  for ev in reversed(events):
    if ev.get("type") != ET.RESULT:
      continue
    if ev.get("subtype") == "success" and ev.get("is_error") in (False, None):
      log.warning(
          "worker_exit_overridden_by_result_subtype",
          thread_id=thread.id,
          original_exit_code=exit_code,
      )
      return 0
    break
  return exit_code


async def _cleanup_worker_directory(thread: ThreadMetadata, skip_cleanup: bool, worktree_parent: Path) -> Optional[str]:
  """Remove the worker's worktree after a successful run.

  Returns an error message when the (success-path) cleanup fails so the caller can
  surface it; returns None on success or when there is nothing to clean.
  """
  if skip_cleanup:
    return None

  # Git worktree removal for repo-based workers.
  if thread.worktree_path and thread.repo_path:
    wt = Path(thread.worktree_path)
    if wt.exists():
      if not thread.branch_name:
        raise RuntimeError(f"thread {thread.id} has worktree_path but no branch_name")
      try:
        removed = await git_worktree_remove(
            thread.repo_path,
            wt,
            thread.id,
            allowed_parent=worktree_parent,
            expected_residue_name=git_worktree_dir_name(thread.branch_name),
        )
      except Exception as wt_err:
        log.error("worktree_cleanup_error", thread_id=thread.id, worktree=str(wt), error=str(wt_err), exc_info=True)
        return f"Worktree cleanup failed for {wt}: {wt_err}"
      if not removed:
        log.error("worktree_cleanup_remove_failed", thread_id=thread.id, worktree=str(wt))
        return f"Worktree cleanup failed for {wt}: git worktree remove reported failure"
      await git_worktree_prune(thread.repo_path, thread.id)
  return None


def _should_skip_worktree_cleanup(thread: ThreadMetadata, exit_code: int) -> bool:
  """Decide whether the worker's worktree must survive past this exit.

  Survives for: keep_worktree pin, explicit skip_cleanup, reviewer chain, a non-zero
  exit (failures keep their worktree so the debug state is not destroyed), or the
  reviewer-handoff handled on the zero-exit success path.
  """
  if thread.keep_worktree:
    return True
  if thread.skip_cleanup:
    return True
  if thread.review_of:
    return True
  if exit_code != 0:
    return True
  can_spawn_reviewer = all([thread.repo_path, thread.branch_name, thread.worktree_path])
  return thread.require_review and can_spawn_reviewer


async def _run_finalize_effects(
    session_id: str,
    description: str,
    thread: ThreadMetadata,
    exit_code: int,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    cfg: CharlieBotConfig,
    *,
    quota_exhausted: bool,
    error: str,
    skip_notify: bool,
    task_type: TaskType,
) -> None:
  """Run a finalized thread's side effects — worktree cleanup, then the notify chain.

  Holds no status write: the caller owns that. Every effect behind this call is
  judgment-idempotent (src/core/finalize_effects), so repetition converges to a no-op.
  """
  skip_cleanup = _should_skip_worktree_cleanup(thread, exit_code)
  cleanup_error = await _cleanup_worker_directory(thread, skip_cleanup, Path(cfg.worktree_dir))
  if cleanup_error:
    await session_mgr.persist_and_broadcast(session_id, {"type": ET.ERROR, "content": cleanup_error})

  if skip_notify:
    return
  await _notify_completion(
      session_id,
      description,
      thread,
      exit_code,
      thread_mgr,
      session_mgr,
      cfg,
      quota_exhausted=quota_exhausted,
      error=error,
      task_type=task_type)


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
    task_type: TaskType = TaskType.IMPLEMENT,
    completed_at: Optional[datetime] = None,
) -> None:
  """Update thread status and notify completion.

  ``completed_at`` overrides the terminal-status timestamp when given — the
  caller passes the raw log's final mtime so the recorded completion time is
  the run's true end, not whenever finalization happened to run.
  """
  # Re-read from disk: the cancel endpoint may have already set CANCELLED.
  current = await thread_mgr.get_thread(session_id, thread.id)
  cancelled = current and current.status == ThreadStatus.CANCELLED
  if task_type == TaskType.VERIFY and not cancelled and not quota_exhausted:
    report = await read_verify_final_report(session_id, thread.id, thread_mgr)
    trailer_error = verify_result_trailer_error(report)
    if trailer_error:
      exit_code = -1
      error = f"{error}; {trailer_error}" if error else trailer_error

  if cancelled:
    # Cancel endpoint already set the status; don't overwrite.
    log.info("worker_already_cancelled", thread_id=thread.id)
  elif quota_exhausted:
    await thread_mgr.update_status(session_id, thread.id, ThreadStatus.FAILED, completed_at=completed_at)
  elif error:
    await thread_mgr.update_status(session_id, thread.id, ThreadStatus.FAILED, exit_code=-1, completed_at=completed_at)
  elif exit_code == 0:
    await thread_mgr.update_status(session_id, thread.id, ThreadStatus.COMPLETED, exit_code=0, completed_at=completed_at)
    log.info("worker_completed", thread_id=thread.id)
  else:
    await thread_mgr.update_status(
        session_id, thread.id, ThreadStatus.FAILED, exit_code=exit_code, completed_at=completed_at)
    log.warning("worker_failed_nonzero", thread_id=thread.id, exit_code=exit_code)

  await _run_finalize_effects(
      session_id,
      description,
      thread,
      exit_code,
      thread_mgr,
      session_mgr,
      cfg,
      quota_exhausted=quota_exhausted,
      error=error,
      skip_notify=skip_notify,
      task_type=task_type)


async def _finalize_worker_safely(
    session_id: str,
    description: str,
    thread: ThreadMetadata,
    exit_code: int,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    cfg: CharlieBotConfig,
    quota_exhausted: bool,
    error: str,
    skip_notify: bool,
    task_type: TaskType = TaskType.IMPLEMENT,
    completed_at: Optional[datetime] = None,
) -> None:
  """Finalize a worker thread; on failure, log and best-effort-broadcast a session ERROR event."""
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
        error=error,
        skip_notify=skip_notify,
        task_type=task_type,
        completed_at=completed_at)
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


async def recomplete_finalize_effects(
    session_id: str,
    description: str,
    thread: ThreadMetadata,
    exit_code: int,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    cfg: CharlieBotConfig,
    quota_exhausted: bool = False,
    error: str = "",
    task_type: TaskType = TaskType.IMPLEMENT,
) -> None:
  """Re-run ONLY the side effects of _finalize_worker — never the status write.

  Startup-reconcile entry point for threads already marked terminal when a
  crash interrupted their notify chain. Every effect behind this call is
  judgment-idempotent (src/core/finalize_effects), so repetition converges to
  a no-op; the status/completed_at fields are left exactly as recorded.
  """
  await _run_finalize_effects(
      session_id,
      description,
      thread,
      exit_code,
      thread_mgr,
      session_mgr,
      cfg,
      quota_exhausted=quota_exhausted,
      error=error,
      skip_notify=False,
      task_type=task_type)


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

  thread = None
  worker = None
  exit_code = -1
  quota_exhausted = False
  error_msg = ""
  cancelled = False
  try:
    thread = await thread_mgr.get_thread(session_id, thread_id)
    if not thread:
      log.error("spawn_worker_thread_missing", session=session_id, thread_id=thread_id)
      return

    if req.repo_path is None:
      # Repo-less worker: run prompt directly without worktree
      worker = await _create_repoless_process(session_id, thread, description, cfg, thread_mgr, req)
    else:
      resolved_repo = Path(req.repo_path).resolve()
      worker = await _create_worktree_and_process(
          session_id, thread, description, cfg, session_mgr, thread_mgr, resolved_repo, req)

    exit_code, quota_exhausted, error_msg = await _stream_worker_events(
        worker, session_id, description, thread, thread_mgr, session_mgr)

    if req.task_type == TaskType.VERIFY and quota_exhausted:
      retry_backend = await _select_verify_quota_retry_backend(session_id, cfg, session_mgr, thread)
      if retry_backend is not None:
        resolved_backend, resolved_model, tried_backends = retry_backend
        log.warning(
            "verify_quota_retry",
            thread_id=thread.id,
            exhausted_backend=thread.backend,
            retry_backend=resolved_backend,
        )
        req.resolved_backend = resolved_backend
        req.resolved_model = resolved_model
        thread.tried_backends = tried_backends
        quota_exhausted = False
        worker = await _create_repoless_process(session_id, thread, description, cfg, thread_mgr, req)
        exit_code, quota_exhausted, error_msg = await _stream_worker_events(
            worker, session_id, description, thread, thread_mgr, session_mgr)

    if exit_code != 0 and not quota_exhausted and not error_msg:
      exit_code = await _maybe_override_exit_code_from_result(exit_code, session_id, thread, thread_mgr)

  except asyncio.CancelledError:
    # Only live trigger: event-loop shutdown (graceful restart). Never write a
    # terminal state here — the thread stays as-is on disk and the next boot's
    # reconcile judges the truth via resolve_run: covered transports are
    # re-attached for their real result, everything else is finalized with an
    # explicit reason. Covered workers keep running on their own raw-log fds
    # (detach stops the closing loop's transports from killing them);
    # uncovered transports and improve iterations (whose loop dies with this
    # process) cannot outlive the server usefully, so their processes are
    # still terminated (ff82c34's orphan prevention).
    cancelled = True
    transport = runs.backend_type(cfg, thread.backend if thread else None)
    let_go = (worker is not None and transport not in runs.UNCOVERED_BACKEND_TYPES
              and not description.startswith(runs.IMPROVE_ITERATION_PREFIX))
    log.warning(
        "spawn_worker_cancelled",
        session=session_id,
        thread_id=thread_id,
        transport=transport,
        action="let_go" if let_go else ("terminate" if worker else "none"),
    )
    if worker:
      if let_go:
        worker.detach()
      else:
        await worker.terminate()
    raise
  except Exception as e:
    log.error("spawn_worker_setup_failed", session=session_id, error=str(e), traceback=traceback.format_exc())
    error_msg = str(e)
  finally:
    if thread is not None and not cancelled:
      await _finalize_worker_safely(
          session_id,
          description,
          thread,
          exit_code,
          thread_mgr,
          session_mgr,
          cfg,
          quota_exhausted,
          error_msg,
          req.skip_notify,
          task_type=req.task_type,
          completed_at=_run_completion_time(cfg, session_id, thread.id))


def _run_completion_time(cfg: CharlieBotConfig, session_id: str, thread_id: str) -> Optional[datetime]:
  """The run's true completion time: its raw log's final mtime, when one exists.

  Independent of backend, event content, and how long the server was down; the
  finalize chain writes it into completed_at so downtime never shifts a run's
  recorded end.
  """
  thread_dir = cfg.sessions_dir / session_id / "threads" / thread_id
  return runs.raw_completion_time(runs.raw_log_path(thread_dir))


# Recovery-event reason recorded when the finalize liveness gate keeps a run
# alive: resume hit an exception or cancellation while the run's death was
# unproven, so the FAILED finalize was skipped (resume-exception-alive).
RESUME_EXCEPTION_ALIVE_REASON = "resume-exception-alive"


async def resume_worker(
    session_id: str,
    description: str,
    thread_id: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
    *,
    is_alive: Callable[[], bool],
    interrupt_reason: str = "",
    on_silence: Optional[Callable[[], Awaitable[None]]] = None,
) -> None:
  """Re-attach to an interrupted run's raw stream, then run the finalize chain.

  Server-startup entry point: RUNNING/STALLED outcomes pass the recorded
  (pid, pid_start) liveness judgment as ``is_alive``; COMPLETED/DIED drains
  pass ``lambda: False``. Finalize is judgment-idempotent, so finishing here
  and finishing without a restart take exactly the same path.

  ``interrupt_reason`` is the reconcile's resolve_run reason: when the drain
  ends without a successful result and no harder error occurred, it becomes
  the finalize error, so the master's summary states why the run failed
  instead of a bare exit -1.

  ``on_silence`` is the follow-time silence recheck, forwarded to the
  re-attached follow loop.

  Liveness gate: before a FAILED outcome is recorded under any exception or
  cancellation path (asyncio.CancelledError included — it bypasses
  ``except Exception`` and lands in the same finally), the same ``is_alive``
  probe is consulted. Probe true (alive, or constant-true because death is
  unverifiable) means the failure is our own, not the run's: a recovery event
  is emitted and the FAILED finalize is skipped, leaving the thread running.
  Probe false (proven dead) finalizes exactly as before. Normal completion
  (exit_code 0) and the quota branch (which kills the process itself) are
  not gated.
  """
  thread = None
  worker = None
  exit_code = -1
  quota_exhausted = False
  error_msg = ""
  try:
    thread = await thread_mgr.get_thread(session_id, thread_id)
    if not thread:
      log.error("resume_worker_thread_missing", session=session_id, thread_id=thread_id)
      return
    backend_option = None
    if thread.backend:
      try:
        backend_option = resolve_backend_option(cfg, thread.backend, thread.model)
      except ValueError as e:
        # Translate-only fallback: a stale backend id degrades result
        # detection to the raw claude shape, never crashes recovery.
        log.warning("resume_backend_unresolved", thread_id=thread.id, error=str(e))
    events_log = await thread_mgr.get_events_log_path(session_id, thread.id)
    working_dir = Path(thread.worktree_path) if thread.worktree_path else events_log.parent
    worker = Worker(thread, working_dir, events_log, description, cfg, backend_option=backend_option)
    exit_code = await worker.resume(is_alive=is_alive, on_silence=on_silence)
  except QuotaExhaustedException:
    log.warning("resume_worker_quota_exhausted", thread_id=thread_id)
    if is_alive() and thread is not None and thread.pid is not None:
      kill_process_group(thread.pid, signal.SIGTERM)
    quota_exhausted = True
  except Exception as e:
    log.error("resume_worker_failed", thread_id=thread_id, error=str(e), traceback=traceback.format_exc())
    error_msg = str(e)
  finally:
    if exit_code != 0 and not quota_exhausted and not error_msg and interrupt_reason:
      error_msg = interrupt_reason
    if thread is not None:
      if exit_code != 0 and not quota_exhausted and is_alive():
        # Alive or unverifiable death: never record FAILED on our own error.
        log.warning("resume_finalize_skipped_alive", thread_id=thread_id, error=error_msg)
        from src.core import (
          init as init_module,  # lazy: init imports this module lazily too
        )
        await init_module._report_recovery_event(
            session_mgr,
            session_id,
            f"Worker thread {thread_id[:8]} hit a resume error but its process cannot be proven "
            f"dead ({RESUME_EXCEPTION_ALIVE_REASON}). It is NOT being killed — left running "
            "and judged again on the next restart.")
      else:
        await _finalize_worker_safely(
            session_id,
            description,
            thread,
            exit_code,
            thread_mgr,
            session_mgr,
            cfg,
            quota_exhausted,
            error_msg,
            False,
            task_type=thread.task_type or TaskType.IMPLEMENT,
            completed_at=_run_completion_time(cfg, session_id, thread.id))


async def _persist_worker_summary_once(
    session_id: str,
    thread_id: str,
    event: dict,
    session_mgr: SessionManager,
    *,
    fallback: bool = False,
) -> None:
  """Persist and broadcast a worker_summary event unless the session already carries one.

  Idempotency judgment: a rerun of the finalize chain (e.g. startup reconcile
  completing a crashed finalize) never duplicates the summary. ``fallback`` marks
  the degraded summary written when the notify chain itself failed.
  """
  if finalize_effects.terminal_summary_present(session_mgr.load_chat_events_sync(session_id), thread_id):
    log.info("worker_summary_skip_duplicate", session=session_id, thread=thread_id, fallback=fallback)
    return
  await session_mgr.mark_unread(session_id)
  await session_mgr.persist_and_broadcast(session_id, event)
  log.info("worker_summary_sent", session=session_id, thread=thread_id, fallback=fallback)


async def _broadcast_completion(
    session_id: str,
    description: str,
    thread: ThreadMetadata,
    exit_code: int,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    quota_exhausted: bool = False,
    error: str = "",
    task_type: TaskType = TaskType.IMPLEMENT,
) -> tuple[str, str]:
  """Build and broadcast the worker_summary event. Returns (events_summary, full_summary)."""
  # Update last_run_status for scheduled sessions
  session_meta = await session_mgr.get_session(session_id)
  if session_meta and session_meta.scheduled_task:
    session_meta.last_run_status = "success" if exit_code == 0 else "failed"
    session_meta.updated_at = datetime.now(timezone.utc)
    await session_mgr.save_metadata(session_meta)

  if task_type == TaskType.VERIFY:
    events_summary = await read_verify_final_report(session_id, thread.id, thread_mgr)
  else:
    events_summary = await _read_events_summary(session_id, thread.id, thread_mgr)

  # Re-read to pick up cancel endpoint's status
  current_thread = await thread_mgr.get_thread(session_id, thread.id)
  cancelled = current_thread and current_thread.status == ThreadStatus.CANCELLED

  status = 'cancelled' if cancelled else ('completed' if exit_code == 0 else 'failed')
  if task_type == TaskType.VERIFY:
    report = events_summary or "(no verifier final report)"
    full_summary = f"**Verifier completion: thread `{thread.id}`**\n\n{report}"
  else:
    full_summary = f"**Worker finished: {description}**\n\n{events_summary}"

  suffix = ""
  if cancelled:
    suffix = "\n\n*Cancelled by user.*"
  elif quota_exhausted:
    suffix = "\n\n*Worker stopped: API quota exhausted.*"
  elif error:
    if task_type == TaskType.VERIFY:
      suffix = f"\n\n*Verifier completion failed: {error}*"
    else:
      suffix = f"\n\n*Worker error: {error}*"
  elif exit_code != 0:
    suffix = f"\n\n*Worker exited with code {exit_code}.*"
  full_summary += suffix

  worker_event = _thread_worker_event(thread, status, full_content=full_summary)
  await _persist_worker_summary_once(session_id, thread.id, worker_event, session_mgr)
  return events_summary, full_summary


async def _notify_completion(
    session_id: str,
    description: str,
    thread: ThreadMetadata,
    exit_code: int,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    cfg: CharlieBotConfig,
    quota_exhausted: bool = False,
    error: str = "",
    task_type: TaskType = TaskType.IMPLEMENT,
) -> None:
  """Broadcast worker_summary event to the session WebSocket and trigger master agent."""
  try:
    events_summary, full_summary = await _broadcast_completion(
        session_id,
        description,
        thread,
        exit_code,
        thread_mgr,
        session_mgr,
        quota_exhausted,
        error,
        task_type=task_type)

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

    await review.maybe_spawn_reviewer(
        session_id, thread, exit_code, events_summary, full_summary, thread_mgr, session_mgr, cfg)
  except Exception as e:
    log.error("notify_completion_failed", thread_id=thread.id, error=str(e))
    try:
      status = "completed" if exit_code == 0 else "failed"
      fallback = _worker_locator_summary(thread.id, status, _worker_summary_timestamp())
      fallback_event = _build_worker_event(
          thread.id,
          fallback,
          status,
          full_content=f"{fallback}\n\n*(summary unavailable: {e})*",
          backend=thread.backend,
          model=thread.model,
      )
      await _persist_worker_summary_once(session_id, thread.id, fallback_event, session_mgr, fallback=True)
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
