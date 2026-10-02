"""Unit tests for the Discord gateway listener loop (src.core.discord_listener.run_listener).

Each test drives the real loop over a scripted fake websocket handed out through
the patched ``_connect``, with the REST client, the MESSAGE_CREATE handler and
the READY backfill as mocks. The listener's sleeps compress 1000x (a 1 s
reconnect backoff waits 1 ms, a 30 000 ms heartbeat interval beats every
30 ms), so every test stays inside the 2 s unit budget on the real wall clock.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import sys
import time
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
    BROADCAST_PATCH_TARGET,
    DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET,
    build_slack_cfg,
    cancel_and_drain,
    fake_backends,
    make_task_spawner,
    stub_credentials,
)
from structlog.testing import capture_logs
from websockets.exceptions import ConnectionClosedError
from websockets.frames import Close

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.discord_client import REQUIRED_PERMISSIONS
from src.core.discord_listener import (
    _INTENTS,
    _STOP_CLOSE_CODES,
    run_listener,
)
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager

_CONNECT_PATCH_TARGET = "src.core.discord_listener._connect"
_HANDLER_PATCH_TARGET = "src.core.discord_listener.handle_message_create"
_BACKFILL_PATCH_TARGET = "src.core.discord_listener._backfill_followed_threads"

_GATEWAY_URL = "wss://gateway.discord.gg"
_BOT_USER = "600000000000000001"
_GUILD = "900000000000000001"
_USER = "700000000000000001"

_REAL_SLEEP = asyncio.sleep


class _IdleClosedError(Exception):
  """Internal fake-socket signal: the script is empty and the socket closed."""


async def _compressed_sleep(delay: float) -> None:
  """The listener's sleep with the clock turned 1000x faster."""
  await _REAL_SLEEP(delay / 1000)


class FakeGatewaySocket:
  """Scripted gateway websocket: replays payloads in order, then idles until closed.

  A scripted ``close_code`` raises the real ConnectionClosedError at the first
  received frame after the script runs out — the shape websockets itself hands
  the listener for a server-initiated close. Sent frames land in ``sent`` as
  parsed JSON, close requests in ``closes`` in order. With ``ack_heartbeats``
  every op 1 the listener sends gets its op 11 ACK fed back to the receive
  side, the way the real gateway answers each heartbeat.
  """

  def __init__(self, payloads: list[dict], *, close_code: int | None = None, ack_heartbeats: bool = False) -> None:
    self.sent: list[dict] = []
    self.closes: list[int] = []
    self._queue: asyncio.Queue[dict | None] = asyncio.Queue()
    for payload in payloads:
      self._queue.put_nowait(payload)
    self._close_code = close_code
    self._ack_heartbeats = ack_heartbeats

  async def send(self, raw: str) -> None:
    message = json.loads(raw)
    self.sent.append(message)
    if self._ack_heartbeats and message.get("op") == 1:
      self._queue.put_nowait({"op": 11})

  async def close(self, code: int = 1000) -> None:
    self.closes.append(code)
    self._queue.put_nowait(None)  # wake a recv parked on the drained script: the socket is closed

  async def recv(self) -> str:
    if self._queue.empty() and self._close_code is not None:
      code, self._close_code = self._close_code, None
      raise ConnectionClosedError(Close(code, "server closed"), None)
    item = await self._queue.get()
    if item is None:
      raise _IdleClosedError
    return json.dumps(item)

  def __aiter__(self) -> FakeGatewaySocket:
    return self

  async def __anext__(self) -> str:
    try:
      return await self.recv()
    except _IdleClosedError:
      raise StopAsyncIteration from None


class FakeDiscordREST:
  """The REST face run_listener needs: the application's flags, guild permissions, the gateway url."""

  def __init__(self, *, flags: int = 1 << 18, guilds: list[dict] | None = None) -> None:
    self.flags = flags
    self.guilds = guilds or []

  async def get_current_application(self) -> dict:
    return {"id": "1", "flags": self.flags}

  async def list_current_user_guilds(self) -> list[dict]:
    return self.guilds

  async def get_gateway_url(self) -> str:
    return _GATEWAY_URL


