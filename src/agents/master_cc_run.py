"""Master CC turn execution — spawn or re-attach one backend run and stream its events."""

import asyncio
import os
import time
from collections.abc import Awaitable, Callable
from datetime import datetime, timezone
from pathlib import Path

import structlog

from src.agents import master_cc_state
from src.agents.backends.base import (
  AgentBackend,
  _read_stderr_tail,
  make_text_event,
  tail_follow_events,
)
from src.core import event_types as ET
from src.core import runs
from src.core.config import CharlieBotConfig, claude_config_dir
from src.core.latex import check_tex_changed, clear_snapshot
from src.core.memory import assemble_master
from src.core.models import (
  PROJECT_ROLE,
  BackendOption,
  MasterRunRecord,
  SessionMetadata,
  backend_type_allows_missing_model,
)
from src.core.process import kill_group_escalating
from src.core.streaming import handle_compaction_events

log = structlog.get_logger()

# Prefixed to a salvaged silent turn so the user sees the thinking the model
# produced instead of nothing. Local chat-stream only: preserved verbatim even
# though it is non-English, because it never leaves the session's stream.
NOTICE = "[模型未输出正文，以下为其思考内容]"


def _is_manual_compact_boundary(event: dict) -> bool:
  """True for a compact_boundary system event whose trigger is exactly "manual".

  Exact-string match only: an "auto" boundary is followed by mandatory model
  output (silence there is the zero-output guard's own failure class), and
  unknown/absent triggers fail loud — neither may exempt the turn.
  """
  return (
      event.get("type") == "system"
      and event.get("subtype") == "compact_boundary"
      and (event.get("compact_metadata") or {}).get("trigger") == "manual"
  )


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
    # terminal result settled with all-zero usage, whether the turn ever
    # produced thinking content or a tool_use event, and whether the turn
    # observed a manual-compaction boundary. Used by the guard at teardown so
    # a genuinely-empty master run fails loudly instead of silently consuming
    # its trigger — while a manual /compact turn, whose healthy completion IS
    # the compaction itself, stays exempt.
    self._saw_zero_usage = False
    self._saw_thinking = False
    self._saw_tool_use = False
    self._saw_manual_compact = False

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

    # Fresh-path evidence channel for the manual-compaction observation (the
    # re-attach path's whole-file projection is the other one).
    if _is_manual_compact_boundary(event):
      self._saw_manual_compact = True

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

  def note_manual_compact(self) -> None:
    """Latch the manual-compaction observation from a whole-file projection.

    Re-attach counterpart of the live-stream latch in on_event: the persisted
    read cursor may already sit past the boundary line (a pre-restart process
    consumed it), so the cursor-forward tail cannot be its only source.
    """
    self._saw_manual_compact = True

  def _zero_output_guard(self) -> bool:
    """True when this run settled with zero model output and must fail loudly.

    Five-part conjunction, mirroring the salvage rule's shape: a terminal
    result event was received, its usage is all-zero, and the turn produced no
    assistant text, no thinking content, no tool_use events, and no manual
    compaction boundary. The result check is the same single guard for
    cancellation / let-go / mid-run death — none of those reach a result
    event, so the guard stays quiet for them. The manual-compaction clause
    exempts the /compact turn, whose output is the compaction itself; an
    auto-compact boundary must be followed by model output, so it never
    exempts a silent turn.
    """
    return (
        self._saw_result
        and self._saw_zero_usage
        and not self._saw_first_assistant
        and not self._saw_thinking
        and not self._saw_tool_use
        and not self._saw_manual_compact
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
    persist_and_broadcast: Callable[[str, dict], Awaitable[None]],
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


_CLAUDE_RESUME_FLAG_BACKEND_TYPES = {"cc-claude", "cc-kimi", "cc-openai-compatible"}
_NATIVE_RESUME_SESSION_BACKEND_TYPES = {"codex", "gemini", "opencode", "charlie-code", "antigravity"}
# Every backend that can resume a prior session (via --resume or a native id).
# The pre-flight anchor-missing alarm fires only for these.
_RESUME_CAPABLE_BACKEND_TYPES = _CLAUDE_RESUME_FLAG_BACKEND_TYPES | _NATIVE_RESUME_SESSION_BACKEND_TYPES


# Ambient Project Manager identity: appended after the memory block for
# role=project sessions bound to a group, so the behavior contract travels with
# every master turn in the session (user messages, agent relays, triggers),
# not only scheduled fires. Filled via str.format; keep `{group}` the only
# placeholder.
_PM_IDENTITY_PART = """# Project Manager session

This session is the Project Manager for group {group}. Your behavior
contract is prompts/project_manager.md in the charlie-bot repo: read it
before acting on any message in this session, and follow it."""


class _Instructions(str):
  """Instructions string carrying the overlay read failure, when one occurred.

  The builder's return must stay a plain ``str`` for every consumer, so a
  declared overlay's read failure rides upward as this attribute instead of a
  changed return shape. The wake path reads it to log and emit the unified
  ``backend_overlay_inactive`` alert; it is ``None`` on every other path.
  """

  overlay_error: OSError | UnicodeDecodeError | None = None


def _build_instructions_content(session_meta: SessionMetadata, cfg: CharlieBotConfig, prompt_overlay: str | None) -> str | None:
  """Build master agent instructions: base prompt + per-host override + memory store.

  The memory block is assembled from the labeled-entry store via
  :func:`src.core.memory.assemble_master` (resident topics full text + index
  lines for the rest); it replaces the former three-file concatenation.

  *prompt_overlay* names a file under ``prompts/model_overlays/`` (without the
  ``.md`` suffix) whose full text is appended as the final part. The backend
  declares it explicitly. A declared-but-unreadable file (``OSError`` /
  ``UnicodeDecodeError``) does not raise: the overlay segment is skipped and
  the failure rides upward on the returned string's ``overlay_error``
  attribute — this function stays a pure builder and emits no events; the
  caller (the wake path) owns logging and the ``backend_overlay_inactive``
  alert. Any other exception type still propagates. ``None`` appends nothing.
  The ``model`` string never enters this function — the overlay binding is
  wholly driven by the declaration.
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

  # 5. Declared overlay (prompts/model_overlays/<prompt_overlay>.md). The read
  # degrades, never raises for missing/unreadable files: OSError and
  # UnicodeDecodeError skip the overlay segment and ride upward on the
  # result's overlay_error attribute — the wake continues without a fence and
  # _run_cc logs and emits the unified backend_overlay_inactive alert with
  # reason="unreadable". Any other exception type still propagates.
  overlay_error: OSError | UnicodeDecodeError | None = None
  if prompt_overlay is not None:
    overlay_file = cfg.charlie_bot_repo / "prompts" / "model_overlays" / f"{prompt_overlay}.md"
    try:
      parts.append(overlay_file.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError) as exc:
      overlay_error = exc

  content = _Instructions("\n\n".join(parts))
  content.overlay_error = overlay_error
  return content


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
    persist_and_broadcast: Callable[[str, dict], Awaitable[None]],
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


async def _run_cc(item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
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

  from src.agents.backends.registry import build_backend
  option = item.backend_option
  # A caller that passed no option must not silently inherit backend_options[0]:
  # the session's own pin is the explicit choice and takes precedence.
  if option is None and session_meta.backend:
    option = cfg.get_backend_option(session_meta.backend)
  if option is None:
    if session_meta.backend:
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
    else:
      # Neither an explicit per-run option nor a session pin. Refusing the
      # backend_options[0] fallback avoids silently running on an arbitrary
      # backend; error and exit 1. (The sibling fallback inside
      # _resolve_resume_option stays, deliberately out of scope.)
      msg = (
          "no backend option was given and this session pins none — refusing to "
          "fall back to backend_options[0]; this run did not execute.")
      log.error(
          "master_cc_backend_unresolved",
          session=session_meta.id,
          requested="(none)",
      )
    await item.callbacks.persist_and_broadcast(
        session_meta.id, {
            "type": ET.ASSISTANT_ERROR,
            "content": f"Agent error: {msg}"
        })
    await item.callbacks.mark_unread(session_meta.id)
    return None, 1, msg, {}
  if backend_type_allows_missing_model(option.type) and option.model is not None:
    option = option.model_copy(update={"model": None})

  # Three-state overlay judgment on the wake path, never at config-load time
  # (a hot reload would swallow a load-time exception as a warning and keep the
  # old config). None (absent key or explicit YAML null) = undeclared: run
  # without a fence and emit the unified overlay-inactive alert with
  # reason="undeclared"; the literal string "none" = explicitly no overlay:
  # pass None, silent, no alert; any other string names the overlay file
  # (without ".md") under prompts/model_overlays/, read by the builder — a
  # read failure degrades the same way as undeclared: fenceless run plus the
  # same unified alert with reason="unreadable", never a raise.
  prompt_overlay = option.prompt_overlay
  if prompt_overlay is None:
    log.warning("master_cc_overlay_undeclared", session=session_meta.id, backend=option.id)
    await item.callbacks.persist_and_broadcast(
        session_meta.id, {
            "type": ET.BACKEND_OVERLAY_INACTIVE,
            "backend": option.id,
            "reason": "undeclared",
        })
  elif prompt_overlay == "none":
    prompt_overlay = None
  # Any other string is the overlay filename (sans ".md"); pass it through.

  instructions_content = await asyncio.to_thread(_build_instructions_content, session_meta, cfg, prompt_overlay)
  overlay_error = getattr(instructions_content, "overlay_error", None)
  if overlay_error is not None:
    log.warning(
        "master_cc_overlay_unreadable",
        session=session_meta.id,
        backend=option.id,
        overlay=prompt_overlay,
        error=type(overlay_error).__name__,
        detail=str(overlay_error),
    )
    await item.callbacks.persist_and_broadcast(
        session_meta.id, {
            "type": ET.BACKEND_OVERLAY_INACTIVE,
            "backend": option.id,
            "reason": "unreadable",
            "overlay": prompt_overlay,
            "error": type(overlay_error).__name__,
        })

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
    master_cc_state._active_procs[session_meta.id] = backend

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
    master_cc_state._active_procs.pop(session_meta.id, None)
    raise
  except Exception as e:
    log.exception("master_cc_crashed", session=session_meta.id)
    error_msg = str(e)

  finally:
    master_cc_state._active_procs.pop(session_meta.id, None)
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


async def _resume_cc(item: master_cc_state._WorkItem) -> tuple[str | None, int, str | None, dict]:
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
    # Recover the manual-compaction observation from the same whole-file
    # projection the result summary uses (zero new I/O): the persisted cursor
    # may already sit past the boundary line, so the cursor-forward tail above
    # cannot be the observation's only evidence channel.
    if any(_is_manual_compact_boundary(event) for event in events):
      tracker.note_manual_compact()
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
      alive = runs.run_alive_probe(record.pid, record.pid_start, record.started_at, host_boot)
      if alive():
        log.warning("master_cc_resumed_run_hung_after_result", session=session_meta.id, pid=record.pid)
        await kill_group_escalating(record.pid, alive)

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
