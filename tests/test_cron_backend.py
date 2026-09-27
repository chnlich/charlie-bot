"""Tests for scheduled task backend overrides."""

from datetime import UTC, datetime
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock
from zoneinfo import ZoneInfo

import pytest
import yaml
from conftest import (
    OPUS_BACKEND_ID,
    make_cron_client,
    make_scheduler_setup,
    write_nightly_prompt,
)

from src.api import cron as cron_api
from src.core.config import (
    CharlieBotConfig,
    ScheduledTaskConfig,
    _load_cron_file,
)
from src.core.models import (
    CreateSessionRequest,
    LastRunStatus,
    SessionStatus,
)
from src.core.sessions import SessionManager


def _patch_cron_d(monkeypatch: pytest.MonkeyPatch, cron_dir: Path) -> None:
  """Redirect the cron API's host-file IO at *cron_dir*.

  src.api.cron binds cron_dir and cron_path as separate module globals, so
  both must move; patching cron_dir alone leaves cron_path resolving the real
  profile directory through src.core.config.
  """
  monkeypatch.setattr(cron_api, "cron_dir", lambda: cron_dir)
  monkeypatch.setattr(cron_api, "cron_path", lambda name: cron_dir / f"{name}.yaml")


_NIGHTLY_PROMPT_MD = "run nightly\n"


def _cron_api_rig(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    preseed_backend: str | None = None,
) -> tuple[Path, CharlieBotConfig, SessionManager, Path]:
  """Stage the cron API world: a cron.d dir, the nightly prompt source, the
  patched cron-dir bindings, and the real scheduler trio.

  ``preseed_backend`` also seeds nightly.yaml carrying that backend; without
  it the host file is absent, as before the task's first create. Returns
  (cron_dir, cfg, session_mgr, md_path).
  """
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True, exist_ok=True)
  if preseed_backend is None:
    md_path = write_nightly_prompt(tmp_path, _NIGHTLY_PROMPT_MD)
  else:
    _yaml_path, md_path, _md_content = _seed_prompt_file_task(cron_dir, tmp_path, backend=preseed_backend)
  _patch_cron_d(monkeypatch, cron_dir)
  cfg, session_mgr, _ = make_scheduler_setup(tmp_path)
  return cron_dir, cfg, session_mgr, md_path


@pytest.mark.asyncio
async def test_scheduler_rotates_scheduled_session_backend_and_copies_bookkeeping(tmp_path: Path) -> None:
  cfg, session_mgr, scheduler = make_scheduler_setup(tmp_path)
  old_session = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: nightly", scheduled_task="nightly"),
      backend=OPUS_BACKEND_ID,
  )
  old_session.last_scheduled_run = "2026-06-07T02:00:00-07:00"
  old_session.last_scheduled_cron = "0 2 * * *"
  old_session.last_run_status = LastRunStatus.SUCCESS
  old_session.cc_session_id = "old-backend-conversation"
  old_session.cc_session_started_at = datetime(2026, 6, 7, 9, 0, tzinfo=UTC)
  await session_mgr.save_metadata(old_session)

  task_cfg = ScheduledTaskConfig(
      name="nightly",
      cron="0 2 * * *",
      prompt="nightly prompt",
      backend="codex-o3",
  )

  new_session = await scheduler._get_or_create_session(task_cfg, cfg, session_mgr)

  assert new_session is not None
  assert new_session.id != old_session.id
  assert new_session.backend == "codex-o3"
  assert new_session.scheduled_task == "nightly"
  assert new_session.last_scheduled_run == old_session.last_scheduled_run
  assert new_session.last_scheduled_cron == old_session.last_scheduled_cron
  assert new_session.last_run_status == old_session.last_run_status
  assert new_session.cc_session_id is None
  assert new_session.cc_session_started_at is None
  archived_old = await session_mgr.get_session(old_session.id)
  assert archived_old is not None
  assert archived_old.status == SessionStatus.ARCHIVED


