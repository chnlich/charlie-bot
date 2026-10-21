"""The backend router: the host-configured router program that orders unpinned work.

Covers the route entry's contract errors, the ordered pick over the route, the delegation's
router-before-task-creation wiring, the quota failover that walks the route then the pool, the
reviewer that reuses the worker's route list, and the thread summon that reads the thread and
calls the router before it creates the session. Every backend id here is synthetic.
"""

import asyncio
import datetime
import json
import pathlib
import sys

import conftest
import pytest
import test_task_execution as tte

from src.infra import config, metadata_slots, models
from src.infra import event_types as ET
from src.runtime import backend_pool

OP_HEADERS = tte.OP_HEADERS

_ROUTER_SCRIPT = '''
import json, sys, time
mode = sys.argv[1]
if mode == "slow":
    time.sleep(2)
    sys.exit(0)
req = json.load(sys.stdin)
if mode == "order":
    json.dump({"backends": sys.argv[2].split(","), "reason": "design p=0.9"}, sys.stdout)
elif mode == "empty":
    json.dump({"backends": [], "reason": "routine p=0.9"}, sys.stdout)
elif mode == "echo":
    json.dump({"backends": [c["id"] for c in req["candidates"]], "reason": "echo"}, sys.stdout)
elif mode == "stdin":
    json.dump({"backends": [], "reason": json.dumps(req)}, sys.stdout)
elif mode == "badjson":
    sys.stdout.write("not json at all")
elif mode == "unknown":
    json.dump({"backends": ["ghost-backend"], "reason": "x"}, sys.stdout)
elif mode == "fail":
    sys.stderr.write("router boom")
    sys.exit(3)
'''


def _router_script(tmp_path: pathlib.Path) -> str:
  path = tmp_path / "fake_router.py"
  path.write_text(_ROUTER_SCRIPT)
  return str(path)


def _router_cfg(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, *command: str,
    pool: tuple[str, ...] = ("fake", "fake2")) -> config.CharlieBotConfig:
  cfg, _blocks, _tree = tte.build_spawning_env(
      tmp_path,
      monkeypatch,
      options=[conftest.backend_option(id=i, label=i, type="codex", model="fake-model") for i in ("fake", "fake2")],
      pool=list(pool))
  cfg.backends.routing = config.RoutingConfig(command=list(command))
  return cfg


def _route(
    cfg: config.CharlieBotConfig, *, role: str = "implement", text: str = "work", candidates: list[str] | None = None):
  return backend_pool.route(
      cfg, role=role, text=text, candidates=candidates if candidates is not None else ["fake", "fake2"])


# ---------------------------------------------------------------------------
# route: the contract errors
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_route_returns_the_router_stdout_object(tmp_path, monkeypatch) -> None:
  cfg = _router_cfg(tmp_path, monkeypatch, sys.executable, _router_script(tmp_path), "order", "fake2,fake")
  route = await _route(cfg)
  assert route.backends == ["fake2", "fake"]
  assert route.reason == "design p=0.9"


@pytest.mark.asyncio
async def test_route_sends_role_text_and_cooling_deadlines_on_stdin(tmp_path, monkeypatch) -> None:
  script = _router_script(tmp_path)
  cfg = _router_cfg(tmp_path, monkeypatch, sys.executable, script, "stdin")
  backend_pool.cool("fake2", models.utc_now())
  out = await _route(cfg, role="thread", text="hello thread")
  stdin_obj = json.loads(out.reason)
  assert stdin_obj["role"] == "thread"
  assert stdin_obj["text"] == "hello thread"
  assert stdin_obj["candidates"] == [
      {
          "id": "fake",
          "cooling_until": None
      },
      {
          "id": "fake2",
          "cooling_until": None
      },  # a past deadline reads as not cooling
  ]


@pytest.mark.asyncio
async def test_route_error_when_the_command_cannot_start(tmp_path, monkeypatch) -> None:
  cfg = _router_cfg(tmp_path, monkeypatch, "definitely-not-a-real-router-command")
  with pytest.raises(backend_pool.RouterError, match="definitely-not-a-real-router-command"):
    await _route(cfg)


@pytest.mark.asyncio
async def test_route_error_when_the_command_exits_non_zero(tmp_path, monkeypatch) -> None:
  cfg = _router_cfg(tmp_path, monkeypatch, sys.executable, _router_script(tmp_path), "fail")
  with pytest.raises(backend_pool.RouterError, match=r"exited 3.*router boom"):
    await _route(cfg)


