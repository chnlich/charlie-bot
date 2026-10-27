"""Tests for scheduled task backend overrides."""

import datetime
import pathlib
import zoneinfo
from typing import Any
from unittest import mock

import conftest
import pytest
import yaml

from src.features.cron import config as cron_config
from src.features.cron import loader
from src.infra import config


def _patch_cron_d(monkeypatch: pytest.MonkeyPatch, cron_dir: pathlib.Path) -> None:
  """Redirect the cron host-file IO at *cron_dir*.

  The loader resolves cron_dir and cron_path at call time, so both patches land on src.features.cron.loader.
  """
  monkeypatch.setattr(loader, "cron_dir", lambda: cron_dir)
  monkeypatch.setattr(loader, "cron_path", lambda name: cron_dir / f"{name}.yaml")


_NIGHTLY_PROMPT_MD = "run nightly\n"


@pytest.mark.asyncio
async def test_scheduler_aligns_bound_node_backend_in_place(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A bound task's node follows the task config: the tick's alignment switches
  the node's backend in place and leaves the scheduler bookkeeping untouched."""
  cfg, session_blocks, scheduler = conftest.make_scheduler_setup(tmp_path)
  tree = conftest.build_task_tree(cfg, session_blocks)
  conftest.bind_deps_blocks(monkeypatch, tree, session_blocks)
  node = await conftest.create_scheduled_node(tree, name="nightly", backend=conftest.OPUS_BACKEND_ID)
  node.last_scheduled_run = "2026-06-07T09:00:00+00:00"
  node.last_scheduled_cron = "0 2 * * *"
  await session_blocks.store.save_metadata(node)

  task_cfg = cron_config.ScheduledTaskConfig(
      name="nightly",
      cron="0 2 * * *",
      prompt="nightly prompt",
      backend="codex-o3",
      session_id=node.id,
  )
  await scheduler._align_bound_backend(task_cfg, cfg)

  fresh = await session_blocks.store.get_session(node.id)
  assert fresh is not None
  assert fresh.backend == "codex-o3"
  assert fresh.scheduled_task is None  # the node is a task-tree node, never re-stamped
  assert fresh.last_scheduled_run == "2026-06-07T09:00:00+00:00"
  assert fresh.last_scheduled_cron == "0 2 * * *"
  # Idempotent: a second alignment on the now-current backend writes nothing.
  await scheduler._align_bound_backend(task_cfg, cfg)
  sessions = await session_blocks.listing.list_sessions()
  assert len(sessions) == 1


@pytest.mark.asyncio
async def test_backend_alignment_preserves_last_run_to_avoid_catchup_fire(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A hand-edited yaml backend seen by the tick switches the node in place and
  preserves its last_scheduled_run: the next fire is computed from the true last
  occurrence, not from a rotated generation's empty bookkeeping."""
  cfg, session_blocks, scheduler = conftest.make_scheduler_setup(tmp_path)
  tree = conftest.build_task_tree(cfg, session_blocks)
  conftest.bind_deps_blocks(monkeypatch, tree, session_blocks)
  node = await conftest.create_scheduled_node(tree, name="nightly", backend=conftest.OPUS_BACKEND_ID)
  now = datetime.datetime.now(zoneinfo.ZoneInfo("America/Los_Angeles"))
  await tree.update_slot_fields(node.id, "cron", last_scheduled_run=now.isoformat(), last_scheduled_cron="* * * * *")
  task_cfg = cron_config.ScheduledTaskConfig(
      name="nightly",
      cron="* * * * *",
      prompt="nightly prompt",
      backend="codex-o3",
      session_id=node.id,
  )
  execute_task = mock.AsyncMock()
  scheduler._execute_task = execute_task

  await scheduler._align_bound_backend(task_cfg, cfg)
  await scheduler._maybe_run(task_cfg, session_blocks.events, {}, cfg)

  execute_task.assert_not_awaited()
  fresh = await session_blocks.store.get_session(node.id)
  assert fresh is not None
  assert fresh.backend == "codex-o3"
  assert fresh.last_scheduled_run == now.isoformat()
  assert fresh.last_scheduled_cron == "* * * * *"


def _seed_prompt_file_task(cron_dir: pathlib.Path,
                           tmp_path: pathlib.Path,
                           *,
                           backend: str | None = None) -> tuple[pathlib.Path, pathlib.Path, str]:
  """Write a prompt_file-backed 'nightly' job; returns (yaml_path, md_path, md_content).

  The host file carries the path to its prompt source under ``prompt_file`` and
  the pointed file owns the body, exactly as production host files look.
  ``backend`` adds the backend key to the host file.
  """
  md_path = conftest.write_nightly_prompt(tmp_path, _NIGHTLY_PROMPT_MD)
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


def test_load_cron_file_loads_prompt_file(tmp_path: pathlib.Path) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True, exist_ok=True)
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path)
  yaml_path, md_path, md_content = _seed_prompt_file_task(cron_dir, tmp_path)

  task, _ = loader._load_cron_file(yaml_path, cfg.charlie_bot_repo, "nightly")
  assert task.prompt == md_content
  assert task.prompt_file == str(md_path)
