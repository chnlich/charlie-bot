"""Tests for session/requested subagent backend resolution in src.runtime.spawner."""

import pathlib
from unittest import mock

import conftest
import pytest

from src.infra import config, models
from src.runtime import spawner


def _build_cfg(options: list[models.BackendOption]) -> config.CharlieBotConfig:
  return config.CharlieBotConfig(
      charliebot_home=pathlib.Path("/tmp/charliebot-test"),
      backends={"options": options},
  )


def _mock_session_mgr(session: models.SessionMetadata) -> mock.AsyncMock:
  mgr = mock.AsyncMock()
  mgr.get_session.return_value = session
  return mgr


@pytest.mark.asyncio
async def test_session_default_returns_configured_backend() -> None:
  cfg = _build_cfg(
      [
          conftest.backend_option(id="claude-opus-4.7", label="Opus", type="cc-claude", model="claude-opus-4-7"),
      ])
  session = models.SessionMetadata(profile="manager", name="s", backend="claude-opus-4.7")
  mgr = _mock_session_mgr(session)

  backend, model = await spawner.spawner_backends.resolve_requested_subagent_backend_model(
      session.id, cfg, mgr, requested_backend=None)

  assert backend == "claude-opus-4.7"
  assert model == "claude-opus-4-7"


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("options", "session_backend", "requested_backend", "match"),
    [
        pytest.param(
            [
                conftest.backend_option(
                    id="claude-opus-4.7", label="Opus 4.7", type="cc-claude", model="claude-opus-4-7"),
                conftest.CODEX_BACKEND_OPTION,
            ],
            "claude-opus-4.6",
            None,
            "refusing to substitute",
            id="stale-pinned-id",
        ),
        pytest.param([], "claude-opus-4.6", None, "requires a configured backends.options entry", id="no-options"),
        pytest.param(
            [conftest.backend_option(id="claude-opus-4.7", label="Opus", type="cc-claude", model="claude-opus-4-7")],
            "claude-opus-4.7",
            "missing-backend",
            "is not in backends.options",
            id="unknown-typo",
        ),
        pytest.param(
            [conftest.backend_option(id="claude-opus-4.7", label="Opus", type="cc-claude", model="")],
            "claude-opus-4.7",
            None,
            "has no default model",
            id="option-without-model",
        ),
    ],
)
async def test_unresolvable_backend_resolution_raises(
    options: list[models.BackendOption],
    session_backend: str,
    requested_backend: str | None,
    match: str,
) -> None:
  """Every unresolvable backend resolution raises with its own reason and never substitutes:
  a session pinned to an id config no longer defines (e.g. renamed from claude-opus-4.6 to
  claude-opus-4.7; the refusal names backends.options[0] as the substitute it refuses even
  with a second option configured), an empty backends.options, an explicit --backend typo,
  and a selected option whose type needs a model it does not declare."""
  cfg = _build_cfg(options)
  session = models.SessionMetadata(profile="manager", name="s", backend=session_backend)
  mgr = _mock_session_mgr(session)

  with pytest.raises(ValueError, match=match):
    await spawner.spawner_backends.resolve_requested_subagent_backend_model(
        session.id, cfg, mgr, requested_backend=requested_backend)
