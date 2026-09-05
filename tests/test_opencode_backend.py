import asyncio
import json
import re
import sys
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import httpx
import pytest
from conftest import (
  OPENCODE_RESOLVE_BINARY_PATCH_TARGET,
  SYNTHETIC_MODEL,
  FakeChunkedResponse,
  build_cli_backend,
)

import src.agents.backends.opencode as opencode_mod
from src.agents.backends.base import make_error_event
from src.agents.backends.opencode import (
    SSE_EVENT_MESSAGE_PART_UPDATED,
    SSE_EVENT_MESSAGE_UPDATED,
    SSE_EVENT_PERMISSION_ASKED,
    SSE_EVENT_SERVER_CONNECTED,
  SSE_EVENT_SESSION_ERROR,
  SSE_EVENT_SESSION_IDLE,
  OpenCodeBackend,
  OpenCodeSseSilenceError,
)
from src.core import event_types as ET
from src.core.streaming import handle_compaction_events

_CREATE_SUBPROCESS_EXEC_PATCH_TARGET = "src.agents.backends.opencode.asyncio.create_subprocess_exec"


def _build_backend(monkeypatch, **kwargs) -> OpenCodeBackend:
  return build_cli_backend(
      monkeypatch, OpenCodeBackend, OPENCODE_RESOLVE_BINARY_PATCH_TARGET, "/usr/bin/opencode", **kwargs)


def _rig_end_to_end_run(monkeypatch, backend: OpenCodeBackend, response) -> MagicMock:
  """Mock the serve-and-connect path so backend.run() consumes `response` as the
  /event stream end-to-end; returns the spawned process mock for spawn assertions."""
  process = MagicMock()
  process.pid = 4321
  process.returncode = 0
  process.wait = AsyncMock(return_value=0)
  monkeypatch.setattr(
      _CREATE_SUBPROCESS_EXEC_PATCH_TARGET, AsyncMock(return_value=process))
  monkeypatch.setattr(backend, "_read_server_url", AsyncMock(return_value="http://127.0.0.1:4242"))
  monkeypatch.setattr(backend, "_stream_stderr", AsyncMock())
  monkeypatch.setattr(backend, "_stream_stdout", AsyncMock())
  monkeypatch.setattr(backend, "_check_health", AsyncMock())
  monkeypatch.setattr(backend, "_fetch_model_limit", AsyncMock(return_value=None))
  monkeypatch.setattr(backend, "_create_session", AsyncMock(return_value="session-1"))
  monkeypatch.setattr(backend, "_send_prompt", AsyncMock())
  monkeypatch.setattr(
      "src.agents.backends.opencode.httpx.AsyncClient",
      lambda **kwargs: _FakeRunHttpClient(response))
  return process


class _FakeEventStream:
  """Async-iterable of pre-baked SSE event dicts for _consume_sse_events tests."""

  def __init__(self, events: list[dict]) -> None:
    self._events = events

  async def __aiter__(self):
    for event in self._events:
      yield event


class _FakeSseResponse:
  """SSE response double over pre-chunked bytes (one chunk per line)."""

  def __init__(self, lines: list[str]) -> None:
    self._lines = lines

  async def aiter_bytes(self):
    for line in self._lines:
      yield (line + "\n").encode("utf-8")


class _FakeOneShotStdout:

  def __aiter__(self):
    return self

  async def __anext__(self):
    raise StopAsyncIteration


@pytest.mark.asyncio
async def test_iter_sse_events_ignores_comments_and_metadata(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)
  response = _FakeSseResponse(
      [
          ": keepalive",
          "event: message.updated",
          "id: event-1",
          "retry: 1000",
          'data: {"type": "server.connected"}',
          "",
          ": another keepalive",
          "event: session.idle",
          "x-extra: ignored",
          'data: {"type": "session.idle",',
          'data: "properties": {"sessionID": "session-1"}}',
          "",
      ])

  events = [event async for event in backend._iter_sse_events(response)]

  assert events == [
      {
          "type": SSE_EVENT_SERVER_CONNECTED
      },
      {
          "type": SSE_EVENT_SESSION_IDLE,
          "properties": {
              "sessionID": "session-1"
          }
      },
  ]


@pytest.mark.asyncio
async def test_raw_splitline_chars_in_frame_parse_as_one_event_end_to_end(monkeypatch, tmp_path: Path) -> None:
  """Regression for run 6dc42358: a message.part.updated frame whose JSON string
  carries raw U+0085/U+2028 (which JSON.stringify leaves unescaped and the SSE
  spec keeps inside the line) must parse as exactly one event through the full
  backend chain instead of dying with 'Unterminated string'."""
  _patch_watchdog_timeout(monkeypatch)
  backend = _build_backend(monkeypatch, model="provider/model")
  nel = "\x85"
  ls = "\u2028"
  part_text = '...identifier\\");else{a:48<' + nel + ">NEL-CHAR" + ls + '>LS-CHAR"},"status":"running",...'
  payload = json.dumps(
      {
          "type": SSE_EVENT_MESSAGE_PART_UPDATED,
          "properties": {
              "sessionID": "session-1",
              "part": {
                  "messageID": "m1",
                  "id": "part-1",
                  "type": "text",
                  "text": part_text,
              },
          },
      },
      ensure_ascii=False,
  )
  raw_frame = ("data: " + payload + "\n\n").encode("utf-8")
  cut_nel = raw_frame.index(nel.encode("utf-8")) + 1
  cut_ls = raw_frame.index(ls.encode("utf-8"))
  frame_chunks = [raw_frame[:cut_nel], raw_frame[cut_nel:cut_ls], raw_frame[cut_ls:]]
  chunks = [
      b'data: {"type": "server.connected", "properties": {}}\n',
      b"\n",
      b'data: {"type": "message.updated", "properties": {"sessionID": "session-1", '
      b'"info": {"id": "m1", "role": "assistant"}}}\n',
      b"\n",
      *frame_chunks,
      b'data: {"type": "session.idle", "properties": {"sessionID": "session-1"}}\n',
      b"\n",
  ]

  parsed = [
      event async for event in backend._iter_sse_events(FakeChunkedResponse(frame_chunks))
  ]
  assert parsed == [json.loads(payload)]
  assert parsed[0]["properties"]["part"]["text"] == part_text

  _rig_end_to_end_run(monkeypatch, backend, FakeChunkedResponse(chunks))

  events = [event async for event in backend.run("prompt", str(tmp_path), {"PATH": "/usr/bin"})]

  error_events = [event for event in events if event.get("type") == ET.ERROR]
  assert not error_events
  text_events = [event for event in events if event.get("type") == ET.ASSISTANT]
  assert text_events == [{
      "type": ET.ASSISTANT,
      "message": {
          "content": [{
              "type": "text",
              "text": part_text,
          }]
      },
  }]
  assert backend.exit_code == 0


