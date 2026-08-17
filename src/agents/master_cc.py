"""Master CC — spawns a Claude Code subprocess for the master agent."""

import asyncio
import dataclasses
import os
import signal
import time
from collections.abc import Callable
from datetime import datetime, timezone
from pathlib import Path
from typing import TYPE_CHECKING

import structlog

from src.agents.backends.base import (
  AgentBackend,
  _read_stderr_tail,
  make_error_event,
  make_text_event,
  tail_follow_events,
)
from src.core import event_types as ET
from src.core import runs
from src.core.config import CharlieBotConfig, claude_config_dir
from src.core.latex import check_tex_changed, clear_snapshot, get_tex_path, snapshot_tex
from src.core.memory import assemble_master
from src.core.models import (
    PROJECT_ROLE,
    BackendOption,
    MasterRunRecord,
    SessionCallbacks,
    SessionMetadata,
    backend_type_allows_missing_model,
)
from src.core.process import kill_process_group
from src.core.streaming import handle_compaction_events, streaming_manager
from src.core.thinking_state import busy_since, clear_busy, mark_busy

if TYPE_CHECKING:
  from src.core.sessions import SessionManager

log = structlog.get_logger()

# Prefixed to a salvaged silent turn so the user sees the thinking the model
# produced instead of nothing. Local chat-stream only: preserved verbatim even
# though it is non-English, because it never leaves the session's stream.
NOTICE = "[模型未输出正文，以下为其思考内容]"


class _RunTimingTracker:
  """Tracks monotonic timing milestones during a single _run_cc execution."""

  def __init__(self, session_id: str, backend_type: str, model: str | None) -> None:
    self._session_id = session_id
    self._backend_type = backend_type
    self._model = model
    self._t_start = time.monotonic()
    self._t_spawn: float | None = None
    self._t_first_event: float | None = None
    self._t_first_assistant: float | None = None
    self._saw_first_assistant = False
    # Per-run salvage state: accumulations with the same lifecycle as the timing
    # fields above, created/destroyed with the tracker. Thinking text lands here
    # when the turn never produces assistant text, so teardown can surface it;
    # the result flag does so only for a turn the stream actually settled.
    self._thinking_text: list[str] = []
    self._saw_result = False
    # Zero-output guard state (same lifecycle as the timing fields): whether a
    # terminal result settled with all-zero usage, and whether the turn ever
    # produced thinking content or a tool_use event. Used by the guard at
    # teardown so a genuinely-empty master run fails loudly instead of
    # silently consuming its trigger.
    self._saw_zero_usage = False
    self._saw_thinking = False
    self._saw_tool_use = False

  async def on_spawn(self, pid: int) -> None:
    self._t_spawn = time.monotonic()
    log.info("master_cc_spawned", session=self._session_id, pid=pid, backend=self._backend_type, model=self._model)

  def on_event(self, event: dict) -> None:
    if self._t_first_event is None:
      self._t_first_event = time.monotonic()
      spawn_ref = self._t_spawn if self._t_spawn is not None else self._t_start
      spawn_to_first_ms = int((self._t_first_event - spawn_ref) * 1000)
      log.info(
          "master_cc_first_event",
          session=self._session_id,
          event_type=event.get("type"),
          spawn_to_first_event_ms=spawn_to_first_ms,
      )
      if spawn_to_first_ms > 10_000:
        log.warning(
            "master_cc_slow_first_event",
            session=self._session_id,
            spawn_to_first_event_ms=spawn_to_first_ms,
        )

    if event.get("type") == ET.RESULT:
      self._saw_result = True
      usage = event.get("usage")
      if isinstance(usage, dict) and all(
          usage.get(k, 0) == 0
          for k in ("input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens")
      ):
        self._saw_zero_usage = True

    # Standalone tool_use events (codex/gemini flat format).
    if event.get("type") == ET.TOOL_USE:
      self._saw_tool_use = True

    # Standalone thinking events (opencode/codex deltas) carry their text in
    # "content". Accumulated unconditionally: a turn that later speaks is
    # untouched, and a silent turn gets the whole stream surfaced.
    if event.get("type") == ET.THINKING and event.get("content"):
      self._thinking_text.append(event["content"])
      self._saw_thinking = True

    if not self._saw_first_assistant and event.get("type") == ET.ASSISTANT:
      msg = event.get("message", {})
      content_blocks = msg.get("content") if isinstance(msg, dict) else None
      if content_blocks:
        for block in content_blocks:
          if not isinstance(block, dict):
            continue
          # claude-family thinking blocks nest the text under "thinking".
          if block.get("type") == "thinking" and block.get("thinking"):
            self._thinking_text.append(block["thinking"])
            self._saw_thinking = True
          # Wrapped tool_use blocks (opencode/glm) live in assistant content.
          if block.get("type") == ET.TOOL_USE:
            self._saw_tool_use = True
          if block.get("type") == "text" and block.get("text"):
            self._saw_first_assistant = True
            self._t_first_assistant = time.monotonic()
            log.info(
                "master_cc_first_assistant_text",
                session=self._session_id,
                first_assistant_ms=int((self._t_first_assistant - self._t_start) * 1000),
            )
            break

  def _zero_output_guard(self) -> bool:
    """True when this run settled with zero model output and must fail loudly.

    Four-part conjunction, mirroring the salvage rule's shape: a terminal
    result event was received, its usage is all-zero, and the turn produced no
    assistant text, no thinking content, and no tool_use events. The result
    check is the same single guard for cancellation / let-go / mid-run death
    — none of those reach a result event, so the guard stays quiet for them.
    """
    return (
        self._saw_result
        and self._saw_zero_usage
        and not self._saw_first_assistant
        and not self._saw_thinking
        and not self._saw_tool_use
    )

  def build_finish_extras(self) -> dict:
    total_ms = int((time.monotonic() - self._t_start) * 1000)
    extras: dict = {
        "backend": self._backend_type,
        "model": self._model,
        "total_ms": total_ms,
    }
    if self._zero_output_guard():
      extras["zero_output"] = True
    if self._t_first_event is not None:
      spawn_ref = self._t_spawn if self._t_spawn is not None else self._t_start
      extras["spawn_to_first_event_ms"] = int((self._t_first_event - spawn_ref) * 1000)
    if self._t_first_assistant is not None:
      extras["first_assistant_ms"] = int((self._t_first_assistant - self._t_start) * 1000)
    if total_ms > 120_000:
      log.warning("master_cc_slow_total", session=self._session_id, total_ms=total_ms)
    return extras

  def _salvage_thinking_text(self) -> str | None:
    """Thinking to surface for a silent run, or None when nothing should emit.

    The visibility criterion is assistant text: only a result-settled turn with
    no assistant text and non-empty thinking warrants a salvage. The result
    check doubles as the single guard for user cancellation, let-go handover,
    and mid-run death — none of them reach a result event, so the stream cut
    before it and there is nothing to surface.
    """
    if self._saw_result and not self._saw_first_assistant:
      thinking = "".join(self._thinking_text)
      if thinking.strip():
        return thinking
    return None