@pytest.mark.asyncio
async def test_route_error_when_stdout_is_not_the_contract_json(tmp_path, monkeypatch) -> None:
  cfg = _router_cfg(tmp_path, monkeypatch, sys.executable, _router_script(tmp_path), "badjson")
  with pytest.raises(backend_pool.RouterError, match=r"exited 0.*contract"):
    await _route(cfg)


@pytest.mark.asyncio
async def test_route_error_when_an_id_sits_outside_the_candidates(tmp_path, monkeypatch) -> None:
  cfg = _router_cfg(tmp_path, monkeypatch, sys.executable, _router_script(tmp_path), "unknown")
  with pytest.raises(backend_pool.RouterError, match="ghost-backend"):
    await _route(cfg)


@pytest.mark.asyncio
async def test_route_error_when_the_call_times_out(tmp_path, monkeypatch) -> None:
  cfg = _router_cfg(tmp_path, monkeypatch, sys.executable, _router_script(tmp_path), "slow")
  monkeypatch.setattr(backend_pool, "ROUTER_TIMEOUT", 0.2)
  with pytest.raises(backend_pool.RouterError, match="timed out"):
    await _route(cfg)


def test_the_router_bound_sits_below_the_delegate_client_timeout() -> None:
  from src.infra import timeouts
  assert timeouts.ROUTER_TIMEOUT_SECONDS < timeouts.HTTP_INTERNAL_API_TIMEOUT


# ---------------------------------------------------------------------------
# pick_ordered: the ordered pick over a route
# ---------------------------------------------------------------------------


def test_pick_ordered_takes_the_first_available_member_in_order(tmp_path, monkeypatch) -> None:
  cfg = _router_cfg(tmp_path, monkeypatch, sys.executable, "x")
  option = backend_pool.pick_ordered(cfg, caller="test", backends=["fake2", "fake"])
  assert option is not None and option.id == "fake2"


def test_pick_ordered_skips_a_cooling_member(tmp_path, monkeypatch) -> None:
  cfg = _router_cfg(tmp_path, monkeypatch, sys.executable, "x")
  backend_pool.cool("fake2", None)
  option = backend_pool.pick_ordered(cfg, caller="test", backends=["fake2", "fake"])
  assert option is not None and option.id == "fake"


def test_pick_ordered_returns_none_when_every_member_is_out(tmp_path, monkeypatch) -> None:
  cfg = _router_cfg(tmp_path, monkeypatch, sys.executable, "x")
  backend_pool.cool("fake", None)
  backend_pool.cool("fake2", None)
  assert backend_pool.pick_ordered(cfg, caller="test", backends=["fake2", "fake"]) is None


def test_the_ordered_pick_shares_selects_cooldown_table(tmp_path, monkeypatch) -> None:
  cfg = _router_cfg(tmp_path, monkeypatch, sys.executable, "x")
  backend_pool.cool("fake", None)
  # select skips the member pick_ordered would have taken first: one table.
  assert backend_pool.select(cfg, caller="test").id == "fake2"
  assert backend_pool.pick_ordered(cfg, caller="test", backends=["fake", "fake2"]).id == "fake2"


# ---------------------------------------------------------------------------
# Delegation: the router runs before task creation
# ---------------------------------------------------------------------------


async def _routed_delegation(
    tmp_path,
    monkeypatch,
    *,
    task_type="quick-edit",
    script_mode="order",
    order="fake2,fake",
    backend_ids=("fake", "fake2"),
    pool=("fake", "fake2")):
  """One manager with a takeoff phrase, the API client, and a routing section wired to the fake
  router; returns (client, tree, session_blocks, manager, payload)."""
  script = _router_script(tmp_path)
  cfg, session_blocks, tree = tte.build_spawning_env(
      tmp_path,
      monkeypatch,
      options=[conftest.backend_option(id=i, label=i, type="codex", model="fake-model") for i in backend_ids],
      pool=list(pool))
  cfg.backends.routing = config.RoutingConfig(command=[sys.executable, script, script_mode, order])
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  conftest.stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, tte.make_pm_build("manager turn", []))
  adapter = tte._adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  tree.dispatch.executor = adapter
  launched: list[str] = []
  monkeypatch.setattr(adapter, "launch", lambda session_id, run_id, **kwargs: launched.append(run_id))
  monkeypatch.setattr(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  from src.infra import event_types as ET
  await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="Take off and delegate the phrase task.", actor="user")
  payload = conftest.delegate_payload(manager.id, pathlib.Path("/nonexistent-repo"), task_type=task_type)
  return cfg, session_blocks, tree, manager, payload, launched


