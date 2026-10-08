"""Master CC turn execution — spawn or re-attach one backend run and stream its events."""

import asyncio
import os
import time
from collections.abc import Awaitable, Callable
from datetime import UTC, datetime
from pathlib import Path

from src.infra import event_types as ET
from src.infra.config import CLAUDE_CONFIG_DIR_ENV_VAR, CharlieBotConfig
from src.infra.constants import SESSION_ID_ENV_VAR
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import (
    BackendOption,
    MasterRunRecord,
    SessionMetadata,
    backend_type_allows_missing_model,
)
from src.infra.ndjson import type_line_filter
from src.infra.process import kill_group_escalating
from src.runtime import launch_loop, master_cc_state, runs
from src.runtime.agent_process.base import AgentBackend, _read_stderr_tail, make_text_event, tail_follow_events
from src.runtime.hooks import backend_lifecycle, backend_type_registration, backend_types, turn_contributions
from src.runtime.streaming import handle_compaction_events

log = LazyStructlogLogger()

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
      event.get("type") == ET.SYSTEM and event.get("subtype") == ET.COMPACT_BOUNDARY and
      (event.get(ET.COMPACT_METADATA) or {}).get("trigger") == "manual")


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
          usage.get(k, 0) == 0 for k in (ET.USAGE_INPUT_TOKENS, ET.USAGE_OUTPUT_TOKENS,
                                         ET.USAGE_CACHE_READ_INPUT_TOKENS, ET.USAGE_CACHE_CREATION_INPUT_TOKENS)):
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

    Six-part conjunction, mirroring the salvage rule's shape: a terminal
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
        self._saw_result and self._saw_zero_usage and not self._saw_first_assistant and not self._saw_thinking and
        not self._saw_tool_use and not self._saw_manual_compact)

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


# The turn-end attribution's parse bound: assistant lines only. ET.ASSISTANT
# is the raw stream's own type name here — a lifecycle with round notices
# belongs to a backend that runs the claude CLI and inherits the identity
# translate — so a raw line head-proving another type cannot reach the
# notice detector.
_ASSISTANT_LINE_FILTER = type_line_filter(frozenset({ET.ASSISTANT}))

_VOICE_DISCLAIMER = (
    "[Voice input: this message was dictated via speech transcription and may "
    "contain recognition errors. Interpret unclear words from context; ask only "
    "when the intent is genuinely ambiguous.]")


def _build_prompt(user_content: str, is_voice: bool) -> str:
  if is_voice:
    return _VOICE_DISCLAIMER + "\n" + user_content
  return user_content


def _route_resume_session(backend_type: str, cc_session_id: str | None) -> tuple[list[str], str | None]:
  """Return CLI resume flags and native resume ID for a backend type."""
  if not cc_session_id:
    return [], None
  resume = backend_types.traits_for(backend_type).resume
  if resume == backend_type_registration.RESUME_CLI_FLAG:
    return ["--resume", cc_session_id], None
  if resume == backend_type_registration.RESUME_NATIVE_ID:
    return [], cc_session_id
  raise ValueError(f"unknown resume style {resume!r} for backend type {backend_type}")


def _build_extra_flags(
    option: BackendOption,
    launch: backend_lifecycle.Launch,
    item: master_cc_state._WorkItem,
) -> tuple[list[str], str | None]:
  """CLI flags and native resume id for one spawn of this turn.

  Shared by the first spawn and every relay: the flags are the type's resume flag, the flags
  the lifecycle's launch carries in ``backend_kwargs["extra_flags"]``, and the turn's own extra
  flags, in that order.
  """
  extra_flags, resume_session_id = _route_resume_session(option.type, launch.resume_id)
  extra_flags = [*extra_flags, *launch.backend_kwargs.get("extra_flags", [])]
  if item.extra_claude_flags:
    extra_flags.extend(item.extra_claude_flags)
  return extra_flags, resume_session_id


def _build_master_env(cfg: CharlieBotConfig, session_id: str) -> dict[str, str]:
  """Build the environment for the master backend subprocess.

  ``CHARLIEBOT_SESSION_ID`` carries this master's own session identity, so the
  session-scoped CLIs the master runs resolve to it wherever the shell cd's to
  (``src.runtime.cli.common.resolve_session_id``). ``launch_loop.child_env`` strips any
  inherited value first, so a server started from inside another session's
  environment hands down no stale id. PATH is the inherited one: it already
  carries the ``charliebot`` shim (``src.runtime.agent_environment``), and a venv
  bin directory on it would give uv an install target.
  """
  env = launch_loop.child_env(os.environ)
  env[SESSION_ID_ENV_VAR] = session_id
  env["GIT_CEILING_DIRECTORIES"] = str(cfg.charliebot_home)
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


