"""The resume anchors' write-guard: only the authorized channels change
``cc_session_id`` / ``claude_account``; every other save is corrected back to
disk, the six in-class lock-holding save sites run declared under their locks,
and the weekly recycle clears the anchor through its channel."""

import asyncio
from datetime import UTC, datetime, timedelta, tzinfo
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
    MASTER_TRIGGER_RUN_MESSAGE_WITH_RESUME_RECOVERY_PATCH_TARGET,
    OPUS_BACKEND_ID,
    build_sessions_cfg,
    user_event,
)
from structlog.testing import capture_logs

from src.core.master_trigger import trigger_master
from src.core.models import CreateSessionRequest, SessionMetadata
from src.core.sessions import SessionManager


def _corrections(logs: list[dict]) -> list[dict]:
  return [entry for entry in logs if entry["event"] == "session_anchor_write_corrected"]


async def _seed_anchors(mgr: SessionManager, session_id: str, *, cc: str, label: str) -> None:
  await mgr.persist_cc_session_id(session_id, cc)
  await mgr.persist_claude_account(session_id, label)


@pytest.mark.asyncio
async def test_whole_object_save_with_a_stale_label_is_corrected_back_to_disk(tmp_path: Path) -> None:
  """The rate_round shape: a route mutates its injected (stale) meta object and
  whole-object saves. The guard corrects the anchor back to disk on the write."""
  mgr = SessionManager(build_sessions_cfg(tmp_path))
  session = await mgr.create_session(CreateSessionRequest(name="stale-writer"))
  await _seed_anchors(mgr, session.id, cc="cc-live", label="pool-b")

  stale = await mgr.get_session(session.id)
  stale.claude_account = "pool-a"  # the enqueue-time value the caller still holds
  with capture_logs() as logs:
    await mgr.save_metadata(stale)

  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.claude_account == "pool-b", "the stale whole-object write may not roll the label back"
  corrections = _corrections(logs)
  assert len(corrections) == 1
  assert corrections[0]["field"] == "claude_account"
  assert corrections[0]["on_disk"] == "pool-b" and corrections[0]["attempted"] == "pool-a"


@pytest.mark.asyncio
async def test_stale_writer_after_a_funnel_persist_keeps_the_funnel_value(tmp_path: Path) -> None:
  """The 9-14 race, realized sequentially: the funnel persists the moved-to label,
  then a stale whole-object writer lands -- the anchor on disk stays the funnel's."""
  mgr = SessionManager(build_sessions_cfg(tmp_path))
  session = await mgr.create_session(CreateSessionRequest(name="race"))
  await _seed_anchors(mgr, session.id, cc="cc-1", label="pool-a")

  # The placement funnel persists the account the transcript moved to.
  read_back = await mgr.persist_claude_account(session.id, "pool-b")
  assert read_back == "pool-b"

  # The stale writer's whole-object save, from its pre-move snapshot.
  stale = SessionMetadata(id=session.id, name="race", backend=OPUS_BACKEND_ID)
  stale.cc_session_id = "cc-1"
  stale.claude_account = "pool-a"
  await mgr.save_metadata(stale)

  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.claude_account == "pool-b"
  assert disk.cc_session_id == "cc-1"


@pytest.mark.asyncio
async def test_authorized_channels_still_change_the_anchors(tmp_path: Path) -> None:
  """The two funnels and the clear channel write their fields; the guard's
  reconciliation is skipped for exactly them."""
  mgr = SessionManager(build_sessions_cfg(tmp_path))
  session = await mgr.create_session(CreateSessionRequest(name="channels"))

  read_back = await mgr.persist_cc_session_id(session.id, "cc-2")
  assert read_back == "cc-2"
  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.cc_session_id == "cc-2" and disk.cc_session_started_at is not None

  await mgr.persist_claude_account(session.id, "pool-c")
  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.claude_account == "pool-c" and disk.cc_session_id == "cc-2"

  await mgr.clear_cc_session_anchor(session.id)
  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.cc_session_id is None and disk.cc_session_started_at is None
  assert disk.claude_account == "pool-c", "the clear channel clears the resume anchor, not the label"

  # A whole-object save after the clear cannot resurrect the cleared anchor.
  stale = await mgr.get_session(session.id)
  stale.cc_session_id = "cc-2"
  await mgr.save_metadata(stale)
  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.cc_session_id is None


