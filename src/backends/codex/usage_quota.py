"""Codex's quota account for the quota panel (src/runtime/hooks/usage_sources.py).

``usage_logs.quota_accounts()`` imports this module on its first call, so a usage capture and
``charliebot --help`` never load it. Codex has one account, ``main``, on the default Codex home.
Its payload is read from the rollouts under ``<home>/sessions``: the newest plan-pool
``token_count`` event gives the rate-limit windows and credits, and the 24-hour and 7-day spend is
priced from every recent rollout's per-request token counts. Nothing is written.
"""

import asyncio
import datetime
import json
import os
import pathlib
import time
from collections.abc import Callable
from typing import Any

from src.backends.codex import codex_pricing, codex_usage
from src.infra import log_once, memo, models
from src.runtime.hooks import usage_sources

log = log_once.LazyStructlogLogger()

# A token_count event closes every Codex turn, so the newest one sits in the
# rollout file's trailing bytes; a tail miss (a turn in flight appended more
# than this window since the last event) falls back to a full-file read.
_USAGE_TAIL_BYTES = 1 << 20
# Scan-set bound for plan-pool readings: time-bounded, not count-bounded, so
# concurrent sessions can never cut a fresh plan event out of the scan set.
_CODEX_USAGE_SCAN_WINDOW_HOURS = 6
# Cap for the per-account spend memo. The live sweep drops files outside the
# 7-day window each round, so the resident set is the window's file count and
# the cap must stay above it: a working set past the cap re-parses the whole
# corpus every round, because each miss's re-record evicts the next hit in
# walk order. This bound only stops a pathological dir from growing the memo
# without bound.
_SPEND_CACHE_LIMIT = 8192

CODEX_LIMIT_SLOTS = ("primary", "secondary")


class CodexQuotaAccount(usage_sources.QuotaAccount):
  """Reads usage from <home_dir>/sessions/ JSONL files for one account.

  ``_spend_cache`` is a StatSignatureMemo holding resolved spend events per
  rollout file, fresh while the file's (mtime_ns, size) stands. The poll loop
  fetches one account at a time and awaits each fetch, so the cache never sees
  concurrent access.
  """

  provider = "codex"

  def __init__(self, label: str, home_dir: str) -> None:
    self.label = label
    self.home_dir = home_dir
    self.sessions_dir = pathlib.Path(home_dir) / "sessions"
    self.last_error = "no sessions found"
    self._spend_cache: memo.StatSignatureMemo[pathlib.Path,
                                              list[_SpendEvent]] = memo.StatSignatureMemo(_SPEND_CACHE_LIMIT)

  async def fetch(self) -> dict[str, Any] | None:
    rollout_paths = await asyncio.to_thread(_list_rollout_files, self.sessions_dir)
    usage, spend = await asyncio.gather(
        asyncio.to_thread(self._fetch_usage, rollout_paths),
        asyncio.to_thread(self._compute_spend, rollout_paths),
        return_exceptions=True,
    )
    if isinstance(usage, Exception):
      log.error("ext_usage_codex_usage_error", account=self.label, error=str(usage))
      self.last_error = "usage read failed"
      return None
    if usage is None:
      # _fetch_usage already named the cause in last_error.
      return None
    if isinstance(spend, Exception):
      log.error("ext_usage_codex_spend_failed", account=self.label, error=str(spend))
      spend = None
    usage["spend"] = spend
    return usage

  def _fetch_usage(self, rollout_paths: list[pathlib.Path]) -> dict[str, Any] | None:
    """Return the newest plan-pool quota reading across the recently-written rollouts.

    The scan set is every file with an mtime inside the last
    ``_CODEX_USAGE_SCAN_WINDOW_HOURS`` hours, falling back to the single
    newest-mtime file when none qualify so a stale weekly reading stays visible
    however old. Each file contributes its newest plan-pool token_count event,
    read from its tail window; model-level pool events are skipped, and the
    winning event is the one with the max timestamp. A scan set with no
    plan-pool event is a no-reading state (None with ``last_error`` set), never
    an empty-windows payload.
    """
    stats: dict[pathlib.Path, os.stat_result] = {}
    for path in rollout_paths:
      try:
        stats[path] = path.stat()
      except OSError as e:
        log.warning("ext_usage_codex_rollout_stat_failed", path=str(path), error=str(e))
    if not stats:
      self.last_error = "no sessions found"
      return None
    min_mtime = time.time() - _CODEX_USAGE_SCAN_WINDOW_HOURS * 3600
    scan = [path for path, stat in stats.items() if stat.st_mtime >= min_mtime]
    if not scan:
      scan = [max(stats, key=lambda path: stats[path].st_mtime)]

    events: list[dict[str, Any]] = []
    for path in scan:
      event = _read_latest_token_count_event(path, stats[path].st_size, match=_is_plan_pool_event)
      if event is not None:
        events.append(event)

    if not events:
      self.last_error = f"no plan-quota reading in {_CODEX_USAGE_SCAN_WINDOW_HOURS}h"
      return None
    chosen = max(events, key=lambda event: _parse_codex_timestamp(event["timestamp"]))
    return _transform_codex_response(chosen, fetched_at=models.utc_now_iso(), account=self.label)

  def _compute_spend(self, rollout_paths: list[pathlib.Path]) -> dict[str, float]:
    # A changed file is re-read from the start, never tail-only: a token_count
    # event's model comes from the turn_context line above it.
    now = datetime.datetime.now(datetime.UTC)
    min_mtime = (now - datetime.timedelta(days=7)).timestamp()
    live: set[pathlib.Path] = set()
    events_by_file = []
    for path in rollout_paths:
      try:
        stat = path.stat()
      except OSError as e:
        log.warning("ext_usage_codex_spend_file_skip", path=str(path), error=str(e))
        continue
      if stat.st_mtime < min_mtime:
        continue
      live.add(path)
      cached = self._spend_cache.fresh(path, stat)
      if cached is not None:
        events_by_file.append(cached)
        continue
      events = _extract_codex_spend_events(path)
      # An unreadable file (None) is skipped, not cached, so the next round retries it.
      if events is not None:
        self._spend_cache.record(path, stat, events)
        events_by_file.append(events)
    self._spend_cache.drop_where(lambda path: path not in live)
    return _sum_codex_spend_events(events_by_file, now=now)


