"""Codex-specific context-window usage resolution from native rollout logs."""

import json
from collections.abc import Callable
from pathlib import Path
from typing import Any

import structlog

from src.core.codex_pricing import calculate_codex_usage_cost_usd
from src.core.config import CharlieBotConfig

log = structlog.get_logger()

# Default codex home searched last in the candidate directory list.
_DEFAULT_CODEX_HOME = Path.home() / ".codex"


def _extract_codex_rollout_usage_event(event: dict[str, Any]) -> dict[str, Any] | None:
  """Return context usage from a native Codex token_count event."""
  if event.get("type") != "event_msg":
    return None
  payload = event.get("payload") or {}
  if payload.get("type") != "token_count":
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
      "context_tokens": input_tokens,
      # The bar's full scale is the model's context window (the longest context the
      # prompt can reach); the compaction line comes from the backend option and is
      # merged in by the resolver.
      "context_full": model_context_window,
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
  if event.get("type") != "turn_context":
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

      for raw_line in reversed(lines):
        line = raw_line.strip()
        if not line:
          continue
        try:
          event = json.loads(line)
        except json.JSONDecodeError as e:
          log.debug("codex_rollout_parse_skip", path=str(path), error=str(e))
          continue
        if usage is None:
          usage = _extract_codex_rollout_usage_event(event)
        if model is None:
          model = _extract_codex_rollout_model_event(event)
        if usage is not None and model is not None:
          usage["model"] = model
          return usage

    if carry.strip():
      try:
        event = json.loads(carry)
      except json.JSONDecodeError as e:
        log.debug("codex_rollout_parse_skip", path=str(path), error=str(e))
      else:
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
  ):
    self._cfg = cfg
    self._events_cache = events_cache
    self._chat_events_path_fn = chat_events_path_fn
    self._codex_rollout_path_cache: dict[str, Path] = {}
    self._codex_rollout_usage_cache: dict[str, tuple[int, int, dict | None]] = {}

  def is_codex_backend(self, backend_id: str) -> bool:
    option_getter = getattr(self._cfg, "get_backend_option", None)
    if callable(option_getter):
      option = option_getter(backend_id)
      if option is not None:
        return option.type == "codex"
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
    native_thread_id = self._resolve_codex_thread_id(
        session_id, session_meta.cc_session_id, events)
    if not native_thread_id:
      return None

    native_usage = self._load_codex_rollout_usage(native_thread_id, backend_id)
    if native_usage is None:
      return None

    auto_compact_limit = self._backend_auto_compact_limit(backend_id)
    context_compact_at: int | None = auto_compact_limit
    model = native_usage.get("model") or ""
    total_token_usage = native_usage.get("total_token_usage")
    merged_usage: dict[str, Any] = {
        "context_tokens": native_usage["context_tokens"],
        "context_full": native_usage["context_full"],
        "context_compact_at": context_compact_at,
        "total_cost_usd": (
            calculate_codex_usage_cost_usd(model, total_token_usage) if total_token_usage else None),
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
      for raw_line in f:
        line = raw_line.strip()
        if not line:
          continue
        try:
          event = json.loads(line)
        except json.JSONDecodeError as e:
          log.debug("translated_session_id_parse_skip", path=str(path), error=str(e))
          continue
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

  def _codex_candidate_session_dirs(self, backend_id: str) -> list[Path]:
    """Ordered candidate ``<home>/sessions`` directories searched for rollout logs.

    Searched in order, stopping at the first directory that yields a match:

    1. the session's own backend option ``codex_home``, when that backend id is
       still in config;
    2. the union of ``codex_home`` across every configured backend of type codex;
    3. ``~/.codex``.

    Each candidate is searched as ``<home>/sessions``. Rollout file names embed a
    globally unique codex thread id, so searching several trees cannot mis-match.
    """
    dirs: list[Path] = []
    seen: set[Path] = set()

    def _add(home: Path) -> None:
      sessions_dir = home / "sessions"
      if sessions_dir not in seen:
        seen.add(sessions_dir)
        dirs.append(sessions_dir)

    own_option = self._cfg.get_backend_option(backend_id) if backend_id else None
    if own_option is not None and own_option.codex_home:
      _add(Path(own_option.codex_home).expanduser())

    for opt in self._cfg.backend_options:
      if opt.type == "codex" and opt.codex_home:
        _add(Path(opt.codex_home).expanduser())

    _add(_DEFAULT_CODEX_HOME)
    return dirs

  def _find_codex_rollout_path(self, native_thread_id: str, backend_id: str) -> Path | None:
    cached_path = self._codex_rollout_path_cache.get(native_thread_id)
    if cached_path is not None and cached_path.exists():
      return cached_path

    for candidate_dir in self._codex_candidate_session_dirs(backend_id):
      if not candidate_dir.exists():
        continue
      matches = list(candidate_dir.rglob(f"rollout-*{native_thread_id}.jsonl"))
      if not matches:
        continue
      matches.sort(key=lambda path: path.stat().st_mtime, reverse=True)
      rollout_path = matches[0]
      self._codex_rollout_path_cache[native_thread_id] = rollout_path
      return rollout_path
    return None

  def _load_codex_rollout_usage(self, native_thread_id: str, backend_id: str) -> dict | None:
    rollout_path = self._find_codex_rollout_path(native_thread_id, backend_id)
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