async def _salvage_silent_turn(
    tracker: _RunTimingTracker,
    error_msg: str | None,
    session_id: str,
    persist_and_broadcast,
) -> None:
  """Emit accumulated thinking as a visible assistant text event on a silent turn.

  Shared salvage rule for both master run paths. Emits only when all four hold:
  the run saw a terminal result event, it never spoke assistant text, the
  thinking is non-empty, and no error event was already synthesized this turn
  (avoids two contradicting closing messages). The result check is the single
  guard for cancellation / let-go / mid-run death — those never reach a result
  event, so nothing emits. Whole text, never truncated: truncation would
  recreate the incomplete-answer symptom this rule exists to heal.
  """
  if error_msg:
    return
  thinking = tracker._salvage_thinking_text()
  if thinking is None:
    return
  event = make_text_event(f"{NOTICE}\n\n{thinking}")
  await persist_and_broadcast(session_id, event)
  log.info("master_cc_silent_turn_salvaged", session=session_id)


# Per-session FIFO queue for serializing run_message calls.
_session_queues: dict[str, asyncio.Queue] = {}

# One consumer task per session — drains the queue sequentially.
_session_consumers: dict[str, asyncio.Task] = {}

# Per-session running backend reference for external cancellation.
_active_procs: dict[str, AgentBackend] = {}

_CLAUDE_RESUME_FLAG_BACKEND_TYPES = {"cc-claude", "cc-kimi", "cc-openai-compatible"}
_NATIVE_RESUME_SESSION_BACKEND_TYPES = {"codex", "gemini", "opencode", "charlie-code"}
# Every backend that can resume a prior session (via --resume or a native id).
# The pre-flight anchor-missing alarm fires only for these.
_RESUME_CAPABLE_BACKEND_TYPES = _CLAUDE_RESUME_FLAG_BACKEND_TYPES | _NATIVE_RESUME_SESSION_BACKEND_TYPES


@dataclasses.dataclass
class _WorkItem:
  """All arguments needed to execute a single CC run, plus a future for the result."""
  cfg: CharlieBotConfig
  session_meta: SessionMetadata
  user_content: str
  callbacks: SessionCallbacks
  is_voice: bool
  auto_trigger: bool
  backend_option: BackendOption | None
  extra_claude_flags: list[str] | None
  should_check_tex: bool
  future: asyncio.Future
  # True only on the scheduled-session weekly-recycle path that deliberately
  # clears the anchor; suppresses the resume-anchor-missing pre-flight alarm.
  expect_fresh_session: bool = False
  # Chat event id of the user message this turn answers; persisted into
  # master_run so restart reconcile can exclude exactly one event from replay.
  user_event_id: str | None = None
  # Set for re-attach items enqueued by startup reconcile: follow a recorded
  # live turn's raw log instead of spawning a new process.
  resume_record: MasterRunRecord | None = None
  resume_is_alive: Callable[[], bool] | None = None


# Per-session currently-processing work item. Read by queued_user_event_ids so
# startup replay can skip user messages this process already owns — the
# restart-reconcile exclusion must be per-event, never per-session, or a
# message queued behind a running one would be replayed.
_current_items: dict[str, _WorkItem] = {}


# Ambient Project Manager identity: appended after the memory block for
# role=project sessions bound to a group, so the behavior contract travels with
# every master turn in the session (user messages, agent relays, triggers),
# not only scheduled fires. Filled via str.format; keep `{group}` the only
# placeholder.
_PM_IDENTITY_PART = """# Project Manager session

This session is the Project Manager for group {group}. Your behavior
contract is prompts/project_manager.md in the charlie-bot repo: read it
before acting on any message in this session, and follow it."""


def _build_instructions_content(session_meta: SessionMetadata, cfg: CharlieBotConfig) -> str | None:
  """Build master agent instructions: base prompt + per-host override + memory store.

  The memory block is assembled from the labeled-entry store via
  :func:`src.core.memory.assemble_master` (resident topics full text + index
  lines for the rest); it replaces the former three-file concatenation.
  """
  parts: list[str] = []

  # 1. Git-shared base prompt (prompts/master.md in the repo)
  base_prompt_file = cfg.charlie_bot_repo / "prompts" / "master.md"
  if base_prompt_file.exists():
    base_text = base_prompt_file.read_text(encoding="utf-8")
    base_text = base_text.replace("{{session_id}}", session_meta.id)
    parts.append(base_text)

  # 2. Per-host override (~/.charliebot/MASTER_AGENT_PROMPT.md)
  host_prompt_file = cfg.claude_md_file
  if host_prompt_file.exists():
    host_text = host_prompt_file.read_text(encoding="utf-8")
    host_text = host_text.replace("YOUR_SESSION_UUID", session_meta.id)
    parts.append(host_text)

  if not parts:
    log.warning("master_prompt_files_missing", base=str(base_prompt_file), host=str(host_prompt_file))
    return None

  # 3. Memory store (resident topics full text + index lines for the rest)
  memory_block = assemble_master(cfg.memory_dir)
  if memory_block:
    parts.append(memory_block)

  # 4. Ambient Project Manager identity (role=project sessions bound to a group)
  if session_meta.role == PROJECT_ROLE and session_meta.group:
    parts.append(_PM_IDENTITY_PART.format(group=session_meta.group))

  return "\n\n".join(parts)


