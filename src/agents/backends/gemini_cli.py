"""GeminiCliBackend — AgentBackend wrapping the `gemini` CLI in stream-json mode."""

from pathlib import Path

import structlog

from src.agents.backends.base import (
    AgentBackend, make_error_event, make_result_event, make_text_event, make_tool_result_event, make_tool_use_event,
    resolve_binary)

log = structlog.get_logger()


class GeminiCliBackend(AgentBackend):
  """Runs a `gemini` CLI subprocess in stream-json mode and translates NDJSON events to CC-compatible format."""

  def __init__(self, **kwargs):
    model = kwargs.pop("model", "gemini-3-pro-preview")
    instructions_content = kwargs.pop("instructions_content", None)
    resume_session_id = kwargs.pop("resume_session_id", None)
    extra_flags = kwargs.pop("extra_flags", None)
    super().__init__(
        model=model,
        instructions_content=instructions_content,
        resume_session_id=resume_session_id,
        extra_flags=extra_flags,
        **kwargs)
    self._gemini_bin = resolve_binary("gemini", str(Path.home() / ".local" / "bin"))
    self._text_buffer = ""

  def _build_command(self, prompt: str) -> list[str]:
    cmd = [
        self._gemini_bin, "-m", self._model, "-p",
        self._effective_prompt(prompt), "-o", "stream-json", "-y", "--sandbox=false"
    ]
    if self._resume_session_id:
      cmd.extend(["--resume", self._resume_session_id])
    cmd.extend(self._extra_flags)
    return cmd

  def _prepare_env(self, env: dict) -> dict:
    """Strip API keys so the CLI uses OAuth auth instead of key-based billing."""
    gemini_env = {**env}
    gemini_env.pop('GEMINI_API_KEY', None)
    gemini_env.pop('GOOGLE_API_KEY', None)
    return gemini_env

  def translate_event(self, ev: dict) -> list[dict]:
    """Translate a single Gemini stream-json NDJSON event into CC-compatible event(s)."""
    ev_type = ev.get("type", "")

    def flush_buffer():
      if self._text_buffer:
        msg = [make_text_event(self._text_buffer)]
        self._text_buffer = ""
        return msg
      return []

    # --- init ---
    if ev_type == "init":
      return [{"session_id": ev.get("session_id", "")}]

    # --- message ---
    if ev_type == "message":
      role = ev.get("role", "")
      if role == "user":
        return []
      if role == "assistant":
        self._text_buffer += ev.get("content", "")
        if not ev.get("delta", False):
          return flush_buffer()
        return []

    # --- tool_use ---
    if ev_type == "tool_use":
      events = flush_buffer()
      events.append(make_tool_use_event(ev.get("tool_name", ""), ev.get("parameters", {})))
      return events

    # --- tool_result ---
    if ev_type == "tool_result":
      events = flush_buffer()
      status = ev.get("status", "")
      if status == "success":
        events.append(make_tool_result_event(ev.get("tool_id", ""), ev.get("output", "")))
      elif status == "error":
        error = ev.get("error", {})
        msg = error.get("message", str(error)) if isinstance(error, dict) else str(error)
        events.append(make_tool_result_event(ev.get("tool_id", ""), msg))
      return events

    # --- error ---
    if ev_type == "error":
      events = flush_buffer()
      msg = ev.get("message", "")
      events.append(make_error_event(msg))
      return events

    # --- result ---
    if ev_type == "result":
      events = flush_buffer()
      stats = ev.get("stats", {})
      events.append(
          make_result_event(
              input_tokens=stats.get("input_tokens", 0),
              output_tokens=stats.get("output_tokens", 0),
              cache_read=stats.get("cached", 0),
          ))
      return events

    log.debug("gemini_event_unhandled", type=ev_type)
    return flush_buffer()