def _agent_error_event(msg: str, quota_exhausted: bool | None) -> dict:
  """The ASSISTANT_ERROR chat event of a failed turn.

  *quota_exhausted* is the refusal's flag when the turn ended on a ``LaunchRefused`` and None for
  every other failure: only a launch-refusal event carries the ``QUOTA_EXHAUSTED`` key.
  """
  event: dict = {"type": ET.ASSISTANT_ERROR, "content": f"Agent error: {msg}"}
  if quota_exhausted is not None:
    event[ET.QUOTA_EXHAUSTED] = quota_exhausted
  return event


async def _report_turn_error_and_salvage(
    tracker: _RunTimingTracker,
    item: master_cc_state._WorkItem,
    error_msg: str | None,
    quota_exhausted: bool | None,
) -> None:
  """Terminal event pair shared by the _run_cc and _resume_cc finally blocks.

  A non-None *error_msg* becomes one ASSISTANT_ERROR event; the silent-turn
  salvage then sees the same value and suppresses itself, so an errored turn
  never also emits salvaged thinking.
  """
  session_id = item.session_meta.id
  if error_msg:
    await item.callbacks.persist_and_broadcast(session_id, _agent_error_event(error_msg, quota_exhausted))
  await _salvage_silent_turn(tracker, error_msg, session_id, item.callbacks.persist_and_broadcast)


async def _refuse_turn(item: master_cc_state._WorkItem, msg: str,
                       quota_exhausted: bool | None) -> tuple[None, int, str, dict]:
  """Fail a turn before any backend spawn: one error event in chat, the triggering message left unread.

  Returns the run's refusal shape: no cc_session_id, exit code 1, the message, no finish extras.
  """
  await item.callbacks.persist_and_broadcast(item.session_meta.id, _agent_error_event(msg, quota_exhausted))
  await item.callbacks.mark_unread(item.session_meta.id)
  return None, 1, msg, {}


def _resolve_turn_option(item: master_cc_state._WorkItem) -> BackendOption | None:
  """The backend option a live turn runs on: the item's own, else the session's pin, else None.

  A caller that passed no option must not silently inherit backends.options[0]: the session's
  own pin is the explicit choice and takes precedence.
  """
  if item.backend_option is not None:
    return item.backend_option
  if item.session_meta.backend:
    return item.cfg.get_backend_option(item.session_meta.backend)
  return None


