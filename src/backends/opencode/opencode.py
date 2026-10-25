"""OpenCodeBackend: one turn on the shared opencode 2.0 server, and the standalone one-shot generation."""

from __future__ import annotations

import asyncio
import base64
import contextlib
import json
import os
import pathlib
import shutil
import tempfile
from collections.abc import AsyncIterator, Awaitable
from typing import TypeVar

import httpx

from src.backends.opencode import opencode_limits, options, server
from src.infra import event_types as ET
from src.infra import home, log_once, process
from src.runtime.agent_process import base

log = log_once.LazyStructlogLogger()

T = TypeVar("T")

# Session events a turn does not translate. Any other type unknown to this module is logged once.
_IGNORED_SESSION_EVENT_TYPES = {
    "permission.replied",
    "session.agent.selected",
    "session.created",
    "session.deleted",
    "session.execution.started",
    "session.forked",
    "session.inbox.cancelled",
    "session.inbox.delivered",
    "session.inbox.delivery.changed",
    "session.inbox.enqueued",
    "session.instructions.updated",
    "session.message.content.updated",
    "session.metadata.updated",
    "session.model.selected",
    "session.permissions",
    "session.renamed",
    "session.retry.scheduled",
    "session.reasoning.started",
    "session.shell.ended",
    "session.shell.started",
    "session.skill.activated",
    "session.step.failed",
    "session.step.streamed",
    "session.synthetic",
    "session.text.started",
    "session.tool.input.delta",
    "session.tool.input.ended",
    "session.tool.progress",
    "session.usage.recorded",
    "session.usage.updated",
    "session.viewed",
    "session.compaction.delta",
}
# Both registries sit on the translator's unhandled paths only, so a key that
# grows a branch or an ignore entry stops reaching them at all; one sighting
# carries the whole signal because the set of mapped keys is code, fixed for
# the process.
_UNHANDLED_SESSION_EVENT_TYPES = log_once.WarnOnceRegistry()

# The turn process: it only waits for a signal. Absolute path, because the spawn execs without a PATH search.
TURN_PROCESS_CMD = ("/usr/bin/sleep", "infinity")


class OpenCodeSseSilenceError(RuntimeError):
  """The turn's session produced no event within opencode_limits.OPENCODE_SSE_PROGRESS_TIMEOUT."""


class _TurnProcessExitedError(Exception):
  """The turn's own process ended (a chat cancel signals it), so the turn ends as cancelled."""


def _image_file_attachments(uploaded_files: list[dict] | None) -> list[dict]:
  """One 2.0 file attachment per readable image, in reference order.

  The mime comes from the filename extension via ``base.IMAGE_MIME_BY_EXT``; the attachment's
  uri is the file's bytes as a ``data:`` URL. A non-image reference is skipped silently and keeps
  riding the task text's [Attached files] path list, and a missing or unreadable image file is
  skipped with one ``log.warning`` — a broken attachment never fails the turn.
  """
  attachments: list[dict] = []
  for ref in uploaded_files or []:
    filename = str(ref.get("filename", ""))
    mime = base.IMAGE_MIME_BY_EXT.get(filename.rsplit(".", 1)[-1].lower())
    if mime is None:
      continue
    path = str(ref.get("path", ""))
    try:
      data = pathlib.Path(path).read_bytes()
    except OSError as e:
      log.warning("opencode_attachment_unreadable", path=path, filename=filename, error=str(e))
      continue
    attachments.append({"uri": f"data:{mime};base64,{base64.b64encode(data).decode('ascii')}", "name": filename})
  return attachments


def _error_text(error: object) -> str:
  if isinstance(error, dict):
    message = error.get("message") or (error.get("data") or {}).get("message")
    return str(message) if message else json.dumps(error, default=str)
  return str(error)


