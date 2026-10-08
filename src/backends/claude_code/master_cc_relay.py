"""Account placement and relay for one run on the Claude account pool.

A run (a master turn or a task run) executes on one pool account
(src/backends/claude_code/claude_accounts.py). This module owns what happens around that
process, for ``ClaudeCodeLifecycle`` (src/backends/claude_code/claude_lifecycle.py): which
account a turn starts on, and how a run continues on the next one. The event reading and the
relay mechanics themselves are in src/backends/claude_code/claude_relay.py.

Turn start: the session keeps its account while the prompt cache is warm, the
account has headroom, and no other running session holds that account (one
account carries one turn; a busy account breaks the warm stickiness). A cold
cache (more than an hour since the last request), an account at the warning
line, or a busy one frees the turn to pick the idle account with the most
headroom, moving the transcript with it and, on a cold cache, compacting a
large Fable context with Sonnet first (src/backends/claude_code/claude_compaction.py).

Relay: the transcript moves to the account with the most headroom among the
others, a large Fable context is compacted by Sonnet there, and Claude Code
resumes the same session id with the shared continuation prompt. The user sees
the compaction notice only; account names stay in the server log, metadata and
the usage panel.
"""

from __future__ import annotations

import datetime

from src.backends.claude_code import claude_accounts, claude_compaction, claude_metadata, claude_relay
from src.backends.claude_code.claude_config import ClaudeAccount
from src.infra import config, log_once, models
from src.runtime import master_cc_state
from src.runtime.hooks import backend_lifecycle

log = log_once.LazyStructlogLogger()


async def _compact_session_transcript(
    ctx: backend_lifecycle.LaunchContext,
    cc_session_id: str,
    config_dir: str,
    account_label: str,
    trigger: str,
    context_tokens: int | None,
) -> None:
  """One Sonnet compaction of the session's transcript, attributed to the session.

  Single home of the call shape: the compaction event leaves through the run's ``emit``, the
  process lands in the session's cgroup, and the log context names the session, the account,
  and the trigger.
  """
  await claude_compaction.compact_with_sonnet(
      cc_session_id=cc_session_id,
      cwd=ctx.cwd,
      config_dir=config_dir,
      pre_tokens=context_tokens,
      persist_and_broadcast=ctx.emit,
      cgroup_session_id=ctx.session_meta.id,
      log_context={
          "session": ctx.session_meta.id,
          "account": account_label,
          "trigger": trigger
      },
  )


# ---------------------------------------------------------------------------
# Turn start
# ---------------------------------------------------------------------------


def choose_turn_account(
    cfg: config.CharlieBotConfig,
    session_meta: models.SessionMetadata,
    model: str | None,
    last_request_at: datetime.datetime | None,
    now: datetime.datetime | None,
    account_pool: str | None = None,
) -> tuple[ClaudeAccount | None, bool]:
  """The account this turn runs on and whether the cache is cold.

  The current account stays while it belongs to pool *account_pool*, the cache
  is warm, the account is healthy, its newest reading sits under the warning
  line with no rejection pending, and no other running session holds it; every
  other case re-selects among idle accounts inside the pool. The busy set is
  derived read-only from the running work items -- one account carries one turn
  -- so two sessions never pile onto one login, and when every healthy account
  is busy the choice falls back to all of them.
  """
  moment = claude_accounts.now_or(now)
  current = claude_accounts.account_by_label(cfg, claude_metadata.account_of(session_meta))
  cold = claude_compaction.cache_expired(last_request_at, moment)
  busy = {
      claude_metadata.account_of(item.session_meta)
      for sid, item in master_cc_state._current_items.items()
      if sid != session_meta.id and item.session_meta and claude_metadata.account_of(item.session_meta)
  }
  if (current is not None and claude_accounts.in_pool(cfg, current.label, account_pool) and
      current.label not in busy and not cold and claude_accounts.healthy(current, moment)):
    reading = claude_accounts.latest_reading(current.label, model)
    rejected = reading is not None and reading.rejected_until is not None and reading.rejected_until > moment
    if reading is None or (reading.utilization < claude_accounts.WARNING_UTILIZATION and not rejected):
      return current, cold
  chosen = claude_accounts.select(cfg, model, busy_accounts=busy, now=moment, account_pool=account_pool)
  return chosen, cold


# The reason both guard-refusal consumers label their reconcile row with: the
# placement move at turn start and the mid-turn relay move react to the same
# refusal, so the grep-able value has one spelling.
GUARD_REFUSED_NEWER_TRANSCRIPT = "guard_refused_newer_transcript"


