"""The scheduler-side restores: cron auto-bind brings an archived reattached
node back (covered end-to-end in test_cron_auto_bind), and the cron editor's
enable restores the bound archived node. Real managers drive the tree."""

from __future__ import annotations

import pathlib

import conftest
import pytest
import yaml

from src.features.cron import config as cron_config
from src.features.cron import cron_files
from src.infra import config, models
from src.infra import event_types as ET
from tests import test_cron_auto_bind, test_cron_backend


@pytest.fixture()
def cron_env(tmp_path: pathlib.Path, temp_home: pathlib.Path, monkeypatch: pytest.MonkeyPatch):
  """The cron editor's env: synthetic home for cron.d, real managers bound as
  the deps singleton, the cron router mounted with the tree override."""
  cfg = config.CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      backends={"options": [conftest.OPUS_BACKEND_OPTION]},
      paths={"worktree_dir": str(tmp_path / "worktrees")})
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  session_blocks = conftest.build_session_blocks(cfg)
  tree = conftest.build_task_tree(cfg, session_blocks)
  conftest.bind_deps_blocks(monkeypatch, tree, session_blocks)
  test_cron_backend._patch_cron_d(monkeypatch, temp_home / ".charliebot" / "config.d" / "cron.d")
  return cfg, session_blocks, tree, temp_home, conftest.make_cron_client


@pytest.mark.asyncio
async def test_cron_editor_enable_restores_the_bound_archived_node(cron_env) -> None:
  cfg, session_blocks, tree, home, make_cron_client = cron_env
  conftest.write_nightly_task(home, backend=conftest.OPUS_BACKEND_ID)
  node = await conftest.create_scheduled_node(tree, name="nightly", backend=conftest.OPUS_BACKEND_ID)
  # Bind, then let the user archive the node (its cron task stops with it).
  cron_files.write_cron_key("nightly", "session_id", node.id)
  await tree.completion.archive_subtree(
      node.id,
      request_id="archive-test",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR)
  assert tree.task_state(node.id) == "archived"
  # The archive disabled the bound task.
  body = test_cron_auto_bind._read_task_yaml(home)
  assert body["session_id"] == node.id and body["enabled"] is False

  with make_cron_client(cfg, session_blocks) as client:
    enabled = client.put("/api/cron/tasks/nightly", json={"enabled": True})
    assert enabled.status_code == 200
    assert enabled.json()["enabled"] is True

  # The enable restored the node: open, with the cron enable as the named source.
  assert tree.task_state(node.id) == "open"
  reopens = [e for e in tree.events.load_events(node.id) if e["type"] == ET.TASK_REOPENED]
  assert [e["reason"] for e in reopens] == ["cron enable"]

  # Disabling again (and archiving again) writes no restore.
  cron_files.write_cron_key("nightly", "enabled", value=False)
  await tree.completion.archive_subtree(
      node.id,
      request_id="archive-test-after-enable",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR)
  with make_cron_client(cfg, session_blocks) as client:
    disabled = client.put("/api/cron/tasks/nightly", json={"enabled": False})
    assert disabled.status_code == 200
  assert tree.task_state(node.id) == "archived"
  assert len([e for e in tree.events.load_events(node.id) if e["type"] == ET.TASK_REOPENED]) == 1


@pytest.mark.asyncio
async def test_archive_disables_every_task_bound_to_the_node(cron_env) -> None:
  """Two tasks bound to one node both stop: a task left enabled on an archived
  node fails its binding check on every scheduler tick."""
  _, _, tree, home, _ = cron_env
  conftest.write_nightly_task(home, backend=conftest.OPUS_BACKEND_ID)
  weekly = {**test_cron_auto_bind._read_task_yaml(home), "cron": "0 3 * * 0"}
  (home / ".charliebot" / "config.d" / "cron.d" / "weekly.yaml").write_text(yaml.safe_dump(weekly), encoding="utf-8")
  node = await conftest.create_scheduled_node(tree, name="nightly", backend=conftest.OPUS_BACKEND_ID)
  cron_files.write_cron_key("nightly", "session_id", node.id)
  cron_files.write_cron_key("weekly", "session_id", node.id)

  await tree.completion.archive_subtree(
      node.id,
      request_id="archive-test",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR)

  assert tree.task_state(node.id) == "archived"
  for name in ("nightly", "weekly"):
    body = test_cron_auto_bind._read_task_yaml(home, name)
    assert body["session_id"] == node.id and body["enabled"] is False, name


@pytest.mark.asyncio
async def test_auto_bind_restore_leaves_the_legacy_status_branch_alone(cron_env) -> None:
  """The legacy stored-status branch of auto-bind still unarchives a
  status-archived node without any task fact."""
  cfg, session_blocks, tree, home = cron_env[0], cron_env[1], cron_env[2], cron_env[3]
  conftest.write_nightly_task(home, backend=conftest.OPUS_BACKEND_ID)
  node = await conftest.create_scheduled_node(tree, name="nightly", backend=conftest.OPUS_BACKEND_ID)
  node = await session_blocks.lifecycle.archive_session(node.id)
  assert node is not None and node.status == models.SessionStatus.ARCHIVED
  # A legacy-status archived node's task state is still open (no close fact).
  assert tree.task_state(node.id) == "open"

  scheduler = conftest.build_scheduler(cfg, session_blocks)

  task_cfg = cron_config.ScheduledTaskConfig(
      name="nightly", cron="0 3 * * *", prompt="run nightly", enabled=True, timezone="America/Los_Angeles")
  session_cache: dict[str, list] = {}
  await scheduler._auto_bind(task_cfg, cfg, session_cache)
  assert task_cfg.session_id == node.id
  restored = await session_blocks.store.get_session(node.id)
  assert restored is not None and restored.status == models.SessionStatus.ACTIVE
  # No task_reopened fact: the legacy branch never wrote task facts.
  assert [e for e in tree.events.load_events(node.id) if e["type"] == ET.TASK_REOPENED] == []
  _ = yaml
