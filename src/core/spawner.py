"""Direct worker spawner — creates a task, enriches the prompt, and runs the worker."""

import asyncio
import shlex
import time
import traceback
import uuid
from datetime import datetime, timedelta, timezone
from zoneinfo import ZoneInfo
from pathlib import Path
from typing import Optional

import structlog

from src.api.message_utils import extract_text_from_message
from src.agents.backends.claude_code import BASE_COMMAND, ClaudeCodeBackend
from src.agents.worker import QuotaExhaustedException, Worker
from src.core import event_types as ET
from src.core import review
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
from src.core.verify_trailer import (
    VERIFY_RESULT_TRAILER_EXPECTED as _VERIFY_RESULT_TRAILER_EXPECTED,
    read_verify_final_report as _read_verify_final_report,
    verify_result_trailer_error as _verify_result_trailer_error,
)
from src.core.git import (
    git_current_branch,
    git_create_worktree,
    git_worktree_dir_name,
    git_worktree_prune,
    git_worktree_remove,
)
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.config import CharlieBotConfig, get_scheduled_tasks
from src.core.notifications import send_telegram

log = structlog.get_logger()

_PRE_TAKEOFF_PHRASE = "pre take off"
_TAKEOFF_PHRASE = "take off"
_PRE_TAKEOFF_WINDOW = timedelta(hours=12)

_CODING_PRINCIPLES = (
    "## Coding Principles\n"
    "The codebase has a single user. Apply these principles:\n"
    "- **Fail fast**: surface errors immediately. Do NOT add fallbacks, defaults, or silent recovery.\n"
    "- **No swallowed exceptions**: always log or re-raise. Never use bare `except: pass`.\n"
    "- **No defensive programming**: do not add guards for scenarios that cannot happen.\n")

_VERIFY_PROMPT_PREAMBLE = (
    "You are a read-only plan verifier. Retrieve evidence through allowed local and network reads, and report "
    "findings.\n"
    "Local and network evidence reads are allowed through already-available tools, commands, connectivity, and "
    "credentials. Network examples include web search/fetch, read-only API queries, and read-only SSH commands; "
    "the boundary is semantic read-only behavior, not a transport or HTTP-method allowlist.\n"
    "Refuse and report any requested part that would mutate local or external state instead of executing it. This "
    "includes operations that create, update, delete, trigger, submit, upload, or send messages, as well as file "
    "edits, Git writes, and job submissions.\n"
    "Mark a claim `unverifiable` only when reasonable allowed local or network reads cannot access the evidence, "
    "or verification would require state mutation. Network access alone never makes a claim `unverifiable`.\n"
    "Report format: one line per checked claim, `<verdict> | <claim> | <anchor> | <one-line evidence>` "
    "with verdict exactly one of `confirmed` / `mismatch` / `mismatch-approval` / `unverifiable`. "
    "`mismatch-approval` is a mismatch that invalidates a term of the approval object "
    "(the plan's 4.1 Schema, resolved Trade-offs, or promoted Other Details entries). "
    f"The final line must match {_VERIFY_RESULT_TRAILER_EXPECTED}. Complete examples are "
    "`RESULT: clean`, `RESULT: 2 mismatches (1 approval)`, and `RESULT: 1 mismatch (0 approval)`. "
    "For a non-clean result, N is the total count of `mismatch` and `mismatch-approval` lines, "
    "and M is the count of `mismatch-approval` lines, with 0 <= M <= N; when N = 1, both "
    "singular `mismatch` and plural `mismatches` are legal.\n"
    "This report format is fixed by the harness and overrides any output format the task spec requests; "
    "a task spec may add checks or scope, never change the report format.\n"
    "Adequacy findings use the labelled block form defined in the plan scope block instead of this "
    "single-line form; fidelity findings keep it.")


