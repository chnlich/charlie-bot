import asyncio
import os
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

from src.agents import master_cc
from src.agents.backends import base as backend_base
from src.core import config as core_config
from src.core import models


def _make_callbacks() -> models.SessionCallbacks:
  return models.SessionCallbacks(
      persist_and_broadcast=AsyncMock(),
      update_thinking_state=AsyncMock(),
      mark_unread=AsyncMock(),
      clear_thinking_since=AsyncMock(),
  )


class _FakeBackend:
  exit_code = 0
  stderr_text = ""

  async def run(self, prompt: str, cwd: str, env: dict):
    yield backend_base.make_result_event()


def test_build_master_env_removes_session_env_and_prepends_repo_venv(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  repo = tmp_path / "repo"
  venv_bin = repo / ".venv" / "bin"
  venv_bin.mkdir(parents=True)
  cfg = SimpleNamespace(charliebot_home=tmp_path / "home", charlie_bot_repo=repo)

  monkeypatch.setenv("PATH", "/usr/bin")
  monkeypatch.setenv("CLAUDECODE", "1")
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "stale-session")

  env = master_cc._build_master_env(cfg, "session-id")

  assert "CHARLIEBOT_SESSION_ID" not in env
  assert env["GIT_CEILING_DIRECTORIES"] == str(tmp_path / "home")
  assert env["CLAUDE_CODE_DISABLE_AUTO_MEMORY"] == "1"
  assert env["PATH"].split(os.pathsep)[:2] == [str(venv_bin), "/usr/bin"]
  assert "CLAUDECODE" not in env


def test_route_resume_session_uses_native_resume_id_for_charlie_code() -> None:
  assert master_cc._route_resume_session("charlie-code", "existing-session-id") == (
      [],
      "existing-session-id",
  )


@pytest.mark.asyncio
async def test_run_cc_does_not_route_claude_resume_flags_to_antigravity(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          models.BackendOption(id="agy", label="Antigravity", type="antigravity"),
      ],
  )
  session_meta = models.SessionMetadata(
      id="session-id",
      name="Antigravity",
      cc_session_id="existing-session-id",
      backend="agy",
  )
  backend_option = cfg.backend_options[0]
  captures: dict[str, object] = {}

  def fake_build_backend(option: models.BackendOption, cfg: core_config.CharlieBotConfig, **kwargs):
    captures["option"] = option
    captures["kwargs"] = kwargs
    return _FakeBackend()

  monkeypatch.setattr("src.agents.backends.registry.build_backend", fake_build_backend)
  monkeypatch.setattr(master_cc, "_build_instructions_content", lambda session_meta, cfg: "instructions")

  item = master_cc._WorkItem(
      cfg=cfg,
      session_meta=session_meta,
      user_content="hello",
      callbacks=_make_callbacks(),
      is_voice=False,
      auto_trigger=False,
      backend_option=backend_option,
      extra_claude_flags=None,
      should_check_tex=False,
      future=asyncio.get_running_loop().create_future(),
  )

  cc_session_id, exit_code, error_msg, _finish_extras = await master_cc._run_cc(item)

  assert captures["option"] is backend_option
  assert backend_option.model is None
  backend_kwargs = captures["kwargs"]
  assert isinstance(backend_kwargs, dict)
  assert backend_kwargs["extra_flags"] is None
  assert backend_kwargs["resume_session_id"] is None
  assert cc_session_id == "existing-session-id"
  assert exit_code == 0
  assert error_msg is None


@pytest.mark.asyncio
async def test_run_cc_adds_exclude_dynamic_flag_for_cc_claude(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          models.BackendOption(id="cc", label="CC", type="cc-claude", model="claude-fable-5"),
      ],
  )
  session_meta = models.SessionMetadata(id="session-id", name="CC", backend="cc")
  backend_option = cfg.backend_options[0]
  captures: dict[str, object] = {}

  def fake_build_backend(option, cfg, **kwargs):
    captures["kwargs"] = kwargs
    return _FakeBackend()

  monkeypatch.setattr("src.agents.backends.registry.build_backend", fake_build_backend)
  monkeypatch.setattr(master_cc, "_build_instructions_content", lambda session_meta, cfg: "instructions")

  item = master_cc._WorkItem(
      cfg=cfg,
      session_meta=session_meta,
      user_content="hello",
      callbacks=_make_callbacks(),
      is_voice=False,
      auto_trigger=False,
      backend_option=backend_option,
      extra_claude_flags=None,
      should_check_tex=False,
      future=asyncio.get_running_loop().create_future(),
  )

  await master_cc._run_cc(item)

  backend_kwargs = captures["kwargs"]
  assert backend_kwargs["extra_flags"] == ["--exclude-dynamic-system-prompt-sections"]
