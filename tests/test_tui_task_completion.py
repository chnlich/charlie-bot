"""The TUI task's complete public path: create, terminal attach, status, stop,
durable input, operator acknowledgement, and completion through the same
guards — under a scripted isolated terminal backend (no real tmux, no real
claude process, no copied native ids).

The operator flow this pins: a terminal-driven manager task takes its input
through the public message route (durable and pending — the terminal is its
execution surface), the operator resolves exactly the inputs they handled via
the acknowledge route, and completion runs the ordinary closure guards. Later
arrivals keep blocking; replays are stable; run-token agents cannot confirm on
behalf of the operator; a quiet or detached terminal completes nothing.
"""

from __future__ import annotations

import asyncio
import base64
import os
import queue
import threading
from contextlib import suppress
from pathlib import Path

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.core import event_types as ET
from src.core.models import TaskSpec
from src.core.run_token import RunTokenClaims, sign_run_token


class ScriptedTtyAttachment:
  """The scripted native terminal: a real pipe for the pump, recorded writes,
  no process, no tmux, no real native ids."""

  instances: "dict[str, ScriptedTtyAttachment]" = {}

  def __init__(self, session_id: str) -> None:
    self.session_id = session_id
    self.spawned = False
    self.closed = False
    self.written = bytearray()
    self._read_fd, self._write_fd = os.pipe()
    os.set_blocking(self._read_fd, False)
    ScriptedTtyAttachment.instances[session_id] = self

  @property
  def fd(self) -> int:
    return self._read_fd

  def emit(self, data: bytes) -> None:
    os.write(self._write_fd, data)

  def spawn(self) -> None:
    self.spawned = True

  def write(self, data: bytes) -> None:
    self.written.extend(data)

  def resize(self, cols: int, rows: int) -> None:
    return

  def close(self) -> None:
    if self.closed:
      return
    self.closed = True
    for fd in (self._read_fd, self._write_fd):
      with suppress(OSError):
        os.close(fd)

  def is_alive(self) -> bool:
    return not self.closed


@pytest.fixture()
def tui_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
  """One synthetic instance with a tui backend, the full public API mounted
  (sessions + chat + the real session websocket route), and the terminal
  boundary scripted."""
  import src.core.config as core_config
  from src.api import chat as chat_api
  from src.api import sessions as sessions_api
  from src.api.deps import (
      get_config,
      get_config_on_loop,
      get_run_store,
      get_session_manager,
      get_task_manager,
  )
  from src.core.config import CharlieBotConfig
  from src.core.sessions import SessionManager
  from src.core.task_sessions import TaskTreeManager

  home = tmp_path / "charliebot-home"
  cfg = CharlieBotConfig(
      charliebot_home=home,
      paths={"worktree_dir": str(home / "worktrees")},
      backends={"options": [
          {"id": "fake", "label": "Fake", "type": "codex", "model": "fake-model"},
          {"id": "claude-tui", "label": "Claude TUI", "type": "tui-cli"},
      ]})
  core_config._credentials_cache.seed(
      core_config.Credentials(path=home / "credentials.yaml",
                              sections={"charliebot": {"access_key": "tui-op-key"}}))
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  session_mgr = SessionManager(cfg)
  tree = TaskTreeManager(cfg, session_mgr)

  # The terminal boundary: tmux + the attach PTY are scripted; the relay and
  # the pump run for real against the scripted pipe.
  import src.agents.backends.tui as tui

  ensured: list[tuple[str, Path]] = []
  tmux_live: set[str] = set()

  async def fake_ensure_tmux_session(session_id: str, working_dir: Path, **kwargs) -> None:
    ensured.append((session_id, working_dir))
    working_dir.mkdir(parents=True, exist_ok=True)
    tmux_live.add(session_id)

  async def fake_tmux_session_exists(session_id: str) -> bool:
    return session_id in tmux_live

  killed: list[str] = []

  async def fake_kill_tmux_session(session_id: str) -> None:
    killed.append(session_id)
    tmux_live.discard(session_id)

  monkeypatch.setattr(tui, "ensure_tmux_session", fake_ensure_tmux_session)
  monkeypatch.setattr(tui, "tmux_session_exists", fake_tmux_session_exists)
  monkeypatch.setattr(tui, "kill_tmux_session", fake_kill_tmux_session)
  monkeypatch.setattr(tui, "PtyAttachment", ScriptedTtyAttachment)

  import server as server_module
  from server import session_websocket, streaming_manager

  async def fake_ws_auth(websocket) -> bool:
    return True
  monkeypatch.setattr(server_module, "_check_ws_auth", fake_ws_auth)
  monkeypatch.setattr(server_module, "session_manager", lambda: session_mgr)
  monkeypatch.setattr(server_module, "get_config", lambda: cfg)

  app = FastAPI()
  app.include_router(sessions_api.router, prefix="/api/sessions")
  app.include_router(chat_api.router, prefix="/api/chat")
  app.add_api_websocket_route("/ws/sessions/{session_id}", session_websocket)
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_config_on_loop] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[get_task_manager] = lambda: tree
  app.dependency_overrides[get_run_store] = lambda: tree.runs

  return cfg, session_mgr, tree, TestClient(app), ensured, killed, streaming_manager