# The one account, kept across quota_accounts() calls so its spend memo survives.
_account: CodexQuotaAccount | None = None


def quota_accounts() -> list[usage_sources.QuotaAccount]:
  """Codex's one account, on the default Codex home."""
  global _account
  home_dir = str(codex_usage.default_codex_home())
  if _account is None or _account.home_dir != home_dir:
    _account = CodexQuotaAccount("main", home_dir)
  return [_account]


def _list_rollout_files(sessions_dir: pathlib.Path) -> list[pathlib.Path]:
  """List every rollout log under one account's sessions dir.

  A single walk feeds both readers: the usage scrape applies its own scan-set
  window with a newest-file fallback, while the spend aggregation applies its
  own mtime cutoff.
  """
  if not sessions_dir.exists():
    return []
  return list(sessions_dir.glob("**/rollout-*.jsonl"))


def _is_plan_pool_event(event: dict[str, Any]) -> bool:
  """Whether a token_count event reports the plan quota pool.

  The plan pool is ``rate_limits.limit_id == "codex"`` or an absent/empty
  ``limit_name``; anything else (e.g. a named model pool) is a model-level
  reading the usage strip must never show.
  """
  rate_limits = event.get("payload", {}).get("rate_limits") or {}
  return rate_limits.get("limit_id") == "codex" or not rate_limits.get("limit_name")


def _latest_token_count_event(
    lines: list[str],
    match: Callable[[dict[str, Any]], bool] | None = None,
) -> dict[str, Any] | None:
  """Return the newest token_count event in *lines*, scanned newest first.

  *match*, when given, filters candidates: an event it rejects (e.g. a
  model-level pool reading) is skipped as if it were not a token_count event.
  """
  for raw_line in reversed(lines):
    line = raw_line.strip()
    if not line:
      continue
    try:
      event = json.loads(line)
    except json.JSONDecodeError:
      continue
    if codex_usage.codex_token_count_payload(event) is None:
      continue
    if match is not None and not match(event):
      continue
    return event
  return None


def _read_latest_token_count_event(
    path: pathlib.Path,
    size: int,
    match: Callable[[dict[str, Any]], bool],
) -> dict[str, Any] | None:
  """Read the newest token_count event in *path*, from a tail window first.

  The window starts at a line boundary: a 0x0A byte never sits inside a
  multi-byte UTF-8 sequence, so decoding the window strictly fails exactly
  where decoding the whole file would. A tail miss (or a tail holding only
  *match*-rejected events) falls back to a full-file read.
  """
  offset = max(0, size - _USAGE_TAIL_BYTES)
  with path.open("rb") as stream:
    stream.seek(offset)
    blob = stream.read()
  if offset:
    blob = blob.split(b"\n", 1)[1] if b"\n" in blob else b""
  event = _latest_token_count_event(blob.decode().splitlines(), match)
  if event is not None or offset == 0:
    return event
  return _latest_token_count_event(path.read_text().splitlines(), match)


