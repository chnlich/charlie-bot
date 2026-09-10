"""Codex-specific context-window usage resolution from native rollout logs.

The rollout record-type names are wire bytes an outside producer (the Codex CLI)
writes; this module defines the CODEX_* constants for them, and the other rollout
readers (token_tally, ext_usage) import them instead of restating the strings.
"""

from collections.abc import Callable
from pathlib import Path
from typing import Any

import structlog

from src.core import event_types as ET
from src.core.codex_pricing import calculate_codex_usage_cost_usd
from src.core.config import CharlieBotConfig
from src.core.models import BackendType
from src.core.ndjson import iter_ndjson_events

log = structlog.get_logger()

# Codex rollout record-type wire names: session_meta opens a thread file,
# turn_context carries the model in force, and event_msg wraps the token_count
# payload the usage extractors read.
CODEX_SESSION_META = "session_meta"
CODEX_TURN_CONTEXT = "turn_context"
CODEX_EVENT_MSG = "event_msg"
CODEX_TOKEN_COUNT = "token_count"

# Default codex home searched last in the candidate directory list.
_DEFAULT_CODEX_HOME = Path.home() / ".codex"


def _extract_codex_rollout_usage_event(event: dict[str, Any]) -> dict[str, Any] | None:
  """Return context usage from a native Codex token_count event."""
  if event.get("type") != CODEX_EVENT_MSG:
    return None
  payload = event.get("payload") or {}
  if payload.get("type") != CODEX_TOKEN_COUNT:
    return None
  info = payload.get("info") or {}
  last_usage = info.get("last_token_usage") or {}
  input_tokens = last_usage.get("input_tokens")
  model_context_window = info.get("model_context_window")
  if input_tokens is None or model_context_window is None:
    return None
  usage: dict[str, Any] = {
      # Codex reports the active prompt window in last_token_usage.input_tokens.
      # total_token_usage is cumulative for the whole session and cached_input_tokens
      # is an informational subset, not an additive context-window component.
      ET.CONTEXT_TOKENS:
          input_tokens,
      # The bar's full scale is the model's context window (the longest context the
      # prompt can reach); the compaction line comes from the backend option and is
      # merged in by the resolver.
      ET.CONTEXT_FULL:
          model_context_window,
  }
  total_token_usage = info.get("total_token_usage")
  if isinstance(total_token_usage, dict):
    usage["total_token_usage"] = {
        "input_tokens": total_token_usage.get("input_tokens", 0),
        "cached_input_tokens": total_token_usage.get("cached_input_tokens", 0),
        "output_tokens": total_token_usage.get("output_tokens", 0),
    }
  return usage


def _extract_codex_rollout_model_event(event: dict[str, Any]) -> str | None:
  """Return the model from a native Codex turn_context event."""
  if event.get("type") != CODEX_TURN_CONTEXT:
    return None
  payload = event.get("payload") or {}
  model = payload.get("model")
  if isinstance(model, str) and model:
    return model
  return None


def _extract_latest_codex_rollout_usage(path: Path) -> dict[str, Any] | None:
  """Scan a native Codex rollout log backwards for latest token_count and model."""
  if not path.exists():
    return None

  usage: dict[str, Any] | None = None
  model: str | None = None
  chunk_size = 8192
  with open(path, "rb") as f:
    f.seek(0, 2)
    pos = f.tell()
    carry = b""

    while pos > 0:
      read_size = min(chunk_size, pos)
      pos -= read_size
      f.seek(pos)
      chunk = f.read(read_size)
      parts = (chunk + carry).split(b"\n")
      carry = parts[0] if pos > 0 else b""
      lines = parts[1:] if pos > 0 else parts

      for event in iter_ndjson_events(reversed(lines), log_event="codex_rollout_parse_skip",
                                      log_fields={"path": str(path)}):
        if usage is None:
          usage = _extract_codex_rollout_usage_event(event)
        if model is None:
          model = _extract_codex_rollout_model_event(event)
        if usage is not None and model is not None:
          usage["model"] = model
          return usage

    for event in iter_ndjson_events([carry], log_event="codex_rollout_parse_skip", log_fields={"path": str(path)}):
      if usage is None:
        usage = _extract_codex_rollout_usage_event(event)
      if model is None:
        model = _extract_codex_rollout_model_event(event)

  if usage is None:
    return None
  usage["model"] = model or ""
  return usage