def _create_tui_task(client: TestClient) -> dict:
  resp = client.post("/api/sessions/", json={
      "request_id": "create-tui-task",
      "name": "TUI Feature",
      "profile": "manager",
      "backend": "claude-tui",
      "task": {"goal": "Ship the terminal-driven feature"},
  })
  assert resp.status_code == 200, resp.text
  return resp.json()


def _send_input(client: TestClient, session_id: str, content: str, request_id: str) -> dict:
  resp = client.post(f"/api/chat/{session_id}/message",
                     json={"content": content, "request_id": request_id})
  assert resp.status_code == 202, resp.text
  return resp.json()


@pytest.mark.asyncio
async def test_public_tui_task_full_route_under_scripted_terminal(
        tui_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """Create → attach (stable task identity) → status → stop, then the durable
  input, the operator acknowledgement, and the completion through the ordinary
  guards. A second synthetic instance stays untouched throughout."""
  cfg, session_mgr, tree, client, ensured, killed, streaming = tui_env

  # A second, isolated synthetic instance: its data must never be touched.
  other_home = cfg.charliebot_home.parent / "second-instance-home"
  from src.core.config import CharlieBotConfig
  from src.core.sessions import SessionManager
  from src.core.task_sessions import TaskTreeManager
  other_cfg = CharlieBotConfig(
      charliebot_home=other_home,
      backends={"options": [
          {"id": "fake", "label": "Fake", "type": "codex", "model": "fake-model"}]})
  other_mgr = SessionManager(other_cfg)
  other_tree = TaskTreeManager(other_cfg, other_mgr)
  other_task = await other_tree.create_task(
      request_id="other", task_parent_id=None, profile="manager",
      task=TaskSpec(goal="decoy"), name="Decoy", backend=None, caller="operator")
  other_events_before = len(other_tree.events.load_events(other_task.id))

  task = _create_tui_task(client)
  session_id = task["id"]
  meta = await tree.load_meta(session_id)
  assert meta is not None and meta.profile == "manager"
  assert meta.backend == "claude-tui"
  assert meta.task is not None and meta.task.goal == "Ship the terminal-driven feature"

  # The durable initial input through the public message route: accepted, and
  # the decision names the terminal transport (no headless turn started).
  first = _send_input(client, session_id, "first terminal-directed instruction", "input-1")
  assert first["launch"] is False
  assert "terminal" in first["reason"]
  input_event_id = first["input_event_id"]
  pending = tree.dispatch.pending_inputs(session_id)
  assert [str(e.get("id")) for e in pending] == [input_event_id]

  # Attach through the real websocket route: the tmux session is ensured under
  # the stable task id, the terminal bytes pump through, and input reaches the
  # scripted attachment.
  with client.websocket_connect(f"/ws/sessions/{session_id}?token=x") as ws:
    ws.send_json({"type": "cursor", "index": 0})
    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
      attachment = ScriptedTtyAttachment.instances.get(session_id)
      if attachment is not None and attachment.spawned:
        break
      await asyncio.sleep(0.05)
    else:
      pytest.fail("the attach path never spawned the task's own terminal")
    assert ensured and ensured[0][0] == session_id
    assert ensured[0][1] == cfg.sessions_dir / session_id
    frames: queue.Queue = queue.Queue()

    def _read_frames() -> None:
      try:
        while True:
          frames.put(ws.receive_json())
      except Exception as e:  # the socket closed; the reader ends
        frames.put(e)

    threading.Thread(target=_read_frames, daemon=True, name="tui-frame-reader").start()

    async def _next_frame(timeout: float = 10.0) -> dict:
      deadline = asyncio.get_event_loop().time() + timeout
      while asyncio.get_event_loop().time() < deadline:
        try:
          item = await asyncio.wait_for(asyncio.to_thread(frames.get, True, 0.2), 1.0)
        except TimeoutError:
          continue
        if isinstance(item, BaseException):
          raise item
        return item
      raise AssertionError("no frame arrived before the deadline")

    # Catchup deltas and catchup_complete come first; then the terminal frame.
    attachment.emit(b"terminal ready")
    frame = None
    while frame is None:
      msg = await _next_frame()
      if msg.get("type") == "pty_output":
        frame = msg
    assert base64.b64decode(frame["data"]) == b"terminal ready"
    ws.send_json({"type": "pty_input", "data": base64.b64encode(b"ls\n").decode()})
    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline and not attachment.written:
      await asyncio.sleep(0.05)
    assert bytes(attachment.written) == b"ls\n"

  # Detach leaves the task open with its input pending: silence and detach
  # never complete anything, and no completion fact appeared.
  assert tree.task_state(session_id) == "open"
  assert [e for e in tree.events.load_events(session_id) if e.get("type") == ET.TASK_CLOSED] == []

  # Status sees the scripted terminal; stop goes through the real route. The
  # jsonl activity glob reads HOME — redirect it to the synthetic home so no
  # real native session state is ever read or written.
  monkeypatch.setenv("HOME", str(cfg.charliebot_home))
  status = client.get("/api/sessions/tui/status", params={"ids": session_id})
  assert status.status_code == 200, status.text
  assert status.json().get(session_id, {}).get("running") is True
  stop = client.post(f"/api/sessions/{session_id}/tui/stop")
  assert stop.status_code == 200, stop.text
  assert killed == [session_id]

  # The stop is the ordinary explicit stop, not a completion.
  assert tree.task_state(session_id) == "open"
  assert [e for e in tree.events.load_events(session_id) if e.get("type") == ET.TASK_CLOSED] == []

  # A later input arrives concurrently with the acknowledgement of the first.
  second = _send_input(client, session_id, "a later instruction", "input-2")
  second_id = second["input_event_id"]

  # A run-token agent cannot confirm on behalf of the operator.
  agents_task = await tree.create_task(
      request_id="agent-leaf", task_parent_id=session_id, profile="worker",
      task=TaskSpec(goal="leaf"), name="AL", backend=None, caller="operator")
  from src.core.models import RunRecord
  await tree.runs.register_run(RunRecord(id="agent-run", session_id=agents_task.id, kind="work"))
  import subprocess

  from src.core.runs import read_pid_stat
  proc = subprocess.Popen(["/bin/sleep", "30"])
  pair = read_pid_stat(proc.pid)
  await tree.runs.record_launch(agents_task.id, "agent-run", pid=proc.pid, pid_start=pair[0])
  agent_token = sign_run_token(
      RunTokenClaims(session_id=agents_task.id, run_id="agent-run", agent="worker"),
      "tui-op-key")
  try:
    ack_forbidden = client.post(
        f"/api/sessions/{session_id}/task-inputs/acknowledge",
        json={"request_id": "agent-ack", "input_ids": [input_event_id], "note": "agent says so"},
        headers={"Authorization": f"Bearer {agent_token}"})
    assert ack_forbidden.status_code == 403
    assert "operator credentials" in ack_forbidden.json()["detail"]

    # Completion with an unresolved input is still blocked by the ordinary guard.
    blocked_complete = client.post(f"/api/sessions/{session_id}/complete", json={
        "request_id": "complete-1",
        "summary": "delivered through the terminal",
        "result_refs": ["terminal:transcript"],
    })
    assert blocked_complete.status_code == 409
    blockers = blocked_complete.json()["detail"]["blockers"]
    assert any("unprocessed input" in b and second_id in b for b in blockers), blockers

    # The operator resolves exactly the first input; the later one stays pending.
    ack = client.post(f"/api/sessions/{session_id}/task-inputs/acknowledge", json={
        "request_id": "ack-1", "input_ids": [input_event_id], "note": "handled in the terminal"})
    assert ack.status_code == 200, ack.text
    ack_body = ack.json()
    assert ack_body["input_ids"] == [input_event_id]
    assert ack_body["already_acknowledged"] == []
    assert ack_body["acknowledged_event_id"]

    # Replay: the same request id returns the original acknowledgement.
    replay = client.post(f"/api/sessions/{session_id}/task-inputs/acknowledge", json={
        "request_id": "ack-1", "input_ids": [input_event_id], "note": "handled in the terminal"})
    assert replay.status_code == 200
    assert replay.json() == ack_body
    # Re-acking an acknowledged id is an idempotent no-op (no second fact).
    noop = client.post(f"/api/sessions/{session_id}/task-inputs/acknowledge", json={
        "request_id": "ack-2", "input_ids": [input_event_id]})
    assert noop.status_code == 200
    assert noop.json()["input_ids"] == []
    assert noop.json()["already_acknowledged"] == [input_event_id]

    # The later input still blocks; acknowledging it resolves the last one.
    still = client.post(f"/api/sessions/{session_id}/complete", json={
        "request_id": "complete-2",
        "summary": "delivered through the terminal",
        "result_refs": ["terminal:transcript"],
    })
    assert still.status_code == 409
    assert any("unprocessed input" in b and second_id in b
               for b in still.json()["detail"]["blockers"]), still.json()["detail"]
    ack2 = client.post(f"/api/sessions/{session_id}/task-inputs/acknowledge", json={
        "request_id": "ack-3", "input_ids": [second_id]})
    assert ack2.status_code == 200

    # Unknown ids refuse; they never silently become acknowledgements.
    unknown = client.post(f"/api/sessions/{session_id}/task-inputs/acknowledge", json={
        "request_id": "ack-4", "input_ids": ["00000000-0000-0000-0000-00000000dead"]})
    assert unknown.status_code == 409

    # The agent child's active Run and open state keep their normal blockers
    # too; the operator stops that run and cancels the child explicitly before
    # completing.
    run_cancel = client.post(f"/api/sessions/{agents_task.id}/runs/agent-run/cancel", json={
        "request_id": "cancel-agent-run"})
    assert run_cancel.status_code == 200, run_cancel.text
    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
        if tree.runs.terminal_outcome(
                tree.runs.load_events_sync(agents_task.id), "agent-run") is not None:
            break
        await asyncio.sleep(0.05)
    else:
        pytest.fail("the agent run never reached its terminal fact after cancellation")
    cancelled = client.post(f"/api/sessions/{agents_task.id}/cancel", json={
        "request_id": "cancel-agent-leaf", "reason": "not needed"})
    assert cancelled.status_code == 200, cancelled.text
    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
        if tree.task_state(agents_task.id) != "open":
            break
        await asyncio.sleep(0.05)
    else:
        pytest.fail("the agent child never settled after cancellation")

    # The cancelled child's report landed on this node as a durable input (the
    # ordinary delivery chain). The operator resolved it in the terminal too.
    remaining = tree.dispatch.pending_inputs(session_id)
    ack_remaining = client.post(f"/api/sessions/{session_id}/task-inputs/acknowledge", json={
        "request_id": "ack-5",
        "input_ids": [str(e.get("id")) for e in remaining],
    })
    assert ack_remaining.status_code == 200, ack_remaining.text
    assert tree.dispatch.pending_inputs(session_id) == []

    # The explicit operator completion now passes the ordinary guards: no runs
    # exist on this terminal-driven node, so the operator's own attributed
    # evidence carries it.
    done = client.post(f"/api/sessions/{session_id}/complete", json={
        "request_id": "complete-3",
        "summary": "feature delivered via the terminal session",
        "result_refs": ["terminal:transcript"],
    })
    assert done.status_code == 200, done.text
    assert tree.task_state(session_id) == "completed"
    closed = [e for e in tree.events.load_events(session_id) if e.get("type") == ET.TASK_CLOSED]
    assert len(closed) == 1
    assert closed[0]["outcome"] == "completed"
    assert closed[0]["actor"] == "user"
    acks = [e for e in tree.events.load_events(session_id)
            if e.get("type") == ET.TASK_INPUT_ACKNOWLEDGED]
    acked_ids = sorted(i for e in acks for i in e["input_ids"])
    assert acked_ids == sorted([input_event_id, second_id] + [str(e.get("id")) for e in remaining])
    assert all(e["actor"] == "user" for e in acks)
    # Exactly the three acknowledgement facts the operator issued (the no-op
    # replay wrote nothing), no more.
    assert len(acks) == 3
  finally:
    proc.terminate()

  # The second instance's data was never touched.
  assert len(other_tree.events.load_events(other_task.id)) == other_events_before
  assert other_tree.task_state(other_task.id) == "open"
  assert not (other_home / "sessions" / session_id).exists()


@pytest.mark.asyncio
async def test_acknowledged_input_unblocks_a_structural_operation(
        tui_env) -> None:
  """The acknowledgement is durable task state: the folded pending set loses
  the acknowledged ids, so the same guard set the tree UI reads reports the
  node unblocked without any second model."""
  cfg, session_mgr, tree, client, ensured, killed, streaming = tui_env
  task = _create_tui_task(client)
  session_id = task["id"]
  first = _send_input(client, session_id, "the instruction", "input-1")
  input_event_id = first["input_event_id"]
  # The same guard set the tree UI reads reports the pending input, and a
  # structural mutation is refused while it stands.
  await tree._get_index()
  blockers = tree.completion.completion_blockers(session_id)
  assert any("unprocessed input" in b and input_event_id in b for b in blockers), blockers
  from src.core.models import PatchSessionTaskRequest
  from src.core.run_token import CallerIdentity
  with pytest.raises(Exception) as exc:
    await tree.patch_task(
        session_id, PatchSessionTaskRequest(task_parent_id=None),
        caller=CallerIdentity(kind="operator"))
  assert "unprocessed input" in str(exc.value)
  ack = client.post(f"/api/sessions/{session_id}/task-inputs/acknowledge", json={
      "request_id": "ack-1", "input_ids": [input_event_id]})
  assert ack.status_code == 200
  await tree._get_index()
  assert tree.completion.completion_blockers(session_id) == []
  renamed = await tree.patch_task(
      session_id, PatchSessionTaskRequest(name="renamed"), caller=CallerIdentity(kind="operator"))
  assert renamed.name == "renamed"
