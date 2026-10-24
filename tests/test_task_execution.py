"""Execution-adapter tests: v2 Runs bound to the existing master/worker harnesses.

Covers the required behavior wiring: durable dispatch to actual execution (one
Run, exact batch, launched identity persisted before its credential works),
delegate as task/run with stable replay identity, queued retry launches,
failure requiring explicit retry, first-terminal-fact-wins, worker work/review
delivery through the same task, and real landing verification.
"""

from __future__ import annotations

import asyncio
import datetime
import errno
import json
import os
import pathlib
import subprocess
import time
import uuid
from collections.abc import AsyncIterator, Awaitable, Callable

import conftest
import fastapi
import pytest
from fastapi import testclient

import src.runtime.task_execution as task_execution_module
import src.runtime.task_input_prompt as task_input_prompt_module
from src.backends.claude_code import claude_accounts, claude_relay, master_cc_relay
from src.features.voice import turn_contribution
from src.infra import config, constants, models
from src.infra import event_types as ET
from src.runtime import (
    backend_pool,
    message_events,
    midturn_delivery,
    session_anchors,
    session_fork,
    task_errors,
    task_sessions,
)
from src.runtime import runs as runs_mod
from src.runtime.api import message_utils

# The internal-API auth headers carrying the access key stub_credentials seeds: tests
# pass it as headers=. It is not a caller identity; conftest's OPERATOR (CallerIdentity)
# is what caller= takes.
OP_HEADERS = {"Authorization": "Bearer op-secret"}


