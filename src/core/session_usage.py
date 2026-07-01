"""Session usage resolution and caching."""

import asyncio
from pathlib import Path
from typing import Callable

from src.core import event_types as ET
from src.core.codex_usage import CodexUsageResolver
from src.core.config import CharlieBotConfig
from src.core.models import SessionMetadata

_DEFAULT_CONTEXT_LIMIT = 200_000


def _extract_usage_from_result(event: dict) -> tuple[int, int, str]:
  """Extract (context_tokens, context_limit, model) from a single 'result' event.

  context_tokens = input_tokens + cache_creation_input_tokens + cache_read_input_tokens.
  context_limit and model come from the first modelUsage entry (defaults:
  200_000 and "").
  """
  usage = event.get("usage", {})
  context_tokens = (
      usage.get("input_tokens", 0) + usage.get("cache_creation_input_tokens", 0) +
      usage.get("cache_read_input_tokens", 0))
  context_limit = _DEFAULT_CONTEXT_LIMIT
  model = ""
  for model_name, info in event.get("modelUsage", {}).items():
    model = model_name
    context_limit = info.get("contextWindow", _DEFAULT_CONTEXT_LIMIT)
    break
  return context_tokens, context_limit, model


class SessionUsageResolver:
  """Resolve and cache display usage for sessions."""

  def __init__(
      self,
      cfg: CharlieBotConfig,
      events_cache: dict[str, list[dict]],
      chat_events_path_fn: Callable[[str], Path],
      load_chat_events_sync_fn: Callable[[str], list[dict]],
  ):
    # In-memory usage cache: session_id -> usage dict (context_tokens, context_limit, total_cost_usd, model).
    # Incrementally updated on each 'result' event via save_chat_event(), avoiding O(n) rescans.
    self._usage_cache: dict[str, dict] = {}
    self._load_chat_events_sync = load_chat_events_sync_fn
    # Codex-specific usage resolution delegated to a dedicated module.
    self._codex_resolver = CodexUsageResolver(cfg, events_cache, chat_events_path_fn)

  def clear_cache(self, session_id: str) -> None:
    self._usage_cache.pop(session_id, None)

  async def resolve_session_usage(
      self,
      session_id: str,
      session_meta: SessionMetadata,
      events: list[dict] | None = None,
  ) -> dict | None:
    """Resolve display usage for a session view.

    Non-Codex backends keep the existing CharlieBot result-derived behavior.
    Codex backends override context usage from the native rollout log when the
    native thread id can be resolved.
    """
    usage = self.get_usage_cached(session_id)
    if usage is None and events is not None:
      usage = self.usage_from_events(events)
      if usage is not None:
        self._usage_cache[session_id] = usage
    if usage is None and events is None:
      await asyncio.to_thread(self._load_chat_events_sync, session_id)
      usage = self.get_usage_cached(session_id)

    if not self._codex_resolver.is_codex_backend(session_meta.backend):
      return usage

    merged = await asyncio.to_thread(
        self._codex_resolver.resolve, session_id, session_meta.cc_session_id, events, usage)
    if merged is not None:
      self._usage_cache[session_id] = merged
      return merged
    return usage

  def cache_usage_from_events(self, session_id: str, events: list[dict]) -> None:
    # Always derive usage from a full load so the cache stays exact.
    usage = self.usage_from_events(events)
    if usage:
      self._usage_cache[session_id] = usage
    else:
      self._usage_cache.pop(session_id, None)

  @staticmethod
  def usage_from_events(events: list[dict]) -> dict | None:
    """Extract context-window token usage from pre-loaded events.

    Scans for the most recent 'result' event and accumulates total_cost_usd
    across ALL result events.

    Returns a dict with:
      context_tokens  – input + cache_creation + cache_read from last result
      context_limit   – from modelUsage contextWindow (default 200000)
      total_cost_usd  – sum across every result event
      model           – primary model name
    Returns None if no result events exist.
    """
    if not events:
      return None

    last_result: dict | None = None
    last_usage_result: dict | None = None
    total_cost = 0.0
    unknown_cost = False

    for ev in events:
      if ev.get("type") != ET.RESULT:
        continue
      last_result = ev
      event_cost = ev.get("total_cost_usd", 0.0)
      if event_cost is None:
        unknown_cost = True
      else:
        total_cost += event_cost
      if _extract_usage_from_result(ev)[0] > 0:
        last_usage_result = ev

    if last_result is None:
      return None

    context_tokens, context_limit, model = _extract_usage_from_result(last_usage_result or last_result)
    return {
        "context_tokens": context_tokens,
        "context_limit": context_limit,
        "total_cost_usd": None if unknown_cost else round(total_cost, 4),
        "model": model,
    }

  def get_usage_cached(self, session_id: str) -> dict | None:
    """Return cached usage data for a session, or None if not yet computed."""
    return self._usage_cache.get(session_id)

  def _update_usage_cache(self, session_id: str, result_event: dict) -> None:
    """Incrementally update the usage cache from a single 'result' event."""
    cached = self._usage_cache.get(session_id) or {
        "context_tokens": 0,
        "context_limit": _DEFAULT_CONTEXT_LIMIT,
        "total_cost_usd": 0.0,
        "model": "",
    }
    event_cost = result_event.get("total_cost_usd", 0.0)
    if event_cost is None:
      cached["total_cost_usd"] = None
    elif cached["total_cost_usd"] is not None:
      cached["total_cost_usd"] = round(cached["total_cost_usd"] + event_cost, 4)
    ctx, context_limit, model = _extract_usage_from_result(result_event)
    if ctx > 0:
      cached["context_tokens"] = ctx
      cached["context_limit"] = context_limit
      cached["model"] = model
    self._usage_cache[session_id] = cached
