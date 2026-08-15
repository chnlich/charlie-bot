"""OpenCodeBackend wrapping the `opencode run --format json` CLI (NDJSON stdout).

The backend spawns one `opencode run --format json` process per turn on the
shared ``AgentBackend.run()`` transport (raw log + cursor + pid pin), so an
interrupted turn is recovered by the same covered restart semantics as every
other covered backend. Turn continuity is the CLI's native ``--session <id>``
flag: the first turn spawns bare, later turns resume the session id captured
from the first raw event's top-level ``sessionID`` (mirrored into the event
stream as a synthesized ``{"session_id": ...}`` event for master_cc's anchor).

Run-json behavior pinned by the 2026-08-15 probe of opencode 1.18.18 (fixtures
in tests/fixtures/opencode_runjson_*.ndjson):

- Events are flat NDJSON lines: ``step_start`` / ``text`` / ``tool_use`` /
  ``step_finish`` with the part at top level, plus a terminal ``error`` event
  for CLI-level failures. Every event carries the top-level ``sessionID``.
- A successful turn's terminal event is the ``step_finish`` whose reason is
  ``stop``; mid-loop steps finish with ``tool-calls``. Usage accumulates over
  all of them and surfaces as ONE result event at the terminal stop (no
  ``context_snapshot`` — the accepted fork-2 usage-panel regression).
- An error terminal (e.g. an unknown model) is exactly one ``error`` event,
  no step_finish of any reason; the process exit code and stderr tail are
  surfaced by the shared base run machinery, never by this translator.
- A permission denial under an injected headless policy unregisters the tool;
  the model's call arrives as a completed ``tool="invalid"`` part explaining
  itself — a tool-level result, never a turn-level error, and the turn
  continues.
- Subagent (task tool) child sessions never interleave into the parent's
  stream: every event on it carries the parent ``sessionID``, so no
  sessionID filtering exists here.
"""

import asyncio
import contextlib
import json
import os
import signal
from pathlib import Path

import structlog

from src.agents.backends.base import (
  AgentBackend,
  make_error_event,
  make_result_event,
  make_text_event,
  resolve_binary,
)
from src.core import event_types as ET
from src.core.process import kill_process_group

log = structlog.get_logger()

# opencode's own compaction output-reserve default ($d = 20000 in the opencode binary,
# applied as `compaction.reserved ?? min($d, maxOutputTokens)`; checkable via
# `grep -ao "compaction?\.reserved.\{0,140\}" <opencode binary>`). Historical
# serve-era sessions carry context_snapshot result events that
# src/core/session_usage.py resolves against this constant; run-json turns
# carry none.
OPENCODE_COMPACT_OUTPUT_RESERVE = 20_000
_LOCAL_NO_PROXY_ENTRIES = ("localhost", "127.0.0.1", "::1")