@contextlib.contextmanager
def _listener(
    sockets: list[FakeGatewaySocket],
    *,
    flags: int = 1 << 18,
    guilds: list[dict] | None = None,
    handler: AsyncMock | None = None,
) -> Iterator[dict[str, Any]]:
  """Patch every seam run_listener touches; yield the handles the tests assert on.

  Sockets are handed out one per ``_connect`` call; the handler and the
  backfill are AsyncMocks (a pre-built *handler* carries a test's side
  effects), so the gateway loop is the only real code on the wire.
  """
  stub_credentials({"discord": {"bot_token": "test-bot-token"}})
  rest = FakeDiscordREST(flags=flags, guilds=guilds or [])
  handler = handler if handler is not None else AsyncMock(return_value=None)
  backfill = AsyncMock(return_value=0)
  connects: list[str] = []

  async def _fake_connect(url: str) -> FakeGatewaySocket:
    connects.append(url)
    return sockets.pop(0)

  with contextlib.ExitStack() as stack:
    stack.enter_context(patch(_CONNECT_PATCH_TARGET, new=_fake_connect))
    stack.enter_context(patch(DISCORD_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=rest))
    stack.enter_context(patch(_HANDLER_PATCH_TARGET, new=handler))
    stack.enter_context(patch(_BACKFILL_PATCH_TARGET, new=backfill))
    stack.enter_context(patch("src.core.discord_listener.asyncio.sleep", new=_compressed_sleep))
    yield {"rest": rest, "connects": connects, "handler": handler, "backfill": backfill}


def _rig(tmp_path: Path) -> tuple[CharlieBotConfig, SessionManager]:
  """The config and session manager under tmp_path, with the stubbed test token."""
  stub_credentials({"discord": {"bot_token": "test-bot-token"}})
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      discord={"allowed_users": {
          _USER: "tester"
      }},
      backends=fake_backends(),
  )
  return cfg, SessionManager(cfg)


async def _until(predicate: Callable[[], bool], timeout: float = 0.9) -> None:
  """Poll until predicate() holds, on the real clock, while the listener runs compressed."""
  deadline = time.monotonic() + timeout
  while not predicate():
    if time.monotonic() > deadline:
      raise AssertionError("condition not reached within the poll budget")
    await _REAL_SLEEP(0.005)


def _hello(interval_ms: int = 30_000) -> dict:
  return {"op": 10, "d": {"heartbeat_interval": interval_ms}}


def _ready(seq: int = 2) -> dict:
  return {"op": 0, "t": "READY", "s": seq, "d": {"user": {"id": _BOT_USER}}}


def _message(seq: int) -> dict:
  return {"op": 0, "t": "MESSAGE_CREATE", "s": seq, "d": {"id": str(seq), "channel_id": _GUILD, "content": "x"}}


# ---------------------------------------------------------------------------
# Identify and heartbeat
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_identify_carries_token_and_intents(tmp_path: Path) -> None:
  cfg, session_mgr = _rig(tmp_path)
  ws = FakeGatewaySocket([_hello(), _ready()])
  with _listener([ws]):
    task = asyncio.create_task(run_listener(cfg, session_mgr))
    try:
      await _until(lambda: any(m.get("op") == 2 for m in ws.sent))
      identify = next(m for m in ws.sent if m.get("op") == 2)
      assert identify == {
          "op": 2,
          "d":
              {
                  "token": "test-bot-token",
                  "intents": 37377,
                  "properties": {
                      "os": sys.platform,
                      "browser": "charlie-bot",
                      "device": "charlie-bot"
                  },
              },
      }
      assert _INTENTS == 37377
    finally:
      await cancel_and_drain(task)


@pytest.mark.asyncio
async def test_heartbeat_carries_the_last_seq(tmp_path: Path) -> None:
  cfg, session_mgr = _rig(tmp_path)
  ws = FakeGatewaySocket([_hello(30_000), _ready(seq=7)])
  with _listener([ws]):
    task = asyncio.create_task(run_listener(cfg, session_mgr))
    try:
      await _until(lambda: any(m == {"op": 1, "d": 7} for m in ws.sent))
    finally:
      await cancel_and_drain(task)


@pytest.mark.asyncio
async def test_acked_heartbeats_keep_beating_one_interval_apart(tmp_path: Path) -> None:
  """A beat whose previous beat got its op 11 is answered with the next beat an interval later, not a close."""
  cfg, session_mgr = _rig(tmp_path)
  ws = FakeGatewaySocket([_hello(30_000), _ready()], ack_heartbeats=True)
  with _listener([ws]):
    task = asyncio.create_task(run_listener(cfg, session_mgr))
    try:
      # the third beat only exists when the earlier beats each saw their ACK
      # and the loop waited out the interval instead of closing at once
      await _until(lambda: len([m for m in ws.sent if m.get("op") == 1]) >= 3)
      assert ws.closes == []
    finally:
      await cancel_and_drain(task)


