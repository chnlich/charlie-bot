"""Scheduled triggers against an archived task node: creation refuses, the
fire-time backstop cancels with the named reason, and the CLI exits non-zero
printing the refusal reason. Real managers drive the tree; the CLI test rides a
real socket."""

from __future__ import annotations

import json
import sys
from datetime import UTC, datetime
from pathlib import Path

import pytest
from conftest import bind_deps_managers, build_env, create_task

from src.core.models import PendingTrigger
from src.core.triggers import ArchivedSessionError, TriggerManager


def _trigger(session_id: str, trigger_id: str = "trig-archived-1") -> PendingTrigger:
  return PendingTrigger(id=trigger_id, session_id=session_id, fire_at=datetime.now(UTC), message="wake")


@pytest.mark.asyncio
async def test_create_trigger_on_an_archived_task_node_is_refused(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  bind_deps_managers(monkeypatch, tree, session_mgr)
  node = await create_task(tree, parent=None, request_id="node")
  from conftest import OPERATOR
  await tree.archive_subtree(node.id, caller=OPERATOR)

  trigger_mgr = TriggerManager(cfg, session_mgr)
  with pytest.raises(ArchivedSessionError, match="target task is archived"):
    await trigger_mgr.create_trigger(node.id, 60, "check the run")

  # A pending trigger on the still-open node is untouched; the refusal wrote
  # nothing for the archived target.
  assert list(await trigger_mgr.list_triggers(node.id)) == []


@pytest.mark.asyncio
async def test_fire_time_backstop_cancels_with_the_archived_reason(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  bind_deps_managers(monkeypatch, tree, session_mgr)
  node = await create_task(tree, parent=None, request_id="node")
  from conftest import OPERATOR
  trigger_mgr = TriggerManager(cfg, session_mgr)
  trigger = _trigger(node.id)
  await trigger_mgr._save_trigger(trigger)
  # The archive lands mid-wait: the fire-time re-check must cancel, not fire.
  await tree.archive_subtree(node.id, caller=OPERATOR)

  await trigger_mgr._wait_and_fire(trigger)

  fresh = await trigger_mgr._load_trigger(node.id, trigger.id)
  assert fresh is not None
  assert fresh.status.value == "cancelled"
  assert fresh.fire_reason == "target task is archived"
  # No scheduled input was delivered to the archived node.
  assert not [e for e in tree.events.load_events(node.id) if e["type"] == "scheduled_trigger"]


@pytest.mark.asyncio
async def test_watchdog_reason_carries_into_the_cancel(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  bind_deps_managers(monkeypatch, tree, session_mgr)
  node = await create_task(tree, parent=None, request_id="node")
  from conftest import OPERATOR
  await tree.archive_subtree(node.id, caller=OPERATOR)

  trigger_mgr = TriggerManager(cfg, session_mgr)
  # The dormancy judgment is the one predicate both racers read.
  assert await trigger_mgr._dormancy_reason(node.id) == "target task is archived"
  assert await trigger_mgr._is_dormant_target(node.id) is True
  # A session without a profile keeps the legacy chain-end check (its answer
  # is the legacy reason, not the task-node one).
  from src.core.models import CreateSessionRequest
  legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"), backend=None)
  assert await trigger_mgr._dormancy_reason(legacy.id) is None


def test_schedule_trigger_cli_exits_nonzero_and_prints_the_refusal(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """A refused registration leaves the CLI non-zero with the refusal reason on
  stderr (the 422 -> exit 2 mapping rides the real socket)."""
  import threading
  from http.server import HTTPServer

  from src.cli import schedule_trigger as cli
  from tests.test_cli_restart_contract import _QuietHandler

  detail = "task arch-node is archived (target task is archived); trigger rejected"
  received: dict = {}

  class Handler(_QuietHandler):

    def do_POST(self) -> None:
      length = int(self.headers.get("Content-Length", "0"))
      received["body"] = json.loads(self.rfile.read(length)) if length else None
      body = json.dumps({"detail": detail}).encode("utf-8")
      self.send_response(422)
      self.send_header("Content-Type", "application/json")
      self.send_header("Content-Length", str(len(body)))
      self.end_headers()
      self.wfile.write(body)

  httpd = HTTPServer(("127.0.0.1", 0), Handler)
  port = httpd.server_address[1]
  threading.Thread(target=httpd.serve_forever, daemon=True).start()
  try:
    monkeypatch.setattr("src.cli.common._internal_base_url", lambda: f"http://127.0.0.1:{port}")
    from conftest import stub_credentials
    stub_credentials({"charliebot": {"access_key": "op-secret"}})
    monkeypatch.delenv("CHARLIEBOT_SESSION_ID", raising=False)
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(
        sys, "argv", ["charliebot-schedule-trigger", "--session", "arch-node", "--max-wait", "60", "--message", "wake"])

    with pytest.raises(SystemExit) as excinfo:
      cli.main()
    assert excinfo.value.code == cli.EXIT_VERIFY_REJECTED
    payload = json.loads(capsys.readouterr().err)
    assert payload["error"] == detail
    assert received["body"]["session_id"] == "arch-node"
  finally:
    httpd.shutdown()
    httpd.server_close()
