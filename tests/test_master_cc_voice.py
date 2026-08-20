"""Acceptance tests for the per-message voice-transcription disclaimer."""

from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.agents import master_cc, master_cc_queue, master_cc_run, master_cc_state
from src.core import config as core_config
from src.core import event_types as ET
from src.core import models
from src.core.models import SendMessageRequest

DISCLAIMER = master_cc_run._VOICE_DISCLAIMER


def _make_callbacks() -> models.SessionCallbacks:
  return models.SessionCallbacks(
      persist_and_broadcast=AsyncMock(),
      update_thinking_state=AsyncMock(),
      mark_unread=AsyncMock(),
      persist_cc_session_id=AsyncMock(side_effect=lambda sid, ccid: ccid),
      has_completed_round=AsyncMock(return_value=False),
      persist_master_run=AsyncMock(),
  )


def _make_cfg(tmp_path: Path) -> core_config.CharlieBotConfig:
  return core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[models.BackendOption(id="fake", label="Fake", type="codex")],
  )


class _PromptCapturingBackend:
  exit_code = 0
  stderr_text = ""
  terminated = False

  def __init__(self) -> None:
    self.prompt = None

  async def terminate(self) -> None:
    self.terminated = True

  async def run(self, prompt: str, cwd: str, env: dict):
    self.prompt = prompt
    if False:
      yield {}  # keeps run() an async generator; the consumer's async-for would TypeError on a coroutine


def test_build_prompt_prepends_disclaimer_for_voice() -> None:
  assert master_cc._build_prompt("hello world", True) == DISCLAIMER + "\n" + "hello world"


def test_build_prompt_passes_through_when_not_voice() -> None:
  assert master_cc._build_prompt("hello world", False) == "hello world"


def test_send_message_request_defaults_is_voice_false() -> None:
  req = SendMessageRequest(content="hi")
  assert req.is_voice is False


@pytest.mark.asyncio
async def test_run_cc_hands_disclaimer_prefixed_prompt_to_backend(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _make_cfg(tmp_path)
  meta = models.SessionMetadata(id="voice-cc", name="Voice")
  backend = _PromptCapturingBackend()
  monkeypatch.setattr("src.agents.backends.registry.build_backend", lambda *a, **kw: backend)
  monkeypatch.setattr(master_cc_run, "_build_instructions_content", lambda session_meta, cfg, model=None: "instructions")

  item = master_cc._WorkItem(
      cfg=cfg,
      session_meta=meta,
      user_content="transcribed hello",
      callbacks=_make_callbacks(),
      is_voice=True,
      auto_trigger=False,
      backend_option=cfg.backend_options[0],
      extra_claude_flags=None,
      should_check_tex=False,
      future=asyncio.get_running_loop().create_future(),
  )

  await master_cc._run_cc(item)

  assert backend.prompt == DISCLAIMER + "\n" + "transcribed hello"


@pytest.mark.asyncio
async def test_run_cc_passes_verbatim_prompt_when_not_voice(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _make_cfg(tmp_path)
  meta = models.SessionMetadata(id="plain-cc", name="Plain")
  backend = _PromptCapturingBackend()
  monkeypatch.setattr("src.agents.backends.registry.build_backend", lambda *a, **kw: backend)
  monkeypatch.setattr(master_cc_run, "_build_instructions_content", lambda session_meta, cfg, model=None: "instructions")

  item = master_cc._WorkItem(
      cfg=cfg,
      session_meta=meta,
      user_content="plain hello",
      callbacks=_make_callbacks(),
      is_voice=False,
      auto_trigger=False,
      backend_option=cfg.backend_options[0],
      extra_claude_flags=None,
      should_check_tex=False,
      future=asyncio.get_running_loop().create_future(),
  )

  await master_cc._run_cc(item)

  assert backend.prompt == "plain hello"


async def _run_message_with_capturing_backend(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    user_content: str,
    is_voice_arg: bool,
):
  cfg = _make_cfg(tmp_path)
  meta = models.SessionMetadata(id="voice-msg-session", name="Voice")
  callbacks = _make_callbacks()
  backend = _PromptCapturingBackend()
  monkeypatch.setattr("src.agents.backends.registry.build_backend", lambda *a, **kw: backend)
  monkeypatch.setattr(master_cc_run, "_build_instructions_content", lambda session_meta, cfg, model=None: "instructions")
  master_cc_state._session_queues.pop(meta.id, None)
  master_cc_state._session_consumers.pop(meta.id, None)
  try:
    with (
        patch.object(master_cc_queue.streaming_manager, "broadcast", new=AsyncMock()),
        patch("src.core.sessions.SessionManager") as session_mgr_cls,
    ):
      session_mgr_inst = MagicMock()
      session_mgr_inst._has_running_tasks = AsyncMock(return_value=False)
      session_mgr_cls.return_value = session_mgr_inst
      await master_cc.run_message(cfg, meta, user_content, callbacks, is_voice=is_voice_arg)
      consumer = master_cc_state._session_consumers.get(meta.id)
      if consumer and not consumer.done():
        await asyncio.wait_for(consumer, timeout=5)
  finally:
    master_cc_state._session_queues.pop(meta.id, None)
    master_cc_state._session_consumers.pop(meta.id, None)

  user_events = [
      call.args[1]
      for call in callbacks.persist_and_broadcast.await_args_list
      if len(call.args) > 1 and call.args[1].get("type") == ET.USER
  ]
  return backend, user_events


@pytest.mark.asyncio
async def test_run_message_voice_true_prepends_disclaimer_and_flags_event(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  backend, user_events = await _run_message_with_capturing_backend(
      tmp_path, monkeypatch, user_content="transcribed hello", is_voice_arg=True)

  assert backend.prompt == DISCLAIMER + "\n" + "transcribed hello"
  assert len(user_events) == 1
  assert user_events[0]["is_voice"] is True
  assert user_events[0]["content"] == "transcribed hello"


@pytest.mark.asyncio
async def test_run_message_voice_false_keeps_verbatim_prompt_and_event(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  backend, user_events = await _run_message_with_capturing_backend(
      tmp_path, monkeypatch, user_content="plain hello", is_voice_arg=False)

  assert backend.prompt == "plain hello"
  assert len(user_events) == 1
  assert user_events[0]["is_voice"] is False
  assert user_events[0]["content"] == "plain hello"