@pytest.mark.asyncio
async def test_server_op1_is_answered_at_once(tmp_path: Path) -> None:
  cfg, session_mgr = _rig(tmp_path)
  # A one-hour heartbeat interval: the scheduler never beats inside the test,
  # so the only op 1 on the wire is the answer to the server's demand.
  ws = FakeGatewaySocket([_hello(3_600_000), {"op": 1}])
  with _listener([ws]):
    task = asyncio.create_task(run_listener(cfg, session_mgr))
    try:
      await _until(lambda: any(m == {"op": 1, "d": None} for m in ws.sent))
      # exactly two frames went out: the identify, then the op 1 answer (d=None —
      # no frame carried a seq yet); a scheduler beat could not have fired yet.
      assert ws.sent[0]["op"] == 2
      assert ws.sent[1:] == [{"op": 1, "d": None}]
    finally:
      await cancel_and_drain(task)


# ---------------------------------------------------------------------------
# Reconnects
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_missing_ack_closes_and_reconnects(tmp_path: Path) -> None:
  cfg, session_mgr = _rig(tmp_path)
  ws1 = FakeGatewaySocket([_hello(30_000)])  # no ACK ever comes
  ws2 = FakeGatewaySocket([_hello(3_600_000)])
  with _listener([ws1, ws2]) as rig, capture_logs() as logs:
    task = asyncio.create_task(run_listener(cfg, session_mgr))
    try:
      await _until(lambda: len(rig["connects"]) == 2)
      assert ws1.closes[0] == 4000
      assert any(e["event"] == "discord_listener_connection_dropped" for e in logs)
    finally:
      await cancel_and_drain(task)


@pytest.mark.asyncio
async def test_op7_and_op9_end_the_connection_and_reconnect(tmp_path: Path) -> None:
  cfg, session_mgr = _rig(tmp_path)
  ws1 = FakeGatewaySocket([_hello(), _ready(seq=2), {"op": 7}])
  ws2 = FakeGatewaySocket([_hello(), _ready(seq=3), {"op": 9}])
  ws3 = FakeGatewaySocket([_hello(3_600_000)])
  with _listener([ws1, ws2, ws3]) as rig, capture_logs() as logs:
    task = asyncio.create_task(run_listener(cfg, session_mgr))
    try:
      await _until(lambda: len(rig["connects"]) == 3)
      dropped = [e for e in logs if e["event"] == "discord_listener_connection_dropped"]
      assert len(dropped) == 2
      # every reconnect identifies afresh
      for ws in (ws1, ws2, ws3):
        assert any(m.get("op") == 2 for m in ws.sent)
    finally:
      await cancel_and_drain(task)


@pytest.mark.asyncio
@pytest.mark.parametrize("code", [4004, 4014])
async def test_stop_close_codes_stop_without_reconnecting(tmp_path: Path, code: int) -> None:
  cfg, session_mgr = _rig(tmp_path)
  ws = FakeGatewaySocket([_hello(), _ready()], close_code=code)
  with _listener([ws]) as rig, capture_logs() as logs:
    await asyncio.wait_for(run_listener(cfg, session_mgr), 1)
  assert rig["connects"] == [_GATEWAY_URL]
  stopped = [e for e in logs if e["event"] == "discord_listener_stopped"]
  assert len(stopped) == 1
  assert stopped[0]["log_level"] == "error"
  assert stopped[0]["code"] == code
  assert stopped[0]["reason"] == _STOP_CLOSE_CODES[code]
  if code == 4014:
    assert "Message Content" in stopped[0]["reason"]


# ---------------------------------------------------------------------------
# READY and dispatch
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_ready_runs_the_backfill_per_connection(tmp_path: Path) -> None:
  cfg, session_mgr = _rig(tmp_path)
  ws = FakeGatewaySocket([_hello(), _ready()])
  with _listener([ws]) as rig, capture_logs() as logs:
    task = asyncio.create_task(run_listener(cfg, session_mgr))
    try:
      await _until(lambda: rig["backfill"].await_count == 1)
      rig["backfill"].assert_awaited_once()
      cfg_arg, mgr_arg, client_arg, trigger_arg = rig["backfill"].await_args.args
      assert cfg_arg is cfg
      assert mgr_arg is session_mgr
      assert client_arg is rig["rest"]
      assert isinstance(trigger_arg, TriggerManager)
      assert any(e["event"] == "discord_listener_connected" for e in logs)
    finally:
      await cancel_and_drain(task)


