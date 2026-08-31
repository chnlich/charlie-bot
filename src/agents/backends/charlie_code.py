"""CharlieCodeBackend — AgentBackend wrapping the `charlie-code --json` CLI."""

from pathlib import Path

import structlog

from src.core import event_types as ET
from src.agents.backends.base import (
  AgentBackend,
  make_error_event,
  make_result_event,
  make_text_event,
  make_tool_result_event,
  make_tool_use_event,
  prepend_path_dir,
  resolve_binary,
)

log = structlog.get_logger()


class CharlieCodeBackend(AgentBackend):
  """Runs a `charlie-code --json` subprocess and translates NDJSON events to CC-compatible format."""

  def __init__(self, *, model: str, api_base: str | None = None, **kwargs):
    super().__init__(model=model, **kwargs)
    self._api_base = api_base
    if not self._api_base:
      raise ValueError("charlie-code backend requires api_base (set backend_options[].api_base in config.yaml)")
    self._bin = resolve_binary("charlie-code", str(Path.home() / ".local" / "bin"))

  def _prepare_env(self, env: dict) -> dict:
    charlie_code_env = {**env}
    prepend_path_dir(charlie_code_env, str(Path.home() / ".local" / "bin"))
    return charlie_code_env

  def _build_command(self, prompt: str) -> list[str]:
    cmd = [self._bin, "--json", "--model", self._model]
    if self._api_base:
      cmd += ["--api-base", self._api_base]
    if self._resume_session_id:
      cmd += ["--resume", self._resume_session_id]
    cmd += self._extra_flags
    cmd += ["--", self._effective_prompt(prompt)]
    return cmd

  def _effective_prompt(self, prompt: str) -> str:
    if self._resume_session_id:
      return prompt
    return super()._effective_prompt(prompt)

  def translate_event(self, event: dict) -> list[dict]:
    """Translate a single charlie-code NDJSON event into CC-compatible event(s)."""
    event_type = event.get("type")

    if event_type == "session":
      return [{"session_id": event["session_id"]}]

    if event_type == "thought":
      return [make_text_event(event["text"])]

    if event_type == "command":
      translated = make_tool_use_event("Bash", {"command": event["command"]})
      translated["id"] = event["id"]
      return [translated]

    if event_type == "observation":
      translated = make_tool_result_event("Bash", event.get("output", ""))
      translated["tool_use_id"] = event["id"]
      return [translated]

    if event_type == "result":
      usage = event.get("usage", {})
      return [
          make_result_event(
              input_tokens=usage.get("input_tokens", 0),
              output_tokens=usage.get("output_tokens", 0),
              cost=None,
          )
      ]

    if event_type == "error":
      return [make_error_event(event.get("message", ""))]

    if event_type == "compact":
      return [{
          "type": ET.SYSTEM,
          "subtype": ET.COMPACT_BOUNDARY,
          ET.COMPACT_METADATA: {
              "trigger": event["trigger"],
              "pre_tokens": event["pre_tokens"],
          },
      }]

    log.debug("charlie_code_unknown_event", event_type=event_type)
    return []
