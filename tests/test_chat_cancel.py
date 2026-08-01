"""Regression tests for master cancel endpoint behavior."""

import asyncio
from unittest.mock import AsyncMock, patch

import pytest
from fastapi import HTTPException

from src.agents import master_cc
from src.agents.backends.base import AgentBackend
from src.api.chat import cancel_master_agent
from src.core import config as core_config
from src.core import event_types as ET
from src.core import models


def _make_callbacks() -> models.SessionCallbacks:
  return models.SessionCallbacks(
      persist_and_broadcast=AsyncMock(),
      update_thinking_state=AsyncMock(),
      mark_unread=AsyncMock(),
      persist_cc_session_id=AsyncMock(side_effect=lambda sid, ccid: ccid),
      has_completed_round=AsyncMock(return_value=False),
      persist_master_run=AsyncMock(),
  )


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
      yield {}


async def _run_cc_with_stderr_backend(
    tmp_path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    terminate_before_stderr: bool,
):
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          models.BackendOption(id="fake", label="Fake", type="codex"),
      ],
  )
  session_meta = models.SessionMetadata(
      id=f"session-terminated-{terminate_before_stderr}",
      name="Cancel",
      backend="fake",
  )
  callbacks = _make_callbacks()
  backend = _StderrOnlyBackend(terminate_before_stderr=terminate_before_stderr)

  def fake_build_backend(option: models.BackendOption, cfg: core_config.CharlieBotConfig, **kwargs):
    return backend

  monkeypatch.setattr("src.agents.backends.registry.build_backend", fake_build_backend)
  monkeypatch.setattr(master_cc, "_build_instructions_content", lambda session_meta, cfg: "instructions")

  item = master_cc._WorkItem(
      cfg=cfg,
      session_meta=session_meta,
      user_content="stop",
      callbacks=callbacks,
      is_voice=False,
      auto_trigger=False,
      backend_option=cfg.backend_options[0],
      extra_claude_flags=None,
      should_check_tex=False,
      future=asyncio.get_running_loop().create_future(),
  )

  result = await master_cc._run_cc(item)
  return callbacks, result


@pytest.mark.asyncio
async def test_cancel_master_agent_success() -> None:
  session_mgr = AsyncMock()

  with patch("src.api.chat.cancel_master", new=AsyncMock(return_value=True)) as mock_cancel:
    result = await cancel_master_agent("session-ok", _meta=object(), session_mgr=session_mgr)

  assert result == {"ok": True}
  mock_cancel.assert_awaited_once_with("session-ok")
  session_mgr.persist_and_broadcast.assert_not_awaited()


@pytest.mark.asyncio
async def test_cancel_master_agent_no_active_master_broadcasts_error() -> None:
  session_mgr = AsyncMock()

  with patch("src.api.chat.cancel_master", new=AsyncMock(return_value=False)) as mock_cancel:
    with pytest.raises(HTTPException) as exc_info:
      await cancel_master_agent("session-missing", _meta=object(), session_mgr=session_mgr)

  assert exc_info.value.status_code == 404
  assert exc_info.value.detail == "No active master agent"
  mock_cancel.assert_awaited_once_with("session-missing")
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
