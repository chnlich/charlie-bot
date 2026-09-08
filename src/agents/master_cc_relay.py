"""Master-side account relay: keep a Claude turn running when its login runs out of quota.

A master turn runs on one pool account (src/core/claude_accounts.py). This module
owns what happens around that run so ``master_cc_run`` stays a spawn-and-stream
loop: which account a turn starts on, and how the turn continues on the next one.
The event reading and the relay mechanics themselves are shared with delegated
workers in src/core/claude_relay.py.

Turn start: the session keeps its account while the prompt cache is warm and the
account has headroom; a cold cache (more than an hour since the last request)
or an account at the warning line frees the turn to pick the account with the
most headroom, moving the transcript with it and, on a cold cache, compacting a
large Fable context with Sonnet first (src/core/claude_compaction.py).

Relay: the transcript moves to the account with the most headroom among the
others, a large Fable context is compacted by Sonnet there, and Claude Code
resumes the same session id with the shared continuation prompt. The user sees
the compaction notice only; account names stay in the server log, metadata and
the usage panel.
"""

from __future__ import annotations

from datetime import datetime

import structlog

from src.agents import master_cc_state
from src.core import claude_accounts, claude_compaction, claude_relay
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, ClaudeAccount, SessionMetadata

log = structlog.get_logger()

_now = claude_accounts._now


def _persist(item: master_cc_state._WorkItem):
  session_id = item.session_meta.id

  async def persist(event: dict) -> None:
    await item.callbacks.persist_and_broadcast(session_id, event)

  return persist


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

  The current account stays while the cache is warm, it is healthy and its
  newest reading sits under the warning line with no rejection pending; every
  other case re-selects (a tie keeps the current account).
  """
  moment = _now(now)
  current = claude_accounts.account_by_label(cfg, session_meta.claude_account)
  cold = claude_compaction.cache_expired(last_request_at, moment)
  if current is not None and not cold and claude_accounts.healthy(current, moment):
    reading = claude_accounts.latest_reading(current.label, model)
    rejected = reading is not None and reading.rejected_until is not None and reading.rejected_until > moment
    if reading is None or (reading.utilization < claude_accounts.WARNING_UTILIZATION and not rejected):
      return current, cold
  chosen = claude_accounts.select(cfg, model, current=current.label if current else None, now=moment)
  return chosen, cold


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

  Returns ``(account, None)`` or ``(None, error)`` when no account is available
  or the transcript could not be moved; the caller ends the turn loudly.
  """
  session_meta = item.session_meta
  chosen, cold = choose_turn_account(cfg, session_meta, option.model, last_request_at, now)
  if chosen is None:
    return None, claude_relay.pool_exhausted_message(cfg, now)
  previous = claude_accounts.account_by_label(cfg, session_meta.claude_account)
  if previous is None or previous.label != chosen.label:
    if resume_id and previous is not None:
      try:
        claude_accounts.move_transcript(resume_id, previous.config_dir, chosen.config_dir)
      except claude_accounts.TranscriptMoveError as exc:
        return None, f"Claude account switch failed: {exc}"
    log.info(
        "master_cc_account_chosen",
        session=session_meta.id,
        account=chosen.label,
        previous=previous.label if previous else None,
        cold_cache=cold,
    )
  session_meta.claude_account = chosen.label
  await report_empty_credentials(cfg, item)
  if (resume_id and cold and
      claude_compaction.expired_cache_compaction_wanted(cfg, option.model, context_tokens, last_request_at, now)):
    await claude_compaction.compact_with_sonnet(
        cc_session_id=resume_id,
        cwd=cwd,
        config_dir=chosen.config_dir,
        pre_tokens=context_tokens,
        persist_and_broadcast=_persist(item),
        log_context={
            "session": session_meta.id,
            "account": chosen.label,
            "trigger": "expired_cache"
        },
    )
  return chosen, None


# ---------------------------------------------------------------------------
# Relaying
# ---------------------------------------------------------------------------


async def report_login_failure(
    item: master_cc_state._WorkItem,
    account: ClaudeAccount,
    now: datetime | None = None,
) -> None:
  """Mark *account* unhealthy for the cooldown and tell the operator (account-free in chat)."""
  claude_accounts.record_auth_failure(account.label, now)
  log.error(
      "claude_account_login_required",
      session=item.session_meta.id,
      account=account.label,
      config_dir=account.config_dir,
      reason="auth_failed")
  await _persist(item)(claude_relay.login_required_event(account, "auth_failed"))


async def report_empty_credentials(cfg: CharlieBotConfig, item: master_cc_state._WorkItem) -> None:
  """One notice per account whose credential store has gone empty, until it recovers."""
  for account in claude_accounts.pool(cfg):
    present = claude_accounts.credentials_present(account)
    if claude_accounts.login_notice_due(account.label, unhealthy=not present):
      log.error(
          "claude_account_login_required",
          session=item.session_meta.id,
          account=account.label,
          config_dir=account.config_dir,
          reason="empty_credentials")
      await _persist(item)(claude_relay.login_required_event(account, "empty_credentials"))


async def prepare_relay(
    cfg: CharlieBotConfig,
    item: master_cc_state._WorkItem,
    option: BackendOption,
    cc_session_id: str | None,
    current: ClaudeAccount,
    cwd: str,
    reason: str,
    now: datetime | None = None,
) -> tuple[ClaudeAccount | None, str | None]:
  """Move the turn to the next account and compact a large Fable context there.

  Returns ``(account, None)``, or ``(None, error)`` when the pool is exhausted,
  the transcript has no id yet, or the copy failed.
  """
  session_meta = item.session_meta
  nxt, error = claude_relay.move_to_next_account(cfg, option.model, current, cc_session_id, now)
  if nxt is None:
    return None, error
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
    await claude_compaction.compact_with_sonnet(
        cc_session_id=cc_session_id,
        cwd=cwd,
        config_dir=nxt.config_dir,
        pre_tokens=context_tokens,
        persist_and_broadcast=_persist(item),
        log_context={
            "session": session_meta.id,
            "account": nxt.label,
            "trigger": "relay"
        },
    )
  return nxt, None