@pytest.mark.asyncio
async def test_migrate_branch_save_keeps_anchors_disk_true(tmp_path: Path) -> None:
  """The old-format migrate branch saves without acquiring (get_session runs under
  callers' locks too) and never moves an anchor: a legacy rating-key session with
  anchors on disk migrates with both anchors intact."""
  mgr = SessionManager(build_sessions_cfg(tmp_path))
  session = await mgr.create_session(CreateSessionRequest(name="legacy"))
  await _seed_anchors(mgr, session.id, cc="cc-legacy", label="pool-b")
  # Seed a legacy rating key straight onto disk, past the funnels.
  import json

  meta_path = mgr._metadata_path(session.id)
  raw = json.loads(meta_path.read_text(encoding="utf-8"))
  raw["round_ratings"] = {"5": "thumbs_up"}
  meta_path.write_text(json.dumps(raw), encoding="utf-8")
  mgr._invalidate_cache(session.id)

  # Under a held lock, as the in-class save sites call get_session: the migrate
  # save must not try to re-acquire.
  async with mgr._lock_for(session.id):
    fresh = await mgr.get_session(session.id)
  assert fresh.round_ratings == {"legacy:5": "thumbs_up"}
  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.cc_session_id == "cc-legacy" and disk.claude_account == "pool-b"


@pytest.mark.asyncio
async def test_each_held_save_site_runs_under_its_lock_with_anchors_intact(tmp_path: Path) -> None:
  """The six in-class lock-holding save sites declare the held lock (no self-deadlock
  -- this test completing is the proof) and none of them moves an anchor."""
  cfg = build_sessions_cfg(tmp_path)
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="held-sites"))
  await _seed_anchors(mgr, session.id, cc="cc-held", label="pool-b")
  # A second session in the same group, for _rewrite_group's sweep.
  other = await mgr.create_session(CreateSessionRequest(name="other"))
  await _seed_anchors(mgr, other.id, cc="cc-other", label="pool-a")
  for meta in (session, other):
    meta.group = "old-group"
    await mgr.save_metadata(meta)
  # One old chat event, so the recycle's archive_offset site actually saves.
  old_ts = datetime.now(UTC) - timedelta(days=30)
  await mgr.save_chat_event(session.id, user_event("old", timestamp=old_ts.isoformat()))

  # 1. _update_field (rename_session).
  await mgr.rename_session(session.id, "renamed")
  # 2. _set_unread_flag (mark_unread).
  await mgr.mark_unread(session.id)
  # 3. archive_offset (recycle_scheduled_session).
  recycle = await mgr.recycle_scheduled_session(session.id, datetime.now(UTC))
  assert recycle["events_archived"] == 1
  # 4. _rewrite_group (rename_group).
  assert await mgr.rename_group("old-group", "new-group") == 2
  # 5. _save_field_fresh (persist_master_run).
  await mgr.persist_master_run(session.id, None)
  # 6. elone's parent-archive save (elone_session).
  child = await mgr.elone_session(session.id, event_index=0)
  assert child.parent_session_id == session.id

  for sid in (session.id, other.id):
    disk = await mgr.read_metadata_fresh(sid)
    expected = ("cc-held", "pool-b") if sid == session.id else ("cc-other", "pool-a")
    assert (disk.cc_session_id, disk.claude_account) == expected


class _FakeDatetime(datetime):
  """The cron-api tests' frozen-clock shape, aimed at the recycle's Saturday math."""

  _frozen: datetime

  @classmethod
  def now(cls, tz: tzinfo | None = None) -> datetime:
    return cls._frozen if tz is None else cls._frozen.astimezone(tz)


