"""OpenCodeBackend — AgentBackend wrapping the `opencode run --format json` CLI."""

import json
import os
from pathlib import Path

import structlog

from src.agents.backends.base import AgentBackend, make_error_event, make_result_event, make_text_event, resolve_binary
from src.core import event_types as ET

log = structlog.get_logger()


class OpenCodeBackend(AgentBackend):
  """Runs an `opencode run --format json` subprocess and translates NDJSON events to CC-compatible format."""

  def __init__(self, **kwargs):
    super().__init__(**kwargs)
    self._opencode_bin = resolve_binary("opencode", str(Path.home() / ".opencode" / "bin"))
    self._session_id_emitted = False

  def _prepare_cwd(self, cwd: str) -> None:
    """Write permissive project config so tool calls aren't auto-denied in non-interactive mode."""
    if self._instructions_content:
      agents_md = os.path.join(cwd, 'AGENTS.md')
      with open(agents_md, 'w', encoding='utf-8') as f:
        f.write(self._instructions_content)
      log.debug('opencode_wrote_agents_md', path=agents_md)

    config_dir = os.path.join(cwd, ".opencode")
    config_path = os.path.join(config_dir, "opencode.json")
    legacy_config_path = os.path.join(config_dir, "config.json")
    if os.path.exists(config_path):
      return
    os.makedirs(config_dir, exist_ok=True)
    if os.path.exists(legacy_config_path):
      with open(legacy_config_path, "r", encoding="utf-8") as f:
        config = json.load(f)
      config_source = "legacy_config_json"
    else:
      allow_all = {"*": "allow"}
      config = {
          "agent":
              {
                  "build":
                      {
                          "permission":
                              {
                                  "external_directory": allow_all,
                                  "read": allow_all,
                                  "edit": allow_all,
                                  "bash": allow_all,
                                  "glob": allow_all,
                                  "grep": allow_all,
                                  "list": allow_all,
                                  "write": allow_all,
                                  "skill": allow_all,
                              }
                      }
              }
      }
      config_source = "generated"
    with open(config_path, "w") as f:
      json.dump(config, f, indent=2)
    log.debug("opencode_config_written", path=config_path, source=config_source)

  def _build_command(self, prompt: str) -> list[str]:
    cmd = [self._opencode_bin, "run", "--format", "json"]
    if self._model:
      cmd.extend(["--model", self._model])
    if self._resume_session_id:
      cmd.extend(["--session", self._resume_session_id])
    cmd.extend(self._extra_flags)
    cmd.append(self._effective_prompt(prompt))

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
      return results + self._translate_text(ev)
    if ev_type in ("tool_use", "tool"):
      return results + self._translate_tool(ev)
    if ev_type == "error":
      return results + self._translate_error(ev)
    if ev_type == "step_finish":
      return results + self._translate_step_finish(ev)

    log.debug("opencode_event_unhandled", type=ev_type)
    return results

  def _translate_text(self, ev: dict) -> list[dict]:
    text = ev.get("part", {}).get("text", "")
    if text:
      return [make_text_event(text)]
    return []

  def _translate_tool(self, ev: dict) -> list[dict]:
    part = ev.get("part", {})
    call_id = part.get("callID", "")
    tool_name = part.get("tool", "")
    state = part.get("state", {})
    input_data = state.get("input", {})
    output = state.get("output", "")
    error = state.get("error", "")

    results: list[dict] = [
        {
            "type": ET.ASSISTANT,
            "message": {
                "content": [{
                    "type": ET.TOOL_USE,
                    "name": tool_name,
                    "id": call_id,
                    "input": input_data,
                }]
            },
        }
    ]
    if output:
      results.append({
          "type": ET.TOOL_RESULT,
          "tool_use_id": call_id,
          "content": output,
      })
    elif error:
      results.append({
          "type": ET.TOOL_RESULT,
          "tool_use_id": call_id,
          "content": error,
      })
    return results

  def _translate_error(self, ev: dict) -> list[dict]:
    msg = ev.get("part", {}).get("error", str(ev))
    return [make_error_event(msg)]

  def _translate_step_finish(self, ev: dict) -> list[dict]:
    part = ev.get("part", {})
    if part.get("reason", "") != "stop":
      return []
    tokens = part.get("tokens", {})
    cache = tokens.get("cache", {})
    return [
        make_result_event(
            input_tokens=tokens.get("input", 0),
            output_tokens=tokens.get("output", 0),
            cache_read=cache.get("read", 0),
            cache_creation=cache.get("write", 0),
            cost=part.get("cost", 0),
        )
    ]
