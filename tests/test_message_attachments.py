from __future__ import annotations

import io
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
  CHAT_CREATE_LOGGED_TASK_PATCH_TARGET,
  CHAT_RUN_AND_FINALIZE_PATCH_TARGET,
  close_create_logged_task,
  make_home_config,
)
from fastapi import UploadFile

from src.api.chat import send_message, upload_file
from src.api.message_utils import events_to_messages
from src.api.slash import SlashExecuteRequest, execute_command
from src.core.models import SendMessageRequest, SessionMetadata, UploadedFileRef
from src.core.slash_commands import SlashDispatchResult

VOICE_KEY = "is_" + "voice"


@pytest.mark.asyncio
async def test_upload_file_strips_directory_components(tmp_path) -> None:
  cfg = make_home_config(tmp_path)
  meta = SessionMetadata(name="Upload Session")
  outside_path = cfg.sessions_dir / "evil.txt"
  outside_path.parent.mkdir(parents=True)
  outside_path.write_text("do not overwrite", encoding="utf-8")
  upload = UploadFile(file=io.BytesIO(b"safe contents"), filename="../../evil.txt")

  response = await upload_file(
      meta.id,
      upload,
      _meta=meta,
      cfg=cfg,
  )

  stored_path = cfg.sessions_dir / meta.id / "uploads" / "evil.txt"
  assert stored_path.read_bytes() == b"safe contents"
  assert outside_path.read_text(encoding="utf-8") == "do not overwrite"
  assert response == {"filename": "../../evil.txt", "path": str(stored_path.resolve()), "size": 13}


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

  expected = {
      "role": "user",
      "content": "Please review these notes",
      "uploaded_files": [
          {
              "filename": "notes.txt",
              "path": "/tmp/notes.txt",
              "size": 12,
          },
      ],
      "event_index": 0,
      "id": "legacy:0",
      "timestamp": "2026-04-02T10:00:00Z",
  }
  expected[VOICE_KEY] = False
  assert messages == [expected]


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
  cfg = make_home_config(tmp_path)
  meta = SessionMetadata(name="Test Session")
  session_mgr = AsyncMock()
  req = SendMessageRequest(
      content="Summarize this",
      uploaded_files=[
          UploadedFileRef(filename="notes.txt", path="/tmp/notes.txt", size=12),
      ],
  )

  with (
      patch(CHAT_RUN_AND_FINALIZE_PATCH_TARGET, new=AsyncMock()) as mock_run,
      patch(CHAT_CREATE_LOGGED_TASK_PATCH_TARGET, side_effect=close_create_logged_task),
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
  cfg = make_home_config(tmp_path)
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
      patch(CHAT_RUN_AND_FINALIZE_PATCH_TARGET, new=AsyncMock()) as mock_run,
      patch(CHAT_CREATE_LOGGED_TASK_PATCH_TARGET, side_effect=close_create_logged_task),
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
