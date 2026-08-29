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

from src.agents.backends.base import (
  AgentBackend,
  make_error_event,
  make_result_event,
  make_text_event,
  prepend_path_dir,
  resolve_binary,
)
from src.core import event_types as ET
from src.core.process import wait_or_kill_group
from src.core.sse import iter_sse_lines
from src.core.timeouts import (
    OPENCODE_ABORT_TIMEOUT,
    OPENCODE_HTTP_API_TIMEOUT,
    OPENCODE_SSE_PROGRESS_TIMEOUT,
    OPENCODE_STDOUT_DRAIN_TIMEOUT,
)

log = structlog.get_logger()

_SERVER_URL_RE = re.compile(r"opencode server listening on (http://127\.0\.0\.1:\d+)")
# Handled `opencode serve` SSE event types: each frame's "type" value, fixed by the
# opencode binary's event stream. Ignored types live in _IGNORED_SSE_EVENT_TYPES.
SSE_EVENT_SERVER_CONNECTED = "server.connected"
SSE_EVENT_PERMISSION_ASKED = "permission.asked"
SSE_EVENT_SESSION_ERROR = "session.error"
SSE_EVENT_SESSION_IDLE = "session.idle"
SSE_EVENT_MESSAGE_UPDATED = "message.updated"
SSE_EVENT_MESSAGE_PART_UPDATED = "message.part.updated"
_IGNORED_SSE_EVENT_TYPES = {
    "message.part.delta",
    "session.diff",
    "session.next.agent.switched",
    "session.next.model.switched",
    "session.status",
    "session.updated",
}
# opencode's own compaction output-reserve default ($d = 20000 in the opencode binary,
# applied as `compaction.reserved ?? min($d, maxOutputTokens)`; checkable via
# `grep -ao "compaction?\.reserved.\{0,140\}" <opencode binary>`).
OPENCODE_COMPACT_OUTPUT_RESERVE = 20_000
_LOCAL_NO_PROXY_ENTRIES = ("localhost", "127.0.0.1", "::1")


class OpenCodeSseSilenceError(RuntimeError):
  """The SSE stream carried no session-id-bearing event within OPENCODE_SSE_PROGRESS_TIMEOUT."""