def _build_worker_prompt(
    description: str,
    repo_path: Path,
    base_branch: str,
    branch_name: str,
    wt_path: str,
    session_meta: SessionMetadata,
    task_type: TaskType,
    loop_dir: Optional[str] = None,
    iteration_number: Optional[int] = None,
    is_continuation: bool = False,
    keep_worktree: bool = False,
    start_point: Optional[str] = None,
) -> str:
  """Build the task-specific worker prompt (session info + worktree workflow + task)."""
  session_info = (f"## Session Info\n"
                  f"- Session: {session_meta.name}\n")

  if is_continuation:
    intro_line = (
        "You are continuing work in an existing worktree from a previous iteration. "
        "Review previous iteration changes before starting.")
  else:
    intro_line = "A dedicated git worktree is already created for you."

  branch_origin = f"`{base_branch}`" + (f" @ `{start_point}`" if start_point else "")
  branch_lines = (
      f"- Branch: `{branch_name}` (from {branch_origin})\n"
      f"- Worktree: `{wt_path}`\n"
      f"- Repo: `{repo_path}`")

  commit_steps = (
      f"Follow these steps exactly:\n"
      f"1. `cd {wt_path}` — do ALL your work inside this worktree.\n"
      f"2. Commit your changes with descriptive messages.\n"
      f"   Use structured commit messages: first line is a short summary, then a blank line, "
      f"then a \"Why:\" line explaining the business reason for the change.\n")

  if task_type == TaskType.IMPLEMENT:
    workflow_body = (
        f"{intro_line}\n"
        f"{branch_lines}\n\n"
        f"{commit_steps}\n"
        f"STOP here. Do NOT rebase, merge, or remove the worktree. A reviewer will handle that.")
  elif task_type == TaskType.QUICK_EDIT:
    workflow_body = (
        f"{intro_line}\n"
        f"{branch_lines}\n\n"
        f"{commit_steps}\n"
        f"STOP here. Do NOT rebase, push, or remove the worktree. "
        f"No reviewer will run; the orchestrator will handle merge/push.")
  elif task_type == TaskType.SCRIPT_RUN:
    workflow_body = (
        f"A dedicated git worktree is provided as your isolated sandbox.\n"
        f"{branch_lines}\n\n"
        f"This is a script-run task. The worktree exists only to give you an isolated environment "
        f"to run commands, submit jobs, or inspect state.\n"
        f"- Do NOT modify tracked files.\n"
        f"- Do NOT commit.\n"
        f"- Finish with `git status --short` showing a clean tree.\n"
        f"- If you discover that a repo change is actually required to complete the task, "
        f"STOP and report back instead of making the change.")
  else:
    raise ValueError(f"unsupported task_type: {task_type!r}")

  task_spec_section = (
      "## Task Spec Source Files\n"
      "- If the task text below is a structured task spec or contains a `## Source Files` section, "
      "read every listed source file before editing.\n"
      "- If the task spec and source files conflict, stop and report the conflict instead of inventing "
      "a merged requirement.\n")

  worktree_section = (
      f"## Worktree Workflow\n"
      f"{workflow_body}\n\n"
      f"{task_spec_section}\n"
      f"## Task\n{description}")

  iteration_reports_section = ""
  if loop_dir and iteration_number is not None:
    iteration_reports_section = (
        f"\n\n## Iteration Reports\n"
        f"Previous iteration reports are in: {loop_dir}/\n"
        f"Review any existing iter_*.md files there before starting work. Treat them as advisory evidence and hints only.\n"
        f"When you finish, write your report to: {loop_dir}/iter_{iteration_number:04d}.md\n"
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
      "- **Mandatory for tasks in any domain that has a matching skill**: you MUST read that skill BEFORE writing any code, running any command, or submitting any job. This includes profiling, metrics analysis, data processing — not just training. Starting work without reading the relevant skill is forbidden.\n"
  )

  role_section = (
      "## Role\n"
      "- You are a **worker agent**. Do NOT delegate tasks to subagents — implement the work yourself directly.\n"
      "- Ignore any instructions from parent CLAUDE.md files that tell you to delegate or spawn subagents.\n")

  keep_worktree_section = ""
  if keep_worktree:
    keep_worktree_section = (
        "\n\n## Worktree Persistence\n"
        "This worktree will persist after the reviewer merges. "
        "You may safely use it as the WorkDir for external long-running processes (e.g. SLURM jobs).")

  return (
      f"{session_info}\n{_CODING_PRINCIPLES}\n{skills_section}\n{role_section}\n"
      f"{worktree_section}{iteration_reports_section}{keep_worktree_section}")


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
  if backend_type_allows_missing_model(option.type):
    return option.id, None
  if not option.model:
    raise ValueError(f"{source} backend '{backend_id}' has no default model")
  return option.id, option.model