_VOICE_DISCLAIMER = (
    "[Voice input: this message was dictated via speech transcription and may "
    "contain recognition errors. Interpret unclear words from context; ask only "
    "when the intent is genuinely ambiguous.]")


def _build_prompt(user_content: str, is_voice: bool) -> str:
  if is_voice:
    return _VOICE_DISCLAIMER + "\n" + user_content
  return user_content


def _cc_transcript_exists(config_dir: Path, cc_session_id: str) -> bool:
  """True when *config_dir* holds a resumable transcript for *cc_session_id*.

  Top-level conversations live at projects/<cwd-slug>/<uuid>.jsonl; the glob avoids
  depending on Claude Code's undocumented cwd-slug rule. Files nested deeper are
  subagent logs named agent-*.jsonl and cannot collide with a conversation uuid.
  """
  return any((config_dir / "projects").glob(f"*/{cc_session_id}.jsonl"))


def _resolve_resume_id(option: BackendOption, session_meta: SessionMetadata) -> str | None:
  """Return the cc_session_id to resume, or None when it is not reachable.

  Each cc-claude account has its own CLAUDE_CONFIG_DIR and cannot see another's
  conversations, so resuming an id recorded under a different account always fails.
  Backends with native resume ids carry no local transcript and pass through.
  """
  cc_session_id = session_meta.cc_session_id
  if not cc_session_id:
    return None
  if option.type not in _CLAUDE_RESUME_FLAG_BACKEND_TYPES:
    return cc_session_id
  config_dir = claude_config_dir(option)
  if _cc_transcript_exists(config_dir, cc_session_id):
    return cc_session_id
  log.warning(
      "master_cc_resume_transcript_missing",
      session=session_meta.id,
      cc_session_id=cc_session_id,
      config_dir=str(config_dir),
  )
  return None


def _route_resume_session(backend_type: str, cc_session_id: str | None) -> tuple[list[str], str | None]:
  """Return CLI resume flags and native resume ID for a backend type."""
  if not cc_session_id:
    return [], None
  if backend_type in _CLAUDE_RESUME_FLAG_BACKEND_TYPES:
    return ["--resume", cc_session_id], None
  if backend_type in _NATIVE_RESUME_SESSION_BACKEND_TYPES:
    return [], cc_session_id
  return [], None


def _build_master_env(cfg: CharlieBotConfig) -> dict[str, str]:
  """Build the environment for the master backend subprocess."""
  env = {**os.environ}
  env.pop("CLAUDECODE", None)
  env.pop("CHARLIEBOT_SESSION_ID", None)
  env["GIT_CEILING_DIRECTORIES"] = str(cfg.charliebot_home)
  env["CLAUDE_CODE_DISABLE_AUTO_MEMORY"] = "1"

  venv_bin = cfg.charlie_bot_repo / ".venv" / "bin"
  if venv_bin.is_dir():
    existing_path = env.get("PATH")
    env["PATH"] = str(venv_bin) if not existing_path else f"{venv_bin}{os.pathsep}{existing_path}"

  return env


async def _handle_event(
    event: dict,
    session_id: str,
    cc_session_id: str | None,
    persist_and_broadcast,
) -> str | None:
  """Process a single backend event: persist, broadcast, and handle compaction events.

  Returns the cc_session_id (possibly updated from the event).
  """
  if not cc_session_id:
    sid = event.get("session_id")
    if sid:
      cc_session_id = sid

  # Persist first (injects timestamp), then broadcast with timestamp included
  await persist_and_broadcast(session_id, event)

  await handle_compaction_events(
      event,
      persist_and_broadcast=lambda evt: persist_and_broadcast(session_id, evt),
      log_context={"session": session_id},
  )

  return cc_session_id


