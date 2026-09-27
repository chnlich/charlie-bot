"""The resume anchors' write-guard: only the authorized channels change
``cc_session_id`` / ``claude_account``; every other save is corrected back to
disk, the six in-class lock-holding save sites run declared under their locks,
and the weekly recycle clears the anchor through its channel."""

from pathlib import Path

import pytest
from conftest import build_sessions_cfg
from structlog.testing import capture_logs

from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager


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

  # The switch endpoint's backfill funnel records the producing backend.
  read_back = await mgr.persist_native_backend(session.id, "codex-o3")
  assert read_back == "codex-o3"
  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.native_backend == "codex-o3"

  # The v2 launch's spawn-time channel writes the provenance triple in one
  # authorized save (driven here through the real TaskTreeManager channel).
  cfg = build_sessions_cfg(tmp_path)
  tree = TaskTreeManager(cfg, mgr)
  await tree.record_native_anchor(
      session.id, prompt_hash="hash-3", backend="opus", model="opus-model", reset_anchor=False)
  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.native_prompt_hash == "hash-3"
  assert disk.native_backend == "opus" and disk.native_model == "opus-model"

  # A whole-object save after the funnels cannot roll either anchor back.
  stale = await mgr.get_session(session.id)
  stale.native_backend = "claude-opus-5"
  await mgr.save_metadata(stale)
  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.native_backend == "opus"

  await mgr.clear_cc_session_anchor(session.id)
  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.cc_session_id is None and disk.cc_session_started_at is None
  assert disk.claude_account == "pool-c", "the clear channel clears the resume anchor, not the label"
  assert disk.native_backend == "opus", "the clear channel clears the resume anchor, not the provenance"

  # A whole-object save after the clear cannot resurrect the cleared anchor.
  stale = await mgr.get_session(session.id)
  stale.cc_session_id = "cc-2"
  await mgr.save_metadata(stale)
  disk = await mgr.read_metadata_fresh(session.id)
  assert disk.cc_session_id is None
