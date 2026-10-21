"""Idle compaction of a cc-claude master session, run before its one-hour prompt cache expires.

A request renews the session's one-hour prompt cache. A session that stays idle past the hour reads
its whole context cold on its next message. About 54 minutes after a master round ends, this module
compacts the session with its own command line (model, account and turn flags), so the compaction
reads the cache that round wrote. The Sonnet compaction of claude_compaction cannot do that: it runs
another model, which cannot read this cache.

Everything here is in memory. A server restart drops the timers, and nothing re-arms them at start.

Vocabulary:

- *timer*: the pending trigger of one session, armed when a master round ends. A second arm replaces it.
- *trigger path*: ``fire``. It reads the session's facts, decides with ``should_compact``, and registers
  one *running compaction*.
- *running compaction*: the task of one idle run. A master turn that starts waits for it (``settle_for_turn``).
"""

from __future__ import annotations

import asyncio
import dataclasses
import datetime

from src.backends.claude_code import (
    claude_accounts,
    claude_code,
    claude_compaction,
    claude_config,
    claude_metadata,
    login_dirs,
)
from src.infra import config, log_once, models, tasks
from src.infra import event_types as ET
from src.runtime import master_cc_run, master_cc_state, model_family, session_events, session_store
from src.runtime.hooks import backend_lifecycle, backend_types

log = log_once.LazyStructlogLogger()

# The timer fires at 0.9 of the cache lifetime, so the compaction request lands while the entry is live.
IDLE_DELAY = claude_compaction.CACHE_TTL * 0.9

# The trigger of the compaction's chat events.
TRIGGER = "idle"


@dataclasses.dataclass
class _Timer:
  task: asyncio.Task
  ctx: backend_lifecycle.LaunchContext
  # The last request time the timer was armed for; the trigger path compares it with the current one.
  armed_at: datetime.datetime


_timers: dict[str, _Timer] = {}
_running: dict[str, asyncio.Task] = {}


@dataclasses.dataclass(frozen=True)
class IdleFacts:
  """What the trigger path reads for one session. ``turn_busy`` is read last, in the registration block."""
  archived: bool
  backend_unchanged: bool
  turn_busy: bool
  armed_at: datetime.datetime | None
  last_request_at: datetime.datetime | None
  compacted_after_request: bool
  context_tokens: int | None
  resume_id: str | None
  transcript_present: bool


def should_compact(facts: IdleFacts, *, idle_tokens: int, now: datetime.datetime) -> bool:
  """True when every condition for an idle compaction holds.

  The session is not archived, still runs on the backend its timer was armed for, and has no turn
  running or queued. Its last request is the one the timer was armed for and is less than one cache
  lifetime old. No compaction followed that request, the context reads at least *idle_tokens*, and
  the session's transcript sits in its account directory.
  """
  return (
      not facts.archived and facts.backend_unchanged and not facts.turn_busy and facts.last_request_at is not None and
      facts.last_request_at == facts.armed_at and now - facts.last_request_at < claude_compaction.CACHE_TTL and
      not facts.compacted_after_request and facts.context_tokens is not None and facts.context_tokens >= idle_tokens and
      facts.resume_id is not None and facts.transcript_present)


async def arm(ctx: backend_lifecycle.LaunchContext) -> None:
  """Arm the session's timer at the end of a master round; a second arm replaces the first.

  The timer fires at the round's last request time plus IDLE_DELAY. A round with no request arms nothing.
  """
  _, last_request_at = await ctx.context_state()
  session_id = ctx.session_meta.id
  _disarm(session_id)
  if last_request_at is None:
    return
  delay = (last_request_at + IDLE_DELAY - models.utc_now()).total_seconds()
  task = tasks.create_logged_task(_fire_after(session_id, delay), name=f"idle-timer-{session_id[:8]}")
  _timers[session_id] = _Timer(task=task, ctx=ctx, armed_at=last_request_at)


async def settle_for_turn(session_id: str) -> None:
  """Before a master turn starts: drop the session's pending timer and wait out its running compaction."""
  _disarm(session_id)
  running = _running.get(session_id)
  if running is not None:
    # Shielded: cancelling the turn must not cancel the compaction it waits for.
    await asyncio.shield(running)


