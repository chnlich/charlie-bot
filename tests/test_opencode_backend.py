import asyncio
import json
import pathlib
from collections.abc import AsyncIterator
from typing import Any, Self
from unittest import mock

import conftest
import httpx
import pytest

from src.backends.opencode import opencode
from src.infra import event_types as ET
from src.runtime.agent_process import base

# The opencode backend's httpx seam: the module imports httpx at top level, so the
# string target resolves through its `httpx` global onto the shared httpx module,
# and the stand-in lands on httpx.AsyncClient where the backend's call sites read it.
_OPENCODE_HTTPX_ASYNC_CLIENT_PATCH_TARGET = "src.backends.opencode.opencode.httpx.AsyncClient"


def _build_backend(monkeypatch: pytest.MonkeyPatch, **kwargs: Any) -> opencode.OpenCodeBackend:
  return conftest.build_cli_backend_rig(monkeypatch, opencode.OpenCodeBackend, **kwargs)


def _rig_end_to_end_run(
    monkeypatch: pytest.MonkeyPatch,
    backend: opencode.OpenCodeBackend,
    response: _FakeDelayedStreamResponse | conftest.FakeChunkedResponse,
) -> mock.MagicMock:
  """Mock the serve-and-connect path so backend.run() consumes `response` as the
  /event stream end-to-end; returns the spawned process mock for spawn assertions."""
  process = conftest.stub_subprocess_spawn(monkeypatch, conftest.OPENCODE_SPAWN_SUBPROCESS_PATCH_TARGET, 4321)
  process.returncode = 0
  process.wait = mock.AsyncMock(return_value=0)
  monkeypatch.setattr(backend, "_read_server_url", mock.AsyncMock(return_value="http://127.0.0.1:4242"))
  monkeypatch.setattr(backend, "_stream_stderr", mock.AsyncMock())
  monkeypatch.setattr(backend, "_stream_stdout", mock.AsyncMock())
  monkeypatch.setattr(backend, "_check_health", mock.AsyncMock())
  monkeypatch.setattr(backend, "_fetch_model_limit", mock.AsyncMock(return_value=None))
  monkeypatch.setattr(backend, "_create_session", mock.AsyncMock(return_value="session-1"))
  monkeypatch.setattr(backend, "_send_prompt", mock.AsyncMock())
  monkeypatch.setattr(_OPENCODE_HTTPX_ASYNC_CLIENT_PATCH_TARGET, lambda **kwargs: _FakeRunHttpClient(response))
  return process


class _FakeEventStream:
  """Async-iterable of pre-baked SSE event dicts for _consume_sse_events tests."""

  def __init__(self, events: list[dict]) -> None:
    self._events = events

  async def __aiter__(self) -> AsyncIterator[dict]:
    for event in self._events:
      yield event


def _message_updated(info: dict, session_id: str | None = None) -> dict:
  properties: dict = {"info": info}
  if session_id is not None:
    properties["sessionID"] = session_id
  return {"type": opencode.SSE_EVENT_MESSAGE_UPDATED, "properties": properties}


def _part_updated(part: dict, session_id: str | None = None) -> dict:
  properties: dict = {"part": part}
  if session_id is not None:
    properties["sessionID"] = session_id
  return {"type": opencode.SSE_EVENT_MESSAGE_PART_UPDATED, "properties": properties}


def _text_part(message_id: str, part_id: str, part_type: str, text: str) -> dict:
  """The text/reasoning part shape opencode streams inside part.updated."""
  return {"messageID": message_id, "id": part_id, "type": part_type, "text": text}


def _session_attached(session_id: str) -> dict:
  """The translated run-start adopt signal: run() opens every stream with it."""
  return {"type": ET.SESSION_ATTACHED, "session_id": session_id}


