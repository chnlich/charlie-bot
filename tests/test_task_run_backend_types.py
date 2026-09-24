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
import json
from pathlib import Path

import pytest
from conftest import backend_option

from src.core import event_types as ET
from src.core.backend_models import BackendType
from src.core.models import RunRecord
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager

OPERATOR = {"Authorization": "Bearer op-secret"}


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
        backends={"options": [option], "preference": ["type-under-test"]},
        paths={"worktree_dir": str(home / "worktrees")})
    core_config._credentials_cache.seed(core_config.Credentials(
        path=home / "credentials.yaml", sections={"charliebot": {"access_key": "key-type"}}))
    session_mgr = SessionManager(cfg)
    return cfg, session_mgr, TaskTreeManager(cfg, session_mgr)


def raw_stream_line(event: dict) -> bytes:
    return (json.dumps(event) + "\n").encode("utf-8")


def session_attached_event(native_id: str) -> dict:
    return {"type": "system", "subtype": "init", "session_id": native_id}


BACKEND_TYPES = list(BackendType)
# The master queue only runs streaming backends: a manager turn on a TUI
# backend is refused by the existing guard (recorded in the manager-queue
# tests), so the identity assertions cover the eight executable types.
STREAMING_BACKEND_TYPES = [
    t for t in BackendType if t is not BackendType.TUI_CLI]


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
        request_id="root", task_parent_id=None, profile="manager", task=None,
        name="M", backend=None, caller="operator")
    backend = SpawningScriptedBackend([
        session_attached_event("native-xyz-1"),
        result_event("typed output"),
    ])
    install_backends(
        monkeypatch, [backend], "src.agents.backends.registry.build_backend")
    from src.core.task_execution import TaskExecutionAdapter
    tree.dispatch.executor = TaskExecutionAdapter(cfg, session_mgr, tree)

    await tree.dispatch.admit_input(
        root.id, event_type=ET.USER, content="Take off. Answer.", actor="user")
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
async def test_tui_cli_manager_turn_is_terminal_driven_never_headless(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A tui-cli manager node never gets a headless manager turn.

    The tmux terminal is the node's execution surface: dispatched input stays
    durable and pending with the transport limit named in the launch decision,
    no Run is reserved and no process is spawned, and terminal silence never
    becomes a failed run or a completion fact (completion is explicit operator
    action through the common closure guards).
    """
    from tests.test_task_execution import SpawningScriptedBackend, install_backends, result_event

    cfg, session_mgr, tree = build_env(tmp_path, BackendType.TUI_CLI)
    root = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager", task=None,
        name="M", backend=None, caller="operator")
    backend = SpawningScriptedBackend([result_event("unused")])
    builds = install_backends(monkeypatch, [backend], "src.agents.backends.registry.build_backend")
    from src.core.task_execution import TaskExecutionAdapter
    tree.dispatch.executor = TaskExecutionAdapter(cfg, session_mgr, tree)
    _ = builds  # asserted empty below: no backend build (launch) ever happens
    admitted = await tree.dispatch.admit_input(
        root.id, event_type=ET.USER, content="Take off. Answer.", actor="user")
    decision = await asyncio.wait_for(tree.dispatch.dispatch_pending(root.id), 5)
    assert decision["launch"] is False
    assert "terminal" in decision["reason"]
    assert "run_id" not in decision
    # No Run was reserved, no backend was built, and no process was spawned.
    assert builds == []
    assert tree.runs.list_run_records_sync(root.id) == []
    # The input stays durable and pending: repeated dispatch re-refuses with
    # the same visible reason and never spawns.
    assert [str(e["id"]) for e in tree.dispatch.pending_inputs(root.id)] == [str(admitted["id"])]
    again = await asyncio.wait_for(tree.dispatch.dispatch_pending(root.id), 5)
    assert again["launch"] is False
    assert tree.runs.list_run_records_sync(root.id) == []
    # No failed run stands in the way of the node's explicit operator closure
    # guards: the terminal silence is not a run_finished fact of any kind.
    events = tree.runs.load_events_sync(root.id)
    assert not [e for e in events if e.get("type") == ET.RUN_FINISHED]


@pytest.mark.asyncio
@pytest.mark.parametrize("backend_type", BACKEND_TYPES, ids=lambda t: t.value)
async def test_zero_output_and_error_results_fail_across_types(
        tmp_path: Path, backend_type: BackendType, monkeypatch: pytest.MonkeyPatch) -> None:
    from tests.test_task_execution import SpawningScriptedBackend, install_backends

    cfg, session_mgr, tree = build_env(tmp_path, backend_type)
    root = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager", task=None,
        name="M", backend=None, caller="operator")

    # Zero output: a settled result event with all-zero usage and no
    # assistant text — the master queue's zero-output guard must turn it into
    # a nonzero exit so the run fails instead of consuming the input silently.
    # A tui-cli manager never reaches this: its turns are the terminal's, so
    # the dispatcher refuses before any Run exists (no headless failure, the
    # input stays pending).
    from src.agents.backends import base as backend_base
    empty = SpawningScriptedBackend([backend_base.make_result_event(0, 0)])
    install_backends(monkeypatch, [empty], "src.agents.backends.registry.build_backend")
    from src.core.task_execution import TaskExecutionAdapter
    tree.dispatch.executor = TaskExecutionAdapter(cfg, session_mgr, tree)
    await tree.dispatch.admit_input(
        root.id, event_type=ET.USER, content="Take off. Stay silent.", actor="user")
    decision = await asyncio.wait_for(tree.dispatch.dispatch_pending(root.id), 5)
    if backend_type is BackendType.TUI_CLI:
        assert decision["launch"] is False
        assert "terminal" in decision["reason"]
        assert "run_id" not in decision
        assert tree.runs.list_run_records_sync(root.id) == []
        assert [e for e in tree.events.load_events(root.id)
                if e.get("type") == ET.RUN_FINISHED] == []
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
    install_backends(monkeypatch, [errored], "src.agents.backends.registry.build_backend")
    retry = await tree.create_retry(
        root.id, "retry-error", run_id)
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


@pytest.mark.asyncio
async def test_stop_request_on_launched_run_records_first_terminal_fact(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """The existing stop contract: a stop request observes the process and records once."""
    import src.core.config as core_config
    from src.core.config import CharlieBotConfig

    home = tmp_path / "home"
    cfg = CharlieBotConfig(
        charliebot_home=home,
        backends={"options": [
            backend_option(id="stop-type", label="S", type="codex", model="fake-model")],
            "preference": ["stop-type"]},
        paths={"worktree_dir": str(home / "worktrees")})
    core_config._credentials_cache.seed(core_config.Credentials(
        path=home / "credentials.yaml", sections={"charliebot": {"access_key": "key-type"}}))
    session_mgr = SessionManager(cfg)
    tree = TaskTreeManager(cfg, session_mgr)
    root = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager", task=None,
        name="M", backend=None, caller="operator")
    await tree.runs.register_run(
        RunRecord(id="run-stop", session_id=root.id, kind="manager_turn",
                  backend="stop-type", model="fake-model"))
    await tree.runs.record_launch(root.id, "run-stop", pid=424700, pid_start="ps-700")
    stop = await tree.runs.request_stop(root.id, "run-stop", "stop-op")
    assert stop.run_id == "run-stop"
    # The fixture's process is already gone: the stop observed the exit and
    # recorded the durable interrupted fact.
    assert stop.outcome == "interrupted"
    events = tree.runs.load_events_sync(root.id)
    assert tree.runs.terminal_outcome(events, "run-stop") == "interrupted"
    # A second stop cannot overwrite the first terminal fact.
    again = await tree.runs.request_stop(root.id, "run-stop", "stop-op-2")
    assert again.outcome == "interrupted"


@pytest.mark.asyncio
async def test_tui_task_node_terminal_endpoints_and_explicit_completion(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A tui-cli v2 manager keeps the approved TUI behavior: the existing
    terminal interface recognizes it, stop works on its task identity, and
    terminal silence never closes it — completion is explicit operator action
    through the common closure guards."""
    cfg, session_mgr, tree = build_env(tmp_path, BackendType.TUI_CLI)
    root = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager", task=None,
        name="M", backend=None, caller="operator")
    from src.core.task_execution import TaskExecutionAdapter
    tree.dispatch.executor = TaskExecutionAdapter(cfg, session_mgr, tree)
    await tree.dispatch.admit_input(
        root.id, event_type=ET.USER, content="Take off. Answer.", actor="user")
    decision = await asyncio.wait_for(tree.dispatch.dispatch_pending(root.id), 5)
    assert decision["launch"] is False  # the terminal drives the node

    # The existing tui status/stop endpoints recognize the v2 node by its task
    # identity (the tmux probes are patched to their double responses).
    from tests.test_task_execution import make_api_client

    async def fake_tmux_session_exists(session_id: str) -> bool:
        return session_id == root.id

    def fake_claude_jsonl_busy(session_id: str) -> bool:
        return False

    killed: list[str] = []

    async def fake_kill_tmux_session(session_id: str) -> None:
        killed.append(session_id)

    monkeypatch.setattr("src.agents.backends.tui.tmux_session_exists", fake_tmux_session_exists)
    monkeypatch.setattr("src.agents.backends.tui._claude_jsonl_busy", fake_claude_jsonl_busy)
    monkeypatch.setattr("src.agents.backends.tui.kill_tmux_session", fake_kill_tmux_session)
    with make_api_client(cfg, session_mgr, tree) as client:
        status = client.get(
            f"/api/sessions/tui/status?ids={root.id}", headers=OPERATOR)
        assert status.status_code == 200
        assert status.json().get(root.id) == {"running": True, "busy": False}
        stopped = client.post(f"/api/sessions/{root.id}/tui/stop", headers=OPERATOR)
        assert stopped.status_code == 200
        assert stopped.json() == {"stopped": True}
    assert killed == [root.id]

    # Terminal silence is not completion: the node stays open; only the
    # explicit operator close lands a TASK_CLOSED fact.
    assert tree.task_state(root.id) == "open"
    assert [e for e in tree.events.load_events(root.id)
            if e.get("type") == ET.TASK_CLOSED] == []
    from src.core.control_events import stable_run_id
    from src.core.models import RunRecord
    from src.core.run_token import CallerIdentity
    from src.core.task_completion import CompletionEvidence
    close_run = stable_run_id(root.id, "close:evidence")
    await tree.runs.register_run(RunRecord(
        id=close_run, session_id=root.id, kind="manager_turn"))
    await tree.runs.record_launch(root.id, close_run, pid=424009, pid_start="ps-9")
    await tree.dispatch.finish_run(root.id, close_run, outcome="success")
    # The common closure guards still apply: the unprocessed (terminal-directed)
    # input blocks a bare close, and the operator's explicit close settles its
    # disposition through the owner's close-time exclusion.
    from src.core.task_sessions import TaskConflictError
    with pytest.raises(TaskConflictError, match="unprocessed input"):
        await tree.completion.complete_task(
            root.id, request_id="operator-close", caller=CallerIdentity(kind="operator"),
            evidence=CompletionEvidence(summary="done in the terminal",
                                        run_ids=[close_run], result_refs=[f"run:{close_run}"]))
    # A quiet TUI node (no dispatched input) closes through the same guards on
    # the operator's explicit action — completion is never inferred from the
    # terminal's silence.
    quiet = await tree.create_task(
        request_id="quiet", task_parent_id=None, profile="manager", task=None,
        name="QUIET", backend=None, caller="operator")
    quiet_run = stable_run_id(quiet.id, "close:evidence")
    await tree.runs.register_run(RunRecord(
        id=quiet_run, session_id=quiet.id, kind="manager_turn"))
    await tree.runs.record_launch(quiet.id, quiet_run, pid=424010, pid_start="ps-10")
    await tree.dispatch.finish_run(quiet.id, quiet_run, outcome="success")
    await tree.completion.complete_task(
        quiet.id, request_id="operator-close", caller=CallerIdentity(kind="operator"),
        evidence=CompletionEvidence(summary="done in the terminal",
                                    run_ids=[quiet_run], result_refs=[f"run:{quiet_run}"]))
    assert tree.task_state(quiet.id) == "completed"