class OpenCodeBackend(base.AgentBackend):
  """Runs one turn on the shared opencode server and translates its session events to CharlieBot's."""

  # A manager turn's session cwd holds this AGENTS.md, which opencode auto-detects.
  _INSTRUCTIONS_TARGET: tuple[str, str] = ("AGENTS.md", "opencode_wrote_agents_md")

  def __init__(
      self,
      *,
      proxy_url: str | None = None,
      backend_option: options.OpencodeBackend | None = None,
      **kwargs: object) -> None:
    super().__init__(**kwargs)
    self._proxy_url = proxy_url
    self._backend_option = backend_option
    self._reset_run_state()

  def _build_command(self, prompt: str) -> list[str]:
    # The turn's own process: the Run's pinned identity, not the opencode server.
    del prompt
    return list(TURN_PROCESS_CMD)

  def _effective_prompt(self, prompt: str) -> str:
    # opencode reads the instructions from the cwd's AGENTS.md, which a worker or review run
    # never writes, so that run frames them into the prompt. A manager turn's prompt goes out bare.
    if self._instructions_file is None:
      return prompt
    return super()._effective_prompt(prompt)

  def _reset_run_state(self) -> None:
    # __init__ delegates here, so every per-run field is assigned fresh;
    # in-place .clear() would hit missing attributes on a fresh instance.
    self._session_id: str | None = None
    self._turn: server.Turn | None = None
    # The shared server and the directory this turn claims until its turn-end steps drop the claim.
    self._claim: tuple[server.OpenCodeServer, str] | None = None
    self._turn_end_steps_called = False
    self._exit_task: asyncio.Task | None = None
    self._streamed_parts: set[tuple[str, int]] = set()
    self._tool_names: dict[str, str] = {}
    self._usage_input = 0
    self._usage_output = 0
    self._usage_cache_read = 0
    self._usage_cache_write = 0
    self._usage_cost = 0.0
    self._model_limit: dict | None = None
    self._last_step_tokens: dict | None = None
    # Epoch milliseconds at which the turn's first model request started (session.step.started).
    self.model_request_started_ms: int | None = None
    self._failed = False
    self._succeeded = False

  async def run(self,
                prompt: str,
                cwd: str,
                env: dict,
                uploaded_files: list[dict] | None = None) -> AsyncIterator[dict]:
    """Run one turn: its own process, a session on the shared server, the prompt, then the events.

    The turn process is the Run's pinned identity: a chat cancel signals it, and this method ends the
    turn as cancelled. The turn-end steps run on every exit path.
    """
    self._reset_run_state()
    turn_end_error: Exception | None = None
    try:
      await asyncio.to_thread(self._prepare_cwd, cwd)
      await self._spawn_turn_process(cwd, env)
      shared = server.server_for(self._proxy_url)
      await shared.claim(cwd)
      self._claim = (shared, cwd)
      await self._until_exit(shared.ensure_running())
      self._session_id = await self._until_exit(self._open_session(shared, cwd))
      self._turn = shared.register(self._session_id, cwd)
      yield {"type": ET.SESSION_ATTACHED, "session_id": self._session_id}
      self._model_limit = await self._until_exit(self._fetch_model_limit(shared, cwd))
      await self._until_exit(shared.wait_interrupts_settled(self._session_id))
      self._turn.drop_earlier_interrupts()
      await self._until_exit(self._put_environment(shared, env))
      if self._instructions_file is None and self._instructions_content:
        # A manager turn's _prepare_cwd rewrote the session's AGENTS.md only moments ago, and
        # the server's instruction watcher can miss such a fresh write when the prompt follows
        # at once (measured on 2.0.32: the model request carries the old instructions). Reload
        # the locations so the reread has settled before the prompt.
        await self._until_exit(self._reload_locations(shared))
      await self._until_exit(
          self._send_prompt(shared, self._session_id, self._effective_prompt(prompt), uploaded_files))
      async for translated in self._consume_turn(self._turn):
        yield translated
    except _TurnProcessExitedError:
      log.info("opencode_turn_cancelled", session_id=self._session_id)
    except server.ServerLostError as e:
      self._failed = True
      yield base.make_error_event(f"OpenCode backend failed: {e}")
    except Exception as e:
      self._failed = True
      log.exception("opencode_backend_failed", error=str(e))
      detail = str(e) or e.__class__.__name__
      yield base.make_error_event(f"OpenCode backend failed: {detail}")
    finally:
      try:
        await self._turn_end_steps()
      except Exception as e:
        self._failed = True
        log.exception("opencode_turn_end_failed", error=str(e))
        turn_end_error = e
      finally:
        await self._end_turn_process()
    if turn_end_error is not None:
      detail = str(turn_end_error) or turn_end_error.__class__.__name__
      yield base.make_error_event(f"OpenCode backend failed: {detail}")

  async def terminate(self) -> None:
    try:
      await self._turn_end_steps()
    finally:
      await super().terminate()

  async def _turn_end_steps(self) -> None:
    """The turn-end steps, once per turn: the turn's own steps, then drop the claim and release.

    The claim is dropped even when the turn's own steps fail, so a finished turn never holds its
    directory. A second call does nothing.
    """
    if self._turn_end_steps_called:
      return
    self._turn_end_steps_called = True
    claim = self._claim
    self._claim = None
    try:
      if self._turn is not None:
        await self._turn.finish()
    except BaseException:
      if claim is not None:
        shared, directory = claim
        try:
          await shared.release_claim(directory)
        except BaseException:
          log.exception("opencode_release_failed_during_turn_cleanup", directory=directory)
      raise
    if claim is not None:
      shared, directory = claim
      await shared.release_claim(directory)

  async def _spawn_turn_process(self, cwd: str, env: dict) -> None:
    """Start this turn's /usr/bin/sleep, pinned as the Run's process identity and placed in the session cgroup."""
    await self._spawn_piped_and_pin_identity(self._build_command(""), cwd, env)
    assert self._proc is not None
    self._exit_task = asyncio.ensure_future(self._proc.wait())

  async def _until_exit(self, awaitable: Awaitable[T]) -> T:
    """Await *awaitable*, unless the turn process exits first: then the awaitable is cancelled and the turn ends."""
    task = asyncio.ensure_future(awaitable)
    try:
      done, _ = await asyncio.wait({task, self._exit_task}, return_when=asyncio.FIRST_COMPLETED)
    except BaseException:
      task.cancel()
      raise
    if task in done:
      return task.result()
    task.cancel()
    with contextlib.suppress(asyncio.CancelledError):
      await task
    raise _TurnProcessExitedError()

  async def _open_session(self, shared: server.OpenCodeServer, cwd: str) -> str:
    """Resume the recorded session, or create one at the turn's directory.

    A recorded id the store lacks fails the turn: the turn never creates a replacement session.
    """
    if self._resume_session_id is not None:
      response = await shared.client.get(f"/api/session/{self._resume_session_id}")
      if response.status_code == httpx.codes.NOT_FOUND:
        raise RuntimeError(
            f"OpenCode session {self._resume_session_id} is not in the opencode store at "
            f"{home.opencode_data_dir()}; the turn does not create a replacement session")
      response.raise_for_status()
      return self._resume_session_id
    provider_id, model_id = self._split_model()
    response = await shared.client.post(
        "/api/session", json={
            "location": {
                "directory": cwd
            },
            "model": {
                "id": model_id,
                "providerID": provider_id
            },
        })
    response.raise_for_status()
    return response.json()["data"]["id"]

  async def _fetch_model_limit(self, shared: server.OpenCodeServer, cwd: str) -> dict | None:
    """The model's limit dict from the 2.0 model list, read once per turn.

    Any failure — request error, provider or model absent, malformed payload — is contained:
    one ``log.warning`` is emitted and ``None`` is returned so the turn proceeds normally (the
    usage resolver then reports ``unknown`` for this session).
    """
    provider_id, model_id = self._split_model()
    try:
      response = await shared.client.get("/api/model", params={"location[directory]": cwd})
      response.raise_for_status()
      payload = response.json()
    except Exception as e:
      log.warning("opencode_context_catalog_unavailable", provider_id=provider_id, model_id=model_id, error=str(e))
      return None
    limit = self._extract_model_limit(payload, provider_id, model_id)
    if limit is None:
      log.warning("opencode_context_catalog_unavailable", provider_id=provider_id, model_id=model_id)
    return limit

  @staticmethod
  def _extract_model_limit(payload: object, provider_id: str, model_id: str) -> dict | None:
    """Find ``provider_id``/``model_id`` in the 2.0 model list and return its ``limit`` dict, or None."""
    models = payload.get("data") if isinstance(payload, dict) else None
    if not isinstance(models, list):
      return None
    for model in models:
      if isinstance(model, dict) and model.get("providerID") == provider_id and model.get("id") == model_id:
        limit = model.get("limit")
        return limit if isinstance(limit, dict) else None
    return None

  async def _put_environment(self, shared: server.OpenCodeServer, env: dict) -> None:
    """Replace the session's shell environment with the turn's, plus the join script when the cgroup exists."""
    if not server.JOIN_SCRIPT.is_file():
      raise RuntimeError(f"opencode join script is missing at {server.JOIN_SCRIPT}")
    variables = dict(env)
    if self._proxy_url is not None:
      base.apply_proxy_env(variables, self._proxy_url)
    cgroup = self._active_session_cgroup
    if cgroup is not None:
      variables["BASH_ENV"] = str(server.JOIN_SCRIPT)
      variables["CHARLIEBOT_SESSION_CGROUP"] = str(cgroup.path)
      variables["CHARLIEBOT_SESSION_OOM_SCORE_ADJ"] = str(cgroup.oom_score_adj)
    response = await shared.client.put(f"/api/session/{self._session_id}/environment", json={"variables": variables})
    response.raise_for_status()

  async def _reload_locations(self, shared: server.OpenCodeServer) -> None:
    response = await shared.client.post("/api/location/reload")
    response.raise_for_status()

  async def _send_prompt(
      self, shared: server.OpenCodeServer, session_id: str, prompt: str, uploaded_files: list[dict] | None) -> None:
    body: dict = {"text": prompt}
    attachments = _image_file_attachments(uploaded_files)
    if attachments:
      body["files"] = attachments
    response = await shared.client.post(f"/api/session/{session_id}/prompt", json=body)
    if response.status_code != httpx.codes.OK:
      raise RuntimeError(f"OpenCode prompt returned HTTP {response.status_code}: {response.text}")

  async def _consume_turn(self, turn: server.Turn) -> AsyncIterator[dict]:
    """Translate the session's events until one of them ends the turn."""
    while True:
      try:
        item = await self._until_exit(
            asyncio.wait_for(turn.events.get(), timeout=opencode_limits.OPENCODE_SSE_PROGRESS_TIMEOUT))
      except TimeoutError as e:
        log.error("opencode_sse_silence_timeout", session_id=turn.session_id)
        raise OpenCodeSseSilenceError(
            f"OpenCode session {turn.session_id} produced no event for "
            f"{opencode_limits.OPENCODE_SSE_PROGRESS_TIMEOUT:g} s") from e
      if isinstance(item, server.ServerLostError):
        raise item
      event_type = item["type"]
      data = item.get("data", {})
      if event_type == "permission.asked":
        self._failed = True
        yield base.make_error_event(
            f"OpenCode requested interactive permission '{data.get('permission')}' "
            f"(patterns={data.get('patterns')}) but CharlieBot runs headless with no approval path; aborting run.")
        return
      if event_type == "session.execution.succeeded":
        self._succeeded = True
        yield self._make_accumulated_result()
        return
      if event_type == "session.execution.failed":
        self._failed = True
        yield base.make_error_event(_error_text(data.get("error")))
        return
      if event_type == "session.execution.interrupted":
        self._failed = True
        yield base.make_error_event(f"OpenCode execution interrupted (reason: {data.get('reason')})")
        return
      for translated in self._translate_event(item):
        yield translated

  def _translate_event(self, event: dict) -> list[dict]:
    """Map one session event to CharlieBot's event dicts, updating the turn's usage and text state."""
    event_type = event["type"]
    data = event.get("data", {})

    if event_type == "session.step.started":
      if self.model_request_started_ms is None:
        self.model_request_started_ms = data["started"]
      return []

    if event_type == "session.text.delta":
      self._streamed_parts.add((data["assistantMessageID"], data["ordinal"]))
      return [base.make_text_event(data["delta"])]
    if event_type == "session.text.ended":
      if (data["assistantMessageID"], data["ordinal"]) in self._streamed_parts or not data["text"]:
        return []
      return [base.make_text_event(data["text"])]

    if event_type == "session.reasoning.delta":
      self._streamed_parts.add((data["assistantMessageID"], data["ordinal"]))
      return [{"type": ET.THINKING, "content": data["delta"]}]
    if event_type == "session.reasoning.ended":
      if (data["assistantMessageID"], data["ordinal"]) in self._streamed_parts or not data["text"]:
        return []
      return [{"type": ET.THINKING, "content": data["text"]}]

    if event_type == "session.tool.input.started":
      self._tool_names[data["id"]] = data["name"]
      return []
    if event_type == "session.tool.called":
      name = self._tool_names.get(data["id"])
      if name is None:
        raise RuntimeError(f"OpenCode tool call {data['id']} arrived without a tool name")
      return [
          {
              "type": ET.ASSISTANT,
              "message": {
                  "content": [{
                      "type": ET.TOOL_USE,
                      "name": name,
                      "id": data["id"],
                      "input": data["input"],
                  }]
              },
          }
      ]
    if event_type == "session.tool.success":
      content = "\n".join(
          part["text"] if part["type"] == "text" else part.get("name", part["mime"]) for part in data["content"])
      return [{"type": ET.TOOL_RESULT, "tool_use_id": data["id"], "content": content}]
    if event_type == "session.tool.failed":
      return [{"type": ET.TOOL_RESULT, "tool_use_id": data["id"], "content": _error_text(data.get("error"))}]

    if event_type == "session.step.ended":
      self._accumulate_usage(data["tokens"], data["cost"])
      return []
    if event_type == "session.compaction.started":
      pre_tokens = self._last_step_tokens["input"] if self._last_step_tokens is not None else None
      return [base.make_compact_boundary_event(data["reason"], pre_tokens)]
    if event_type == "session.compaction.ended":
      self._accumulate_usage(data["tokens"], data["cost"])
      return []

    if event_type in _IGNORED_SESSION_EVENT_TYPES:
      return []
    _UNHANDLED_SESSION_EVENT_TYPES.log(log.debug, "opencode_session_event_unhandled", event_type, type=event_type)
    return []

  def _accumulate_usage(self, tokens: dict, cost: float) -> None:
    cache = tokens["cache"]
    self._usage_input += tokens["input"]
    self._usage_output += tokens["output"]
    self._usage_cache_read += cache["read"]
    self._usage_cache_write += cache["write"]
    self._usage_cost += cost
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
    return base.make_result_event(
        input_tokens=self._usage_input,
        output_tokens=self._usage_output,
        cache_read=self._usage_cache_read,
        cache_creation=self._usage_cache_write,
        cost=self._usage_cost,
        context_snapshot=snapshot,
    )

  async def _end_turn_process(self) -> None:
    """Stop this turn's process and set the exit code: 1 failed, 0 succeeded, else the process's own code."""
    if self._proc is not None and self._proc.returncode is None:
      await self._graceful_shutdown(
          opencode_limits.OPENCODE_SERVER_STOP_TIMEOUT, timeout_log_event="opencode_turn_process_sigkill")
    if self._exit_task is not None:
      self._exit_task.cancel()
    if self._failed:
      self.exit_code = 1
    elif self._succeeded:
      self.exit_code = 0
    else:
      self.exit_code = self._proc.returncode if self._proc is not None and self._proc.returncode is not None else -1

  def _split_model(self) -> tuple[str, str]:
    if not self._model:
      raise ValueError("opencode backend requires a model")
    provider_id, model_id = self._model.split("/", 1)
    return provider_id, model_id

  async def one_shot_text(self, prompt: str, system_prompt: str, *, timeout: float | None) -> str:
    """Generate text with ``opencode run --standalone`` in a private store, every tool denied.

    The run gets its own server and a fresh XDG root, so it never touches the shared server's store;
    the root is deleted when the call returns. ``--format json`` emits one event per line: a text event
    per completed text part, and an error event that fails the call.
    """
    if not pathlib.Path(server.OPENCODE_BIN).is_file():
      raise RuntimeError(f"opencode 2.0 binary not found at {server.OPENCODE_BIN}")
    framed = self._frame_system_prompt(system_prompt, prompt)
    root = pathlib.Path(tempfile.mkdtemp(prefix="charliebot-opencode-oneshot-"))
    try:
      one_shot_config = {
          "permission": {
              "*": "deny"
          },
          **options.model_overrides_config([self._backend_option] if self._backend_option is not None else []),
      }
      env = {
          **os.environ,
          "XDG_DATA_HOME": str(root / "data"),
          "XDG_STATE_HOME": str(root / "state"),
          "XDG_CACHE_HOME": str(root / "cache"),
          "OPENCODE_CONFIG_CONTENT": json.dumps(one_shot_config),
      }
      if self._proxy_url is not None:
        base.apply_proxy_env(env, self._proxy_url)
      cmd = [server.OPENCODE_BIN, "run", "--standalone", "--format", "json", "-m", self._model, "--", framed]
      proc = await self._spawn_one_shot_subprocess(cmd, env, pdeathsig=True)

      async def _collect() -> str:
        parts: list[str] = []
        assert proc.stdout is not None
        try:
          async for event in base.iter_ndjson_events(proc.stdout):
            if event.get("type") == "error":
              raise RuntimeError(f"opencode run failed: {_error_text(event.get('error'))}")
            if event.get("type") == "text":
              parts.append(event["part"]["text"])
        finally:
          await proc.wait()
        return "".join(parts).strip()

      stderr_task = asyncio.create_task(proc.stderr.read())
      return await process.wait_or_kill_group(_collect(), timeout, proc.pid, stderr_task)
    finally:
      shutil.rmtree(root, ignore_errors=True)
