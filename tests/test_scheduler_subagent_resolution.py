from pathlib import Path

import pytest
from pydantic import ValidationError

from src.core.config import CharlieBotConfig, ScheduledTaskConfig
from src.core.models import BackendOption, SessionMetadata
from src.core.scheduler import Scheduler


def _build_cfg(options: list[BackendOption]) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      backend_options=options,
  )


def test_subagent_schema_rejects_extra_fallback_fields() -> None:
  with pytest.raises(ValidationError):
    ScheduledTaskConfig(
        name="daily",
        cron="0 * * * *",
        prompt="run checks",
        subagent={"backend": "codex-o3", "model": "o3", "fallback_model": "o4-mini"},
    )


def test_subagent_schema_rejects_blank_values() -> None:
  with pytest.raises(ValidationError):
    ScheduledTaskConfig(
        name="daily",
        cron="0 * * * *",
        prompt="run checks",
        subagent={"backend": "codex-o3", "model": "   "},
    )


def test_scheduler_resolution_prefers_job_override() -> None:
  cfg = _build_cfg([
      BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
      BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
  ])
  scheduler = Scheduler(cfg)
  session = SessionMetadata(name="Scheduled: daily", backend="claude-opus-4.6")
  task = ScheduledTaskConfig(
      name="daily",
      cron="0 * * * *",
      prompt="run checks",
      subagent={"backend": "codex-o3", "model": "o3-pro"},
  )

  backend, model, source = scheduler._resolve_subagent_backend_model(task, session, cfg)

  assert backend == "codex-o3"
  assert model == "o3-pro"
  assert source == "task_override"


def test_scheduler_resolution_falls_back_to_session_default() -> None:
  cfg = _build_cfg([
      BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
  ])
  scheduler = Scheduler(cfg)
  session = SessionMetadata(name="Scheduled: daily", backend="claude-opus-4.6")
  task = ScheduledTaskConfig(name="daily", cron="0 * * * *", prompt="run checks")

  backend, model, source = scheduler._resolve_subagent_backend_model(task, session, cfg)

  assert backend == "claude-opus-4.6"
  assert model == "claude-opus-4-6"
  assert source == "session_default"


def test_scheduler_resolution_raises_for_unknown_backend() -> None:
  cfg = _build_cfg([
      BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
  ])
  scheduler = Scheduler(cfg)
  session = SessionMetadata(name="Scheduled: daily", backend="missing-backend")
  task = ScheduledTaskConfig(name="daily", cron="0 * * * *", prompt="run checks")

  with pytest.raises(ValueError, match="is not in backend_options"):
    scheduler._resolve_subagent_backend_model(task, session, cfg)


def test_scheduler_resolution_raises_for_missing_default_model() -> None:
  cfg = _build_cfg([
      BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model=None),
  ])
  scheduler = Scheduler(cfg)
  session = SessionMetadata(name="Scheduled: daily", backend="claude-opus-4.6")
  task = ScheduledTaskConfig(name="daily", cron="0 * * * *", prompt="run checks")

  with pytest.raises(ValueError, match="has no default model"):
    scheduler._resolve_subagent_backend_model(task, session, cfg)
