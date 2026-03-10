"""OpenCodeBackend — AgentBackend wrapping the `opencode run --format json` CLI."""

import shutil
from pathlib import Path

import structlog

from src.agents.backends.base import AgentBackend

log = structlog.get_logger()


def _resolve_opencode_binary() -> str:
  """Resolve the opencode binary path, falling back to ~/.opencode/bin/opencode."""
  path = shutil.which("opencode")
  if path:
    return path
  fallback = Path.home() / ".opencode" / "bin" / "opencode"
  if fallback.exists():
    return str(fallback)
  raise FileNotFoundError("opencode binary not found on PATH or at ~/.opencode/bin/opencode")


class OpenCodeBackend(AgentBackend):
  """Runs an `opencode run --format json` subprocess and translates NDJSON events to CC-compatible format."""

  def __init__(self, **kwargs):
    model = kwargs.pop("model", None)
    instructions_content = kwargs.pop("instructions_content", None)
    resume_session_id = kwargs.pop("resume_session_id", None)
    extra_flags = kwargs.pop("extra_flags", None)
    super().__init__(
        model=model,
        instructions_content=instructions_content,
        resume_session_id=resume_session_id,
        extra_flags=extra_flags,
        **kwargs)
    self._opencode_bin = _resolve_opencode_binary()
    self._session_id_emitted = False

  def _build_command(self, prompt: str) -> list[str]:
    effective_prompt = prompt
    if self._instructions_content:
      effective_prompt = (f"<system-instructions>\n{self._instructions_content}\n</system-instructions>\n\n{prompt}")

    cmd = [self._opencode_bin, "run", "--format", "json"]
    if self._model:
      cmd.extend(["--model", self._model])
    if self._resume_session_id:
      cmd.extend(["--session", self._resume_session_id])
    cmd.extend(self._extra_flags)
    cmd.append(effective_prompt)

    self._session_id_emitted = False
    return cmd

  def _prepare_env(self, env: dict) -> dict:
    oc_env = {**env}
    opencode_bin_dir = str(Path.home() / ".opencode" / "bin")
    current_path = oc_env.get("PATH", "")
    if opencode_bin_dir not in current_path.split(":"):
      oc_env["PATH"] = f"{opencode_bin_dir}:{current_path}"
    return oc_env

  def translate_event(self, ev: dict) -> list[dict]:
    """Translate a single OpenCode NDJSON event into CC-compatible event(s)."""
    results: list[dict] = []

    # Emit session_id from the first event that carries one
    session_id = ev.get("sessionID")
    if session_id and not self._session_id_emitted:
      results.append({"session_id": session_id})
      self._session_id_emitted = True

    ev_type = ev.get("type", "")

    if ev_type == "step_start":
      return results

    if ev_type == "text":
      part = ev.get("part", {})
      text = part.get("text", "")
      if text:
        results.append({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": text
                }]
            },
        })
      return results

    if ev_type == "tool_use":
      part = ev.get("part", {})
      call_id = part.get("callID", "")
      tool_name = part.get("tool", "")
      state = part.get("state", {})
      input_data = state.get("input", {})
      output = state.get("output", "")

      # Emit tool_use event
      results.append({
          "type": "assistant",
          "message": {
              "content": [{
                  "type": "tool_use",
                  "name": tool_name,
                  "id": call_id,
                  "input": input_data,
              }]
          },
      })
      # Emit tool_result event only when output is present
      if output:
        results.append({
            "type": "tool_result",
            "tool_use_id": call_id,
            "content": output,
        })
      return results

    if ev_type == "error":
      msg = ev.get("part", {}).get("error", str(ev))
      results.append({"type": "error", "message": msg, "content": msg})
      return results

    if ev_type == "step_finish":
      part = ev.get("part", {})
      reason = part.get("reason", "")
      if reason == "stop":
        cost = part.get("cost", 0)
        tokens = part.get("tokens", {})
        cache = tokens.get("cache", {})
        results.append({
            "type": "result",
            "result": "",
            "usage": {
                "input_tokens": tokens.get("input", 0),
                "output_tokens": tokens.get("output", 0),
                "cache_read_input_tokens": cache.get("read", 0),
                "cache_creation_input_tokens": cache.get("write", 0),
            },
            "total_cost_usd": cost,
        })
      return results

    log.debug("opencode_event_unhandled", type=ev_type)
    return results
