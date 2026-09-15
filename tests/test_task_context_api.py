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
from conftest import make_home_config
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import sessions as sessions_api
from src.api.deps import get_config, get_config_on_loop, get_run_store, get_session_manager, get_task_manager
from src.core.control_events import sha256_hex
from src.core.models import PatchSessionTaskRequest, RunRecord, TaskSpec
from src.core.run_token import CallerIdentity
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager

pytestmark = pytest.mark.asyncio

OPERATOR = CallerIdentity(kind="operator")


class _TaskEnv:
  def __init__(self, tmp_path: Path) -> None:
    self.cfg = make_home_config(tmp_path)
    self.session_mgr = SessionManager(self.cfg)
    self.tree = TaskTreeManager(self.cfg, self.session_mgr)
    app = FastAPI()
    app.include_router(sessions_api.router, prefix="/api/sessions")
    app.dependency_overrides[get_config] = lambda: self.cfg
    app.dependency_overrides[get_config_on_loop] = lambda: self.cfg
    app.dependency_overrides[get_session_manager] = lambda: self.session_mgr
    app.dependency_overrides[get_task_manager] = lambda: self.tree
    app.dependency_overrides[get_run_store] = lambda: self.tree.runs
    self.client = TestClient(app)


@pytest_asyncio.fixture
async def env(tmp_path: Path):
  return _TaskEnv(tmp_path)


async def _tree(env: _TaskEnv) -> dict[str, str]:
  root = await env.tree.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=None, name="Root",
      backend=None, caller=OPERATOR)
  worker = await env.tree.create_task(
      request_id="w", task_parent_id=root.id, profile="worker",
      task=TaskSpec(goal="worker goal", repo_path="/tmp/repo"), name="W", backend=None,
      caller=OPERATOR)
  await env.tree.patch_task(
      root.id, PatchSessionTaskRequest(subtree_prompt="the root subtree rule"),
      caller=OPERATOR)
  return {"root": root.id, "worker": worker.id}


async def test_effective_prompt_defaults_and_kinds(env: _TaskEnv) -> None:
  ids = await _tree(env)
  # A manager defaults to the manager_turn kind; a worker to work.
  preview = env.client.get(f"/api/sessions/{ids['root']}/effective-prompt")
  assert preview.status_code == 200, preview.text
  body = preview.json()
  assert body["kind"] == "manager_turn"
  assert body["mode"] == "preview"
  assert set(body) >= {"blocks", "prompt_hash", "char_count"}
  assert body["char_count"] == len("\n\n".join(b["text"] for b in body["blocks"]))
  # Every block names real origins; the manager template and the inherited
  # subtree rule (the root's rule enters the worker's chain) are present.
  refs = [s["source_ref"] for b in body["blocks"] for s in b["sources"]]
  assert "prompts/task_manager.md" in refs
  worker_preview = env.client.get(f"/api/sessions/{ids['worker']}/effective-prompt")
  assert worker_preview.status_code == 200
  worker_refs = [s["source_ref"] for b in worker_preview.json()["blocks"]
                 for s in b["sources"]]
  assert any(r.startswith("prompt_bodies/") for r in worker_refs)
  assert worker_preview.json()["kind"] == "work"
  assert "prompts/worker.md" in worker_refs
  assert "prompts/task_manager.md" not in worker_refs


async def test_effective_prompt_kind_selection_and_errors(env: _TaskEnv) -> None:
  ids = await _tree(env)
  review = env.client.get(
      f"/api/sessions/{ids['worker']}/effective-prompt", params={"kind": "review"})
  assert review.status_code == 200
  refs = [s["source_ref"] for b in review.json()["blocks"] for s in b["sources"]]
  assert any(r.startswith("src/core/review.py") for r in refs)
  bad = env.client.get(
      f"/api/sessions/{ids['worker']}/effective-prompt", params={"kind": "nonsense"})
  assert bad.status_code == 400
  missing = env.client.get("/api/sessions/00000000-0000-0000-0000-00000000dead/effective-prompt")
  assert missing.status_code == 404


async def test_run_context_returns_the_stored_snapshot(env: _TaskEnv) -> None:
  ids = await _tree(env)
  run_id = "ctx-run"
  await env.tree.runs.register_run(
      RunRecord(id=run_id, session_id=ids["worker"], kind="work"),
      task_spec_text="pinned spec")
  # The launch seam commits the snapshot; simulate the commit the adapter does.
  from src.core.task_execution import capture_prompt_chain
  from src.core.task_prompts import preview_snapshot
  meta = await env.tree.load_meta(ids["worker"])
  assert meta is not None
  index = await env.tree._get_index()
  chain, node_ref = capture_prompt_chain(env.tree, index, meta)
  snapshot, _err = preview_snapshot(
      env.cfg, meta, "work", chain=chain, node_ref=node_ref, overlay=None)
  path = env.tree.runs.run_dir(ids["worker"], run_id) / "prompt_snapshot.json"
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(json.dumps(snapshot.to_json_dict(), indent=2, ensure_ascii=False),
                  encoding="utf-8")
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


async def test_run_context_labels_legacy_raw_prompt_as_limited(env: _TaskEnv) -> None:
  ids = await _tree(env)
  run_id = "legacy-run"
  await env.tree.runs.register_run(RunRecord(id=run_id, session_id=ids["root"], kind="manager_turn"))
  legacy = env.tree.runs.run_dir(ids["root"], run_id) / "launch_prompt.md"
  legacy.parent.mkdir(parents=True, exist_ok=True)
  legacy.write_text("## some raw launch text\nno provenance recorded\n", encoding="utf-8")
  resp = env.client.get(f"/api/sessions/{ids['root']}/runs/{run_id}/context")
  assert resp.status_code == 200, resp.text
  body = resp.json()
  assert body["snapshot"] is None
  assert body["legacy_prompt"] is not None
  assert body["legacy_prompt"]["sha256"] == sha256_hex(
      legacy.read_text(encoding="utf-8"))
  assert "limited" in body["legacy_prompt"]["note"]
  assert "provenance was not recorded" in body["legacy_prompt"]["note"]


async def test_run_context_unknown_run_and_no_evidence(env: _TaskEnv) -> None:
  ids = await _tree(env)
  missing = env.client.get(f"/api/sessions/{ids['root']}/runs/nope/context")
  assert missing.status_code == 404
  run_id = "bare-run"
  await env.tree.runs.register_run(RunRecord(id=run_id, session_id=ids["root"], kind="manager_turn"))
  bare = env.client.get(f"/api/sessions/{ids['root']}/runs/{run_id}/context")
  assert bare.status_code == 200
  assert bare.json()["snapshot"] is None and bare.json()["legacy_prompt"] is None


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


async def test_session_detail_exposes_prompt_rule_facts(env: _TaskEnv) -> None:
  ids = await _tree(env)
  detail = env.client.get(f"/api/sessions/{ids['root']}").json()
  rules = detail["prompt_rules"]
  assert rules["subtree"]["ref"] is not None
  assert rules["subtree"]["chars"] == len("the root subtree rule")
  assert rules["subtree"]["source"].endswith(f"prompt_bodies/{rules['subtree']['ref']}.md")
  assert rules["node"]["ref"] is None
  # A subtree rule change on the root affects both descendants.
  assert rules["affected_descendants"] == 1
  worker_detail = env.client.get(f"/api/sessions/{ids['worker']}").json()
  assert worker_detail["prompt_rules"]["affected_descendants"] == 0
