"""Acceptance tests for the per-message voice-transcription disclaimer."""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    CapturingBackend,
    build_master_cc_cfg,
    make_work_item,
    patch_instructions_content,
    run_captured_round,
)

from src.agents import master_cc, master_cc_run
from src.core import event_types as ET
from src.core import models
from src.core.models import SendMessageRequest

DISCLAIMER = master_cc_run._VOICE_DISCLAIMER


def _user_events(callbacks) -> list[dict]:
  """The USER events the round persisted, in order, from the mocked callbacks."""
  return [
      call.args[1]
      for call in callbacks.persist_and_broadcast.await_args_list
      if len(call.args) > 1 and call.args[1].get("type") == ET.USER
  ]


def test_build_prompt_prepends_disclaimer_for_voice() -> None:
  assert master_cc._build_prompt("hello world", True) == DISCLAIMER + "\n" + "hello world"


def test_build_prompt_passes_through_when_not_voice() -> None:
  assert master_cc._build_prompt("hello world", False) == "hello world"


def test_send_message_request_defaults_is_voice_false() -> None:
  req = SendMessageRequest(content="hi")
  assert req.is_voice is False


@pytest.mark.asyncio
@pytest.mark.parametrize("is_voice", [True, False], ids=["voice", "plain"])
async def test_run_cc_prompt_follows_is_voice(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    is_voice: bool,
) -> None:
  cfg = build_master_cc_cfg(tmp_path)
  meta = models.SessionMetadata(id="voice-cc", name="Voice")
  backend = CapturingBackend()
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, lambda *a, **kw: backend)
  patch_instructions_content(monkeypatch)

  text = "transcribed hello" if is_voice else "plain hello"
  item = make_work_item(cfg, meta, cfg.backends.options[0], user_content=text, is_voice=is_voice)

  await master_cc._run_cc(item)

  assert backend.calls[0]["prompt"] == ((DISCLAIMER + "\n" + text) if is_voice else text)


@pytest.mark.asyncio
@pytest.mark.parametrize("is_voice", [True, False], ids=["voice", "plain"])
async def test_run_message_prompt_and_user_event_follow_is_voice(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    is_voice: bool,
) -> None:
  text = "transcribed hello" if is_voice else "plain hello"
  backend = CapturingBackend()

  async def drive(cfg, meta, callbacks):
    await master_cc.run_message(cfg, meta, text, callbacks, is_voice=is_voice)

  callbacks = await run_captured_round(
      tmp_path, monkeypatch, session_id="voice-msg-session", name="Voice", backend=backend, drive=drive)

  assert backend.calls[0]["prompt"] == ((DISCLAIMER + "\n" + text) if is_voice else text)
  user_events = _user_events(callbacks)
  assert len(user_events) == 1
  assert user_events[0]["is_voice"] is is_voice
  assert user_events[0]["content"] == text