async def fire(session_id: str, now: datetime.datetime | None = None) -> asyncio.Task | None:
  """The trigger path: decide on the session's facts and, when every condition holds, start one compaction.

  Returns the started compaction's task, or None when the session has no timer or does not qualify.
  *now* is the decision's clock; None reads the wall clock.
  """
  timer = _timers.get(session_id)
  if timer is None:
    return None
  _disarm(session_id)
  moment = models.utc_now() if now is None else now
  cfg = config.get_config()
  meta = await session_store.store().read_metadata_fresh(session_id)
  if meta is None:
    return None
  context_tokens, last_request_at = await timer.ctx.context_state()
  compacted_after_request = last_request_at is not None and await asyncio.to_thread(
      _compacted_after, session_id, last_request_at)
  option = timer.ctx.option
  account = claude_accounts.account_by_label(cfg, claude_metadata.account_of(meta))
  if account is None and not claude_accounts.is_pooled(option, cfg):
    # Unpooled cc-claude sessions keep their transcript in the process login directory, not in
    # accounts.claude metadata. Wrap that directory as the account target used by the shared runner.
    account = claude_config.ClaudeAccount(label="default", config_dir=str(login_dirs.claude_config_dir()))
  resume_id = meta.cc_session_id
  transcript_present = (
      account is not None and resume_id is not None and
      bool(claude_accounts.transcript_matches(account.config_dir, resume_id)))
  # No await from here to the registration. A turn queued before this point counts as busy and the
  # compaction does not start; a turn that starts after it waits for the compaction (settle_for_turn).
  facts = IdleFacts(
      archived=meta.status == models.SessionStatus.ARCHIVED,
      backend_unchanged=meta.backend == option.id,
      turn_busy=master_cc_state.session_busy(session_id),
      armed_at=timer.armed_at,
      last_request_at=last_request_at,
      compacted_after_request=compacted_after_request,
      context_tokens=context_tokens,
      resume_id=resume_id,
      transcript_present=transcript_present)
  if not should_compact(facts, idle_tokens=cfg.accounts.claude_compaction.idle_tokens, now=moment):
    return None
  # should_compact requires a transcript in the account directory and a reading at or above the floor.
  assert account is not None and resume_id is not None and context_tokens is not None
  task = tasks.create_logged_task(
      _compact(timer.ctx, cfg, option, account, resume_id, context_tokens), name=f"idle-compaction-{session_id[:8]}")
  _running[session_id] = task
  return task


def _disarm(session_id: str) -> None:
  """Drop the session's timer and cancel it, unless the caller is that timer's own task."""
  timer = _timers.pop(session_id, None)
  if timer is not None and timer.task is not asyncio.current_task():
    timer.task.cancel()


async def _fire_after(session_id: str, delay: float) -> None:
  await asyncio.sleep(delay)
  await fire(session_id)


def _compacted_after(session_id: str, last_request_at: datetime.datetime) -> bool:
  """True when the session's newest context_compacted event is later than *last_request_at*."""
  for event in reversed(session_events.events().load_chat_events_sync(session_id)):
    if event.get("type") == ET.CONTEXT_COMPACTED and isinstance(event.get("timestamp"), str):
      return models.parse_utc_datetime(event["timestamp"]) > last_request_at
  return False


def _compaction_process(
    cfg: config.CharlieBotConfig,
    option: models.BackendOption,
    account: claude_config.ClaudeAccount,
    session_id: str,
    cc_session_id: str,
    run_kind: models.RunKind,
) -> tuple[list[str], dict[str, str]]:
  """The command line and environment of the session's next turn on *account*, the process the compaction runs as.

  The turn's extra flags resume the conversation and keep the per-machine sections out of the system
  prompt. No instructions content is passed, so the session's CLAUDE.md keeps the last turn's content.
  *run_kind* is the kind of Run the compacted session serves, so the process declares the window the
  session's own turns declare.
  """
  backend = backend_types.build_backend(
      option,
      cfg,
      claude_account=account,
      run_kind=run_kind,
      extra_flags=["--resume", cc_session_id, claude_code.EXCLUDE_DYNAMIC_SECTIONS_FLAG],
      cgroup_session_id=session_id)
  process_env = master_cc_run._build_master_env(cfg, session_id)
  # The pool chose the login directory; an inherited CLAUDE_CONFIG_DIR must never shadow it.
  process_env.pop(config.CLAUDE_CONFIG_DIR_ENV_VAR, None)
  return backend._build_command(claude_compaction.COMPACT_PROMPT), backend._prepare_env(process_env)


async def _compact(
    ctx: backend_lifecycle.LaunchContext,
    cfg: config.CharlieBotConfig,
    option: models.BackendOption,
    account: claude_config.ClaudeAccount,
    cc_session_id: str,
    pre_tokens: int,
) -> None:
  """One idle compaction on *account*; the runner emits its chat event and judges it."""
  session_id = ctx.session_meta.id
  model = models.option_default_model(option, subject="backend ")
  try:
    argv, env = _compaction_process(cfg, option, account, session_id, cc_session_id, ctx.run_kind)
    outcome = await claude_compaction.run_compaction(
        argv=argv,
        env=env,
        cwd=ctx.cwd,
        config_dir=account.config_dir,
        cc_session_id=cc_session_id,
        family=model_family.model_family(model),
        model=model,
        trigger=TRIGGER,
        failure_trigger=TRIGGER,
        pre_tokens=pre_tokens,
        persist_and_broadcast=ctx.emit,
        log_context={
            "session": session_id,
            "account": account.label,
            "trigger": TRIGGER
        },
        cgroup_session_id=session_id,
    )
    if outcome.ok:
      log.info(
          "claude_idle_compaction_done",
          session=session_id,
          pre_tokens=pre_tokens,
          post_tokens=outcome.post_tokens,
          cache_read_tokens=outcome.cache_read_tokens,
          account=account.label,
      )
  finally:
    _running.pop(session_id, None)
