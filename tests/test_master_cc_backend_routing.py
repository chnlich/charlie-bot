import asyncio
from pathlib import Path
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


@pytest.mark.asyncio
async def test_run_cc_does_not_route_claude_resume_flags_to_antigravity(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          models.BackendOption(id="agy-gemini", label="Antigravity", type="antigravity", model="gemini-test-model"),
      ],
  )
  session_meta = models.SessionMetadata(
      id="session-id",
      name="Antigravity",
      cc_session_id="existing-session-id",
      backend="agy-gemini",
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
  backend_kwargs = captures["kwargs"]
  assert isinstance(backend_kwargs, dict)
  assert backend_kwargs["extra_flags"] is None
  assert backend_kwargs["resume_session_id"] is None
  assert cc_session_id == "existing-session-id"
  assert exit_code == 0
  assert error_msg is None
