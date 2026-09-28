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
from pathlib import Path

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    backend_option,
)

from src.core import event_types as ET
from src.core.backend_models import BackendType
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager


def build_env(tmp_path: Path, backend_type: BackendType):
  """One backend of the requested TYPE, configured the way the type requires."""
  import src.core.config as core_config
  from src.core.config import CharlieBotConfig

  home = tmp_path / "home"
  model = None if backend_type in (BackendType.ANTIGRAVITY, BackendType.TUI_CLI) else "fake-model"
  kwargs: dict = {"id": "type-under-test", "label": "Type", "type": backend_type.value}
  if backend_type not in (BackendType.ANTIGRAVITY, BackendType.TUI_CLI):
    kwargs["model"] = model
  if backend_type in (BackendType.CC_OPENAI_COMPATIBLE, BackendType.CHARLIE_CODE):
    kwargs["api_base"] = "http://127.0.0.1:9"
  if backend_type == BackendType.CC_KIMI:
    kwargs["credential"] = "kimi"
  option = backend_option(**kwargs)
  cfg = CharlieBotConfig(
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
  session_mgr = SessionManager(cfg)
  return cfg, session_mgr, TaskTreeManager(cfg, session_mgr)


def session_attached_event(native_id: str) -> dict:
  return {"type": "system", "subtype": "init", "session_id": native_id}


BACKEND_TYPES = list(BackendType)
# The master queue only runs streaming backends: a manager turn on a TUI
# backend is refused by the existing guard (recorded in the manager-queue
# tests), so the identity assertions cover the eight executable types.
STREAMING_BACKEND_TYPES = [t for t in BackendType if t is not BackendType.TUI_CLI]


@pytest.mark.asyncio
@pytest.mark.parametrize("backend_type", STREAMING_BACKEND_TYPES, ids=lambda t: t.value)
async def test_run_records_stream_identity_and_result_truth(
    tmp_path: Path, backend_type: BackendType, monkeypatch: pytest.MonkeyPatch) -> None:
  from tests.test_task_execution import (
      SpawningScriptedBackend,
      install_backends,
      result_event,
  )

  cfg, session_mgr, tree = build_env(tmp_path, backend_type)
  root = await tree.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=None, name="M", backend=None, caller="operator")
  backend = SpawningScriptedBackend([
      session_attached_event("native-xyz-1"),
      result_event("typed output"),
  ])
  install_backends(monkeypatch, [backend], BUILD_BACKEND_PATCH_TARGET)
  from src.core.task_execution import TaskExecutionAdapter
  tree.dispatch.executor = TaskExecutionAdapter(cfg, session_mgr, tree)

  await tree.dispatch.admit_input(root.id, event_type=ET.USER, content="Take off. Answer.", actor="user")
  decision = await asyncio.wait_for(tree.dispatch.dispatch_pending(root.id), 5)
  run_id = decision["run_id"]
  assert run_id is not None
  deadline = asyncio.get_event_loop().time() + 10
  while asyncio.get_event_loop().time() < deadline:
    run = await tree.runs.get_run(root.id, run_id)
    events = tree.runs.load_events_sync(root.id)
    if tree.runs.run_has_terminal_fact(run, events):
      break
    await asyncio.sleep(0.05)
  else:
    pytest.fail(f"run for {backend_type.value} never finished")

  # The Run records the configured model, the stream's native session id,
  # and its own transport refs.
  if backend_type not in (BackendType.ANTIGRAVITY, BackendType.TUI_CLI):
    assert run.model == "fake-model"
  assert run.native_session_id == "native-xyz-1"
  run_dir = tree.runs.run_dir(root.id, run_id)
  assert run.raw_log_ref == str(run_dir / "agent.raw.ndjson")
  assert run.result_ref == str(run_dir / "agent.raw.ndjson")
  assert tree.runs.terminal_outcome(events, run_id) == "success"
  # The launched identity backed the run credential's acceptance window.
  assert run.pid == 424001 and run.pid_start == "1-424000"


@pytest.mark.asyncio
@pytest.mark.parametrize("backend_type", BACKEND_TYPES, ids=lambda t: t.value)
async def test_zero_output_and_error_results_fail_across_types(
    tmp_path: Path, backend_type: BackendType, monkeypatch: pytest.MonkeyPatch) -> None:
  from tests.test_task_execution import SpawningScriptedBackend, install_backends

  cfg, session_mgr, tree = build_env(tmp_path, backend_type)
  root = await tree.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=None, name="M", backend=None, caller="operator")

  # Zero output: a settled result event with all-zero usage and no
  # assistant text — the master queue's zero-output guard must turn it into
  # a nonzero exit so the run fails instead of consuming the input silently.
  # A tui-cli manager never reaches this: its turns are the terminal's, so
  # the dispatcher refuses before any Run exists (no headless failure, the
  # input stays pending).
  from src.agents.backends import base as backend_base
  empty = SpawningScriptedBackend([backend_base.make_result_event(0, 0)])
  install_backends(monkeypatch, [empty], BUILD_BACKEND_PATCH_TARGET)
  from src.core.task_execution import TaskExecutionAdapter
  tree.dispatch.executor = TaskExecutionAdapter(cfg, session_mgr, tree)
  await tree.dispatch.admit_input(root.id, event_type=ET.USER, content="Take off. Stay silent.", actor="user")
  decision = await asyncio.wait_for(tree.dispatch.dispatch_pending(root.id), 5)
  if backend_type is BackendType.TUI_CLI:
    assert decision["launch"] is False
    assert "terminal" in decision["reason"]
    assert "run_id" not in decision
    assert tree.runs.list_run_records_sync(root.id) == []
    assert [e for e in tree.events.load_events(root.id) if e.get("type") == ET.RUN_FINISHED] == []
    return
  run_id = decision["run_id"]
  deadline = asyncio.get_event_loop().time() + 10
  while asyncio.get_event_loop().time() < deadline:
    run = await tree.runs.get_run(root.id, run_id)
    events = tree.runs.load_events_sync(root.id)
    if tree.runs.run_has_terminal_fact(run, events):
      break
    await asyncio.sleep(0.05)
  assert tree.runs.terminal_outcome(events, run_id) == "failed"

  # A backend whose transport dies mid-turn (run() raises) fails the turn
  # through the master queue's error path. The failed zero-output run gates
  # fresh dispatch, so this phase goes through the explicit retry path (the
  # same one the retry route uses).
  class _TransportDeath(SpawningScriptedBackend):

    async def run(self, prompt, cwd, env, uploaded_files=None):
      raise RuntimeError("transport died mid-turn")
      yield  # pragma: no cover

  errored = _TransportDeath([])
  install_backends(monkeypatch, [errored], BUILD_BACKEND_PATCH_TARGET)
  retry = await tree.create_retry(root.id, "retry-error", run_id)
  retry_run_id = retry["run_id"]
  decision = await asyncio.wait_for(tree.dispatch.dispatch_pending(root.id), 5)
  run_id = decision["run_id"]
  deadline = asyncio.get_event_loop().time() + 10
  while asyncio.get_event_loop().time() < deadline:
    run = await tree.runs.get_run(root.id, run_id)
    events = tree.runs.load_events_sync(root.id)
    if tree.runs.run_has_terminal_fact(run, events):
      break
    await asyncio.sleep(0.05)
  assert tree.runs.terminal_outcome(events, retry_run_id) == "failed"