async def _run_cc(item: _WorkItem) -> tuple[str | None, int, str | None, dict]:
  """Execute a single CC run — spawn backend, stream events.

  Manages _active_procs for cancel support.  Does NOT broadcast MASTER_DONE
  or manage thinking state (the consumer loop handles that).

  Returns (cc_session_id, exit_code, error_msg, finish_extras).
  """
  cfg = item.cfg
  session_meta = item.session_meta
  session_dir = cfg.sessions_dir / session_meta.id
  session_dir.mkdir(parents=True, exist_ok=True)
  cwd = str(session_dir)

  instructions_content = await asyncio.to_thread(_build_instructions_content, session_meta, cfg)

  from src.agents.backends.registry import build_backend
  option = item.backend_option
  # A caller that passed no option must not silently inherit backend_options[0]:
  # the session's own pin is the explicit choice and takes precedence.
  if option is None and session_meta.backend:
    option = cfg.get_backend_option(session_meta.backend)
  if option is None:
    if session_meta.backend and cfg.get_backend_option(session_meta.backend) is None:
      # The session pins a backend id config.yaml no longer defines. Substituting a
      # different backend would silently run on another model and another account.
      fallback_id = cfg.backend_options[0].id if cfg.backend_options else "(none)"
      msg = (
          f"backend '{session_meta.backend}' is not in config.yaml backend_options — "
          f"refusing to substitute '{fallback_id}'; this run did not execute.")
      log.error(
          "master_cc_backend_unresolved",
          session=session_meta.id,
          requested=session_meta.backend,
          fallback=fallback_id,
      )
      await item.callbacks.persist_and_broadcast(
          session_meta.id, {
              "type": ET.ASSISTANT_ERROR,
              "content": f"Agent error: {msg}"
          })
      await item.callbacks.mark_unread(session_meta.id)
      return None, 1, msg, {}
    option = cfg.backend_options[0]
  if backend_type_allows_missing_model(option.type) and option.model is not None:
    option = option.model_copy(update={"model": None})

  resume_id = _resolve_resume_id(option, session_meta)
  # Pre-flight: a resume-capable backend about to run with no resolved resume
  # id, when the session already has an anchor on disk or a completed round, is
  # about to start a zero-context conversation. Fail loudly unless the caller
  # declared a fresh start (the scheduled-session weekly-recycle path).
  if (option.type in _RESUME_CAPABLE_BACKEND_TYPES and not resume_id
      and not item.expect_fresh_session):
    anchor_on_disk = session_meta.cc_session_id
    if anchor_on_disk or await item.callbacks.has_completed_round(session_meta.id):
      reason = "transcript_missing" if anchor_on_disk else "anchor_missing"
      log.error(
          "master_cc_resume_anchor_missing",
          session=session_meta.id,
          backend=option.type,
          reason=reason,
      )
      await item.callbacks.persist_and_broadcast(
          session_meta.id, {
              "type": ET.RESUME_CONTEXT_DROPPED,
              "reason": reason,
          })
  extra_flags, resume_session_id = _route_resume_session(option.type, resume_id)
  # Move per-machine sections (cwd, env info, memory paths, git status) out of the
  # system prompt into the first user message. Keeps the system prompt stable across
  # sessions so cross-run prompt-cache reuse improves. Only the Claude Code CLI
  # family supports this flag.
  if option.type in _CLAUDE_RESUME_FLAG_BACKEND_TYPES:
    extra_flags = [*extra_flags, "--exclude-dynamic-system-prompt-sections"]
  resume_session = bool(resume_id)
  # Gate on backend capability, not on a resume-id variable: a backend outside
  # _RESUME_CAPABLE_BACKEND_TYPES cannot resume any prior session, so a session
  # that carries an anchor (cc_session_id) is misconfigured regardless of
  # whether this round resolved a reachable resume id. Keying off resume_id or
  # resume_session_id would silence the warning whenever the id is absent (fresh
  # start, unreachable transcript) and let the misconfiguration pass undetected.
  if session_meta.cc_session_id and option.type not in _RESUME_CAPABLE_BACKEND_TYPES:
    log.warning("master_cc_resume_unsupported_backend", session=session_meta.id, backend=option.type)
  if item.extra_claude_flags:
    extra_flags.extend(item.extra_claude_flags)

  env = _build_master_env(cfg)

  prompt = _build_prompt(item.user_content, item.is_voice)

  log.info(
      "master_cc_starting",
      session=session_meta.id,
      backend=option.type,
      model=option.model,
      prompt_chars=len(prompt),
      resume_session=resume_session,
      cwd=cwd,
  )

  cc_session_id: str | None = session_meta.cc_session_id
  exit_code = 1
  error_msg: str | None = None
  # Set inside _on_spawn the moment the master_run record hits disk; the cancel
  # path lets the turn go only once a boot can find it, so this flag — not
  # backend.pid — is the let-go precondition.
  record_persisted = False
  # True only on the cancel path when the turn is handed to the next boot: the
  # finally block then skips every terminal state write.
  let_go = False

  tracker = _RunTimingTracker(session_meta.id, option.type, option.model)
  backend: AgentBackend | None = None

  # Per-turn transport dir: the backend pins its raw NDJSON log, stderr log,
  # and read cursor here so a restarted server can re-attach to this exact
  # turn from the persisted master_run record.
  started_at = datetime.now(timezone.utc)
  log_dir = cfg.sessions_dir / session_meta.id / "data" / "master_runs" / started_at.isoformat()
  raw_log = str(log_dir / runs.RAW_LOG_NAME)

  async def _on_spawn(pid: int) -> None:
    nonlocal record_persisted
    await tracker.on_spawn(pid)
    # pid_start was pinned to this exact process instance just before this
    # callback fired — same contract as the worker path — so the pair cannot
    # be faked by a later pid reuse.
    assert backend is not None
    record = MasterRunRecord(
        pid=pid,
        pid_start=backend.pid_start,
        started_at=started_at,
        raw_log=raw_log,
        user_event_id=item.user_event_id,
    )
    await item.callbacks.persist_master_run(session_meta.id, record)
    record_persisted = True

  try:
    backend = build_backend(
        option,
        cfg,
        extra_flags=extra_flags or None,
        buffer_limit=cfg.subprocess_buffer_limit,
        on_spawn=_on_spawn,
        instructions_content=instructions_content,
        resume_session_id=resume_session_id,
        log_dir=log_dir,
    )
    _active_procs[session_meta.id] = backend

    async for event in backend.run(prompt, cwd, env):
      tracker.on_event(event)
      cc_session_id = await _handle_event(event, session_meta.id, cc_session_id, item.callbacks.persist_and_broadcast)

    exit_code = backend.exit_code
    if backend.stderr_text:
      log.warning("master_cc_stderr", session=session_meta.id, stderr=backend.stderr_text)
      if exit_code != 0 and not backend.terminated:
        error_msg = backend.stderr_text[:500]

  except asyncio.CancelledError:
    # Only live trigger: event-loop shutdown (graceful restart). Same let-go
    # rule as the worker path (spawner.py): a covered transport whose
    # master_run record is already persisted keeps running on its own raw-log
    # fds, and the next boot's reconcile re-attaches for the real result.
    # Uncovered transports die with their transport process, and a process
    # whose record never hit disk can never be found by a boot, so both are
    # still terminated.
    let_go = (backend is not None and option.type not in runs.UNCOVERED_BACKEND_TYPES
              and record_persisted)
    log.warning(
        "master_cc_cancelled",
        session=session_meta.id,
        transport=option.type,
        action="let_go" if let_go else "terminate",
    )
    if backend:
      if let_go:
        backend.detach()
      else:
        await backend.terminate()
    _active_procs.pop(session_meta.id, None)
    raise
  except Exception as e:
    log.exception("master_cc_crashed", session=session_meta.id)
    error_msg = str(e)

  finally:
    _active_procs.pop(session_meta.id, None)
    finish_extras = tracker.build_finish_extras()

    if error_msg:
      err_event = {"type": ET.ASSISTANT_ERROR, "content": f"Agent error: {error_msg}"}
      await item.callbacks.persist_and_broadcast(session_meta.id, err_event)

    # Salvage a run that settled with only thinking and no assistant text. This
    # sits before and outside the let-go branch below: a user-cancelled run
    # still enters that branch, but it also still reaches a result event only
    # when the stream genuinely settled — the salvage helper's result check is
    # what distinguishes "finished silently" from "cancelled mid-stream".
    await _salvage_silent_turn(
        tracker, error_msg, session_meta.id, item.callbacks.persist_and_broadcast)

    # On the let-go path the turn is still running in another process: writing
    # any terminal state (unread marker, tex snapshot, finished log) would lie
    # about it. The next boot's reconcile owns the outcome of this turn.
    if not let_go:
      await item.callbacks.mark_unread(session_meta.id)

      if item.should_check_tex:
        proposal = await asyncio.to_thread(check_tex_changed)
        if proposal:
          tex_event = {'type': ET.TEX_EDIT_PROPOSED}
          await item.callbacks.persist_and_broadcast(session_meta.id, tex_event)
          log.info('tex_edit_proposed', session=session_meta.id)
        else:
          clear_snapshot()

      log.info(
          "master_cc_finished",
          session=session_meta.id,
          exit_code=exit_code,
          **(finish_extras or {}),
      )

  return cc_session_id, exit_code, error_msg, finish_extras


