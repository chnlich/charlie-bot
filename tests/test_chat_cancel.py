"""Regression tests for master cancel endpoint behavior."""

import pathlib
from collections.abc import AsyncIterator
from typing import Any

import conftest
import fastapi
import pytest

from src.infra import config as core_config
from src.infra import event_types as ET
from src.infra import models
from src.runtime import master_cc_run, runs, task_execution
from src.runtime.agent_process import base
from src.runtime.api import chat


async def _run_cc_with_backend(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    backend: base.AgentBackend,
    session_meta: models.SessionMetadata,
    user_content: str,
) -> tuple[models.SessionCallbacks, tuple[str | None, int, str | None, dict]]:
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={
          "options":
              [conftest.backend_option(id="fake", label="Fake", type="codex", model="o3", prompt_overlay="none"),],
      },
  )
  callbacks = conftest.mock_session_callbacks()

  def fake_build_backend(
      option: models.BackendOption, cfg: core_config.CharlieBotConfig, **kwargs: Any) -> base.AgentBackend:
    return backend

  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, fake_build_backend)

  item = conftest.make_work_item(
      cfg, session_meta, cfg.backends.options[0], user_content=user_content, callbacks=callbacks)
  result = await master_cc_run._run_cc(item)
  return callbacks, result


# ---------------------------------------------------------------------------
# Task-tree nodes: the stop button rides the run store's request_stop
# ---------------------------------------------------------------------------


async def _task_node(tmp_path: pathlib.Path, profile: str = "manager"):
  cfg = conftest.make_home_config(tmp_path)
  session_blocks = conftest.build_session_blocks(cfg)
  tree = conftest.build_task_tree(cfg, session_blocks)
  node = await tree.create_task(
      request_id="node",
      task_parent_id=None,
      profile=profile,
      task=None,
      name="Node",
      backend=None,
      caller=conftest.OPERATOR)
  task_execution.set_task_manager(tree)
  return cfg, session_blocks, tree, node


@pytest.mark.asyncio
async def test_chat_cancel_on_task_node_stops_the_launched_run(tmp_path: pathlib.Path) -> None:
  _cfg, session_blocks, tree, node = await _task_node(tmp_path)
  run = await tree.runs.register_run(models.RunRecord(id="run-live", session_id=node.id, kind="manager_turn"))
  # A launched run whose process is already gone: request_stop converges it to
  # the interrupted terminal fact the way a live process's exit would.
  await tree.runs.record_launch(node.id, run.id, pid=2**23, pid_start="1")

  meta = await session_blocks.store.get_session(node.id)
  assert meta is not None and meta.profile == "manager"
  result = await chat.cancel_master_agent(node.id, _meta=meta, events=session_blocks.events, task_mgr=tree)

  assert result == {"ok": True}
  events = tree.events.load_events(node.id)
  stops = [e for e in events if e["type"] == ET.RUN_STOP_REQUESTED]
  assert stops and stops[0]["run_id"] == "run-live"
  assert stops[0]["request_id"] == "chat-cancel:run-live"
  finished = [e for e in events if e["type"] == ET.RUN_FINISHED and e.get("run_id") == "run-live"]
  assert finished and finished[0]["outcome"] == "interrupted"
  task_execution.set_task_manager(None)


@pytest.mark.asyncio
async def test_chat_cancel_identity_conflict_maps_to_409(tmp_path: pathlib.Path) -> None:
  """A pid reuse (the recorded identity no longer matches /proc) surfaces as
  the v2 run-cancel route's 409 shape, never as a silent miss."""
  import subprocess

  _cfg, session_blocks, tree, node = await _task_node(tmp_path)
  run = await tree.runs.register_run(models.RunRecord(id="run-reused", session_id=node.id, kind="manager_turn"))
  live = subprocess.Popen(["/bin/sleep", "30"])
  try:
    pair = runs.read_pid_stat(live.pid)
    assert pair is not None
    # A recorded pid_start that /proc no longer reports: pid reuse evidence.
    await tree.runs.record_launch(node.id, run.id, pid=live.pid, pid_start="not-this-boot")
    meta = await session_blocks.store.get_session(node.id)
    with pytest.raises(fastapi.HTTPException) as exc_info:
      await chat.cancel_master_agent(node.id, _meta=meta, events=session_blocks.events, task_mgr=tree)
    assert exc_info.value.status_code == 409
  finally:
    if live.poll() is None:
      live.kill()
  # The durable stop request survives (the requesting phase landed first).
  events = tree.events.load_events(node.id)
  assert [e for e in events if e["type"] == ET.RUN_STOP_REQUESTED]
  task_execution.set_task_manager(None)


class _ScriptedBackend(base.AgentBackend):

  def __init__(self, events: list[dict]) -> None:
    super().__init__()
    self._events = events

  def _build_command(self, prompt: str) -> list[str]:
    raise AssertionError("_build_command should not be called")

  async def run(self,
                prompt: str,
                cwd: str,
                env: dict,
                uploaded_files: list[dict] | None = None) -> AsyncIterator[dict]:
    self.exit_code = 0
    self.stderr_text = ""
    for event in self._events:
      yield event


async def _run_cc_with_scripted_events(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    events: list[dict],
) -> models.SessionCallbacks:
  callbacks, _result = await _run_cc_with_backend(
      tmp_path,
      monkeypatch,
      backend=_ScriptedBackend(events),
      session_meta=models.SessionMetadata(profile="manager", id="session-salvage", name="Salvage", backend="fake"),
      user_content="hi",
  )
  return callbacks


def _synthesized_notice_events(callbacks: models.SessionCallbacks) -> list[str]:
  texts = []
  for call in callbacks.persist_and_broadcast.await_args_list:
    event = call.args[1]
    if event.get("type") != ET.ASSISTANT:
      continue
    blocks = event.get("message", {}).get("content", [])
    for block in blocks:
      if isinstance(block, dict) and block.get("type") == "text":
        text = block.get("text", "")
        if text.startswith(master_cc_run.NOTICE):
          texts.append(text)
  return texts


@pytest.mark.asyncio
async def test_silent_turn_salvaged(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  deltas = ["first thinking ", "second thinking ", "third thinking"]
  callbacks = await _run_cc_with_scripted_events(
      tmp_path,
      monkeypatch,
      events=[
          {
              "type": ET.THINKING,
              "content": deltas[0]
          },
          {
              "type": ET.THINKING,
              "content": deltas[1]
          },
          {
              "type": ET.THINKING,
              "content": deltas[2]
          },
          {
              "type": ET.RESULT,
              "usage": {}
          },
      ],
  )
  texts = _synthesized_notice_events(callbacks)
  assert len(texts) == 1
  assert texts[0].startswith(master_cc_run.NOTICE)
  body = texts[0][len(master_cc_run.NOTICE) + len("\n\n"):]
  assert body == "".join(deltas)


@pytest.mark.asyncio
async def test_stream_cut_before_settlement_not_salvaged(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  callbacks = await _run_cc_with_scripted_events(
      tmp_path,
      monkeypatch,
      events=[
          {
              "type": ET.THINKING,
              "content": "thinking but no result"
          },
      ],
  )
  assert not _synthesized_notice_events(callbacks)
