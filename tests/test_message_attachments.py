from __future__ import annotations

import io
from pathlib import Path
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
from src.core.models import (
    SendMessageRequest,
    SessionMetadata,
    UploadedFileRef,
)


@pytest.mark.asyncio
async def test_upload_file_strips_directory_components(tmp_path: Path) -> None:
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


@pytest.mark.asyncio
async def test_send_message_passes_structured_files_to_run_and_finalize(tmp_path: Path) -> None:
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
      {
          "filename": "notes.txt",
          "path": "/tmp/notes.txt",
          "size": 12
      },
  ]
