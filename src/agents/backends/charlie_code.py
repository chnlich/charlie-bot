"""CharlieCodeBackend — AgentBackend wrapping the `charlie-code --json` CLI.

The task text reaches the child through a `task.md` file in the run's
transport directory, passed via `--task-file`; it never rides argv.
"""

from pathlib import Path

import structlog

from src.agents.backends.base import (
  USER_LOCAL_BIN,
  AgentBackend,
  make_error_event,
  make_result_event,
  make_text_event,
  make_tool_result_event,
  make_tool_use_event,
  prepend_path_dir,
  resolve_binary,
)
from src.core import event_types as ET

log = structlog.get_logger()


class CharlieCodeBackend(AgentBackend):
  """Runs a `charlie-code --json` subprocess and translates NDJSON events to CC-compatible format.

  The task text is written to ``task.md`` in the run's transport directory
  during ``_prepare_transport()`` and handed to the CLI via ``--task-file``.
  """

  def __init__(
      self,
      *,
      model: str,
      api_base: str | None = None,
      context_window: int | None = None,
      api_key: str | None = None,
      **kwargs,
  ):
    super().__init__(model=model, **kwargs)
    self._api_base = api_base
    if not self._api_base:
      raise ValueError("charlie-code backend requires api_base (set backend_options[].api_base in config.yaml)")
    self._context_window = context_window
    self._api_key = api_key
    self._bin = resolve_binary("charlie-code", USER_LOCAL_BIN)
    self._transport_dir: Path | None = None

  def _prepare_transport(self, log_dir: Path) -> None:
    """Record the transport dir the task file will be written into."""
    self._transport_dir = log_dir

  def _prepare_env(self, env: dict) -> dict:
    charlie_code_env = {**env}
    prepend_path_dir(charlie_code_env, USER_LOCAL_BIN)
    if self._api_key is not None:
      charlie_code_env["CHARLIE_CODE_API_KEY"] = self._api_key
    return charlie_code_env

  def _prepare_cwd(self, cwd: str) -> None:
    """Write AGENTS.md into the cwd so charlie-code appends it to its system message."""
    self._write_instructions_file(cwd, "AGENTS.md", "charlie_code_wrote_agents_md")

  def _build_command(self, prompt: str) -> list[str]:
    if self._transport_dir is None:
      raise RuntimeError("charlie-code backend: _prepare_transport must run before _build_command")
    task_path = self._transport_dir / "task.md"
    task_path.write_text(self._effective_prompt(prompt), encoding="utf-8")
    cmd = [self._bin, "--json", "--model", self._model, "--api-base", self._api_base]
    if self._context_window is not None:
      cmd += ["--context-window", str(self._context_window)]
    if self._resume_session_id:
      cmd += ["--resume", self._resume_session_id]
    cmd += self._extra_flags
    cmd += ["--task-file", str(task_path)]
    return cmd

  def _effective_prompt(self, prompt: str) -> str:
    # Identity override: base's _effective_prompt re-adds the <system-instructions>
    # frame whenever instructions_content is set, so deleting this override would
    # re-enable it; instructions ride the cwd AGENTS.md system channel instead
    # (see _prepare_cwd) and task.md must carry the bare prompt.
    return prompt

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