def _turn_launch_context(
    item: master_cc_state._WorkItem,
    option: BackendOption,
    cwd: str,
    held_native_id: str | None,
) -> backend_lifecycle.LaunchContext:
  """The ``LaunchContext`` of one master turn: events go to the session, the account label to its funnel."""
  session_meta = item.session_meta
  callbacks = item.callbacks

  async def emit(event: dict) -> None:
    await callbacks.persist_and_broadcast(session_meta.id, event)

  async def record_account(label: str) -> None:
    if callbacks.persist_account_label is not None:
      await callbacks.persist_account_label(session_meta.id, label)

  async def context_state() -> tuple[int | None, datetime | None]:
    if callbacks.context_state is None:
      return None, None
    return await callbacks.context_state(session_meta.id, session_meta)

  return backend_lifecycle.LaunchContext(
      cfg=item.cfg,
      option=option,
      session_meta=session_meta,
      kind="turn",
      cwd=cwd,
      held_native_id=held_native_id,
      preassigned_native_id=None,
      emit=emit,
      record_account=record_account,
      context_state=context_state)


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

  option = _resolve_turn_option(item)
  if option is None:
    if session_meta.backend:
      # The session pins a backend id config.yaml no longer defines.
      # lazy: spawner_backends→review→master_trigger→master_cc would close a cycle
      # through this module if imported at top level.
      from src.runtime.spawner_backends import unknown_backend_pin_refusal
      fallback_id = cfg.backends.options[0].id if cfg.backends.options else "(none)"
      msg = (f"backend {unknown_backend_pin_refusal(session_meta.backend, fallback_id)}; "
             "this run did not execute.")
      log.error(
          "master_cc_backend_unresolved",
          session=session_meta.id,
          requested=session_meta.backend,
          fallback=fallback_id,
      )
    else:
      # Neither an explicit per-run option nor a session pin. Refusing the
      # backends.options[0] fallback avoids silently running on an arbitrary
      # backend; error and exit 1. (The sibling fallback inside
      # _resolve_resume_option stays, deliberately out of scope.)
      msg = (
          "no backend option was given and this session pins none — refusing to "
          "fall back to backends.options[0]; this run did not execute.")
      log.error(
          "master_cc_backend_unresolved",
          session=session_meta.id,
          requested="(none)",
      )
    return await _refuse_turn(item, msg, None)
  if backend_type_allows_missing_model(option.type) and option.model is not None:
    option = option.model_copy(update={"model": None})
  # A contribution may set the session's context window in place of the option's
  # own (a thread session runs the fixed thread window): the option entry is
  # shared with main sessions and workers, so its window must keep serving
  # them, and a thread session is born without a backend of its own to pin a
  # narrower entry on. Only a backend type that reads a context window takes it;
  # other backend types are untouched.
  if backend_types.traits_for(option.type).reads_context_window:
    context_window = turn_contributions.resolve_context_window(session_meta)
    if context_window is not None:
      option = option.model_copy(update={"context_window": context_window})

  assert item.task_run is not None
  assert item.task_instructions is not None
  instructions_content = item.task_instructions

  # A fresh-native launch starts without the previous conversation. The
  # adapter clears the durable anchor when the Run spawns.
  fresh_native = item.task_run.fresh_native_context
  if fresh_native:
    session_meta.cc_session_id = None
  held_native_id = None if fresh_native else session_meta.cc_session_id
  lifecycle = backend_types.lifecycle_for(option)
  ctx = _turn_launch_context(item, option, cwd, held_native_id)
  env = _build_master_env(cfg, session_meta.id)
  if item.extra_env:
    # The v2 adapter's child identity (its own session id, signed run token,
    # selected home) rides on top of the supervisor env.
    env.update(item.extra_env)

  prompt = _build_prompt(item.user_content, item.is_voice)

  # A fresh task turn starts a new conversation, so its state starts empty: a
  # new id from the backend is adoptable, and a round that lands none returns
  # None, so the consumer's persist leaves the disk's old id and producer
  # untouched for the next turn to judge again. The fresh-native adapter
  # clears the snapshot before launch.
  cc_session_id: str | None = None if fresh_native else session_meta.cc_session_id
  exit_code = 1
  error_msg: str | None = None
  # The launch refusal's flag when the turn ended on one; None for every other failure.
  quota_exhausted: bool | None = None
  # Set inside _on_spawn after the task Run's process identity lands on disk;
  # the cancel path lets the turn go only once recovery can find it, so this flag — not
  # backend.pid — is the let-go precondition.
  record_persisted = False
  # True only on the cancel path when the turn is handed to the next boot: the
  # finally block then skips every terminal state write.
  let_go = False

  tracker = _RunTimingTracker(session_meta.id, option.type, option.model)
  backend: AgentBackend | None = None
  # Relays this turn performed (a backend with a login pool only).
  relays = 0

  # The Run directory holds the backend transport files.
  log_dir = Path(item.task_run.transport_dir)
  raw_log = str(log_dir / runs.RAW_LOG_NAME)

  # The invocation's own translated error-event messages, in stream order — the
  # raw material for the end-of-run error hint (runs.select_error_hint). Reset
  # at each invocation's start, so a relayed round's error never outlives its
  # own round.
  error_event_messages: list[str] = []

  async def _on_spawn(pid: int) -> None:
    nonlocal record_persisted
    await tracker.on_spawn(pid)
    # pid_start was pinned to this exact process instance just before this
    # callback fired — same contract as the worker path — so the pair cannot
    # be faked by a later pid reuse.
    assert backend is not None
    assert item.on_task_spawn is not None
    await item.on_task_spawn(pid, backend.pid_start)
    record_persisted = True

  def _on_relay(relay_count: int) -> None:
    nonlocal relays
    relays = relay_count

  async def _run_process(
      process_launch: backend_lifecycle.Launch,
      watch: backend_lifecycle.LaunchWatch | None,
      relays_before: int,
  ) -> tuple[int, str]:
    """One process of this turn: build the backend, stream its events, record its exit."""
    nonlocal backend, exit_code, cc_session_id, record_persisted, log_dir, raw_log, relays
    relays = relays_before
    error_event_messages.clear()
    if backend is not None:
      record_persisted = False
    raw_log = str(log_dir / runs.RAW_LOG_NAME)
    process_prompt = prompt
    if relays_before == 0:
      resume_id = process_launch.resume_id
      # Pre-flight catches a missing transcript when a durable anchor exists.
      if not resume_id and not fresh_native:
        if session_meta.cc_session_id:
          reason = ET.RESUME_REASON_TRANSCRIPT_MISSING
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
      log.info(
          "master_cc_starting",
          session=session_meta.id,
          backend=option.type,
          model=option.model,
          prompt_chars=len(process_prompt),
          resume_session=bool(resume_id),
          cwd=cwd,
          account=process_launch.account_label,
      )
    process_env = dict(env)
    if process_launch.account_label is not None:
      # The pool chose the login directory; an inherited CLAUDE_CONFIG_DIR must never shadow it.
      process_env.pop(CLAUDE_CONFIG_DIR_ENV_VAR, None)
    spawn_flags, spawn_resume_id = _build_extra_flags(option, process_launch, item)
    launch_kwargs = {key: value for key, value in process_launch.backend_kwargs.items() if key != "extra_flags"}
    backend = backend_types.build_backend(
        option,
        cfg,
        **launch_kwargs,
        extra_flags=spawn_flags or None,
        buffer_limit=cfg.subprocess_buffer_limit,
        on_spawn=_on_spawn,
        instructions_content=instructions_content,
        resume_session_id=spawn_resume_id,
        log_dir=log_dir,
        cgroup_session_id=session_meta.id,
    )
    master_cc_state._active_procs[session_meta.id] = backend

    spawn_prompt = process_launch.prompt if process_launch.prompt is not None else process_prompt
    async for event in backend.run(spawn_prompt, cwd, process_env, uploaded_files=item.uploaded_files):
      tracker.on_event(event)
      if event.get("type") == ET.ERROR:
        error_event_messages.append(event.get("message", ""))
      cc_session_id = await _handle_event(event, session_meta.id, cc_session_id, item.callbacks.persist_and_broadcast)
      if watch is not None and watch.observe(event):
        # Armed relay at its safe point: the tool result is on disk, stop here.
        await backend.terminate()

    exit_code = backend.exit_code
    if backend.stderr_text:
      log.warning("master_cc_stderr", session=session_meta.id, stderr=backend.stderr_text)
    return exit_code, backend.stderr_text

  try:
    try:
      relays = await launch_loop.run_launches(
          ctx,
          lifecycle,
          run_process=_run_process,
          native_id=lambda: cc_session_id,
          on_relay=_on_relay,
      )
    except backend_lifecycle.LaunchRefused as refusal:
      log.error(
          "master_cc_launch_refused",
          session=session_meta.id,
          error=str(refusal),
          quota_exhausted=refusal.quota_exhausted)
      error_msg = str(refusal)
      exit_code = 1
      quota_exhausted = refusal.quota_exhausted
    else:
      assert backend is not None
      if exit_code != 0 and not backend.terminated:
        # The invocation's own structured error event outranks the stderr
        # help banner (runs.select_error_hint); an explicit user stop keeps
        # today's no-hint behavior, and the cgroup report below still wins
        # over both channels.
        error_msg = runs.select_error_hint(error_event_messages, backend.stderr_text)
      # Session memory-cap / host-OOM attribution: the
      # routing report supersedes a bare stderr tail ("Killed") whenever the
      # cgroup's counters moved.
      error_msg = backend.cgroup_exit_report() or error_msg

    # Turn-end attribution: a lifecycle that reads the round's events (the Claude CLI
    # family reports a visible reply that a model outside the pinned family wrote) gets
    # this invocation's own raw log re-read through the same whole-file projection the
    # re-attach path uses (fresh translate) — no detection state accumulates in the
    # stream loop. The notices land after the round's own events; a backend without
    # round notices emits nothing.
    if backend is not None and launch_loop.has_round_notices(lifecycle):
      raw_path = Path(raw_log)
      if not raw_path.is_file():
        # A live cc round always has one (run() creates the raw log before
        # spawn); the guard mirrors the re-attach path's and keeps backend
        # doubles that model only the event stream from failing the turn.
        # Fail open with a warning — the notice is advisory.
        log.warning("master_cc_fallback_notice_raw_log_missing", session=session_meta.id, raw_log=raw_log)
      else:
        # The projection is a full read of the turn's raw log (tens of ms on a
        # multi-MB turn) — off the loop it stops freezing every concurrent
        # request and WebSocket at turn end, the same shape as the git-diff
        # hop. The parse bounds itself to the assistant lines the detector
        # reads: the claude-family raw stream leads every line with its type
        # and the family's translate is the identity, so the echoed user
        # context (~98% of a multi-MB round's bytes) never parses; a head the
        # filter cannot read parses anyway, keeping a foreign raw shape on
        # the whole-file projection.
        turn_events = await asyncio.to_thread(
            runs.project_raw_file, raw_path, _build_fresh_translate(cfg, option), _ASSISTANT_LINE_FILTER)
        await _emit_round_notices(item, option, turn_events)

  except asyncio.CancelledError:
    # A covered transport with a persisted Run identity can be followed after
    # restart; an uncovered process is terminated during shutdown.
    let_go = (backend is not None and backend_types.traits_for(option.type).restart_reattach and record_persisted)
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
    if backend is None:
      cc_session_id = None
    else:
      # The backend id this round actually ran on, for the consumer's round-end
      # anchor persist (the option resolved above, never the request's hint).
      finish_extras["native_backend"] = option.id
    if relays:
      finish_extras["account_relays"] = relays

    # The pair runs before the let-go branch below: a let-go turn still gets
    # its error event and silent-turn salvage; only the terminal state writes
    # (unread marker, finished log) would lie about a turn that
    # keeps running in another process.
    await _report_turn_error_and_salvage(tracker, item, error_msg, quota_exhausted)

    # On the let-go path the turn is still running in another process: writing
    # any terminal state (unread marker, finished log) would lie
    # about it. The next boot's reconcile owns the outcome of this turn.
    if not let_go:
      await item.callbacks.mark_unread(session_meta.id)

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
  return cfg.backends.options[0] if cfg.backends.options else None


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
    return backend_types.build_backend(option, cfg).translate_event
  except Exception as e:
    log.warning("master_cc_resume_translate_unresolved", backend=option.id, error=str(e))
    return lambda event: [event]


