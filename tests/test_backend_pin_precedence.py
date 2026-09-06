"""Run path never adopts a backend the session did not select.

Covers both `backend_option is None` substitution sites (chat.py's message
path, master_trigger.py's wake path) and the run_cc guard's precedence over
`backend_options[0]` for callers (replay) that pass no option at all.
"""

from __future__ import annotations

from collections.abc import Awaitable, Callable
from pathlib import Path

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    OPUS_BACKEND_OPTION,
    FakeBackend,
    build_two_backend_cfg,
    patch_instructions_content,
)

from src.agents import master_cc
from src.api.chat import run_and_finalize
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.master_trigger import trigger_master
from src.core.models import BackendOption, CreateSessionRequest
from src.core.sessions import SessionManager


def _assistant_errors(session_mgr: SessionManager, session_id: str) -> list[dict]:
  events = session_mgr.load_chat_events_sync(session_id)
  return [e for e in events if e["type"] == ET.ASSISTANT_ERROR]


async def _expect_pin_hard_fail(
    session_mgr: SessionManager,
    session_id: str,
    drive: Callable[[], Awaitable[object]],
    monkeypatch: pytest.MonkeyPatch,
    error_substring: str,
) -> None:
  """Run one path's driver under the no-spawn guard and assert the pin hard-failed:
  no backend built, and an assistant error naming the pin written."""
  spawned: list[object] = []
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, lambda *a, **k: spawned.append(1) or FakeBackend())
  patch_instructions_content(monkeypatch)
  await drive()
  assert not spawned
  errors = _assistant_errors(session_mgr, session_id)
  assert errors
  assert any(error_substring in e["content"] for e in errors)


# --------------------------------------------------- unresolvable codex pin


@pytest.mark.asyncio
async def test_message_path_unresolvable_codex_pin_hard_fails(tmp_path: Path, monkeypatch) -> None:
  """No backend_options entry starts with codex — the pin still must not be
  substituted onto anything."""
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[OPUS_BACKEND_OPTION],
  )
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Test Session"), backend="codex-ghost-9")
  await _expect_pin_hard_fail(
      session_mgr,
      session.id,
      lambda: run_and_finalize(cfg, session, "hello", session_mgr),
      monkeypatch,
      error_substring="codex-ghost-9",
  )


@pytest.mark.asyncio
async def test_wake_path_unresolvable_codex_pin_hard_fails(tmp_path: Path, monkeypatch) -> None:
  """Same session/pin as above, driven through the async-wake entry instead
  of the message path — guards delegation merge / improve completion /
  schedule triggers / review wakes, which all funnel through trigger_master."""
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[OPUS_BACKEND_OPTION],
  )
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Test Session"), backend="codex-ghost-9")
  await _expect_pin_hard_fail(
      session_mgr,
      session.id,
      lambda: trigger_master(session.id, "worker summary", cfg, session_mgr),
      monkeypatch,
      error_substring="codex-ghost-9",
  )


@pytest.mark.asyncio
async def test_unresolvable_codex_pin_lands_on_none_of_several_codex_options(tmp_path: Path, monkeypatch) -> None:
  """Discriminates the deleted `next(... type == "codex" ...)` substitution:
  with several codex-type entries configured, an unresolvable codex-prefixed
  pin must still hard-fail rather than land on any of them (the arbitrary
  first match `next()` used to pick)."""
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          OPUS_BACKEND_OPTION,
          BackendOption(id="codex-alpha", label="Codex Alpha", type="codex", model="alpha"),
          BackendOption(id="codex-beta", label="Codex Beta", type="codex", model="beta"),
      ],
  )
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Test Session"), backend="codex-ghost-9")

  spawned_option_ids: list[str] = []
  monkeypatch.setattr(
      BUILD_BACKEND_PATCH_TARGET, lambda option, cfg, **k: spawned_option_ids.append(option.id) or FakeBackend())
  patch_instructions_content(monkeypatch)

  await run_and_finalize(cfg, session, "hello", session_mgr)

  assert not spawned_option_ids
  assert "codex-alpha" not in spawned_option_ids
  assert "codex-beta" not in spawned_option_ids
  errors = _assistant_errors(session_mgr, session.id)
  assert any("codex-ghost-9" in e["content"] for e in errors)


# --------------------------------------------------------------- replay path


@pytest.mark.asyncio
async def test_replay_runs_on_the_sessions_pinned_backend_not_backend_options_zero(tmp_path: Path, monkeypatch) -> None:
  """replay_user_message never passes backend_option — the only caller that
  doesn't. A resolvable pin that isn't backend_options[0] must still win."""
  cfg = build_two_backend_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Test Session"), backend="codex-o3")
  assert session.backend != cfg.backend_options[0].id

  captured: dict[str, object] = {}
  monkeypatch.setattr(
      BUILD_BACKEND_PATCH_TARGET, lambda option, cfg, **k: captured.update(option=option) or FakeBackend())
  patch_instructions_content(monkeypatch)

  user_event = {"id": "u1", "type": "user", "content": "unanswered message"}
  await master_cc.replay_user_message(cfg, session, user_event, session_mgr.callbacks())

  assert captured["option"].id == "codex-o3"


@pytest.mark.asyncio
async def test_replay_unresolvable_pin_hard_fails_not_substituted(tmp_path: Path, monkeypatch) -> None:
  """Replay of an unresolvable pin must reach the hard fail, same as the
  message and wake paths — never a substitution onto backend_options[0]."""
  cfg = build_two_backend_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Test Session"), backend="codex-ghost-9")
  user_event = {"id": "u1", "type": "user", "content": "unanswered message"}
  await _expect_pin_hard_fail(
      session_mgr,
      session.id,
      lambda: master_cc.replay_user_message(cfg, session, user_event, session_mgr.callbacks()),
      monkeypatch,
      error_substring="codex-ghost-9",
  )


# ------------------------------------------------------- factory default


@pytest.mark.asyncio
async def test_empty_pin_no_option_rejects_not_backend_options_zero(tmp_path: Path, monkeypatch) -> None:
  """No pin at all and no explicit option must hard-fail, not fall back to
  backend_options[0] (the wake-path fallback was removed)."""
  cfg = build_two_backend_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Test Session"))
  session.backend = ""
  await _expect_pin_hard_fail(
      session_mgr,
      session.id,
      lambda: run_and_finalize(cfg, session, "hello", session_mgr),
      monkeypatch,
      error_substring="no backend option",
  )
