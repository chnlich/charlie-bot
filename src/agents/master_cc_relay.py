"""Master-side account relay: keep a Claude turn running when its login runs out of quota.

A master turn runs on one pool account (src/core/claude_accounts.py). This module
owns what happens around that run so ``master_cc_run`` stays a spawn-and-stream
loop: which account a turn starts on, and how the turn continues on the next one.
The event reading and the relay mechanics themselves are shared with delegated
workers in src/core/claude_relay.py.

Turn start: the session keeps its account while the prompt cache is warm, the
account has headroom, and no other running session holds that account (one
account carries one turn; a busy account breaks the warm stickiness). A cold
cache (more than an hour since the last request), an account at the warning
line, or a busy one frees the turn to pick the idle account with the most
headroom, moving the transcript with it and, on a cold cache, compacting a
large Fable context with Sonnet first (src/core/claude_compaction.py).

Relay: the transcript moves to the account with the most headroom among the
others, a large Fable context is compacted by Sonnet there, and Claude Code
resumes the same session id with the shared continuation prompt. The user sees
the compaction notice only; account names stay in the server log, metadata and
the usage panel.
"""

from __future__ import annotations

from collections.abc import Awaitable, Callable
from datetime import datetime

from src.agents import master_cc_state
from src.core import claude_accounts, claude_compaction, claude_relay
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.log_once import LazyStructlogLogger
from src.core.models import BackendOption, ClaudeAccount, SessionMetadata

log = LazyStructlogLogger()


def _persist(item: master_cc_state._WorkItem) -> Callable[[dict], Awaitable[None]]:
  session_id = item.session_meta.id

  async def persist(event: dict) -> None:
    await item.callbacks.persist_and_broadcast(session_id, event)

  return persist


async def _compact_session_transcript(
    item: master_cc_state._WorkItem,
    cc_session_id: str,
    cwd: str,
    config_dir: str,
    account_label: str,
    trigger: str,
    context_tokens: int | None,
) -> None:
  """One Sonnet compaction of the session's transcript, attributed to the session.

  Single home of the master-side call shape: the compaction event rides the
  item's persist hook, the process lands in the session's cgroup, and the log
  context names the session, the account, and the trigger.
  """
  await claude_compaction.compact_with_sonnet(
      cc_session_id=cc_session_id,
      cwd=cwd,
      config_dir=config_dir,
      pre_tokens=context_tokens,
      persist_and_broadcast=_persist(item),
      cgroup_session_id=item.session_meta.id,
      log_context={
          "session": item.session_meta.id,
          "account": account_label,
          "trigger": trigger
      },
  )


# ---------------------------------------------------------------------------
# Turn start
# ---------------------------------------------------------------------------


def choose_turn_account(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    model: str | None,
    last_request_at: datetime | None,
    now: datetime | None = None,
) -> tuple[ClaudeAccount | None, bool]:
  """The account this turn runs on and whether the cache is cold.

  The current account stays while the cache is warm, it is healthy, its newest
  reading sits under the warning line with no rejection pending, and no other
  running session holds it; every other case re-selects among idle accounts.
  The busy set is derived read-only from the running work items -- one account
  carries one turn -- so two sessions never pile onto one login, and when every
  healthy account is busy the choice falls back to all of them.
  """
  moment = claude_accounts.now_or(now)
  current = claude_accounts.account_by_label(cfg, session_meta.claude_account)
  cold = claude_compaction.cache_expired(last_request_at, moment)
  busy = {
      item.session_meta.claude_account
      for sid, item in master_cc_state._current_items.items()
      if sid != session_meta.id and item.session_meta and item.session_meta.claude_account
  }
  if current is not None and current.label not in busy and not cold and claude_accounts.healthy(current, moment):
    reading = claude_accounts.latest_reading(current.label, model)
    rejected = reading is not None and reading.rejected_until is not None and reading.rejected_until > moment
    if reading is None or (reading.utilization < claude_accounts.WARNING_UTILIZATION and not rejected):
      return current, cold
  chosen = claude_accounts.select(cfg, model, busy_accounts=busy, now=moment)
  return chosen, cold