def _resolve_resume_option(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    backend_option: BackendOption | None,
) -> BackendOption | None:
  """Pick the backend option a resume follows with (translate ownership only)."""
  if backend_option is not None:
    return backend_option
  if session_meta.backend:
    option = cfg.get_backend_option(session_meta.backend)
    if option is not None:
      return option
    log.warning("master_cc_resume_backend_unresolved", session=session_meta.id, backend=session_meta.backend)
  return cfg.backend_options[0] if cfg.backend_options else None


def _build_fresh_translate(cfg: CharlieBotConfig, option: BackendOption | None) -> Callable[[dict], list[dict]]:
  """A fresh translate_event callable for one scan/stream.

  Stateful translates (codex text buffering, gemini) require one instance per
  stream. A missing/unbuildable option degrades to the identity translate (raw
  claude shape) instead of failing the re-attach — same rule as the worker
  side's reconcile translate.
  """
  if option is None:
    return lambda event: [event]
  try:
    from src.agents.backends.registry import build_backend
    return build_backend(option, cfg).translate_event
  except Exception as e:
    log.warning("master_cc_resume_translate_unresolved", backend=option.id, error=str(e))
    return lambda event: [event]


async def _kill_run_group_escalating(pid: int, is_alive: Callable[[], bool]) -> None:
  """SIGTERM the run's process group; escalate to SIGKILL when it outlives the 5 s grace."""
  kill_process_group(pid, signal.SIGTERM)
  deadline = time.monotonic() + 5.0
  while is_alive() and time.monotonic() < deadline:
    await asyncio.sleep(0.2)
  if is_alive():
    kill_process_group(pid, signal.SIGKILL)


async def _resume_cc(item: _WorkItem) -> tuple[str | None, int, str | None, dict]:
  """Re-attach to a recorded live master turn: follow its raw log to the end.

  Consumer-side mirror of _run_cc with no spawn: the same per-event handling,
  the same finish logging. Only the truth source differs — liveness comes from
  the caller's (pid, pid_start) closure instead of an in-process handle, and
  the exit code is derived from the raw log's trailing result event (a
  detached process's real exit code is unreachable). Managed by the same
  per-session consumer, so the re-attach drains before any queued turn spawns.
  """
  cfg = item.cfg
  session_meta = item.session_meta
  record = item.resume_record
  assert record is not None and item.resume_is_alive is not None
  is_alive = item.resume_is_alive

  raw_path = Path(record.raw_log)
  log_dir = raw_path.parent
  cursor_path = log_dir / runs.CURSOR_NAME
  stderr_path = log_dir / runs.STDERR_LOG_NAME

  option = _resolve_resume_option(cfg, session_meta, item.backend_option)
  log.info("master_cc_resuming", session=session_meta.id, pid=record.pid, raw_log=record.raw_log)

  cc_session_id: str | None = session_meta.cc_session_id
  exit_code = -1
  error_msg: str | None = None
  tracker = _RunTimingTracker(
      session_meta.id, option.type if option else "unknown", option.model if option else None)

  try:
    stream_translate = _build_fresh_translate(cfg, option)
    async for event in tail_follow_events(
        raw_path,
        translate=stream_translate,
        is_alive=is_alive,
        cursor=cursor_path,
        start_offset=runs.read_raw_cursor(cursor_path),
        post_result_timeout=AgentBackend._POST_RESULT_TIMEOUT,
    ):
      tracker.on_event(event)
      cc_session_id = await _handle_event(event, session_meta.id, cc_session_id, item.callbacks.persist_and_broadcast)

    # Whole-file result scan with a FRESH translate instance (stateful
    # translates may not reuse the one that consumed the stream tail). -1
    # matches the worker side's "no result event" code for died-mid-run.
    if raw_path.is_file():
      events = runs.project_raw_events(
          runs.parse_raw_lines(raw_path.read_bytes()), _build_fresh_translate(cfg, option))
    else:
      events = []
    result = runs.summarize_result(events)
    exit_code = 0 if (result is not None and runs.result_success(result)) else -1

    stderr_text = await asyncio.to_thread(_read_stderr_tail, stderr_path)
    if stderr_text:
      log.warning("master_cc_stderr", session=session_meta.id, stderr=stderr_text)
      if exit_code != 0:
        error_msg = stderr_text[:500]

    # The loop ended on the post-result timeout: same contract as the live
    # path's cleanup — SIGTERM the recorded process group, escalate to
    # SIGKILL. An irreversible kill is authorized only by the record's own
    # liveness proof (is_run_alive); the follower's is_alive probe stays the
    # stream's liveness input and is constant-true for an unpinned record,
    # which must never authorize a kill.
    if record.pid is not None:
      host_boot = await asyncio.to_thread(runs.read_host_boot_time)

      def record_alive() -> bool:
        return runs.is_run_alive(record.pid, record.pid_start, record.started_at, host_boot)

      if record_alive():
        log.warning("master_cc_resumed_run_hung_after_result", session=session_meta.id, pid=record.pid)
        await _kill_run_group_escalating(record.pid, record_alive)

  except asyncio.CancelledError:
    log.warning("master_cc_resume_cancelled", session=session_meta.id)
    raise
  except Exception as e:
    log.exception("master_cc_resume_crashed", session=session_meta.id)
    cc_session_id = None
    error_msg = str(e)

  finally:
    finish_extras = tracker.build_finish_extras()
    if error_msg:
      err_event = {"type": ET.ASSISTANT_ERROR, "content": f"Agent error: {error_msg}"}
      await item.callbacks.persist_and_broadcast(session_meta.id, err_event)
    await _salvage_silent_turn(
        tracker, error_msg, session_meta.id, item.callbacks.persist_and_broadcast)
    await item.callbacks.mark_unread(session_meta.id)
    log.info(
        "master_cc_resume_finished",
        session=session_meta.id,
        exit_code=exit_code,
        **(finish_extras or {}),
    )

  return cc_session_id, exit_code, error_msg, finish_extras