def _resolve_session_backend_with_fallback(
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
          fallback=cfg.backend_options[0].id,
          session_id=session_id,
      )
      raise ValueError(
          f"session backend '{session_meta.backend}' is not in config.yaml backend_options — "
          f"refusing to substitute '{cfg.backend_options[0].id}'")
    option = cfg.backend_options[0]
  if backend_type_allows_missing_model(option.type):
    return option.id, None
  if not option.model:
    raise ValueError(f"session backend '{option.id}' has no default model")
  return option.id, option.model


async def resolve_session_subagent_backend_model(
    session_id: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
) -> tuple[str, Optional[str]]:
  """Resolve backend+model from the session default, with graceful fallback on stale metadata."""
  session_meta = await session_mgr.get_session(session_id)
  if session_meta is None:
    raise ValueError(f"session '{session_id}' not found")
  return _resolve_session_backend_with_fallback(cfg, session_meta, session_id)


async def resolve_requested_subagent_backend_model(
    session_id: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    requested_backend: Optional[str] = None,
) -> tuple[str, Optional[str]]:
  """Resolve backend+model from an explicit configured backend or the session default.

  Explicit `requested_backend` stays strict (typo in user input must fail). Session-default
  resolution falls back gracefully when the stored backend id is stale.
  """
  session_meta = await session_mgr.get_session(session_id)
  if session_meta is None:
    raise ValueError(f"session '{session_id}' not found")
  if requested_backend is not None:
    return _resolve_configured_backend_model(cfg, requested_backend, source="requested")
  return _resolve_session_backend_with_fallback(cfg, session_meta, session_id)


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
    thread.keep_worktree = req.keep_worktree
    thread.context = req.context

    session_meta = await session_mgr.get_session(session_id)
    worker_prompt = _build_worker_prompt(
        description,
        resolved_repo,
        base_branch,
        branch_name,
        str(wt_path),
        session_meta,
        task_type=req.task_type,
        loop_dir=req.loop_dir,
        iteration_number=req.iteration_number,
        is_continuation=req.is_continuation,
        keep_worktree=req.keep_worktree)
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
    resolution = await git_create_worktree(resolved_repo, base_branch, branch_name, wt_path)

    # Store branch_name, repo_path, worktree path, and optional context on thread metadata.
    thread.branch_name = branch_name
    thread.repo_path = str(resolved_repo)
    thread.worktree_path = str(wt_path)
    thread.base_branch = resolution.canonical
    thread.keep_worktree = req.keep_worktree
    thread.context = req.context

    # Build enriched prompt with worktree workflow instructions
    session_meta = await session_mgr.get_session(session_id)
    worker_prompt = _build_worker_prompt(
        description,
        resolved_repo,
        resolution.canonical,
        branch_name,
        str(wt_path),
        session_meta,
        task_type=req.task_type,
        loop_dir=req.loop_dir,
        iteration_number=req.iteration_number,
        keep_worktree=req.keep_worktree,
        start_point=resolution.start_point)
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
  _prepare_thread_backend_metadata(thread, backend_option, description)
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
    thread_mgr: ThreadManager,
    req: SpawnRequest,
) -> Worker:
  """Create a repo-less worker for prompt-only tasks (no worktree, no git)."""
  if req.task_type == TaskType.VERIFY:
    canonical_template_path = (cfg.charlie_bot_repo / "prompts" / "plan_template.html").resolve()
    worker_prompt = (
        f"{_VERIFY_PROMPT_PREAMBLE}\n"
        "Verification scope:\n"
        "Check exactly the scope the task spec declares; do not add checks beyond it.\n"
        "- Full verification (the spec declares full): read the plan artifact by itself and complete an "
        "artifact-only standalone-comprehension pass first, then read the canonical plan template at "
        f"`{canonical_template_path}` and all task-provided evidence, and check the plan against every canonical "
        "rule in the template's BLOCK KIT.\n"
        "- Delta verification (the spec declares delta): check only the declared terms, their dependent claims, "
        "prior mismatches, and document structure; do not reopen unchanged content.\n"
        "- Adequacy (both plan scopes, delta included): assume the plan's claims are true and its design "
        "implemented as written, then test each outcome its section 1 claims by attempted refutation. Report "
        "one labelled block per claimed outcome, one aspect per line:\n"
        "  `mismatch:` or `confirmed:` — the outcome being judged\n"
        "  `gap-design:` — why the described mechanism does not entail it, or `gap-goal:` — why the goal "
        "statement itself is the defect (omit both when confirmed)\n"
        "  `scenario:` — the counterexample you built, or the strongest you tried and why it failed, stated "
        "as a scenario rather than a restatement of the plan\n"
        "  `anchor:` — where in the plan\n"
        "A boundary section 1 states explicitly is correct-as-scoped. When the spec quotes the originating "
        "request, also report `gap-goal:` for an outcome it asks that section 1 neither claims nor scopes "
        "out.\n"
        "Use reasonable allowed local and network reads through already-available tools, commands, connectivity, "
        "and credentials when checking evidence, including web search/fetch, read-only API queries, and read-only "
        "SSH commands. The boundary remains "
        "semantic read-only behavior, not a transport or HTTP-method allowlist. Refuse and report any check that "
        "would mutate local or external state instead of executing it, including operations that create, update, "
        "delete, trigger, submit, upload, or send messages, as well as file edits, Git writes, and job submissions.\n"
        "Report a missing or unreadable plan artifact or canonical template, a missing required in-scope source "
        "anchor, or any in-scope canonical-rule deviation as `mismatch`. Preserve `unverifiable` only when "
        "reasonable allowed local or network reads cannot access the evidence, or verification would require "
        "state mutation. Network access alone never makes a claim `unverifiable`."
        f"\n\n{description}")
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

  backend_option = resolve_backend_option(cfg, req.resolved_backend, req.resolved_model)
  thread.backend = backend_option.id
  thread.model = backend_option.model
  if req.task_type == TaskType.VERIFY and backend_option.id not in thread.tried_backends:
    thread.tried_backends.append(backend_option.id)
  _prepare_thread_backend_metadata(thread, backend_option, description)
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
  if thread.cli_command is None:
    thread.cli_command = " ".join(BASE_COMMAND + [description])
  thread.status = ThreadStatus.RUNNING
  thread.started_at = datetime.now(timezone.utc)
  await thread_mgr.save_metadata(thread)
  log.info("worker_running", thread_id=thread.id, session=session_id)

  now = _worker_summary_timestamp()
  started_event = _build_worker_event(
      thread.id,
      _worker_locator_summary(thread.id, 'running', now),
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
  return exit_code == 0 and thread.require_review and not thread.review_of and can_spawn_reviewer


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
) -> None:
  """Update thread status and notify completion."""
  # Re-read from disk: the cancel endpoint may have already set CANCELLED.
  current = await thread_mgr.get_thread(session_id, thread.id)
  cancelled = current and current.status == ThreadStatus.CANCELLED
  if task_type == TaskType.VERIFY and not cancelled and not quota_exhausted:
    report = await _read_verify_final_report(session_id, thread.id, thread_mgr)
    trailer_error = _verify_result_trailer_error(report)
    if trailer_error:
      exit_code = -1
      error = f"{error}; {trailer_error}" if error else trailer_error

  if cancelled:
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

  skip_cleanup = _should_skip_worktree_cleanup(thread, exit_code)
  cleanup_error = await _cleanup_worker_directory(thread, skip_cleanup, Path(cfg.worktree_dir))
  if cleanup_error:
    await session_mgr.persist_and_broadcast(session_id, {"type": ET.ERROR, "content": cleanup_error})

  if not skip_notify:
    if task_type == TaskType.VERIFY:
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
    else:
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
        task_type=task_type)
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