def _parse_codex_timestamp(timestamp: Any) -> datetime.datetime:
  if not isinstance(timestamp, str):
    raise ValueError(f"expected string timestamp, got {type(timestamp).__name__}")
  parsed = datetime.datetime.fromisoformat(timestamp)
  if parsed.tzinfo is None:
    raise ValueError(f"Codex timestamp is missing timezone: {timestamp}")
  return parsed.astimezone(datetime.UTC)


def _new_token_usage_bucket() -> dict[str, int]:
  return {
      "input_tokens": 0,
      "cached_input_tokens": 0,
      "output_tokens": 0,
  }


def _add_token_usage(accumulator: dict[str, dict[str, int]], model: str, usage: dict[str, Any]) -> None:
  bucket = accumulator.setdefault(model, _new_token_usage_bucket())
  bucket["input_tokens"] += usage["input_tokens"]
  bucket["cached_input_tokens"] += usage["cached_input_tokens"]
  bucket["output_tokens"] += usage["output_tokens"]


def _sum_codex_spend(accumulator: dict[str, dict[str, int]]) -> float:
  total = 0.0
  for model, usage in accumulator.items():
    cost = codex_pricing.calculate_codex_usage_cost_usd(model, usage)
    if cost is not None:
      total += cost
  return total


def _log_codex_spend_row_skip(path: pathlib.Path, line_number: int, error: Exception | str) -> None:
  log.warning(
      "ext_usage_codex_spend_row_skipped",
      path=str(path),
      line_number=line_number,
      error=str(error),
  )


# One spend event from a rollout log: (observed_at, model, token_usage).
_SpendEvent = tuple[datetime.datetime, str, dict[str, int]]


def _extract_codex_spend_events(path: pathlib.Path) -> list[_SpendEvent] | None:
  # Model attribution is positional (a turn_context line applies to the token_count
  # events after it), so it is resolved here during the single pass. None means the
  # file could not be read (a warning was logged); event lists may be cached, None not.
  events: list[_SpendEvent] = []
  current_model = ""
  try:
    for line_number, raw_line in enumerate(path.read_text().splitlines(), start=1):
      line = raw_line.strip()
      if not line:
        continue
      try:
        event = json.loads(line)
        if not isinstance(event, dict):
          raise ValueError(f"expected JSON object, got {type(event).__name__}")
        event_type = event.get("type")
        payload = event.get("payload") or {}
        if not isinstance(payload, dict):
          raise ValueError(f"payload must be an object, got {type(payload).__name__}")
        if event_type == codex_usage.CODEX_TURN_CONTEXT:
          model = payload.get("model")
          if isinstance(model, str):
            current_model = model
          continue
        if codex_usage.codex_token_count_payload(event) is None:
          continue

        info = payload.get("info") or {}
        if not isinstance(info, dict):
          raise ValueError(f"info must be an object, got {type(info).__name__}")
        last_usage = info.get("last_token_usage")
        if not last_usage:
          continue
        if not isinstance(last_usage, dict):
          raise ValueError(f"last_token_usage must be an object, got {type(last_usage).__name__}")
        token_usage = {
            "input_tokens": last_usage["input_tokens"],
            "cached_input_tokens": last_usage["cached_input_tokens"],
            "output_tokens": last_usage["output_tokens"],
        }
        for key, value in token_usage.items():
          if not isinstance(value, int):
            raise ValueError(f"{key} must be an int, got {type(value).__name__}")

        events.append((_parse_codex_timestamp(event["timestamp"]), current_model, token_usage))
      except (json.JSONDecodeError, KeyError, TypeError, ValueError) as e:
        _log_codex_spend_row_skip(path, line_number, e)
  except OSError as e:
    log.warning("ext_usage_codex_spend_file_skip", path=str(path), error=str(e))
    return None
  return events


def _sum_codex_spend_events(events_by_file: list[list[_SpendEvent]], *, now: datetime.datetime) -> dict[str, float]:
  if now.tzinfo is None:
    raise ValueError("now must be timezone-aware")
  effective_now = now.astimezone(datetime.UTC)
  one_day_ago = effective_now - datetime.timedelta(days=1)
  seven_days_ago = effective_now - datetime.timedelta(days=7)

  last_24h_by_model: dict[str, dict[str, int]] = {}
  last_7d_by_model: dict[str, dict[str, int]] = {}
  for events in events_by_file:
    for observed_at, model, token_usage in events:
      if observed_at < seven_days_ago or observed_at > effective_now:
        continue
      _add_token_usage(last_7d_by_model, model, token_usage)
      if observed_at >= one_day_ago:
        _add_token_usage(last_24h_by_model, model, token_usage)

  return {
      "last_24h_usd": _sum_codex_spend(last_24h_by_model),
      "last_7d_usd": _sum_codex_spend(last_7d_by_model),
  }


