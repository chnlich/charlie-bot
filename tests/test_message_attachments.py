from __future__ import annotations

import json
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

import pytest

from src.api.chat import send_message
from src.api.message_utils import events_to_messages
from src.api.slash import SlashExecuteRequest, execute_command
from src.core.config import CharlieBotConfig
from src.core.models import SendMessageRequest, SessionMetadata, UploadedFileRef
from src.core.slash_commands import SlashDispatchResult


def _close_scheduled_task(coro) -> None:
  coro.close()


def test_events_to_messages_uses_structured_uploaded_files_without_leaking_paths() -> None:
  messages = events_to_messages([
      {
          "type": "user",
          "content": "Please review these notes",
          "uploaded_files": [
              {
                  "filename": "notes.txt",
                  "path": "/tmp/notes.txt",
                  "size": 12,
              },
          ],
          "timestamp": "2026-04-02T10:00:00Z",
      },
  ])

  assert messages == [
      {
          "role": "user",
          "content": "Please review these notes",
          "uploaded_files": [
              {
                  "filename": "notes.txt",
                  "path": "/tmp/notes.txt",
                  "size": 12,
              },
          ],
          "is_voice": False,
          "event_index": 0,
          "id": "legacy:0",
          "timestamp": "2026-04-02T10:00:00Z",
      },
  ]


def test_events_to_messages_extracts_legacy_attachment_block() -> None:
  messages = events_to_messages([
      {
          "type": "user",
          "content": "Please review\n\n[Attached files]\n- /tmp/alpha.txt\n- /tmp/beta.md",
          "timestamp": "2026-04-02T10:00:00Z",
      },
      {
          "type": "user",
          "content": "\n\n[Attached files]\n- /tmp/file-only.pdf",
          "timestamp": "2026-04-02T10:01:00Z",
      },
  ])

  assert messages[0]["content"] == "Please review"
  assert messages[0]["uploaded_files"] == [
      {"filename": "alpha.txt", "path": "/tmp/alpha.txt"},
      {"filename": "beta.md", "path": "/tmp/beta.md"},
  ]
  assert messages[1]["content"] == ""
  assert messages[1]["uploaded_files"] == [
      {"filename": "file-only.pdf", "path": "/tmp/file-only.pdf"},
  ]


@pytest.mark.asyncio
async def test_send_message_passes_structured_files_to_run_and_finalize(tmp_path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  meta = SessionMetadata(name="Test Session")
  session_mgr = AsyncMock()
  req = SendMessageRequest(
      content="Summarize this",
      uploaded_files=[
          UploadedFileRef(filename="notes.txt", path="/tmp/notes.txt", size=12),
      ],
  )

  with (
      patch("src.api.chat.run_and_finalize", new=AsyncMock()) as mock_run,
      patch("src.api.chat.create_logged_task", side_effect=_close_scheduled_task),
  ):
    response = await send_message(
        meta.id,
        req,
        meta=meta,
        session_mgr=session_mgr,
        cfg=cfg,
    )

  assert response.status_code == 202
  assert mock_run.call_count == 1
  assert mock_run.call_args.args[2] == "Summarize this\n\n[Attached files]\n- /tmp/notes.txt"
  assert mock_run.call_args.kwargs["display_content"] == "Summarize this"
  assert mock_run.call_args.kwargs["uploaded_files"] == [
      {"filename": "notes.txt", "path": "/tmp/notes.txt", "size": 12},
  ]


@pytest.mark.asyncio
async def test_execute_command_persists_uploaded_files_for_prompt_dispatch(tmp_path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  meta = SessionMetadata(name="Slash Session")
  session_mgr = AsyncMock()
  request = SimpleNamespace(app=SimpleNamespace(state=SimpleNamespace()))
  req = SlashExecuteRequest(
      command="review",
      args="please",
      uploaded_files=[
          UploadedFileRef(filename="report.pdf", path="/tmp/report.pdf", size=99),
      ],
  )
  dispatch = SlashDispatchResult(kind="prompt", substituted_prompt="Read the attachment")

  with (
      patch("src.api.slash.dispatch_slash_command", new=AsyncMock(return_value=dispatch)),
      patch("src.api.slash.run_and_finalize", new=AsyncMock()) as mock_run,
      patch("src.api.slash.create_logged_task", side_effect=_close_scheduled_task),
  ):
    response = await execute_command(
        request=request,
        session_id=meta.id,
        req=req,
        meta=meta,
        session_mgr=session_mgr,
        cfg=cfg,
    )

  assert response.status_code == 202
  session_mgr.persist_and_broadcast.assert_awaited_once()
  persisted_event = session_mgr.persist_and_broadcast.await_args.args[1]
  assert persisted_event["content"] == "/review please"
  assert persisted_event["uploaded_files"] == [
      {"filename": "report.pdf", "path": "/tmp/report.pdf", "size": 99},
  ]

  assert mock_run.call_count == 1
  assert mock_run.call_args.args[2] == "Read the attachment\n\n[Attached files]\n- /tmp/report.pdf"
  assert mock_run.call_args.kwargs["skip_user_event"] is True
  assert mock_run.call_args.kwargs["display_content"] == "/review please"
  assert mock_run.call_args.kwargs["uploaded_files"] == [
      {"filename": "report.pdf", "path": "/tmp/report.pdf", "size": 99},
  ]

