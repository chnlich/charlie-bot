"""Tests for the elone succession pointer and successor-chain resolution."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
import yaml
from conftest import (
    OPUS_BACKEND_ID,
    build_two_backend_cfg,
    user_event,
)
from conftest import append_events as _append_events
from conftest import make_parent as _make_parent
from conftest import session_dir_names as _session_dir_names

from src.core.config import (
    CharlieBotConfig,
)
from src.core.models import (
    CreateSessionRequest,
    SessionMetadata,
    SessionStatus,
)
from src.core.sessions import (
    SessionManager,
    SuccessionRefusedError,
)


def _seed_scheduled_task(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    cron: str = "0 2 * * *",
) -> Path:
  """Seed a prompt_file-backed host cron file and point the core backend-write helper at it.

  Shaped like a production host file (path to prompt source under
  ``prompt_file``), and resolvable through the production loader, so a
  scheduler tick can rebuild the task config from what the write-back left on
  disk.
  """
  prompt = tmp_path / "prompts" / "nightly.md"
  prompt.parent.mkdir(parents=True, exist_ok=True)
  prompt.write_text("run nightly", encoding="utf-8")
  cron_d = tmp_path / "cron.d"
  cron_d.mkdir(parents=True, exist_ok=True)
  path = cron_d / "nightly.yaml"
  path.write_text(
      yaml.safe_dump(
          {
              "cron": cron,
              "prompt_file": str(prompt),
              "timezone": "America/Los_Angeles",
              "backend": OPUS_BACKEND_ID,
          }),
      encoding="utf-8")
  monkeypatch.setattr("src.core.scheduled_sessions.cron_path", lambda name: cron_d / f"{name}.yaml")
  return path


def _scheduled_succession_rig(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    cron: str = "0 2 * * *",
) -> tuple[CharlieBotConfig, SessionManager, Path]:
  """The scheduler-owned succession rig: two-backend config, its manager, and the seeded task yaml path."""
  cfg = build_two_backend_cfg(tmp_path)
  mgr = SessionManager(cfg)
  return cfg, mgr, _seed_scheduled_task(tmp_path, monkeypatch, cron=cron)


async def _make_scheduled_parent(
    mgr: SessionManager,
    *,
    group: str | None = None,
    events: int = 3,
) -> SessionMetadata:
  """Create the active scheduled session for the nightly task, with *events* chat events."""
  parent = await mgr.create_session(
      CreateSessionRequest(name="Scheduled: nightly", scheduled_task="nightly"),
      backend=OPUS_BACKEND_ID,
  )
  if group is not None:
    parent.group = group
    await mgr.save_metadata(parent)
  _append_events(
      mgr.get_chat_events_path(parent.id),
      [user_event(f"e{i}") for i in range(events)],
  )
  return parent


@pytest.mark.asyncio
async def test_elone_writes_successor_pointer_and_archives_parent(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent_id = await _make_parent(mgr)

  child = await mgr.elone_session(parent_id, event_index=0)

  fresh_parent = await mgr.read_metadata_fresh(parent_id)
  assert fresh_parent is not None
  assert fresh_parent.successor_session_id == child.id
  assert fresh_parent.status == SessionStatus.ARCHIVED
  # Session-level rating is gone: the persisted metadata carries no rating key.
  assert "rating" not in json.loads(mgr._metadata_path(parent_id).read_text())


@pytest.mark.asyncio
async def test_second_elone_of_scheduler_owned_parent_refuses_and_mutates_nothing(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg, mgr, _ = _scheduled_succession_rig(tmp_path, monkeypatch)
  parent = await _make_scheduled_parent(mgr)

  first_child = await mgr.elone_session(parent.id, event_index=1, backend="codex-o3")
  before = _session_dir_names(cfg)

  with pytest.raises(SuccessionRefusedError):
    await mgr.elone_session(parent.id, event_index=1, backend="codex-o3")

  # No new session directory appears.
  assert _session_dir_names(cfg) == before

  # The parent's metadata is unchanged: successor still the first child,
  # archived, thumbs_down.
  fresh_parent = await mgr.read_metadata_fresh(parent.id)
  assert fresh_parent is not None
  assert fresh_parent.successor_session_id == first_child.id
  assert fresh_parent.status == SessionStatus.ARCHIVED


@pytest.mark.asyncio
async def test_resolve_successor_chain_walks_across_three_generations(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  gen0 = await _make_parent(mgr, name="G0")

  gen1 = await mgr.elone_session(gen0, event_index=0)
  gen2 = await mgr.elone_session(gen1.id, event_index=0)
  gen3 = await mgr.elone_session(gen2.id, event_index=0)

  resolved = await mgr.resolve_successor_chain(gen0)
  assert resolved is not None
  assert resolved.id == gen3.id


# ---------------------------------------------------------------------------
# Scheduler-owned elone: inheriting succession
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_failed_write_back_rolls_back_the_succession(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  _cfg, mgr, yaml_path = _scheduled_succession_rig(tmp_path, monkeypatch)
  parent = await _make_scheduled_parent(mgr)
  original_yaml = yaml_path.read_text(encoding="utf-8")

  write_failure = OSError("forced yaml write failure")

  def boom(path: Path, data: dict, **_kwargs: object) -> None:
    raise write_failure

  monkeypatch.setattr("src.core.scheduled_sessions.save_yaml", boom)

  with pytest.raises(OSError) as excinfo:
    await mgr.elone_session(parent.id, event_index=1, backend="codex-o3")
  assert excinfo.value is write_failure

  # The parent is untouched: active, no successor pointer.
  fresh_parent = await mgr.read_metadata_fresh(parent.id)
  assert fresh_parent is not None
  assert fresh_parent.status == SessionStatus.ACTIVE
  assert fresh_parent.successor_session_id is None
  # No new scheduled session remains registered; the brief successor is archived.
  active = await mgr.list_sessions(status=SessionStatus.ACTIVE, scheduled=True)
  assert [s.id for s in active] == [parent.id]
  assert all(s.id == parent.id or s.status == SessionStatus.ARCHIVED for s in await mgr.list_sessions(scheduled=True))
  # ...and the task yaml was never rewritten.
  assert yaml_path.read_text(encoding="utf-8") == original_yaml