def _codex_windows(rate_limits: dict[str, Any], *, account: str) -> list[dict[str, Any]]:
  """Turn every non-null rate-limit slot into a self-describing window entry.

  Slot position carries no meaning; a window is identified by the
  ``window_minutes`` it reports. A slot that omits it is dropped with a warning
  rather than guessed at, because inferring a window from slot order is exactly
  the failure this shape exists to remove.
  """
  windows: list[dict[str, Any]] = []
  for slot in CODEX_LIMIT_SLOTS:
    limit = rate_limits.get(slot)
    if not isinstance(limit, dict):
      continue
    window_minutes = limit.get("window_minutes")
    if isinstance(window_minutes, bool) or not isinstance(window_minutes, int):
      usage_sources.warn_unknown_limit_shape(
          provider="codex", account=account, slot=slot, reason="missing window_minutes")
      continue
    utilization = usage_sources.as_utilization(limit.get("used_percent"))
    if utilization is None:
      usage_sources.warn_unknown_limit_shape(
          provider="codex", account=account, slot=slot, reason="missing used_percent")
    resets_at = limit.get("resets_at")
    windows.append(
        {
            usage_sources.PANEL_WINDOW_MINUTES:
                window_minutes,
            usage_sources.PANEL_UTILIZATION:
                utilization,
            usage_sources.PANEL_RESETS_AT:
                datetime.datetime.fromtimestamp(resets_at, tz=datetime.UTC).isoformat()
                if isinstance(resets_at, (int, float)) and not isinstance(resets_at, bool) else "",
        })
  windows.sort(key=lambda w: w[usage_sources.PANEL_WINDOW_MINUTES])
  return windows


def _codex_credits(raw_credits: Any, *, account: str) -> dict[str, Any] | None:
  """The credits reading to emit, or None when the wire shape is unrecognized.

  ``unlimited`` is forwarded only as the strict bool the wire carries; the
  decimal-string ``balance`` is parsed into a JSON number. Any other shape
  emits nothing and warns through the unknown-limit-shape path, mirroring how
  unknown window shapes are handled.
  """
  if not isinstance(raw_credits, dict):
    usage_sources.warn_unknown_limit_shape(
        provider="codex", account=account, slot="credits", reason="credits is not an object")
    return None
  unlimited = raw_credits.get("unlimited")
  if not isinstance(unlimited, bool):
    usage_sources.warn_unknown_limit_shape(
        provider="codex", account=account, slot="credits", reason="unlimited is not a bool")
    return None
  emitted: dict[str, Any] = {"unlimited": unlimited}
  try:
    emitted["balance"] = float(raw_credits.get("balance"))
  except (TypeError, ValueError):
    usage_sources.warn_unknown_limit_shape(
        provider="codex", account=account, slot="credits", reason="missing or unparseable balance")
  return emitted


def _transform_codex_response(
    event: dict[str, Any],
    *,
    fetched_at: str,
    account: str = "",
) -> dict[str, Any]:
  """Transform a Codex token_count event into our cached usage format."""
  payload = event.get("payload", {})
  rate_limits = payload.get("rate_limits") or {}
  primary = rate_limits.get("primary")
  secondary = rate_limits.get("secondary")
  # An absent credits key (older CLI events) and a present-but-unreadable one
  # are different states: only the latter warns, and only the former keeps the
  # plan_type fallback below in play.
  credits_reading = _codex_credits(rate_limits["credits"], account=account) if "credits" in rate_limits else None

  usage = {
      usage_sources.PANEL_WINDOWS: _codex_windows(rate_limits, account=account),
      usage_sources.PANEL_FETCHED_AT: fetched_at,
      usage_sources.PANEL_PROVIDER: "codex",
      "token_count_observed_at": event.get("timestamp", ""),
  }
  if credits_reading is not None:
    usage["credits"] = credits_reading
  if ("primary" in rate_limits and "secondary" in rate_limits and primary is None and secondary is None and
      ((credits_reading is not None and credits_reading["unlimited"] is True) or
       ("credits" not in rate_limits and rate_limits.get("plan_type") == "business"))):
    usage["rate_limits_state"] = "business-unlimited"
  return usage