@pytest.mark.asyncio
async def test_run_opens_with_the_typed_session_attach_signal(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  """The run's first event is the typed adoption signal carrying the attached
  session id — never a bare session_id dict the persist funnels would write."""
  backend = _build_backend(monkeypatch, model="provider/model")
  chunks = [
      b'data: {"type": "server.connected", "properties": {}}\n',
      b"\n",
      b'data: {"type": "session.idle", "properties": {"sessionID": "session-1"}}\n',
      b"\n",
  ]
  _rig_end_to_end_run(monkeypatch, backend, conftest.FakeChunkedResponse(chunks))

  events = [event async for event in backend.run("prompt", str(tmp_path), {"PATH": "/usr/bin"})]

  assert events[0] == _session_attached("session-1")
  assert backend.exit_code == 0


def test_translate_sse_event_buffers_part_until_message_role_known(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(monkeypatch)

  assert not backend._translate_sse_event(_part_updated(_text_part("message-1", "part-1", "text", "Hello")))
  assert not backend._translate_sse_event(_part_updated(_text_part("message-1", "part-1", "text", "Hello world")))

  translated = backend._translate_sse_event(_message_updated({"id": "message-1", "role": "assistant"}))

  assert translated == [conftest.assistant_text_event("Hello"), conftest.assistant_text_event(" world")]


async def _drain(events: AsyncIterator[dict]) -> list[dict]:
  return [event async for event in events]


@pytest.mark.asyncio
async def test_consume_sse_events_parent_permission_ask_fails_fast(monkeypatch: pytest.MonkeyPatch) -> None:
  """A permission ask for the parent session is a terminal error that ends the turn."""
  backend = _build_backend(monkeypatch)
  backend._session_id = "parent-session"

  events = await _drain(
      backend._consume_sse_events(
          _FakeEventStream(
              [
                  {
                      "type": opencode.SSE_EVENT_PERMISSION_ASKED,
                      "properties":
                          {
                              "id": "perm-1",
                              "sessionID": "parent-session",
                              "permission": "external_directory",
                              "patterns": ["/etc"],
                          },
                  },
                  {
                      "type": opencode.SSE_EVENT_SESSION_IDLE,
                      "properties": {
                          "sessionID": "parent-session"
                      },
                  },
              ])))

  assert len(events) == 1
  assert events[0]["type"] == ET.ERROR
  assert "external_directory" in events[0]["message"]
  assert "/etc" in events[0]["message"]
  assert "parent-session" in events[0]["message"]
  assert backend._failed is True


# ---------------------------------------------------------------------------
# context_snapshot: last step's tokens, reasoning, model limit, catalog failure
# ---------------------------------------------------------------------------


def _step_finish_part(
    input_t: int, output_t: int, reasoning_t: int, cache_read_t: int, cache_write_t: int, cost: float) -> dict:
  return {
      "messageID": "m1",
      "id": "p1",
      "type": "step-finish",
      "tokens":
          {
              "input": input_t,
              "output": output_t,
              "reasoning": reasoning_t,
              "cache": {
                  "read": cache_read_t,
                  "write": cache_write_t
              },
          },
      "cost": cost,
  }


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
    "cache": {
        "write": 0,
        "read": 0
    },
}


def _compaction_message_info(message_id: str, *, completed: bool) -> dict:
  """Recorded compaction message shape: first delivery is all-zero tokens with no
  time.completed; the later delivery carries the real tokens once the step finishes."""
  tokens = _COMPACTION_TOKENS if completed else {
      "total": 0,
      "input": 0,
      "output": 0,
      "reasoning": 0,
      "cache": {
          "write": 0,
          "read": 0
      }
  }
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


def test_compaction_boundary_emitted_exactly_once_per_message(monkeypatch: pytest.MonkeyPatch) -> None:
  """Two summary messages, each message.updated delivered twice, yield exactly two
  compact_boundary events; pre_tokens reflects the _last_step_tokens snapshot in effect
  at registration (None before any step has completed)."""
  backend = _build_backend(monkeypatch)

  events: list[dict] = []
  events += backend._translate_sse_event(_message_updated(_compaction_message_info("msg_c1", completed=False)))
  events += backend._translate_sse_event(_message_updated(_compaction_message_info("msg_c1", completed=True)))

  backend._accumulate_step_finish(_step_finish_part(999, 88, 7, 3, 4, 0.02))

  events += backend._translate_sse_event(_message_updated(_compaction_message_info("msg_c2", completed=False)))
  events += backend._translate_sse_event(_message_updated(_compaction_message_info("msg_c2", completed=True)))

  boundary_events = [e for e in events if e.get("type") == "system" and e.get("subtype") == "compact_boundary"]
  assert len(boundary_events) == 2
  assert boundary_events[0]["compact_metadata"] == {"trigger": "auto", "pre_tokens": None}
  assert boundary_events[1]["compact_metadata"] == {"trigger": "auto", "pre_tokens": backend._last_step_tokens["input"]}


# ---------------------------------------------------------------------------
# SSE silence watchdog: session progress resets the deadline; heartbeats,
# server.connected and idle time do not. Timeout is monkeypatched far below
# OPENCODE_SSE_PROGRESS_TIMEOUT so tests never wait real seconds.
# ---------------------------------------------------------------------------

_WATCHDOG_TEST_TIMEOUT = 0.2  # seconds


def _patch_watchdog_timeout(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr("src.backends.opencode.opencode_limits.OPENCODE_SSE_PROGRESS_TIMEOUT", _WATCHDOG_TEST_TIMEOUT)


class _FakeDelayedStreamResponse:
  """SSE response whose lines arrive with per-line delays, driving the watchdog."""

  def __init__(self, lines_with_delays: list[tuple[float, str]]) -> None:
    self._lines_with_delays = lines_with_delays

  def raise_for_status(self) -> None:
    pass

  async def aiter_bytes(self) -> AsyncIterator[bytes]:
    for delay, line in self._lines_with_delays:
      await asyncio.sleep(delay)
      yield (line + "\n").encode("utf-8")


class _ClientContextDouble:
  """Async-context boilerplate shared by the file's httpx client doubles.

  Entering yields the double itself; exiting never suppresses the wrapped
  exception. A double that hands back a scripted response overrides
  ``__aenter__`` with its own typed return and inherits only ``__aexit__``.
  """

  async def __aenter__(self) -> Self:
    return self

  async def __aexit__(self, *exc: object) -> bool:
    return False


class _FakeStreamContextManager(_ClientContextDouble):

  def __init__(self, response: _FakeDelayedStreamResponse) -> None:
    self._response = response

  async def __aenter__(self) -> _FakeDelayedStreamResponse:
    return self._response


class _FakeRunHttpClient(_ClientContextDouble):
  """Stand-in for httpx.AsyncClient in run(): only the /event stream is real."""

  def __init__(self, response: _FakeDelayedStreamResponse) -> None:
    self._response = response

  def stream(self, method: str, path: str, timeout: float | None = None) -> _FakeStreamContextManager:
    assert path == "/event"
    return _FakeStreamContextManager(self._response)


@pytest.mark.asyncio
async def test_sse_watchdog_timeout_fails_run_end_to_end(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
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
  abort_session = mock.AsyncMock()
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


def _clear_unhandled_part_registries() -> None:
  """Keep the process-wide warn-once registries from leaking across tests."""
  opencode._UNHANDLED_PART_TYPES.clear()
  opencode._UNHANDLED_SSE_EVENT_TYPES.clear()


_fresh_unhandled_part_type_registry = conftest.fresh_state_fixture(_clear_unhandled_part_registries)

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
    self.stdout = mock.MagicMock()
    self.stdout.readline = mock.AsyncMock(return_value=b"opencode server listening on http://127.0.0.1:15331\n")
    self.stdout.read = mock.AsyncMock(return_value=b"")
    self.stderr = mock.MagicMock()
    self.stderr.read = self._read_stderr
    self.wait = mock.AsyncMock(return_value=0)

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

  async def aiter_bytes(self) -> AsyncIterator[bytes]:
    for event in self._sse_events:
      yield ("data: " + json.dumps(event) + "\n\n").encode("utf-8")


class _StubStreamContext(_ClientContextDouble):
  """Async context manager handing the scripted /event response to run()."""

  def __init__(self, response: _StubEventStreamResponse) -> None:
    self._response = response

  async def __aenter__(self) -> _StubEventStreamResponse:
    return self._response


class _StubServeScript:
  """Per-test scripting + recording shared across the retry attempts' HTTP clients."""

  def __init__(self, session_id: str) -> None:
    self.session_id = session_id
    self.event_streams: list[_StubEventStreamResponse] = []
    self.prompt_statuses: list[int] = []
    self.create_session_calls = 0
    self.prompt_posts: list[tuple[str, dict]] = []
    self.abort_posts: list[str] = []


class _StubServeHttpClient(_ClientContextDouble):
  """httpx.AsyncClient double over ``_StubServeScript`` (fake endpoints, no real server)."""

  def __init__(self, script: _StubServeScript) -> None:
    self._script = script

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

  def stream(self, method: str, path: str, timeout: float | None = None) -> _StubStreamContext:
    assert method == "GET" and path == "/event"
    return _StubStreamContext(self._script.event_streams.pop(0))


def _rig_stub_serve_run(
    monkeypatch: pytest.MonkeyPatch,
    backend: opencode.OpenCodeBackend,
    script: _StubServeScript,
    stderr_chunks_per_attempt: list[list[bytes]],
) -> tuple[mock.AsyncMock, list[float]]:
  """Patch spawn + httpx so run() drives the stub-serve script; return (spawn mock, sleep record).

  Attempt N spawns the Nth fake serve process (fed the Nth stderr chunk list),
  so the spawn mock's await_count is the number of attempts the run made. The
  retry backoff seam is replaced by a recorder — no test sleeps real seconds.
  """
  processes = [_StubServeProcess(chunks) for chunks in stderr_chunks_per_attempt]
  create_process = mock.AsyncMock(side_effect=processes)
  monkeypatch.setattr(conftest.OPENCODE_SPAWN_SUBPROCESS_PATCH_TARGET, create_process)
  monkeypatch.setattr(_OPENCODE_HTTPX_ASYNC_CLIENT_PATCH_TARGET, lambda **kwargs: _StubServeHttpClient(script))
  sleep_calls: list[float] = []

  async def _record_sleep(seconds: float) -> None:
    sleep_calls.append(seconds)

  backend._sleep = _record_sleep
  return create_process, sleep_calls


def _sse_connected() -> dict:
  return {"type": opencode.SSE_EVENT_SERVER_CONNECTED, "properties": {}}


def _sse_session_idle(session_id: str) -> dict:
  return {"type": opencode.SSE_EVENT_SESSION_IDLE, "properties": {"sessionID": session_id}}


def _sse_session_error(session_id: str, message: str) -> dict:
  return {
      "type": opencode.SSE_EVENT_SESSION_ERROR,
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
      _message_updated({
          "id": message_id,
          "role": "assistant"
      }, session_id=session_id),
      _part_updated(_text_part(message_id, part_id, "text", text), session_id=session_id),
  ]


def _prompt_bodies(script: _StubServeScript) -> list[str]:
  return [json.dumps(body, sort_keys=True) for _, body in script.prompt_posts]


def _assert_resumed_same_session(script: _StubServeScript, sid: str, prompt: str) -> None:
  """Attempt 2 resumed the SAME opencode session: no second POST /session, and the
  prompt re-sent to the recorded session id is byte-identical to attempt 1's."""
  assert script.create_session_calls == 1
  assert [path for path, _ in script.prompt_posts] == [f"/session/{sid}/prompt_async"] * 2
  bodies = _prompt_bodies(script)
  assert bodies[0] == bodies[1]
  assert json.loads(bodies[0])["parts"] == [{"type": "text", "text": prompt}]


@pytest.mark.asyncio
async def test_run_lock_failure_retries_same_session_mid_stream(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
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
  _assert_resumed_same_session(script, sid, prompt)
  assert events == [
      _session_attached(sid),
      base.make_text_event("hello "),
      _session_attached(sid),
      base.make_text_event("world"),
      backend._make_accumulated_result(),
  ]
  assert backend.exit_code == 0
  out = capsys.readouterr().out
  assert "opencode_lock_retry" in out
  assert sid in out
  assert not [task for task in asyncio.all_tasks() if task is not asyncio.current_task()]