def _enqueue_work_item(session_id: str, work_item: _WorkItem) -> tuple[datetime, bool]:
  """Atomically mark busy, queue the item, and ensure a consumer exists.

  No await and no statement that can raise: a work item in the queue always
  implies busy_since is set; the consumer clears it only at teardown, in its
  own await-free sequence. No other statement may interleave with this block —
  correctness of the busy-state invariant depends on that.
  """
  if session_id not in _session_queues:
    _session_queues[session_id] = asyncio.Queue()
  # A resume item is re-attaching a turn that already started, so its busy
  # interval begins at the recorded start rather than at this enqueue.
  resumed = work_item.resume_record
  thinking_since, created = mark_busy(session_id, since=resumed.started_at if resumed else None)
  _session_queues[session_id].put_nowait(work_item)
  if session_id not in _session_consumers or _session_consumers[session_id].done():
    _session_consumers[session_id] = asyncio.create_task(
        _session_consumer(session_id),
        name=f"master-consumer-{session_id[:8]}",
    )
  return thinking_since, created


async def _session_consumer(session_id: str) -> None:
  """Drain the per-session queue sequentially, one CC run at a time."""
  queue = _session_queues[session_id]
  # Relay cc_session_id across items: queued _WorkItems may carry distinct
  # SessionMetadata instances (e.g. fork bootstrap vs. user message loaded later).
  last_cc_session_id: str | None = None
  # Teardown context for the idle RUNNING_CHANGED broadcast, captured per item
  # so the finally never reads the loop variable — `item` is unbound when the
  # consumer exits (e.g. via cancellation) before the first queue.get() returns.
  teardown_cfg: CharlieBotConfig | None = None
  teardown_auto_trigger = False
  try:
    while True:
      item: _WorkItem = await queue.get()
      _current_items[session_id] = item
      teardown_cfg = item.cfg
      teardown_auto_trigger = item.auto_trigger
      try:
        # Carry the previous run's cc_session_id onto a freshly-loaded meta
        # so --resume picks up the in-progress CC transcript.
        if last_cc_session_id and not item.session_meta.cc_session_id:
          item.session_meta.cc_session_id = last_cc_session_id
        result = await (_resume_cc(item) if item.resume_record is not None else _run_cc(item))
        cc_session_id, exit_code, _error_msg, finish_extras = result

        # Zero-output guard: a run that settled with a result event of all-zero
        # usage and no assistant text/thinking/tool_use must fail loudly — the
        # backend reported it as done but produced nothing, so the triggering
        # message would otherwise be consumed silently. Lives on the single
        # MASTER_DONE path, so both the fresh-run and resume outcomes are
        # covered by construction. If an error event was already emitted this
        # turn (error_msg set), skip a second contradicting ERROR but still
        # exit nonzero — mirrors the salvage rule's error_msg gate. mark_unread
        # is already invoked by both run paths' teardown.
        if finish_extras.get("zero_output"):
          if not _error_msg:
            resume_ref = cc_session_id if cc_session_id else "fresh session"
            zero_err = make_error_event(
                f"Master run produced zero model output (cc_session_id={resume_ref}) "
                f"and the triggering message was left unread. "
                f"See LESSONS.md: opencode message-ID wraparound, 2026-08-14.")
            await item.callbacks.persist_and_broadcast(session_id, zero_err)
          exit_code = 1

        # Update session_meta.cc_session_id for subsequent queued runs.
        if cc_session_id:
          item.session_meta.cc_session_id = cc_session_id
          last_cc_session_id = cc_session_id
          # The consumer is the single owner of persisting the resume anchor:
          # every round, unconditionally, with no comparison against any
          # in-memory value. The read-back verifies the write landed on disk.
          read_back = await item.callbacks.persist_cc_session_id(session_id, cc_session_id)
          if read_back != cc_session_id:
            log.error(
                "resume_anchor_persist_mismatch",
                session=session_id,
                written=cc_session_id,
                read_back=read_back,
            )
            await item.callbacks.persist_and_broadcast(session_id, {
                "type": ET.ERROR,
                "source": "resume_anchor",
                "message": (
                    f"Resume anchor persist mismatch: wrote {cc_session_id!r}, "
                    f"read back {read_back!r} from disk"),
            })

        # Computed once, with no re-check: a queued item keeps this round's
        # busy interval alive, so the only question is whether one is queued.
        still_thinking = not queue.empty()

        # Broadcast MASTER_DONE. thinking_seconds is the length of the
        # continuous busy interval reported by thinking_state, attached only
        # when this round leaves the queue empty.
        thinking_seconds = None
        if not still_thinking:
          busy_start = busy_since(session_id)
          if busy_start is not None:
            thinking_seconds = int((datetime.now(timezone.utc) - busy_start).total_seconds())

        done_event = {"type": ET.MASTER_DONE, "exit_code": exit_code, "still_thinking": still_thinking}
        if item.user_event_id:
          done_event["input_event_id"] = item.user_event_id
        if thinking_seconds is not None:
          done_event["thinking_seconds"] = thinking_seconds
        done_event.update(finish_extras)
        await item.callbacks.persist_and_broadcast(session_id, done_event)

        # The turn is fully resolved — clear its restart-identity so the next
        # startup reconcile neither re-attaches nor replays its user message.
        # Written after MASTER_DONE: crash between the two replays the message
        # with a marker (duplicate-tolerant) rather than silently dropping it.
        await item.callbacks.persist_master_run(session_id, None)

        # Resolve the caller's future
        if not item.future.done():
          item.future.set_result(cc_session_id)

      except Exception as exc:
        log.exception("session_consumer_item_error", session=session_id)
        if not item.future.done():
          item.future.set_exception(exc)

      finally:
        queue.task_done()
        _current_items.pop(session_id, None)

      # If queue is empty, exit the consumer loop — it will be re-created lazily.
      if queue.empty():
        break
  finally:
    # Await-free teardown: the loop's queue.empty() exit check, this
    # deregistration, and the busy-state clear form one synchronous sequence —
    # an enqueue cannot interleave inside it, so a new work item either extends
    # the current consumer (loop sees a non-empty queue) or starts a fresh
    # consumer that re-marks busy. No await is allowed in this sequence; that
    # property is what makes the busy-state invariant hang-free.
    _session_consumers.pop(session_id, None)
    # Clean up the queue if empty to avoid memory leaks from abandoned sessions.
    if session_id in _session_queues and _session_queues[session_id].empty():
      _session_queues.pop(session_id, None)
    clear_busy(session_id)
    if teardown_cfg is not None:
      # Check if workers are still running before declaring idle.
      from src.core.sessions import SessionManager
      workers_running = await SessionManager(teardown_cfg)._has_running_tasks(session_id)
      await streaming_manager.broadcast(
          "sidebar", {
              "type": ET.RUNNING_CHANGED,
              "session_id": session_id,
              "has_running_tasks": workers_running,
              "thinking_since": None,
              "auto_trigger": teardown_auto_trigger,
          })