@pytest.mark.asyncio
async def test_scheduler_backend_rotation_preserves_last_run_to_avoid_duplicate_fire(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg, session_mgr, scheduler = make_scheduler_setup(tmp_path)
  old_session = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: nightly", scheduled_task="nightly"),
      backend=OPUS_BACKEND_ID,
  )
  now = datetime.now(ZoneInfo("America/Los_Angeles"))
  old_session.last_scheduled_run = now.isoformat()
  old_session.last_scheduled_cron = "* * * * *"
  old_session.last_run_status = LastRunStatus.SUCCESS
  await session_mgr.save_metadata(old_session)
  task_cfg = ScheduledTaskConfig(
      name="nightly",
      cron="* * * * *",
      prompt="nightly prompt",
      backend="codex-o3",
  )
  execute_task = AsyncMock()
  monkeypatch.setattr(scheduler, "_execute_task", execute_task)

  await scheduler._maybe_run(task_cfg, session_mgr, {"nightly": [old_session]}, cfg)

  execute_task.assert_not_awaited()
  active_sessions = await session_mgr.list_sessions(status=SessionStatus.ACTIVE, scheduled=True)
  assert len(active_sessions) == 1
  assert active_sessions[0].backend == "codex-o3"
  assert active_sessions[0].last_scheduled_run == now.isoformat()
  assert active_sessions[0].last_scheduled_cron == "* * * * *"


def test_cron_api_persists_and_clears_backend(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cron_dir, cfg, session_mgr, md_path = _cron_api_rig(tmp_path, monkeypatch)
  nightly_path = cron_dir / "nightly.yaml"

  def _read_backend() -> str:
    return yaml.safe_load(nightly_path.read_text(encoding="utf-8")).get("backend")

  with make_cron_client(cfg, session_mgr) as client:
    create_response = client.post(
        "/api/cron/tasks",
        json={
            "name": "nightly",
            "cron": "0 2 * * *",
            "prompt_file": str(md_path),
            "backend": "codex-o3",
        },
    )
    assert create_response.status_code == 200
    assert create_response.json()["backend"] == "codex-o3"
    assert _read_backend() == "codex-o3"

    clear_response = client.put("/api/cron/tasks/nightly", json={"backend": None})
    assert clear_response.status_code == 200
    assert "backend" not in clear_response.json()
    assert "backend" not in yaml.safe_load(nightly_path.read_text(encoding="utf-8"))

    update_response = client.put("/api/cron/tasks/nightly", json={"backend": "codex-o3"})
    assert update_response.status_code == 200
    assert update_response.json()["backend"] == "codex-o3"

    empty_clear_response = client.put("/api/cron/tasks/nightly", json={"backend": ""})
    assert empty_clear_response.status_code == 200
    assert "backend" not in yaml.safe_load(nightly_path.read_text(encoding="utf-8"))


def test_cron_api_rejects_invalid_backend_on_create(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cron_dir, cfg, session_mgr, md_path = _cron_api_rig(tmp_path, monkeypatch)

  with make_cron_client(cfg, session_mgr) as client:
    response = client.post(
        "/api/cron/tasks",
        json={
            "name": "nightly",
            "cron": "0 2 * * *",
            "prompt_file": str(md_path),
            "backend": "missing-backend",
        },
    )

  assert response.status_code == 400
  assert response.json()["detail"] == "backend 'missing-backend' is not in backends.options"
  assert not (cron_dir / "nightly.yaml").exists()


def _seed_prompt_file_task(cron_dir: Path, tmp_path: Path, *, backend: str | None = None) -> tuple[Path, Path, str]:
  """Write a prompt_file-backed 'nightly' job; returns (yaml_path, md_path, md_content).

  The host file carries the path to its prompt source under ``prompt_file`` and
  the pointed file owns the body, exactly as production host files look.
  ``backend`` adds the backend key to the host file.
  """
  md_path = write_nightly_prompt(tmp_path, _NIGHTLY_PROMPT_MD)
  body: dict[str, Any] = {
      "cron": "0 3 * * *",
      "prompt_file": str(md_path),  # absolute path, as production files use
      "timezone": "America/Los_Angeles",
      "enabled": True,
  }
  if backend is not None:
    body["backend"] = backend
  yaml_path = cron_dir / "nightly.yaml"
  yaml_path.write_text(yaml.safe_dump(body, sort_keys=False), encoding="utf-8")
  return yaml_path, md_path, _NIGHTLY_PROMPT_MD


def test_load_cron_file_loads_prompt_file(tmp_path: Path) -> None:
  from src.core.config import CharlieBotConfig

  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True, exist_ok=True)
  cfg = CharlieBotConfig(charliebot_home=tmp_path)
  yaml_path, md_path, md_content = _seed_prompt_file_task(cron_dir, tmp_path)

  task, _ = _load_cron_file(yaml_path, cfg.charlie_bot_repo, "nightly")
  assert task.prompt == md_content
  assert task.prompt_file == str(md_path)