# The reason both guard-refusal consumers label their reconcile row with: the
# placement move at turn start and the mid-turn relay move react to the same
# refusal, so the grep-able value has one spelling.
GUARD_REFUSED_NEWER_TRANSCRIPT = "guard_refused_newer_transcript"


async def adopt_transcript_holder(
    item: master_cc_state._WorkItem,
    cc_session_id: str | None,
    holder: ClaudeAccount,
    previous_label: str | None,
    reason: str,
) -> None:
  """Make *holder* the session's account label: funnel-persist it, then warn.

  The one adoption reaction the plan's reconciliation paths share -- the
  placement probe (forked or stale lineage) and the two guard-refusal consumers
  (the placement move, the mid-turn relay): the label follows the holder that
  actually carries the newest transcript, the turn continues from it with no
  copy, and every trigger lands a persistent grep-able row.
  """
  session_meta = item.session_meta
  if item.callbacks.persist_claude_account is not None:
    await item.callbacks.persist_claude_account(session_meta.id, holder.label)
  session_meta.claude_account = holder.label
  log.warning(
      "master_cc_account_label_reconciled",
      session=session_meta.id,
      cc_session_id=cc_session_id,
      adopted=holder.label,
      previous=previous_label,
      reason=reason,
  )


async def _probe_reconcile_label(
    cfg: CharlieBotConfig,
    item: master_cc_state._WorkItem,
    cc_session_id: str | None,
) -> None:
  """Placement probe: adopt the newest pool holder when the label's copy split from it.

  Runs before account selection on every placement that has a transcript to
  resume, so the corrected label is what the warm-stickiness and busy-set rules
  read. Skipped for the no-transcript scenarios -- no resume id, or the weekly
  recycle's declared fresh start -- which keep the existing label path, and
  when the pool holds at most the label's own copy: with nothing to compare
  against there is nothing to reconcile. The kill-shaped window (a relay's move
  landed, its label persist did not) shows up here as a label copy whose tail
  line the newest holder's tail window no longer contains; the adoption never
  moves bytes.
  """
  if not cc_session_id or item.expect_fresh_session:
    return
  label = claude_accounts.account_by_label(cfg, item.session_meta.claude_account)
  if label is None:
    return
  label_copy = claude_accounts.transcript_path(label.config_dir, cc_session_id)
  newest = claude_accounts.newest_transcript_copy(cfg, cc_session_id)
  if label_copy is None or newest is None or newest[1] == label_copy:
    return
  newest_account, newest_copy = newest
  if not claude_accounts.transcript_lineage_split(label_copy, newest_copy):
    return
  await adopt_transcript_holder(item, cc_session_id, newest_account, label.label, reason="forked_or_stale_lineage")


async def place_turn(
    cfg: CharlieBotConfig,
    item: master_cc_state._WorkItem,
    option: BackendOption,
    resume_id: str | None,
    cwd: str,
    context_tokens: int | None,
    last_request_at: datetime | None,
    now: datetime | None = None,
) -> tuple[ClaudeAccount | None, str | None]:
  """Pick the turn's account, move the transcript to it and compact on a cold cache.

  The account label is disk-true from the moment the move lands: the lineage
  probe runs before selection, and a successful (or refusal-adopted) move is
  followed immediately by the ``persist_claude_account`` funnel, so the label
  and the transcript cannot split for a whole round. Returns ``(account,
  None)`` or ``(None, error)`` when no account is available or the transcript
  could not be moved; the caller ends the turn loudly.
  """
  session_meta = item.session_meta
  await _probe_reconcile_label(cfg, item, resume_id)
  chosen, cold = choose_turn_account(cfg, session_meta, option.model, last_request_at, now)
  if chosen is None:
    return None, claude_relay.pool_exhausted_message(cfg, now)
  previous = claude_accounts.account_by_label(cfg, session_meta.claude_account)
  if previous is None or previous.label != chosen.label:
    if resume_id and previous is not None:
      try:
        claude_accounts.move_transcript(resume_id, previous.config_dir, chosen.config_dir)
      except claude_accounts.TranscriptMoveError as exc:
        if not claude_accounts.is_newer_transcript_refusal(exc):
          return None, f"Claude account switch failed: {exc}"
        # The guard refused: the destination holds the newer transcript (the
        # kill between a relay's move and its label persist, or an unknown
        # defect). The move layer never redirects -- this consumer reconciles:
        # adopt the newer holder and continue the turn from it with no copy.
        await adopt_transcript_holder(item, resume_id, chosen, previous.label, reason=GUARD_REFUSED_NEWER_TRANSCRIPT)
    session_meta.claude_account = chosen.label
    if item.callbacks.persist_claude_account is not None:
      await item.callbacks.persist_claude_account(session_meta.id, chosen.label)
    log.info(
        "master_cc_account_chosen",
        session=session_meta.id,
        account=chosen.label,
        previous=previous.label if previous else None,
        cold_cache=cold,
    )
  await report_empty_credentials(cfg, item)
  if (resume_id and cold and
      claude_compaction.expired_cache_compaction_wanted(cfg, option.model, context_tokens, last_request_at, now)):
    await _compact_session_transcript(
        item,
        cc_session_id=resume_id,
        cwd=cwd,
        config_dir=chosen.config_dir,
        account_label=chosen.label,
        trigger="expired_cache",
        context_tokens=context_tokens)
  return chosen, None


