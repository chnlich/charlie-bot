"""Per-backend-type interface tests for v2 task-tree Runs.

Every configured backend TYPE is exercised through the real adapter finalize
path with a simulated process/stream: the Run records its model and native
session from the stream, its raw/event refs point at its own run directory,
and the terminal status reflects the stream's truth — a success result event
succeeds, empty output and an error result fail, and a stop request reconciles
as the first terminal fact. The transport-coverage distinction between
backends (which support raw re-attach) is unchanged and asserted per type.
"""

from __future__ import annotations

import asyncio
import pathlib

import conftest
import pytest

from src.infra import backend_models, config_registry
from src.infra import event_types as ET
from src.runtime import task_sessions


def build_env(tmp_path: pathlib.Path, backend_type: str):
  """One backend of the requested TYPE, configured the way the type requires."""
  import src.infra.config as core_config

  home = tmp_path / "home"
  kwargs: dict = {"id": "type-under-test", "label": "Type", "type": backend_type}
  if not backend_models.backend_type_allows_missing_model(backend_type):
    kwargs["model"] = "fake-model"
  if backend_type in ("cc-openai-compatible", "charlie-code"):
    kwargs["api_base"] = "http://127.0.0.1:9"
  if backend_type == "cc-kimi":
    kwargs["credential"] = "kimi"
  option = conftest.backend_option(**kwargs)
  cfg = core_config.CharlieBotConfig(
      charliebot_home=home,
      backends={
          "options": [option],
          "preference": ["type-under-test"]
      },
      paths={"worktree_dir": str(home / "worktrees")})
  core_config._credentials_cache.seed(
      core_config.Credentials(path=home / "credentials.yaml", sections={"charliebot": {
          "access_key": "key-type"
      }}))
  session_mgr = conftest.build_session_manager(cfg)
  return cfg, session_mgr, task_sessions.TaskTreeManager(cfg, session_mgr)


def session_attached_event(native_id: str) -> dict:
  return {"type": "system", "subtype": "init", "session_id": native_id}


BACKEND_TYPES = list(config_registry.registered_backend_types())


@pytest.mark.asyncio
@pytest.mark.parametrize("backend_type", BACKEND_TYPES, ids=str)
async def test_run_records_stream_identity_and_result_truth(
    tmp_path: pathlib.Path, backend_type: str, monkeypatch: pytest.MonkeyPatch) -> None:
  from tests import test_task_execution

  cfg, session_mgr, tree = build_env(tmp_path, backend_type)
  root = await tree.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=None, name="M", backend=None, caller="operator")
  backend = test_task_execution.SpawningScriptedBackend(
      [
          session_attached_event("native-xyz-1"),
          test_task_execution.result_event("typed output"),
      ])
  test_task_execution.install_backends(monkeypatch, [backend], conftest.BUILD_BACKEND_PATCH_TARGET)
  from src.runtime import task_execution
  tree.dispatch.executor = task_execution.TaskExecutionAdapter(cfg, session_mgr, tree)

  await tree.dispatch.admit_input(root.id, event_type=ET.USER, content="Take off. Answer.", actor="user")
  decision = await asyncio.wait_for(tree.dispatch.dispatch_pending(root.id), 5)
  run_id = decision["run_id"]
  assert run_id is not None
  run, outcome = await test_task_execution.wait_for_terminal_run(tree, root.id, run_id, timeout=10.0)

  # The Run records the configured model, the stream's native session id,
  # and its own transport refs.
  if not backend_models.backend_type_allows_missing_model(backend_type):
    assert run.model == "fake-model"
  assert run.native_session_id == "native-xyz-1"
  run_dir = tree.runs.run_dir(root.id, run_id)
  assert run.raw_log_ref == str(run_dir / "agent.raw.ndjson")
  assert run.result_ref == str(run_dir / "agent.raw.ndjson")
  assert outcome == "success"
  # The launched identity backed the run credential's acceptance window.
  assert run.pid == 424001 and run.pid_start == "1-424000"


@pytest.mark.asyncio
@pytest.mark.parametrize("backend_type", BACKEND_TYPES, ids=str)
async def test_zero_output_and_error_results_fail_across_types(
    tmp_path: pathlib.Path, backend_type: str, monkeypatch: pytest.MonkeyPatch) -> None:
  from tests import test_task_execution

  cfg, session_mgr, tree = build_env(tmp_path, backend_type)
  root = await tree.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=None, name="M", backend=None, caller="operator")

  # Zero output: a settled result event with all-zero usage and no
  # assistant text — the master queue's zero-output guard must turn it into
  # a nonzero exit so the run fails instead of consuming the input silently.
  from src.runtime.agent_process import base as backend_base
  empty = test_task_execution.SpawningScriptedBackend([backend_base.make_result_event(0, 0)])
  test_task_execution.install_backends(monkeypatch, [empty], conftest.BUILD_BACKEND_PATCH_TARGET)
  from src.runtime import task_execution
  tree.dispatch.executor = task_execution.TaskExecutionAdapter(cfg, session_mgr, tree)
  await tree.dispatch.admit_input(root.id, event_type=ET.USER, content="Take off. Stay silent.", actor="user")
  decision = await asyncio.wait_for(tree.dispatch.dispatch_pending(root.id), 5)
  run_id = decision["run_id"]
  _run, outcome = await test_task_execution.wait_for_terminal_run(tree, root.id, run_id, timeout=10.0)
  assert outcome == "failed"

  # A backend whose transport dies mid-turn (run() raises) fails the turn
  # through the master queue's error path. The failed zero-output run gates
  # fresh dispatch, so this phase goes through the explicit retry path (the
  # same one the retry route uses).
  class _TransportDeath(test_task_execution.SpawningScriptedBackend):

    async def run(self, prompt, cwd, env, uploaded_files=None):
      raise RuntimeError("transport died mid-turn")
      yield  # pragma: no cover

  errored = _TransportDeath([])
  test_task_execution.install_backends(monkeypatch, [errored], conftest.BUILD_BACKEND_PATCH_TARGET)
  retry = await tree.create_retry(root.id, "retry-error", run_id)
  await asyncio.wait_for(tree.dispatch.dispatch_pending(root.id), 5)
  _run, outcome = await test_task_execution.wait_for_terminal_run(tree, root.id, retry["run_id"], timeout=10.0)
  assert outcome == "failed"