def test_translate_sse_event_buffers_part_until_message_role_known(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  assert not backend._translate_sse_event(
      {
          "type": SSE_EVENT_MESSAGE_PART_UPDATED,
          "properties": {
              "part": {
                  "messageID": "message-1",
                  "id": "part-1",
                  "type": "text",
                  "text": "Hello",
              }
          },
      })
  assert not backend._translate_sse_event(
      {
          "type": SSE_EVENT_MESSAGE_PART_UPDATED,
          "properties": {
              "part": {
                  "messageID": "message-1",
                  "id": "part-1",
                  "type": "text",
                  "text": "Hello world",
              }
          },
      })

  translated = backend._translate_sse_event(
      {
          "type": SSE_EVENT_MESSAGE_UPDATED,
          "properties": {
              "info": {
                  "id": "message-1",
                  "role": "assistant",
              }
          },
      })

  assert translated == [
      {
          "type": "assistant",
          "message": {
              "content": [{
                  "type": "text",
                  "text": "Hello"
              }]
          }
      },
      {
          "type": "assistant",
          "message": {
              "content": [{
                  "type": "text",
                  "text": " world"
              }]
          }
      },
  ]


def test_translate_sse_event_discards_buffered_non_assistant_parts(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  assert not backend._translate_sse_event(
      {
          "type": SSE_EVENT_MESSAGE_PART_UPDATED,
          "properties": {
              "part": {
                  "messageID": "message-1",
                  "id": "part-1",
                  "type": "text",
                  "text": "Hello",
              }
          },
      })

  translated = backend._translate_sse_event(
      {
          "type": SSE_EVENT_MESSAGE_UPDATED,
          "properties": {
              "info": {
                  "id": "message-1",
                  "role": "user",
              }
          },
      })

  assert not translated
  assert not backend._pending_parts


def test_prepare_env_sets_charliebot_opencode_config(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  env = backend._prepare_env({"PATH": "/usr/bin"})

  data = json.loads(env["OPENCODE_CONFIG_CONTENT"])
  assert data["permission"] == {"*": "allow", "question": "deny"}
  assert data["default_agent"] == "charliebot"
  assert data["agent"]["charliebot"] == {"mode": "primary"}


def test_prepare_env_merges_proxy_and_local_no_proxy_without_mutating_input(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, opencode_proxy_url="http://proxy.test:8080")
  input_env = {
      "PATH": "/usr/bin",
      "NO_PROXY": "internal.test,localhost,127.0.0.1",
  }
  original_env = dict(input_env)

  prepared = backend._prepare_env(input_env)

  assert prepared["HTTP_PROXY"] == "http://proxy.test:8080"
  assert prepared["HTTPS_PROXY"] == "http://proxy.test:8080"
  assert prepared["NO_PROXY"] == "internal.test,localhost,127.0.0.1,::1"
  assert input_env == original_env

  repeated = backend._prepare_env(prepared)
  assert repeated["NO_PROXY"] == prepared["NO_PROXY"]


def test_prepare_env_without_proxy_preserves_proxy_related_environment(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)
  input_env = {
      "PATH": "/usr/bin",
      "HTTP_PROXY": "http://existing-http.test:8080",
      "HTTPS_PROXY": "http://existing-https.test:8080",
      "NO_PROXY": "internal.test,localhost",
  }
  original_env = dict(input_env)

  prepared = backend._prepare_env(input_env)

  assert {
      key: prepared[key] for key in ("HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY")
  } == {
      "HTTP_PROXY": "http://existing-http.test:8080",
      "HTTPS_PROXY": "http://existing-https.test:8080",
      "NO_PROXY": "internal.test,localhost",
  }
  assert input_env == original_env


def test_proxy_state_is_isolated_between_backend_instances(monkeypatch) -> None:
  proxied = _build_backend(monkeypatch, opencode_proxy_url="http://proxy.test:8080")
  unproxied = _build_backend(monkeypatch)

  proxied_env = proxied._prepare_env({"PATH": "/usr/bin"})
  unproxied_env = unproxied._prepare_env({"PATH": "/usr/bin"})

  assert proxied_env["HTTP_PROXY"] == "http://proxy.test:8080"
  assert proxied_env["HTTPS_PROXY"] == "http://proxy.test:8080"
  assert proxied_env["NO_PROXY"] == "localhost,127.0.0.1,::1"
  assert "HTTP_PROXY" not in unproxied_env
  assert "HTTPS_PROXY" not in unproxied_env
  assert "NO_PROXY" not in unproxied_env


@pytest.mark.asyncio
async def test_run_passes_proxy_environment_to_serve_subprocess(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch, model="provider/model", opencode_proxy_url="http://proxy.test:8080")
  process = MagicMock()
  process.pid = 1234
  create_process = AsyncMock(return_value=process)
  monkeypatch.setattr(_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, create_process)
  monkeypatch.setattr(backend, "_read_server_url", AsyncMock(side_effect=RuntimeError("stop after spawn")))
  monkeypatch.setattr(backend, "_stream_stderr", AsyncMock())
  cleanup = AsyncMock()
  monkeypatch.setattr(backend, "_cleanup_server", cleanup)
  input_env = {"PATH": "/usr/bin", "NO_PROXY": "internal.test,localhost"}
  original_env = dict(input_env)

  events = [event async for event in backend.run("prompt", str(tmp_path), input_env)]

  child_env = create_process.await_args.kwargs["env"]
  assert child_env["HTTP_PROXY"] == "http://proxy.test:8080"
  assert child_env["HTTPS_PROXY"] == "http://proxy.test:8080"
  assert child_env["NO_PROXY"] == "internal.test,localhost,127.0.0.1,::1"
  assert json.loads(child_env["OPENCODE_CONFIG_CONTENT"])["permission"] == {
      "*": "allow",
      "question": "deny",
  }
  assert input_env == original_env
  assert events[0]["type"] == ET.ERROR
  cleanup.assert_awaited_once()
  assert backend._stderr_task is not None
  await backend._stderr_task
  _assert_pdeathsig_preexec(create_process.await_args.kwargs)


def _assert_pdeathsig_preexec(kwargs: dict) -> None:
  """The shared piped spawn passes the PDEATHSIG preexec on Linux, an untouched spawn elsewhere."""
  if sys.platform == "linux":
    assert callable(kwargs["preexec_fn"])
  else:
    assert kwargs["preexec_fn"] is None


@pytest.mark.asyncio
async def test_one_shot_text_passes_proxy_environment_and_deny_policy(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="provider/model", opencode_proxy_url="http://proxy.test:8080")
  monkeypatch.setenv("NO_PROXY", "internal.test,localhost")
  monkeypatch.setenv("HTTP_PROXY", "http://ambient-http.test:8080")
  monkeypatch.setenv("HTTPS_PROXY", "http://ambient-https.test:8080")
  process = MagicMock()
  process.stdout = _FakeOneShotStdout()
  process.stderr = MagicMock()
  process.stderr.read = AsyncMock(return_value=b"")
  process.wait = AsyncMock(return_value=0)
  process.pid = 5678
  process.returncode = 0
  create_process = AsyncMock(return_value=process)

  with patch(_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, new=create_process):
    result = await backend.one_shot_text("prompt", "system", timeout=5.0)

  child_env = create_process.await_args.kwargs["env"]
  assert result == ""
  assert child_env["HTTP_PROXY"] == "http://proxy.test:8080"
  assert child_env["HTTPS_PROXY"] == "http://proxy.test:8080"
  assert child_env["NO_PROXY"] == "internal.test,localhost,127.0.0.1,::1"
  assert json.loads(child_env["OPENCODE_CONFIG_CONTENT"]) == {"permission": {"*": "deny"}}
  process.wait.assert_awaited_once()
  _assert_pdeathsig_preexec(create_process.await_args.kwargs)


def test_prepare_cwd_writes_agents_md_when_instructions_provided(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch, instructions_content="# Test Instructions\nDo things.")

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert agents_md.exists()
  assert agents_md.read_text(encoding="utf-8") == "# Test Instructions\nDo things."


def test_prepare_cwd_writes_agents_md_even_when_config_exists(monkeypatch, tmp_path: Path) -> None:
  """AGENTS.md must be written even when opencode.json already exists (resumed sessions)."""
  backend = _build_backend(monkeypatch, instructions_content="# Instructions")
  config_dir = tmp_path / ".opencode"
  config_dir.mkdir()
  (config_dir / "opencode.json").write_text("{}", encoding="utf-8")

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert agents_md.exists()
  assert agents_md.read_text(encoding="utf-8") == "# Instructions"


def test_prepare_cwd_skips_agents_md_when_no_instructions(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert not agents_md.exists()


def test_translate_tool_error_emits_tool_result(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  translated = backend.translate_event({
      "type": "tool",
      "part": {
          "callID": "call-1",
          "tool": "glob",
          "state": {
              "input": {"pattern": "AGENTS.md", "path": "/tmp"},
              "error": "The user rejected permission to use this specific tool call.",
          },
      },
  })

  assert translated == [
      {
          "type": "assistant",
          "message": {
              "content": [{
                  "type": "tool_use",
                  "name": "glob",
                  "id": "call-1",
                  "input": {"pattern": "AGENTS.md", "path": "/tmp"},
              }]
          },
      },
      {
          "type": "tool_result",
          "tool_use_id": "call-1",
          "content": "The user rejected permission to use this specific tool call.",
      },
  ]


async def _drain(events):
  return [event async for event in events]


@pytest.mark.asyncio
async def test_consume_sse_events_parent_permission_ask_fails_fast(monkeypatch) -> None:
  """A permission ask for the parent session is a terminal error that ends the turn."""
  backend = _build_backend(monkeypatch)
  backend._session_id = "parent-session"

  events = await _drain(
      backend._consume_sse_events(
          _FakeEventStream([
              {
                  "type": SSE_EVENT_PERMISSION_ASKED,
                  "properties": {
                      "id": "perm-1",
                      "sessionID": "parent-session",
                      "permission": "external_directory",
                      "patterns": ["/etc"],
                  },
              },
              {
                  "type": SSE_EVENT_SESSION_IDLE,
                  "properties": {"sessionID": "parent-session"},
              },
          ])))

  assert len(events) == 1
  assert events[0]["type"] == ET.ERROR
  assert "external_directory" in events[0]["message"]
  assert "/etc" in events[0]["message"]
  assert "parent-session" in events[0]["message"]
  assert backend._failed is True


@pytest.mark.asyncio
async def test_consume_sse_events_child_permission_ask_fails_before_filtering(monkeypatch) -> None:
  """A child-session permission ask must be caught before the parent-session filter drops it."""
  backend = _build_backend(monkeypatch)
  backend._session_id = "parent-session"

  events = await _drain(
      backend._consume_sse_events(
          _FakeEventStream([{
              "type": SSE_EVENT_PERMISSION_ASKED,
              "properties": {
                  "id": "perm-2",
                  "sessionID": "child-session",
                  "permission": "doom_loop",
                  "patterns": ["task"],
              },
          }])))

  assert len(events) == 1
  assert events[0]["type"] == ET.ERROR
  assert "doom_loop" in events[0]["message"]
  assert "child-session" in events[0]["message"]
  assert backend._failed is True


@pytest.mark.asyncio
async def test_consume_sse_events_normal_parent_turn(monkeypatch) -> None:
  """Assistant text, parent-session filtering, idle, and usage aggregation still work."""
  backend = _build_backend(monkeypatch)
  backend._session_id = "parent-session"

  events = await _drain(
      backend._consume_sse_events(
          _FakeEventStream([
              {  # dropped: belongs to an unrelated session
                  "type": SSE_EVENT_MESSAGE_PART_UPDATED,
                  "properties": {"sessionID": "other", "part": {"messageID": "m9", "id": "p9", "type": "text",
                                                                "text": "ignored"}},
              },
              {
                  "type": SSE_EVENT_MESSAGE_UPDATED,
                  "properties": {"sessionID": "parent-session", "info": {"id": "m1", "role": "assistant"}},
              },
              {
                  "type": SSE_EVENT_MESSAGE_PART_UPDATED,
                  "properties": {"sessionID": "parent-session",
                                 "part": {"messageID": "m1", "id": "p1", "type": "text", "text": "Hello"}},
              },
              {
                  "type": SSE_EVENT_SESSION_IDLE,
                  "properties": {"sessionID": "parent-session"},
              },
          ])))

  assert events == [
      {"type": "assistant", "message": {"content": [{"type": "text", "text": "Hello"}]}},
      backend._make_accumulated_result(),
  ]
  assert backend._failed is False


def test_is_cancellation_disconnect_true_after_terminate(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)
  backend.terminated = True

  assert backend._is_cancellation_disconnect(httpx.RemoteProtocolError("incomplete chunked read")) is True


def test_is_cancellation_disconnect_false_without_terminate(monkeypatch) -> None:
  """An unexpected disconnect with no deliberate terminate stays a visible backend failure."""
  backend = _build_backend(monkeypatch)

  assert backend._is_cancellation_disconnect(httpx.RemoteProtocolError("incomplete chunked read")) is False


def test_translate_sse_event_reasoning_part_emits_thinking_delta(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  assert not backend._translate_sse_event({
      "type": SSE_EVENT_MESSAGE_PART_UPDATED,
      "properties": {
          "part": {
              "messageID": "message-1",
              "id": "part-r",
              "type": "reasoning",
              "text": "I need",
          }
      },
  })
  assert not backend._translate_sse_event({
      "type": SSE_EVENT_MESSAGE_PART_UPDATED,
      "properties": {
          "part": {
              "messageID": "message-1",
              "id": "part-r",
              "type": "reasoning",
              "text": "I need to think",
          }
      },
  })

  translated = backend._translate_sse_event({
      "type": SSE_EVENT_MESSAGE_UPDATED,
      "properties": {
          "info": {
              "id": "message-1",
              "role": "assistant",
          }
      },
  })

  assert translated == [
      {
          "type": ET.THINKING,
          "content": "I need"
      },
      {
          "type": ET.THINKING,
          "content": " to think"
      },
  ]


# ---------------------------------------------------------------------------
# context_snapshot: last step's tokens, reasoning, model limit, catalog failure
# ---------------------------------------------------------------------------


def _step_finish_part(input_t, output_t, reasoning_t, cache_read_t, cache_write_t, cost):
  return {
      "messageID": "m1",
      "id": "p1",
      "type": "step-finish",
      "tokens": {
          "input": input_t,
          "output": output_t,
          "reasoning": reasoning_t,
          "cache": {"read": cache_read_t, "write": cache_write_t},
      },
      "cost": cost,
  }


def test_make_accumulated_result_snapshot_carries_last_step_tokens_not_sum(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model=SYNTHETIC_MODEL)
  backend._model_limit = {"context": 409600, "input": 270000, "output": 131072}
  backend._accumulate_step_finish(
      _step_finish_part(100, 10, 5, 20, 30, 0.1))
  backend._accumulate_step_finish(
      _step_finish_part(200, 20, 8, 40, 60, 0.2))

  result = backend._make_accumulated_result()

  assert result["type"] == ET.RESULT
  snapshot = result["context_snapshot"]
  # The snapshot carries the *last* step's tokens, not the turn's sum.
  assert snapshot["tokens"] == {
      "input": 200, "output": 20, "reasoning": 8, "cache_read": 40, "cache_write": 60}
  # The usage block still carries the turn's accumulated sum.
  assert result["usage"]["input_tokens"] == 300
  assert result["usage"]["output_tokens"] == 30
  assert result["usage"]["cache_read_input_tokens"] == 60
  assert result["usage"]["cache_creation_input_tokens"] == 90
  # reasoning is preserved in the snapshot.
  assert snapshot["tokens"]["reasoning"] == 8
  # cost still accumulates.
  assert result["total_cost_usd"] == pytest.approx(0.3)
  assert snapshot["model"] == SYNTHETIC_MODEL
  assert snapshot["limit"] == {"context": 409600, "input": 270000, "output": 131072}


def test_make_accumulated_result_omits_snapshot_when_no_step_finish(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model=SYNTHETIC_MODEL)
  backend._model_limit = {"context": 409600, "input": 270000, "output": 131072}

  result = backend._make_accumulated_result()

  assert "context_snapshot" not in result


def test_make_accumulated_result_snapshot_limit_none_when_catalog_unavailable(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model=SYNTHETIC_MODEL)
  backend._model_limit = None  # catalog unavailable -> limit stays None
  backend._accumulate_step_finish(_step_finish_part(100, 10, 0, 0, 0, 0.1))

  result = backend._make_accumulated_result()

  snapshot = result["context_snapshot"]
  assert snapshot["limit"] is None
  assert snapshot["tokens"] == {
      "input": 100, "output": 10, "reasoning": 0, "cache_read": 0, "cache_write": 0}


def test_reset_run_state_clears_model_limit_and_last_step_tokens(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model=SYNTHETIC_MODEL)
  backend._model_limit = {"context": 409600, "input": 270000, "output": 131072}
  backend._accumulate_step_finish(_step_finish_part(100, 10, 0, 0, 0, 0.1))
  assert backend._last_step_tokens is not None

  backend._reset_run_state()

  assert backend._model_limit is None
  assert backend._last_step_tokens is None


class _FakeConfigResponse:

  def __init__(self, payload=None, exc: Exception | None = None, status: int = 200) -> None:
    self._payload = payload
    self._exc = exc
    self._status = status

  def raise_for_status(self) -> None:
    if self._status >= 400:
      raise httpx.HTTPStatusError("bad status", request=None, response=self)

  def json(self):
    return self._payload


class _FakeConfigClient:
  """Minimal httpx-like client for ``_fetch_model_limit`` tests."""

  def __init__(self, response: _FakeConfigResponse | None = None,
               exc: Exception | None = None) -> None:
    self._response = response
    self._exc = exc

  async def get(self, url: str) -> _FakeConfigResponse:
    assert url == "/config/providers"
    if self._exc is not None:
      raise self._exc
    return self._response


@pytest.mark.asyncio
async def test_fetch_model_limit_returns_limit_from_recorded_providers(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model=SYNTHETIC_MODEL)
  # Recorded /config/providers payload: top-level {"providers": [...]}, each
  # provider's models is a dict keyed by model id.
  providers = {
      "providers": [
          {"id": "other-provider", "models": {"x": {"id": "x", "limit": {"context": 100}}}},
          {
              "id": "synthetic-provider",
              "models": {
                  "nvidia/Synthetic-Model": {
                      "id": "nvidia/Synthetic-Model",
                      "limit": {"context": 409600, "input": 270000, "output": 131072},
                  },
              },
          },
      ],
      "default": "synthetic-provider",
  }
  client = _FakeConfigClient(response=_FakeConfigResponse(payload=providers))

  limit = await backend._fetch_model_limit(client)

  assert limit == {"context": 409600, "input": 270000, "output": 131072}


@pytest.mark.asyncio
async def test_fetch_model_limit_accepts_bare_list_and_list_models(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="prov/model-id")
  # Alternative shapes: bare top-level list and list-shaped models.
  providers = [
      {"id": "prov", "models": [{"id": "model-id", "limit": {"context": 8, "output": 4}}]},
  ]
  client = _FakeConfigClient(response=_FakeConfigResponse(payload=providers))

  limit = await backend._fetch_model_limit(client)

  assert limit == {"context": 8, "output": 4}


@pytest.mark.asyncio
async def test_fetch_model_limit_returns_none_and_warns_on_request_error(monkeypatch, capsys) -> None:
  backend = _build_backend(monkeypatch, model=SYNTHETIC_MODEL)
  client = _FakeConfigClient(exc=httpx.ConnectError("connection refused"))

  limit = await backend._fetch_model_limit(client)

  assert limit is None
  out = capsys.readouterr().out
  assert "opencode_context_catalog_unavailable" in out
  assert "synthetic-provider" in out


@pytest.mark.asyncio
async def test_fetch_model_limit_returns_none_and_warns_when_provider_absent(monkeypatch, capsys) -> None:
  backend = _build_backend(monkeypatch, model=SYNTHETIC_MODEL)
  providers = {"providers": [{"id": "other-provider", "models": {}}]}
  client = _FakeConfigClient(response=_FakeConfigResponse(payload=providers))

  limit = await backend._fetch_model_limit(client)

  assert limit is None
  out = capsys.readouterr().out
  assert "opencode_context_catalog_unavailable" in out


@pytest.mark.asyncio
async def test_fetch_model_limit_returns_none_and_warns_when_model_absent(monkeypatch, capsys) -> None:
  backend = _build_backend(monkeypatch, model=SYNTHETIC_MODEL)
  providers = {"providers": [{"id": "synthetic-provider", "models": {"other-model": {"id": "other-model"}}}]}
  client = _FakeConfigClient(response=_FakeConfigResponse(payload=providers))

  limit = await backend._fetch_model_limit(client)

  assert limit is None
  out = capsys.readouterr().out
  assert "opencode_context_catalog_unavailable" in out


@pytest.mark.asyncio
async def test_fetch_model_limit_returns_none_and_warns_on_malformed_payload(monkeypatch, capsys) -> None:
  backend = _build_backend(monkeypatch, model=SYNTHETIC_MODEL)
  # Not a recognised shape (no "providers" key, not a list).
  client = _FakeConfigClient(response=_FakeConfigResponse(payload={"random": "shape"}))

  limit = await backend._fetch_model_limit(client)

  assert limit is None
  out = capsys.readouterr().out
  assert "opencode_context_catalog_unavailable" in out


@pytest.mark.asyncio
async def test_fetch_model_limit_returns_none_when_model_unset(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model=None)
  client = _FakeConfigClient(response=_FakeConfigResponse(payload=[]))

  limit = await backend._fetch_model_limit(client)

  assert limit is None


@pytest.mark.asyncio
async def test_consume_sse_events_emits_snapshot_with_last_step_tokens(monkeypatch) -> None:
  """A recorded SSE step-finish sequence yields a result event whose
  context_snapshot carries the last step's tokens while the usage block carries
  the turn's accumulated sum; the catalog failure (limit None) does not fail the
  turn."""
  backend = _build_backend(monkeypatch, model=SYNTHETIC_MODEL)
  backend._session_id = "parent-session"
  backend._model_limit = None  # catalog unavailable

  events = await _drain(
      backend._consume_sse_events(
          _FakeEventStream([
              {"type": SSE_EVENT_MESSAGE_UPDATED,
               "properties": {"sessionID": "parent-session", "info": {"id": "m1", "role": "assistant"}}},
              {"type": SSE_EVENT_MESSAGE_PART_UPDATED,
               "properties": {"sessionID": "parent-session", "part": _step_finish_part(100, 10, 5, 20, 30, 0.1)}},
              {"type": SSE_EVENT_MESSAGE_PART_UPDATED,
               "properties": {"sessionID": "parent-session", "part": _step_finish_part(200, 20, 8, 40, 60, 0.2)}},
              {"type": SSE_EVENT_SESSION_IDLE, "properties": {"sessionID": "parent-session"}},
          ])))

  assert backend._failed is False
  result = events[-1]
  assert result["type"] == ET.RESULT
  snapshot = result["context_snapshot"]
  # Last step's tokens, not the turn's sum.
  assert snapshot["tokens"] == {
      "input": 200, "output": 20, "reasoning": 8, "cache_read": 40, "cache_write": 60}
  # The usage block carries the turn's accumulated sum.
  assert result["usage"]["input_tokens"] == 300
  assert result["usage"]["output_tokens"] == 30
  assert result["usage"]["cache_read_input_tokens"] == 60
  assert result["usage"]["cache_creation_input_tokens"] == 90
  # reasoning preserved.
  assert snapshot["tokens"]["reasoning"] == 8
  # Catalog unavailable -> limit None, but the turn still completed.
  assert snapshot["limit"] is None
  assert snapshot["model"] == SYNTHETIC_MODEL


# ---------------------------------------------------------------------------
# Compaction summary suppression: opencode auto-compaction publishes an
# assistant message (summary=true, agent="compaction") whose text/reasoning
# must never reach the chat stream; instead exactly one compact_boundary
# event is synthesized, and the message's own step-finish usage is kept.
# ---------------------------------------------------------------------------

_COMPACTION_TOKENS = {
    "total": 142466,
    "input": 140853,
    "output": 1613,
    "reasoning": 0,
    "cache": {"write": 0, "read": 0},
}
_COMPACTION_COST = 0.42


def _compaction_message_info(message_id: str, *, completed: bool) -> dict:
  """Recorded compaction message shape: first delivery is all-zero tokens with no
  time.completed; the later delivery carries the real tokens once the step finishes."""
  tokens = _COMPACTION_TOKENS if completed else {
      "total": 0, "input": 0, "output": 0, "reasoning": 0, "cache": {"write": 0, "read": 0}}
  time = {"created": 1, "completed": 2} if completed else {"created": 1}
  return {
      "id": message_id,
      "role": "assistant",
      "mode": "compaction",
      "agent": "compaction",
      "summary": True,
      "tokens": tokens,
      "time": time,
  }


def _compaction_parts(message_id: str) -> list[dict]:
  """Recorded part sequence for a compaction message: step-start, reasoning, text,
  step-finish (step-finish carries the compaction call's own tokens/cost)."""
  return [
      {"messageID": message_id, "id": f"{message_id}-step-start", "type": "step-start"},
      {"messageID": message_id, "id": f"{message_id}-reasoning", "type": "reasoning",
       "text": "[placeholder compaction reasoning]"},
      {"messageID": message_id, "id": f"{message_id}-text", "type": "text",
       "text": "[placeholder compaction summary]"},
      {
          "messageID": message_id,
          "id": f"{message_id}-step-finish",
          "type": "step-finish",
          "tokens": _COMPACTION_TOKENS,
          "cost": _COMPACTION_COST,
      },
  ]


def test_compaction_message_full_sequence_emits_no_chat_content(monkeypatch) -> None:
  """message.updated (created) -> parts -> message.updated (completed): zero text/thinking."""
  backend = _build_backend(monkeypatch)
  message_id = "msg_compaction_full"

  translated: list[dict] = []
  translated += backend._translate_sse_event({
      "type": SSE_EVENT_MESSAGE_UPDATED,
      "properties": {"info": _compaction_message_info(message_id, completed=False)},
  })
  for part in _compaction_parts(message_id):
    translated += backend._translate_sse_event({
        "type": SSE_EVENT_MESSAGE_PART_UPDATED,
        "properties": {"part": part},
    })
  translated += backend._translate_sse_event({
      "type": SSE_EVENT_MESSAGE_UPDATED,
      "properties": {"info": _compaction_message_info(message_id, completed=True)},
  })

  assert not any(event["type"] == ET.ASSISTANT for event in translated)
  assert not any(event["type"] == ET.THINKING for event in translated)


def test_compaction_message_adversarial_buffered_order_emits_no_chat_content(monkeypatch) -> None:
  """Adversarial order (parts buffered before message.updated arrives): still zero leak."""
  backend = _build_backend(monkeypatch)
  message_id = "msg_compaction_adversarial"

  translated: list[dict] = []
  for part in _compaction_parts(message_id):
    translated += backend._translate_sse_event({
        "type": SSE_EVENT_MESSAGE_PART_UPDATED,
        "properties": {"part": part},
    })
  translated += backend._translate_sse_event({
      "type": SSE_EVENT_MESSAGE_UPDATED,
      "properties": {"info": _compaction_message_info(message_id, completed=True)},
  })

  assert not any(event["type"] == ET.ASSISTANT for event in translated)
  assert not any(event["type"] == ET.THINKING for event in translated)
  assert not backend._pending_parts


def test_compaction_step_finish_usage_is_conserved(monkeypatch) -> None:
  """Accumulated usage after one normal message and one compaction message, each with a
  step-finish part, equals the sum over every step-finish part fed."""
  backend = _build_backend(monkeypatch)
  normal_step_finish = _step_finish_part(500, 50, 10, 5, 7, 0.03)
  message_id = "msg_compaction_usage"

  backend._translate_sse_event({
      "type": SSE_EVENT_MESSAGE_UPDATED,
      "properties": {"info": {"id": normal_step_finish["messageID"], "role": "assistant"}},
  })
  backend._translate_sse_event({
      "type": SSE_EVENT_MESSAGE_PART_UPDATED,
      "properties": {"part": normal_step_finish},
  })
  backend._translate_sse_event({
      "type": SSE_EVENT_MESSAGE_UPDATED,
      "properties": {"info": _compaction_message_info(message_id, completed=False)},
  })
  for part in _compaction_parts(message_id):
    backend._translate_sse_event({
        "type": SSE_EVENT_MESSAGE_PART_UPDATED,
        "properties": {"part": part},
    })

  assert backend._usage_input == normal_step_finish["tokens"]["input"] + _COMPACTION_TOKENS["input"]
  assert backend._usage_output == normal_step_finish["tokens"]["output"] + _COMPACTION_TOKENS["output"]
  assert backend._usage_cache_read == (
      normal_step_finish["tokens"]["cache"]["read"] + _COMPACTION_TOKENS["cache"]["read"])
  assert backend._usage_cache_write == (
      normal_step_finish["tokens"]["cache"]["write"] + _COMPACTION_TOKENS["cache"]["write"])
  assert backend._usage_cost == pytest.approx(normal_step_finish["cost"] + _COMPACTION_COST)


def test_compaction_boundary_emitted_exactly_once_per_message(monkeypatch) -> None:
  """Two summary messages, each message.updated delivered twice, yield exactly two
  compact_boundary events; pre_tokens reflects the _last_step_tokens snapshot in effect
  at registration (None before any step has completed)."""
  backend = _build_backend(monkeypatch)

  events: list[dict] = []
  events += backend._translate_sse_event({
      "type": SSE_EVENT_MESSAGE_UPDATED,
      "properties": {"info": _compaction_message_info("msg_c1", completed=False)},
  })
  events += backend._translate_sse_event({
      "type": SSE_EVENT_MESSAGE_UPDATED,
      "properties": {"info": _compaction_message_info("msg_c1", completed=True)},
  })

  backend._accumulate_step_finish(_step_finish_part(999, 88, 7, 3, 4, 0.02))

  events += backend._translate_sse_event({
      "type": SSE_EVENT_MESSAGE_UPDATED,
      "properties": {"info": _compaction_message_info("msg_c2", completed=False)},
  })
  events += backend._translate_sse_event({
      "type": SSE_EVENT_MESSAGE_UPDATED,
      "properties": {"info": _compaction_message_info("msg_c2", completed=True)},
  })

  boundary_events = [e for e in events if e.get("type") == "system" and e.get("subtype") == "compact_boundary"]
  assert len(boundary_events) == 2
  assert boundary_events[0]["compact_metadata"] == {"trigger": "auto", "pre_tokens": None}
  assert boundary_events[1]["compact_metadata"] == {
      "trigger": "auto", "pre_tokens": backend._last_step_tokens["input"]}


@pytest.mark.asyncio
async def test_compaction_boundary_event_wires_into_handle_compaction_events(monkeypatch) -> None:
  """The synthesized compact_boundary event feeds handle_compaction_events and yields
  exactly one persisted ET.CONTEXT_COMPACTED event carrying the same trigger/pre_tokens."""
  backend = _build_backend(monkeypatch)

  events = backend._translate_sse_event({
      "type": SSE_EVENT_MESSAGE_UPDATED,
      "properties": {"info": _compaction_message_info("msg_wire", completed=False)},
  })
  assert len(events) == 1
  boundary_event = events[0]

  persisted: list[dict] = []

  async def _record(event: dict) -> None:
    persisted.append(event)

  await handle_compaction_events(boundary_event, _record, {"thread_id": "t1"})

  assert len(persisted) == 1
  assert persisted[0]["type"] == ET.CONTEXT_COMPACTED
  assert persisted[0]["trigger"] == boundary_event["compact_metadata"]["trigger"]
  assert persisted[0]["pre_tokens"] == boundary_event["compact_metadata"]["pre_tokens"]


# ---------------------------------------------------------------------------
# SSE silence watchdog: session progress resets the deadline; heartbeats,
# server.connected and idle time do not. Timeout is monkeypatched far below
# OPENCODE_SSE_PROGRESS_TIMEOUT so tests never wait real seconds.
# ---------------------------------------------------------------------------

_WATCHDOG_TEST_TIMEOUT = 0.2  # seconds


def _patch_watchdog_timeout(monkeypatch) -> None:
  monkeypatch.setattr(
      "src.agents.backends.opencode.OPENCODE_SSE_PROGRESS_TIMEOUT", _WATCHDOG_TEST_TIMEOUT)


async def _timed_event_stream(schedule: list[tuple[float, dict]]):
  """Yield each event after its delay, simulating SSE arrival timing."""
  for delay, event in schedule:
    await asyncio.sleep(delay)
    yield event


def _heartbeat_schedule(count: int, interval: float) -> list[tuple[float, dict]]:
  return [(interval, {"type": "server.heartbeat", "properties": {}}) for _ in range(count)]


@pytest.mark.asyncio
async def test_sse_watchdog_heartbeat_is_not_progress(monkeypatch, capsys) -> None:
  """Acceptance 1: a stream of only server.heartbeat events fails at timeout expiry —
  the watchdog tracks session progress, not bytes on the wire."""
  _patch_watchdog_timeout(monkeypatch)
  backend = _build_backend(monkeypatch)
  backend._session_id = "session-1"
  interval = 0.02  # far below the timeout: heartbeats flow the whole silent window

  with pytest.raises(OpenCodeSseSilenceError) as excinfo:
    await _drain(
        backend._with_sse_progress_watchdog(
            _timed_event_stream(_heartbeat_schedule(count=50, interval=interval))))

  message = str(excinfo.value)
  assert "no session progress" in message
  silent_seconds = float(re.search(r"for ([\d.]+) s", message).group(1))
  heartbeat_count = int(re.search(r"heartbeats during silence: (\d+)", message).group(1))
  assert silent_seconds == pytest.approx(_WATCHDOG_TEST_TIMEOUT, abs=0.1)
  assert heartbeat_count >= 1
  out = capsys.readouterr().out
  assert "opencode_sse_silence_timeout" in out
  assert "session-1" in out
  assert "heartbeat_count" in out


@pytest.mark.asyncio
async def test_sse_watchdog_resets_per_event_not_total_duration(monkeypatch) -> None:
  """Acceptance 2: session events arriving below the timeout interval keep the turn
  alive even when total stream duration exceeds the timeout — per-event reset,
  not a total-duration cap."""
  _patch_watchdog_timeout(monkeypatch)
  backend = _build_backend(monkeypatch)
  backend._session_id = "parent-session"
  interval = 0.1  # below the 0.2 s timeout; 6 events -> ~0.6 s total, far above it
  events = [
      {"type": "session.updated", "properties": {"sessionID": "parent-session"}}
      for _ in range(6)
  ]

  received = await _drain(
      backend._with_sse_progress_watchdog(
          _timed_event_stream([(interval, event) for event in events])))

  assert received == events


@pytest.mark.asyncio
async def test_sse_watchdog_child_session_event_is_progress(monkeypatch) -> None:
  """Acceptance 3: events carrying a child (subagent) session id reset the timer —
  no false kill of a parent turn waiting on subagent work. Covers both the
  top-level and the info-nested session-id shapes."""
  _patch_watchdog_timeout(monkeypatch)
  backend = _build_backend(monkeypatch)
  backend._session_id = "parent-session"
  interval = 0.1  # below the 0.2 s timeout; sequence totals ~0.4 s, above it
  events = [
      {"type": "session.updated", "properties": {"sessionID": "child-session"}},
      {"type": SSE_EVENT_MESSAGE_UPDATED,
       "properties": {"info": {"id": "m1", "role": "assistant", "sessionID": "child-session"}}},
      {"type": "session.updated", "properties": {"sessionID": "child-session"}},
      {"type": SSE_EVENT_MESSAGE_UPDATED,
       "properties": {"info": {"id": "m1", "role": "assistant", "sessionID": "child-session"}}},
  ]

  received = await _drain(
      backend._with_sse_progress_watchdog(
          _timed_event_stream([(interval, event) for event in events])))

  assert received == events


class _FakeDelayedStreamResponse:
  """SSE response whose lines arrive with per-line delays, driving the watchdog."""

  def __init__(self, lines_with_delays: list[tuple[float, str]]) -> None:
    self._lines_with_delays = lines_with_delays

  def raise_for_status(self) -> None:
    pass

  async def aiter_bytes(self):
    for delay, line in self._lines_with_delays:
      await asyncio.sleep(delay)
      yield (line + "\n").encode("utf-8")


class _FakeStreamContextManager:

  def __init__(self, response: _FakeDelayedStreamResponse) -> None:
    self._response = response

  async def __aenter__(self) -> _FakeDelayedStreamResponse:
    return self._response

  async def __aexit__(self, *exc) -> bool:
    return False


class _FakeRunHttpClient:
  """Stand-in for httpx.AsyncClient in run(): only the /event stream is real."""

  def __init__(self, response: _FakeDelayedStreamResponse) -> None:
    self._response = response

  async def __aenter__(self) -> "_FakeRunHttpClient":
    return self

  async def __aexit__(self, *exc) -> bool:
    return False

  def stream(self, method: str, path: str, timeout=None) -> _FakeStreamContextManager:
    assert path == "/event"
    return _FakeStreamContextManager(self._response)


@pytest.mark.asyncio
async def test_sse_watchdog_timeout_fails_run_end_to_end(monkeypatch, tmp_path: Path, capsys) -> None:
  """Acceptance 4: silence after server.connected yields an error event, a non-zero
  exit_code, and serve cleanup through the existing failure path."""
  _patch_watchdog_timeout(monkeypatch)
  backend = _build_backend(monkeypatch, model="provider/model")
  heartbeat_line = 'data: {"type": "server.heartbeat", "properties": {}}'
  lines = [(0.0, 'data: {"type": "server.connected", "properties": {}}'), (0.0, "")]
  for _ in range(50):
    lines.extend([(0.02, heartbeat_line), (0.0, "")])
  stream_response = _FakeDelayedStreamResponse(lines)
  process = _rig_end_to_end_run(monkeypatch, backend, stream_response)
  abort_session = AsyncMock()
  monkeypatch.setattr(backend, "_abort_session", abort_session)

  events = [event async for event in backend.run("prompt", str(tmp_path), {"PATH": "/usr/bin"})]

  error_events = [event for event in events if event.get("type") == ET.ERROR]
  assert len(error_events) == 1
  assert "no session progress" in error_events[0]["message"]
  assert "heartbeats during silence" in error_events[0]["message"]
  assert backend.exit_code == 1
  abort_session.assert_awaited_once()
  process.wait.assert_awaited()
  assert "opencode_sse_silence_timeout" in capsys.readouterr().out


@pytest.fixture(autouse=True)
def _fresh_unhandled_part_type_registry():
  """Keep the process-wide warn-once registry from leaking across tests."""
  opencode_mod._reset_unhandled_part_types_for_tests()
  yield
  opencode_mod._reset_unhandled_part_types_for_tests()


def test_unhandled_part_type_logs_once_per_process(monkeypatch) -> None:
  """60 unhandled parts of one type log one line; a second type earns one more."""
  backend = _build_backend(monkeypatch)
  logged = []
  monkeypatch.setattr(opencode_mod.log, "debug", lambda event, **kw: logged.append({"event": event, **kw}))

  for _ in range(60):
    assert backend._translate_part({"id": "p1", "messageID": "m1", "type": "patch"}) == []
  for _ in range(60):
    assert backend._translate_part({"id": "p2", "messageID": "m1", "type": "snapshot"}) == []

  lines = [line for line in logged if line["event"] == "opencode_part_unhandled"]
  assert [(line["type"]) for line in lines] == ["patch", "snapshot"]


# --- SQLite lock-retry harness: a stub `opencode serve` (fake process + fake
# HTTP/SSE endpoints; no real opencode binary) driving run() end to end. ---

_LOCK_STDERR_CHUNKS = [b"2026-09-05 ERROR database is locked (code 5)\n"]


class _StubServeProcess:
  """`opencode serve` process double: canned stdout URL line + scripted stderr chunks.

  The real ``_stream_stderr`` runs against ``stderr.read`` so the backend's
  bounded in-memory tail holds exactly ``stderr_chunks`` once the per-attempt
  cleanup drains the pipe (read() returns b"" from then on).
  """

  def __init__(self, stderr_chunks: list[bytes]) -> None:
    self.pid = 4242
    self.returncode = 0
    self._stderr_chunks = list(stderr_chunks)
    self.stdout = MagicMock()
    self.stdout.readline = AsyncMock(return_value=b"opencode server listening on http://127.0.0.1:15331\n")
    self.stdout.read = AsyncMock(return_value=b"")
    self.stderr = MagicMock()
    self.stderr.read = self._read_stderr
    self.wait = AsyncMock(return_value=0)

  async def _read_stderr(self, _size: int) -> bytes:
    return self._stderr_chunks.pop(0) if self._stderr_chunks else b""


class _StubHttpResponse:
  """Minimal httpx.Response double: status code, JSON payload, raise_for_status."""

  def __init__(self, status_code: int, payload: dict | None = None) -> None:
    self.status_code = status_code
    self._payload = payload if payload is not None else {}
    self.text = json.dumps(self._payload)

  def raise_for_status(self) -> None:
    if self.status_code < 400:
      return
    request = httpx.Request("GET", "http://127.0.0.1:15331")
    raise httpx.HTTPStatusError(
        f"stub serve returned HTTP {self.status_code}",
        request=request,
        response=httpx.Response(self.status_code, request=request))

  def json(self) -> dict:
    return self._payload


class _StubEventStreamResponse(_StubHttpResponse):
  """/event stream double: an immediate HTTP failure status, or a canned SSE event feed."""

  def __init__(self, status_code: int, sse_events: list[dict] | None = None) -> None:
    super().__init__(status_code)
    self._sse_events = sse_events or []

  async def aiter_bytes(self):
    for event in self._sse_events:
      yield ("data: " + json.dumps(event) + "\n\n").encode("utf-8")


class _StubStreamContext:
  """Async context manager handing the scripted /event response to run()."""

  def __init__(self, response: _StubEventStreamResponse) -> None:
    self._response = response

  async def __aenter__(self) -> _StubEventStreamResponse:
    return self._response

  async def __aexit__(self, *exc) -> bool:
    return False


class _StubServeScript:
  """Per-test scripting + recording shared across the retry attempts' HTTP clients."""

  def __init__(self, session_id: str) -> None:
    self.session_id = session_id
    self.event_streams: list[_StubEventStreamResponse] = []
    self.prompt_statuses: list[int] = []
    self.create_session_calls = 0
    self.prompt_posts: list[tuple[str, dict]] = []
    self.abort_posts: list[str] = []


class _StubServeHttpClient:
  """httpx.AsyncClient double over ``_StubServeScript`` (fake endpoints, no real server)."""

  def __init__(self, script: _StubServeScript) -> None:
    self._script = script

  async def __aenter__(self) -> "_StubServeHttpClient":
    return self

  async def __aexit__(self, *exc) -> bool:
    return False

  async def get(self, path: str) -> _StubHttpResponse:
    if path == "/global/health":
      return _StubHttpResponse(200)
    if path == "/config/providers":
      return _StubHttpResponse(200, {"providers": []})
    raise AssertionError(f"unexpected GET {path}")

  async def post(self, path: str, json: dict | None = None) -> _StubHttpResponse:
    script = self._script
    if path == "/session":
      script.create_session_calls += 1
      return _StubHttpResponse(200, {"id": script.session_id})
    if path.endswith("/prompt_async"):
      script.prompt_posts.append((path, json))
      status = script.prompt_statuses.pop(0) if script.prompt_statuses else 204
      return _StubHttpResponse(status)
    if path.endswith("/abort"):
      script.abort_posts.append(path)
      return _StubHttpResponse(200)
    raise AssertionError(f"unexpected POST {path}")

  def stream(self, method: str, path: str, timeout=None) -> _StubStreamContext:
    assert method == "GET" and path == "/event"
    return _StubStreamContext(self._script.event_streams.pop(0))


def _rig_stub_serve_run(
    monkeypatch,
    backend: OpenCodeBackend,
    script: _StubServeScript,
    stderr_chunks_per_attempt: list[list[bytes]],
) -> tuple[AsyncMock, list[float]]:
  """Patch spawn + httpx so run() drives the stub-serve script; return (spawn mock, sleep record).

  Attempt N spawns the Nth fake serve process (fed the Nth stderr chunk list),
  so the spawn mock's await_count is the number of attempts the run made. The
  retry backoff seam is replaced by a recorder — no test sleeps real seconds.
  """
  processes = [_StubServeProcess(chunks) for chunks in stderr_chunks_per_attempt]
  create_process = AsyncMock(side_effect=processes)
  monkeypatch.setattr(_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, create_process)
  monkeypatch.setattr("src.agents.backends.opencode.httpx.AsyncClient", lambda **kwargs: _StubServeHttpClient(script))
  sleep_calls: list[float] = []

  async def _record_sleep(seconds: float) -> None:
    sleep_calls.append(seconds)

  backend._sleep = _record_sleep
  return create_process, sleep_calls


def _sse_connected() -> dict:
  return {"type": SSE_EVENT_SERVER_CONNECTED, "properties": {}}


def _sse_session_idle(session_id: str) -> dict:
  return {"type": SSE_EVENT_SESSION_IDLE, "properties": {"sessionID": session_id}}


def _sse_session_error(session_id: str, message: str) -> dict:
  return {
      "type": SSE_EVENT_SESSION_ERROR,
      "properties": {
          "sessionID": session_id,
          "error": {
              "data": {
                  "message": message
              }
          }
      },
  }


def _sse_assistant_text(session_id: str, message_id: str, part_id: str, text: str) -> list[dict]:
  return [
      {
          "type": SSE_EVENT_MESSAGE_UPDATED,
          "properties": {
              "sessionID": session_id,
              "info": {
                  "id": message_id,
                  "role": "assistant"
              }
          },
      },
      {
          "type": SSE_EVENT_MESSAGE_PART_UPDATED,
          "properties":
              {
                  "sessionID": session_id,
                  "part": {
                      "messageID": message_id,
                      "id": part_id,
                      "type": "text",
                      "text": text
                  },
              },
      },
  ]


def _assistant_text_event(text: str) -> dict:
  return {"type": ET.ASSISTANT, "message": {"content": [{"type": "text", "text": text}]}}


def _prompt_bodies(script: _StubServeScript) -> list[str]:
  return [json.dumps(body, sort_keys=True) for _, body in script.prompt_posts]


@pytest.mark.asyncio
async def test_run_lock_failure_retries_same_session_mid_stream(monkeypatch, tmp_path: Path, capsys) -> None:
  """Lock retry 1: an attempt dying on session.error with the lock stderr signature
  retries once, resuming the SAME opencode session with a byte-identical prompt.
  The run yields attempt-1 partial events + attempt-2 events; the held
  session.error translation is never emitted."""
  sid = "ses-lock-mid"
  backend = _build_backend(monkeypatch, model="provider/model")
  script = _StubServeScript(sid)
  script.event_streams = [
      _StubEventStreamResponse(
          200, [
              _sse_connected(),
              *_sse_assistant_text(sid, "m1", "p1", "hello "),
              _sse_session_error(sid, "attempt-1 lock death"),
          ]),
      _StubEventStreamResponse(
          200, [
              _sse_connected(),
              *_sse_assistant_text(sid, "m2", "p2", "world"),
              _sse_session_idle(sid),
          ]),
  ]
  create_process, sleep_calls = _rig_stub_serve_run(monkeypatch, backend, script, [_LOCK_STDERR_CHUNKS, []])

  prompt = "fix the flaky test"
  events = [event async for event in backend.run(prompt, str(tmp_path), {"PATH": "/usr/bin"})]

  assert create_process.await_count == 2
  assert sleep_calls == [10.0]
  # Attempt 2 resumed: no second POST /session, prompt re-sent to the same id.
  assert script.create_session_calls == 1
  assert [path for path, _ in script.prompt_posts] == [f"/session/{sid}/prompt_async"] * 2
  bodies = _prompt_bodies(script)
  assert bodies[0] == bodies[1]
  assert json.loads(bodies[0])["parts"] == [{"type": "text", "text": prompt}]
  assert events == [
      {
          "session_id": sid
      },
      _assistant_text_event("hello "),
      {
          "session_id": sid
      },
      _assistant_text_event("world"),
      backend._make_accumulated_result(),
  ]
  assert backend.exit_code == 0
  out = capsys.readouterr().out
  assert "opencode_lock_retry" in out
  assert sid in out
  assert not [task for task in asyncio.all_tasks() if task is not asyncio.current_task()]


@pytest.mark.asyncio
async def test_run_lock_failure_retries_after_event_connect_500(monkeypatch, tmp_path: Path, capsys) -> None:
  """Lock retry 2 (boot death): /event connect dies with HTTP 500 + lock stderr on
  attempt 1; attempt 2 uses the resume branch (no second POST /session) and succeeds."""
  sid = "ses-lock-boot"
  backend = _build_backend(monkeypatch, model="provider/model")
  script = _StubServeScript(sid)
  script.event_streams = [
      _StubEventStreamResponse(500),
      _StubEventStreamResponse(200, [_sse_connected(), _sse_session_idle(sid)]),
  ]
  create_process, sleep_calls = _rig_stub_serve_run(
      monkeypatch, backend, script, [[b"LockTimeoutError: insert into \"project\" timed out\n"], []])

  events = [event async for event in backend.run("prompt", str(tmp_path), {"PATH": "/usr/bin"})]

  assert create_process.await_count == 2
  assert sleep_calls == [10.0]
  assert script.create_session_calls == 1
  assert [path for path, _ in script.prompt_posts] == [f"/session/{sid}/prompt_async"]
  assert events == [
      {
          "session_id": sid
      },
      {
          "session_id": sid
      },
      backend._make_accumulated_result(),
  ]
  assert backend.exit_code == 0
  assert "opencode_lock_retry" in capsys.readouterr().out


@pytest.mark.asyncio
async def test_run_lock_failure_retries_after_prompt_async_500(monkeypatch, tmp_path: Path, capsys) -> None:
  """Lock retry 3 (the production-observed case): prompt_async's boot-time HTTP 500
  with the lock stderr signature MUST retry; attempt 2 re-sends the identical
  prompt to the same session and succeeds."""
  sid = "ses-lock-prompt"
  backend = _build_backend(monkeypatch, model="provider/model")
  script = _StubServeScript(sid)
  script.event_streams = [
      _StubEventStreamResponse(200, [_sse_connected()]),
      _StubEventStreamResponse(
          200, [
              _sse_connected(),
              *_sse_assistant_text(sid, "m1", "p1", "recovered"),
              _sse_session_idle(sid),
          ]),
  ]
  script.prompt_statuses = [500]
  create_process, sleep_calls = _rig_stub_serve_run(monkeypatch, backend, script, [_LOCK_STDERR_CHUNKS, []])

  prompt = "same bytes please"
  events = [event async for event in backend.run(prompt, str(tmp_path), {"PATH": "/usr/bin"})]

  assert create_process.await_count == 2
  assert sleep_calls == [10.0]
  assert script.create_session_calls == 1
  assert [path for path, _ in script.prompt_posts] == [f"/session/{sid}/prompt_async"] * 2
  bodies = _prompt_bodies(script)
  assert bodies[0] == bodies[1]
  assert json.loads(bodies[0])["parts"] == [{"type": "text", "text": prompt}]
  assert events == [
      {
          "session_id": sid
      },
      {
          "session_id": sid
      },
      _assistant_text_event("recovered"),
      backend._make_accumulated_result(),
  ]
  assert backend.exit_code == 0
  assert "opencode_lock_retry" in capsys.readouterr().out


@pytest.mark.asyncio
async def test_run_lock_failure_exhausts_budget_with_single_error_event(monkeypatch, tmp_path: Path, capsys) -> None:
  """Lock retry 4 (budget): both attempts locked -> exactly 2 attempts, exactly one
  terminal error event (attempt 2's; attempt 1's held error is discarded), one
  recorded 10-second backoff, exit_code 1."""
  sid = "ses-lock-budget"
  backend = _build_backend(monkeypatch, model="provider/model")
  script = _StubServeScript(sid)
  script.event_streams = [
      _StubEventStreamResponse(200, [_sse_connected(), _sse_session_error(sid, "lock boom 1")]),
      _StubEventStreamResponse(200, [_sse_connected(), _sse_session_error(sid, "lock boom 2")]),
  ]
  create_process, sleep_calls = _rig_stub_serve_run(
      monkeypatch, backend, script, [_LOCK_STDERR_CHUNKS, [b"LockTimeoutError: still locked\n"]])

  events = [event async for event in backend.run("prompt", str(tmp_path), {"PATH": "/usr/bin"})]

  assert create_process.await_count == 2
  assert sleep_calls == [10.0]
  assert script.create_session_calls == 1
  assert events == [
      {
          "session_id": sid
      },
      {
          "session_id": sid
      },
      make_error_event("lock boom 2"),
  ]
  assert backend.exit_code == 1
  assert backend._failed is True
  out = capsys.readouterr().out
  assert out.count("opencode_lock_retry") == 1
  assert not [task for task in asyncio.all_tasks() if task is not asyncio.current_task()]


@pytest.mark.asyncio
async def test_run_session_error_without_lock_signature_no_retry(monkeypatch, tmp_path: Path, capsys) -> None:
  """Lock retry 5a (no signature): an unrelated session.error keeps today's
  single-attempt behaviour and the unchanged error event."""
  sid = "ses-unrelated"
  backend = _build_backend(monkeypatch, model="provider/model")
  script = _StubServeScript(sid)
  script.event_streams = [
      _StubEventStreamResponse(200, [_sse_connected(), _sse_session_error(sid, "model exploded")]),
  ]
  create_process, sleep_calls = _rig_stub_serve_run(monkeypatch, backend, script, [[b"something else broke\n"]])

  events = [event async for event in backend.run("prompt", str(tmp_path), {"PATH": "/usr/bin"})]

  assert create_process.await_count == 1
  assert sleep_calls == []
  assert events == [
      {
          "session_id": sid
      },
      make_error_event("model exploded"),
  ]
  assert backend.exit_code == 1
  assert "opencode_lock_retry" not in capsys.readouterr().out


@pytest.mark.asyncio
async def test_run_permission_ask_without_lock_signature_no_retry(monkeypatch, tmp_path: Path, capsys) -> None:
  """Lock retry 5b (no signature): permission.asked is untouched by the retry
  path — single attempt and the pre-patch error text."""
  sid = "ses-perm"
  backend = _build_backend(monkeypatch, model="provider/model")
  script = _StubServeScript(sid)
  permission_properties = {
      "id": "perm-1",
      "sessionID": sid,
      "permission": "external_directory",
      "patterns": ["/etc"],
  }
  script.event_streams = [
      _StubEventStreamResponse(
          200, [
              _sse_connected(),
              {
                  "type": SSE_EVENT_PERMISSION_ASKED,
                  "properties": permission_properties
              },
          ]),
  ]
  create_process, sleep_calls = _rig_stub_serve_run(monkeypatch, backend, script, [[b"plain logs\n"]])

  events = [event async for event in backend.run("prompt", str(tmp_path), {"PATH": "/usr/bin"})]

  assert create_process.await_count == 1
  assert sleep_calls == []
  assert events == [
      {
          "session_id": sid
      },
      make_error_event(backend._format_permission_error(permission_properties)),
  ]
  assert backend.exit_code == 1
  assert "opencode_lock_retry" not in capsys.readouterr().out


@pytest.mark.asyncio
async def test_run_http_failure_without_lock_signature_no_retry(monkeypatch, tmp_path: Path, capsys) -> None:
  """Lock retry 5c (no signature): a plain HTTP failure stays a single attempt
  with the pre-patch 'OpenCode backend failed' error text."""
  sid = "ses-http"
  backend = _build_backend(monkeypatch, model="provider/model")
  script = _StubServeScript(sid)
  script.event_streams = [_StubEventStreamResponse(500)]
  create_process, sleep_calls = _rig_stub_serve_run(monkeypatch, backend, script, [[b"unrelated crash\n"]])

  events = [event async for event in backend.run("prompt", str(tmp_path), {"PATH": "/usr/bin"})]

  assert create_process.await_count == 1
  assert sleep_calls == []
  assert len(events) == 2
  assert events[0] == {"session_id": sid}
  assert events[1]["type"] == ET.ERROR
  assert events[1]["message"].startswith("OpenCode backend failed: stub serve returned HTTP 500")
  assert backend.exit_code == 1
  assert "opencode_lock_retry" not in capsys.readouterr().out


@pytest.mark.asyncio
async def test_run_lock_failure_never_retries_after_terminate(monkeypatch, tmp_path: Path, capsys) -> None:
  """Lock retry 6 (cancellation): terminate() never retries, even with the lock
  signature present; the held error is emitted on that single attempt."""
  sid = "ses-cancel"
  backend = _build_backend(monkeypatch, model="provider/model")
  backend.terminated = True
  script = _StubServeScript(sid)
  script.event_streams = [
      _StubEventStreamResponse(200, [_sse_connected(), _sse_session_error(sid, "lock boom")]),
  ]
  create_process, sleep_calls = _rig_stub_serve_run(monkeypatch, backend, script, [_LOCK_STDERR_CHUNKS])

  events = [event async for event in backend.run("prompt", str(tmp_path), {"PATH": "/usr/bin"})]

  assert create_process.await_count == 1
  assert sleep_calls == []
  assert events == [
      {
          "session_id": sid
      },
      make_error_event("lock boom"),
  ]
  assert "opencode_lock_retry" not in capsys.readouterr().out
