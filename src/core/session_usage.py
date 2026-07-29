"""Session usage resolution — a read-only projection over the in-memory event list.

Usage is computed on demand from the full chat-event stream; no incremental cache
is maintained. See the plan ``panel-usage-readout-fix`` for the tier contract.
"""

import asyncio
from typing import Callable

from src.agents.backends.claude_code import headless_claude_context_ceiling
from src.core import event_types as ET
from src.core.codex_usage import CodexUsageResolver
from src.core.config import CharlieBotConfig
from src.core.models import SessionMetadata


def _prompt_token_sum(usage: dict) -> int:
  """input + cache_creation + cache_read from a Claude Code usage block."""
  return (
      usage.get("input_tokens", 0)
      + usage.get("cache_creation_input_tokens", 0)
      + usage.get("cache_read_input_tokens", 0))


def _cost_from_result_events(events: list[dict]) -> float | None:
  """Sum ``total_cost_usd`` over every result event in the full event list.

  Returns ``None`` when no result event reported a positive cost, or when any
  result event's ``total_cost_usd`` is ``None``. A sum of exactly 0 means
  "never reported", not "free".
  """
  total_cost = 0.0
  unknown_cost = False
  for ev in events:
    if ev.get("type") != ET.RESULT:
      continue
    event_cost = ev.get("total_cost_usd", 0.0)
    if event_cost is None:
      unknown_cost = True
    else:
      total_cost += event_cost
  if unknown_cost or total_cost <= 0:
    return None
  return round(total_cost, 4)


def _model_context_windows_from_results(events: list[dict]) -> dict[str, int]:
  """Merge ``modelUsage[model].contextWindow`` across every result event (last wins)."""
  windows: dict[str, int] = {}
  for ev in events:
    if ev.get("type") != ET.RESULT:
      continue
    for model_name, info in (ev.get("modelUsage") or {}).items():
      context_window = info.get("contextWindow")
      if isinstance(context_window, int):
        windows[model_name] = context_window
  return windows


def _resolve_claude_tier(events: list[dict]) -> dict | None:
  """Resolve usage from the last main-chain assistant event with positive prompt tokens.

  context_tokens / model come from that assistant event's ``message.usage`` /
  ``message.model``. context_limit is ``min(contextWindow of modelUsage[model],
  ceiling)`` where ``modelUsage`` is merged from result events; when the assistant
  model is absent from ``modelUsage`` the ceiling alone is used. cost is summed over
  result events. Returns ``None`` when no qualifying assistant event exists.
  """
  chosen: dict | None = None
  for ev in events:
    if ev.get("type") != ET.ASSISTANT:
      continue
    if ev.get("parent_tool_use_id"):
      continue
    message = ev.get("message") or {}
    usage = message.get("usage") or {}
    if _prompt_token_sum(usage) > 0:
      chosen = ev

  if chosen is None:
    return None

  message = chosen.get("message") or {}
  usage = message.get("usage") or {}
  context_tokens = _prompt_token_sum(usage)
  model = message.get("model") or ""
  ceiling = headless_claude_context_ceiling()
  windows = _model_context_windows_from_results(events)
  if model and model in windows:
    context_limit = min(windows[model], ceiling)
  else:
    context_limit = ceiling
  return {
      "context_tokens": context_tokens,
      "context_limit": context_limit,
      "total_cost_usd": _cost_from_result_events(events),
      "model": model,
  }


def _resolve_no_source_tier(events: list[dict]) -> dict:
  """Usage object when neither the claude nor codex tier applies.

  context_tokens / context_limit are ``None`` (rendered as ``unknown``); cost is
  still summed over result events.
  """
  return {
      "context_tokens": None,
      "context_limit": None,
      "total_cost_usd": _cost_from_result_events(events),
      "model": "",
  }


class SessionUsageResolver:
  """Resolve display usage for a session as a projection over its event list."""

  def __init__(
      self,
      cfg: CharlieBotConfig,
      events_cache: dict[str, list[dict]],
      chat_events_path_fn: Callable[[str], "object"],
      load_chat_events_sync_fn: Callable[[str], list[dict]],
  ):
    self._load_chat_events_sync = load_chat_events_sync_fn
    self._codex_resolver = CodexUsageResolver(cfg, events_cache, chat_events_path_fn)

  async def resolve_session_usage(
      self,
      session_id: str,
      session_meta: SessionMetadata,
  ) -> dict | None:
    """Resolve display usage for a session view.

    Loads the full event list itself (memoised by the chat-events cache, so no
    extra disk I/O) and selects a tier by data availability, not backend name:

    - claude: a main-chain ``assistant`` event with positive prompt tokens exists.
    - codex: the backend is codex and the native rollout resolves.
    - no-source: neither — context fields are ``None``.

    Returns ``None`` only when the event list is empty.
    """
    events = await asyncio.to_thread(self._load_chat_events_sync, session_id)

    usage = _resolve_claude_tier(events)
    if usage is not None:
      return usage

    if self._codex_resolver.is_codex_backend(session_meta.backend):
      merged = await asyncio.to_thread(
          self._codex_resolver.resolve, session_id, session_meta, events)
      if merged is not None:
        return merged

    if not events:
      return None
    return _resolve_no_source_tier(events)

  @staticmethod
  def usage_from_events(events: list[dict]) -> dict | None:
    """Pure projection over a pre-loaded event list (no caching).

    Provided for callers that already hold the full list. Resolves the claude
    tier, then the no-source tier; the codex tier requires backend + rollout
    state that this entry point does not have, so callers needing codex should
    use ``resolve_session_usage`` instead.
    """
    if not events:
      return None
    usage = _resolve_claude_tier(events)
    if usage is not None:
      return usage
    return _resolve_no_source_tier(events)
