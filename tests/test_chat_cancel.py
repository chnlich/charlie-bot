"""Regression tests for master cancel endpoint behavior."""

from unittest.mock import AsyncMock, patch

import pytest
from conftest import make_work_item, mock_session_callbacks, patch_instructions_content
from fastapi import HTTPException

from src.agents import master_cc, master_cc_run
from src.agents.backends.base import AgentBackend, make_text_event
from src.api.chat import cancel_master_agent
from src.core import config as core_config
from src.core import event_types as ET
from src.core import models


class _StderrOnlyBackend(AgentBackend):

  def __init__(self, *, terminate_before_stderr: bool) -> None:
    super().__init__()
    self._terminate_before_stderr = terminate_before_stderr

  def _build_command(self, prompt: str) -> list[str]:
    raise AssertionError("_build_command should not be called")

  async def run(self, prompt: str, cwd: str, env: dict):
    if self._terminate_before_stderr:
      await self.terminate()
    self.exit_code = 1
    self.stderr_text = "claude-sub: terminated"
    if False:
      yield {}  # keeps run() an async generator; the consumer's async-for would TypeError on a coroutine


async def _run_cc_with_stderr_backend(
    tmp_path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    terminate_before_stderr: bool,
):
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          models.BackendOption(id="fake", label="Fake", type="codex", prompt_overlay="none"),
      ],
  )
  session_meta = models.SessionMetadata(
      id=f"session-terminated-{terminate_before_stderr}",
      name="Cancel",
      backend="fake",
  )
  callbacks = mock_session_callbacks()
  backend = _StderrOnlyBackend(terminate_before_stderr=terminate_before_stderr)

  def fake_build_backend(option: models.BackendOption, cfg: core_config.CharlieBotConfig, **kwargs):
    return backend

  monkeypatch.setattr("src.agents.backends.registry.build_backend", fake_build_backend)
  patch_instructions_content(monkeypatch)

  item = make_work_item(cfg, session_meta, cfg.backend_options[0], user_content="stop", callbacks=callbacks)

  result = await master_cc._run_cc(item)
  return callbacks, result


@pytest.mark.asyncio
async def test_cancel_master_agent_success() -> None:
  session_mgr = AsyncMock()
  meta = object()

  with patch("src.api.chat.cancel_master", new=AsyncMock(return_value=True)) as mock_cancel:
    result = await cancel_master_agent("session-ok", meta=meta, session_mgr=session_mgr)

  assert result == {"ok": True}
  mock_cancel.assert_awaited_once_with("session-ok", meta=meta, session_mgr=session_mgr)
  session_mgr.persist_and_broadcast.assert_not_awaited()


@pytest.mark.asyncio
async def test_cancel_master_agent_no_active_master_broadcasts_error() -> None:
  session_mgr = AsyncMock()
  meta = object()

  with patch("src.api.chat.cancel_master", new=AsyncMock(return_value=False)) as mock_cancel:
    with pytest.raises(HTTPException) as exc_info:
      await cancel_master_agent("session-missing", meta=meta, session_mgr=session_mgr)

  assert exc_info.value.status_code == 404
  assert exc_info.value.detail == "No active master agent"
  mock_cancel.assert_awaited_once_with("session-missing", meta=meta, session_mgr=session_mgr)
  session_mgr.persist_and_broadcast.assert_awaited_once_with(
      "session-missing",
      {
          "type": "assistant_error",
          "content": "No active master agent to cancel.",
      },
  )


@pytest.mark.asyncio
async def test_run_cc_suppresses_assistant_error_only_for_user_terminated_stderr(
    tmp_path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  stopped_callbacks, stopped_result = await _run_cc_with_stderr_backend(
      tmp_path,
      monkeypatch,
      terminate_before_stderr=True,
  )

  assert stopped_result[1] == 1
  assert stopped_result[2] is None
  stopped_callbacks.persist_and_broadcast.assert_not_awaited()

  crashed_callbacks, crashed_result = await _run_cc_with_stderr_backend(
      tmp_path,
      monkeypatch,
      terminate_before_stderr=False,
  )

  assert crashed_result[1] == 1
  assert crashed_result[2] == "claude-sub: terminated"
  error_events = [
      call.args[1]
      for call in crashed_callbacks.persist_and_broadcast.await_args_list
      if call.args[1].get("type") == ET.ASSISTANT_ERROR
  ]
  assert error_events == [{
      "type": ET.ASSISTANT_ERROR,
      "content": "Agent error: claude-sub: terminated",
  }]


class _ScriptedBackend(AgentBackend):

  def __init__(self, events: list[dict]) -> None:
    super().__init__()
    self._events = events

  def _build_command(self, prompt: str) -> list[str]:
    raise AssertionError("_build_command should not be called")

  async def run(self, prompt: str, cwd: str, env: dict):
    self.exit_code = 0
    self.stderr_text = ""
    for event in self._events:
      yield event


async def _run_cc_with_scripted_events(
    tmp_path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    events: list[dict],
) -> models.SessionCallbacks:
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          models.BackendOption(id="fake", label="Fake", type="codex", prompt_overlay="none"),
      ],
  )
  session_meta = models.SessionMetadata(id="session-salvage", name="Salvage", backend="fake")
  callbacks = mock_session_callbacks()
  backend = _ScriptedBackend(events)

  def fake_build_backend(option: models.BackendOption, cfg: core_config.CharlieBotConfig, **kwargs):
    return backend

  monkeypatch.setattr("src.agents.backends.registry.build_backend", fake_build_backend)
  patch_instructions_content(monkeypatch)

  item = make_work_item(cfg, session_meta, cfg.backend_options[0], user_content="hi", callbacks=callbacks)

  await master_cc._run_cc(item)
  return callbacks


