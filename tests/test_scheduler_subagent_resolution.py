"""Tests for session/requested subagent backend resolution in src.runtime.spawner."""

import datetime
import pathlib
from unittest import mock

import conftest
import pytest

from src.infra import config, models
from src.runtime import backend_pool, spawner, spawner_backends


def _build_cfg(options: list[models.BackendOption]) -> config.CharlieBotConfig:
  return config.CharlieBotConfig(
      charliebot_home=pathlib.Path("/tmp/charliebot-test"),
      backends={"options": options},
  )


def _mock_store(session: models.SessionMetadata) -> mock.AsyncMock:
  store = mock.AsyncMock()
  store.get_session.return_value = session
  return store


@pytest.mark.asyncio
async def test_session_default_returns_configured_backend() -> None:
  cfg = _build_cfg(
      [
          conftest.backend_option(id="claude-opus-4.7", label="Opus", type="cc-claude", model="claude-opus-4-7"),
      ])
  session = models.SessionMetadata(profile="manager", name="s", backend="claude-opus-4.7")
  store = _mock_store(session)

  backend, model, _from_pool = await spawner.spawner_backends.resolve_requested_subagent_backend_model(
      session.id, cfg, store, requested_backend=None, role=models.TaskType.IMPLEMENT)

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
  store = _mock_store(session)

  with pytest.raises(ValueError, match=match):
    await spawner.spawner_backends.resolve_requested_subagent_backend_model(
        session.id, cfg, store, requested_backend=requested_backend, role=models.TaskType.IMPLEMENT)


_SESSION_DEFAULT = (conftest.OPUS_BACKEND_ID, conftest.OPUS_BACKEND_OPTION.model, False)
_POOL_MEMBER = ("codex-o3", "o3", True)


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("role", "pool", "requested_backend", "expected"),
    [
        *(
            pytest.param(role, ["codex-o3"], None, _POOL_MEMBER, id=f"{role}-takes-the-pool-member")
            for role in (models.TaskType.IMPLEMENT, models.TaskType.QUICK_EDIT, models.TaskType.SCRIPT_RUN)),
        pytest.param(
            models.TaskType.IMPLEMENT,
            ["codex-o3"],
            conftest.OPUS_BACKEND_ID,
            (conftest.OPUS_BACKEND_ID, conftest.OPUS_BACKEND_OPTION.model, False),
            id="named-backend-is-never-pooled",
        ),
        pytest.param(models.TaskType.VERIFY, ["codex-o3"], None, _SESSION_DEFAULT, id="verify-keeps-session-backend"),
        pytest.param(
            spawner_backends.IMPROVE_ROLE, ["codex-o3"], None, _SESSION_DEFAULT, id="improve-picks-per-iteration"),
        *(
            pytest.param(role, [], None, _SESSION_DEFAULT, id=f"{role}-inherits-with-an-empty-pool")
            for role in (models.TaskType.IMPLEMENT, spawner_backends.IMPROVE_ROLE)),
    ],
)
async def test_pool_resolution_for_subagent_roles(
    role: spawner_backends.SubagentRole,
    pool: list[str],
    requested_backend: str | None,
    expected: tuple[str, str | None, bool],
) -> None:
  """Worker roles that name no backend take the next backends.pool member; a named backend is never
  pooled, other roles keep the session backend, and an empty pool leaves every worker on it."""
  cfg = config.CharlieBotConfig(
      charliebot_home=pathlib.Path("/tmp/charliebot-test"),
      backends={
          "options": [conftest.OPUS_BACKEND_OPTION, conftest.CODEX_BACKEND_OPTION],
          "pool": pool,
      },
  )
  session = models.SessionMetadata(profile="manager", name="s", backend=conftest.OPUS_BACKEND_ID)

  resolved = await spawner_backends.resolve_requested_subagent_backend_model(
      session.id, cfg, _mock_store(session), requested_backend=requested_backend, role=role)

  assert resolved == expected


@pytest.mark.asyncio
async def test_pool_with_every_member_cooling_reports_each_recovery_time() -> None:
  """With every member cooling, an unpinned worker is refused and the refusal names each member's recovery time."""
  cfg = config.CharlieBotConfig(
      charliebot_home=pathlib.Path("/tmp/charliebot-test"),
      backends={
          "options": [conftest.OPUS_BACKEND_OPTION, conftest.CODEX_BACKEND_OPTION],
          "pool": [conftest.OPUS_BACKEND_ID, "codex-o3"],
      },
  )
  session = models.SessionMetadata(profile="manager", name="s", backend=conftest.OPUS_BACKEND_ID)
  reset = datetime.datetime(2030, 1, 2, 3, 4, tzinfo=datetime.UTC)
  backend_pool.cool(conftest.OPUS_BACKEND_ID, reset)
  backend_pool.cool("codex-o3", reset + datetime.timedelta(hours=1))

  with pytest.raises(ValueError) as exc_info:
    await spawner_backends.resolve_requested_subagent_backend_model(
        session.id, cfg, _mock_store(session), requested_backend=None, role=models.TaskType.IMPLEMENT)

  message = str(exc_info.value)
  assert f"{conftest.OPUS_BACKEND_ID} cooling until 2030-01-02T03:04" in message
  assert "codex-o3 cooling until 2030-01-02T04:04" in message