class DelegationBlockedError(Exception):
  """Raised when the takeoff gate rejects a delegation attempt."""


def _is_real_user_message(event: dict) -> bool:
  """Real user message = ET.USER with string content (excludes trigger events and nested tool_result blocks)."""
  if event.get("type") != ET.USER:
    return False
  return isinstance(event.get("content"), str)


def _normalize_takeoff_content(content: str) -> str:
  """Normalize case and consecutive whitespace for authorization phrase matching."""
  return " ".join(content.casefold().split())


def _parse_pre_takeoff_timestamp(event: dict, session_id: str) -> Optional[datetime]:
  """Parse a pre-takeoff event timestamp as UTC, failing closed when it is invalid."""
  timestamp = event.get("timestamp")
  if not isinstance(timestamp, str) or not timestamp:
    log.warning(
        "pre_takeoff_timestamp_missing",
        session=session_id,
        event_id=event.get("id"),
    )
    return None
  try:
    issued_at = datetime.fromisoformat(timestamp.replace("Z", "+00:00"))
  except ValueError:
    log.warning(
        "pre_takeoff_timestamp_unparseable",
        session=session_id,
        event_id=event.get("id"),
        timestamp=timestamp,
    )
    return None
  if issued_at.tzinfo is None:
    log.warning(
        "pre_takeoff_timestamp_timezone_missing",
        session=session_id,
        event_id=event.get("id"),
        timestamp=timestamp,
    )
    return None
  return issued_at.astimezone(timezone.utc)