async def adopt_transcript_holder(
    ctx: backend_lifecycle.LaunchContext,
    cc_session_id: str | None,
    holder: ClaudeAccount,
    previous_label: str | None,
    reason: str,
) -> None:
  """Make *holder* the session's account label: funnel-persist it, then warn.

  The one adoption reaction the plan's reconciliation paths share -- the
  placement probe (forked or stale lineage) and the guard-refusal consumers
  (the placement move, the mid-turn relay): the label follows the holder that
  actually carries the newest transcript, the turn continues from it with no
  copy, and every trigger lands a persistent grep-able row.
  """
  session_meta = ctx.session_meta
  await ctx.record_account(holder.label)
  claude_metadata.set_account(session_meta, holder.label)
  log.warning(
      "master_cc_account_label_reconciled",
      session=session_meta.id,
      cc_session_id=cc_session_id,
      adopted=holder.label,
      previous=previous_label,
      reason=reason,
  )


async def _probe_reconcile_label(ctx: backend_lifecycle.LaunchContext, cc_session_id: str | None) -> None:
  """Placement probe: adopt the newest pool holder when the label's copy split from it.

  Runs before account selection on every placement that has a transcript to
  resume, so the corrected label is what the warm-stickiness and busy-set rules
  read. Skipped for the no-transcript scenarios -- no resume id, which includes
  the weekly recycle's declared fresh start -- which keep the existing label
  path, and when the pool holds at most the label's own copy: with nothing to
  compare against there is nothing to reconcile. The kill-shaped window (a
  relay's move landed, its label persist did not) shows up here as a label copy
  whose tail line the newest holder's tail window no longer contains; the
  adoption never moves bytes.
  """
  if not cc_session_id:
    return
  label = claude_accounts.account_by_label(ctx.cfg, claude_metadata.account_of(ctx.session_meta))
  if label is None:
    return
  label_copy = claude_accounts.transcript_path(label.config_dir, cc_session_id)
  newest = claude_accounts.newest_transcript_copy(ctx.cfg, cc_session_id)
  if label_copy is None or newest is None or newest[1] == label_copy:
    return
  newest_account, newest_copy = newest
  if not claude_accounts.transcript_lineage_split(label_copy, newest_copy):
    return
  await adopt_transcript_holder(ctx, cc_session_id, newest_account, label.label, reason="forked_or_stale_lineage")


async def place_turn(
    ctx: backend_lifecycle.LaunchContext,
    resume_id: str | None,
    context_tokens: int | None,
    last_request_at: datetime.datetime | None,
    now: datetime.datetime | None = None,
) -> ClaudeAccount:
  """Pick the turn's account, move the transcript to it and compact on a cold cache.

  The account label is disk-true from the moment the move lands: the lineage
  probe runs before selection, and a successful (or refusal-adopted) move is
  followed immediately by the ``record_account`` funnel, so the label
  and the transcript cannot split for a whole round. Raises ``LaunchRefused``
  when no account is available (a spent quota). A failed transcript move raises
  ``RuntimeError`` and keeps the ordinary launch-error event shape.
  """
  cfg, option, session_meta = ctx.cfg, ctx.option, ctx.session_meta
  await _probe_reconcile_label(ctx, resume_id)
  account_pool = claude_accounts.option_pool(option)
  chosen, cold = choose_turn_account(cfg, session_meta, option.model, last_request_at, now, account_pool)
  if chosen is None:
    raise backend_lifecycle.LaunchRefused(
        claude_relay.pool_exhausted_message(cfg, now, account_pool), quota_exhausted=True)
  previous = claude_accounts.account_by_label(cfg, claude_metadata.account_of(session_meta))
  if previous is None or previous.label != chosen.label:
    if resume_id and previous is not None:
      try:
        claude_accounts.move_transcript(resume_id, previous.config_dir, chosen.config_dir)
      except claude_accounts.TranscriptMoveError as exc:
        if not claude_accounts.is_newer_transcript_refusal(exc):
          raise RuntimeError(f"Claude account switch failed: {exc}") from exc
        # The guard refused: the destination holds the newer transcript (the
        # kill between a relay's move and its label persist, or an unknown
        # defect). The move layer never redirects -- this consumer reconciles:
        # adopt the newer holder and continue the turn from it with no copy.
        await adopt_transcript_holder(ctx, resume_id, chosen, previous.label, reason=GUARD_REFUSED_NEWER_TRANSCRIPT)
    claude_metadata.set_account(session_meta, chosen.label)
    await ctx.record_account(chosen.label)
    log.info(
        "master_cc_account_chosen",
        session=session_meta.id,
        account=chosen.label,
        previous=previous.label if previous else None,
        cold_cache=cold,
    )
  await report_empty_credentials(ctx)
  if (resume_id and cold and
      claude_compaction.expired_cache_compaction_wanted(cfg, option.model, context_tokens, last_request_at, now)):
    await _compact_session_transcript(
        ctx,
        cc_session_id=resume_id,
        config_dir=chosen.config_dir,
        account_label=chosen.label,
        trigger="expired_cache",
        context_tokens=context_tokens)
  return chosen