async def run_message(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    user_content: str,
    callbacks: SessionCallbacks,
    skip_user_event: bool = False,
    auto_trigger: bool = False,
    backend_option: BackendOption | None = None,
    extra_claude_flags: list[str] | None = None,
    display_content: str | None = None,
    uploaded_files: list[dict] | None = None,
    is_voice: bool = False,
    expect_fresh_session: bool = False,
    user_event_id: str | None = None,
) -> str | None:
  """Spawn a Claude Code process for the master agent and stream NDJSON events.

  Args:
    cfg: App configuration.
    session_meta: The session to run in.
    user_content: The user's message text.
    callbacks: Bundle of session hooks (persist_and_broadcast,
      update_thinking_state, mark_unread, persist_cc_session_id,
      has_completed_round).
    skip_user_event: If True, skip persisting/broadcasting the user event
      (used when the master is triggered by a worker completion, not a real user message).
    display_content: User-visible content persisted to the chat log. Defaults
      to ``user_content`` when omitted.
    uploaded_files: Structured uploaded-file metadata persisted on the user event.
    expect_fresh_session: True only on the scheduled-session weekly-recycle
      path that deliberately clears the anchor; suppresses the
      resume-anchor-missing pre-flight alarm.
    user_event_id: Chat event id of the user message this turn answers;
      recorded in master_run so restart reconcile excludes exactly it from
      replay. Only pass explicitly on the replay path (skip_user_event=True);
      otherwise captured from the freshly persisted user event.

  Returns:
    The CC session ID (for --resume on subsequent messages), or None.
  """
  # tui-cli sessions are interactive terminal sessions: messages flow through
  # tmux, not the SDK. Skip the master agent entirely so we never spawn a
  # claude SDK subprocess for them.
  backend_id = session_meta.backend or (cfg.backend_options[0].id if cfg.backend_options else "")
  backend_lookup = cfg.get_backend_option(backend_id)
  if backend_lookup is not None and backend_lookup.type == "tui-cli":
    log.info("master_cc_skip_tui_backend", session=session_meta.id, backend=backend_id)
    return None

  session_dir = cfg.sessions_dir / session_meta.id
  session_dir.mkdir(parents=True, exist_ok=True)

  tex_path = get_tex_path()
  should_check_tex = tex_path.exists()
  if should_check_tex:
    await asyncio.to_thread(snapshot_tex)

  # Persist the user message so it survives page refresh (WebSocket catch-up).
  # All awaits in run_message happen here, BEFORE the atomic enqueue block
  # below — between mark_busy and put_nowait nothing may yield or raise.
  if not skip_user_event:
    user_event = {
        "type": ET.USER,
        "content": user_content if display_content is None else display_content,
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "is_voice": is_voice,
    }
    if uploaded_files:
      user_event["uploaded_files"] = uploaded_files
    await callbacks.persist_and_broadcast(session_meta.id, user_event)
    user_event_id = user_event.get("id")
    session_meta.updated_at = datetime.now(timezone.utc)
    await callbacks.update_thinking_state(session_meta.id, updated_at=session_meta.updated_at)

  # Create a future for the caller to await.
  loop = asyncio.get_running_loop()
  future: asyncio.Future = loop.create_future()

  work_item = _WorkItem(
      cfg=cfg,
      session_meta=session_meta,
      user_content=user_content,
      callbacks=callbacks,
      is_voice=is_voice,
      auto_trigger=auto_trigger,
      backend_option=backend_option,
      extra_claude_flags=extra_claude_flags,
      should_check_tex=should_check_tex,
      future=future,
      expect_fresh_session=expect_fresh_session,
      user_event_id=user_event_id,
  )

  # --- atomic enqueue block: no await, no statement that can raise ---
  # Busy state is marked before the item enters the queue, so a work item in
  # the queue always implies busy_since is set; the consumer clears it only at
  # teardown, in its own await-free sequence.
  thinking_since, created = _enqueue_work_item(session_meta.id, work_item)
  # --- end atomic block ---

  # Notify only when this call opened a new busy interval; the broadcast is a
  # pure notification — correctness comes from readers deriving the state.
  if created:
    await streaming_manager.broadcast(
        'sidebar', {
            'type': ET.RUNNING_CHANGED,
            'session_id': session_meta.id,
            'has_running_tasks': True,
            'thinking_since': thinking_since.isoformat(),
            'auto_trigger': auto_trigger,
        })

  # Await until this specific work item completes.
  return await future


