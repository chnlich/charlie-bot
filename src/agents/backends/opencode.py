"""OpenCodeBackend wrapping the `opencode serve` HTTP/SSE API."""

import asyncio
import json
import os
import re
from collections.abc import AsyncIterator
from pathlib import Path

import aiofiles
import httpx
import structlog

from src.agents.backends.base import AgentBackend, make_error_event, make_result_event, make_text_event, resolve_binary
from src.core import event_types as ET

log = structlog.get_logger()

_SERVER_URL_RE = re.compile(r"opencode server listening on (http://127\.0\.0\.1:\d+)")
_IGNORED_SSE_EVENT_TYPES = {
    "message.part.delta",
    "session.diff",
    "session.next.agent.switched",
    "session.next.model.switched",
    "session.status",
    "session.updated",
}


class OpenCodeBackend(AgentBackend):
  """Runs an `opencode serve` subprocess and translates SSE events to CC-compatible format."""

  _SERVER_START_TIMEOUT = 30.0
  _SERVER_STOP_TIMEOUT = 5.0

  def __init__(self, **kwargs):
    super().__init__(**kwargs)
    self._opencode_bin = resolve_binary("opencode", str(Path.home() / ".opencode" / "bin"))
    self._server_url: str | None = None
    self._session_id: str | None = None
    self._stdout_task: asyncio.Task | None = None
    self._message_roles: dict[str, str] = {}
    self._pending_parts: dict[str, list[dict]] = {}
    self._last_part_text: dict[str, str] = {}
    self._tool_use_emitted: set[str] = set()
    self._tool_result_emitted: set[str] = set()
    self._usage_input = 0
    self._usage_output = 0
    self._usage_cache_read = 0
    self._usage_cache_write = 0
    self._usage_cost = 0.0
    self._failed = False

  def _prepare_cwd(self, cwd: str) -> None:
    """Write AGENTS.md instruction file when provided."""
    if self._instructions_content:
      agents_md = os.path.join(cwd, 'AGENTS.md')
      with open(agents_md, 'w', encoding='utf-8') as f:
        f.write(self._instructions_content)
      log.debug('opencode_wrote_agents_md', path=agents_md)

  def _build_command(self, prompt: str) -> list[str]:
    del prompt
    cmd = [self._opencode_bin, "serve", "--port", "0", "--print-logs"]
    cmd.extend(self._extra_flags)
    return cmd

  def _prepare_env(self, env: dict) -> dict:
    oc_env = {**env}
    opencode_bin_dir = str(Path.home() / ".opencode" / "bin")
    current_path = oc_env.get("PATH", "")
    if opencode_bin_dir not in current_path.split(":"):
      oc_env["PATH"] = f"{opencode_bin_dir}:{current_path}"
    oc_env["OPENCODE_CONFIG_CONTENT"] = json.dumps(self._headless_config())
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

  async def run(self, prompt: str, cwd: str, env: dict) -> AsyncIterator[dict]:
    """Drive OpenCode through its per-run HTTP server and SSE event stream."""
    self._reset_run_state()
    try:
      await asyncio.to_thread(self._prepare_cwd, cwd)
      cmd = self._build_command(prompt)
      final_env = self._prepare_env(env)
      stdout_log_path, stderr_log_path = self._log_paths()

      self._proc = await asyncio.create_subprocess_exec(
          *cmd,
          cwd=cwd,
          stdin=asyncio.subprocess.DEVNULL,
          stdout=asyncio.subprocess.PIPE,
          stderr=asyncio.subprocess.PIPE,
          env=final_env,
          limit=self._buffer_limit,
          start_new_session=True,
      )
      if self._on_spawn is not None:
        await self._on_spawn(self._proc.pid)

      self._stderr_task = asyncio.create_task(self._stream_stderr(stderr_log_path))
      self._server_url = await self._read_server_url(stdout_log_path)
      self._stdout_task = asyncio.create_task(self._stream_stdout(stdout_log_path))

      async with httpx.AsyncClient(base_url=self._server_url, timeout=30.0) as client:
        await self._check_health(client)
        self._session_id = self._resume_session_id or await self._create_session(client)
        yield {"session_id": self._session_id}

        async with client.stream("GET", "/event", timeout=None) as response:
          response.raise_for_status()
          sse_events = self._iter_sse_events(response)
          await self._wait_for_server_connected(sse_events)
          await self._send_prompt(client, self._session_id, prompt)

          async for translated in self._consume_sse_events(sse_events):
            yield translated
    except Exception as e:
      if self._is_cancellation_disconnect(e):
        # terminate() killed the server, which closes the /event stream mid-read. That
        # transport error is cancellation cleanup, not the root failure — do not report it.
        log.info("opencode_sse_closed_after_terminate", error=str(e))
      else:
        self._failed = True
        log.exception("opencode_backend_failed", error=str(e))
        detail = str(e) or e.__class__.__name__
        yield make_error_event(f"OpenCode backend failed: {detail}")
    finally:
      await self._cleanup_server()

  async def _consume_sse_events(self, sse_events: AsyncIterator[dict]) -> AsyncIterator[dict]:
    """Translate parent-session SSE events until the turn ends.

    A ``permission.asked`` event is a terminal backend error: headless runs have no human
    to approve it, and this opencode server is dedicated to a single run, so an ask from
    ANY session (parent or a subagent child) means the run cannot proceed. It is detected
    before the parent-session filter so child-session asks — which carry a different
    ``sessionID`` — are not silently dropped.
    """
    async for event in sse_events:
      properties = event.get("properties", {})
      event_type = event.get("type")

      if event_type == "permission.asked":
        self._failed = True
        yield make_error_event(self._format_permission_error(properties))
        return

      if properties.get("sessionID") != self._session_id:
        continue

      for translated in self._translate_sse_event(event):
        yield translated

      if event_type == "session.error":
        self._failed = True
        return

      if event_type == "session.idle":
        yield self._make_accumulated_result()
        return
    raise RuntimeError("OpenCode SSE stream closed before session.idle")

  def _is_cancellation_disconnect(self, error: Exception) -> bool:
    """True when an SSE stream error is the expected fallout of a deliberate terminate().

    A genuine unexpected disconnect (terminate() not called) stays a visible backend error.
    """
    return self.terminated and isinstance(error, httpx.RemoteProtocolError)

  def _format_permission_error(self, properties: dict) -> str:
    return (
        f"OpenCode requested interactive permission '{properties.get('permission')}' "
        f"(patterns={properties.get('patterns')}, session={properties.get('sessionID')}) "
        "but CharlieBot runs headless with no approval path; aborting run.")

  def _reset_run_state(self) -> None:
    self._server_url = None
    self._session_id = None
    self._stdout_task = None
    self._message_roles.clear()
    self._pending_parts.clear()
    self._last_part_text.clear()
    self._tool_use_emitted.clear()
    self._tool_result_emitted.clear()
    self._usage_input = 0
    self._usage_output = 0
    self._usage_cache_read = 0
    self._usage_cache_write = 0
    self._usage_cost = 0.0
    self._failed = False

  def _log_paths(self) -> tuple[Path | None, Path | None]:
    if self._log_dir is None:
      return None, None
    self._log_dir.mkdir(parents=True, exist_ok=True)
    return self._log_dir / "stdout.log", self._log_dir / "stderr.log"

  async def _read_server_url(self, stdout_log_path: Path | None) -> str:
    assert self._proc is not None and self._proc.stdout is not None
    loop = asyncio.get_running_loop()
    deadline = loop.time() + self._SERVER_START_TIMEOUT
    while True:
      remaining = deadline - loop.time()
      if remaining <= 0:
        raise TimeoutError("OpenCode serve did not print its server URL")
      try:
        raw_line = await asyncio.wait_for(self._proc.stdout.readline(), timeout=remaining)
      except asyncio.TimeoutError as e:
        raise TimeoutError("OpenCode serve did not print its server URL") from e
      if not raw_line:
        raise RuntimeError("OpenCode serve exited before printing its server URL")
      await self._append_stdout(stdout_log_path, raw_line)
      line = raw_line.decode("utf-8", errors="replace").strip()
      match = _SERVER_URL_RE.search(line)
      if match:
        return match.group(1)

  async def _append_stdout(self, stdout_log_path: Path | None, data: bytes) -> None:
    if stdout_log_path is None:
      return
    async with aiofiles.open(stdout_log_path, "ab") as stdout_log:
      await stdout_log.write(data)
      await stdout_log.flush()

  async def _stream_stdout(self, stdout_log_path: Path | None) -> None:
    assert self._proc is not None and self._proc.stdout is not None
    if stdout_log_path is None:
      while await self._proc.stdout.read(8192):
        pass
      return
    async with aiofiles.open(stdout_log_path, "ab") as stdout_log:
      while True:
        chunk = await self._proc.stdout.read(8192)
        if not chunk:
          break
        await stdout_log.write(chunk)
        await stdout_log.flush()

  async def _check_health(self, client: httpx.AsyncClient) -> None:
    response = await client.get("/global/health")
    response.raise_for_status()

  async def _create_session(self, client: httpx.AsyncClient) -> str:
    response = await client.post("/session", json={})
    response.raise_for_status()
    return response.json()["id"]

  async def _wait_for_server_connected(self, events: AsyncIterator[dict]) -> None:
    async for event in events:
      if event.get("type") == "server.connected":
        return
    raise RuntimeError("OpenCode SSE stream closed before server.connected")

  async def _send_prompt(self, client: httpx.AsyncClient, session_id: str, prompt: str) -> None:
    if not self._model:
      raise ValueError("opencode backend requires a model")
    provider_id, model_id = self._model.split("/", 1)
    response = await client.post(
        f"/session/{session_id}/prompt_async",
        json={
            "model": {
                "providerID": provider_id,
                "modelID": model_id,
            },
            "parts": [{
                "type": "text",
                "text": prompt,
            }],
        },
    )
    if response.status_code != 204:
      raise RuntimeError(f"OpenCode prompt_async returned HTTP {response.status_code}: {response.text}")

  async def _iter_sse_events(self, response: httpx.Response) -> AsyncIterator[dict]:
    data_lines: list[str] = []
    async for line in response.aiter_lines():
      if line == "":
        if not data_lines:
          continue
        payload = "\n".join(data_lines)
        data_lines = []
        yield json.loads(payload)
        continue
      if line.startswith("data:"):
        data_lines.append(line[len("data:"):].lstrip())
        continue
      if line.startswith(":"):
        continue
      if ":" in line:
        log.debug("opencode_sse_field_ignored", field=line.split(":", 1)[0])
        continue
      log.debug("opencode_sse_line_ignored", line=line)
    if data_lines:
      yield json.loads("\n".join(data_lines))

  def _translate_sse_event(self, ev: dict) -> list[dict]:
    ev_type = ev.get("type", "")
    properties = ev.get("properties", {})

    if ev_type == "message.updated":
      info = properties["info"]
      self._message_roles[info["id"]] = info["role"]
      pending_parts = self._pending_parts.pop(info["id"], [])
      if info["role"] != "assistant":
        return []
      translated: list[dict] = []
      for part in pending_parts:
        translated.extend(self._translate_part(part))
      return translated

    if ev_type == "message.part.updated":
      part = properties["part"]
      role = self._message_roles.get(part["messageID"])
      if role is None:
        self._pending_parts.setdefault(part["messageID"], []).append(part)
        return []
      if role != "assistant":
        return []
      return self._translate_part(part)

    if ev_type == "session.error":
      return [make_error_event(self._format_session_error(ev))]

    if ev_type == "session.idle":
      if self._pending_parts:
        log.warning(
            "opencode_pending_parts_discarded",
            message_ids=list(self._pending_parts),
            count=sum(len(parts) for parts in self._pending_parts.values()),
        )
        self._pending_parts.clear()
      return []

    if ev_type in _IGNORED_SSE_EVENT_TYPES:
      return []

    log.debug("opencode_sse_event_unhandled", type=ev_type)
    return []

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
    if part_type == "step-finish":
      self._accumulate_step_finish(part)
      return []
    if part_type == "step-start":
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

  def _make_accumulated_result(self) -> dict:
    return make_result_event(
        input_tokens=self._usage_input,
        output_tokens=self._usage_output,
        cache_read=self._usage_cache_read,
        cache_creation=self._usage_cache_write,
        cost=self._usage_cost,
    )

  def _format_session_error(self, ev: dict) -> str:
    error = ev.get("properties", {}).get("error")
    if isinstance(error, dict):
      data = error.get("data")
      if isinstance(data, dict) and data.get("message"):
        return data["message"]
      if error.get("message"):
        return error["message"]
      return json.dumps(error, default=str)
    if error:
      return str(error)
    return f"OpenCode session.error with no error payload: {json.dumps(ev, default=str)}"

  async def _abort_session(self) -> None:
    if self._server_url is None or self._session_id is None:
      return
    try:
      async with httpx.AsyncClient(base_url=self._server_url, timeout=5.0) as client:
        response = await client.post(f"/session/{self._session_id}/abort")
        response.raise_for_status()
    except Exception as e:
      log.warning("opencode_abort_failed", session_id=self._session_id, error=str(e), exc_info=True)

  async def _finish_stdout_task(self) -> None:
    if self._stdout_task is None:
      return
    try:
      await asyncio.wait_for(asyncio.shield(self._stdout_task), timeout=5.0)
    except asyncio.TimeoutError:
      self._stdout_task.cancel()
      try:
        await self._stdout_task
      except asyncio.CancelledError:
        log.debug("opencode_stdout_stream_cancelled")
      except Exception as e:
        log.warning("opencode_stdout_stream_cancel_failed", error=str(e), exc_info=True)
    except Exception as e:
      log.warning("opencode_stdout_stream_failed", error=str(e), exc_info=True)

  async def _cleanup_server(self) -> None:
    try:
      await self._abort_session()
      if self._proc is not None and self._proc.returncode is None:
        await self._graceful_shutdown(self._SERVER_STOP_TIMEOUT, timeout_log_event="opencode_server_stop_timeout")
      await self._finish_stdout_task()
      if self._proc is not None:
        await self._drain_and_cleanup(self._CLEANUP_TIMEOUT)
        if self._failed and self.exit_code == 0:
          self.exit_code = 1
        elif not self._failed:
          self.exit_code = 0
    except Exception as e:
      log.warning("opencode_cleanup_failed", error=str(e), exc_info=True)
      if self._failed and self.exit_code == 0:
        self.exit_code = 1

  async def terminate(self) -> None:
    await self._abort_session()
    await super().terminate()

  def translate_event(self, ev: dict) -> list[dict]:
    """Translate legacy part-shaped events for direct unit tests."""
    results: list[dict] = []

    session_id = ev.get("sessionID")
    if session_id:
      results.append({"session_id": session_id})

    ev_type = ev.get("type", "")

    if ev_type == "step_start":
      return results
    if ev_type == "text":
      return results + self._translate_text(ev)
    if ev_type in ("tool_use", "tool"):
      return results + self._translate_tool_part(ev.get("part", {}))
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
    return self._translate_tool_part(ev.get("part", {}))

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