def seed_signing_home(home: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Make the synthetic home the one spawned children sign against: seed its
    access key and pin CHARLIEBOT_HOME at it, so every child environment signs
    its run token against the synthetic home (never operator credentials)."""
  import src.infra.config as core_config
  core_config._credentials_cache.seed(
      core_config.Credentials(
          path=home / "credentials.yaml", sections={"charliebot": {
              "access_key": "task-exec-test-key"
          }}))
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))


def build_spawning_env(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, *, options: list, pool: list[str] | None = None):
  """(cfg, session blocks, TaskTreeManager) over one synthetic home under tmp_path:
    the given backend options, worktree_dir inside the home, and the seeded access
    key every child environment signs against (seed_signing_home)."""
  from src.infra import config
  home = tmp_path / "charliebot-home"
  backends: dict = {"options": options}
  if pool is not None:
    backends["pool"] = pool
  cfg = config.CharlieBotConfig(
      charliebot_home=home, backends=backends, paths={"worktree_dir": str(home / "worktrees")})
  seed_signing_home(home, monkeypatch)
  session_blocks = conftest.build_session_blocks(cfg)
  return cfg, session_blocks, conftest.build_task_tree(cfg, session_blocks)


def build_env(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    backend_ids: list[str] | None = None,
    *,
    pool: list[str] | None = None):
  """One fake backend registered, so task creation's default resolution works.

    ``backend_ids`` names every configured option (default: the single "fake");
    tests that switch a session's backend pin build a config with more than one.
    ``pool`` overrides the rotation members (default: the first option); a test that
    expects a review names a second member — the reviewer never uses the worker's member.
    """
  ids = backend_ids if backend_ids is not None else ["fake"]
  return build_spawning_env(
      tmp_path,
      monkeypatch,
      options=[conftest.backend_option(id=i, label=i, type="codex", model="fake-model") for i in ids],
      pool=pool if pool is not None else ids[:1])


class SpawningScriptedBackend:
  """Backend double that fires the on_spawn callback, records its launch env,
    and yields a scripted event list ending in a result event. A *post_events*
    hook is awaited after that stream, before run() returns: it is how a test
    acts on the world between one build's events and the next build's prompt."""

  def __init__(
      self,
      events: list[dict],
      exit_code: int = 0,
      stderr_text: str = "",
      pre_run: Callable[[], None] | None = None,
      gate: Callable[[], object] | None = None,
      post_events: Callable[[], Awaitable[None]] | None = None) -> None:
    self._events = events
    self.exit_code = exit_code
    self.stderr_text = stderr_text
    self._pre_run = pre_run
    self.gate = gate
    self.post_events = post_events
    self.pid_start = "1-424000"
    self.terminated = False
    self.hang_diagnostics = None
    self.prompt: str | None = None
    self.env: dict | None = None
    self.cwd: str | None = None
    self._on_spawn = None
    self._pid = 424000
    self.cgroup_exit_report = lambda: None

  def set_on_spawn(self, on_spawn) -> None:
    self._on_spawn = on_spawn

  async def terminate(self) -> None:
    self.terminated = True

  def detach(self) -> None:
    pass

  async def run(self,
                prompt: str,
                cwd: str,
                env: dict,
                uploaded_files: list[dict] | None = None) -> AsyncIterator[dict]:
    self.prompt = prompt
    self.cwd = cwd
    self.env = dict(env)
    self._pid += 1
    if self._pre_run is not None:
      self._pre_run()
    if self.gate is not None:
      await asyncio.wait_for(self.gate(), timeout=15)
    if self._on_spawn is not None:
      await self._on_spawn(self._pid)
    for event in self._events:
      if self.terminated:
        return
      yield event
    if self.post_events is not None:
      await self.post_events()


def result_event(text: str) -> dict:
  """A result event carrying real usage and text (the zero-output guard reads both)."""
  from src.runtime.agent_process import base as backend_base
  event = backend_base.make_result_event(input_tokens=10, output_tokens=5)
  event["result"] = text
  return event


def script_manager_turn(monkeypatch: pytest.MonkeyPatch, notes: list[str]) -> None:
  """Script the parent node's report-consuming turn through the registry
  builder (the manager dispatch path), so no external process starts
  from these tests. One scripted backend per expected manager-turn build."""
  conftest.install_scripted_backends(
      monkeypatch, [SpawningScriptedBackend([result_event(note)]) for note in notes],
      conftest.BUILD_BACKEND_PATCH_TARGET)


async def drain_manager_turns(tree: task_sessions.TaskTreeManager, manager_id: str) -> None:
  """Wait until every manager_turn Run of the node reached a terminal fact,
  so no launch task outlives the test as background residue."""
  deadline = asyncio.get_event_loop().time() + 15
  while asyncio.get_event_loop().time() < deadline:
    records = [r for r in tree.runs.list_run_records_sync(manager_id) if r.kind == "manager_turn"]
    events = tree.runs.load_events_sync(manager_id)
    if records and all(tree.runs.terminal_outcome(events, r.id) is not None for r in records):
      return
    await asyncio.sleep(0.05)
  pytest.fail(f"the manager turn never settled: {tree.runs.list_run_records_sync(manager_id)}")


def signal_then_hold(started: asyncio.Event, release: asyncio.Event) -> Callable[[], Awaitable[None]]:
  """A backend gate: set *started* when the run reaches it, then hold the run until *release* is set."""

  async def hold() -> None:
    started.set()
    await release.wait()

  return hold


def make_pm_build(text: str, pm_builds: list | None = None):
  """The parent-manager turn's build function for conftest.BUILD_BACKEND_PATCH_TARGET:
    one scripted double per registry build, wired for on_spawn the way
    install_scripted_backends wires worker builds. ``pm_builds`` collects the built
    doubles for tests that assert the parent turn actually built one."""

  def pm_build(option, cfg_, **kwargs):
    b = SpawningScriptedBackend([result_event(text)])
    on_spawn = kwargs.get("on_spawn")
    if on_spawn is not None:
      b.set_on_spawn(on_spawn)
    if pm_builds is not None:
      pm_builds.append(b)
    return b

  return pm_build


def make_api_client(cfg, session_blocks, task_mgr) -> testclient.TestClient:
  from src.features.improve import api as improve_api
  from src.infra import config
  from src.runtime.api import deps
  from src.runtime.api import internal as internal_api
  from src.runtime.api import sessions as sessions_api

  app = fastapi.FastAPI()
  app.include_router(sessions_api.router, prefix="/api/sessions")
  app.include_router(internal_api.router, prefix="/api/internal")
  app.include_router(improve_api.router, prefix="/api/internal")
  app.dependency_overrides[config.get_config] = lambda: cfg
  app.dependency_overrides[deps.get_config_on_loop] = lambda: cfg
  conftest.override_session_blocks(app, session_blocks)
  app.dependency_overrides[deps.get_task_manager] = lambda: task_mgr
  app.dependency_overrides[deps.get_run_store] = lambda: task_mgr.runs
  return testclient.TestClient(app)


async def wait_for_terminal_run(
    tree: task_sessions.TaskTreeManager,
    session_id: str,
    run_id: str,
    timeout: float = 15.0) -> tuple[models.RunRecord, str]:
  """Poll one Run until its end record lands (the launch is fire-and-forget).

    The terminal fact and the metadata mirror land as two writes under one
    lock; a poll that reads the record between them returns a stale record
    whose ``ended_at`` is still empty, so the terminal outcome alone is not
    the landing's edge — wait for the mirror's ``ended_at`` too.
    """
  deadline = asyncio.get_event_loop().time() + timeout
  while asyncio.get_event_loop().time() < deadline:
    run = await tree.runs.get_run(session_id, run_id)
    assert run is not None, f"run {run_id} vanished"
    events = tree.runs.load_events_sync(session_id)
    outcome = tree.runs.terminal_outcome(events, run_id)
    if outcome is not None and run.ended_at is not None:
      return run, str(outcome)
    await asyncio.sleep(0.05)
  pytest.fail(f"run {run_id} never reached a terminal fact within {timeout}s")


async def wait_for_worktree(
    tree: task_sessions.TaskTreeManager, session_id: str, run_id: str, timeout: float) -> pathlib.Path:
  """Poll one Run until the spawner records its worktree and return the path.

    ``executor.launch`` returns before the spawn chain fills the record in, so
    the caller must poll; the timeout covers the worktree_dir mkdir only.
    """
  deadline = asyncio.get_event_loop().time() + timeout
  while asyncio.get_event_loop().time() < deadline:
    run = await tree.runs.get_run(session_id, run_id)
    if run is not None and run.worktree_path:
      return pathlib.Path(run.worktree_path)
    await asyncio.sleep(0.05)
  pytest.fail(f"run {run_id} never recorded its worktree within {timeout}s")


async def wait_for_review_terminal(
    tree: task_sessions.TaskTreeManager, session_id: str, timeout: float, what: str) -> models.RunRecord:
  """Poll a session's runs until its first review Run carries a terminal fact; return it.

    The review Run's id is not known before the work Run's gate releases, so the
    poll matches on ``kind == "review"`` rather than on a run id.
    """
  deadline = asyncio.get_event_loop().time() + timeout
  while asyncio.get_event_loop().time() < deadline:
    runs = {r.id: r for r in tree.runs.list_run_records_sync(session_id)}
    review = [r for r in runs.values() if r.kind == "review"]
    if review and tree.runs.terminal_outcome(tree.runs.load_events_sync(session_id), review[0].id) is not None:
      return review[0]
    await asyncio.sleep(0.1)
  pytest.fail(f"{what} never reached a terminal fact within {timeout}s")


def work_run_worktree(tree: task_sessions.TaskTreeManager, worker_id: str) -> pathlib.Path:
  """The one work-kind run's worktree path (asserts exactly one work run)."""
  work = [r for r in tree.runs.list_run_records_sync(worker_id) if r.kind == "work"]
  assert len(work) == 1 and work[0].worktree_path
  return pathlib.Path(work[0].worktree_path)


def implement_marker_commit(tree: task_sessions.TaskTreeManager, worker_id: str) -> None:
  """The implement backend's side effect: commit marker.txt on the work branch."""
  wt = work_run_worktree(tree, worker_id)
  (wt / "marker.txt").write_text("implemented\n")
  conftest.run_git(wt, "add", "-A")
  conftest.run_git(wt, "-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", "implement marker")


# ---------------------------------------------------------------------------
# Manager turns: durable dispatch to actual execution
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_admit_input_replay_compares_extra_fields_and_rejects_core_collisions(tmp_path: pathlib.Path) -> None:
  _cfg, _blocks, tree = conftest.build_env(tmp_path)
  manager = await conftest.create_task(tree, parent=None, request_id="fields", name="Fields")

  with pytest.raises(task_errors.TaskInvalidError):
    await tree.dispatch.admit_input(
        manager.id,
        event_type=ET.USER,
        content="message",
        actor="user",
        request_id="collision",
        fields={"content": "replacement"},
    )

  event = await tree.dispatch.admit_input(
      manager.id,
      event_type=ET.USER,
      content="message",
      actor="user",
      request_id="fields-request",
      fields={"summon": {
          "mention_id": "m1"
      }},
  )
  replay = await tree.dispatch.admit_input(
      manager.id,
      event_type=ET.USER,
      content="message",
      actor="user",
      request_id="fields-request",
      fields={"summon": {
          "mention_id": "m1"
      }},
  )
  assert replay == event
  assert event["request_id"] == "fields-request"
  assert event["summon"] == {"mention_id": "m1"}

  with pytest.raises(task_errors.TaskConflictError):
    await tree.dispatch.admit_input(
        manager.id,
        event_type=ET.USER,
        content="message",
        actor="user",
        request_id="fields-request",
    )

  with pytest.raises(task_errors.TaskConflictError):
    await tree.dispatch.admit_input(
        manager.id,
        event_type=ET.USER,
        content="message",
        actor="user",
        request_id="fields-request",
        fields={"summon": {
            "mention_id": "m2"
        }},
    )

  inputs = [event for event in tree.events.load_events(manager.id) if event.get("request_id") == "fields-request"]
  assert inputs == [event]


@pytest.mark.asyncio
async def test_manager_turn_persists_run_identity_and_acknowledges_batch(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, _session_blocks, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
  backend = SpawningScriptedBackend([result_event("SMOKE reply")])
  builds = conftest.install_scripted_backends(monkeypatch, [backend], conftest.BUILD_BACKEND_PATCH_TARGET)

  admitted = await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="Take off. Reply with the phrase.", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision["launch"] is True
  run_id = decision["run_id"]
  assert run_id is not None

  run, outcome = await wait_for_terminal_run(tree, manager.id, run_id)
  await conftest.drain_session_consumer(manager.id, timeout=5)

  runs = tree.runs.list_run_records_sync(manager.id)
  assert [r.id for r in runs] == [run_id]
  assert run.kind == "manager_turn"
  assert run.pid == 424001 and run.pid_start == "1-424000"
  assert run.native_session_id is None or isinstance(run.native_session_id, str)
  assert run.model == "fake-model"
  # The ref names the Run's own transport dir (the backend double writes no
  # raw file; the live smoke asserts the real file exists).
  assert run.raw_log_ref == str(tree.runs.run_dir(manager.id, run_id) / "agent.raw.ndjson")
  assert run.input_event_ids == [str(admitted["id"])]
  assert outcome == "success"
  # The exact claimed batch is acknowledged; no second synthetic USER copy
  # was persisted and no master_run mirror exists for the v2 turn.
  assert tree.dispatch.pending_inputs(manager.id) == []
  events = tree.events.load_events(manager.id)
  assert [e["type"] for e in events if e["type"] == ET.USER] == ["user"]
  assert not (cfg.sessions_dir / manager.id / "data" / "master_runs").exists()
  # The launched process identity is the precondition of a run credential:
  # a queued run's token is rejected, a launched run's is accepted, and a
  # finished run's is not.
  from src.runtime import run_token
  from src.runtime.api import deps
  queued_claims = run_token.RunTokenClaims(session_id=manager.id, run_id="run-queued", agent=manager.name or "m")
  await tree.runs.register_run_locked(models.RunRecord(id="run-queued", session_id=manager.id, kind="manager_turn"))
  with pytest.raises(fastapi.HTTPException, match="has not launched"):
    await deps.require_caller(_bearer(queued_claims), tree.runs)
  launched_claims = run_token.RunTokenClaims(session_id=manager.id, run_id="run-launched", agent=manager.name or "m")
  await tree.runs.register_run_locked(models.RunRecord(id="run-launched", session_id=manager.id, kind="manager_turn"))
  await tree.runs.record_launch(manager.id, "run-launched", pid=424900, pid_start="ps-900")
  identity = await deps.require_caller(_bearer(launched_claims), tree.runs)
  assert identity.claims.run_id == "run-launched"
  finished_claims = run_token.RunTokenClaims(session_id=manager.id, run_id=run_id, agent=manager.name or "m")
  with pytest.raises(fastapi.HTTPException, match="active run"):
    await deps.require_caller(_bearer(finished_claims), tree.runs)
  assert len(builds) == 1
  assert builds[0]["kwargs"].get("on_spawn") is not None
  captured_env = builds[0]["backend"].env or {}
  assert captured_env.get(constants.SESSION_ID_ENV_VAR) == manager.id
  assert captured_env.get("CHARLIEBOT_HOME") == str(cfg.charliebot_home)


def _bearer(claims):
  """A signed run-token request against the synthetic home's seeded key."""
  import src.infra.config as core_config
  from src.runtime import run_token
  key = str(core_config.get_credentials().get("charliebot", "access_key") or "")
  token = run_token.sign_run_token(claims, key)
  # Lowercase key: the real Header object is case-insensitive; the plain
  # dict double must match the exact key require_caller reads.
  return type("R", (), {"headers": {"authorization": f"Bearer {token}"}})()


def _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch):
  from src.runtime import streaming
  monkeypatch.setattr(streaming.streaming_manager, "broadcast", _async_noop)
  return conftest.build_execution_adapter(cfg, session_blocks, tree)


async def _async_noop(*args, **kwargs) -> None:
  return None


async def build_manager_root(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch):
  """One synthetic instance with an operator-created root manager: build_env
    plus the operator credentials, the silent-broadcast executor, and the
    config-owner patch the spawn-style routes read. Returns
    (cfg, session_blocks, tree, root)."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  conftest.stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  # The spawn-style routes resolve backends through the config owner directly.
  monkeypatch.setattr(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  root = await tree.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=models.TaskSpec(goal="project"),
      name="Project",
      backend=None,
      caller=conftest.OPERATOR)
  return cfg, session_blocks, tree, root


async def _launch_manager_turn(cfg, session_blocks, tree, monkeypatch, manager, content) -> list:
  """The launch rig the implement and quick-edit tests share: silent-broadcast
    executor, the parent-manager build double, the instructions patch, the
    operator credentials, and the user takeoff message through admit_input.

    Ordering is load-bearing: the executor sits in place before admit_input
    dispatches, and the build double before the manager turn builds. Returns
    the pm_builds list for tests that assert the parent turn built one.
    """
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  pm_builds = []
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, make_pm_build("manager turn", pm_builds))
  conftest.stub_credentials({"charliebot": {"access_key": "op-secret"}})
  # The work-run launch re-judges the nearest-user authorization gate; the
  # manager carries the real user takeoff message the delegation rode in on.
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content=content, actor="user")
  return pm_builds


@pytest.mark.asyncio
async def test_first_message_on_empty_goal_task_dispatches_a_manager_turn(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The one-click New Task product (empty goal, no acceptance, no refs) takes
    the user's first message through the normal durable input path: the message
    is admitted, one manager_turn Run claims exactly that batch, and the turn
    executes — no Goal-required obstacle anywhere in the dispatch."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(
      tree,
      parent=None,
      request_id="one-click-root",
      profile="manager",
      task=models.TaskSpec(goal="", acceptance=[], context_refs=[]),
      name=None)
  assert manager.task is not None and manager.task.goal == ""
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  backend = SpawningScriptedBackend([result_event("SMOKE reply")])
  conftest.install_scripted_backends(monkeypatch, [backend], conftest.BUILD_BACKEND_PATCH_TARGET)

  admitted = await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="First message on a brand-new task.", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision["launch"] is True
  run_id = decision["run_id"]

  run, outcome = await wait_for_terminal_run(tree, manager.id, run_id)
  assert outcome == "success"
  assert run.kind == "manager_turn"
  assert run.input_event_ids == [str(admitted["id"])]
  assert tree.dispatch.pending_inputs(manager.id) == []
  events = tree.events.load_events(manager.id)
  user_events = [e for e in events if e["type"] == ET.USER]
  assert [e["content"] for e in user_events] == ["First message on a brand-new task."]


@pytest.mark.asyncio
async def test_manager_finish_claims_only_echoed_pending_user_ids(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _session_blocks, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
  initial = await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Initial", actor="user")
  run_id = str(uuid.uuid4())
  run = models.RunRecord(id=run_id, session_id=manager.id, kind="manager_turn")
  async with tree.control_lock:
    await tree.runs.register_run_locked(run)
    await tree.dispatch.claim_input_batch_locked(manager.id, run_id)
  await tree.runs.record_launch(manager.id, run_id, pid=424001, pid_start="1-424000")
  echoed = await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Echoed", actor="user")
  queued = await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Still pending", actor="user")
  agent = await tree.dispatch.admit_input(
      manager.id, event_type=ET.AGENT_MESSAGE, content="Agent input", actor="agent", from_session="agent-session")
  transport_dir = tree.runs.run_dir(manager.id, run_id)
  raw_path = transport_dir / runs_mod.RAW_LOG_NAME
  raw_path.write_text(
      "\n".join(
          json.dumps(event) for event in (
              {
                  "type": ET.USER,
                  "uuid": run_id,
                  "isReplay": True
              },
              {
                  "type": ET.USER,
                  "uuid": echoed["id"],
                  "isReplay": True
              },
              {
                  "type": ET.USER,
                  "uuid": agent["id"],
                  "isReplay": True
              },
          )) + "\n",
      encoding="utf-8",
  )

  claimed = await midturn_delivery.claim_finish_inputs(manager, tree.dispatch, run_id, transport_dir, successful=True)

  assert claimed == [str(echoed["id"])]
  updated = await tree.runs.get_run(manager.id, run_id)
  assert updated is not None
  assert updated.input_event_ids == [str(initial["id"]), str(echoed["id"])]
  assert {str(event["id"]) for event in tree.dispatch.pending_inputs(manager.id)
         } == {str(queued["id"]), str(agent["id"])}

  await tree.dispatch.finish_run(manager.id, run_id, outcome="success", exit_code=0)
  finished = next(
      event for event in tree.runs.load_events_sync(manager.id)
      if event.get("type") == ET.RUN_FINISHED and event.get("run_id") == run_id)
  assert finished["input_event_ids"] == [str(initial["id"]), str(echoed["id"])]
  assert {str(event["id"]) for event in tree.dispatch.pending_inputs(manager.id)
         } == {str(queued["id"]), str(agent["id"])}


@pytest.mark.asyncio
async def test_concurrent_dispatch_starts_one_process(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _session_blocks, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
  backend = SpawningScriptedBackend([result_event("one")])
  builds = conftest.install_scripted_backends(monkeypatch, [backend], conftest.BUILD_BACKEND_PATCH_TARGET)

  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off. First message.", actor="user")
  # Two dispatch calls race the same pending batch: the reservation serializes
  # them, so exactly one Run and one process exist.
  results = await asyncio.gather(tree.dispatch.dispatch_pending(manager.id), tree.dispatch.dispatch_pending(manager.id))
  launched = [d for d in results if d.get("launch")]
  assert len(launched) == 1
  run_id = launched[0]["run_id"]
  await wait_for_terminal_run(tree, manager.id, run_id)
  assert len(tree.runs.list_run_records_sync(manager.id)) == 1
  assert len(builds) == 1
  assert tree.dispatch.pending_inputs(manager.id) == []


# ---------------------------------------------------------------------------
# Serialized turns: inputs admitted during an active run dispatch on its finish
# ---------------------------------------------------------------------------


class _SpawnFirstBackend(SpawningScriptedBackend):
  """Records the spawn identity first, then holds the turn open until released."""

  async def run(self, prompt, cwd, env, uploaded_files=None):
    self.prompt = prompt
    self.cwd = cwd
    self.env = dict(env)
    self._pid += 1
    if self._on_spawn is not None:
      await self._on_spawn(self._pid)
    if self.gate is not None:
      await asyncio.wait_for(self.gate(), timeout=15)
    for event in self._events:
      if self.terminated:
        return
      yield event


@pytest.mark.asyncio
@pytest.mark.parametrize("later_type", [ET.USER, ET.AGENT_MESSAGE])
@pytest.mark.parametrize("session_kind", ["web", "slack", "discord"])
async def test_input_admitted_during_active_run_dispatches_after_its_finish(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, later_type: str, session_kind: str) -> None:
  """Un-delivered user and agent inputs wait for the next serialized turn."""
  _cfg, _session_blocks, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
  if session_kind == "slack":
    await tree.update_slot_fields(
        manager.id, "slack", slack_origin={
            "team_id": "T",
            "channel_id": "C",
            "thread_ts": "1.0"
        })
  elif session_kind == "discord":
    await tree.update_slot_fields(
        manager.id, "discord", discord_origin={
            "guild_id": "G",
            "parent_channel_id": "C",
            "thread_id": "1"
        })
  gate_release = asyncio.Event()
  first = _SpawnFirstBackend([result_event("first")], gate=gate_release.wait)
  second = _SpawnFirstBackend([result_event("second")])
  conftest.install_scripted_backends(monkeypatch, [first, second], conftest.BUILD_BACKEND_PATCH_TARGET)

  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off. First.", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision["launch"] is True
  run1 = decision["run_id"]

  deadline = asyncio.get_event_loop().time() + 10
  while asyncio.get_event_loop().time() < deadline:
    run = await tree.runs.get_run(manager.id, run1)
    if run is not None and run.pid is not None:
      break
    await asyncio.sleep(0.05)
  else:
    pytest.fail("the first turn never launched")

  # The later input is durably admitted but never launched under the active run.
  admitted_later = await tree.dispatch.admit_input(
      manager.id,
      event_type=later_type,
      content="Second message.",
      actor="user" if later_type == ET.USER else "agent",
      from_session="agent-session" if later_type == ET.AGENT_MESSAGE else None)
  decision_later = await tree.dispatch.dispatch_pending(manager.id)
  assert decision_later["launch"] is False

  gate_release.set()
  await wait_for_terminal_run(tree, manager.id, run1)

  # The turn's own finish dispatches the waiting input as the next serialized
  # turn: no second explicit dispatch call is needed and nothing is dropped.
  deadline = asyncio.get_event_loop().time() + 15
  run2_id = None
  while asyncio.get_event_loop().time() < deadline:
    others = [r for r in tree.runs.list_run_records_sync(manager.id) if r.id != run1]
    if others:
      run2_id = others[0].id
      break
    await asyncio.sleep(0.1)
  assert run2_id is not None, "the later input was never dispatched after the turn finished"
  run2, outcome2 = await wait_for_terminal_run(tree, manager.id, run2_id)
  assert outcome2 == "success"
  assert run2.input_event_ids == [str(admitted_later["id"])]
  assert tree.dispatch.pending_inputs(manager.id) == []


# ---------------------------------------------------------------------------
# Delegate: one worker child task with its first Run, stable replay identity
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_delegate_creates_one_child_and_replays_are_stable(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:

  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  conftest.stub_credentials({"charliebot": {"access_key": "op-secret"}})
  pm_builds = []
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, make_pm_build("manager turn", pm_builds))
  backend = SpawningScriptedBackend([result_event("phrase")])
  builds = conftest.install_scripted_backends(
      monkeypatch,
      [backend,
       SpawningScriptedBackend([result_event("phrase")]),
       SpawningScriptedBackend([result_event("phrase")])], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  repo, _origin = conftest.init_repo_with_origin(tmp_path / "delegate-work")

  # The runtime gate re-judges at delegation: the manager's chain needs a real
  # user message, takeoff phrase or not.
  await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="Take off and delegate the phrase task.", actor="user")
  monkeypatch.setattr(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  with make_api_client(cfg, session_blocks, tree) as client:
    payload = {
        "session_id": manager.id,
        "description": "## Goal\n\nsay the phrase\n",
        "task_type": "quick-edit",
        "keep_worktree": False,
        "repo_path": str(repo),
        "base_branch": "main",
    }
    first = client.post("/api/internal/delegate", json=payload, headers=OP_HEADERS)
    assert first.status_code == 200, first.text
    body = first.json()
    child_id, run_id = body["session_id"], body["run_id"]
    assert body["parent_session_id"] == manager.id
    assert body["thread_id"] == run_id
    child_meta = await tree.load_meta(child_id)
    assert child_meta is not None and child_meta.profile == "worker"
    assert child_meta.task_parent_id == manager.id

    # The replayed request (same spec, derived stable id) returns the
    # original child and Run instead of a second process.
    replay = client.post("/api/internal/delegate", json=payload, headers=OP_HEADERS)
    assert replay.status_code == 200, replay.text
    assert replay.json()["session_id"] == child_id
    assert replay.json()["run_id"] == run_id

    # The same explicit request_id replays identically; a distinct explicit
    # id names a genuinely different operation (an intentional sibling).
    first_named = client.post("/api/internal/delegate", json=dict(payload, request_id="op-1"), headers=OP_HEADERS)
    assert first_named.json()["session_id"] != child_id
    replay_named = client.post("/api/internal/delegate", json=dict(payload, request_id="op-1"), headers=OP_HEADERS)
    assert replay_named.json()["session_id"] == first_named.json()["session_id"]
    sibling = client.post("/api/internal/delegate", json=dict(payload, request_id="op-2"), headers=OP_HEADERS)
    assert sibling.json()["session_id"] != first_named.json()["session_id"]

    # The runs execute through the worker adapter while the API loop that
    # scheduled them is still alive. Every delivery follow-up — child close,
    # parent report dispatch, the parent's serialized turns — is scheduled
    # on that same loop, so it must outlive them all.
    deadline = asyncio.get_event_loop().time() + 30
    while asyncio.get_event_loop().time() < deadline:
      states = [tree.task_state(s) for s in (child_id, first_named.json()["session_id"], sibling.json()["session_id"])]
      if all(s == "completed" for s in states):
        break
      await asyncio.sleep(0.1)
    for cid in (child_id, first_named.json()["session_id"], sibling.json()["session_id"]):
      assert tree.task_state(cid) == "completed"

    # The completed delivery closed and auto-archived the worker; the
    # manager remains open and received the completed report.
    deadline = asyncio.get_event_loop().time() + 20
    archived = False
    while asyncio.get_event_loop().time() < deadline:
      if tree.task_state(child_id) == "completed" and tree.archived_of(await tree._get_index(), await
                                                                       tree.load_meta(child_id)):
        archived = True
        break
      await asyncio.sleep(0.1)
    assert archived, "the worker task was not auto-archived after its delivered report"
    assert tree.task_state(manager.id) == "open"

    def reports_of(cid: str) -> list[dict]:
      return [e for e in conftest.child_reports(tree, manager.id) if e["child_session_id"] == cid]

    # Each child's close fact and its parent report are two appends under one
    # lock (_append_closed). The lock-free state reads above can fall between
    # them, and the settle below returns at once while a report is unwritten,
    # so each child's report is waited for on its own.
    for cid in (child_id, first_named.json()["session_id"], sibling.json()["session_id"]):
      await poll_until(lambda cid=cid: reports_of(cid), timeout=15.0, what=f"the completed report of child {cid}")
      assert reports_of(cid)[-1]["outcome"] == "completed"

    legacy_dir = cfg.sessions_dir / child_id / "threads"
    assert not list(legacy_dir.iterdir()) if legacy_dir.is_dir() else True

    # Every delivered child report triggered the parent's next serialized
    # turn: all reports were consumed and acknowledged, the parent stays open.
    await conftest._settle_parent(tree, manager, timeout=30.0, poll=0.2)
    assert pm_builds, "the delivered child reports never triggered a parent manager turn"
    assert tree.task_state(manager.id) == "open"

  # Three distinct operations, three builds: the replays did not spawn a
  # second process for their operation.
  assert len(builds) == 3
  captured_envs = [build["backend"].env or {} for build in builds]
  assert {env.get(constants.SESSION_ID_ENV_VAR) for env in captured_envs
         } == {child_id, first_named.json()["session_id"],
               sibling.json()["session_id"]}
  assert all(env.get(constants.RUN_TOKEN_ENV) for env in captured_envs)
  assert all(env.get("CHARLIEBOT_HOME") == str(cfg.charliebot_home) for env in captured_envs)
  worker_run = await tree.runs.get_run(child_id, run_id)
  assert worker_run is not None and worker_run.kind == "work"
  assert worker_run.backend == "fake"
  pm_events = tree.runs.load_events_sync(manager.id)
  for r in tree.runs.list_run_records_sync(manager.id):
    assert tree.runs.terminal_outcome(pm_events, r.id) == "success"


# ---------------------------------------------------------------------------
# Review context: the spec is the node's task record, the one the delegate wrote
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_repo_review_context_carries_the_delegated_spec(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  repo, _origin = conftest.init_repo_with_origin(tmp_path / "review-spec-work")
  await _launch_manager_turn(cfg, session_blocks, tree, monkeypatch, manager, "Take off and rename the config key.")
  conftest.install_scripted_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("renamed")])], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  monkeypatch.setattr(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  spec = "## Goal\n\nrename the config key\n\n## Reviewer Checklist\n- the legacy key still loads\n"
  # The delegated run launches on the API loop, which lives only inside this block.
  with make_api_client(cfg, session_blocks, tree) as client:
    response = client.post(
        "/api/internal/delegate",
        json={
            "session_id": manager.id,
            "description": spec,
            "task_type": "quick-edit",
            "keep_worktree": False,
            "repo_path": str(repo),
            "base_branch": "main",
        },
        headers=OP_HEADERS)
    assert response.status_code == 200, response.text
    child_id, run_id = response.json()["session_id"], response.json()["run_id"]
    # The finished work Run carries the worktree and branch the review reuses.
    work_run, outcome = await wait_for_terminal_run(tree, child_id, run_id)
    assert outcome == "success"
    review_context = await tree.dispatch.executor._build_review_context(child_id, work_run)
  assert "- the legacy key still loads" in review_context


@pytest.mark.asyncio
async def test_repo_review_context_refuses_a_task_with_an_empty_goal(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _session_blocks, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
  repo, _origin = conftest.init_repo_with_origin(tmp_path / "empty-goal-work")
  empty_spec = {"goal": "", "repo_path": str(repo), "base_branch": "main", "task_type": "implement"}
  worker = await conftest.create_task(
      tree, parent=manager.id, request_id="w", profile="worker", task=_task_spec(tree, empty_spec))
  work_run = models.RunRecord(
      id="run-work",
      session_id=worker.id,
      kind="work",
      backend="fake",
      model="fake-model",
      repo_path=str(repo),
      base_branch="main",
      branch_name="charliebot/task-empty-goal",
      worktree_path=str(tmp_path / "empty-goal-worktree"))
  with pytest.raises(task_errors.TaskInvalidError):
    await tree.dispatch.executor._build_review_context(worker.id, work_run)


async def _claimed_work_run_after(
    tree: task_sessions.TaskTreeManager, session_id: str, earlier_run_ids: set[str],
    timeout: float) -> models.RunRecord:
  """The node's next work run, once its reservation holds a claimed batch.

    The dispatcher binds a run's input batch at reservation, before the run's
    process starts, so a message admitted after this returns never joins the
    batch this run already claimed.
    """
  deadline = asyncio.get_event_loop().time() + timeout
  while asyncio.get_event_loop().time() < deadline:
    for run in tree.runs.list_run_records_sync(session_id):
      if run.kind == "work" and run.id not in earlier_run_ids and run.input_event_ids:
        return run
    await asyncio.sleep(0.05)
  pytest.fail(f"node {session_id} never reserved a work run claiming a batch within {timeout}s")


@pytest.mark.asyncio
async def test_review_contexts_list_the_agent_message_the_second_run_claimed(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.runtime import review as review_module
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root", name="Root manager")
  repo, _origin = conftest.init_repo_with_origin(tmp_path / "review-later-message")
  await _launch_manager_turn(cfg, session_blocks, tree, monkeypatch, manager, "Take off and rename the config key.")
  conftest.install_scripted_backends(
      monkeypatch, [
          SpawningScriptedBackend([conftest.assistant_text_event("started")], exit_code=1),
          SpawningScriptedBackend([result_event("adjusted")]),
      ], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  monkeypatch.setattr(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  with make_api_client(cfg, session_blocks, tree) as client:
    response = client.post(
        "/api/internal/delegate",
        json={
            "session_id": manager.id,
            "description": "## Goal\n\nrename the config key\n",
            "task_type": "quick-edit",
            "keep_worktree": False,
            "repo_path": str(repo),
            "base_branch": "main",
        },
        headers=OP_HEADERS)
    assert response.status_code == 200, response.text
    child_id, run_id = response.json()["session_id"], response.json()["run_id"]
    first_run, first_outcome = await wait_for_terminal_run(tree, child_id, run_id)
    assert first_outcome == "failed" and first_run.input_event_ids == []
    # The failed run's report archived the child: the manager's in-place
    # continuation is refused with the archived sentence, and the operator
    # restore reopens the node for the second run. The archive rides the
    # failed run's delivery chain, so it can land after the terminal fact.
    await poll_until(lambda: tree.task_state(child_id) == "archived", timeout=15.0, what="the failed child's archive")
    relay = client.post(
        "/api/internal/session-message",
        json={
            "session_id": manager.id,
            "target_session_id": child_id,
            "content": "## Rule change: keep the legacy key working",
        },
        headers=OP_HEADERS)
    assert relay.status_code == 409, relay.text
    assert relay.json()["detail"] == f"task {child_id} is archived"
    restored = client.post(f"/api/sessions/{child_id}/unarchive", headers=OP_HEADERS)
    assert restored.status_code == 200, restored.text
    relay = client.post(
        "/api/internal/session-message",
        json={
            "session_id": manager.id,
            "target_session_id": child_id,
            "content": "## Rule change: keep the legacy key working",
        },
        headers=OP_HEADERS)
    assert relay.status_code == 200, relay.text
    second_run = await _claimed_work_run_after(tree, child_id, {run_id}, timeout=15.0)
    agent_messages = [e for e in tree.events.load_events(child_id) if e["type"] == ET.AGENT_MESSAGE]
    assert [str(e["id"]) for e in agent_messages] == second_run.input_event_ids
    second_run, second_outcome = await wait_for_terminal_run(tree, child_id, second_run.id)
    assert second_outcome == "success"
    review_context = await tree.dispatch.executor._build_review_context(child_id, second_run)
    repo_less_context = await tree.dispatch.executor._build_repo_less_review_context(child_id, second_run)

  meta = await tree.load_meta(child_id)
  assert meta is not None and meta.task is not None
  spec_text = review_module.review_spec_text(meta.task)
  section = "**Later Messages:**\n[Message from session Root manager] ## Rule change: keep the legacy key working"
  # The repo review context shows the message after the task spec, before the worker summary.
  assert f"**Task spec:**\n{spec_text}\n{section}\n**Worker summary:**" in review_context
  # The repo-less review context shows the same section text.
  assert f"### Task spec (acceptance tests included)\n{spec_text}\n\n{section}\n\n### Work report" in repo_less_context


@pytest.mark.asyncio
async def test_review_context_without_claimed_inputs_goes_from_spec_to_summary(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.runtime import review as review_module
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  repo, _origin = conftest.init_repo_with_origin(tmp_path / "review-no-later-message")
  await _launch_manager_turn(cfg, session_blocks, tree, monkeypatch, manager, "Take off and rename the config key.")
  conftest.install_scripted_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("renamed")])], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  monkeypatch.setattr(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  with make_api_client(cfg, session_blocks, tree) as client:
    response = client.post(
        "/api/internal/delegate",
        json={
            "session_id": manager.id,
            "description": "## Goal\n\nrename the config key\n",
            "task_type": "quick-edit",
            "keep_worktree": False,
            "repo_path": str(repo),
            "base_branch": "main",
        },
        headers=OP_HEADERS)
    assert response.status_code == 200, response.text
    child_id, run_id = response.json()["session_id"], response.json()["run_id"]
    work_run, outcome = await wait_for_terminal_run(tree, child_id, run_id)
    assert outcome == "success"
    review_context = await tree.dispatch.executor._build_review_context(child_id, work_run)

  meta = await tree.load_meta(child_id)
  assert meta is not None and meta.task is not None
  spec_text = review_module.review_spec_text(meta.task)
  assert "Later Messages" not in review_context
  # The task spec is followed directly by the worker summary.
  assert f"**Task spec:**\n{spec_text}\n**Worker summary:**" in review_context


@pytest.mark.asyncio
async def test_review_context_lists_each_runs_claimed_messages_in_claim_order(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root", name="Root manager")
  repo, _origin = conftest.init_repo_with_origin(tmp_path / "review-claim-order")
  await _launch_manager_turn(cfg, session_blocks, tree, monkeypatch, manager, "Take off and rename the config key.")
  conftest.install_scripted_backends(
      monkeypatch, [
          SpawningScriptedBackend([conftest.assistant_text_event("started")], exit_code=1),
          SpawningScriptedBackend([conftest.assistant_text_event("retrying")], exit_code=1),
          SpawningScriptedBackend([result_event("done")]),
      ], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  monkeypatch.setattr(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  with make_api_client(cfg, session_blocks, tree) as client:
    response = client.post(
        "/api/internal/delegate",
        json={
            "session_id": manager.id,
            "description": "## Goal\n\nrename the config key\n",
            "task_type": "quick-edit",
            "keep_worktree": False,
            "repo_path": str(repo),
            "base_branch": "main",
        },
        headers=OP_HEADERS)
    assert response.status_code == 200, response.text
    child_id, run_id = response.json()["session_id"], response.json()["run_id"]
    # Each failed run's report archives the child, so each relayed message
    # follows an operator restore and dispatches its own follow-up work run:
    # two runs, one claimed message apiece. The archive rides the failed
    # run's delivery chain, so it can land after the terminal fact.
    first_run, first_outcome = await wait_for_terminal_run(tree, child_id, run_id)
    assert first_outcome == "failed" and first_run.input_event_ids == []
    await poll_until(lambda: tree.task_state(child_id) == "archived", timeout=15.0, what="the first failure's archive")
    restored = client.post(f"/api/sessions/{child_id}/unarchive", headers=OP_HEADERS)
    assert restored.status_code == 200, restored.text
    first_relay = client.post(
        "/api/internal/session-message",
        json={
            "session_id": manager.id,
            "target_session_id": child_id,
            "content": "First later message.",
        },
        headers=OP_HEADERS)
    assert first_relay.status_code == 200, first_relay.text
    second_run = await _claimed_work_run_after(tree, child_id, {run_id}, timeout=15.0)
    agent_messages = [e for e in tree.events.load_events(child_id) if e["type"] == ET.AGENT_MESSAGE]
    assert len(agent_messages) == 1 and second_run.input_event_ids == [str(agent_messages[0]["id"])]
    second_run, second_outcome = await wait_for_terminal_run(tree, child_id, second_run.id)
    assert second_outcome == "failed"
    await poll_until(lambda: tree.task_state(child_id) == "archived", timeout=15.0, what="the second failure's archive")
    restored = client.post(f"/api/sessions/{child_id}/unarchive", headers=OP_HEADERS)
    assert restored.status_code == 200, restored.text
    second_relay = client.post(
        "/api/internal/session-message",
        json={
            "session_id": manager.id,
            "target_session_id": child_id,
            "content": "Second later message.",
        },
        headers=OP_HEADERS)
    assert second_relay.status_code == 200, second_relay.text
    third_run = await _claimed_work_run_after(tree, child_id, {run_id, second_run.id}, timeout=15.0)
    agent_messages = [e for e in tree.events.load_events(child_id) if e["type"] == ET.AGENT_MESSAGE]
    assert len(agent_messages) == 2 and third_run.input_event_ids == [str(agent_messages[1]["id"])]
    third_run, third_outcome = await wait_for_terminal_run(tree, child_id, third_run.id)
    assert third_outcome == "success"
    review_context = await tree.dispatch.executor._build_review_context(child_id, third_run)

  section_pos = review_context.index("**Later Messages:**")
  first_pos = review_context.index("[Message from session Root manager] First later message.")
  second_pos = review_context.index("[Message from session Root manager] Second later message.")
  assert section_pos < first_pos < second_pos


# ---------------------------------------------------------------------------
# Retry: queued runs, stops, and the explicit-retry gate
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_manager_retry_reruns_its_own_batch_and_stopped_retry_never_launches(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _session_blocks, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)

  # A manager round claims its batch and fails. The failed round counts as
  # handled whatever its outcome: its batch never reappears as pending, and
  # a new message dispatches a new round at once — no explicit retry gate.
  first_in = await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off. First.", actor="user")
  await tree.runs.register_run_locked(
      models.RunRecord(id="run-failed", session_id=manager.id, kind="manager_turn", backend="fake", model="fake-model"))
  async with tree.control_lock:
    await tree.dispatch.claim_input_batch_locked(manager.id, "run-failed")
  await tree.dispatch.finish_run(manager.id, "run-failed", outcome="failed", exit_code=1)
  assert tree.dispatch.pending_inputs(manager.id) == []
  admitted = await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off. Next.", actor="user")
  backend = SpawningScriptedBackend([result_event("next round")])
  conftest.install_scripted_backends(monkeypatch, [backend], conftest.BUILD_BACKEND_PATCH_TARGET)
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision["launch"] is True
  next_run, _outcome = await wait_for_terminal_run(tree, manager.id, decision["run_id"])
  assert next_run.input_event_ids == [str(admitted["id"])]

  # The explicit retry of the failed manager round reruns that round's OWN
  # batch: the retry Run binds the original's input_event_ids, so it
  # launches with nothing pending and finishes with exactly that payload.
  retry = await tree.create_retry(manager.id, "retry-1", "run-failed")
  retry_run = await tree.runs.get_run(manager.id, retry["run_id"])
  assert retry_run is not None and retry_run.input_event_ids == [str(first_in["id"])]
  backend2 = SpawningScriptedBackend([result_event("retried")])
  conftest.install_scripted_backends(monkeypatch, [backend2], conftest.BUILD_BACKEND_PATCH_TARGET)
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision["launch"] is True and decision["run_id"] == retry["run_id"]
  retried, _retried_outcome = await wait_for_terminal_run(tree, manager.id, retry["run_id"])
  assert retried.input_event_ids == [str(first_in["id"])]
  finished = [
      e for e in tree.events.load_events(manager.id)
      if e["type"] == ET.RUN_FINISHED and e.get("run_id") == retry["run_id"]
  ]
  assert finished and finished[0]["input_event_ids"] == [str(first_in["id"])]

  # A stop request on a queued retry keeps THAT retry from ever launching;
  # the pending message still dispatches as a fresh round of its own.
  third_in = await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off. Again.", actor="user")
  retry2 = await tree.create_retry(manager.id, "retry-2", "run-failed")
  await tree.runs.request_stop(manager.id, retry2["run_id"], "stop-1")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision["launch"] is True and decision["run_id"] != retry2["run_id"]
  fresh_run, _fresh_outcome = await wait_for_terminal_run(tree, manager.id, decision["run_id"])
  assert fresh_run.input_event_ids == [str(third_in["id"])]
  stopped_run = await tree.runs.get_run(manager.id, retry2["run_id"])
  assert stopped_run is not None and stopped_run.pid is None
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(manager.id), retry2["run_id"]) is None
  assert tree.dispatch.pending_inputs(manager.id) == []


@pytest.mark.asyncio
async def test_first_terminal_fact_wins_governs_followups(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _session_blocks, tree = build_env(tmp_path, monkeypatch)
  root = await conftest.create_task(tree, parent=None, request_id="root")
  worker = await conftest.create_task(tree, parent=root.id, request_id="w", profile="worker")
  await tree.runs.register_run_locked(models.RunRecord(id="run-w", session_id=worker.id, kind="work"))
  # Two concurrent finishers: the recorded failed fact stands even though the
  # losing finisher passed outcome="success", so the worker never closes.
  await asyncio.gather(
      tree.dispatch.finish_run(worker.id, "run-w", outcome="failed", exit_code=1),
      tree.dispatch.finish_run(worker.id, "run-w", outcome="success", exit_code=0),
  )
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(worker.id), "run-w") == "failed"
  assert tree.task_state(worker.id) == "open"
  assert [e for e in tree.events.load_events(root.id) if e["type"] == ET.CHILD_REPORT] == []


# ---------------------------------------------------------------------------
# Bare-branch bases judge the local ref against origin (origin-tip starts, loud refusals)
# ---------------------------------------------------------------------------


def _advance_origin_from_second_clone(tmp_path: pathlib.Path, origin: pathlib.Path, filename: str) -> str:
  """Push one commit to the bare origin from a second clone; returns the new tip."""
  second = tmp_path / "second-clone"
  subprocess.run(["git", "clone", "-q", str(origin), str(second)], check=True)
  conftest.run_git(second, "config", "user.email", "t@example.com")
  conftest.run_git(second, "config", "user.name", "t")
  (second / filename).write_text("advance\n")
  conftest.run_git(second, "add", "-A")
  conftest.run_git(second, "commit", "-q", "-m", f"advance origin via {filename}")
  conftest.run_git(second, "push", "-q", "origin", "main")
  return conftest.run_git(second, "rev-parse", "refs/heads/main")


@pytest.mark.integration
@pytest.mark.asyncio
async def test_bare_branch_base_behind_starts_from_origin_tip(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The bare-branch base form when the shared checkout's local branch is only
    behind origin (the 2026-09-26 incident shape): the Run launches anyway, its
    work branch starts at the origin tip, and the local main ref is untouched."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch, backend_ids=["fake", "fake2"], pool=["fake", "fake2"])
  repo, origin = conftest.init_repo_with_origin(tmp_path)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  task_spec = {
      "goal": "## Goal\n\nadd a marker file\n",
      "repo_path": str(repo),
      "base_branch": "main",  # the bare form: local main is judged against origin/main
      "task_type": "implement",
      "keep_worktree": False,
  }
  worker = await conftest.create_task(
      tree, parent=manager.id, request_id="w", profile="worker", task=_task_spec(tree, task_spec))
  await _launch_manager_turn(cfg, session_blocks, tree, monkeypatch, manager, "Take off and implement the marker file.")

  # Origin gains a commit from a second clone while the fixture repo's local
  # main stays put: local main is strictly behind origin/main.
  origin_tip = _advance_origin_from_second_clone(tmp_path, origin, "ahead.txt")
  local_main = conftest.run_git(repo, "rev-parse", "main")
  assert local_main != origin_tip

  # The review lands the work and removes the work worktree. It runs on the
  # event loop right after the work Run ends, so the test holds it at its
  # backend's gate until it has read the worktree.
  work_backend = SpawningScriptedBackend([result_event("implemented")])
  review_released: asyncio.Event = asyncio.Event()
  review_backend = SpawningScriptedBackend([result_event("review ok")], gate=review_released.wait)
  conftest.install_scripted_backends(
      monkeypatch, [work_backend, review_backend], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)

  record = models.RunRecord(
      id="run-work",
      session_id=worker.id,
      kind="work",
      backend="fake",
      model="fake-model",
      repo_path=str(repo),
      base_branch="main")
  await tree.runs.register_run_locked(record, task_spec_text=task_spec["goal"])
  tree.dispatch.executor.launch(worker.id, "run-work")

  work_run, outcome = await wait_for_terminal_run(tree, worker.id, "run-work")
  assert outcome == "success"
  assert work_run.worktree_path
  work_wt = pathlib.Path(work_run.worktree_path)
  # A repository work Run starts in its worktree root, where the CLI finds the repo guide.
  assert work_backend.cwd == work_run.worktree_path
  # The work branch started at the new origin tip, never at the stale local main.
  assert conftest.run_git(work_wt, "rev-parse", "HEAD") == origin_tip
  assert conftest.run_git(repo, "rev-parse", "main") == local_main
  review_released.set()

  # The header the worker page projects for the launched Run: plain success,
  # both refs pointing at the files the launch produced.
  from src.runtime import worker_transcript
  entry = worker_transcript.load_worker_transcript(tree, worker.id)
  work_header = next(
      m for m in entry.projection.committed if m.get("kind") == ET.RUN_HEADER and m.get("run_id") == "run-work")
  assert work_header["state"] == "success"
  assert work_header["launched"] is True and work_header["launch_failed"] is False
  assert pathlib.Path(work_header["task_spec_ref"]).is_file()
  assert pathlib.Path(work_header["launch_prompt_ref"]).is_file()

  # The chain settles through the real delivery path: the work branch adds
  # nothing beyond its base, so the review lands it and the task completes.
  reports: list[dict] = []
  deadline = asyncio.get_event_loop().time() + 15
  while asyncio.get_event_loop().time() < deadline:
    reports = [e for e in tree.events.load_events(manager.id) if e["type"] == ET.CHILD_REPORT]
    if reports and reports[-1]["outcome"] == "completed":
      break
    await asyncio.sleep(0.05)
  assert reports and reports[-1]["outcome"] == "completed"
  await conftest._settle_parent(tree, manager, timeout=30.0, poll=0.2)


@pytest.mark.asyncio
async def test_bare_branch_base_with_unpushed_local_commit_launch_fails(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The case the bare-branch base check still refuses: the local branch holds
    commits absent from origin. The real launch path lands the durable failure,
    and the worker page's header reads "launch failed" with the error text
    exactly once, a recorded task spec, and no launch prompt."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  repo, _origin = conftest.init_repo_with_origin(tmp_path)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  task_spec = {
      "goal": "## Goal\n\nadd a marker file\n",
      "repo_path": str(repo),
      "base_branch": "main",
      "task_type": "implement",
      "keep_worktree": False,
  }
  worker = await conftest.create_task(
      tree, parent=manager.id, request_id="w", profile="worker", task=_task_spec(tree, task_spec))
  await _launch_manager_turn(cfg, session_blocks, tree, monkeypatch, manager, "Take off and implement the marker file.")

  # One unpushed commit on the fixture repo's local main: origin does not
  # have it, so the base check fails closed.
  (repo / "local_only.txt").write_text("local work\n")
  conftest.run_git(repo, "add", "-A")
  conftest.run_git(
      repo, "-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", "unpushed local commit")
  local_tip = conftest.run_git(repo, "rev-parse", "main")

  work_backend = SpawningScriptedBackend([result_event("never runs")])
  conftest.install_scripted_backends(monkeypatch, [work_backend], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)

  record = models.RunRecord(
      id="run-work",
      session_id=worker.id,
      kind="work",
      backend="fake",
      model="fake-model",
      repo_path=str(repo),
      base_branch="main")
  await tree.runs.register_run_locked(record, task_spec_text=task_spec["goal"])
  tree.dispatch.executor.launch(worker.id, "run-work")

  work_run, outcome = await wait_for_terminal_run(tree, worker.id, "run-work")
  assert outcome == "failed"
  # The real launch-failure path: the Run never spawned (no started_at, the
  # backend double never ran), and the durable evidence carries the error.
  assert work_run.started_at is None
  assert work_backend.prompt is None
  error_text = _read_error_event(tree.runs.run_dir(worker.id, "run-work") / "events.jsonl")
  assert "BaseBranchResolutionError" in error_text
  assert "differs from origin/main" in error_text
  assert local_tip[:12] in error_text
  assert "fast-forward" not in error_text

  from src.runtime import worker_transcript
  entry = worker_transcript.load_worker_transcript(tree, worker.id)
  messages = entry.projection.committed
  header = messages[0]
  assert header["kind"] == ET.RUN_HEADER
  assert header["state"] == "failed"
  assert header["content"].endswith("launch failed · 0 tool calls")
  assert header["launched"] is False and header["launch_failed"] is True
  assert header["error"] == error_text
  # The error text appears exactly once across the projection: only in the
  # header's error field, never as an ordinary row.
  assert [m for m in messages if error_text in str(m.get("content") or "")] == []
  assert [m for m in messages if m.get("error") == error_text] == [header]
  assert header["task_spec_ref"] != "" and pathlib.Path(header["task_spec_ref"]).is_file()
  assert header["launch_prompt_ref"] == ""  # never assembled: no process started

  # The failed launch reported to the parent through the real after-run path.
  report = await _wait_for_parent_report(tree, manager.id)
  assert report.get("outcome") == "failed"
  assert "differs from origin/main" in str(report.get("summary"))


# ---------------------------------------------------------------------------
# Repo-less work Runs: the Run directory is the working directory
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_repo_less_implement_delivers_after_review_passes(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch, backend_ids=["fake", "fake2"], pool=["fake", "fake2"])
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  task_spec = {
      "goal": "## Goal\n\nrefresh the host lint config\n\n## Acceptance Tests\n- config parses\n",
      "task_type": "implement",
  }
  worker = await conftest.create_task(
      tree, parent=manager.id, request_id="w", profile="worker", task=_task_spec(tree, task_spec))
  pm_builds = await _launch_manager_turn(
      cfg, session_blocks, tree, monkeypatch, manager, "Take off and refresh the host lint config.")

  report = (
      "Created: /tmp/lint/ruff.toml\n"
      "Modified: /tmp/lint/setup.cfg\n"
      "Acceptance tests: config parses — pass")
  work_backend = SpawningScriptedBackend([result_event(report)])
  review_backend = SpawningScriptedBackend([result_event("review ok")])
  conftest.install_scripted_backends(
      monkeypatch, [work_backend, review_backend], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)

  record = models.RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake", model="fake-model")
  await tree.runs.register_run_locked(record, task_spec_text=task_spec["goal"])
  tree.dispatch.executor.launch(worker.id, "run-work")

  work_run, outcome = await wait_for_terminal_run(tree, worker.id, "run-work")
  assert outcome == "success"
  # Repo-less: the Run directory is the working directory; no worktree or
  # branch is ever recorded.
  assert work_run.repo_path is None and work_run.worktree_path is None
  assert work_run.branch_name is None
  assert work_backend.cwd == str(tree.runs.run_dir(worker.id, "run-work"))

  # The implement work Run's review spawns without a repo and reads the
  # spec-plus-paths context, not a diff.
  review_run = await wait_for_review_terminal(tree, worker.id, timeout=10.0, what="the repo-less review")
  assert review_run.review_of_run_id == "run-work"
  assert review_run.repo_path is None and review_run.worktree_path is None
  prompt = review_backend.prompt
  assert "no diff to read" in prompt
  assert "nothing to merge or push" in prompt
  assert "## Acceptance Tests" in prompt
  assert "Created: /tmp/lint/ruff.toml" in prompt

  # The reviewer's verdict is the delivery: the task closes and reports
  # completed to the parent with no landing step.
  deadline = asyncio.get_event_loop().time() + 10
  while asyncio.get_event_loop().time() < deadline:
    if tree.task_state(worker.id) == "completed":
      break
    await asyncio.sleep(0.1)
  else:
    pytest.fail("the repo-less implement task never closed after its review passed")
  # The close and the report land in separate awaits of the delivery chain;
  # a loaded runner's poll can see the closed task before the report
  # append, so the report gets the same bounded wait the review did.
  deadline = asyncio.get_event_loop().time() + 10
  while asyncio.get_event_loop().time() < deadline:
    reports = [e for e in tree.events.load_events(manager.id) if e["type"] == ET.CHILD_REPORT]
    if reports:
      break
    await asyncio.sleep(0.1)
  else:
    pytest.fail("the repo-less implement task never reported to its parent")
  assert reports[-1]["outcome"] == "completed"

  await conftest._settle_parent(tree, manager, timeout=30.0, poll=0.2)
  assert pm_builds, "the delivered report never triggered a parent manager turn"
  assert tree.task_state(manager.id) == "open"


@pytest.mark.asyncio
async def test_repo_less_implement_review_failure_takes_the_failure_report(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch, backend_ids=["fake", "fake2"], pool=["fake", "fake2"])
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  task_spec = {"goal": "fix the host script", "task_type": "implement"}
  worker = await conftest.create_task(
      tree, parent=manager.id, request_id="w", profile="worker", task=_task_spec(tree, task_spec))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, make_pm_build("manager turn"))
  conftest.stub_credentials({"charliebot": {"access_key": "op-secret"}})
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off and fix it.", actor="user")

  work_backend = SpawningScriptedBackend([result_event("done; modified /tmp/x.sh")])
  # The review on the pool's other member fails (no result event: the durable
  # outcome is failed); no untried member remains, so the retry policy
  # exhausts and the existing blocked-report path reports to the parent.
  review_backend = SpawningScriptedBackend([])
  conftest.install_scripted_backends(
      monkeypatch, [work_backend, review_backend], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)

  record = models.RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake", model="fake-model")
  await tree.runs.register_run_locked(record, task_spec_text=task_spec["goal"])
  tree.dispatch.executor.launch(worker.id, "run-work")

  await wait_for_terminal_run(tree, worker.id, "run-work")
  deadline = asyncio.get_event_loop().time() + 15
  while asyncio.get_event_loop().time() < deadline:
    reports = [e for e in tree.events.load_events(manager.id) if e["type"] == ET.CHILD_REPORT]
    if reports and reports[-1]["outcome"] == "blocked" and tree.task_state(worker.id) == "archived":
      break
    await asyncio.sleep(0.1)
  else:
    pytest.fail("the failed repo-less review never reported blocked to the parent")
  assert "review of work run run-work failed on every configured reviewer backend" in reports[-1]["summary"]
  # The blocked report's delivery archived the stopped worker.
  assert tree.task_state(worker.id) == "archived"


@pytest.mark.asyncio
async def test_a_failed_worker_run_reports_once_and_archives_the_worker(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A run that fails while running: one failed report wakes the parent once,
    and the report's delivery archives the worker with the system archive fact."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  task_spec = {"goal": "say the phrase", "task_type": "quick-edit"}
  worker = await conftest.create_task(
      tree, parent=manager.id, request_id="w", profile="worker", task=_task_spec(tree, task_spec))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  pm_builds: list = []
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, make_pm_build("failure noted", pm_builds))
  conftest.stub_credentials({"charliebot": {"access_key": "op-secret"}})
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off and say it.", actor="user")
  conftest.install_scripted_backends(
      monkeypatch, [SpawningScriptedBackend([conftest.assistant_text_event("started")], exit_code=1)],
      conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)

  record = models.RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake", model="fake-model")
  await tree.runs.register_run_locked(record, task_spec_text=task_spec["goal"])
  tree.dispatch.executor.launch(worker.id, "run-work")

  await wait_for_terminal_run(tree, worker.id, "run-work")
  await poll_until(lambda: tree.task_state(worker.id) == "archived", timeout=15.0, what="the failed worker's archive")
  reports = conftest.child_reports(tree, manager.id)
  assert [r["outcome"] for r in reports] == ["failed"]
  closes = [e for e in tree.events.load_events(worker.id) if e["type"] == ET.TASK_CLOSED]
  assert len(closes) == 1
  fact = closes[0]
  assert fact["outcome"] == "archived"
  assert fact["actor"] == "system"
  assert fact.get("report_to") is None
  assert fact["request_id"] == "unsuccessful:run-work"
  assert fact["run_ids"] == ["run-work"]
  assert fact["summary"] == "run run-work ended failed; archived by the server"
  # One report, one wake: the parent's report turn built exactly one turn.
  await conftest._settle_parent(tree, manager, timeout=30.0, poll=0.2)
  assert len(pm_builds) == 1


@pytest.mark.asyncio
async def test_an_interrupted_worker_run_reports_once_and_archives_the_worker(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An interrupted run reports on the spot through the same failure entry
    (the report used to wait for the next restart's replay), and the report's
    delivery archives the worker."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  task_spec = {"goal": "say the phrase", "task_type": "quick-edit"}
  worker = await conftest.create_task(
      tree, parent=manager.id, request_id="w", profile="worker", task=_task_spec(tree, task_spec))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  pm_builds: list = []
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, make_pm_build("stop noted", pm_builds))
  conftest.stub_credentials({"charliebot": {"access_key": "op-secret"}})

  record = models.RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake", model="fake-model")
  await tree.runs.register_run_locked(record, task_spec_text=task_spec["goal"])
  await tree.dispatch.finish_run(worker.id, "run-work", outcome="interrupted", exit_code=-1)
  # The durable interrupted fact drives exactly the finish chain the live
  # finalize runs.
  meta = await tree.load_meta(worker.id)
  fresh = await tree.runs.get_run(worker.id, "run-work")
  await tree.dispatch.executor._after_worker_run(meta, fresh, "interrupted")

  assert tree.task_state(worker.id) == "archived"
  reports = conftest.child_reports(tree, manager.id)
  assert [r["outcome"] for r in reports] == ["interrupted"]
  closes = [e for e in tree.events.load_events(worker.id) if e["type"] == ET.TASK_CLOSED]
  assert len(closes) == 1
  fact = closes[0]
  assert fact["outcome"] == "archived"
  assert fact["actor"] == "system"
  assert fact["request_id"] == "unsuccessful:run-work"
  assert fact["run_ids"] == ["run-work"]
  assert fact["summary"] == "run run-work ended interrupted; archived by the server"
  await conftest._settle_parent(tree, manager, timeout=30.0, poll=0.2)
  assert len(pm_builds) == 1


@pytest.mark.asyncio
async def test_a_failed_review_with_a_queued_retry_reports_nothing_and_stays_open(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A failed review whose next-backend retry is already queued reports
    nothing and archives nothing: the retry's own finish drives the chain."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  # Two reviewer entries beyond the worker's own backend: the failed first
  # attempt leaves a second one to retry on.
  import conftest as conftest_module
  cfg.backends.options.extend(
      [
          conftest_module.backend_option(id="fake2", label="Fake2", type="codex", model="fake2-model"),
          conftest_module.backend_option(id="fake3", label="Fake3", type="codex", model="fake3-model"),
      ])
  cfg.backends.pool = ["fake2", "fake3"]
  task_spec = {"goal": "say the phrase", "task_type": "implement"}
  worker = await conftest.create_task(
      tree, parent=manager.id, request_id="w", profile="worker", task=_task_spec(tree, task_spec))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  # The settled retry's completion close reports and wakes the parent; script
  # that turn so no unstubbed build outlives the test.
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, make_pm_build("manager turn"))
  gate = asyncio.Event()
  install_worker_launch_and_resume_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("review ok")], gate=gate.wait)])

  work = models.RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake", model="fake-model")
  await tree.runs.register_run_locked(work, task_spec_text=task_spec["goal"])
  await tree.dispatch.finish_run(worker.id, "run-work", outcome="success", exit_code=0)
  review = models.RunRecord(
      id="run-review-1",
      session_id=worker.id,
      kind="review",
      review_of_run_id="run-work",
      backend="fake2",
      model="fake2-model")
  await tree.runs.register_run_locked(review, task_spec_text="review of work run run-work")
  await tree.dispatch.finish_run(worker.id, "run-review-1", outcome="failed", exit_code=1)

  meta = await tree.load_meta(worker.id)
  fresh = await tree.runs.get_run(worker.id, "run-review-1")
  await tree.dispatch.executor._after_review_run(meta, fresh, "failed")

  # The retry launched (its gated backend holds it active): no report, no
  # close fact, the node open.
  reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
  assert len(reviews) == 2
  retried = next(r for r in reviews if r.id != "run-review-1")
  assert retried.backend == "fake3"
  assert conftest.child_reports(tree, manager.id) == []
  assert [e for e in tree.events.load_events(worker.id) if e["type"] == ET.TASK_CLOSED] == []
  assert tree.task_state(worker.id) == "open"

  # Let the gated retry finish so no launch task outlives the test.
  gate.set()
  await wait_for_terminal_run(tree, worker.id, retried.id)
  await conftest._settle_parent(tree, manager, timeout=30.0, poll=0.2)


@pytest.mark.asyncio
async def test_an_improve_childs_failed_run_keeps_the_loop_controllers_close_path(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A run a sequence controller launched (the improve loop's iteration)
    reports and archives through its controller alone: the worker failure
    entry never fires for it."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  task_spec = {"goal": "tighten the loop", "task_type": "implement"}
  worker = await conftest.create_task(
      tree, parent=manager.id, request_id="w", profile="worker", task=_task_spec(tree, task_spec))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  owner_ref = str(pathlib.Path(cfg.sessions_dir) / manager.id / "loops" / "1")
  record = models.RunRecord(
      id="run-iter",
      session_id=worker.id,
      kind="work",
      backend="fake",
      model="fake-model",
      sequence_ref=models.SequenceRef(kind="improve", owner_ref=owner_ref, position=0))
  await tree.runs.register_run_locked(record, task_spec_text=task_spec["goal"])
  await tree.dispatch.finish_run(worker.id, "run-iter", outcome="failed", exit_code=1)

  meta = await tree.load_meta(worker.id)
  fresh = await tree.runs.get_run(worker.id, "run-iter")
  await tree.dispatch.executor._after_worker_run(meta, fresh, "failed")

  # The loop's close step found no ended loop (the ref names none), and the
  # generic failure entry never ran: no report, no archive, the node open.
  assert conftest.child_reports(tree, manager.id) == []
  assert [e for e in tree.events.load_events(worker.id) if e["type"] == ET.TASK_CLOSED] == []
  assert tree.task_state(worker.id) == "open"


@pytest.mark.asyncio
async def test_repo_less_quick_edit_closes_without_review(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  task_spec = {"goal": "bump the host cron schedule line", "task_type": "quick-edit"}
  worker = await conftest.create_task(
      tree, parent=manager.id, request_id="w", profile="worker", task=_task_spec(tree, task_spec))
  pm_builds = await _launch_manager_turn(cfg, session_blocks, tree, monkeypatch, manager, "Take off and bump it.")

  work_backend = SpawningScriptedBackend([result_event("modified /etc/cron.d/sweep")])
  conftest.install_scripted_backends(monkeypatch, [work_backend], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)

  record = models.RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake", model="fake-model")
  await tree.runs.register_run_locked(record, task_spec_text=task_spec["goal"])
  tree.dispatch.executor.launch(worker.id, "run-work")

  await wait_for_terminal_run(tree, worker.id, "run-work")
  deadline = asyncio.get_event_loop().time() + 10
  while asyncio.get_event_loop().time() < deadline:
    if tree.task_state(worker.id) == "completed":
      break
    await asyncio.sleep(0.1)
  else:
    pytest.fail("the repo-less quick-edit task never closed on its successful work Run")
  # No review Run exists for a quick-edit delivery.
  review_runs = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
  assert review_runs == []
  # The close fact and the parent report are two appends under one lock
  # (_append_closed); this lock-free read of the closed state can fall between
  # them, so the report is waited for on its own.
  await poll_until(lambda: conftest.child_reports(tree, manager.id), timeout=15.0, what="the completed child report")
  assert conftest.child_reports(tree, manager.id)[-1]["outcome"] == "completed"

  await conftest._settle_parent(tree, manager, timeout=30.0, poll=0.2)
  assert pm_builds, "the delivered report never triggered a parent manager turn"


@pytest.mark.asyncio
async def test_a_follow_up_upload_on_a_worker_node_reaches_its_continuation_prompt(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An upload admitted while a worker run holds its slot pauses that run. The
    continuation resumes the run's native conversation, and its prompt names the
    upload's path."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  task_spec = {"goal": "## Goal\n\nbump the host cron schedule line\n", "task_type": "quick-edit"}
  worker = await conftest.create_task(
      tree, parent=manager.id, request_id="w", profile="worker", task=_task_spec(tree, task_spec))
  await _launch_manager_turn(cfg, session_blocks, tree, monkeypatch, manager, "Take off and bump it.")

  work_started = asyncio.Event()
  work_gate = asyncio.Event()
  hold_work_run = signal_then_hold(work_started, work_gate)

  async def record_native_id() -> None:
    # The finalize scan reads the native session id from the run's raw log.
    work_run = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "work"][-1]
    init = {"type": "system", "subtype": "init", "session_id": "native-1"}
    (tree.runs.run_dir(worker.id, work_run.id) / runs_mod.RAW_LOG_NAME).write_text(
        json.dumps(init) + "\n", encoding="utf-8")

  first = SpawningScriptedBackend([result_event("did the work")], gate=hold_work_run, post_events=record_native_id)
  continuation = SpawningScriptedBackend([result_event("looked at the screenshot")])
  builds = conftest.install_scripted_backends(
      monkeypatch, [first, continuation], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)

  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run1_id = decision["run_id"]
  await asyncio.wait_for(work_started.wait(), timeout=15)

  # Admitted while the first run holds its slot: pending at that run's end.
  upload = {"filename": "screenshot.png", "path": str(tmp_path / "uploads" / "screenshot.png")}
  admitted = await tree.dispatch.admit_input(
      worker.id, event_type=ET.USER, content="see attached", actor="user", uploaded_files=[upload])
  work_gate.set()
  _run1, outcome1 = await wait_for_terminal_run(tree, worker.id, run1_id)
  assert outcome1 == "success"

  run2 = await _claimed_work_run_after(tree, worker.id, {run1_id}, timeout=15)
  _run2, outcome2 = await wait_for_terminal_run(tree, worker.id, run2.id)
  assert outcome2 == "success"
  assert run2.input_event_ids == [str(admitted["id"])]
  assert builds[1]["kwargs"].get("resume_session_id") == "native-1"
  assert upload["path"] in continuation.prompt


def _task_spec(tree: task_sessions.TaskTreeManager, spec: dict):
  return models.TaskSpec(
      goal=spec["goal"],
      context_refs=[],
      repo_path=spec.get("repo_path"),
      base_branch=spec.get("base_branch"),
      task_type=models.TaskType(spec.get("task_type", "implement")),
      keep_worktree=bool(spec.get("keep_worktree", False)),
  )


# ---------------------------------------------------------------------------
# Landing verification: the git layer behind landed: claims
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_landing_verification_negative_cases(tmp_path: pathlib.Path) -> None:
  from src.infra import git

  repo, _origin = conftest.init_repo_with_origin(tmp_path)
  base_sha = conftest.run_git(repo, "rev-parse", "HEAD")
  # The base commit IS landed on origin/main.
  landed, _ = await git.git_verify_commit_landed(repo, "origin/main", base_sha)
  assert landed is True
  # A fake hash fails existence.
  landed, reason = await git.git_verify_commit_landed(repo, "origin/main", "f" * 40)
  assert landed is False and "existence" in reason
  # A real but unmerged commit fails ancestry.
  conftest.run_git(repo, "checkout", "-q", "-b", "feature")
  (repo / "unmerged.txt").write_text("x\n")
  conftest.run_git(repo, "add", ".")
  conftest.run_git(repo, "commit", "-q", "-m", "unmerged")
  unmerged = conftest.run_git(repo, "rev-parse", "HEAD")
  landed, reason = await git.git_verify_commit_landed(repo, "origin/main", unmerged)
  assert landed is False and "ancestry" in reason
  # A commit on a different branch target fails.
  landed, _ = await git.git_verify_commit_landed(repo, "origin/main", base_sha)
  assert landed is True


@pytest.mark.asyncio
async def test_worker_automatic_close_with_forged_landing_ref_stays_open(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.runtime import task_completion

  _cfg, _session_blocks, tree = build_env(tmp_path, monkeypatch)
  repo, _origin = conftest.init_repo_with_origin(tmp_path)
  worker = await conftest.create_task(
      tree,
      parent=None,
      request_id="w",
      profile="worker",
      task=_task_spec(
          tree, {
              "goal": "## Goal\n\nwork\n",
              "repo_path": str(repo),
              "base_branch": "main",
              "task_type": "implement"
          }))
  await tree.runs.register_run_locked(
      models.RunRecord(
          id="run-work",
          session_id=worker.id,
          kind="work",
          backend="fake",
          model="fake-model",
          repo_path=str(repo),
          base_branch="main"))
  review = models.RunRecord(
      id="run-review",
      session_id=worker.id,
      kind="review",
      backend="fake",
      model="fake-model",
      repo_path=str(repo),
      base_branch="main",
      review_of_run_id="run-work")
  await tree.runs.register_run_locked(review, task_spec_text="review of work run run-work")
  await tree.dispatch.finish_run(worker.id, "run-work", outcome="success")
  await tree.dispatch.finish_run(worker.id, "run-review", outcome="success")
  # A real commit that exists but is NOT on the target branch: ancestry must
  # reject it just as strictly as a nonexistent hash.
  conftest.run_git(repo, "checkout", "-q", "-b", "side-work")
  (repo / "unlanded.txt").write_text("x\n")
  conftest.run_git(repo, "add", "-A")
  conftest.run_git(repo, "-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", "unlanded")
  unlanded = conftest.run_git(repo, "rev-parse", "HEAD")
  conftest.run_git(repo, "checkout", "-q", "main")
  evidence = task_completion.CompletionEvidence(
      summary="forged",
      result_refs=["run:run-work", f"landed:main@{unlanded}"],
      run_ids=["run-work"],
      review_run_ids=["run-review"])
  with pytest.raises(task_errors.TaskConflictError, match="landing evidence unverified"):
    await tree.completion.evaluate_automatic_completion(
        worker.id, run_id="run-work", request_id="auto-1", evidence=evidence)
  assert tree.task_state(worker.id) == "open"


# ---------------------------------------------------------------------------
# The context stage: one assembler at the actual launch boundary, snapshot
# evidence, native continuation, and preparation failures
# ---------------------------------------------------------------------------


def _snapshot_of(run: models.RunRecord) -> dict:
  assert run.prompt_snapshot_ref is not None, "the launched Run has no snapshot reference"
  return json.loads(pathlib.Path(run.prompt_snapshot_ref).read_text(encoding="utf-8"))


def _manager_backend(monkeypatch: pytest.MonkeyPatch, tree, cfg, session_blocks, *,
                     events: list[dict]) -> tuple[list[SpawningScriptedBackend], list[dict]]:
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  backend = SpawningScriptedBackend(events)
  builds = conftest.install_scripted_backends(monkeypatch, [backend], conftest.BUILD_BACKEND_PATCH_TARGET)
  return [backend], builds


async def _admit_and_dispatch(tree, session_id: str, content: str, request_id: str) -> str:
  await tree.dispatch.admit_input(session_id, event_type=ET.USER, content=content, actor="user")
  decision = await tree.dispatch.dispatch_pending(session_id)
  assert decision.get("launch") is True, decision
  return decision["run_id"]


@pytest.mark.asyncio
async def test_manager_turn_launch_delivers_the_snapshot_bytes(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The real CLI/API execution path uses the assembler: the backend receives
    exactly the committed snapshot's joined instruction bytes and the composed
    input, and the Run carries the snapshot/hash/char_count evidence."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  await tree.patch_task(
      manager.id, models.PatchSessionTaskRequest(node_prompt="the node rule"), caller=conftest.OPERATOR)
  _backends, builds = _manager_backend(
      monkeypatch, tree, cfg, session_blocks, events=[result_event("manager turn done")])

  run_id = await _admit_and_dispatch(tree, manager.id, "Take off.", "in-1")
  await wait_for_terminal_run(tree, manager.id, run_id)

  run = await tree.runs.get_run(manager.id, run_id)
  assert run is not None
  stored = _snapshot_of(run)
  joined = "\n\n".join(b["text"] for b in stored["blocks"])
  assert stored["char_count"] == len(joined)
  # The delivery boundary got exactly the saved bytes.
  assert builds[0]["kwargs"]["instructions_content"] == joined
  # The managed blocks name their real origins, including the inherited rule.
  scope_refs = [(s["scope"], s["source_ref"], s["source_session_id"]) for b in stored["blocks"] for s in b["sources"]]
  assert ("base", "prompts/task_base.md", None) in scope_refs
  assert ("base", "prompts/task_manager.md", None) in scope_refs
  node_meta = await tree.load_meta(manager.id)
  assert ("node", f"prompt_bodies/{node_meta.node_prompt_ref}.md", manager.id) in scope_refs
  # The memory index header rides the memory scope, selected by the owner.
  memory_blocks = [b for b in stored["blocks"] if any(s["scope"] == "memory" for s in b["sources"])]
  # (A synthetic home has no memory store: the memory scope is absent, not fabricated.)
  assert memory_blocks == []
  # The task/input context is separate evidence: the composed input, not the rules.
  launch_text = (tree.runs.run_dir(manager.id, run_id) / "launch_prompt.md").read_text(encoding="utf-8")
  assert "Take off." in launch_text
  assert "the node rule" not in launch_text


def test_the_voice_note_opens_a_dictated_user_input_only() -> None:
  meta = models.SessionMetadata(id="node", name="node", profile="manager")
  dictated = {"type": ET.USER, "content": "said aloud", "input_mode": "voice"}
  typed = {"type": ET.USER, "content": "typed in"}
  relayed = {"type": ET.AGENT_MESSAGE, "content": "from a peer", "from_session_name": "peer"}

  prompt, _uploads = task_input_prompt_module.compose_input_prompt(meta, [dictated, typed, relayed])

  assert prompt == "\n\n".join(
      [f"{turn_contribution.VOICE_NOTE}\nsaid aloud", "typed in", "[Message from session peer] from a peer"])


def test_compose_input_prompt_puts_the_attachment_block_after_the_text() -> None:
  meta = models.SessionMetadata(id="node", name="node", profile="manager")
  first, second = "/sessions/node/uploads/one.png", "/sessions/node/uploads/two.png"
  event = {"type": ET.USER, "content": "look at these", "uploaded_files": [{"path": first}, {"path": second}]}

  prompt, uploads = task_input_prompt_module.compose_input_prompt(meta, [event])

  assert prompt == (
      f"look at these{message_events._ATTACHED_FILES_MARKER}{message_events.DEFAULT_ATTACHED_FILES_PROMPT}"
      f"\n- {first}\n- {second}")
  assert uploads == event["uploaded_files"]


def test_compose_input_prompt_gives_an_upload_only_event_the_block_alone() -> None:
  meta = models.SessionMetadata(id="node", name="node", profile="manager")
  path = "/sessions/node/uploads/shot.png"
  event = {"type": ET.USER, "content": "", "uploaded_files": [{"path": path}]}

  prompt, uploads = task_input_prompt_module.compose_input_prompt(meta, [event])

  assert prompt.startswith("[Attached files]")
  assert path in prompt
  assert uploads == event["uploaded_files"]


@pytest.mark.asyncio
async def test_a_dictated_message_launches_a_manager_turn_whose_prompt_opens_with_the_voice_note(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  backend = conftest.ScriptedRelayBackend([result_event("heard")], exit_code=0)
  conftest.install_scripted_backends(monkeypatch, [backend], conftest.BUILD_BACKEND_PATCH_TARGET)

  await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="open the settings page", actor="user", input_mode="voice")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision.get("launch") is True, decision
  _run, outcome = await wait_for_terminal_run(tree, manager.id, decision["run_id"])

  assert outcome == "success"
  assert backend.prompt == f"{turn_contribution.VOICE_NOTE}\nopen the settings page"


class _UploadRecordingBackend(conftest.ScriptedRelayBackend):
  """A scripted relay double that keeps the upload list its run() received."""

  def __init__(self, events: list[dict], exit_code: int) -> None:
    super().__init__(events, exit_code)
    self.uploaded_files: list[dict] | None = None

  async def run(self,
                prompt: str,
                cwd: str,
                env: dict,
                uploaded_files: list[dict] | None = None) -> AsyncIterator[dict]:
    self.uploaded_files = uploaded_files
    async for event in super().run(prompt, cwd, env, uploaded_files):
      yield event


@pytest.mark.asyncio
async def test_an_upload_only_message_launches_a_manager_turn_that_names_every_upload(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _session_blocks, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
  backend = _UploadRecordingBackend([result_event("saw both screenshots")], exit_code=0)
  conftest.install_scripted_backends(monkeypatch, [backend], conftest.BUILD_BACKEND_PATCH_TARGET)
  uploads = [{"filename": f"shot-{i}.png", "path": str(tmp_path / "uploads" / f"shot-{i}.png")} for i in (1, 2)]

  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="", actor="user", uploaded_files=uploads)
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision.get("launch") is True, decision
  _run, outcome = await wait_for_terminal_run(tree, manager.id, decision["run_id"])

  assert outcome == "success"
  for upload in uploads:
    assert upload["path"] in backend.prompt
  assert [f["path"] for f in backend.uploaded_files] == [upload["path"] for upload in uploads]


@pytest.mark.asyncio
async def test_worker_run_kinds_deliver_snapshot_bytes_and_task_context(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Work, review, verify, iteration and scheduled-step launches ride the same
    assembler with their applicable contracts and separate task/input context."""
  _cfg, _session_blocks, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
  repo, _origin = conftest.init_repo_with_origin(tmp_path)
  worker = await conftest.create_task(
      tree,
      parent=manager.id,
      profile="worker",
      request_id="w",
      task=models.TaskSpec(goal="ship it", repo_path=str(repo), task_type="script-run"))
  builds: list[dict] = []
  scripted: list[SpawningScriptedBackend] = []

  def build(option, cfg_, **kwargs):
    b = SpawningScriptedBackend([result_event("worker done")])
    if kwargs.get("on_spawn") is not None:
      b.set_on_spawn(kwargs["on_spawn"])
    builds.append({"option": option, "kwargs": kwargs})
    scripted.append(b)
    return b

  monkeypatch.setattr(conftest.WORKER_BUILD_BACKEND_PATCH_TARGET, build)

  # The delegation gate needs a real user authorization along the ancestor chain.
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="take off", actor="user")

  # Work launch: worker contract + bindings context.
  run_id = await _admit_and_dispatch(tree, worker.id, "do the thing", "in-1")
  await wait_for_terminal_run(tree, worker.id, run_id)
  run = await tree.runs.get_run(worker.id, run_id)
  assert run is not None
  stored = _snapshot_of(run)
  joined = "\n\n".join(b["text"] for b in stored["blocks"])
  assert builds[0]["kwargs"]["instructions_content"] == joined
  scope_refs = [s["source_ref"] for b in stored["blocks"] for s in b["sources"]]
  assert "prompts/worker.md" in scope_refs
  assert "prompts/task_manager.md" not in scope_refs
  launch_text = (tree.runs.run_dir(worker.id, run_id) / "launch_prompt.md").read_text(encoding="utf-8")
  assert "ship it" in launch_text
  assert "do the thing" in launch_text
  assert "isolated sandbox" in launch_text  # the script-run bindings context
  assert "This is a script-run task" not in launch_text  # that rule is managed
  # A repo-backed script-run task gets its sandbox worktree, recorded on the Run.
  assert run.worktree_path is not None
  assert run.branch_name is not None

  # Verify contract: the verify task's managed block is the verify contract.
  verify_task = await conftest.create_task(
      tree,
      parent=manager.id,
      profile="worker",
      request_id="v",
      task=models.TaskSpec(goal="verify it", task_type="verify"))
  await tree.dispatch.admit_input(verify_task.id, event_type=ET.USER, content="verify the result", actor="user")
  decision = await tree.dispatch.dispatch_pending(verify_task.id)
  assert decision.get("launch") is True
  await wait_for_terminal_run(tree, verify_task.id, decision["run_id"])
  vrun = await tree.runs.get_run(verify_task.id, decision["run_id"])
  assert vrun is not None
  vstored = _snapshot_of(vrun)
  vrefs = [s["source_ref"] for b in vstored["blocks"] for s in b["sources"]]
  assert "prompts/verify.md" in vrefs
  assert "prompts/worker.md" not in vrefs


