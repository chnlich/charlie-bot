"""Scheduled triggers against an archived task node: creation refuses, the
fire-time backstop cancels with the named reason, and the CLI exits non-zero
printing the refusal reason. Real managers drive the tree; the CLI test rides a
real socket."""

from __future__ import annotations

import datetime
import json
import pathlib
import sys

import conftest
import pytest

from src.infra import models
from src.runtime import triggers


def _trigger(session_id: str, trigger_id: str = "trig-archived-1") -> models.PendingTrigger:
  return models.PendingTrigger(
      id=trigger_id, session_id=session_id, fire_at=datetime.datetime.now(datetime.UTC), message="wake")


@pytest.mark.asyncio
async def test_create_trigger_on_an_archived_task_node_is_refused(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  conftest.bind_deps_blocks(monkeypatch, tree, session_blocks)
  node = await conftest.create_task(tree, parent=None, request_id="node")
  await tree.completion.archive_subtree(
      node.id,
      request_id="archive-test",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR)

  trigger_mgr = triggers.TriggerManager(cfg, tree)
  with pytest.raises(triggers.ArchivedSessionError, match="target task is archived"):
    await trigger_mgr.create_trigger(node.id, 60, "check the run")

  # A pending trigger on the still-open node is untouched; the refusal wrote
  # nothing for the archived target.
  assert list(await trigger_mgr.list_triggers(node.id)) == []


@pytest.mark.asyncio
async def test_fire_time_backstop_cancels_with_the_archived_reason(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  conftest.bind_deps_blocks(monkeypatch, tree, session_blocks)
  node = await conftest.create_task(tree, parent=None, request_id="node")
  trigger_mgr = triggers.TriggerManager(cfg, tree)
  trigger = _trigger(node.id)
  await trigger_mgr._save_trigger(trigger)
  # The archive lands mid-wait: the fire-time re-check must cancel, not fire.
  await tree.completion.archive_subtree(
      node.id,
      request_id="archive-test",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR)

  await trigger_mgr._wait_and_fire(trigger)

  fresh = await trigger_mgr._load_trigger(node.id, trigger.id)
  assert fresh is not None
  assert fresh.status.value == "cancelled"
  assert fresh.fire_reason == "target task is archived"
  # No scheduled input was delivered to the archived node.
  assert not [e for e in tree.events.load_events(node.id) if e["type"] == "scheduled_trigger"]


@pytest.mark.asyncio
async def test_watchdog_reason_carries_into_the_cancel(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  conftest.bind_deps_blocks(monkeypatch, tree, session_blocks)
  node = await conftest.create_task(tree, parent=None, request_id="node")
  await tree.completion.archive_subtree(
      node.id,
      request_id="archive-test",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR)

  trigger_mgr = triggers.TriggerManager(cfg, tree)
  # The dormancy judgment is the one predicate both racers read.
  assert await trigger_mgr._dormancy_reason(node.id) == "target task is archived"
  # An open manager root remains eligible for trigger registration.
  open_root = await conftest.create_root_session(
      session_blocks, models.CreateSessionRequest(name="Open root"), backend=None)
  assert open_root.profile == "manager"
  assert await trigger_mgr._dormancy_reason(open_root.id) is None


def test_schedule_trigger_cli_exits_nonzero_and_prints_the_refusal(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """A refused registration leaves the CLI non-zero with the refusal reason on
  stderr (the 422 -> exit 2 mapping rides the real socket)."""
  import threading
  from http import server

  from src.runtime.cli import schedule_trigger as cli
  from tests import test_cli_restart_contract

  detail = "task arch-node is archived (target task is archived); trigger rejected"
  received: dict = {}

  class Handler(test_cli_restart_contract._QuietHandler):

    def do_POST(self) -> None:
      length = int(self.headers.get("Content-Length", "0"))
      received["body"] = json.loads(self.rfile.read(length)) if length else None
      body = json.dumps({"detail": detail}).encode("utf-8")
      self.send_response(422)
      self.send_header("Content-Type", "application/json")
      self.send_header("Content-Length", str(len(body)))
      self.end_headers()
      self.wfile.write(body)

  httpd = server.HTTPServer(("127.0.0.1", 0), Handler)
  port = httpd.server_address[1]
  threading.Thread(target=httpd.serve_forever, daemon=True).start()
  try:
    monkeypatch.setattr(conftest.CLI_COMMON_BASE_URL_PATCH_TARGET, lambda: f"http://127.0.0.1:{port}")
    conftest.stub_credentials({"charliebot": {"access_key": "op-secret"}})
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