def _synthesized_notice_events(callbacks) -> list[str]:
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
async def test_silent_turn_salvaged(tmp_path, monkeypatch) -> None:
  deltas = ["first thinking ", "second thinking ", "third thinking"]
  callbacks = await _run_cc_with_scripted_events(
      tmp_path, monkeypatch,
      events=[
          {"type": ET.THINKING, "content": deltas[0]},
          {"type": ET.THINKING, "content": deltas[1]},
          {"type": ET.THINKING, "content": deltas[2]},
          {"type": ET.RESULT, "usage": {}},
      ],
  )
  texts = _synthesized_notice_events(callbacks)
  assert len(texts) == 1
  assert texts[0].startswith(master_cc_run.NOTICE)
  body = texts[0][len(master_cc_run.NOTICE) + len("\n\n"):]
  assert body == "".join(deltas)


@pytest.mark.asyncio
async def test_normal_turn_untouched(tmp_path, monkeypatch) -> None:
  callbacks = await _run_cc_with_scripted_events(
      tmp_path, monkeypatch,
      events=[
          make_text_event("hello"),
          {"type": ET.THINKING, "content": "thinking that was followed by speech"},
          {"type": ET.RESULT, "result": {}},
      ],
  )
  assert not _synthesized_notice_events(callbacks)


@pytest.mark.asyncio
async def test_stream_cut_before_settlement_not_salvaged(tmp_path, monkeypatch) -> None:
  callbacks = await _run_cc_with_scripted_events(
      tmp_path, monkeypatch,
      events=[
          {"type": ET.THINKING, "content": "thinking but no result"},
      ],
  )
  assert not _synthesized_notice_events(callbacks)


@pytest.mark.asyncio
async def test_claude_family_thinking_block_salvaged(tmp_path, monkeypatch) -> None:
  thinking = "claude-family buried reasoning"
  callbacks = await _run_cc_with_scripted_events(
      tmp_path, monkeypatch,
      events=[
          {
              "type": ET.ASSISTANT,
              "message": {"content": [{"type": "thinking", "thinking": thinking}]},
          },
          {"type": ET.RESULT, "result": {}},
      ],
  )
  texts = _synthesized_notice_events(callbacks)
  assert len(texts) == 1
  assert texts[0].endswith(thinking)
  assert thinking in texts[0][len(master_cc_run.NOTICE) + len("\n\n"):]


@pytest.mark.asyncio
async def test_salvage_helper_emits_thinking() -> None:
  tracker = master_cc._RunTimingTracker("session-helper", "codex", None)
  tracker.on_event({"type": ET.THINKING, "content": "thinking part one"})
  tracker.on_event({"type": ET.THINKING, "content": " thinking part two"})
  tracker.on_event({"type": ET.RESULT, "result": {}})
  broadcast = AsyncMock()
  await master_cc._salvage_silent_turn(tracker, None, "session-helper", broadcast)
  broadcast.assert_awaited_once()
  event = broadcast.await_args.args[1]
  assert event.get("type") == ET.ASSISTANT
  text = event["message"]["content"][0]["text"]
  assert text.startswith(master_cc_run.NOTICE)
  assert text.endswith("thinking part one thinking part two")


@pytest.mark.asyncio
async def test_salvage_helper_suppressed_by_error() -> None:
  tracker = master_cc._RunTimingTracker("session-helper", "codex", None)
  tracker.on_event({"type": ET.THINKING, "content": "thinking"})
  tracker.on_event({"type": ET.RESULT, "result": {}})
  broadcast = AsyncMock()
  await master_cc._salvage_silent_turn(tracker, "boom", "session-helper", broadcast)
  broadcast.assert_not_awaited()


def test_both_teardowns_call_salvage_helper() -> None:
  import inspect
  run_src = inspect.getsource(master_cc._run_cc)
  resume_src = inspect.getsource(master_cc._resume_cc)
  assert "_salvage_silent_turn(" in run_src
  assert "_salvage_silent_turn(" in resume_src