def check_takeoff_gate(
    session_id: str,
    session_mgr: SessionManager,
    now: Optional[datetime] = None,
) -> None:
  """Verify an active pre-takeoff or ordinary takeoff authorization window."""
  effective_now = now if now is not None else datetime.now(timezone.utc)
  if effective_now.tzinfo is None:
    raise ValueError("authorization check time must be timezone-aware")
  effective_now = effective_now.astimezone(timezone.utc)

  events = session_mgr.load_chat_events_sync(session_id)
  latest_user_has_takeoff = False
  latest_pre_takeoff_at: Optional[datetime] = None
  for event in events:
    if not _is_real_user_message(event):
      continue
    content = event.get("content")
    normalized = _normalize_takeoff_content(content)
    latest_user_has_takeoff = _TAKEOFF_PHRASE in normalized
    if _PRE_TAKEOFF_PHRASE in normalized:
      issued_at = _parse_pre_takeoff_timestamp(event, session_id)
      if issued_at is not None:
        latest_pre_takeoff_at = issued_at

  pre_takeoff_active = (
      latest_pre_takeoff_at is not None and
      latest_pre_takeoff_at <= effective_now < latest_pre_takeoff_at + _PRE_TAKEOFF_WINDOW)
  if latest_user_has_takeoff or pre_takeoff_active:
    return

  raise DelegationBlockedError(
      'Delegation blocked: no active authorization. A valid "pre take off" within 12 hours or '
      '"take off" in the latest real user message is required before delegating.')


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
    log.warning("spawn_worker_cancelled", session=session_id, thread_id=thread_id)
    if worker:
      await worker.terminate()
    raise
  except Exception as e:
    log.error("spawn_worker_setup_failed", session=session_id, error=str(e), traceback=traceback.format_exc())
    error_msg = str(e)
  finally:
    if thread is not None:
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
          task_type=req.task_type)


async def _broadcast_completion(
    session_id: str,
    description: str,
    thread,
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
    events_summary = await _read_verify_final_report(session_id, thread.id, thread_mgr)
  else:
    events_summary = await _read_events_summary(session_id, thread.id, thread_mgr)

  # Re-read to pick up cancel endpoint's status
  current_thread = await thread_mgr.get_thread(session_id, thread.id)
  cancelled = current_thread and current_thread.status == ThreadStatus.CANCELLED

  status = 'cancelled' if cancelled else ('completed' if exit_code == 0 else 'failed')
  now = _worker_summary_timestamp()
  chat_summary = _worker_locator_summary(thread.id, status, now)
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
