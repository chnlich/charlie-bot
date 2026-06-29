import json
from pathlib import Path

import httpx
import pytest

from src.agents.backends.opencode import OpenCodeBackend
from src.core import event_types as ET


def _build_backend(monkeypatch, **kwargs) -> OpenCodeBackend:
  monkeypatch.setattr(
      "src.agents.backends.opencode.resolve_binary",
      lambda name, fallback: "/usr/bin/opencode",
  )
  return OpenCodeBackend(**kwargs)


class _FakeEventStream:
  """Async-iterable of pre-baked SSE event dicts for _consume_sse_events tests."""

  def __init__(self, events: list[dict]) -> None:
    self._events = events

  async def __aiter__(self):
    for event in self._events:
      yield event


class _FakeSseResponse:

  def __init__(self, lines: list[str]) -> None:
    self._lines = lines

  async def aiter_lines(self):
    for line in self._lines:
      yield line


@pytest.mark.asyncio
async def test_iter_sse_events_ignores_comments_and_metadata(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)
  response = _FakeSseResponse(
      [
          ": keepalive",
          "event: message.updated",
          "id: event-1",
          "retry: 1000",
          "data: {\"type\": \"server.connected\"}",
          "",
          ": another keepalive",
          "event: session.idle",
          "x-extra: ignored",
          "data: {\"type\": \"session.idle\",",
          "data: \"properties\": {\"sessionID\": \"session-1\"}}",
          "",
      ])

  events = [event async for event in backend._iter_sse_events(response)]

  assert events == [
      {
          "type": "server.connected"
      },
      {
          "type": "session.idle",
          "properties": {
              "sessionID": "session-1"
          }
      },
  ]


def test_translate_sse_event_buffers_part_until_message_role_known(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  assert backend._translate_sse_event(
      {
          "type": "message.part.updated",
          "properties": {
              "part": {
                  "messageID": "message-1",
                  "id": "part-1",
                  "type": "text",
                  "text": "Hello",
              }
          },
      }) == []
  assert backend._translate_sse_event(
      {
          "type": "message.part.updated",
          "properties": {
              "part": {
                  "messageID": "message-1",
                  "id": "part-1",
                  "type": "text",
                  "text": "Hello world",
              }
          },
      }) == []

  translated = backend._translate_sse_event(
      {
          "type": "message.updated",
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

  assert backend._translate_sse_event(
      {
          "type": "message.part.updated",
          "properties": {
              "part": {
                  "messageID": "message-1",
                  "id": "part-1",
                  "type": "text",
                  "text": "Hello",
              }
          },
      }) == []

  translated = backend._translate_sse_event(
      {
          "type": "message.updated",
          "properties": {
              "info": {
                  "id": "message-1",
                  "role": "user",
              }
          },
      })

  assert translated == []
  assert backend._pending_parts == {}


def test_prepare_env_sets_charliebot_opencode_config(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  env = backend._prepare_env({"PATH": "/usr/bin"})

  data = json.loads(env["OPENCODE_CONFIG_CONTENT"])
  assert data["default_agent"] == "charliebot"
  assert data["agent"]["charliebot"]["permission"] == {"*": "allow"}
  # The internal task tool is disabled so hidden explore subagents cannot run headless.
  assert data["agent"]["charliebot"]["tools"] == {"task": False}


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
                  "type": "permission.asked",
                  "properties": {
                      "id": "perm-1",
                      "sessionID": "parent-session",
                      "permission": "external_directory",
                      "patterns": ["/etc"],
                  },
              },
              {
                  "type": "session.idle",
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
              "type": "permission.asked",
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
                  "type": "message.part.updated",
                  "properties": {"sessionID": "other", "part": {"messageID": "m9", "id": "p9", "type": "text",
                                                                "text": "ignored"}},
              },
              {
                  "type": "message.updated",
                  "properties": {"sessionID": "parent-session", "info": {"id": "m1", "role": "assistant"}},
              },
              {
                  "type": "message.part.updated",
                  "properties": {"sessionID": "parent-session",
                                 "part": {"messageID": "m1", "id": "p1", "type": "text", "text": "Hello"}},
              },
              {
                  "type": "session.idle",
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

  assert backend._translate_sse_event({
      "type": "message.part.updated",
      "properties": {
          "part": {
              "messageID": "message-1",
              "id": "part-r",
              "type": "reasoning",
              "text": "I need",
          }
      },
  }) == []
  assert backend._translate_sse_event({
      "type": "message.part.updated",
      "properties": {
          "part": {
              "messageID": "message-1",
              "id": "part-r",
              "type": "reasoning",
              "text": "I need to think",
          }
      },
  }) == []

  translated = backend._translate_sse_event({
      "type": "message.updated",
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