class CodexUsageResolver:
  """Resolves context-window usage from native Codex rollout logs.

  Encapsulates all Codex-specific thread-id resolution, rollout log discovery,
  and usage extraction.
  """

  def __init__(
      self,
      cfg: CharlieBotConfig,
      events_cache: dict[str, list[dict]],
      chat_events_path_fn: Callable[[str], Path],
  ) -> None:
    self._cfg = cfg
    self._events_cache = events_cache
    self._chat_events_path_fn = chat_events_path_fn
    self._codex_rollout_path_cache: dict[str, Path] = {}
    self._codex_rollout_usage_cache: dict[str, tuple[int, int, dict | None]] = {}

  def is_codex_backend(self, backend_id: str) -> bool:
    option = self._cfg.get_backend_option(backend_id)
    if option is not None:
      return option.type == BackendType.CODEX
    # A session pinned to a backend id since removed from config admits by prefix.
    return backend_id.startswith("codex")

  def resolve(
      self,
      session_id: str,
      session_meta: Any,
      events: list[dict],
  ) -> dict | None:
    """Resolve Codex-native usage and merge with base usage.

    Returns the merged usage dict with Codex context_tokens/context_full/
    context_compact_at overriding the base values, or None if native usage is
    unavailable.

    ``context_full`` uses the rollout's ``model_context_window`` (the longest
    context the prompt can reach). ``context_compact_at`` uses the session
    backend's ``model_auto_compact_token_limit`` when configured, otherwise
    ``None`` — an unconfigured compaction limit is the normal state, not a
    degradation, so no warning is logged. The cost computation stays native
    (``calculate_codex_usage_cost_usd``).
    """
    backend_id = session_meta.backend
    native_thread_id = self._resolve_codex_thread_id(session_id, session_meta.cc_session_id, events)
    if not native_thread_id:
      return None

    native_usage = self._load_codex_rollout_usage(native_thread_id)
    if native_usage is None:
      return None

    auto_compact_limit = self._backend_auto_compact_limit(backend_id)
    context_compact_at: int | None = auto_compact_limit
    model = native_usage.get("model") or ""
    total_token_usage = native_usage.get("total_token_usage")
    merged_usage: dict[str, Any] = {
        ET.CONTEXT_TOKENS: native_usage[ET.CONTEXT_TOKENS],
        ET.CONTEXT_FULL: native_usage[ET.CONTEXT_FULL],
        ET.CONTEXT_COMPACT_AT: context_compact_at,
        ET.RESULT_TOTAL_COST_USD:
            (calculate_codex_usage_cost_usd(model, total_token_usage) if total_token_usage else None),
        "model": model,
    }
    return merged_usage

  def _backend_auto_compact_limit(self, backend_id: str) -> int | None:
    """Return the session backend's ``model_auto_compact_token_limit`` when configured."""
    if not backend_id:
      return None
    option = self._cfg.get_backend_option(backend_id)
    if option is None:
      return None
    return option.model_auto_compact_token_limit

  @staticmethod
  def _extract_translated_session_id(events: list[dict]) -> str | None:
    for event in events:
      session_id = event.get("session_id")
      if isinstance(session_id, str) and session_id:
        return session_id
    return None

  def _read_translated_session_id(self, session_id: str) -> str | None:
    cached_events = self._events_cache.get(session_id)
    if cached_events is not None:
      cached_session_id = self._extract_translated_session_id(cached_events)
      if cached_session_id:
        return cached_session_id

    path = self._chat_events_path_fn(session_id)
    if not path.exists():
      return None

    with open(path, encoding="utf-8") as f:
      for event in iter_ndjson_events(f, log_event="translated_session_id_parse_skip", log_fields={"path": str(path)}):
        session_id_value = event.get("session_id")
        if isinstance(session_id_value, str) and session_id_value:
          return session_id_value
    return None

  def _resolve_codex_thread_id(
      self,
      session_id: str,
      persisted_session_id: str | None,
      events: list[dict] | None = None,
  ) -> str | None:
    if persisted_session_id:
      return persisted_session_id
    if events is not None:
      live_session_id = self._extract_translated_session_id(events)
      if live_session_id:
        return live_session_id
    return self._read_translated_session_id(session_id)

  def _find_codex_rollout_path(self, native_thread_id: str) -> Path | None:
    cached_path = self._codex_rollout_path_cache.get(native_thread_id)
    if cached_path is not None and cached_path.exists():
      return cached_path

    # Codex runs from the default home, so the corpus is that home's sessions
    # tree alone; no per-backend or config-provided home exists to search.
    candidate_dir = _DEFAULT_CODEX_HOME / "sessions"
    if not candidate_dir.exists():
      return None
    matches = list(candidate_dir.rglob(f"rollout-*{native_thread_id}.jsonl"))
    if not matches:
      return None
    matches.sort(key=lambda path: path.stat().st_mtime, reverse=True)
    rollout_path = matches[0]
    self._codex_rollout_path_cache[native_thread_id] = rollout_path
    return rollout_path

  def _load_codex_rollout_usage(self, native_thread_id: str) -> dict | None:
    rollout_path = self._find_codex_rollout_path(native_thread_id)
    if rollout_path is None:
      return None

    stat = rollout_path.stat()
    cached_usage = self._codex_rollout_usage_cache.get(native_thread_id)
    if cached_usage is not None:
      cached_mtime_ns, cached_size, usage = cached_usage
      if cached_mtime_ns == stat.st_mtime_ns and cached_size == stat.st_size:
        return usage

    usage = _extract_latest_codex_rollout_usage(rollout_path)
    self._codex_rollout_usage_cache[native_thread_id] = (stat.st_mtime_ns, stat.st_size, usage)
    return usage