@pytest.mark.asyncio
async def test_delegation_uses_the_router_order_and_records_the_route(tmp_path, monkeypatch) -> None:
  cfg, session_blocks, tree, manager, payload, launched = await _routed_delegation(tmp_path, monkeypatch)
  with tte.make_api_client(cfg, session_blocks, tree) as client:
    resp = client.post("/api/internal/delegate", json=payload, headers=OP_HEADERS)
    assert resp.status_code == 200, resp.text
    body = resp.json()
    child = await tree.load_meta(body["session_id"])
    assert child.backend == "fake2"
    fields = metadata_slots.fields_of(child, backend_pool.METADATA_OWNER)
    assert fields.route is not None
    assert fields.route.backends == ["fake2", "fake"]
    assert fields.route.reason == "design p=0.9"
    events = session_blocks.events.load_chat_events_sync(manager.id)
    delegated = [e for e in events if e.get("type") == ET.TASK_DELEGATED]
    assert delegated and delegated[-1]["route"] == {"backends": ["fake2", "fake"], "reason": "design p=0.9"}
    assert launched == [body["run_id"]]


@pytest.mark.asyncio
async def test_an_empty_route_list_uses_the_pool(tmp_path, monkeypatch) -> None:
  cfg, session_blocks, tree, _manager, payload, _launched = await _routed_delegation(
      tmp_path, monkeypatch, script_mode="empty")
  with tte.make_api_client(cfg, session_blocks, tree) as client:
    resp = client.post("/api/internal/delegate", json=payload, headers=OP_HEADERS)
    assert resp.status_code == 200, resp.text
    child = await tree.load_meta(resp.json()["session_id"])
    assert child.backend == "fake"  # the pool's first member
    fields = metadata_slots.fields_of(child, backend_pool.METADATA_OWNER)
    assert fields.route is not None and fields.route.backends == []
    assert fields.backend_from_pool is True


@pytest.mark.asyncio
async def test_a_router_failure_returns_an_http_error_carrying_the_router_error(tmp_path, monkeypatch) -> None:
  cfg, session_blocks, tree, manager, payload, launched = await _routed_delegation(
      tmp_path, monkeypatch, script_mode="fail")
  with tte.make_api_client(cfg, session_blocks, tree) as client:
    resp = client.post("/api/internal/delegate", json=payload, headers=OP_HEADERS)
    assert resp.status_code == 502, resp.text
    assert "router boom" in resp.text
  assert launched == []
  assert await tree.load_meta(manager.id) is not None
  children = tree.tree_children(manager.id) if hasattr(tree, "tree_children") else []
  assert not children


@pytest.mark.asyncio
async def test_a_cooling_routed_list_falls_back_to_the_pool(tmp_path, monkeypatch) -> None:
  cfg, session_blocks, tree, _manager, payload, _launched = await _routed_delegation(
      tmp_path, monkeypatch, order="fake2,fake")
  backend_pool.cool("fake2", None)
  with tte.make_api_client(cfg, session_blocks, tree) as client:
    resp = client.post("/api/internal/delegate", json=payload, headers=OP_HEADERS)
    assert resp.status_code == 200, resp.text
    child = await tree.load_meta(resp.json()["session_id"])
    assert child.backend == "fake"  # the route is spent; the pool's next free member runs it


@pytest.mark.asyncio
async def test_every_backend_unavailable_returns_400_with_recovery_times(tmp_path, monkeypatch) -> None:
  cfg, session_blocks, tree, _manager, payload, _launched = await _routed_delegation(
      tmp_path, monkeypatch, script_mode="empty")
  backend_pool.cool("fake", None)
  backend_pool.cool("fake2", None)
  with tte.make_api_client(cfg, session_blocks, tree) as client:
    resp = client.post("/api/internal/delegate", json=payload, headers=OP_HEADERS)
    assert resp.status_code == 400, resp.text
    assert "cooling until" in resp.text


