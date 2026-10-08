from __future__ import annotations

import io
import pathlib
from unittest import mock

import conftest
import fastapi
import pytest

from src.infra import models
from src.runtime.api import chat


@pytest.mark.asyncio
async def test_upload_file_strips_directory_components(tmp_path: pathlib.Path) -> None:
  cfg = conftest.make_home_config(tmp_path)
  meta = models.SessionMetadata(profile="manager", name="Upload Session")
  outside_path = cfg.sessions_dir / "evil.txt"
  outside_path.parent.mkdir(parents=True)
  outside_path.write_text("do not overwrite", encoding="utf-8")
  upload = fastapi.UploadFile(file=io.BytesIO(b"safe contents"), filename="../../evil.txt")

  response = await chat.upload_file(
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
async def test_send_message_admits_structured_files_to_the_task_tree(tmp_path: pathlib.Path) -> None:
  cfg = conftest.make_home_config(tmp_path)
  meta = models.SessionMetadata(profile="manager", name="Test Session")
  task_mgr = mock.MagicMock()
  task_mgr.dispatch.admit_input = mock.AsyncMock(return_value={"id": "event-1"})
  task_mgr.dispatch.dispatch_pending = mock.AsyncMock(return_value={"launch": True})
  req = models.SendMessageRequest(
      content="Summarize this",
      uploaded_files=[
          models.UploadedFileRef(filename="notes.txt", path="/tmp/notes.txt", size=12),
      ],
  )

  response = await chat.send_message(
      meta.id,
      req,
      _meta=meta,
      task_mgr=task_mgr,
      caller=conftest.OPERATOR,
  )

  assert response.status_code == 202
  task_mgr.dispatch.admit_input.assert_awaited_once()
  assert task_mgr.dispatch.admit_input.await_args.args == (meta.id,)
  assert task_mgr.dispatch.admit_input.await_args.kwargs["content"] == "Summarize this"
  assert task_mgr.dispatch.admit_input.await_args.kwargs["uploaded_files"] == [
      {
          "filename": "notes.txt",
          "path": "/tmp/notes.txt",
          "size": 12
      },
  ]
  task_mgr.dispatch.dispatch_pending.assert_awaited_once_with(meta.id)