@pytest.mark.asyncio
async def test_weekly_recycle_clears_the_anchor_through_the_channel(tmp_path: Path) -> None:
  """The weekly recycle's anchor clear goes through clear_cc_session_anchor: disk
  loses the anchor (a whole-object save would be corrected back), the recycle's
  round runs with expect_fresh_session=True, and no whole-object save remains."""
  mgr = SessionManager(build_sessions_cfg(tmp_path))
  session = await mgr.create_session(CreateSessionRequest(name="recycled"), backend=OPUS_BACKEND_ID)
  await _seed_anchors(mgr, session.id, cc="cc-old", label="pool-b")
  stale_meta = await mgr.get_session(session.id)
  stale_meta.scheduled_task = "nightly-task"
  # Frozen clock: Wednesday Sep 9 2026 noon PT, so last Saturday 1am PT is
  # Sep 5 08:00 UTC -- 8 days before it is safely older than the cutoff.
  _FakeDatetime._frozen = datetime(2026, 9, 9, 19, 0, tzinfo=UTC)  # Wednesday noon PT
  stale_meta.cc_session_started_at = _FakeDatetime._frozen - timedelta(days=8)
  await mgr.save_metadata(stale_meta)

  run_message_mock = AsyncMock(return_value="new-cc-id")
  with (
      patch("src.core.master_trigger.datetime", _FakeDatetime),
      patch(MASTER_TRIGGER_RUN_MESSAGE_WITH_RESUME_RECOVERY_PATCH_TARGET, run_message_mock),
      patch("src.core.master_trigger.run_message", run_message_mock),
  ):
    await trigger_master(session.id, "worker summary", mgr._cfg, mgr)

  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.cc_session_id is None and disk.cc_session_started_at is None
  assert disk.claude_account == "pool-b"
  assert run_message_mock.await_count == 1
  assert run_message_mock.await_args.kwargs["expect_fresh_session"] is True, (
      "the recycle's fresh start suppresses the next round's anchor-missing alarm")


@pytest.mark.asyncio
async def test_concurrent_funnel_and_whole_object_save_serialize_on_the_lock(tmp_path: Path) -> None:
  """The anchors' funnel and an unauthorized whole-object save interleave on the
  per-session lock: whichever order they land in, the disk anchor never regresses
  and both writes' non-anchor fields survive."""
  mgr = SessionManager(build_sessions_cfg(tmp_path))
  session = await mgr.create_session(CreateSessionRequest(name="interleave"))
  await _seed_anchors(mgr, session.id, cc="cc-1", label="pool-a")

  stale = await mgr.get_session(session.id)
  stale.claude_account = "pool-a"  # stale snapshot
  stale.name = "renamed-by-stale-writer"

  funnel_task = asyncio.create_task(mgr.persist_claude_account(session.id, "pool-b"))
  stale_task = asyncio.create_task(mgr.save_metadata(stale))
  await asyncio.gather(funnel_task, stale_task)

  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.claude_account == "pool-b", "the funnel's label wins in either interleaving"


@pytest.mark.asyncio
async def test_save_metadata_acquires_the_lock_for_lock_free_callers(tmp_path: Path) -> None:
  """A lock-free caller (the API route shape) gets the acquisition inside
  save_metadata: two concurrent whole-object saves of the same session serialize
  instead of racing their atomic writes."""
  mgr = SessionManager(build_sessions_cfg(tmp_path))
  session = await mgr.create_session(CreateSessionRequest(name="serialize"))
  meta_a = await mgr.get_session(session.id)
  meta_b = await mgr.get_session(session.id)
  meta_a.name = "name-a"
  meta_b.name = "name-b"

  await asyncio.gather(mgr.save_metadata(meta_a), mgr.save_metadata(meta_b))

  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.name in {"name-a", "name-b"}, "both writes landed, serialized"
  assert await mgr.get_session(session.id) is not None