@pytest.mark.asyncio
async def test_without_a_routing_section_no_router_starts_and_the_pool_decides(tmp_path, monkeypatch) -> None:
  cfg, session_blocks, tree, _manager, payload, _launched = await _routed_delegation(
      tmp_path, monkeypatch, script_mode="fail")
  cfg.backends.routing = None  # the command would fail loudly if the code ever ran it
  with tte.make_api_client(cfg, session_blocks, tree) as client:
    resp = client.post("/api/internal/delegate", json=payload, headers=OP_HEADERS)
    assert resp.status_code == 200, resp.text
    child = await tree.load_meta(resp.json()["session_id"])
    assert metadata_slots.fields_of(child, backend_pool.METADATA_OWNER).route is None


@pytest.mark.asyncio
async def test_a_pinned_delegation_never_calls_the_router(tmp_path, monkeypatch) -> None:
  cfg, session_blocks, tree, _manager, payload, _launched = await _routed_delegation(
      tmp_path, monkeypatch, script_mode="fail")
  payload["backend"] = "fake2"
  with tte.make_api_client(cfg, session_blocks, tree) as client:
    resp = client.post("/api/internal/delegate", json=payload, headers=OP_HEADERS)
    assert resp.status_code == 200, resp.text
    child = await tree.load_meta(resp.json()["session_id"])
    assert child.backend == "fake2"
    assert metadata_slots.fields_of(child, backend_pool.METADATA_OWNER).route is None


@pytest.mark.asyncio
async def test_a_cold_read_delegation_never_calls_the_router(tmp_path, monkeypatch) -> None:
  cfg, session_blocks, tree, _manager, payload, _launched = await _routed_delegation(
      tmp_path, monkeypatch, script_mode="fail", task_type="cold-read")
  payload.pop("repo_path")
  payload.pop("base_branch")
  payload["cold_read"] = {"page_path": "docs/plan.html", "genre": "plan", "trigger": "read it"}
  with tte.make_api_client(cfg, session_blocks, tree) as client:
    resp = client.post("/api/internal/delegate", json=payload, headers=OP_HEADERS)
    assert resp.status_code == 200, resp.text
    child = await tree.load_meta(resp.json()["session_id"])
    assert metadata_slots.fields_of(child, backend_pool.METADATA_OWNER).route is None


@pytest.mark.asyncio
async def test_verify_candidates_exclude_the_session_backend(tmp_path, monkeypatch) -> None:
  """The router's candidates for a verify delegation leave out the session's own backend."""
  script = _router_script(tmp_path)
  cfg, session_blocks, tree, manager, payload, _launched = await _routed_delegation(
      tmp_path, monkeypatch, script_mode="stdin", task_type="verify")
  cfg.backends.routing = config.RoutingConfig(command=[sys.executable, script, "stdin"])
  with tte.make_api_client(cfg, session_blocks, tree) as client:
    resp = client.post("/api/internal/delegate", json=payload, headers=OP_HEADERS)
    assert resp.status_code == 200, resp.text
    events = session_blocks.events.load_chat_events_sync(manager.id)
    delegated = [e for e in events if e.get("type") == ET.TASK_DELEGATED][-1]
    stdin_obj = json.loads(delegated["route"]["reason"])
    assert "Task type: verify" in stdin_obj["text"]
    assert stdin_obj["candidates"][0]["id"] == "fake2"  # the manager runs on fake; verify avoids it


# ---------------------------------------------------------------------------
# Quota failover: walk the route, then the pool
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_quota_failover_walks_the_route_then_the_pool(tmp_path, monkeypatch) -> None:
  _cfg, session_blocks, tree, manager, _payload, _launched = await _routed_delegation(
      tmp_path, monkeypatch, order="fake3,fake2", backend_ids=("fake", "fake2", "fake3"), pool=("fake2", "fake3"))
  adapter = tree.dispatch.executor
  worker = await tree.create_task(
      request_id="worker",
      task_parent_id=manager.id,
      profile="worker",
      task=models.TaskSpec(goal="do the work", task_type=models.TaskType.IMPLEMENT),
      name=None,
      backend="fake3",
      caller=conftest.OPERATOR)
  await tree.update_slot_fields(
      worker.id,
      backend_pool.METADATA_OWNER,
      route=backend_pool.BackendRoute(backends=["fake3", "fake2"], reason="design p=0.9"))
  run = models.RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake3", model="fake-model")
  await tree.runs.register_run(run)
  await tree.dispatch.finish_run(worker.id, run.id, outcome="failed", exit_code=1)
  adapter._quota_failures[(worker.id, run.id)] = None
  until = backend_pool.cool("fake3", models.utc_now() + datetime.timedelta(hours=2))

  meta = await tree.load_meta(worker.id)
  assert await adapter._fail_over_quota(meta, run) is True
  new_meta = await tree.load_meta(worker.id)
  assert new_meta.backend == "fake2"  # the next route member
  # The worker session carries the one switch line with the old backend's recovery time.
  events = session_blocks.events.load_chat_events_sync(worker.id)
  switches = [e for e in events if e.get("type") == ET.QUOTA_FAILOVER]
  assert len(switches) == 1
  assert switches[0]["from"] == "fake3"
  assert switches[0]["to"] == "fake2"
  assert switches[0]["recovers_at"] == until.isoformat(timespec="minutes")