async def _emit_round_notices(item: master_cc_state._WorkItem, option: BackendOption, events: list[dict]) -> None:
  """Persist and broadcast the notices the backend's lifecycle reads from the finished round's events."""
  for notice in backend_types.lifecycle_for(option).round_notices(option, events):
    await item.callbacks.persist_and_broadcast(item.session_meta.id, notice)


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
  tracker = _RunTimingTracker(session_meta.id, option.type if option else "unknown", option.model if option else None)

  try:
    stream_translate = _build_fresh_translate(cfg, option)
    async for event in tail_follow_events(
        raw_path,
        translate=stream_translate,
        is_alive=is_alive,
        cursor=cursor_path,
        start_offset=runs.read_raw_cursor(cursor_path),
        post_result_timeout=AgentBackend._POST_RESULT_TIMEOUT,
        buffer_limit=cfg.subprocess_buffer_limit,
    ):
      tracker.on_event(event)
      cc_session_id = await _handle_event(event, session_meta.id, cc_session_id, item.callbacks.persist_and_broadcast)

    events, _, exit_code = await asyncio.to_thread(runs.scan_result_exit, raw_path, _build_fresh_translate(cfg, option))
    # Recover the manual-compaction observation from the same whole-file
    # projection the result summary uses (zero new I/O): the persisted cursor
    # may already sit past the boundary line, so the cursor-forward tail above
    # cannot be the observation's only evidence channel.
    if any(_is_manual_compact_boundary(event) for event in events):
      tracker.note_manual_compact()

    stderr_text = await asyncio.to_thread(_read_stderr_tail, stderr_path)
    if stderr_text:
      log.warning("master_cc_stderr", session=session_meta.id, stderr=stderr_text)
    if exit_code != 0:
      # Same selection rule as the live path, fed from the whole-file
      # projection above (zero new I/O): an error event sitting before the
      # persisted cursor is still found — the manual-compaction recovery
      # pattern in this block. The stderr tail is only the fallback.
      error_msg = runs.select_error_hint(
          [event.get("message", "") for event in events if event.get("type") == ET.ERROR], stderr_text)

    # Same turn-end model attribution on the re-attach path: the whole-round
    # projection above is reused (zero new I/O) and the identical notice is
    # emitted. One turn's lifecycle takes exactly one of the two completion
    # paths (a completed live turn writes a terminal Run fact, so a re-attach
    # implies the live path never completed), so no double emit.
    if option is not None:
      await _emit_round_notices(item, option, events)

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
    await _report_turn_error_and_salvage(tracker, item, error_msg, None)
    await item.callbacks.mark_unread(session_meta.id)
    log.info(
        "master_cc_resume_finished",
        session=session_meta.id,
        exit_code=exit_code,
        **(finish_extras or {}),
    )

  return cc_session_id, exit_code, error_msg, finish_extras