@pytest.mark.asyncio
async def test_manager_native_continuation_gates_on_instruction_hash(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Same rules and backend ⇒ the anchor's conversation continues; a rule
    change starts a fresh native context carrying a reset notice; an
    input-only change does not reset anything."""
  _cfg, session_blocks, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
  first = SpawningScriptedBackend([result_event("turn one")])
  second = SpawningScriptedBackend([result_event("turn two")])
  third = SpawningScriptedBackend([result_event("turn three")])
  conftest.install_scripted_backends(monkeypatch, [first, second, third], conftest.BUILD_BACKEND_PATCH_TARGET)

  # Turn 1: no anchor — a fresh native context, identity recorded at spawn.
  run1 = await _admit_and_dispatch(tree, manager.id, "turn one", "in-1")
  await wait_for_terminal_run(tree, manager.id, run1)
  meta = await tree.load_meta(manager.id)
  assert meta.native_prompt_hash is not None and meta.native_backend == "fake"
  snapshot1 = _snapshot_of(await tree.runs.get_run(manager.id, run1))
  assert meta.native_prompt_hash == snapshot1["prompt_hash"]
  # The scripted backend never persists a native anchor; record one the way
  # the real turn-finish path does, so turn 2 has an anchor to continue.
  anchor_id = "native-anchor-1"
  await tree.record_native_anchor(
      manager.id, prompt_hash=snapshot1["prompt_hash"], backend="fake", model="fake-model", reset_anchor=False)
  # The conversation anchor changes only through its authorized channel: a
  # whole-object save's anchor reconciliation would correct it back to disk.
  await session_blocks.anchors.persist_cc_session_id(manager.id, anchor_id)

  # Turn 2 (input-only change): same instructions ⇒ the conversation continues
  # (no reset notice, anchor untouched).
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="turn two", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision.get("launch") is True
  run2 = decision["run_id"]
  await wait_for_terminal_run(tree, manager.id, run2)
  meta2 = await tree.load_meta(manager.id)
  assert meta2.cc_session_id == anchor_id  # the anchor persists across the input-only turn
  assert meta2.native_prompt_hash == snapshot1["prompt_hash"]
  launch2 = (tree.runs.run_dir(manager.id, run2) / "launch_prompt.md").read_text(encoding="utf-8")
  assert "Context reset" not in launch2

  # Turn 3 (a rule changed): fresh native context, history retained elsewhere.
  await tree.patch_task(
      manager.id, models.PatchSessionTaskRequest(node_prompt="changed rule"), caller=conftest.OPERATOR)
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="turn three", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision.get("launch") is True
  run3 = decision["run_id"]
  await wait_for_terminal_run(tree, manager.id, run3)
  meta3 = await tree.load_meta(manager.id)
  snapshot3 = _snapshot_of(await tree.runs.get_run(manager.id, run3))
  assert snapshot3["prompt_hash"] != snapshot1["prompt_hash"]
  assert meta3.native_prompt_hash == snapshot3["prompt_hash"]
  # The stale anchor was cleared at spawn (the fresh native context's own
  # conversation replaces it); the old transcript's history is untouched.
  assert meta3.cc_session_id is None or meta3.cc_session_id != anchor_id
  # The reset notice is the whole assembled note: the standing reason, the
  # task's goal, where earlier history lives, and the clone-style
  # read-the-log instruction.
  launch_text = (tree.runs.run_dir(manager.id, run3) / "launch_prompt.md").read_text(encoding="utf-8")
  note, sep, tail = launch_text.partition("\n\n")
  assert sep
  assert note == (
      "[Context reset: this task's managed instructions or sources changed since the "
      "previous turn, so this turn starts a fresh native conversation. "
      f"The task is: {meta3.name}. {session_fork.HISTORY_LOCATION_NOTE} {session_anchors.CONTEXT_RESET_INSTRUCTION}]")
  assert "turn three" in tail


@pytest.mark.asyncio
async def test_backend_identity_change_starts_a_fresh_native_context(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, session_blocks, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
  first = SpawningScriptedBackend([result_event("one")])
  second = SpawningScriptedBackend([result_event("two")])
  conftest.install_scripted_backends(monkeypatch, [first, second], conftest.BUILD_BACKEND_PATCH_TARGET)
  run1 = await _admit_and_dispatch(tree, manager.id, "one", "in-1")
  await wait_for_terminal_run(tree, manager.id, run1)
  snapshot1 = _snapshot_of(await tree.runs.get_run(manager.id, run1))
  anchor = "native-anchor-id"
  await tree.record_native_anchor(
      manager.id, prompt_hash=snapshot1["prompt_hash"], backend="fake", model="fake-model", reset_anchor=False)
  # The conversation anchor changes only through its authorized channel.
  await session_blocks.anchors.persist_cc_session_id(manager.id, anchor)
  # The anchor's recorded identity no longer matches (a backend switch
  # happened): the next turn cannot claim continuity over it. The identity
  # changes only through its authorized anchor channel — native_backend is an
  # anchor field now, so a whole-object save would be corrected back to disk.
  await session_blocks.anchors.persist_native_backend(manager.id, "some-other-backend")
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="two", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision.get("launch") is True
  await wait_for_terminal_run(tree, manager.id, decision["run_id"])
  meta = await tree.load_meta(manager.id)
  # The launch re-pinned the identity to the actual backend and the old
  # anchor cannot serve it.
  assert meta.native_backend == "fake"
  assert meta.cc_session_id is None or meta.cc_session_id != anchor
  # The reset note names the switch: the recorded producer, the backend that
  # ran, and where the earlier history lives.
  launch_text = (tree.runs.run_dir(manager.id, decision["run_id"]) / "launch_prompt.md").read_text(encoding="utf-8")
  note, sep, tail = launch_text.partition("\n\n")
  assert sep
  assert note == (
      "[Context reset: this session switched from backend some-other-backend to fake, "
      f"which starts its own conversation. The task is: {meta.name}. "
      f"{session_fork.HISTORY_LOCATION_NOTE} {session_anchors.CONTEXT_RESET_INSTRUCTION}]")
  assert "two" in tail


@pytest.mark.asyncio
async def test_switch_away_and_back_before_next_turn_continues_native_conversation(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A mis-click — the backend switched away and back before the next
    message — never resets the anchor: the recorded producer still names the
    current backend, so turn 2 continues turn 1's native conversation and the
    launch prompt carries no reset note."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch, backend_ids=["fake", "fake-2"])
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  first = SpawningScriptedBackend([result_event("turn one")])
  second = SpawningScriptedBackend([result_event("turn two")])
  conftest.install_scripted_backends(monkeypatch, [first, second], conftest.BUILD_BACKEND_PATCH_TARGET)

  run1 = await _admit_and_dispatch(tree, manager.id, "turn one", "in-1")
  await wait_for_terminal_run(tree, manager.id, run1)
  snapshot1 = _snapshot_of(await tree.runs.get_run(manager.id, run1))
  anchor = "native-anchor-mis-click"
  await tree.record_native_anchor(
      manager.id, prompt_hash=snapshot1["prompt_hash"], backend="fake", model="fake-model", reset_anchor=False)
  await session_blocks.anchors.persist_cc_session_id(manager.id, anchor)

  # The mis-click: switch away and back before the next message goes out.
  await session_blocks.lifecycle.switch_backend(manager.id, "fake-2")
  await session_blocks.lifecycle.switch_backend(manager.id, "fake")

  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="turn two", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision.get("launch") is True
  run2 = decision["run_id"]
  await wait_for_terminal_run(tree, manager.id, run2)
  meta = await tree.load_meta(manager.id)
  assert meta.cc_session_id == anchor  # the native conversation continued
  launch_text = (tree.runs.run_dir(manager.id, run2) / "launch_prompt.md").read_text(encoding="utf-8")
  assert "Context reset" not in launch_text
  assert "turn two" in launch_text


@pytest.mark.asyncio
async def test_missing_rule_fails_before_launch_and_leaves_input_unconsumed(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  await tree.patch_task(manager.id, models.PatchSessionTaskRequest(node_prompt="needed rule"), caller=conftest.OPERATOR)
  meta = await tree.load_meta(manager.id)
  body = cfg.charliebot_home / "prompt_bodies" / f"{meta.node_prompt_ref}.md"
  body.unlink()  # the rule vanished before the launch
  _backends, builds = _manager_backend(monkeypatch, tree, cfg, session_blocks, events=[result_event("never")])
  admitted = await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="launch me", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  run_id = decision["run_id"]
  observation = await tree.dispatch.executor.launch_and_settle(manager.id, run_id)
  # Definitely unlaunched: a withheld verdict names the reason; no process,
  # no terminal fact, and the input stays unconsumed.
  assert observation.withheld is not None
  assert "prompt preparation failed" in observation.withheld
  assert builds == []
  run = await tree.runs.get_run(manager.id, run_id)
  assert run is not None and run.pid is None
  # The input stays unconsumed: the queued run holds its claim (never
  # acknowledged), and re-dispatching keeps surfacing the reason.
  run = await tree.runs.get_run(manager.id, run_id)
  assert run.input_event_ids == [str(admitted["id"])]
  acks = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.TASK_INPUT_ACKNOWLEDGED]
  assert acks == []
  # Re-dispatching keeps surfacing the reason (the queued run re-attempts).
  observation2 = await tree.dispatch.executor.launch_and_settle(manager.id, run_id)
  assert observation2.withheld is not None and "prompt preparation failed" in observation2.withheld
  # Restoring the rule makes the next launch a real launch.
  body.write_text("needed rule", encoding="utf-8")
  observation3 = await tree.dispatch.executor.launch_and_settle(manager.id, run_id)
  assert observation3.withheld is None
  await wait_for_terminal_run(tree, manager.id, run_id)


@pytest.mark.asyncio
async def test_corrupt_rule_fails_before_launch_with_the_reason(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  await tree.patch_task(manager.id, models.PatchSessionTaskRequest(node_prompt="needed rule"), caller=conftest.OPERATOR)
  meta = await tree.load_meta(manager.id)
  body = cfg.charliebot_home / "prompt_bodies" / f"{meta.node_prompt_ref}.md"
  body.write_text("tampered bytes", encoding="utf-8")
  _backends, builds = _manager_backend(monkeypatch, tree, cfg, session_blocks, events=[result_event("never")])
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="launch me", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  observation = await tree.dispatch.executor.launch_and_settle(manager.id, decision["run_id"])
  assert observation.withheld is not None
  assert "corrupt" in observation.withheld
  assert builds == []


@pytest.mark.asyncio
async def test_recovery_after_rule_deletion_uses_the_original_snapshot(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Process recovery replays the Run's own saved snapshot/anchor — it never
    recomposes from current memory/templates; an explicit retry uses current sources."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  await tree.patch_task(
      manager.id, models.PatchSessionTaskRequest(node_prompt="original rule"), caller=conftest.OPERATOR)
  _backends, _builds = _manager_backend(monkeypatch, tree, cfg, session_blocks, events=[result_event("turn")])
  run_id = await _admit_and_dispatch(tree, manager.id, "go", "in-1")
  run, outcome = await wait_for_terminal_run(tree, manager.id, run_id)
  assert outcome == "success"
  stored = _snapshot_of(run)
  # The rule is deleted afterwards; the Run's stored snapshot still reads.
  meta = await tree.load_meta(manager.id)
  (cfg.charliebot_home / "prompt_bodies" / f"{meta.node_prompt_ref}.md").unlink()
  ctx_resp_snapshot = _snapshot_of(run)
  assert ctx_resp_snapshot == stored
  # The history endpoint serves the stored object without touching live sources.
  from src.runtime import task_prompts
  restored = task_prompts.PromptSnapshot.from_json_dict(stored)
  assert "original rule" in restored.instructions_text
  # An explicit retry is a new Run with current sources: restoring the body
  # first, then retrying, assembles fresh (hash changes with the new body).
  (cfg.charliebot_home / "prompt_bodies" / f"{meta.node_prompt_ref}.md").write_text("rewritten rule", encoding="utf-8")
  retry = await tree.create_retry(manager.id, "retry-1", run_id)
  assert retry["run_id"] != run_id


@pytest.mark.asyncio
async def test_snapshot_publish_failure_is_a_definitely_unlaunched_preparation_failure(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The snapshot's durable write is part of preparation: a failure there is a
    withheld verdict (no backend invocation, no Run process, input unconsumed)."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  await tree.patch_task(manager.id, models.PatchSessionTaskRequest(node_prompt="the rule"), caller=conftest.OPERATOR)
  _backends, builds = _manager_backend(monkeypatch, tree, cfg, session_blocks, events=[result_event("never")])
  from src.infra import json_utils
  real_atomic = json_utils.atomic_write_text
  calls = {"n": 0}

  def failing_atomic(path, text):
    if str(path).endswith("prompt_snapshot.json"):
      calls["n"] += 1
      raise OSError("injected snapshot publish failure")
    return real_atomic(path, text)

  monkeypatch.setattr(json_utils, "atomic_write_text", failing_atomic)
  admitted = await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="launch me", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  observation = await tree.dispatch.executor.launch_and_settle(manager.id, decision["run_id"])
  assert observation.withheld is not None
  assert "failed-to-start" in observation.withheld
  assert "injected snapshot publish failure" in observation.withheld
  assert builds == []  # the backend was never invoked
  run = await tree.runs.get_run(manager.id, decision["run_id"])
  assert run is not None and run.pid is None
  # The input stays unconsumed by the queued run.
  assert run.input_event_ids == [str(admitted["id"])]


@pytest.mark.asyncio
async def test_own_subtree_rule_launch_parity_and_edit_boundary(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The own-subtree scope holds at the actual launch boundary.

    A manager with its own subtree rule launches: the committed snapshot equals
    the next-start preview byte for byte (hash parity) and carries the own
    rule. Editing that rule changes the node's AND a descendant's next-start
    preview while the current Run's stored snapshot stays fixed.
    """
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  child = await conftest.create_task(tree, parent=manager.id, request_id="child")
  await tree.patch_task(
      manager.id, models.PatchSessionTaskRequest(subtree_prompt="program-wide rule"), caller=conftest.OPERATOR)
  _backends, _builds = _manager_backend(
      monkeypatch, tree, cfg, session_blocks, events=[result_event("manager turn done")])

  run_id = await _admit_and_dispatch(tree, manager.id, "Take off.", "in-1")
  await wait_for_terminal_run(tree, manager.id, run_id)
  run = await tree.runs.get_run(manager.id, run_id)
  assert run is not None
  stored = _snapshot_of(run)
  scope_refs = [(s["scope"], s["source_session_id"]) for b in stored["blocks"] for s in b["sources"]]
  assert ("subtree", manager.id) in scope_refs
  joined = "\n\n".join(b["text"] for b in stored["blocks"])
  assert "program-wide rule" in joined

  # Hash/snapshot parity: the preview API's assembly equals the committed bytes.
  from src.runtime import task_prompts
  meta = await tree.load_meta(manager.id)
  index = await tree._get_index()
  chain, node_ref = task_execution_module.capture_prompt_chain(tree, index, meta)
  segments, _err = task_prompts.build_segments(cfg, meta, "manager_turn", chain=chain, node_ref=node_ref, overlay=None)
  preview = task_prompts.assemble_snapshot(segments)
  assert preview.to_json_dict() == stored

  # Editing the own subtree rule: the next-start preview changes for the node
  # itself and for its descendant, while the current Run's snapshot stays fixed.
  await tree.patch_task(
      manager.id, models.PatchSessionTaskRequest(subtree_prompt="program-wide rule v2"), caller=conftest.OPERATOR)
  meta = await tree.load_meta(manager.id)
  index = await tree._get_index()
  chain, node_ref = task_execution_module.capture_prompt_chain(tree, index, meta)
  segments, _err = task_prompts.build_segments(cfg, meta, "manager_turn", chain=chain, node_ref=node_ref, overlay=None)
  next_preview = task_prompts.assemble_snapshot(segments)
  assert next_preview.prompt_hash != stored["prompt_hash"]
  assert "program-wide rule v2" in "\n\n".join(b.text for b in next_preview.blocks)

  child_meta = await tree.load_meta(child.id)
  chain, node_ref = task_execution_module.capture_prompt_chain(tree, index, child_meta)
  segments, _err = task_prompts.build_segments(
      cfg, child_meta, "manager_turn", chain=chain, node_ref=node_ref, overlay=None)
  child_preview = task_prompts.assemble_snapshot(segments)
  assert "program-wide rule v2" in "\n\n".join(b.text for b in child_preview.blocks)
  # The finished Run's evidence is immutable history.
  assert _snapshot_of(run) == stored


# ---------------------------------------------------------------------------
# Launch failure past admission: a durable failed run, its error evidence, and
# the parent report a failed process would produce
# ---------------------------------------------------------------------------


def _read_error_event(events_log: pathlib.Path) -> str:
  """The run's error-event text from its events log (the leaf view's source)."""
  if not events_log.is_file():
    return ""
  for line in events_log.read_text(encoding="utf-8").splitlines():
    if not line.strip():
      continue
    event = json.loads(line)
    if event.get("type") == ET.ERROR:
      return str(event.get("message") or event.get("content") or "")
  return ""


def wait_for_terminal_run_sync(
    tree: task_sessions.TaskTreeManager,
    session_id: str,
    run_id: str,
    timeout: float = 20.0) -> tuple[models.RunRecord, str]:
  """Poll one Run's durable state without touching the launch's event loop.

    The delegate endpoint schedules the Run on the API client's portal loop;
    awaiting tree APIs from the test's own loop would chain futures across
    loops, so this poller reads only the sync, on-disk views.
    """
  deadline = time.monotonic() + timeout
  while time.monotonic() < deadline:
    run = tree.runs.read_run_sync(session_id, run_id)
    assert run is not None, f"run {run_id} vanished"
    events = tree.runs.load_events_sync(session_id)
    outcome = tree.runs.terminal_outcome(events, run_id)
    if outcome is not None:
      return run, str(outcome)
    time.sleep(0.05)
  pytest.fail(f"run {run_id} never reached a terminal fact within {timeout}s")


async def _wait_for_parent_report(tree: task_sessions.TaskTreeManager, parent_id: str, timeout: float = 15.0) -> dict:
  """Poll the parent's fact history until a delivered child_report lands."""
  deadline = asyncio.get_event_loop().time() + timeout
  while asyncio.get_event_loop().time() < deadline:
    reports = [e for e in tree.events.load_events(parent_id) if e.get("type") == ET.CHILD_REPORT]
    if reports:
      return reports[-1]
    await asyncio.sleep(0.1)
  pytest.fail(f"the parent {parent_id} never received a failure report within {timeout}s")


@pytest.mark.asyncio
async def test_worktree_preparation_failure_lands_failed_run_and_reports_to_parent(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The worktree-prep failure case, exactly: a synthetic repo whose local
    base branch is ahead of its origin makes _prepare_worktree raise after the
    Run was admitted. That Run lands its durable failed fact, keeps the actual
    error as readable evidence in its own events log, and the worker's parent
    receives a failure report naming the error and is woken."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  conftest.stub_credentials({"charliebot": {"access_key": "op-secret"}})

  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, make_pm_build("failure noted"))
  conftest.install_scripted_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("phrase")])], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)

  repo, _origin = conftest.init_repo_with_origin(tmp_path / "prep-fail")
  # The failure's exact shape: the local base branch diverges from origin.
  (repo / "local.txt").write_text("local work\n")
  conftest.run_git(repo, "add", ".")
  conftest.run_git(repo, "commit", "-q", "-m", "local-only")

  await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="Take off and delegate the phrase task.", actor="user")
  monkeypatch.setattr(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  # The client context stays open across the waits: the launches are tasks on
  # the client's portal loop, alive exactly while the block stands.
  with make_api_client(cfg, session_blocks, tree) as client:
    resp = client.post(
        "/api/internal/delegate",
        json={
            "session_id": manager.id,
            "description": "## Goal\n\nsay the phrase\n",
            "task_type": "quick-edit",
            "keep_worktree": False,
            "repo_path": str(repo),
            "base_branch": "main",
        },
        headers=OP_HEADERS)
    assert resp.status_code == 200, resp.text
    body = resp.json()
    child_id, run_id = body["session_id"], body["run_id"]

    run, outcome = wait_for_terminal_run_sync(tree, child_id, run_id)
    assert outcome == "failed"
    assert run.pid is None, "the process never started; the failure is a launch failure"
    # The leaf card's Events link reads the record's events_ref: the
    # launch-failed run's evidence must be reachable the same way a
    # process run's is.
    assert run.events_ref is not None and run.events_ref.endswith("events.jsonl"), (
        f"the run's evidence ref was never set: {run.events_ref!r}")
    error_text = _read_error_event(tree.runs.run_dir(child_id, run_id) / "events.jsonl")
    assert "differs from origin/main" in error_text, (
        f"the run's durable evidence must name the actual error, got: {error_text!r}")

    report = await _wait_for_parent_report(tree, manager.id)
    assert report.get("outcome") == "failed"
    assert "differs from origin/main" in str(
        report.get("summary")), (f"the report summary must name the actual error, got: {report.get('summary')!r}")
    # The failed-before-start report's delivery archived the worker: the one
    # report, then the end state.
    await poll_until(
        lambda: tree.task_state(child_id) == "archived", timeout=15.0, what="the launch-failed worker's archive")
    assert len(conftest.child_reports(tree, manager.id)) == 1
    closes = [e for e in tree.events.load_events(child_id) if e["type"] == ET.TASK_CLOSED]
    assert len(closes) == 1
    assert closes[0]["outcome"] == "archived"
    assert closes[0]["actor"] == "system"
    assert closes[0]["request_id"] == f"unsuccessful:{run_id}"
    assert closes[0]["run_ids"] == [run_id]
    assert closes[0]["summary"] == f"run {run_id} ended failed; archived by the server"

    # The delivered report is the parent's new durable input: its next
    # serialized turn consumes it. All of that runs on the API client's
    # portal loop, so this wait polls the durable views instead of
    # awaiting anything the portal owns.
    deadline = time.monotonic() + 20
    parent_succeeded = False
    while time.monotonic() < deadline:
      pm_events = tree.runs.load_events_sync(manager.id)
      if any(tree.runs.terminal_outcome(pm_events, r.id) == "success"
             for r in tree.runs.list_run_records_sync(manager.id)):
        parent_succeeded = True
        break
      time.sleep(0.1)
    assert parent_succeeded, "the parent's report turn never ran to success"


def _explode_backend_resolution(cfg: config.CharlieBotConfig, backend: str | None, model: str | None) -> None:
  raise ValueError(f"backend {backend!r} is not configured")


@pytest.mark.asyncio
async def test_backend_resolution_failure_lands_the_manager_runs_durable_failure(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A manager turn gets the same guarantee: backend resolution is the first
    post-admission act, and its failure lands the Run's durable failed fact
    with the error as evidence — never a queued run stranded without a fact."""
  _cfg, _session_blocks, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)

  monkeypatch.setattr(conftest.SPAWNER_BACKENDS_RESOLVE_BACKEND_OPTION_PATCH_TARGET, _explode_backend_resolution)

  await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="Take off. Reply with the phrase.", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision["launch"] is True
  run_id = decision["run_id"]

  run, outcome = await wait_for_terminal_run(tree, manager.id, run_id)
  assert outcome == "failed"
  assert run.pid is None
  error_text = _read_error_event(tree.runs.run_dir(manager.id, run_id) / "events.jsonl")
  assert "is not configured" in error_text, (
      f"the run's durable evidence must name the actual error, got: {error_text!r}")
  # The claimed batch stays claimed by the failed round; nothing re-launches
  # behind the failure and the node is left with the fact, not a queued run.
  assert tree.dispatch.pending_inputs(manager.id) == []
  assert [r.id for r in tree.runs.list_run_records_sync(manager.id)] == [run_id]


def _turn_launch_notices(tree: task_sessions.TaskTreeManager, session_id: str) -> list[dict]:
  return [e for e in tree.fact_history(session_id) if e.get("source") == "turn_launch"]


@pytest.mark.asyncio
async def test_a_manager_turn_that_fails_to_launch_shows_one_error_row_on_the_page(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, session_blocks, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
  raised: list[ValueError] = []

  def explode(cfg, backend: str | None, model: str | None):
    raised.append(ValueError(f"backend {backend!r} is not configured"))
    raise raised[-1]

  monkeypatch.setattr(conftest.SPAWNER_BACKENDS_RESOLVE_BACKEND_OPTION_PATCH_TARGET, explode)
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off.", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  run, outcome = await wait_for_terminal_run(tree, manager.id, decision["run_id"])
  assert outcome == "failed"

  notices = _turn_launch_notices(tree, manager.id)
  assert len(notices) == 1, notices
  notice = notices[0]
  assert notice["type"] == ET.ERROR and notice["run_id"] == run.id
  assert notice["content"].startswith("Turn did not start: ")
  assert "is not configured" in notice["content"]
  boot = await message_utils.build_session_bootstrap_data(manager.id, session_blocks.store, session_blocks.events)
  rows = [m for m in boot.messages if str(m.get("content")).startswith("Error: Turn did not start: ")]
  assert [m["role"] for m in rows] == ["system"]

  # Landing the same failure again finds the event and writes nothing.
  meta = await tree.load_meta(manager.id)
  await tree.dispatch.executor._land_launch_failure(meta, run, raised[0])
  assert _turn_launch_notices(tree, manager.id) == [notice]


@pytest.mark.asyncio
async def test_a_withheld_manager_turn_shows_one_error_row_however_often_it_is_withheld(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  await tree.patch_task(manager.id, models.PatchSessionTaskRequest(node_prompt="needed rule"), caller=conftest.OPERATOR)
  meta = await tree.load_meta(manager.id)
  body = cfg.charliebot_home / "prompt_bodies" / f"{meta.node_prompt_ref}.md"
  body.unlink()  # the rule vanished before the launch
  _manager_backend(monkeypatch, tree, cfg, session_blocks, events=[result_event("never")])
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="launch me", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)

  observation = await tree.dispatch.executor.launch_and_settle(manager.id, decision["run_id"])
  assert observation.withheld is not None and "prompt preparation failed" in observation.withheld
  notices = _turn_launch_notices(tree, manager.id)
  assert len(notices) == 1, notices
  assert notices[0]["content"].startswith("Turn did not start: ")

  again = await tree.dispatch.executor.launch_and_settle(manager.id, decision["run_id"])
  assert again.withheld == observation.withheld
  assert _turn_launch_notices(tree, manager.id) == notices


@pytest.mark.asyncio
async def test_a_worker_launch_failure_writes_no_turn_launch_error(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  task_spec = {"goal": "bump the host cron schedule line", "task_type": "quick-edit"}
  worker = await conftest.create_task(
      tree, parent=manager.id, request_id="w", profile="worker", task=_task_spec(tree, task_spec))
  await _launch_manager_turn(cfg, session_blocks, tree, monkeypatch, manager, "Take off and bump it.")

  monkeypatch.setattr(conftest.SPAWNER_BACKENDS_RESOLVE_BACKEND_OPTION_PATCH_TARGET, _explode_backend_resolution)
  record = models.RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake", model="fake-model")
  await tree.runs.register_run_locked(record, task_spec_text=task_spec["goal"])
  tree.dispatch.executor.launch(worker.id, "run-work")

  _run, outcome = await wait_for_terminal_run(tree, worker.id, "run-work")
  assert outcome == "failed"
  assert _turn_launch_notices(tree, worker.id) == []


# ---------------------------------------------------------------------------
# Pooled fresh launches: the Run starts on a Claude pool account
# ---------------------------------------------------------------------------


def build_pooled_env(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    labels: tuple[str, ...] = ("main", "ext-1", "ext-2"),
    claude_pools: dict[str, list[str]] | None = None):
  """The task-tree env over a pooled config: one cc-claude option drawing from the
    Claude account pool *labels* (pool_cfg plants healthy credentials in every account
    dir). ``labels=()`` keeps the same cc-claude option over an empty pool;
    ``claude_pools`` splits the labels into named pools the option draws from by its
    first pool name. Children sign against the synthetic home (seed_signing_home).
    """
  home = tmp_path / "charliebot-home"
  extra = {} if not claude_pools else {"account_pool": next(iter(claude_pools))}
  option = conftest.backend_option(
      id=conftest.POOLED_FABLE_ID, label="Fable", type="cc-claude", model=conftest.FABLE_MODEL, **extra)
  cfg = conftest.pool_cfg(
      tmp_path, [option], home=home, worktree_dir=home / "worktrees", labels=labels, claude_pools=claude_pools)
  seed_signing_home(home, monkeypatch)
  session_blocks = conftest.build_session_blocks(cfg)
  return cfg, session_blocks, conftest.build_task_tree(cfg, session_blocks)


async def _register_work_run(
    tree: task_sessions.TaskTreeManager, worker_id: str, run_id: str, backend: str, model: str) -> None:
  await tree.runs.register_run_locked(
      models.RunRecord(id=run_id, session_id=worker_id, kind="work", backend=backend, model=model))


@pytest.mark.asyncio
async def test_pooled_fresh_worker_launches_build_on_the_selected_account(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """With a non-empty pool and a cc-claude backend, the fresh work Run and the
    review it spawns both launch on the account claude_accounts.select returned,
    and the relay loop is armed from the first process (the backend build receives
    that account). The iteration and scheduled-step kinds launch through their
    controllers and are pinned in the sequence suites."""
  conftest.reset_claude_account_readings()
  cfg, session_blocks, tree = build_pooled_env(tmp_path, monkeypatch)
  # An implement task: the work Run's success spawns its review, the second
  # fresh launch this test pins.
  worker = await conftest.create_task(
      tree, parent=None, request_id="w", profile="worker", task=models.TaskSpec(goal="ship it", task_type="implement"))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  builds = conftest.install_scripted_backends(
      monkeypatch, [
          SpawningScriptedBackend([result_event("work done; modified /tmp/x.sh")]),
          SpawningScriptedBackend([result_event("review ok")])
      ], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)

  expected = claude_accounts.select(cfg, conftest.FABLE_MODEL)
  assert expected is not None and expected.label == "main"

  await _register_work_run(tree, worker.id, "run-work", conftest.POOLED_FABLE_ID, conftest.FABLE_MODEL)
  tree.dispatch.executor.launch(worker.id, "run-work")
  _work_run, outcome = await wait_for_terminal_run(tree, worker.id, "run-work")
  assert outcome == "success"

  # The implement work Run's review is a fresh launch of its own: it spawns
  # automatically and runs on the pool too.
  deadline = asyncio.get_event_loop().time() + 10
  while asyncio.get_event_loop().time() < deadline:
    reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
    if reviews and tree.runs.terminal_outcome(tree.runs.load_events_sync(worker.id), reviews[0].id) is not None:
      break
    await asyncio.sleep(0.1)
  else:
    pytest.fail("the spawned review never reached a terminal fact")

  assert [b["kwargs"]["claude_account"] for b in builds] == [expected, expected]


@pytest.mark.asyncio
async def test_pooled_worker_launch_selects_inside_the_option_pool(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """With pools defined, the launch picks an account of the option's pool: beta's
    untouched accounts, holding more headroom than every alpha member, stay unused."""
  conftest.reset_claude_account_readings()
  cfg, session_blocks, tree = build_pooled_env(
      tmp_path, monkeypatch, claude_pools={
          "alpha": ["main"],
          "beta": ["ext-1", "ext-2"]
      })
  worker = await conftest.create_task(
      tree, parent=None, request_id="w", profile="worker", task=models.TaskSpec(goal="ship it", task_type="quick-edit"))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  builds = conftest.install_scripted_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("done")])], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  # main sits at half a window; across all accounts an untouched beta account wins.
  claude_accounts.observe_rate_limit("main", conftest.rate_limit_event("allowed", 0.50)["rate_limit_info"])
  expected = claude_accounts.select(cfg, conftest.FABLE_MODEL, account_pool="alpha")
  assert expected is not None and expected.label == "main"
  assert claude_accounts.select(cfg, conftest.FABLE_MODEL).label != "main"

  await _register_work_run(tree, worker.id, "run-pool", conftest.POOLED_FABLE_ID, conftest.FABLE_MODEL)
  tree.dispatch.executor.launch(worker.id, "run-pool")
  _run, outcome = await wait_for_terminal_run(tree, worker.id, "run-pool")
  assert outcome == "success"

  assert builds[0]["kwargs"]["claude_account"] == expected


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "env_builder, backend_id", [
        (lambda tmp_path, monkeypatch: build_pooled_env(tmp_path, monkeypatch, labels=()), conftest.POOLED_FABLE_ID),
        (lambda tmp_path, monkeypatch: build_env(tmp_path, monkeypatch), "fake"),
    ],
    ids=["empty-pool", "non-claude-backend"])
async def test_unpooled_fresh_worker_launch_carries_no_account(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, env_builder, backend_id: str) -> None:
  """An empty pool and a non-Claude backend both build with no claude_account key: the
    worker runs on its default login exactly as before this change."""
  cfg, session_blocks, tree = env_builder(tmp_path, monkeypatch)
  worker = await conftest.create_task(
      tree, parent=None, request_id="w", profile="worker", task=models.TaskSpec(goal="ship it", task_type="quick-edit"))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  builds = conftest.install_scripted_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("done")])], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)

  await _register_work_run(tree, worker.id, "run-work", backend_id, conftest.FABLE_MODEL)
  tree.dispatch.executor.launch(worker.id, "run-work")
  _run, outcome = await wait_for_terminal_run(tree, worker.id, "run-work")
  assert outcome == "success"

  assert "claude_account" not in builds[0]["kwargs"]


@pytest.mark.asyncio
async def test_pool_exhausted_launch_fails_the_run_with_evidence_and_no_process(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """select returns None: the Run fails before any process starts, its events
    log carries the pool-exhausted error with the earliest reset time, and the
    improve quota classification reads that text as a quota blocker."""
  conftest.reset_claude_account_readings()
  cfg, session_blocks, tree = build_pooled_env(tmp_path, monkeypatch)
  worker = await conftest.create_task(
      tree, parent=None, request_id="w", profile="worker", task=models.TaskSpec(goal="ship it"))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  # An empty build queue: any process spawn would pop from it and fail loudly.
  builds = conftest.install_scripted_backends(monkeypatch, [], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  for label in ("main", "ext-1", "ext-2"):
    conftest.reject_claude_login(label)
  assert claude_accounts.select(cfg, conftest.FABLE_MODEL) is None

  await _register_work_run(tree, worker.id, "run-x", conftest.POOLED_FABLE_ID, conftest.FABLE_MODEL)
  tree.dispatch.executor.launch(worker.id, "run-x")
  run, outcome = await wait_for_terminal_run(tree, worker.id, "run-x")
  assert outcome == "failed"
  assert run.pid is None
  assert builds == []

  error_text = _read_error_event(tree.runs.run_dir(worker.id, "run-x") / "events.jsonl")
  assert claude_relay.POOL_EXHAUSTED_PHRASE in error_text
  assert "earliest reset" in error_text and "UTC" in error_text
  assert error_text == claude_relay.pool_exhausted_message(cfg)
  # The refusal's flag rides the launch error event.
  error_events = [
      json.loads(line)
      for line in (tree.runs.run_dir(worker.id, "run-x") / "events.jsonl").read_text(encoding="utf-8").splitlines()
      if line.strip()
  ]
  assert [event[ET.QUOTA_EXHAUSTED] for event in error_events if event["type"] == ET.ERROR] == [True]


@pytest.mark.asyncio
async def test_rejected_first_process_relays_to_another_pool_account_and_succeeds(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A Run launched through task_execution relays like a directly driven Worker:
    the first process's rejected conftest.rate_limit_event moves the run to the account
    with the most headroom (same session id, scripted relay backend) and the Run
    ends success."""
  conftest.reset_claude_account_readings()
  cfg, session_blocks, tree = build_pooled_env(tmp_path, monkeypatch)
  worker = await conftest.create_task(
      tree, parent=None, request_id="w", profile="worker", task=models.TaskSpec(goal="ship it", task_type="quick-edit"))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  cc_id = "11111111-2222-3333-4444-555555555555"
  # The fresh launch binds a new Claude session id; pin it so the transcript
  # the relay moves exists before the first process is rejected.
  monkeypatch.setattr(task_execution_module.uuid, "uuid4", lambda: uuid.UUID(cc_id))
  conftest.make_transcript(tmp_path / "claude-main", cc_id)
  first = conftest.ScriptedRelayBackend(
      [conftest.assistant_text_event("halfway"),
       conftest.rate_limit_event("rejected", 1.0)], exit_code=1)
  second = conftest.ScriptedRelayBackend([result_event("done after relay")], exit_code=0)
  builds = conftest.install_scripted_backends(monkeypatch, [first, second], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)

  await _register_work_run(tree, worker.id, "run-relay", conftest.POOLED_FABLE_ID, conftest.FABLE_MODEL)
  tree.dispatch.executor.launch(worker.id, "run-relay")
  run, outcome = await wait_for_terminal_run(tree, worker.id, "run-relay", timeout=20)
  assert outcome == "success"
  assert run.exit_code == 0
  # Both processes registered through the launch callback; the Run's identity
  # is the relay's second process's.
  assert (run.pid, run.pid_start) == (second.pid, second.pid_start)

  assert [b["kwargs"]["claude_account"].label for b in builds] == ["main", "ext-1"]
  # The transcript moved with the run: the resumed process finds it on the
  # new account under the same session id.
  assert (tmp_path / "claude-ext-1" / "projects" / "slug" / f"{cc_id}.jsonl").is_file()


# ---------------------------------------------------------------------------
# Manager-turn account relay: the second process registers through the launch callback
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_manager_turn_relay_registers_the_second_process_through_the_launch_callback(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A manager turn whose pool account relay starts a second process (the Oct 6
    failed-turn scenario: a far warning reading, then a tool result) lands the
    turn: the second process registers through the real launch callback, the
    Run's identity follows it, started_at stays the first process's, and chat
    holds the second process's reply."""
  conftest.reset_claude_account_readings()
  cfg, session_blocks, tree = build_pooled_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  await tree.update_slot_fields(
      manager.id, "slack", slack_origin={
          "team_id": "T",
          "channel_id": "C",
          "thread_ts": "1.0"
      })
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  cc_id = "8b6fd8ac-6f0e-4af2-9d0e-1c2f47ab5311"
  conftest.make_transcript(tmp_path / "claude-main", cc_id)
  first = conftest.ScriptedRelayBackend(
      [
          # The id-carrying event pins the native session id the relay moves the
          # transcript under (a fresh turn lands none before its stream does).
          {
              **conftest.assistant_text_event("scouting"), "session_id": cc_id
          },
          conftest.rate_limit_event("allowed_warning", 0.93),
          conftest.user_tool_result_event(),
      ],
      exit_code=0)
  second = conftest.ScriptedRelayBackend([result_event("reply after relay")], exit_code=0)
  builds = conftest.install_scripted_backends(monkeypatch, [first, second], conftest.BUILD_BACKEND_PATCH_TARGET)

  # record_launch is the registration seam: snapshot each launch's (pid,
  # pid_start, started_at) so the second launch's overwrite — and started_at's
  # survival — is observed, not inferred from the end record alone.
  real_record_launch = tree.runs.record_launch
  launches: list[tuple[int, str, datetime.datetime | None]] = []

  async def recording_launch(session_id: str, launch_run_id: str, *, pid: int, pid_start: str) -> models.RunRecord:
    run = await real_record_launch(session_id, launch_run_id, pid=pid, pid_start=pid_start)
    launches.append((pid, pid_start, run.started_at))
    return run

  monkeypatch.setattr(tree.runs, "record_launch", recording_launch)

  await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="Take off. Reply with the phrase.", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision["launch"] is True
  run_id = decision["run_id"]
  assert run_id is not None

  run, outcome = await wait_for_terminal_run(tree, manager.id, run_id)
  await conftest.drain_session_consumer(manager.id, timeout=5)

  assert outcome == "success"
  assert len(builds) == 2
  # Each process registered through the launch callback, in stream order.
  assert [(pid, pid_start) for pid, pid_start, _ in launches] == [
      (first.pid, first.pid_start),
      (second.pid, second.pid_start),
  ]
  # The Run's identity is the second process's; started_at is still the
  # first process's.
  assert (run.pid, run.pid_start) == (second.pid, second.pid_start)
  assert launches[0][2] is not None and launches[0][2] == launches[1][2]
  assert run.started_at == launches[0][2]
  # The relayed turn's reply is the second process's.
  events = tree.events.load_events(manager.id)
  assert [e.get("result") for e in events if e.get("type") == ET.RESULT] == ["reply after relay"]


@pytest.mark.asyncio
async def test_stop_in_the_relay_gap_refuses_the_second_process_and_interrupts_the_run(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A stop request landing in the relay gap (the first process's stream has
    ended, the second has not registered yet): the second process's registration
    through the real launch callback is refused because of the stop, the turn
    ends with that one error line, and the Run keeps the first process's
    identity under an interrupted outcome."""
  conftest.reset_claude_account_readings()
  cfg, session_blocks, tree = build_pooled_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  await tree.update_slot_fields(
      manager.id, "slack", slack_origin={
          "team_id": "T",
          "channel_id": "C",
          "thread_ts": "1.0"
      })
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  cc_id = "9c7fe9bd-7f1e-5f3a-be1f-2d3f58bc6422"
  conftest.make_transcript(tmp_path / "claude-main", cc_id)
  first = conftest.ScriptedRelayBackend(
      [
          {
              **conftest.assistant_text_event("scouting"), "session_id": cc_id
          },
          conftest.rate_limit_event("rejected", 1.0),
      ],
      exit_code=1)
  second = conftest.ScriptedRelayBackend([result_event("must never land")], exit_code=0)
  conftest.install_scripted_backends(monkeypatch, [first, second], conftest.BUILD_BACKEND_PATCH_TARGET)

  # The stop rides the relay seam: prepare_relay runs after the first process's
  # stream ended (its rejected reading is the relay reason) and before the
  # second process builds and registers. The wrapper stops the Run first, then
  # hands over to the real relay.
  real_prepare_relay = master_cc_relay.prepare_relay
  relay_run_id: str | None = None

  async def stop_then_relay(ctx, current, cc_session_id, reason, relays, **kwargs):
    assert relay_run_id is not None
    await tree.runs.request_stop(manager.id, relay_run_id, "stop-in-relay-gap")
    return await real_prepare_relay(ctx, current, cc_session_id, reason, relays, **kwargs)

  monkeypatch.setattr(master_cc_relay, "prepare_relay", stop_then_relay)

  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off.", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision["launch"] is True
  run_id = decision["run_id"]
  assert run_id is not None
  relay_run_id = run_id

  run, outcome = await wait_for_terminal_run(tree, manager.id, run_id)
  await conftest.drain_session_consumer(manager.id, timeout=5)

  assert outcome == "interrupted"
  # The second process never registered: the Run keeps the first process's
  # identity.
  assert (run.pid, run.pid_start) == (first.pid, first.pid_start)
  events = tree.events.load_events(manager.id)
  assert [e for e in events if e.get("type") == ET.RESULT] == []
  # The turn ends with the one error line naming the refusal reason.
  errors = [e for e in events if e.get("type") == ET.ASSISTANT_ERROR]
  assert len(errors) == 1 and "stop request" in errors[0]["content"]


# ---------------------------------------------------------------------------
# End-landing retry: out-of-space run endings converge without a restart
# ---------------------------------------------------------------------------


def inject_chat_append_fault(
    monkeypatch: pytest.MonkeyPatch,
    *,
    event_type: str,
    session_ids: set[str],
    times: int = 1,
    err: int = errno.ENOSPC,
    on_raise: Callable[[], None] | None = None,
) -> list[bool]:
  """The first *times* matching chat-event appends raise OSError(*err*); every
    other append — and every later one — writes through the real function.

    The fault sits at ``append_ndjson``, the write every control fact and
    parent report rides, so the exception travels the real wrapping and
    propagation path. Returns one bool per matching append — True when that
    append raised — in order.
    """
  from src.infra import ndjson
  real = ndjson.append_ndjson
  state = {"raised": 0}
  hits: list[bool] = []

  async def flaky(path, data):
    target = str(path)
    if data.get("type") == event_type and any(f"/sessions/{s}/data/" in target for s in session_ids):
      if state["raised"] < times:
        state["raised"] += 1
        hits.append(True)
        if on_raise is not None:
          on_raise()
        raise OSError(err, os.strerror(err))
      hits.append(False)
    return await real(path, data)

  monkeypatch.setattr(ndjson, "append_ndjson", flaky)
  return hits


def inject_run_record_write_fault(
    monkeypatch: pytest.MonkeyPatch,
    *,
    only_finished: bool = True,
    times: int = 1,
    err: int = errno.ENOSPC,
    on_raise: Callable[[], None] | None = None,
) -> list[bool]:
  """The first *times* run-metadata writes raise OSError(*err*); every other
    write — and every later one — writes through the real method.

    Sits at ``RunStore.write_record`` (the one record-mirror write funnel over
    ``atomic_write_text``). ``only_finished`` gates the fault to a finish's
    write, the one that carries ``ended_at``. Returns one bool per matching
    write — True when that write raised — in order.
    """
  real = runs_mod.RunStore.write_record
  state = {"raised": 0}
  hits: list[bool] = []

  async def flaky(self, session_id, run):
    if not only_finished or run.ended_at is not None:
      if state["raised"] < times:
        state["raised"] += 1
        hits.append(True)
        if on_raise is not None:
          on_raise()
        raise OSError(err, os.strerror(err))
      hits.append(False)
    return await real(self, session_id, run)

  monkeypatch.setattr(runs_mod.RunStore, "write_record", flaky)
  return hits


def write_raw_result(run_dir: pathlib.Path,
                     text: str,
                     *,
                     age_seconds: float = 0.0) -> tuple[pathlib.Path, datetime.datetime]:
  """The run's raw transport log with one successful result event, optionally
    mtime-stamped into the past — the drain's truth source. Returns (path, mtime)."""
  raw = run_dir / runs_mod.RAW_LOG_NAME
  raw.parent.mkdir(parents=True, exist_ok=True)
  raw.write_text(
      json.dumps({
          "type": "result",
          "subtype": "success",
          "is_error": False,
          "result": text,
      }) + "\n", encoding="utf-8")
  mtime = datetime.datetime.fromtimestamp(raw.stat().st_mtime, tz=datetime.UTC)
  if age_seconds:
    ts = time.time() - age_seconds
    os.utime(raw, (ts, ts))
    mtime = datetime.datetime.fromtimestamp(ts, tz=datetime.UTC)
  return raw, mtime


async def _wired_root_manager(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch):
  """(cfg, session_blocks, tree, manager): a root manager task over build_env's home, its
    dispatch wired to the silent-broadcast adapter — the rig prefix the manager-turn tests
    share. Each site unpacks the names it uses."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  return cfg, session_blocks, tree, manager


async def _manager_with_worker_child(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, task_type: str | None = None):
  """(cfg, session_blocks, tree, manager, worker): a manager node with one
    worker child, the shape the end-landing tests report against."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  worker = await conftest.create_task(
      tree,
      parent=manager.id,
      request_id="child",
      profile="worker",
      task=models.TaskSpec(goal="do the work", task_type=task_type))
  return cfg, session_blocks, tree, manager, worker


class _IdentityTranslator:
  """A translate-only double: the staged raw bytes already carry standard
    event dicts, so a resume's stream translator passes them through."""

  _POST_RESULT_TIMEOUT = 5.0

  def translate_event(self, event: dict) -> list[dict]:
    return [event]


def install_worker_launch_and_resume_backends(monkeypatch: pytest.MonkeyPatch, backends: list) -> list[dict]:
  """Serve worker launcher builds one at a time; translate-only builds (the
    resume follow's fresh translate) get the identity translator instead of
    consuming a launcher double."""
  builds: list[dict] = []
  queue = list(backends)

  def fake_build(option, cfg, **kwargs):
    if kwargs.get("on_spawn") is None:
      return _IdentityTranslator()
    backend = queue.pop(0)
    on_spawn = kwargs.get("on_spawn")
    if on_spawn is not None:
      backend.set_on_spawn(on_spawn)
    builds.append({"option": option, "backend": backend})
    return backend

  monkeypatch.setattr(conftest.WORKER_BUILD_BACKEND_PATCH_TARGET, fake_build)
  return builds


async def poll_until(predicate, *, timeout: float = 5.0, poll: float = 0.02, what: str) -> None:
  """Poll a sync predicate to truth without starving the event loop the work
    runs on; fail naming *what* on timeout."""
  deadline = time.monotonic() + timeout
  while time.monotonic() < deadline:
    if predicate():
      return
    await asyncio.sleep(poll)
  pytest.fail(f"{what} never settled within {timeout}s")


def test_is_out_of_space_error_walks_cause_and_context_chain() -> None:
  """Only ENOSPC/EDQUOT enter the retry, wherever they sit on the chain."""
  enospc = OSError(errno.ENOSPC, "No space left on device")
  edquot = OSError(errno.EDQUOT, "Disk quota exceeded")
  assert task_execution_module.is_out_of_space_error(enospc)
  assert task_execution_module.is_out_of_space_error(edquot)
  assert task_execution_module.is_out_of_space_error(RuntimeError("landing failed")) is False
  assert task_execution_module.is_out_of_space_error(PermissionError("raw log locked")) is False
  # Both chain links: an explicit cause and an implicit context.
  wrapped_cause = RuntimeError("wrap")
  wrapped_cause.__cause__ = enospc
  assert task_execution_module.is_out_of_space_error(wrapped_cause)
  wrapped_context = RuntimeError("wrap")
  wrapped_context.__context__ = edquot
  assert task_execution_module.is_out_of_space_error(wrapped_context)
  # A cycle on the context chain must not spin.
  a: BaseException = RuntimeError("a")
  b: BaseException = RuntimeError("b")
  b.__context__ = a
  a.__context__ = b
  assert task_execution_module.is_out_of_space_error(a) is False
  # A cause nested behind a non-OSError wrapper is still found.
  deep: BaseException = ValueError("outer")
  deep.__cause__ = wrapped_cause
  assert task_execution_module.is_out_of_space_error(deep)


@pytest.mark.asyncio
async def test_worker_run_finished_enospc_retries_and_lands_without_restart(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(a) The run_finished write raises ENOSPC, the retry drains the dead run
    from its raw log and lands the end record; the parent gets exactly one
    report and no second process ever starts."""
  from src.runtime import thinking_state
  monkeypatch.setattr(task_execution_module, "RUN_END_LANDING_RETRY_INTERVAL_SECONDS", 0.05)
  cfg, session_blocks, tree, manager, worker = await _manager_with_worker_child(tmp_path, monkeypatch)
  adapter = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  tree.dispatch.executor = adapter
  worker_builds = install_worker_launch_and_resume_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("work done")])])
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, make_pm_build("report consumed"))
  fault_hits = inject_chat_append_fault(monkeypatch, event_type=ET.RUN_FINISHED, session_ids={worker.id})

  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the task.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run_id = decision["run_id"]
  await wait_for_terminal_run(tree, worker.id, run_id)
  assert fault_hits == [True, False]  # one failed write; the retry's write landed
  # The worker header timer the failed finish could not close is closed by
  # the hook-1 handover (the process is dead): the node shows idle again.
  await poll_until(lambda: thinking_state.busy_since(worker.id) is None, what="the worker header timer")
  # The retried landing delivered the success-based report exactly once, and
  # the parent's report turn settles without a restart. The report rides the
  # retry task's async delivery chain, so wait for the report itself: an
  # idle-parent poll can win the race against the not-yet-appended report.
  await poll_until(lambda: len(conftest.child_reports(tree, manager.id)) == 1, what="the retried landing's report")
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)
  reports = conftest.child_reports(tree, manager.id)
  assert [r.get("outcome") for r in reports] == ["completed"]
  assert adapter._landing_retries == {}  # a clean round ended the retry task
  assert len(worker_builds) == 1  # one process: the retry drained, never relaunched


@pytest.mark.asyncio
async def test_manager_turn_master_done_enospc_retries_through_master_queue(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(b) The manager turn's MASTER_DONE write raises ENOSPC; the retry's node
    reconcile pass re-attaches the dead turn through the master queue (the kept
    future's done-callback releases the follow pair) and lands the end record
    with the raw log's last write time."""
  monkeypatch.setattr(task_execution_module, "RUN_END_LANDING_RETRY_INTERVAL_SECONDS", 0.05)
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  adapter = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  tree.dispatch.executor = adapter
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, make_pm_build("turn reply"))
  conftest.stub_credentials({"charliebot": {"access_key": "op-secret"}})
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off.", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  run_id = decision["run_id"]
  run_dir = tree.runs.run_dir(manager.id, run_id)
  # The turn's own MASTER_DONE write fails out of space; the drain reads the
  # raw log the live turn never wrote (the scripted double writes none), so
  # the fault itself stages it — the retry's drain outruns any later write.
  _staged: list[tuple[pathlib.Path, object]] = []

  def _stage_drain_raw_log() -> None:
    _staged.append(write_raw_result(run_dir, "drained turn result"))

  fault_hits = inject_chat_append_fault(
      monkeypatch, event_type=ET.MASTER_DONE, session_ids={manager.id}, on_raise=_stage_drain_raw_log)
  run, outcome = await wait_for_terminal_run(tree, manager.id, run_id)
  assert fault_hits == [True, False]  # the live write failed; the drain's landed
  assert len(_staged) == 1
  raw_mtime = _staged[0][1]
  assert outcome == "success"
  assert run.ended_at is not None and abs(run.ended_at - raw_mtime) < datetime.timedelta(seconds=1)
  # The follow pair released only when the master-queue future resolved.
  await poll_until(
      lambda: (manager.id, run_id) not in adapter._resume_follows, what="the manager-turn follow pair release")
  assert adapter._landing_retries == {}


@pytest.mark.asyncio
async def test_parent_report_enospc_retry_delivers_report_once(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(c)+(i)+(k) A successful run's delivery write raises ENOSPC: the end
    record is already on disk, the retry delivers the success-based report
    exactly once (never a failed report), and the run's live ended_at survives
    the retry untouched."""
  monkeypatch.setattr(task_execution_module, "RUN_END_LANDING_RETRY_INTERVAL_SECONDS", 0.05)
  cfg, session_blocks, tree, manager, worker = await _manager_with_worker_child(tmp_path, monkeypatch)
  adapter = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  tree.dispatch.executor = adapter
  install_worker_launch_and_resume_backends(monkeypatch, [SpawningScriptedBackend([result_event("work done")])])
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, make_pm_build("report consumed"))
  fault_hits = inject_chat_append_fault(monkeypatch, event_type=ET.CHILD_REPORT, session_ids={manager.id})

  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the task.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run_id = decision["run_id"]
  run, outcome = await wait_for_terminal_run(tree, worker.id, run_id)
  assert outcome == "success"
  # The retry's re-delivery is asynchronous with the live finish: wait for
  # the one report before naming the appends and settling the parent.
  await poll_until(lambda: len(conftest.child_reports(tree, manager.id)) == 1, what="the retried delivery's report")
  assert fault_hits == [True, False]  # the live delivery failed; the retry's landed
  ended_at = run.ended_at
  assert ended_at is not None
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)
  reports = conftest.child_reports(tree, manager.id)
  assert [r.get("outcome") for r in reports] == ["completed"]  # success-based, once
  fresh = await tree.runs.get_run(worker.id, run_id)
  assert fresh is not None and fresh.ended_at == ended_at  # (k) the retry kept it
  assert adapter._landing_retries == {}


@pytest.mark.asyncio
async def test_drain_ended_at_is_raw_log_last_write_live_exit_keeps_write_time(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(d) A drained run's ended_at is the raw log's last write time; a live
    exit's ended_at stays the observed-exit write time."""
  from src.runtime import task_recovery
  cfg, session_blocks, tree, manager, worker = await _manager_with_worker_child(tmp_path, monkeypatch)
  adapter = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  tree.dispatch.executor = adapter
  install_worker_launch_and_resume_backends(monkeypatch, [SpawningScriptedBackend([result_event("drained work")])])

  # Drain: a launched run whose process died unseen, raw log stamped an hour ago.
  run_id = "run-drained"
  await tree.runs.register_run_locked(
      models.RunRecord(id=run_id, session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the task.", actor="user")
  await tree.dispatch.claim_input_batch_locked(worker.id, run_id)
  await tree.runs.record_launch(worker.id, run_id, pid=424901, pid_start="1-424000")
  _raw, raw_mtime = write_raw_result(tree.runs.run_dir(worker.id, run_id), "drained result", age_seconds=3600)
  await task_recovery.reconcile_task_tree(cfg, tree, adapter)
  run, outcome = await wait_for_terminal_run(tree, worker.id, run_id)
  assert outcome == "success"
  assert run.exit_code == 0
  assert run.ended_at is not None and abs(run.ended_at - raw_mtime) < datetime.timedelta(seconds=1)

  # Live exit: the observed-exit write time, never a staged raw-log mtime.
  # A fresh child node: the drained node's trailing dispatch would race a
  # second input admitted on it.
  worker2 = await conftest.create_task(
      tree, parent=manager.id, request_id="child-2", profile="worker", task=models.TaskSpec(goal="more work"))
  install_worker_launch_and_resume_backends(monkeypatch, [SpawningScriptedBackend([result_event("live work")])])
  await tree.dispatch.admit_input(worker2.id, event_type=ET.USER, content="Start.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker2.id)
  live_run, live_outcome = await wait_for_terminal_run(tree, worker2.id, decision["run_id"])
  assert live_outcome == "success"
  assert live_run.ended_at is not None and live_run.ended_at > raw_mtime


@pytest.mark.asyncio
async def test_disk_headroom_precheck_withholds_worker_run_but_not_manager_turn(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(e) Below the threshold a worker-class run stays queued with one blocked
    report; a manager turn still launches; threshold 0 skips the check."""
  cfg, session_blocks, tree, manager, worker = await _manager_with_worker_child(tmp_path, monkeypatch)
  cfg.server.min_free_disk_gib = 10**9  # no filesystem holds this much
  adapter = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  tree.dispatch.executor = adapter
  worker_builds = install_worker_launch_and_resume_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("work done")])])
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, make_pm_build("report consumed"))

  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the task.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run_id = decision["run_id"]

  def _withheld_reason() -> str | None:
    events = tree.events.load_events(worker.id)
    for event in events:
      if event.get("type") == ET.RUN_LAUNCH_WITHHELD:
        return str(event.get("reason"))
    return None

  # The launch is fire-and-forget: poll for the withheld verdict's durable
  # event, then re-read the queued run it left behind.
  await poll_until(lambda: _withheld_reason() is not None, what="the disk-headroom withheld event")
  reason = _withheld_reason() or ""
  assert "disk free" in reason and "below 1000000000 GiB" in reason
  assert str(cfg.charliebot_home) in reason or str(pathlib.Path(cfg.paths.worktree_dir)) in reason
  run = await tree.runs.get_run(worker.id, run_id)
  assert run is not None and run.pid is None  # no process, stays queued
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(worker.id), run_id) is None
  assert worker_builds == []
  await poll_until(lambda: len(conftest.child_reports(tree, manager.id)) == 1, what="the blocked report")
  assert [r.get("outcome") for r in conftest.child_reports(tree, manager.id)] == ["blocked"]

  # A manager turn launches under the same threshold: the blocked report's
  # own wake started one (worker-class runs only are checked).
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)
  manager_events = tree.runs.load_events_sync(manager.id)
  turns = [r for r in tree.runs.list_run_records_sync(manager.id) if r.kind == "manager_turn"]
  assert len(turns) == 1
  assert tree.runs.terminal_outcome(manager_events, turns[0].id) == "success"
  # ...and threshold 0 skips the check, so the queued worker launches.
  cfg.server.min_free_disk_gib = 0
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run, outcome = await wait_for_terminal_run(tree, worker.id, decision["run_id"])
  assert outcome == "success"


@pytest.mark.asyncio
async def test_withheld_launch_on_a_completed_task_settles_the_queued_run(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A launch withheld because its task is closed records the withheld fact
    and the blocked report, plus one durable stop request settling
    the never-launched run: the archive check reads it resolved and the
    parent's archive passes."""
  cfg, session_blocks, tree, manager, worker = await _manager_with_worker_child(tmp_path, monkeypatch)
  adapter = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  # The worker's delivered run closed it completed; the queued run never launches.
  await tree.runs.register_run_locked(models.RunRecord(id="run-done", session_id=worker.id, kind="work"))
  await tree.dispatch.finish_run(worker.id, "run-done", outcome="success", exit_code=0)
  assert tree.task_state(worker.id) == "completed"
  await tree.runs.register_run_locked(
      models.RunRecord(id="run-queued", session_id=worker.id, kind="work", backend="fake", model="fake-model"))

  verdict = await adapter.execute_run(worker.id, "run-queued", launch_prompt=None)

  assert verdict == f"withheld: task {worker.id} is completed"
  events = tree.runs.load_events_sync(worker.id)
  withheld = [e for e in events if e["type"] == ET.RUN_LAUNCH_WITHHELD and e.get("run_id") == "run-queued"]
  assert len(withheld) == 1
  stops = [e for e in events if e["type"] == ET.RUN_STOP_REQUESTED and e.get("run_id") == "run-queued"]
  assert len(stops) == 1 and stops[0]["request_id"] == "closed-task:run-queued"
  run = await tree.runs.get_run(worker.id, "run-queued")
  assert run is not None and run.pid is None
  # The withheld rule stands: a run that never launched has no terminal fact.
  assert tree.runs.terminal_outcome(events, "run-queued") is None
  assert tree.runs.run_blocker(run, events, runs_mod.read_host_boot_time()) is None
  assert [r.get("outcome") for r in conftest.child_reports(tree, manager.id)] == ["completed", "blocked"]
  # The completed worker's stopped run blocks nothing: the archive passes and
  # archives the open parent (a completed node is left unchanged).
  assert await tree.completion.archive_subtree(
      manager.id,
      request_id="archive-test",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR) == [manager.id]


@pytest.mark.asyncio
async def test_startup_recovery_stops_a_launched_run_on_a_closed_worker(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Recovery stops a launched Run when cancellation closed before its stop fact landed."""
  from src.runtime import task_recovery
  monkeypatch.setattr(runs_mod, "STOP_EXIT_WAIT_SECONDS", 0.2)
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  worker = await conftest.create_task(
      tree, parent=None, request_id="closed-live-worker", profile="worker", task=models.TaskSpec(goal="long work"))
  proc = subprocess.Popen(["/bin/sleep", "60"], start_new_session=True)
  pair = runs_mod.read_pid_stat(proc.pid)
  assert pair is not None
  await tree.runs.register_run_locked(
      models.RunRecord(
          id="run-live-closed",
          session_id=worker.id,
          kind="work",
          pid=proc.pid,
          pid_start=pair[0],
          started_at=datetime.datetime.now(datetime.UTC)))
  append_stop = tree.runs.append_stop_request_locked

  async def fail_after_close(session_id: str, run_id: str, request_id: str) -> bool:
    raise OSError("simulated stop-fact write interruption")

  monkeypatch.setattr(tree.runs, "append_stop_request_locked", fail_after_close)
  try:
    with pytest.raises(OSError, match="simulated stop-fact write interruption"):
      await tree.completion.cancel_task(
          worker.id, request_id="cancel-before-crash", reason="stop", caller=conftest.OPERATOR)
    assert tree.task_state(worker.id) == "cancelled"
    assert proc.poll() is None
    monkeypatch.setattr(tree.runs, "append_stop_request_locked", append_stop)

    adapter = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
    await task_recovery.reconcile_task_tree(cfg, tree, adapter)

    events = tree.runs.load_events_sync(worker.id)
    stops = [event for event in events if event.get("type") == ET.RUN_STOP_REQUESTED]
    assert len(stops) == 1 and stops[0]["request_id"] == "closed-task:run-live-closed"
    assert tree.runs.terminal_outcome(events, "run-live-closed") == "interrupted"
    assert proc.poll() is not None
  finally:
    if proc.poll() is None:
      proc.kill()
    proc.wait(timeout=1)


@pytest.mark.asyncio
async def test_non_space_end_failure_lands_immediately_and_starts_no_retry(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(j) A non-out-of-space exception in the end path lands the run
    immediately and starts no retry; a success fact that hits
    OSError(EIO) during delivery sends no failed report."""
  monkeypatch.setattr(task_execution_module, "RUN_END_LANDING_RETRY_INTERVAL_SECONDS", 0.05)
  cfg, session_blocks, tree, manager, worker = await _manager_with_worker_child(tmp_path, monkeypatch)
  adapter = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  tree.dispatch.executor = adapter
  install_worker_launch_and_resume_backends(
      monkeypatch,
      [SpawningScriptedBackend([result_event("work done")]),
       SpawningScriptedBackend([result_event("more work done")])])
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, make_pm_build("report consumed"))

  # First half: the run_finished write raises PermissionError (not space).
  inject_chat_append_fault(monkeypatch, event_type=ET.RUN_FINISHED, session_ids={worker.id}, err=errno.EACCES)
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the task.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run_id = decision["run_id"]
  _run, outcome = await wait_for_terminal_run(tree, worker.id, run_id)
  assert outcome == "failed"  # landed immediately by the existing path
  # The landing's follow-ups (the failed report, the retry decision) run after
  # the terminal fact; the launch task ends when they are done. The parent has
  # no pending input or active run until the report lands, so settling it
  # earlier would pass before the report exists.
  await poll_until(
      lambda: (worker.id, run_id) not in adapter._launch_inflight, timeout=15.0, what="the failed run's launch chain")
  assert adapter._landing_retries == {}  # no retry for a non-space error
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)
  assert [r.get("outcome") for r in conftest.child_reports(tree, manager.id)] == ["failed"]

  # Second half: a success fact that hits EIO during delivery — the durable
  # outcome governs the delivery, so no contradicting failed report, and no
  # retry (EIO does not clear with freed space).
  fresh = await tree.runs.get_run(worker.id, decision["run_id"])
  assert fresh is not None and fresh.id == run_id
  inject_chat_append_fault(monkeypatch, event_type=ET.CHILD_REPORT, session_ids={manager.id}, times=50, err=errno.EIO)
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="More work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  second_id = decision["run_id"]
  _second_run, second_outcome = await wait_for_terminal_run(tree, worker.id, second_id)
  assert second_outcome == "success"
  await poll_until(
      lambda: (worker.id, second_id) not in adapter._launch_inflight,
      timeout=15.0,
      what="the second run's launch chain")
  assert adapter._landing_retries == {}
  outcomes = [r.get("outcome") for r in conftest.child_reports(tree, manager.id)]
  assert "failed" not in outcomes[1:], outcomes  # the second run sent no failed report


@pytest.mark.asyncio
async def test_repeat_finish_fills_only_empty_end_metadata(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """finish_run's repeat path fills empty ended_at/exit_code from the passed
    values and never moves values already written."""
  _cfg, _session_blocks, tree = build_env(tmp_path, monkeypatch)
  worker = await conftest.create_task(
      tree, parent=None, request_id="w", profile="worker", task=models.TaskSpec(goal="g"))
  run_id = "run-half"
  await tree.runs.register_run_locked(models.RunRecord(id=run_id, session_id=worker.id, kind="work"))
  # First finish with an empty-ended_at repeat (the half-written record's
  # shape): the fact lands, the metadata write is lost.
  await tree.dispatch.finish_run(worker.id, run_id, outcome="success", exit_code=0)
  meta_path = tree.runs.metadata_path(worker.id, run_id)
  record = json.loads(meta_path.read_text(encoding="utf-8"))
  record["ended_at"] = None
  record["exit_code"] = None
  meta_path.write_text(json.dumps(record), encoding="utf-8")
  tree.runs.records_generation += 1

  ended = datetime.datetime.now(datetime.UTC) - datetime.timedelta(minutes=5)
  await tree.dispatch.finish_run(worker.id, run_id, outcome="failed", exit_code=-1, ended_at=ended)
  run = await tree.runs.get_run(worker.id, run_id)
  assert run is not None
  # The first outcome never moves; the empty fields take the passed values.
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(worker.id), run_id) == "success"
  assert run.ended_at is not None and abs(run.ended_at - ended) < datetime.timedelta(seconds=1)
  assert run.exit_code == -1
  # A repeat over complete metadata changes nothing, whatever it names.
  other = ended + datetime.timedelta(minutes=1)
  await tree.dispatch.finish_run(worker.id, run_id, outcome="failed", exit_code=-2, ended_at=other)
  run = await tree.runs.get_run(worker.id, run_id)
  assert run is not None
  assert run.ended_at == run.ended_at and run.ended_at is not None
  assert abs(run.ended_at - ended) < datetime.timedelta(seconds=1)
  assert run.exit_code == -1


REVIEW_FEEDBACK = "Review: the rename missed the call in foo.py"


def _failover_env(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch):
  """A task tree whose pool is fake2 then fake3; the adapter's launches are recorded, not run."""
  options = [
      conftest.backend_option(id=i, label=i, type="codex", model="fake-model") for i in ("fake", "fake2", "fake3")
  ]
  cfg, session_blocks, tree = build_spawning_env(tmp_path, monkeypatch, options=options, pool=["fake2", "fake3"])
  adapter = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  tree.dispatch.executor = adapter
  launched: list[str] = []
  monkeypatch.setattr(adapter, "launch", lambda session_id, run_id, **kwargs: launched.append(run_id))
  return cfg, tree, adapter, launched


async def _interrupted_worker_run(
    tree, manager, *, from_pool: bool, task_type: models.TaskType = models.TaskType.IMPLEMENT):
  """A worker of *task_type* whose run on fake2 ended on quota exhaustion with its revision batch bound."""
  worker = await tree.create_task(
      request_id="worker",
      task_parent_id=manager.id,
      profile="worker",
      task=models.TaskSpec(goal="do the work", task_type=task_type),
      name=None,
      backend="fake2",
      caller=conftest.OPERATOR,
  )
  if from_pool:
    await tree.update_slot_fields(worker.id, backend_pool.METADATA_OWNER, backend_from_pool=True)
  feedback = await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content=REVIEW_FEEDBACK, actor="user")
  run = models.RunRecord(
      id="run-revision",
      session_id=worker.id,
      kind="work",
      backend="fake2",
      model="fake-model",
      input_event_ids=[feedback["id"]],
  )
  await tree.runs.register_run_locked(run)
  await tree.dispatch.finish_run(worker.id, run.id, outcome="failed", exit_code=1)
  return worker, await tree.runs.get_run(worker.id, run.id)


def _record_parent_reports(adapter, monkeypatch: pytest.MonkeyPatch) -> list[str | None]:
  summaries: list[str | None] = []

  async def record(session_id, run, outcome, *, summary=None, deciding_run=None):
    summaries.append(summary)

  monkeypatch.setattr(adapter, "_report_failure_to_parent", record)
  return summaries


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "task_type",
    [models.TaskType.IMPLEMENT, models.TaskType.QUICK_EDIT, models.TaskType.SCRIPT_RUN, models.TaskType.VERIFY])
async def test_quota_failure_on_a_pool_node_fails_over_to_the_next_member(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, task_type: models.TaskType) -> None:
  """A pool-chosen worker whose run ends on quota exhaustion cools that member, moves the node to the next
  member, and registers a retry run on it that binds the interrupted run's input batch."""
  cfg, tree, adapter, launched = _failover_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  worker, run = await _interrupted_worker_run(tree, manager, from_pool=True, task_type=task_type)
  summaries = _record_parent_reports(adapter, monkeypatch)
  adapter._quota_failures[(worker.id, run.id)] = None

  await adapter._after_worker_run(await tree.load_meta(worker.id), run, "failed")

  retry = next(r for r in tree.runs.list_run_records_sync(worker.id) if r.retry_of_run_id == run.id)
  assert retry.backend == "fake3"
  assert retry.model == "fake-model"
  assert retry.input_event_ids == run.input_event_ids
  # The retry's work description carries the same review feedback the interrupted run answered.
  description = await adapter._work_description(await tree.load_meta(worker.id), retry)
  assert REVIEW_FEEDBACK in description
  assert launched == [retry.id]
  assert summaries == []
  assert (await tree.load_meta(worker.id)).backend == "fake3"
  assert "fake2 cooling until" in backend_pool.recovery_report(cfg)


@pytest.mark.asyncio
async def test_a_pinned_worker_is_not_failed_over_and_its_member_is_not_cooled(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A node whose delegation named its backend keeps its quota behavior: the failure is reported, nothing
  retries, and the pool is untouched."""
  cfg, tree, adapter, launched = _failover_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  worker, run = await _interrupted_worker_run(tree, manager, from_pool=False)
  summaries = _record_parent_reports(adapter, monkeypatch)
  adapter._quota_failures[(worker.id, run.id)] = None

  await adapter._after_worker_run(await tree.load_meta(worker.id), run, "failed")

  assert launched == []
  assert len(summaries) == 1
  assert [r for r in tree.runs.list_run_records_sync(worker.id) if r.retry_of_run_id == run.id] == []
  assert "fake2 not cooling" in backend_pool.recovery_report(cfg)


@pytest.mark.asyncio
async def test_a_successful_worker_with_no_available_reviewer_reports_blocked(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The reviewer never runs on a member the worker node used: with the only other pool member
    cooling, no review spawns and the parent's blocked report names each member's recovery time."""
  _cfg, tree, adapter, launched = _failover_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  worker = await tree.create_task(
      request_id="worker",
      task_parent_id=manager.id,
      profile="worker",
      task=models.TaskSpec(goal="do the work", task_type=models.TaskType.IMPLEMENT),
      name=None,
      backend="fake2",
      caller=conftest.OPERATOR,
  )
  run = models.RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake2", model="fake-model")
  await tree.runs.register_run_locked(run)
  await tree.dispatch.finish_run(worker.id, run.id, outcome="success", exit_code=0)
  backend_pool.cool("fake3", datetime.datetime(2030, 1, 2, 3, 4, tzinfo=datetime.UTC))
  summaries = _record_parent_reports(adapter, monkeypatch)

  await adapter._after_worker_run(await tree.load_meta(worker.id), run, "success")

  assert launched == []
  assert [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"] == []
  assert len(summaries) == 1 and summaries[0] is not None
  assert "found no available reviewer backend" in summaries[0]
  assert "fake2 not cooling" in summaries[0]
  assert "fake3 cooling until 2030-01-02T03:04" in summaries[0]


@pytest.mark.asyncio
async def test_failover_with_every_member_cooling_reports_each_recovery_time(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """With the next member cooling too, the worker does not start a retry; the parent's report names each
  member's recovery time."""
  _cfg, tree, adapter, launched = _failover_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  worker, run = await _interrupted_worker_run(tree, manager, from_pool=True)
  backend_pool.cool("fake3", datetime.datetime(2030, 1, 2, 3, 4, tzinfo=datetime.UTC))
  summaries = _record_parent_reports(adapter, monkeypatch)
  adapter._quota_failures[(worker.id, run.id)] = datetime.datetime(2030, 1, 2, 2, 0, tzinfo=datetime.UTC)

  await adapter._after_worker_run(await tree.load_meta(worker.id), run, "failed")

  assert launched == []
  assert len(summaries) == 1 and summaries[0] is not None
  assert "fake2 cooling until 2030-01-02T02:00" in summaries[0]
  assert "fake3 cooling until 2030-01-02T03:04" in summaries[0]