@pytest.mark.asyncio
async def test_quota_failover_uses_the_pool_once_the_route_is_spent(tmp_path, monkeypatch) -> None:
  _cfg, _session_blocks, tree, manager, _payload, _launched = await _routed_delegation(
      tmp_path, monkeypatch, order="fake3,fake2", backend_ids=("fake", "fake2", "fake3"), pool=("fake2", "fake3"))
  adapter = tree.dispatch.executor
  worker = await tree.create_task(
      request_id="worker",
      task_parent_id=manager.id,
      profile="worker",
      task=models.TaskSpec(goal="do the work", task_type=models.TaskType.IMPLEMENT),
      name=None,
      backend="fake3",
      caller=conftest.OPERATOR)
  await tree.update_slot_fields(
      worker.id,
      backend_pool.METADATA_OWNER,
      route=backend_pool.BackendRoute(backends=["fake3"], reason="design p=0.9"))
  run = models.RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake3", model="fake-model")
  await tree.runs.register_run(run)
  adapter._quota_failures[(worker.id, run.id)] = None

  meta = await tree.load_meta(worker.id)
  assert await adapter._fail_over_quota(meta, run) is True
  new_meta = await tree.load_meta(worker.id)
  # The route held only the spent member: the pool takes over at its first free member.
  assert new_meta.backend == "fake2"


# ---------------------------------------------------------------------------
# Reviewer: reuse the worker node's route list
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_the_reviewer_reuses_the_route_and_avoids_the_worker_backend(tmp_path, monkeypatch) -> None:
  _cfg, _session_blocks, tree, manager, _payload, _launched = await _routed_delegation(
      tmp_path, monkeypatch, order="fake,fake2")
  adapter = tree.dispatch.executor
  worker = await tree.create_task(
      request_id="worker",
      task_parent_id=manager.id,
      profile="worker",
      task=models.TaskSpec(goal="do the work", task_type=models.TaskType.IMPLEMENT),
      name=None,
      backend="fake",
      caller=conftest.OPERATOR)
  await tree.update_slot_fields(
      worker.id,
      backend_pool.METADATA_OWNER,
      route=backend_pool.BackendRoute(backends=["fake", "fake2"], reason="design p=0.9"))
  run = models.RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake", model="fake-model")
  await tree.runs.register_run(run)
  await tree.dispatch.finish_run(worker.id, run.id, outcome="success", exit_code=0)

  review_run_id = await adapter._maybe_spawn_review(worker.id, run)
  assert review_run_id is not None
  review_run = await tree.runs.get_run(worker.id, review_run_id)
  assert review_run.backend == "fake2"  # the first route member the worker did not use


@pytest.mark.asyncio
async def test_a_fully_avoided_route_falls_back_to_todays_reviewer_selection(tmp_path, monkeypatch) -> None:
  _cfg, _session_blocks, tree, manager, _payload, _launched = await _routed_delegation(
      tmp_path, monkeypatch, order="fake,fake2")
  adapter = tree.dispatch.executor
  worker = await tree.create_task(
      request_id="worker",
      task_parent_id=manager.id,
      profile="worker",
      task=models.TaskSpec(goal="do the work", task_type=models.TaskType.IMPLEMENT),
      name=None,
      backend="fake",
      caller=conftest.OPERATOR)
  await tree.update_slot_fields(
      worker.id, backend_pool.METADATA_OWNER, route=backend_pool.BackendRoute(backends=["fake"], reason="design p=0.9"))
  run = models.RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake", model="fake-model")
  await tree.runs.register_run(run)
  await tree.dispatch.finish_run(worker.id, run.id, outcome="success", exit_code=0)

  review_run_id = await adapter._maybe_spawn_review(worker.id, run)
  assert review_run_id is not None
  review_run = await tree.runs.get_run(worker.id, review_run_id)
  assert review_run.backend == "fake2"  # today's pool selection picks the other member


