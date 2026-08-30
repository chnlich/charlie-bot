"""Session usage resolution — a read-only projection over the in-memory event list.

Usage is computed on demand in ONE pass over the full chat-event stream; no
incremental cache is maintained. The ``/api/sessions/<id>/usage`` endpoint is
polled every 3 s per browser tab while the viewed session is thinking, so one
scan per resolution keeps that recurring CPU charge proportional to the event
count once, not to the tier count. See the plan ``panel-usage-readout-fix``
for the tier contract.

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
from dataclasses import dataclass

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


@dataclass(frozen=True)
class _UsageFacts:
  """One scan's harvest of everything the usage tiers read from the event list.

  ``chosen_prompt_tokens`` > 0 selects the claude tier: the value is the chosen
  assistant event's usage prompt-token sum, and ``chosen_model`` is its
  ``message.model``. ``post_compact_tokens`` is the latest ``compact_boundary``
  int ``post_tokens`` strictly after that assistant event, or None.
  ``model_windows`` merges ``modelUsage[model].contextWindow`` across result
  events (last wins). ``snapshot`` is the newest result event's dict
  ``context_snapshot``, or None. ``cost`` is the rounded ``total_cost_usd`` sum
  over result events: None when any of them is None or the sum is not positive.
  """

  chosen_prompt_tokens: int
  chosen_model: str
  post_compact_tokens: int | None
  model_windows: dict[str, int]
  snapshot: dict | None
  cost: float | None


def _scan_usage_facts(events: list[dict]) -> _UsageFacts:
  """Collect every tier's inputs in one pass over *events*.

  A ``compact_boundary`` counts only after the assistant event the claude tier
  will choose: a newer qualifying assistant resets the boundary seen so far,
  so the final value is the latest ``post_tokens`` strictly after the chosen
  assistant — the same set a suffix scan from the chosen index would consider.
  """
  chosen_prompt_tokens = 0
  chosen_model = ""
  post_compact_tokens: int | None = None
  model_windows: dict[str, int] = {}
  snapshot: dict | None = None
  total_cost = 0.0
  unknown_cost = False
  for ev in events:
    kind = ev.get("type")
    if kind == ET.ASSISTANT:
      if ev.get("parent_tool_use_id"):
        continue
      message = ev.get("message") or {}
      prompt_tokens = _prompt_token_sum(message.get("usage") or {})
      if prompt_tokens > 0:
        chosen_prompt_tokens = prompt_tokens
        chosen_model = message.get("model") or ""
        post_compact_tokens = None
    elif kind == ET.SYSTEM:
      if chosen_prompt_tokens > 0 and ev.get("subtype") == ET.COMPACT_BOUNDARY:
        candidate = (ev.get(ET.COMPACT_METADATA) or {}).get("post_tokens")
        if isinstance(candidate, int):
          post_compact_tokens = candidate
    elif kind == ET.RESULT:
      for model_name, info in (ev.get("modelUsage") or {}).items():
        window = info.get("contextWindow")
        if isinstance(window, int):
          model_windows[model_name] = window
      candidate_snapshot = ev.get("context_snapshot")
      if isinstance(candidate_snapshot, dict):
        snapshot = candidate_snapshot
      event_cost = ev.get("total_cost_usd", 0.0)
      if event_cost is None:
        unknown_cost = True
      else:
        total_cost += event_cost
  cost = round(total_cost, 4)
  if unknown_cost or total_cost <= 0:
    cost = None
  return _UsageFacts(
      chosen_prompt_tokens=chosen_prompt_tokens,
      chosen_model=chosen_model,
      post_compact_tokens=post_compact_tokens,
      model_windows=model_windows,
      snapshot=snapshot,
      cost=cost,
  )


def _usage_dict(
    context_tokens: int | None,
    context_full: int | None,
    context_compact_at: int | None,
    model: str,
    cost: float | None,
) -> dict:
  """Single construction site for the usage-dict shape declared in the module docstring.

  Every tier fills the four context fields from its own source; the cost field
  is the shared scan's sum over result events.
  """
  return {
      "context_tokens": context_tokens,
      "context_full": context_full,
      "context_compact_at": context_compact_at,
      "total_cost_usd": cost,
      "model": model,
  }


def _resolve_claude_tier(facts: _UsageFacts) -> dict | None:
  """Resolve usage from the last main-chain assistant event with positive prompt tokens.

  context_tokens is that assistant event's ``message.usage`` prompt-token sum, unless
  a ``compact_boundary`` event with an int ``compact_metadata.post_tokens`` occurs
  after it -- in which case the latest such ``post_tokens`` wins. ``model`` comes from
  the assistant event's ``message.model``. context_full is ``min(contextWindow of
  modelUsage[model], declared_window)``; when the assistant model is absent from
  ``modelUsage`` the declared window alone is used. context_compact_at is
  ``context_full - OUTPUT_RESERVE - CONTEXT_RESERVE``; it is ``None`` when the
  declared window is degraded (a forwarded-but-unmodelled override is present).
  Returns ``None`` when no qualifying assistant event exists.
  """
  if facts.chosen_prompt_tokens <= 0:
    return None
  context_tokens = (
      facts.post_compact_tokens
      if facts.post_compact_tokens is not None
      else facts.chosen_prompt_tokens)
  declared_window, compact_point = headless_claude_declared_window()
  if facts.chosen_model and facts.chosen_model in facts.model_windows:
    context_full = min(facts.model_windows[facts.chosen_model], declared_window)
  else:
    context_full = declared_window
  if compact_point is None:
    context_compact_at: int | None = None
  else:
    context_compact_at = context_full - CLAUDE_COMPACT_OUTPUT_RESERVE - CLAUDE_COMPACT_CONTEXT_RESERVE
  return _usage_dict(context_tokens, context_full, context_compact_at, facts.chosen_model, facts.cost)


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


def _resolve_snapshot_tier(facts: _UsageFacts) -> dict | None:
  """Resolve usage from the newest result event carrying a ``context_snapshot``.

  context_tokens is the sum of the snapshot's five ``tokens`` fields. context_full /
  context_compact_at derive from the snapshot's ``limit`` (see
  ``_snapshot_full_and_compact``); both are ``None`` when ``limit`` is ``None``.
  Returns ``None`` when no result event carries a ``context_snapshot``.
  """
  snapshot = facts.snapshot
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
  return _usage_dict(context_tokens, context_full, context_compact_at, model, facts.cost)


def _resolve_no_source_tier(facts: _UsageFacts) -> dict:
  """Usage object when no tier applies.

  context_tokens / context_full / context_compact_at are ``None`` (rendered as
  ``unknown``); cost is the shared scan's sum over result events.
  """
  return _usage_dict(
      context_tokens=None,
      context_full=None,
      context_compact_at=None,
      model="",
      cost=facts.cost)


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
    extra disk I/O) and scans it once; the tiers then read the scan's facts,
    selected by data availability, not backend name:

    - claude: a main-chain ``assistant`` event with positive prompt tokens exists.
    - codex: the backend is codex and the native rollout resolves.
    - snapshot: a result event carries a ``context_snapshot`` (opencode backend).
    - no-source: none of the above — context fields are ``None``.

    Returns ``None`` only when the event list is empty.
    """
    events = await asyncio.to_thread(self._load_chat_events_sync, session_id)
    facts = _scan_usage_facts(events)

    usage = _resolve_claude_tier(facts)
    if usage is not None:
      return usage

    if self._codex_resolver.is_codex_backend(session_meta.backend):
      merged = await asyncio.to_thread(
          self._codex_resolver.resolve, session_id, session_meta, events)
      if merged is not None:
        return merged

    usage = _resolve_snapshot_tier(facts)
    if usage is not None:
      return usage

    if not events:
      return None
    return _resolve_no_source_tier(facts)
