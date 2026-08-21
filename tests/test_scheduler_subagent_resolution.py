"""Tests for session/requested subagent backend resolution in src.core.spawner."""

from pathlib import Path
from unittest.mock import AsyncMock

import pytest

from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, SessionMetadata
from src.core.spawner import resolve_requested_subagent_backend_model


def _build_cfg(options: list[BackendOption]) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      backend_options=options,
  )


def _mock_session_mgr(session: SessionMetadata) -> AsyncMock:
  mgr = AsyncMock()
  mgr.get_session.return_value = session
  return mgr


@pytest.mark.asyncio
async def test_session_default_returns_configured_backend() -> None:
  cfg = _build_cfg([
      BackendOption(id="claude-opus-4.7", label="Opus", type="cc-claude", model="claude-opus-4-7"),
  ])
  session = SessionMetadata(name="s", backend="claude-opus-4.7")
  mgr = _mock_session_mgr(session)

  backend, model = await resolve_requested_subagent_backend_model(session.id, cfg, mgr, requested_backend=None)

  assert backend == "claude-opus-4.7"
  assert model == "claude-opus-4-7"


@pytest.mark.asyncio
async def test_session_default_raises_for_stale_backend() -> None:
  """A session pinned to an id config no longer defines must fail loudly, never substitute."""
  cfg = _build_cfg([
      BackendOption(id="claude-opus-4.7", label="Opus 4.7", type="cc-claude", model="claude-opus-4-7"),
      BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
  ])
  # Stored id no longer exists (e.g. renamed from claude-opus-4.6 to claude-opus-4.7).
  session = SessionMetadata(name="s", backend="claude-opus-4.6")
  mgr = _mock_session_mgr(session)

  with pytest.raises(ValueError, match="refusing to substitute"):
    await resolve_requested_subagent_backend_model(session.id, cfg, mgr, requested_backend=None)


@pytest.mark.asyncio
async def test_session_default_uses_first_option_when_no_backend_pinned() -> None:
  """An empty session backend is the documented default, not a substitution."""
  cfg = _build_cfg([
      BackendOption(id="claude-opus-4.7", label="Opus 4.7", type="cc-claude", model="claude-opus-4-7"),
  ])
  session = SessionMetadata(name="s", backend="")
  mgr = _mock_session_mgr(session)

  backend, model = await resolve_requested_subagent_backend_model(session.id, cfg, mgr, requested_backend=None)

  assert backend == "claude-opus-4.7"
  assert model == "claude-opus-4-7"


@pytest.mark.asyncio
async def test_session_default_raises_when_no_backend_options() -> None:
  cfg = _build_cfg([])
  session = SessionMetadata(name="s", backend="claude-opus-4.6")
  mgr = _mock_session_mgr(session)

  with pytest.raises(ValueError, match="configured backend_options entry"):
    await resolve_requested_subagent_backend_model(session.id, cfg, mgr, requested_backend=None)


@pytest.mark.asyncio
async def test_requested_backend_raises_for_unknown_typo() -> None:
  """Explicit --backend typos must still fail fast."""
  cfg = _build_cfg([
      BackendOption(id="claude-opus-4.7", label="Opus", type="cc-claude", model="claude-opus-4-7"),
  ])
  session = SessionMetadata(name="s", backend="claude-opus-4.7")
  mgr = _mock_session_mgr(session)

  with pytest.raises(ValueError, match="is not in backend_options"):
    await resolve_requested_subagent_backend_model(session.id, cfg, mgr, requested_backend="missing-backend")


@pytest.mark.asyncio
async def test_requested_backend_none_raises_for_stale_session_backend() -> None:
  """With no --backend passed, a stale session backend must fail loudly rather than substitute."""
  cfg = _build_cfg([
      BackendOption(id="claude-opus-4.7", label="Opus", type="cc-claude", model="claude-opus-4-7"),
  ])
  session = SessionMetadata(name="s", backend="claude-opus-4.6")
  mgr = _mock_session_mgr(session)

  with pytest.raises(ValueError, match="refusing to substitute"):
    await resolve_requested_subagent_backend_model(session.id, cfg, mgr, requested_backend=None)


@pytest.mark.asyncio
async def test_session_default_raises_when_option_has_no_model() -> None:
  cfg = _build_cfg([
      BackendOption(id="claude-opus-4.7", label="Opus", type="cc-claude", model=None),
  ])
  session = SessionMetadata(name="s", backend="claude-opus-4.7")
  mgr = _mock_session_mgr(session)

  with pytest.raises(ValueError, match="has no default model"):
    await resolve_requested_subagent_backend_model(session.id, cfg, mgr, requested_backend=None)
