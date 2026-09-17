"""Tests for session/requested subagent backend resolution in src.core.spawner."""

from pathlib import Path
from unittest.mock import AsyncMock

import pytest
from conftest import CODEX_BACKEND_OPTION, backend_option

from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, SessionMetadata
from src.core.spawner import resolve_requested_subagent_backend_model


def _build_cfg(options: list[BackendOption]) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      backends={"options": options},
  )


def _mock_session_mgr(session: SessionMetadata) -> AsyncMock:
  mgr = AsyncMock()
  mgr.get_session.return_value = session
  return mgr


@pytest.mark.asyncio
async def test_session_default_returns_configured_backend() -> None:
  cfg = _build_cfg([
      backend_option(id="claude-opus-4.7", label="Opus", type="cc-claude", model="claude-opus-4-7"),
  ])
  session = SessionMetadata(name="s", backend="claude-opus-4.7")
  mgr = _mock_session_mgr(session)

  backend, model = await resolve_requested_subagent_backend_model(session.id, cfg, mgr, requested_backend=None)

  assert backend == "claude-opus-4.7"
  assert model == "claude-opus-4-7"


@pytest.mark.asyncio
async def test_session_default_uses_first_option_when_no_backend_pinned() -> None:
  """An empty session backend is the documented default, not a substitution."""
  cfg = _build_cfg(
      [
          backend_option(id="claude-opus-4.7", label="Opus 4.7", type="cc-claude", model="claude-opus-4-7"),
      ])
  session = SessionMetadata(name="s", backend="")
  mgr = _mock_session_mgr(session)

  backend, model = await resolve_requested_subagent_backend_model(session.id, cfg, mgr, requested_backend=None)

  assert backend == "claude-opus-4.7"
  assert model == "claude-opus-4-7"


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("options", "session_backend", "requested_backend", "match"),
    [
        pytest.param(
            [
                backend_option(id="claude-opus-4.7", label="Opus 4.7", type="cc-claude", model="claude-opus-4-7"),
                CODEX_BACKEND_OPTION,
            ],
            "claude-opus-4.6",
            None,
            "refusing to substitute",
            id="stale-pinned-id",
        ),
        pytest.param([], "claude-opus-4.6", None, "requires a configured backends.options entry", id="no-options"),
        pytest.param(
            [backend_option(id="claude-opus-4.7", label="Opus", type="cc-claude", model="claude-opus-4-7")],
            "claude-opus-4.7",
            "missing-backend",
            "is not in backends.options",
            id="unknown-typo",
        ),
        pytest.param(
            [backend_option(id="claude-opus-4.7", label="Opus", type="cc-claude", model="")],
            "claude-opus-4.7",
            None,
            "has no default model",
            id="option-without-model",
        ),
    ],
)
async def test_unresolvable_backend_resolution_raises(
    options: list[BackendOption],
    session_backend: str,
    requested_backend: str | None,
    match: str,
) -> None:
  """Every unresolvable backend resolution raises with its own reason and never substitutes:
  a session pinned to an id config no longer defines (e.g. renamed from claude-opus-4.6 to
  claude-opus-4.7; the second configured option is the substitute being refused), an empty
  backends.options, an explicit --backend typo, and a selected option whose type needs a
  model it does not declare."""
  cfg = _build_cfg(options)
  session = SessionMetadata(name="s", backend=session_backend)
  mgr = _mock_session_mgr(session)

  with pytest.raises(ValueError, match=match):
    await resolve_requested_subagent_backend_model(session.id, cfg, mgr, requested_backend=requested_backend)
