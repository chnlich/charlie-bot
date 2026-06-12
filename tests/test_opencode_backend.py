import json
from pathlib import Path

import pytest

from src.agents.backends.opencode import OpenCodeBackend


def _build_backend(monkeypatch, **kwargs) -> OpenCodeBackend:
  monkeypatch.setattr(
      "src.agents.backends.opencode.resolve_binary",
      lambda name, fallback: "/usr/bin/opencode",
  )
  return OpenCodeBackend(**kwargs)


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


def test_prepare_cwd_writes_project_opencode_json(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)

  backend._prepare_cwd(str(tmp_path))

  project_config = tmp_path / ".opencode" / "opencode.json"
  assert project_config.exists()

  data = json.loads(project_config.read_text(encoding="utf-8"))
  permission = data["agent"]["build"]["permission"]
  for tool in ("external_directory", "read", "edit", "bash", "glob", "grep", "list", "write", "skill"):
    assert permission[tool] == {"*": "allow"}
  assert permission["task"] == {"*": "deny"}


def test_prepare_cwd_migrates_legacy_config_json(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)
  legacy_dir = tmp_path / ".opencode"
  legacy_dir.mkdir()
  legacy_config = legacy_dir / "config.json"
  legacy_payload = {"agent": {"build": {"permission": {"glob": {"*": "allow"}}}}}
  legacy_config.write_text(json.dumps(legacy_payload), encoding="utf-8")

  backend._prepare_cwd(str(tmp_path))

  project_config = legacy_dir / "opencode.json"
  assert project_config.exists()
  assert json.loads(project_config.read_text(encoding="utf-8")) == legacy_payload
  assert json.loads(legacy_config.read_text(encoding="utf-8")) == legacy_payload


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