async def cancel_master(
    session_id: str,
    *,
    meta: SessionMetadata | None = None,
    session_mgr: "SessionManager | None" = None,
) -> bool:
  """Terminate the running master CC turn for this session.

  In-process hit: terminate the live backend. In-process miss with session
  metadata: the turn may have detached across a graceful restart, so fall
  back to the on-disk master_run record — an irreversible kill goes out only
  when ``runs.is_run_alive`` proves the recorded (pid, pid_start, started_at)
  triple still names a live process; an unprovable record gets no signal at
  all and the endpoint keeps its 404. Callers that pass neither optional keep
  the in-memory-only behavior.

  Returns True if a turn was found and signalled, False otherwise.
  """
  log.info("master_cancel_requested", session=session_id)
  backend = _active_procs.get(session_id)
  if backend is not None:
    await backend.terminate()
    log.info("master_cancel_succeeded", session=session_id)
    return True

  record = meta.master_run if meta is not None else None
  if record is not None and session_mgr is not None:
    host_boot = await asyncio.to_thread(runs.read_host_boot_time)

    def _alive() -> bool:
      return runs.is_run_alive(record.pid, record.pid_start, record.started_at, host_boot)

    if _alive() and record.pid is not None:
      # Detached turn still running: the record's own liveness proof authorized
      # this kill.
      log.info("master_cancel_killing_detached_run", session=session_id, pid=record.pid)
      await _kill_run_group_escalating(record.pid, _alive)
      await session_mgr.persist_master_run(session_id, None)
      log.info("master_cancel_succeeded", session=session_id)
      return True

  log.info("master_cancel_no_active_master", session=session_id)
  return False


async def enqueue_master_resume(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    record: MasterRunRecord,
    callbacks: SessionCallbacks,
    *,
    is_alive: Callable[[], bool],
) -> asyncio.Future:
  """Re-attach a recorded live master turn by queueing a resume-follow item.

  Goes through the normal per-session queue: the re-attached turn always
  drains before any queued or replayed turn spawns a new CLI against the same
  conversation. Returns the future the consumer resolves with the followed
  turn's cc_session_id when its MASTER_DONE lands.
  """
  loop = asyncio.get_running_loop()
  future: asyncio.Future = loop.create_future()
  work_item = _WorkItem(
      cfg=cfg,
      session_meta=session_meta,
      user_content="",
      callbacks=callbacks,
      is_voice=False,
      auto_trigger=False,
      backend_option=None,
      extra_claude_flags=None,
      should_check_tex=False,
      future=future,
      user_event_id=record.user_event_id,
      resume_record=record,
      resume_is_alive=is_alive,
  )
  # Atomic (see _enqueue_work_item); the broadcast below is a pure
  # notification — correctness comes from readers deriving the state.
  thinking_since, created = _enqueue_work_item(session_meta.id, work_item)
  if created:
    await streaming_manager.broadcast(
        'sidebar', {
            'type': ET.RUNNING_CHANGED,
            'session_id': session_meta.id,
            'has_running_tasks': True,
            'thinking_since': thinking_since.isoformat(),
            'auto_trigger': False,
        })
  return future


def queued_user_event_ids(session_id: str) -> set[str]:
  """Chat event ids of user messages this process already owns (running or queued).

  Startup reconcile excludes exactly these (plus any recorded turn's id) from
  replay: within one process the queue survives, so replaying a queued message
  would answer it twice. The exclusion is per-event, never per-session — a
  message queued BEHIND a crashed turn disappears with the killed process and
  must be replayed.
  """
  ids: set[str] = set()
  current = _current_items.get(session_id)
  if current is not None and current.user_event_id:
    ids.add(current.user_event_id)
  queue = _session_queues.get(session_id)
  if queue is not None:
    for item in list(queue._queue):  # same-process snapshot; safe under the GIL
      if item.user_event_id:
        ids.add(item.user_event_id)
  return ids


_REPLAY_MARKER = (
    "[System: the server restarted while answering this message, so it is "
    "being redelivered. Your previous, interrupted attempt may already have "
    "performed some actions — before repeating any side effect (files "
    "written, messages sent, tasks delegated), read back that action's state "
    "first and continue from there instead.]")


async def replay_user_message(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    user_event: dict,
    callbacks: SessionCallbacks,
) -> None:
  """Redeliver an unanswered user message after a restart, marked as a replay.

  The original user event stays put in the chat log (skip_user_event); the
  replayed prompt prefixes ``_REPLAY_MARKER`` so the master checks prior side
  effects before redoing them. The record's user_event_id keeps pointing at
  the ORIGINAL event, which is the one startup reconcile must exclude.
  """
  content = user_event.get("content")
  if not isinstance(content, str) or not content:
    log.error("master_replay_unusable_event", session=session_meta.id, event_id=user_event.get("id"))
    return
  await run_message(
      cfg,
      session_meta,
      user_content=f"{_REPLAY_MARKER}\n\n{content}",
      callbacks=callbacks,
      skip_user_event=True,
      is_voice=bool(user_event.get("is_voice")),
      user_event_id=user_event.get("id"),
  )