# ---------------------------------------------------------------------------
# Relaying
# ---------------------------------------------------------------------------


async def _report_login_required(ctx: backend_lifecycle.LaunchContext, account: ClaudeAccount, reason: str) -> None:
  """Emit the login-required operator notice once.

  The log line keeps the chat event's type as its label, and the chat event
  leaves through the run's ``emit``.
  """
  log.error(
      claude_relay.CLAUDE_ACCOUNT_LOGIN_REQUIRED,
      session=ctx.session_meta.id,
      account=account.label,
      config_dir=account.config_dir,
      reason=reason)
  await ctx.emit(claude_relay.login_required_event(account, reason))


async def report_login_failure(ctx: backend_lifecycle.LaunchContext, account: ClaudeAccount) -> None:
  """Mark *account* unhealthy for the cooldown and tell the operator (account-free in chat)."""
  claude_accounts.record_auth_failure(account.label, None)
  await _report_login_required(ctx, account, claude_relay.LOGIN_REASON_AUTH_FAILED)


async def report_empty_credentials(ctx: backend_lifecycle.LaunchContext) -> None:
  """One notice per account whose credential store has gone empty, until it recovers."""
  for account in claude_accounts.pool(ctx.cfg):
    present = claude_accounts.credentials_present(account)
    if claude_accounts.login_notice_due(account.label, unhealthy=not present):
      await _report_login_required(ctx, account, claude_relay.LOGIN_REASON_EMPTY_CREDENTIALS)


async def prepare_relay(
    ctx: backend_lifecycle.LaunchContext,
    current: ClaudeAccount,
    cc_session_id: str | None,
    reason: str | None,
    relays: int,
) -> ClaudeAccount:
  """Move the run to the next account and compact a large Fable context there.

  Returns the account the run continues on. Raises ``LaunchRefused`` when the
  pool is exhausted (a spent quota), the transcript has no id yet, or the copy
  failed. A copy the newer-transcript guard refused names the destination
  account holding the newer transcript: a turn adopts that account and
  continues from it with no compaction, and a task ends loudly like every other
  refused move. Only a turn compacts on relay; a task carries the full transcript
  forward without a compaction pass. *relays* is the number of relays this one
  makes, for the task log row.
  """
  cfg, option, session_meta = ctx.cfg, ctx.option, ctx.session_meta
  move = claude_relay.relay_move(
      cfg, option.model, current, cc_session_id, None, account_pool=claude_accounts.option_pool(option))
  if move.account is None:
    assert move.error is not None
    if move.refused_holder is None or ctx.kind == "task":
      raise backend_lifecycle.LaunchRefused(move.error, quota_exhausted=move.pool_exhausted)
    # The mid-turn move hit the newer-transcript guard: the destination holds
    # the newer copy (the kill between a previous relay's move and its
    # persist, or an unknown defect). Same predicate and reaction as the
    # placement layer's self-heal -- adopt the destination, persist it through
    # the funnel, continue the turn from it; a refusal the copy on disk
    # already answers never fails the turn.
    await adopt_transcript_holder(
        ctx, cc_session_id, move.refused_holder, current.label, reason=GUARD_REFUSED_NEWER_TRANSCRIPT)
    return move.refused_holder
  nxt = move.account
  if ctx.kind == "turn":
    log.warning(
        "master_cc_account_relay",
        session=session_meta.id,
        cc_session_id=cc_session_id,
        reason=reason,
        from_account=current.label,
        to_account=nxt.label,
    )
  elif ctx.kind == "task":
    log.warning(
        "worker_account_relay",
        session=session_meta.id,
        reason=reason,
        from_account=current.label,
        to_account=nxt.label,
        relays=relays)
  else:
    raise ValueError(f"unknown launch kind {ctx.kind!r}")
  context_tokens, _last = await ctx.context_state()
  if (ctx.kind == "turn" and claude_compaction.relay_compaction_wanted(cfg, option.model, context_tokens)):
    assert cc_session_id is not None
    await _compact_session_transcript(
        ctx,
        cc_session_id=cc_session_id,
        config_dir=nxt.config_dir,
        account_label=nxt.label,
        trigger="relay",
        context_tokens=context_tokens)
  return nxt
