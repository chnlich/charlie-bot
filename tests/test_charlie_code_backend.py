from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock, MagicMock

import pytest
from conftest import (
    BASE_SPAWN_SUBPROCESS_PATCH_TARGET,
    CHARLIE_CODE_RESOLVE_BINARY_PATCH_TARGET,
    FLAG_LIKE_PROMPT,
    RUNS_READ_PID_STAT_PATCH_TARGET,
    build_cli_backend_rig,
)

from src.agents.backends.charlie_code import CharlieCodeBackend
from src.core import event_types as ET


def _build_backend(monkeypatch: pytest.MonkeyPatch, **kwargs: Any) -> CharlieCodeBackend:
  return build_cli_backend_rig(monkeypatch, CharlieCodeBackend, **kwargs)


def test_translate_success_stream_preserves_tool_pair_ids_and_usage(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(monkeypatch)

  command = backend.translate_event({
      "type": "command",
      "step": 1,
      "id": "s-1",
      "command": "pwd",
  })
  assert command == [{
      "type": ET.TOOL_USE,
      "name": "Bash",
      "input": {
          "command": "pwd"
      },
      "id": "s-1",
  }]

  observation = backend.translate_event(
      {
          "type": "observation",
          "step": 1,
          "id": "s-1",
          "returncode": 0,
          "output": "/tmp/worktree\n",
      })
  assert observation == [
      {
          "type": ET.TOOL_RESULT,
          "tool_name": "Bash",
          "content": "/tmp/worktree\n",
          "tool_use_id": "s-1",
      }
  ]

  result = backend.translate_event(
      {
          "type": "result",
          "completed": True,
          "n_steps": 1,
          "usage": {
              "n_calls": 2,
              "input_tokens": 123,
              "output_tokens": 45,
          },
      })
  assert result == [
      {
          "type": ET.RESULT,
          "result": "",
          "usage":
              {
                  "input_tokens": 123,
                  "output_tokens": 45,
                  "cache_read_input_tokens": 0,
                  "cache_creation_input_tokens": 0,
              },
          "total_cost_usd": None,
      }
  ]


def test_translate_failure_stream_preserves_error_message(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(monkeypatch)

  translated = backend.translate_event({
      "type": "error",
      "message": "rate limit: retry later",
  })

  assert translated == [
      {
          "type": ET.ERROR,
          "message": "rate limit: retry later",
          "content": "rate limit: retry later",
      }
  ]


def test_build_command_writes_task_file_and_flags(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch, instructions_content="Use concise answers.")
  session_cwd = tmp_path / "cwd"
  session_cwd.mkdir()
  backend._prepare_cwd(str(session_cwd))
  backend._prepare_transport(tmp_path)

  cmd = backend._build_command(FLAG_LIKE_PROMPT)

  assert cmd[:6] == [
      "/usr/bin/charlie-code",
      "--json",
      "--model",
      "charlie-code-test-model",
      "--api-base",
      "http://test.invalid/v1",
  ]
  assert cmd[-2:] == ["--task-file", str(tmp_path / "task.md")]
  assert "--" not in cmd
  assert "--resume" not in cmd
  assert "--context-window" not in cmd
  # Master instructions ride the cwd AGENTS.md system channel, byte-identical
  # to the assembled instructions string.
  agents_md = session_cwd / "AGENTS.md"
  assert agents_md.read_bytes() == b"Use concise answers."
  # task.md carries the bare prompt: no <system-instructions> frame anywhere.
  task_md = tmp_path / "task.md"
  assert task_md.read_bytes() == FLAG_LIKE_PROMPT.encode("utf-8")
  assert b"<system-instructions>" not in task_md.read_bytes()
  # The task text never rides argv.
  assert not any(FLAG_LIKE_PROMPT in arg for arg in cmd)


# ---------------------------------------------------------------------------
# registry wiring
# ---------------------------------------------------------------------------


def test_api_base_required(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(
      CHARLIE_CODE_RESOLVE_BINARY_PATCH_TARGET,
      lambda name, fallback: "/usr/bin/charlie-code",
  )

  with pytest.raises(ValueError, match="api_base"):
    CharlieCodeBackend(model="charlie-code-test-model")


# ---------------------------------------------------------------------------
# Image attachments: refusal on image_input: false, --image command assembly.
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_run_refuses_images_with_image_input_false(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """image_input: false + an image ref: exactly one error event, no spawn, no result."""
  backend = _build_backend(monkeypatch, image_input=False, log_dir=tmp_path / "logs")
  monkeypatch.setattr(RUNS_READ_PID_STAT_PATCH_TARGET, lambda pid: ("refusal-test-start", "R"))
  process = MagicMock()
  process.pid = 4242
  # A locally built AsyncMock (per the conftest stub's own docstring) so the test holds
  # the call reference `await_args` reads.
  spawn = AsyncMock(return_value=process)
  monkeypatch.setattr(BASE_SPAWN_SUBPROCESS_PATCH_TARGET, spawn)

  events = [
      event async for event in backend.run(
          "what is this error",
          str(tmp_path), {"PATH": "/usr/bin:/bin"},
          uploaded_files=[{
              "filename": "error-shot.png",
              "path": str(tmp_path / "error-shot.png"),
          }])
  ]

  expected = (
      "refused: image attachments not sent — this endpoint declares no image input "
      "(image_input: false): error-shot.png")
  assert events == [{"type": ET.ERROR, "message": expected, "content": expected}]
  # Nothing is sent: no subprocess spawn and no result event.
  assert spawn.await_count == 0
