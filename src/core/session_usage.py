"""Session usage resolution — a read-only projection over the in-memory event list.

Usage is computed on demand from the full chat-event stream; no incremental cache
is maintained. See the plan ``panel-usage-readout-fix`` for the tier contract.

The usage dict carries four context fields plus cost and model:

    {
      "context_tokens":     int | None,   # used
      "context_full":       int | None,   # bar's full scale
      "context_compact_at": int | None,   # vertical line; None = draw no line
      "total_cost_usd":     float | None,
      "model":              str,
    }

``context_full`` is the longest context the prompt can reach on this backend;
``context_compact_at`` is where that backend compacts (a vertical line on the
bar). ``None`` for any context field means "unknown" — the bar is hidden.
"""

import asyncio
from collections.abc import Callable

from src.agents.backends.claude_code import (
    CLAUDE_COMPACT_CONTEXT_RESERVE,
    CLAUDE_COMPACT_OUTPUT_RESERVE,
    headless_claude_declared_window,
)
from src.agents.backends.opencode import OPENCODE_COMPACT_OUTPUT_RESERVE
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


def _usage_dict(
    context_tokens: int | None,
    context_full: int | None,
    context_compact_at: int | None,
    model: str,
    events: list[dict],
) -> dict:
  """Single construction site for the usage-dict shape declared in the module docstring.

  Every tier fills the four context fields from its own source; the cost field
  is always the sum over result events of the same full event list.
  """
  return {
      "context_tokens": context_tokens,
      "context_full": context_full,
      "context_compact_at": context_compact_at,
      "total_cost_usd": _cost_from_result_events(events),
      "model": model,
  }


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


def _post_compact_tokens_after(events: list[dict], chosen_idx: int) -> int | None:
  """Latest ``compact_boundary`` event's int ``post_tokens`` strictly after ``chosen_idx``.

  Returns ``None`` when no qualifying boundary exists after that index.
  """
  post_tokens: int | None = None
  for ev in events[chosen_idx + 1:]:
    if ev.get("type") != ET.SYSTEM or ev.get("subtype") != ET.COMPACT_BOUNDARY:
      continue
    candidate = (ev.get(ET.COMPACT_METADATA) or {}).get("post_tokens")
    if isinstance(candidate, int):
      post_tokens = candidate
  return post_tokens


def _resolve_claude_tier(events: list[dict]) -> dict | None:
  """Resolve usage from the last main-chain assistant event with positive prompt tokens.

  context_tokens is that assistant event's ``message.usage`` prompt-token sum, unless
  a ``compact_boundary`` event with an int ``compact_metadata.post_tokens`` occurs
  after it -- in which case the latest such ``post_tokens`` wins. ``model`` comes from
  the assistant event's ``message.model``. context_full is ``min(contextWindow of
  modelUsage[model], declared_window)``; when the assistant model is absent from
  ``modelUsage`` the declared window alone is used. context_compact_at is
  ``context_full - OUTPUT_RESERVE - CONTEXT_RESERVE``; it is ``None`` when the
  declared window is degraded (a forwarded-but-unmodelled override is present).
  cost is summed over result events. Returns ``None`` when no qualifying
  assistant event exists.
  """
  chosen: dict | None = None
  chosen_idx = -1
  for idx, ev in enumerate(events):
    if ev.get("type") != ET.ASSISTANT:
      continue
    if ev.get("parent_tool_use_id"):
      continue
    message = ev.get("message") or {}
    usage = message.get("usage") or {}
    if _prompt_token_sum(usage) > 0:
      chosen = ev
      chosen_idx = idx

  if chosen is None:
    return None

  message = chosen.get("message") or {}
  usage = message.get("usage") or {}
  post_tokens = _post_compact_tokens_after(events, chosen_idx)
  context_tokens = post_tokens if post_tokens is not None else _prompt_token_sum(usage)
  model = message.get("model") or ""
  declared_window, compact_point = headless_claude_declared_window()
  windows = _model_context_windows_from_results(events)
  if model and model in windows:
    context_full = min(windows[model], declared_window)
  else:
    context_full = declared_window
  if compact_point is None:
    context_compact_at: int | None = None
  else:
    context_compact_at = context_full - CLAUDE_COMPACT_OUTPUT_RESERVE - CLAUDE_COMPACT_CONTEXT_RESERVE
  return _usage_dict(context_tokens, context_full, context_compact_at, model, events)


def _snapshot_tokens_sum(tokens: dict) -> int:
  """Sum the five token fields of a context_snapshot ``tokens`` block."""
  return (
      tokens.get("input", 0)
      + tokens.get("output", 0)
      + tokens.get("reasoning", 0)
      + tokens.get("cache_read", 0)
      + tokens.get("cache_write", 0))


def _snapshot_full_and_compact(limit: dict) -> tuple[int | None, int | None]:
  """Derive (context_full, context_compact_at) from a snapshot ``limit`` dict.

  context_full is ``limit.input`` when present, else ``limit.context - limit.output``.
  context_compact_at is ``limit.input - min(OPENCODE_COMPACT_OUTPUT_RESERVE, limit.output)``
  when ``input`` is present; otherwise ``None`` (it would coincide with full, so the
  line carries no information). A non-int ``output`` (e.g. a JSON ``null`` in the
  catalog payload) degrades to ``(None, None)`` rather than raising ``TypeError``.
  """
  context_input = limit.get("input")
  context_output = limit.get("output", 0)
  context_context = limit.get("context")
  if isinstance(context_input, int) and isinstance(context_output, int):
    context_full = context_input
    context_compact_at = context_input - min(OPENCODE_COMPACT_OUTPUT_RESERVE, context_output)
  elif isinstance(context_context, int) and isinstance(context_output, int):
    context_full = context_context - context_output
    context_compact_at = None
  else:
    context_full = None
    context_compact_at = None
  return context_full, context_compact_at


def _resolve_snapshot_tier(events: list[dict]) -> dict | None:
  """Resolve usage from the newest result event carrying a ``context_snapshot``.

  context_tokens is the sum of the snapshot's five ``tokens`` fields. context_full /
  context_compact_at derive from the snapshot's ``limit`` (see
  ``_snapshot_full_and_compact``); both are ``None`` when ``limit`` is ``None``.
  cost is summed over result events. Returns ``None`` when no result event carries
  a ``context_snapshot``.
  """
  snapshot: dict | None = None
  for ev in events:
    if ev.get("type") != ET.RESULT:
      continue
    candidate = ev.get("context_snapshot")
    if isinstance(candidate, dict):
      snapshot = candidate
  if snapshot is None:
    return None
  tokens = snapshot.get("tokens") or {}
  context_tokens = _snapshot_tokens_sum(tokens)
  limit = snapshot.get("limit")
  if isinstance(limit, dict):
    context_full, context_compact_at = _snapshot_full_and_compact(limit)
  else:
    context_full, context_compact_at = None, None
  model = snapshot.get("model") or ""
  return _usage_dict(context_tokens, context_full, context_compact_at, model, events)


def _resolve_no_source_tier(events: list[dict]) -> dict:
  """Usage object when no tier applies.

  context_tokens / context_full / context_compact_at are ``None`` (rendered as
  ``unknown``); cost is still summed over result events.
  """
  return _usage_dict(
      context_tokens=None,
      context_full=None,
      context_compact_at=None,
      model="",
      events=events)


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
    - snapshot: a result event carries a ``context_snapshot`` (opencode backend).
    - no-source: none of the above — context fields are ``None``.

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

    usage = _resolve_snapshot_tier(events)
    if usage is not None:
      return usage

    if not events:
      return None
    return _resolve_no_source_tier(events)