# ---------------------------------------------------------------------------
# Relaying
# ---------------------------------------------------------------------------


async def _report_login_required(item: master_cc_state._WorkItem, account: ClaudeAccount, reason: str) -> None:
  """Emit the login-required operator notice once.

  The log line keeps the chat event's type as its label, and the chat event
  rides the item's persist hook.
  """
  log.error(
      ET.CLAUDE_ACCOUNT_LOGIN_REQUIRED,
      session=item.session_meta.id,
      account=account.label,
      config_dir=account.config_dir,
      reason=reason)
  await _persist(item)(claude_relay.login_required_event(account, reason))


async def report_login_failure(
    item: master_cc_state._WorkItem,
    account: ClaudeAccount,
    now: datetime | None = None,
) -> None:
  """Mark *account* unhealthy for the cooldown and tell the operator (account-free in chat)."""
  claude_accounts.record_auth_failure(account.label, now)
  await _report_login_required(item, account, claude_relay.LOGIN_REASON_AUTH_FAILED)


async def report_empty_credentials(cfg: CharlieBotConfig, item: master_cc_state._WorkItem) -> None:
  """One notice per account whose credential store has gone empty, until it recovers."""
  for account in claude_accounts.pool(cfg):
    present = claude_accounts.credentials_present(account)
    if claude_accounts.login_notice_due(account.label, unhealthy=not present):
      await _report_login_required(item, account, claude_relay.LOGIN_REASON_EMPTY_CREDENTIALS)


async def prepare_relay(
    cfg: CharlieBotConfig,
    item: master_cc_state._WorkItem,
    option: BackendOption,
    cc_session_id: str | None,
    current: ClaudeAccount,
    cwd: str,
    reason: str,
    now: datetime | None = None,
) -> tuple[ClaudeAccount | None, str | None, ClaudeAccount | None]:
  """Move the turn to the next account and compact a large Fable context there.

  Returns ``(account, None, None)``, or ``(None, error, refused_holder)`` when
  the pool is exhausted, the transcript has no id yet, or the copy failed; a
  copy the newer-transcript guard refused carries the destination account
  holding the newer transcript, for the consumer's adoption decision.
  """
  session_meta = item.session_meta
  nxt, error, refused_holder = claude_relay.move_to_next_account(cfg, option.model, current, cc_session_id, now)
  if nxt is None:
    return None, error, refused_holder
  log.warning(
      "master_cc_account_relay",
      session=session_meta.id,
      cc_session_id=cc_session_id,
      reason=reason,
      from_account=current.label,
      to_account=nxt.label,
  )
  context_tokens: int | None = None
  if item.callbacks.claude_context_state is not None:
    context_tokens, _last = await item.callbacks.claude_context_state(session_meta.id, session_meta)
  if claude_compaction.relay_compaction_wanted(cfg, option.model, context_tokens):
    await _compact_session_transcript(
        item,
        cc_session_id=cc_session_id,
        cwd=cwd,
        config_dir=nxt.config_dir,
        account_label=nxt.label,
        trigger="relay",
        context_tokens=context_tokens)
  return nxt, None, None
