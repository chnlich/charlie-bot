"""The quota panel's poller and API route.

The panel lists the quota accounts the usage sources register (``usage_sources.quota_accounts()``),
in registration order, one entry per account under the cache key ``<provider>:<label>``. This module
holds the generic part: the round-robin poll loop, the cache, the pending markers and the error
fallback, the sidebar broadcast and ``GET /api/ext-usage``. What an account fetches and the marks it
puts on its entry belong to the account (see ``usage_sources.QuotaAccount``).
"""

import asyncio
import datetime
from typing import Any

import fastapi

from src.infra import log_once, tasks, timeouts
from src.runtime import streaming
from src.runtime.hooks import usage_sources, wiring

log = log_once.LazyStructlogLogger()

router = fastapi.APIRouter()

# ---------------------------------------------------------------------------
# Cached usage data (module-level, keyed by "<provider>:<account>")
# ---------------------------------------------------------------------------

_cached_usage: dict[str, dict] = {}
# The accounts of the latest round, by cache key. The cache holds an entry for exactly these keys,
# and each emit asks the entry's account for its expiry marks.
_accounts: dict[str, usage_sources.QuotaAccount] = {}


def _cache_key(account: usage_sources.QuotaAccount) -> str:
  return f"{account.provider}:{account.label}"


# ---------------------------------------------------------------------------
# Background poller
# ---------------------------------------------------------------------------


async def _poll_loop() -> None:
  """Background loop that refreshes one account per round-gap sleep.

  Accounts are fetched round-robin in registration order: each fetch updates that
  account's ``_cached_usage`` entry and broadcasts the cache with each entry's
  expiry marks judged at emit time over the sidebar websocket, then sleeps
  ``EXT_USAGE_ROUND_GAP_SECONDS`` before the next account.
  The account set is re-derived once per full round (every N fetches) so config
  edits apply at round boundaries; dropped accounts are pruned from
  ``_cached_usage``. A zero-account round still sleeps once
  before re-deriving so the loop never busy-spins. Accounts not yet read carry a
  pending marker so the strip never understates the account set.
  """
  global _accounts
  while True:
    try:
      cycle = usage_sources.quota_accounts()
      _accounts = {_cache_key(account): account for account in cycle}
      for cache_key in list(_cached_usage):
        if cache_key not in _accounts:
          del _cached_usage[cache_key]

      # Seed a pending marker for every account not yet read, so the strip lists
      # the whole account set from the first broadcast instead of growing one row
      # per round gap after a restart. Costs no extra request: the markers ride
      # along in the broadcast the first real fetch already sends. Seeding in
      # cycle order also fixes row order at derivation order rather than at
      # fetch-completion order.
      for account in cycle:
        seed_key = _cache_key(account)
        if seed_key not in _cached_usage:
          _cached_usage[seed_key] = {
              usage_sources.PANEL_PROVIDER: account.provider,
              "account": account.label,
              "pending": True,
          }

      if not cycle:
        await asyncio.sleep(timeouts.EXT_USAGE_ROUND_GAP_SECONDS)
        continue

      for account in cycle:
        cache_key = _cache_key(account)
        try:
          result = await account.fetch()
        except Exception as e:
          log.error("ext_usage_poll_error", provider=account.provider, account=account.label, error=str(e))
          result = None
        if result is not None:
          _cached_usage[cache_key] = {**result, "account": account.label}
        else:
          prev = _cached_usage.get(cache_key)
          # A pending marker is a not-yet-read placeholder, so it gives way to the
          # real error the same way a previous error does; only a real reading is
          # worth keeping when a later fetch fails.
          if prev is None or "error" in prev or "pending" in prev:
            _cached_usage[cache_key] = {
                usage_sources.PANEL_PROVIDER: account.provider,
                "account": account.label,
                "error": account.last_error,
            }
        account.mark_login_required(_cached_usage[cache_key])
        await streaming.streaming_manager.broadcast(
            streaming.SIDEBAR_CHANNEL, {
                "type": "ext_usage",
                "providers": _annotated_providers()
            })
        log.info("ext_usage_fetched", providers=list(_cached_usage.keys()))
        await asyncio.sleep(timeouts.EXT_USAGE_ROUND_GAP_SECONDS)
    except Exception:
      log.exception("ext_usage_poll_error")
      # The except path must yield too: an exception raised before the loop's
      # own sleeps (e.g. from quota_accounts()) would otherwise re-loop with
      # no await point and busy-spin the event loop instead of backing off.
      await asyncio.sleep(timeouts.EXT_USAGE_ROUND_GAP_SECONDS)


def _annotated_providers(now: datetime.datetime | None = None) -> dict[str, dict[str, Any]]:
  """The cached snapshot with each entry's expiry marks judged at emit time.

  Each account's ``mark_expired`` decides what its entry shows at the server clock. The
  judgement is recomputed per emit on copies — a frozen cache flips as the clock crosses a
  reset, whether or not the account ever fetches again, and nothing is written back into
  ``_cached_usage``.
  """
  moment = now if now is not None else datetime.datetime.now(datetime.UTC)
  return {key: _accounts[key].mark_expired(entry, moment) for key, entry in _cached_usage.items()}


# ---------------------------------------------------------------------------
# API route
# ---------------------------------------------------------------------------


@router.get("/ext-usage")
async def get_ext_usage() -> dict[str, Any]:
  """Return the cached usage snapshot with each entry's expiry marks judged at read time."""
  if not _cached_usage:
    return {"error": "Usage data not yet available"}
  return {"providers": _annotated_providers()}


# ---------------------------------------------------------------------------
# Service: the poll loop runs for the server's lifetime
# ---------------------------------------------------------------------------

_poller = tasks.SingleTaskPoller(_poll_loop, log, "ext_usage_poller_started", "ext_usage_poller_stopped")


async def start_service(ctx: wiring.ServiceContext) -> None:
  """Start the poll loop."""
  await _poller.start()


async def stop_service() -> None:
  """Cancel the poll loop and wait for it; returns at once when start_service never ran."""
  await _poller.stop()