@pytest.mark.asyncio
async def test_message_create_reaches_the_handler_and_survives_its_errors(tmp_path: Path) -> None:
  cfg, session_mgr = _rig(tmp_path)
  handler = AsyncMock(side_effect=[RuntimeError("boom"), "sid-2"])
  ws = FakeGatewaySocket([_hello(), _ready(), _message(seq=3), _message(seq=4)])
  with _listener([ws], handler=handler) as rig, capture_logs() as logs:
    task = asyncio.create_task(run_listener(cfg, session_mgr))
    try:
      # the second dispatch proves the loop continued past the first failure
      await _until(lambda: rig["handler"].await_count == 2)
      first, second = rig["handler"].await_args_list
      assert first.args[0] == {"id": "3", "channel_id": _GUILD, "content": "x"}
      assert first.kwargs == {"bot_user_id": _BOT_USER}
      assert second.args[0]["id"] == "4"
      assert second.kwargs == {"bot_user_id": _BOT_USER}
      failed = [e for e in logs if e["event"] == "discord_listener_message_handle_failed"]
      assert len(failed) == 1
      assert failed[0]["log_level"] == "error"
    finally:
      await cancel_and_drain(task)


# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_preflight_intent_off_returns_before_connecting(tmp_path: Path) -> None:
  cfg, session_mgr = _rig(tmp_path)
  ws = FakeGatewaySocket([])  # would serve a connection; must never be reached
  with _listener([ws], flags=0) as rig, capture_logs() as logs:
    await asyncio.wait_for(run_listener(cfg, session_mgr), 1)
  assert rig["connects"] == []
  off = [e for e in logs if e["event"] == "discord_listener_message_content_intent_off"]
  assert len(off) == 1
  assert off[0]["log_level"] == "error"


@pytest.mark.asyncio
async def test_preflight_logs_missing_permission_names_and_keeps_running(tmp_path: Path) -> None:
  cfg, session_mgr = _rig(tmp_path)
  permissions = sum(bit for name, bit in REQUIRED_PERMISSIONS.items() if name != "ADD_REACTIONS")
  guilds = [{"id": _GUILD, "name": "town", "permissions": str(permissions)}]
  ws = FakeGatewaySocket([_hello(), _ready()])
  with _listener([ws], guilds=guilds) as rig, capture_logs() as logs:
    task = asyncio.create_task(run_listener(cfg, session_mgr))
    try:
      # the loop kept running: the connection happened and READY ran the backfill
      await _until(lambda: rig["backfill"].await_count == 1)
      missing = [e for e in logs if e["event"] == "discord_listener_missing_permissions"]
      assert len(missing) == 1
      assert missing[0]["guild"] == _GUILD
      assert missing[0]["missing"] == ["ADD_REACTIONS"]
    finally:
      await cancel_and_drain(task)


# ---------------------------------------------------------------------------
# The session-manager round-end hook
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_persisted_master_done_fires_both_deliver_tasks(tmp_path: Path) -> None:
  """A persisted master_done fires the Slack and the Discord deliver task at the same point."""
  cfg = build_slack_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="both"))
  done = {"type": ET.MASTER_DONE, "exit_code": 0, "still_thinking": False}
  tasks: list[asyncio.Task] = []
  with (
      patch("src.core.slack_listener.deliver_done", new=AsyncMock(return_value=True)) as slack_deliver,
      patch("src.core.discord_listener.deliver_done", new=AsyncMock(return_value=True)) as discord_deliver,
      patch("src.core.sessions.create_logged_task", side_effect=make_task_spawner(tasks)),
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
  ):
    await session_mgr.persist_and_broadcast(meta.id, done)
    await asyncio.gather(*tasks)

  slack_deliver.assert_awaited_once_with(meta.id, done, cfg, session_mgr)
  discord_deliver.assert_awaited_once_with(meta.id, done, cfg, session_mgr)
  assert sorted(t.get_name() for t in tasks) == [f"discord-deliver-{meta.id}", f"slack-deliver-{meta.id}"]