class OpenCodeBackend(AgentBackend):
  """Runs an `opencode serve` subprocess and translates SSE events to CC-compatible format."""

  _SERVER_START_TIMEOUT = 30.0
  _SERVER_STOP_TIMEOUT = 5.0

  def __init__(self, *, opencode_proxy_url: str | None = None, **kwargs):
    super().__init__(**kwargs)
    self._opencode_bin = resolve_binary("opencode", str(Path.home() / ".opencode" / "bin"))
    self._opencode_proxy_url = opencode_proxy_url
    self._server_url: str | None = None
    self._session_id: str | None = None
    self._stdout_task: asyncio.Task | None = None
    self._message_roles: dict[str, str] = {}
    self._pending_parts: dict[str, list[dict]] = {}
    self._last_part_text: dict[str, str] = {}
    self._tool_use_emitted: set[str] = set()
    self._tool_result_emitted: set[str] = set()
    self._compaction_message_ids: set[str] = set()
    self._usage_input = 0
    self._usage_output = 0
    self._usage_cache_read = 0
    self._usage_cache_write = 0
    self._usage_cost = 0.0
    self._model_limit: dict | None = None
    self._last_step_tokens: dict | None = None
    self._failed = False

  def _prepare_cwd(self, cwd: str) -> None:
    """Write AGENTS.md into the cwd so opencode auto-detects it."""
    self._write_instructions_file(cwd, 'AGENTS.md', 'opencode_wrote_agents_md')

  def _build_command(self, prompt: str) -> list[str]:
    del prompt
    cmd = [self._opencode_bin, "serve", "--port", "0", "--print-logs"]
    cmd.extend(self._extra_flags)
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
    prepend_path_dir(oc_env, str(Path.home() / ".opencode" / "bin"))
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

  async def run(self, prompt: str, cwd: str, env: dict) -> AsyncIterator[dict]:
    """Drive OpenCode through its per-run HTTP server and SSE event stream."""
    self._reset_run_state()
    try:
      await asyncio.to_thread(self._prepare_cwd, cwd)
      cmd = self._build_command(prompt)
      final_env = self._prepare_env(env)
      stdout_log_path, stderr_log_path = self._log_paths()

      await self._spawn_piped_and_pin_identity(cmd, cwd, final_env)

      self._stderr_task = asyncio.create_task(self._stream_stderr(stderr_log_path))
      self._server_url = await self._read_server_url(stdout_log_path)
      self._stdout_task = asyncio.create_task(self._stream_stdout(stdout_log_path))

      async with httpx.AsyncClient(base_url=self._server_url, timeout=OPENCODE_HTTP_API_TIMEOUT) as client:
        await self._check_health(client)
        self._model_limit = await self._fetch_model_limit(client)
        self._session_id = self._resume_session_id or await self._create_session(client)
        yield {"session_id": self._session_id}

        async with client.stream("GET", "/event", timeout=None) as response:
          response.raise_for_status()
          sse_events = self._with_sse_progress_watchdog(self._iter_sse_events(response))
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

      if event_type == SSE_EVENT_PERMISSION_ASKED:
        self._failed = True
        yield make_error_event(self._format_permission_error(properties))
        return

      if properties.get("sessionID") != self._session_id:
        continue

      for translated in self._translate_sse_event(event):
        yield translated

      if event_type == SSE_EVENT_SESSION_ERROR:
        self._failed = True
        return

      if event_type == SSE_EVENT_SESSION_IDLE:
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
    self._compaction_message_ids.clear()
    self._usage_input = 0
    self._usage_output = 0
    self._usage_cache_read = 0
    self._usage_cache_write = 0
    self._usage_cost = 0.0
    self._model_limit = None
    self._last_step_tokens = None
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
      except TimeoutError as e:
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

  async def _fetch_model_limit(self, client: httpx.AsyncClient) -> dict | None:
    """Fetch this run's model limit dict from ``/config/providers`` once per turn.

    Splits ``self._model`` on the first ``/`` (same split ``_send_prompt`` does),
    finds the provider's model entry, and returns its ``limit`` dict
    (``{"context": int, "input": int | None, "output": int}``). Any failure —
    request error, provider or model absent, malformed payload — is contained:
    one ``log.warning`` is emitted and ``None`` is returned so the turn proceeds
    normally (the usage resolver then reports ``unknown`` for this session).
    """
    if not self._model:
      return None
    provider_id, model_id = self._model.split("/", 1)
    try:
      response = await client.get("/config/providers")
      response.raise_for_status()
      providers = response.json()
    except Exception as e:
      log.warning(
          "opencode_context_catalog_unavailable",
          provider_id=provider_id, model_id=model_id, error=str(e))
      return None
    limit = self._extract_model_limit(providers, provider_id, model_id)
    if limit is None:
      log.warning(
          "opencode_context_catalog_unavailable",
          provider_id=provider_id, model_id=model_id)
    return limit

  @staticmethod
  def _extract_model_limit(providers_payload: object, provider_id: str, model_id: str) -> dict | None:
    """Find ``provider_id``/``model_id`` in a ``/config/providers`` payload.

    opencode returns ``{"providers": [...]}`` where each provider's ``models`` is
    a dict keyed by model id; a bare top-level list and list-shaped ``models`` are
    also accepted for robustness. Returns the model's ``limit`` dict, or ``None``
    when the provider or model is absent or the payload shape is not recognised.
    """
    if isinstance(providers_payload, dict):
      providers = providers_payload.get("providers")
    else:
      providers = providers_payload
    if not isinstance(providers, list):
      return None
    for provider in providers:
      if not isinstance(provider, dict) or provider.get("id") != provider_id:
        continue
      models = provider.get("models")
      if isinstance(models, dict):
        model = models.get(model_id)
        if isinstance(model, dict):
          limit = model.get("limit")
          return limit if isinstance(limit, dict) else None
        return None
      if isinstance(models, list):
        for model in models:
          if isinstance(model, dict) and model.get("id") == model_id:
            limit = model.get("limit")
            return limit if isinstance(limit, dict) else None
        return None
      return None
    return None

  async def _create_session(self, client: httpx.AsyncClient) -> str:
    response = await client.post("/session", json={})
    response.raise_for_status()
    return response.json()["id"]

  async def _wait_for_server_connected(self, events: AsyncIterator[dict]) -> None:
    async for event in events:
      if event.get("type") == SSE_EVENT_SERVER_CONNECTED:
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
    async for line in iter_sse_lines(response):
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

  async def _with_sse_progress_watchdog(self, sse_events: AsyncIterator[dict]) -> AsyncIterator[dict]:
    """Fail the turn when no session progress arrives within OPENCODE_SSE_PROGRESS_TIMEOUT.

    Every upstream event is awaited under ``asyncio.wait_for`` with a monotonic
    deadline. An event carrying a session id (top-level ``properties.sessionID``
    or nested ``info.sessionID``; a subagent child's id counts) resets the
    deadline; server-level events (``server.heartbeat``, ``server.connected``)
    pass through without resetting it. Heartbeats since the last progress event
    are counted so the expiry error can distinguish "stream dead" (no events at
    all) from "session stalled" (heartbeats but no progress). This single
    wrapper sits upstream of both consumers, so coverage spans stream-open
    (including the ``server.connected`` wait) through turn end.
    """
    loop = asyncio.get_running_loop()
    last_progress_at = loop.time()
    deadline = last_progress_at + OPENCODE_SSE_PROGRESS_TIMEOUT
    silent_heartbeats = 0
    events_iter = aiter(sse_events)
    while True:
      try:
        event = await asyncio.wait_for(anext(events_iter), timeout=deadline - loop.time())
      except StopAsyncIteration:
        return
      except TimeoutError as e:
        silent_seconds = loop.time() - last_progress_at
        log.error(
            "opencode_sse_silence_timeout",
            silent_seconds=round(silent_seconds, 3),
            session_id=self._session_id,
            heartbeat_count=silent_heartbeats)
        raise OpenCodeSseSilenceError(
            f"OpenCode SSE stream carried no session progress for {silent_seconds:.1f} s "
            f"(heartbeats during silence: {silent_heartbeats})") from e
      if event.get("type") == "server.heartbeat":
        silent_heartbeats += 1
      if self._event_carries_session_id(event):
        last_progress_at = loop.time()
        deadline = last_progress_at + OPENCODE_SSE_PROGRESS_TIMEOUT
        silent_heartbeats = 0
      yield event

  @staticmethod
  def _event_carries_session_id(event: dict) -> bool:
    """Progress predicate: the event carries a session id, top-level or nested under ``info``."""
    properties = event.get("properties", {})
    return bool(properties.get("sessionID") or properties.get("info", {}).get("sessionID"))

  def _translate_sse_event(self, ev: dict) -> list[dict]:
    ev_type = ev.get("type", "")
    properties = ev.get("properties", {})

    if ev_type == SSE_EVENT_MESSAGE_UPDATED:
      info = properties["info"]
      self._message_roles[info["id"]] = info["role"]
      pending_parts = self._pending_parts.pop(info["id"], [])
      if info["role"] == "assistant" and bool(info.get("summary")):
        already_registered = info["id"] in self._compaction_message_ids
        self._compaction_message_ids.add(info["id"])
        if already_registered:
          return []
        pre_tokens = self._last_step_tokens["input"] if self._last_step_tokens is not None else None
        return [{
            "type": "system",
            "subtype": "compact_boundary",
            "compact_metadata": {"trigger": "auto", "pre_tokens": pre_tokens},
        }]
      if info["role"] != "assistant":
        return []
      translated: list[dict] = []
      for part in pending_parts:
        translated.extend(self._translate_part(part))
      return translated

    if ev_type == SSE_EVENT_MESSAGE_PART_UPDATED:
      part = properties["part"]
      if part["messageID"] in self._compaction_message_ids:
        if part.get("type") == "step-finish":
          self._accumulate_step_finish(part)
        return []
      role = self._message_roles.get(part["messageID"])
      if role is None:
        self._pending_parts.setdefault(part["messageID"], []).append(part)
        return []
      if role != "assistant":
        return []
      return self._translate_part(part)

    if ev_type == SSE_EVENT_SESSION_ERROR:
      return [make_error_event(self._format_session_error(ev))]

    if ev_type == SSE_EVENT_SESSION_IDLE:
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
    # Remember the *last* step's tokens (raw, not the turn's sum) so the result
    # event's context_snapshot reflects the live per-turn window. reasoning is
    # otherwise discarded from the streamed events, so capture it here.
    self._last_step_tokens = {
        "input": tokens["input"],
        "output": tokens["output"],
        "reasoning": tokens.get("reasoning", 0),
        "cache_read": cache["read"],
        "cache_write": cache["write"],
    }

  def _make_accumulated_result(self) -> dict:
    snapshot: dict | None = None
    if self._last_step_tokens is not None:
      snapshot = {
          "model": self._model or "",
          "tokens": dict(self._last_step_tokens),
          "limit": dict(self._model_limit) if self._model_limit is not None else None,
      }
    return make_result_event(
        input_tokens=self._usage_input,
        output_tokens=self._usage_output,
        cache_read=self._usage_cache_read,
        cache_creation=self._usage_cache_write,
        cost=self._usage_cost,
        context_snapshot=snapshot,
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
      async with httpx.AsyncClient(base_url=self._server_url, timeout=OPENCODE_ABORT_TIMEOUT) as client:
        response = await client.post(f"/session/{self._session_id}/abort")
        response.raise_for_status()
    except Exception as e:
      log.warning("opencode_abort_failed", session_id=self._session_id, error=str(e), exc_info=True)

  async def _finish_stdout_task(self) -> None:
    if self._stdout_task is None:
      return
    try:
      await asyncio.wait_for(asyncio.shield(self._stdout_task), timeout=OPENCODE_STDOUT_DRAIN_TIMEOUT)
    except TimeoutError:
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

  async def one_shot_text(self, prompt: str, system_prompt: str, *, timeout: float) -> str:
    """Generate text via `opencode run --format json` with all tools denied.

    Uses the one-shot ``run`` command (not the ``serve`` server the agent loop
    drives) and injects a deny-all permission policy so the naming/recap one-shot
    cannot call tools. opencode run has no system-prompt flag, so the system
    prompt is framed into the user prompt.

    ``opencode run --format json`` emits flat part-shaped NDJSON events (one per
    line) with the part dict at the top level — e.g. ``{"type": "text", "part":
    {"type": "text", "text": "..."}}`` — NOT the SSE-bus events that ``serve``
    uses (which nest the part under ``properties``). The parser therefore feeds
    the top-level ``part`` directly to ``_translate_part`` so ``_part_delta``'s
    cumulative logic applies to ``text``-type parts. The process group is killed
    on timeout.
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
        parts.extend(
            extract_text_from_message(translated.get("message"))
            for translated in self._translate_part(part)
            if translated.get("type") == ET.ASSISTANT)
      await proc.wait()
      return "".join(parts).strip()

    stderr_task = asyncio.create_task(proc.stderr.read())
    return await wait_or_kill_group(_collect(), timeout, proc.pid, stderr_task)