def test_session_naming_never_starts_the_router(tmp_path, monkeypatch) -> None:
  """The session namer walks the pool (iter_light_backends): no router process runs."""
  from src.runtime import autonamer
  cfg = _router_cfg(tmp_path, monkeypatch, sys.executable, _router_script(tmp_path), "fail")
  names = [option.id for option in autonamer.iter_light_backends(cfg)]
  assert names == ["fake", "fake2"]


# ---------------------------------------------------------------------------
# Thread summon: read the thread, ask the router, then create the session
# ---------------------------------------------------------------------------


def _thread_cfg(tmp_path, monkeypatch, *command: str):
  cfg = conftest.build_slack_cfg(tmp_path)
  extra = conftest.backend_option(id="strong", label="Strong", type="cc-claude", model="fake-model")
  cfg.backends.options = [*cfg.backends.options, extra]
  cfg.backends.pool = ["fake"]
  if command:
    cfg.backends.routing = config.RoutingConfig(command=list(command))
  return cfg


@pytest.mark.asyncio
async def test_summon_creates_the_thread_session_on_the_routed_backend(tmp_path, monkeypatch) -> None:
  import test_slack_listener as slack_tests
  script = _router_script(tmp_path)
  cfg = _thread_cfg(tmp_path, monkeypatch, sys.executable, script, "order", "strong,fake")
  session_blocks = conftest.build_session_blocks(cfg)
  conftest.bind_deps_blocks(monkeypatch, conftest.build_task_tree(cfg, session_blocks), session_blocks)
  client = conftest.FakeSlackClient()
  client.thread = [{"ts": "1700000000.1", "user": "U_ALLOWED", "text": "hello router"}]
  tasks = slack_tests._spawn_round_tasks()
  event = slack_tests._make_event()

  with conftest.mention_seam(tasks) as _trigger:
    sid = await slack_listener_sid(event, cfg, session_blocks, client)
    await asyncio.gather(*tasks)

  meta = await session_blocks.store.get_session(sid)
  assert meta is not None and meta.backend == "strong"
  fields = metadata_slots.fields_of(meta, backend_pool.METADATA_OWNER)
  assert fields.route is not None and fields.route.backends == ["strong", "fake"]


async def slack_listener_sid(event, cfg, session_blocks, client):
  from src.features.slack import slack_listener
  return await slack_listener.handle_app_mention(event, cfg, *conftest.thread_blocks(session_blocks), client)


@pytest.mark.asyncio
async def test_a_router_error_posts_to_the_thread_and_creates_no_session(tmp_path, monkeypatch) -> None:
  import test_slack_listener as slack_tests
  script = _router_script(tmp_path)
  cfg = _thread_cfg(tmp_path, monkeypatch, sys.executable, script, "fail")
  session_blocks = conftest.build_session_blocks(cfg)
  conftest.bind_deps_blocks(monkeypatch, conftest.build_task_tree(cfg, session_blocks), session_blocks)
  client = conftest.FakeSlackClient()
  event = slack_tests._make_event()

  with pytest.raises(backend_pool.RouterError, match="router boom"):
    await slack_listener_sid(event, cfg, session_blocks, client)

  # The error reached the thread, and no session exists for it.
  posts = [c for name, c in client.calls if name == "post_message"]
  assert len(posts) == 1 and "router boom" in posts[0]["text"]
  assert await session_blocks.store.get_session(slack_tests._sid(event)) is None


@pytest.mark.asyncio
async def test_an_empty_route_uses_the_pool_for_the_thread_session(tmp_path, monkeypatch) -> None:
  import test_slack_listener as slack_tests
  script = _router_script(tmp_path)
  cfg = _thread_cfg(tmp_path, monkeypatch, sys.executable, script, "empty")
  session_blocks = conftest.build_session_blocks(cfg)
  conftest.bind_deps_blocks(monkeypatch, conftest.build_task_tree(cfg, session_blocks), session_blocks)
  client = conftest.FakeSlackClient()
  tasks = slack_tests._spawn_round_tasks()
  event = slack_tests._make_event()

  with conftest.mention_seam(tasks) as _trigger:
    sid = await slack_listener_sid(event, cfg, session_blocks, client)
    await asyncio.gather(*tasks)

  meta = await session_blocks.store.get_session(sid)
  assert meta is not None
  fields = metadata_slots.fields_of(meta, backend_pool.METADATA_OWNER)
  assert fields.route is not None and fields.route.backends == []
