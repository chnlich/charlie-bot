"""The context read APIs: next-start preview and historical Run context.

Both are reads through the one assembly owner: a preview is the current
configuration in the launch snapshot's shape, a Run's context is the stored
snapshot of record (plus pinned evidence references), legacy raw-prompt
evidence is labeled limited, and neither endpoint can mutate anything or
launch a process.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
import pytest_asyncio
from conftest import OPERATOR, build_session_manager, make_home_config
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.infra import config
from src.infra.models import PatchSessionTaskRequest, RunRecord, TaskSpec
from src.runtime.api import sessions as sessions_api
from src.runtime.api.deps import (
    get_config_on_loop,
    get_run_store,
    get_session_manager,
    get_session_store,
    get_task_manager,
)
from src.runtime.task_sessions import TaskTreeManager

pytestmark = pytest.mark.asyncio


class _TaskEnv:

  def __init__(self, tmp_path: Path) -> None:
    self.cfg = make_home_config(tmp_path)
    self.session_mgr = build_session_manager(self.cfg)
    self.tree = TaskTreeManager(self.cfg, self.session_mgr)
    app = FastAPI()
    app.include_router(sessions_api.router, prefix="/api/sessions")
    app.dependency_overrides[config.get_config] = lambda: self.cfg
    app.dependency_overrides[get_config_on_loop] = lambda: self.cfg
    app.dependency_overrides[get_session_manager] = lambda: self.session_mgr
    app.dependency_overrides[get_session_store] = lambda: self.session_mgr.store
    app.dependency_overrides[get_task_manager] = lambda: self.tree
    app.dependency_overrides[get_run_store] = lambda: self.tree.runs
    self.client = TestClient(app)


@pytest_asyncio.fixture
async def env(tmp_path: Path):
  return _TaskEnv(tmp_path)


async def _tree(env: _TaskEnv) -> dict[str, str]:
  root = await env.tree.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=None, name="Root", backend=None, caller=OPERATOR)
  worker = await env.tree.create_task(
      request_id="w",
      task_parent_id=root.id,
      profile="worker",
      task=TaskSpec(goal="worker goal", repo_path="/tmp/repo"),
      name="W",
      backend=None,
      caller=OPERATOR)
  await env.tree.patch_task(root.id, PatchSessionTaskRequest(subtree_prompt="the root subtree rule"), caller=OPERATOR)
  return {"root": root.id, "worker": worker.id}


async def test_run_context_returns_the_stored_snapshot(env: _TaskEnv) -> None:
  ids = await _tree(env)
  run_id = "ctx-run"
  await env.tree.runs.register_run(
      RunRecord(id=run_id, session_id=ids["worker"], kind="work"), task_spec_text="pinned spec")
  # The launch seam commits the snapshot; simulate the commit the adapter does.
  from src.runtime.task_execution import capture_prompt_chain
  from src.runtime.task_prompts import assemble_snapshot, build_segments
  meta = await env.tree.load_meta(ids["worker"])
  assert meta is not None
  index = await env.tree._get_index()
  chain, node_ref = capture_prompt_chain(env.tree, index, meta)
  segments, _err = build_segments(env.cfg, meta, "work", chain=chain, node_ref=node_ref, overlay=None)
  snapshot = assemble_snapshot(segments)
  path = env.tree.runs.run_dir(ids["worker"], run_id) / "prompt_snapshot.json"
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(json.dumps(snapshot.to_json_dict(), indent=2, ensure_ascii=False), encoding="utf-8")
  await env.tree.runs.record_observation(ids["worker"], run_id, prompt_snapshot_ref=str(path))
  run = await env.tree.runs.get_run(ids["worker"], run_id)
  assert run is not None and run.task_spec_ref is not None

  resp = env.client.get(f"/api/sessions/{ids['worker']}/runs/{run_id}/context")
  assert resp.status_code == 200, resp.text
  body = resp.json()
  assert body["mode"] == "historical"
  assert body["snapshot"] is not None
  assert body["snapshot"]["prompt_hash"] == snapshot.prompt_hash
  assert body["snapshot"]["char_count"] == snapshot.char_count
  assert body["legacy_prompt"] is None
  assert body["task_spec"] == {"ref": run.task_spec_ref, "sha256": run.task_spec_hash}
  assert set(body["logs"]) == {"raw_log_ref", "events_ref", "result_ref"}
  # The read is repeatable and immutable: a second call returns the same bytes.
  again = env.client.get(f"/api/sessions/{ids['worker']}/runs/{run_id}/context")
  assert again.json() == body


async def test_context_reads_are_read_only(env: _TaskEnv) -> None:
  """A preview or run-context read never launches, never touches anchors or events."""
  ids = await _tree(env)
  before_events = [e.get("type") for e in env.tree.events.load_events(ids["root"])]
  meta_before = (await env.tree.load_meta(ids["root"])).model_dump()
  env.client.get(f"/api/sessions/{ids['root']}/effective-prompt")
  env.client.get(f"/api/sessions/{ids['root']}/effective-prompt", params={"kind": "iteration"})
  after_events = [e.get("type") for e in env.tree.events.load_events(ids["root"])]
  meta_after = (await env.tree.load_meta(ids["root"])).model_dump()
  assert before_events == after_events
  assert meta_before == meta_after
  # And no Run appeared.
  assert env.tree.runs.list_run_records_sync(ids["root"]) == []


async def test_corrupt_new_snapshot_is_an_error(env: _TaskEnv) -> None:
  ids = await _tree(env)
  run_id = "corrupt-snapshot"
  await env.tree.runs.register_run(RunRecord(id=run_id, session_id=ids["worker"], kind="work"))
  snapshot_path = env.tree.runs.run_dir(ids["worker"], run_id) / "prompt_snapshot.json"
  snapshot_path.parent.mkdir(parents=True, exist_ok=True)
  snapshot_path.write_text("not json at all", encoding="utf-8")
  await env.tree.runs.record_observation(ids["worker"], run_id, prompt_snapshot_ref=str(snapshot_path))
  resp = env.client.get(f"/api/sessions/{ids['worker']}/runs/{run_id}/context")
  assert resp.status_code == 500
  assert "unreadable" in resp.json()["detail"]