class OpenCodeBackend(AgentBackend):
  """Runs an `opencode run --format json` subprocess and translates its NDJSON events."""

  def __init__(self, *, opencode_proxy_url: str | None = None, **kwargs):
    super().__init__(**kwargs)
    self._opencode_bin = resolve_binary("opencode", str(Path.home() / ".opencode" / "bin"))
    self._opencode_proxy_url = opencode_proxy_url
    self._reset_run_state()

  def _reset_run_state(self) -> None:
    """Per-turn translator state, reset at turn start (``_build_command``) and by one_shot_text."""
    self._last_part_text: dict[str, str] = {}
    self._tool_use_emitted: set[str] = set()
    self._tool_result_emitted: set[str] = set()
    self._session_id_emitted = False
    self._usage_input = 0
    self._usage_output = 0
    self._usage_cache_read = 0
    self._usage_cache_write = 0
    self._usage_cost = 0.0

  def _prepare_cwd(self, cwd: str) -> None:
    """Write AGENTS.md instruction file when provided."""
    if self._instructions_content:
      agents_md = os.path.join(cwd, 'AGENTS.md')
      with open(agents_md, 'w', encoding='utf-8') as f:
        f.write(self._instructions_content)
      log.debug('opencode_wrote_agents_md', path=agents_md)

  def _build_command(self, prompt: str) -> list[str]:
    """`opencode run --format json`; first turn bare, turn N+1 with ``--session <id>``."""
    self._reset_run_state()
    if not self._model:
      raise ValueError("opencode backend requires a model")
    cmd = [self._opencode_bin, "run", "--format", "json", "-m", self._model]
    if self._resume_session_id:
      cmd.extend(["--session", self._resume_session_id])
    cmd.extend(self._extra_flags)
    cmd.extend(["--", self._effective_prompt(prompt)])
    return cmd

  @staticmethod
  def _merge_local_no_proxy(value: str) -> str:
    entries: list[str] = []
    seen_local_entries: set[str] = set()
    for raw_entry in value.split(","):
      entry = raw_entry.strip()
      if not entry:
        continue
      if entry in _LOCAL_NO_PROXY_ENTRIES:
        if entry in seen_local_entries:
          continue
        seen_local_entries.add(entry)
        entries.append(entry)
        continue
      entries.append(raw_entry)
    entries.extend(entry for entry in _LOCAL_NO_PROXY_ENTRIES if entry not in seen_local_entries)
    return ",".join(entries)

  def _prepare_env(self, env: dict, *, opencode_config: dict | None = None) -> dict:
    oc_env = {**env}
    opencode_bin_dir = str(Path.home() / ".opencode" / "bin")
    current_path = oc_env.get("PATH", "")
    if opencode_bin_dir not in current_path.split(":"):
      oc_env["PATH"] = f"{opencode_bin_dir}:{current_path}"
    oc_env["OPENCODE_CONFIG_CONTENT"] = json.dumps(
        self._headless_config() if opencode_config is None else opencode_config)
    if self._opencode_proxy_url is not None:
      oc_env["HTTP_PROXY"] = self._opencode_proxy_url
      oc_env["HTTPS_PROXY"] = self._opencode_proxy_url
      oc_env["NO_PROXY"] = self._merge_local_no_proxy(oc_env.get("NO_PROXY", ""))
    return oc_env

  def _headless_config(self) -> dict:
    """Injected opencode policy for headless master runs.

    Headless runs have no human approver, so the injected policy keeps broad
    auto-approval for all tool calls, including OpenCode's internal subagents.
    This broad auto-approval is deliberate. The explicit ``question`` deny restores
    OpenCode's built-in default, which a bare top-level ``permission: "allow"``
    overrides; headless runs have no UI to answer interactive questions.
    """
    return {
        "permission": {"*": "allow", "question": "deny"},
        "default_agent": "charliebot",
        "agent": {
            "charliebot": {"mode": "primary"},
        },
    }

  def translate_event(self, ev: dict) -> list[dict]:
    """Translate one `opencode run --format json` NDJSON event into CC-compatible event(s).

    Routing is per event type (the probe-selected branch): a top-level
    ``error`` event maps directly to an error event; parts route by shape —
    ``step-finish`` accumulates usage and emits the turn's single result at
    reason ``stop``; parts carrying a ``callID`` are tool parts; the rest flow
    through the shared part translator. The first event carrying a
    ``sessionID`` additionally mirrors it as the one synthesized
    ``{"session_id": ...}`` event.
    """
    results: list[dict] = []
    session_id = ev.get("sessionID")
    if session_id and not self._session_id_emitted:
      self._session_id_emitted = True
      results.append({"session_id": session_id})

    ev_type = ev.get("type", "")
    if ev_type == "error":
      results.append(make_error_event(self._format_run_error(ev)))
      return results

    part = ev.get("part")
    if isinstance(part, dict):
      if part.get("type") == "step-finish":
        self._accumulate_step_finish(part)
        if part.get("reason") == "stop":
          results.append(self._make_turn_result())
      elif "callID" in part:
        results.extend(self._translate_tool_part(part))
      else:
        results.extend(self._translate_part(part))
    else:
      log.debug("opencode_run_event_unhandled", type=ev_type)
    return results

  def _format_run_error(self, ev: dict) -> str:
    error = ev.get("error")
    if isinstance(error, dict):
      data = error.get("data")
      if isinstance(data, dict) and data.get("message"):
        return str(data["message"])
      if error.get("message"):
        return str(error["message"])
      return json.dumps(error, default=str)
    if error:
      return str(error)
    return f"OpenCode error event with no error payload: {json.dumps(ev, default=str)}"

  def _translate_part(self, part: dict) -> list[dict]:
    part_type = part.get("type", "")
    if part_type == "text":
      delta = self._part_delta(part["id"], part["text"])
      return [make_text_event(delta)] if delta else []
    if part_type == "reasoning":
      delta = self._part_delta(part["id"], part["text"])
      return [{"type": ET.THINKING, "content": delta}] if delta else []
    if part_type == "tool":
      return self._translate_tool_part(part)
    if part_type in ("step-start", "step-finish"):
      return []
    log.debug("opencode_part_unhandled", type=part_type)
    return []

  def _part_delta(self, part_id: str, full_text: str) -> str:
    previous = self._last_part_text.get(part_id, "")
    if previous and not full_text.startswith(previous):
      raise RuntimeError(f"OpenCode part text is not cumulative: {part_id}")
    self._last_part_text[part_id] = full_text
    return full_text[len(previous):]

  def _translate_tool_part(self, part: dict) -> list[dict]:
    call_id = part["callID"]
    tool_name = part["tool"]
    state = part["state"]
    events: list[dict] = []

    if call_id not in self._tool_use_emitted and state.get("status") != "pending":
      events.append({
          "type": ET.ASSISTANT,
          "message": {
              "content": [{
                  "type": ET.TOOL_USE,
                  "name": tool_name,
                  "id": call_id,
                  "input": state["input"],
              }]
          },
      })
      self._tool_use_emitted.add(call_id)

    if call_id not in self._tool_result_emitted:
      if "output" in state:
        events.append({
            "type": ET.TOOL_RESULT,
            "tool_use_id": call_id,
            "content": state["output"],
        })
        self._tool_result_emitted.add(call_id)
      elif "error" in state:
        events.append({
            "type": ET.TOOL_RESULT,
            "tool_use_id": call_id,
            "content": state["error"],
        })
        self._tool_result_emitted.add(call_id)

    return events

  def _accumulate_step_finish(self, part: dict) -> None:
    tokens = part["tokens"]
    cache = tokens["cache"]
    self._usage_input += tokens["input"]
    self._usage_output += tokens["output"]
    self._usage_cache_read += cache["read"]
    self._usage_cache_write += cache["write"]
    self._usage_cost += part["cost"]

  def _make_turn_result(self) -> dict:
    """The turn's ONE result event: usage summed over every step-finish of the turn.

    Emitted at the terminal step-finish (the probe-observed reason ``stop``)
    so the restart drain can prove completion from the raw log alone. No
    ``context_snapshot`` — run-json carries no model-limit source.
    """
    return make_result_event(
        input_tokens=self._usage_input,
        output_tokens=self._usage_output,
        cache_read=self._usage_cache_read,
        cache_creation=self._usage_cache_write,
        cost=self._usage_cost,
    )

  async def one_shot_text(self, prompt: str, system_prompt: str, *, timeout: float) -> str:
    """Generate text via `opencode run --format json` with all tools denied.

    Injects a deny-all permission policy so the naming/recap one-shot cannot
    call tools. opencode run has no system-prompt flag, so the system prompt is
    framed into the user prompt. The part dict of each run-json NDJSON event
    feeds ``_translate_part`` directly so ``_part_delta``'s cumulative logic
    applies to ``text``-type parts. The process group is killed on timeout.
    """
    from src.core.message_aggregator import extract_text_from_message

    self._reset_run_state()
    framed = self._frame_system_prompt(system_prompt, prompt)
    cmd = [
        self._opencode_bin,
        "run",
        "--format",
        "json",
        "-m",
        self._model,
        "--dangerously-skip-permissions",
        "--",
        framed,
    ]
    env = self._prepare_env(dict(os.environ), opencode_config={"permission": {"*": "deny"}})

    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdin=asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env=env,
        limit=self._buffer_limit,
        start_new_session=True,
    )

    async def _collect() -> str:
      parts: list[str] = []
      assert proc.stdout is not None
      async for raw_line in proc.stdout:
        line = raw_line.decode("utf-8", errors="replace").strip()
        if not line:
          continue
        try:
          event = json.loads(line)
        except json.JSONDecodeError:
          continue
        part = event.get("part")
        if not isinstance(part, dict):
          continue
        for translated in self._translate_part(part):
          if translated.get("type") == ET.ASSISTANT:
            parts.append(extract_text_from_message(translated.get("message")))
      await proc.wait()
      return "".join(parts).strip()

    stderr_task = asyncio.create_task(proc.stderr.read())
    try:
      return await asyncio.wait_for(_collect(), timeout)
    except asyncio.TimeoutError:
      kill_process_group(proc.pid, signal.SIGKILL)
      raise
    finally:
      stderr_task.cancel()
      with contextlib.suppress(asyncio.CancelledError):
        await stderr_task
