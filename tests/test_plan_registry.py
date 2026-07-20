"""Tests for the plan registry: state machine, rejections, reconcile, derived state, mapping."""

import json
from pathlib import Path
from typing import Optional

import pytest

from src.core.config import CharlieBotConfig
from src.core.models import (
    BackendOption,
    CreateSessionRequest,
    SessionMetadata,
    TaskType,
    ThreadMetadata,
    ThreadStatus,
    utc_now,
)
from src.core.plans import (
    PlanRegistryManager,
    _DerivedState,
    _VerifyState,
    derive_state_str,
    map_verify_outcome,
)
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

BACKEND_OPTIONS = [
    BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
]


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=BACKEND_OPTIONS,
  )


def _write_artifact(cfg: CharlieBotConfig, session_id: str, name: str = "plan_01.html") -> str:
  artifacts_dir = cfg.sessions_dir / session_id / "artifacts"
  artifacts_dir.mkdir(parents=True, exist_ok=True)
  (artifacts_dir / name).write_text("<html>plan</html>", encoding="utf-8")
  return f"artifacts/{name}"


def _write_thread_events(thread_mgr: ThreadManager, session_id: str, thread_id: str, report: str) -> None:
  path = thread_mgr._thread_dir(session_id, thread_id) / "data" / "events.jsonl"
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(json.dumps({"type": "result", "result": report}) + "\n", encoding="utf-8")


async def _create_verify_thread(
    thread_mgr: ThreadManager,
    session_meta: SessionMetadata,
    status: ThreadStatus = ThreadStatus.IDLE,
    report: Optional[str] = None,
) -> str:
  thread = await thread_mgr.create_thread(session_meta, "verify task", require_review=False, task_type=TaskType.VERIFY)
  if status != ThreadStatus.IDLE:
    thread.status = status
    if status in (ThreadStatus.COMPLETED, ThreadStatus.FAILED, ThreadStatus.CANCELLED):
      thread.completed_at = utc_now()
    await thread_mgr.save_metadata(thread)
  if report is not None:
    _write_thread_events(thread_mgr, session_meta.id, thread.id, report)
  return thread.id


async def _setup(
    tmp_path: Path,) -> tuple[CharlieBotConfig, SessionManager, ThreadManager, PlanRegistryManager, SessionMetadata]:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  plan_mgr = PlanRegistryManager(cfg, session_mgr, thread_mgr)
  meta = await session_mgr.create_session(CreateSessionRequest(name="Test"), backend="claude-opus-4.6")
  return cfg, session_mgr, thread_mgr, plan_mgr, meta


CLEAN_REPORT = "confirmed | claim | anchor | evidence\nRESULT: clean"
MISMATCH_REPORT = "mismatch | claim | anchor | evidence\nRESULT: 1 mismatch (0 approval)"
MISMATCH_APPROVAL_REPORT = "mismatch-approval | claim | anchor | evidence\nRESULT: 2 mismatches (1 approval)"

# ---------------------------------------------------------------------------
# Derived-state truth table (pure function)
# ---------------------------------------------------------------------------


def _plan(closed=None, takeoff=None, verify_state="pending") -> dict:
  return {
      "id": 1,
      "title": "P",
      "versions":
          [
              {
                  "v": 1,
                  "file": "artifacts/plan_01.html",
                  "created_at": "2026-07-20T00:00:00+00:00",
                  "trigger": "initial",
                  "verify_thread": "t1",
                  "verify_state": verify_state,
                  "base": None,
              }
          ],
      "takeoff": takeoff,
      "closed": closed,
  }


@pytest.mark.parametrize(
    ("closed", "takeoff", "verify_state", "expected"),
    [
        ({
            "as": "superseded",
            "at": "2026-07-20T00:00:00+00:00"
        }, None, "pending", "superseded"),
        (
            {
                "as": "superseded",
                "at": "2026-07-20T00:00:00+00:00"
            }, {
                "v": 1,
                "at": "2026-07-20T00:00:00+00:00"
            }, "clean", "superseded"),
        ({
            "as": "abandoned",
            "at": "2026-07-20T00:00:00+00:00"
        }, None, "failed", "abandoned"),
        (None, None, "pending", "in flight"),
        (None, None, "clean", "awaiting approval"),
        (None, None, "mismatch", "needs amendment"),
        (None, None, "failed", "verify failed"),
        (None, {
            "v": 1,
            "at": "2026-07-20T00:00:00+00:00"
        }, "clean", "approved"),
        (None, {
            "v": 1,
            "at": "2026-07-20T00:00:00+00:00"
        }, "pending", "approved · awaiting clean verify"),
        (None, {
            "v": 1,
            "at": "2026-07-20T00:00:00+00:00"
        }, "failed", "approved · verify failed"),
    ],
)
def test_derive_state_str_truth_table(closed, takeoff, verify_state, expected) -> None:
  assert derive_state_str(_plan(closed=closed, takeoff=takeoff, verify_state=verify_state)) == expected


def test_derive_state_str_takeoff_mismatch_is_unreachable() -> None:
  with pytest.raises(RuntimeError, match="unreachable"):
    derive_state_str(_plan(takeoff={"v": 1, "at": "x"}, verify_state="mismatch"))


def test_derive_state_str_unknown_closed_as_raises() -> None:
  with pytest.raises(ValueError, match="unknown closed.as"):
    derive_state_str(_plan(closed={"as": "weird", "at": "x"}))


def test_derive_state_str_unknown_verify_state_raises() -> None:
  with pytest.raises(ValueError, match="unknown verify_state"):
    derive_state_str(_plan(verify_state="weird"))


def test_derive_state_str_empty_versions_raises() -> None:
  with pytest.raises(ValueError, match="has no versions"):
    derive_state_str({"id": 1, "title": "P", "versions": [], "takeoff": None, "closed": None})


# ---------------------------------------------------------------------------
# map_verify_outcome (pure function)
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("status", "report", "expected"),
    [
        (ThreadStatus.COMPLETED, CLEAN_REPORT, _VerifyState.CLEAN),
        (ThreadStatus.COMPLETED, MISMATCH_REPORT, _VerifyState.MISMATCH),
        (ThreadStatus.COMPLETED, MISMATCH_APPROVAL_REPORT, _VerifyState.MISMATCH),
        (ThreadStatus.FAILED, "", _VerifyState.FAILED),
        (ThreadStatus.FAILED, CLEAN_REPORT, _VerifyState.FAILED),
        (ThreadStatus.CANCELLED, "", _VerifyState.FAILED),
    ],
)
def test_map_verify_outcome(status, report, expected) -> None:
  assert map_verify_outcome(status, report) == expected


def test_map_verify_outcome_completed_empty_report_raises() -> None:
  with pytest.raises(RuntimeError, match="empty final report"):
    map_verify_outcome(ThreadStatus.COMPLETED, "")


def test_map_verify_outcome_completed_malformed_trailer_raises() -> None:
  with pytest.raises(RuntimeError, match="malformed trailer"):
    map_verify_outcome(ThreadStatus.COMPLETED, "confirmed | claim | anchor | evidence\nRESULT: clean-ish")


def test_map_verify_outcome_unexpected_status_raises() -> None:
  with pytest.raises(ValueError, match="unexpected thread status"):
    map_verify_outcome(ThreadStatus.IDLE, "")
  with pytest.raises(ValueError, match="unexpected thread status"):
    map_verify_outcome(ThreadStatus.RUNNING, "")


# ---------------------------------------------------------------------------
# State machine: present → flip → approve → amend → mismatch/failed flips → reverify → close
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_present_returns_in_flight_then_clean_flip_to_awaiting_approval(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")

  result = await plan_mgr.present(meta.id, file=file_rel, verify_thread=verify_thread, title="P1")
  assert result == {"plan": 1, "v": 1, "state": "in flight"}

  thread = await thread_mgr.get_thread(meta.id, verify_thread)
  thread.status = ThreadStatus.COMPLETED
  thread.completed_at = utc_now()
  _write_thread_events(thread_mgr, meta.id, verify_thread, CLEAN_REPORT)
  await thread_mgr.save_metadata(thread)

  await plan_mgr.flip_on_verify_completion(meta.id, verify_thread)
  listing = await plan_mgr.list_plans(meta.id)
  assert listing["plans"][0]["state"] == "awaiting approval"
  assert listing["plans"][0]["versions"][-1]["verify_state"] == "clean"


@pytest.mark.asyncio
async def test_approve_returns_approved_then_failed_flip_keeps_takeoff(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_rel, verify_thread=verify_thread, title="P1")

  result = await plan_mgr.approve(meta.id)
  assert result == {"plan": 1, "v": 1, "state": "approved · awaiting clean verify"}

  thread = await thread_mgr.get_thread(meta.id, verify_thread)
  thread.status = ThreadStatus.FAILED
  thread.completed_at = utc_now()
  await thread_mgr.save_metadata(thread)
  await plan_mgr.flip_on_verify_completion(meta.id, verify_thread)

  listing = await plan_mgr.list_plans(meta.id)
  assert listing["plans"][0]["state"] == "approved · verify failed"
  assert listing["plans"][0]["takeoff"] is not None
  assert listing["plans"][0]["versions"][-1]["verify_state"] == "failed"


@pytest.mark.asyncio
async def test_mismatch_flip_clears_takeoff(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_rel, verify_thread=verify_thread, title="P1")
  await plan_mgr.approve(meta.id)

  thread = await thread_mgr.get_thread(meta.id, verify_thread)
  thread.status = ThreadStatus.COMPLETED
  thread.completed_at = utc_now()
  _write_thread_events(thread_mgr, meta.id, verify_thread, MISMATCH_REPORT)
  await thread_mgr.save_metadata(thread)
  await plan_mgr.flip_on_verify_completion(meta.id, verify_thread)

  listing = await plan_mgr.list_plans(meta.id)
  assert listing["plans"][0]["state"] == "needs amendment"
  assert listing["plans"][0]["takeoff"] is None
  assert listing["plans"][0]["versions"][-1]["verify_state"] == "mismatch"


@pytest.mark.asyncio
async def test_cancelled_flip_maps_to_failed(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_rel, verify_thread=verify_thread, title="P1")
  await plan_mgr.approve(meta.id)

  thread = await thread_mgr.get_thread(meta.id, verify_thread)
  thread.status = ThreadStatus.CANCELLED
  thread.completed_at = utc_now()
  await thread_mgr.save_metadata(thread)
  await plan_mgr.flip_on_verify_completion(meta.id, verify_thread)

  listing = await plan_mgr.list_plans(meta.id)
  assert listing["plans"][0]["state"] == "approved · verify failed"


@pytest.mark.asyncio
async def test_amend_on_approved_clears_takeoff(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread_1 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  file_1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_1, verify_thread=verify_thread_1, title="P1")

  thread_1 = await thread_mgr.get_thread(meta.id, verify_thread_1)
  thread_1.status = ThreadStatus.COMPLETED
  thread_1.completed_at = utc_now()
  _write_thread_events(thread_mgr, meta.id, verify_thread_1, CLEAN_REPORT)
  await thread_mgr.save_metadata(thread_1)
  await plan_mgr.flip_on_verify_completion(meta.id, verify_thread_1)

  await plan_mgr.approve(meta.id)

  verify_thread_2 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  file_2 = _write_artifact(cfg, meta.id, "plan_02.html")
  result = await plan_mgr.amend(meta.id, file=file_2, verify_thread=verify_thread_2, plan_id=1)
  assert result == {"plan": 1, "v": 2, "state": "in flight"}

  listing = await plan_mgr.list_plans(meta.id)
  assert listing["plans"][0]["takeoff"] is None
  assert len(listing["plans"][0]["versions"]) == 2


@pytest.mark.asyncio
async def test_reverify_only_from_failed_and_resets_to_pending(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread_1 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  file_1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_1, verify_thread=verify_thread_1, title="P1")
  await plan_mgr.approve(meta.id)

  thread_1 = await thread_mgr.get_thread(meta.id, verify_thread_1)
  thread_1.status = ThreadStatus.FAILED
  thread_1.completed_at = utc_now()
  await thread_mgr.save_metadata(thread_1)
  await plan_mgr.flip_on_verify_completion(meta.id, verify_thread_1)

  listing = await plan_mgr.list_plans(meta.id)
  assert listing["plans"][0]["state"] == "approved · verify failed"

  verify_thread_2 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  result = await plan_mgr.reverify(meta.id, verify_thread=verify_thread_2, plan_id=1)
  assert result == {"plan": 1, "v": 1, "state": "approved · awaiting clean verify"}
  listing = await plan_mgr.list_plans(meta.id)
  assert listing["plans"][0]["versions"][-1]["verify_thread"] == verify_thread_2
  assert listing["plans"][0]["versions"][-1]["verify_state"] == "pending"


@pytest.mark.asyncio
async def test_close_superseded_and_abandoned(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_rel, verify_thread=verify_thread, title="P1")

  result = await plan_mgr.close(meta.id, plan_id=1, close_as="superseded")
  assert result == {"plan": 1, "state": "superseded"}

  # second plan, close as abandoned
  verify_thread_2 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  file_2 = _write_artifact(cfg, meta.id, "plan_02.html")
  await plan_mgr.present(meta.id, file=file_2, verify_thread=verify_thread_2, title="P2")
  result = await plan_mgr.close(meta.id, plan_id=2, close_as="abandoned")
  assert result == {"plan": 2, "state": "abandoned"}


@pytest.mark.asyncio
async def test_closing_already_closed_rejected(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_rel, verify_thread=verify_thread, title="P1")
  await plan_mgr.close(meta.id, plan_id=1, close_as="superseded")

  with pytest.raises(ValueError, match="already closed"):
    await plan_mgr.close(meta.id, plan_id=1, close_as="abandoned")


# ---------------------------------------------------------------------------
# Registration reconcile (terminal thread at registration time)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_reconcile_completed_clean_registers_clean(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.COMPLETED, report=CLEAN_REPORT)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")

  result = await plan_mgr.present(meta.id, file=file_rel, verify_thread=verify_thread, title="P1")
  assert result == {"plan": 1, "v": 1, "state": "awaiting approval"}


@pytest.mark.asyncio
async def test_reconcile_completed_mismatch_registers_mismatch(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.COMPLETED, report=MISMATCH_REPORT)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")

  result = await plan_mgr.present(meta.id, file=file_rel, verify_thread=verify_thread, title="P1")
  assert result == {"plan": 1, "v": 1, "state": "needs amendment"}


@pytest.mark.asyncio
async def test_reconcile_failed_registers_failed(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.FAILED)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")

  result = await plan_mgr.present(meta.id, file=file_rel, verify_thread=verify_thread, title="P1")
  assert result == {"plan": 1, "v": 1, "state": "verify failed"}


@pytest.mark.asyncio
async def test_reconcile_cancelled_registers_failed(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.CANCELLED)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")

  result = await plan_mgr.present(meta.id, file=file_rel, verify_thread=verify_thread, title="P1")
  assert result == {"plan": 1, "v": 1, "state": "verify failed"}


# ---------------------------------------------------------------------------
# Rejections
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_present_rejects_missing_file(tmp_path: Path) -> None:
  _cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  with pytest.raises(ValueError, match="not found inside the session directory"):
    await plan_mgr.present(meta.id, file="artifacts/missing.html", verify_thread=verify_thread, title="P1")


@pytest.mark.asyncio
async def test_present_rejects_file_outside_session_dir(tmp_path: Path) -> None:
  _cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  with pytest.raises(ValueError, match="resolves outside the session directory"):
    await plan_mgr.present(meta.id, file="../escape.html", verify_thread=verify_thread, title="P1")


@pytest.mark.asyncio
async def test_present_rejects_missing_thread(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  with pytest.raises(ValueError, match="not found in session"):
    await plan_mgr.present(meta.id, file=file_rel, verify_thread="nonexistent", title="P1")


@pytest.mark.asyncio
async def test_present_rejects_non_verify_thread(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  implement_thread = await thread_mgr.create_thread(meta, "implement task", require_review=True)
  with pytest.raises(ValueError, match="is not a verify thread"):
    await plan_mgr.present(meta.id, file=file_rel, verify_thread=implement_thread.id, title="P1")


@pytest.mark.asyncio
async def test_present_rejects_thread_with_missing_task_type(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  legacy_thread = await thread_mgr.create_thread(meta, "legacy thread", require_review=True)
  legacy_thread.task_type = None
  await thread_mgr.save_metadata(legacy_thread)
  with pytest.raises(ValueError, match="is not a verify thread"):
    await plan_mgr.present(meta.id, file=file_rel, verify_thread=legacy_thread.id, title="P1")


@pytest.mark.asyncio
async def test_present_rejects_already_bound_file(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread_1 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_rel, verify_thread=verify_thread_1, title="P1")
  verify_thread_2 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  with pytest.raises(ValueError, match=r"already bound to plan 1 v1"):
    await plan_mgr.present(meta.id, file=file_rel, verify_thread=verify_thread_2, title="P2")


@pytest.mark.asyncio
async def test_present_rejects_already_bound_thread(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  file_1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_1, verify_thread=verify_thread, title="P1")
  file_2 = _write_artifact(cfg, meta.id, "plan_02.html")
  with pytest.raises(ValueError, match=r"already bound to plan 1 v1"):
    await plan_mgr.present(meta.id, file=file_2, verify_thread=verify_thread, title="P2")


@pytest.mark.asyncio
async def test_approve_rejects_mismatch(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.COMPLETED, report=MISMATCH_REPORT)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_rel, verify_thread=verify_thread, title="P1")
  with pytest.raises(ValueError, match="mismatch verify_state"):
    await plan_mgr.approve(meta.id)


@pytest.mark.asyncio
async def test_approve_rejects_failed(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  verify_thread = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.FAILED)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_rel, verify_thread=verify_thread, title="P1")
  with pytest.raises(ValueError, match="failed verify_state"):
    await plan_mgr.approve(meta.id)


@pytest.mark.asyncio
async def test_approve_ambiguity_requires_plan(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  vt1 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, verify_thread=vt1, title="P1")
  vt2 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  f2 = _write_artifact(cfg, meta.id, "plan_02.html")
  await plan_mgr.present(meta.id, file=f2, verify_thread=vt2, title="P2")

  with pytest.raises(ValueError, match="approve requires --plan"):
    await plan_mgr.approve(meta.id)


@pytest.mark.asyncio
async def test_amend_rejects_closed(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  vt1 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, verify_thread=vt1, title="P1")
  await plan_mgr.close(meta.id, plan_id=1, close_as="superseded")
  vt2 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  f2 = _write_artifact(cfg, meta.id, "plan_02.html")
  with pytest.raises(ValueError, match="is closed"):
    await plan_mgr.amend(meta.id, file=f2, verify_thread=vt2, plan_id=1)


@pytest.mark.asyncio
async def test_amend_ambiguity_requires_plan(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  vt1 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, verify_thread=vt1, title="P1")
  vt2 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  f2 = _write_artifact(cfg, meta.id, "plan_02.html")
  await plan_mgr.present(meta.id, file=f2, verify_thread=vt2, title="P2")

  vt3 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  f3 = _write_artifact(cfg, meta.id, "plan_03.html")
  with pytest.raises(ValueError, match="amend requires --plan"):
    await plan_mgr.amend(meta.id, file=f3, verify_thread=vt3)


@pytest.mark.asyncio
async def test_amend_no_open_lineage_requires_plan(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  vt1 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, verify_thread=vt1, title="P1")
  await plan_mgr.close(meta.id, plan_id=1, close_as="superseded")

  vt2 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  f2 = _write_artifact(cfg, meta.id, "plan_02.html")
  with pytest.raises(ValueError, match="no open lineage to amend"):
    await plan_mgr.amend(meta.id, file=f2, verify_thread=vt2)


@pytest.mark.asyncio
async def test_reverify_rejects_non_failed(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  vt1 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, verify_thread=vt1, title="P1")
  vt2 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  with pytest.raises(ValueError, match="reverify only from failed"):
    await plan_mgr.reverify(meta.id, verify_thread=vt2, plan_id=1)


@pytest.mark.asyncio
async def test_reverify_rejects_already_bound_thread(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  vt1 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, verify_thread=vt1, title="P1")
  await plan_mgr.approve(meta.id)
  t1 = await thread_mgr.get_thread(meta.id, vt1)
  t1.status = ThreadStatus.FAILED
  t1.completed_at = utc_now()
  await thread_mgr.save_metadata(t1)
  await plan_mgr.flip_on_verify_completion(meta.id, vt1)
  with pytest.raises(ValueError, match=r"already bound to plan 1 v1"):
    await plan_mgr.reverify(meta.id, verify_thread=vt1, plan_id=1)


@pytest.mark.asyncio
async def test_close_rejects_invalid_close_as(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  vt1 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, verify_thread=vt1, title="P1")
  with pytest.raises(ValueError, match="--as must be superseded|abandoned"):
    await plan_mgr.close(meta.id, plan_id=1, close_as="weird")


# ---------------------------------------------------------------------------
# Persistence and atomicity
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_plans_json_shape_matches_schema(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  vt1 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, verify_thread=vt1, title="P1", base={"repo": "r", "branch": "b", "sha": "s"})

  raw = (cfg.sessions_dir / meta.id / "plans.json").read_text(encoding="utf-8")
  data = json.loads(raw)
  assert list(data.keys()) == ["plans"]
  plan = data["plans"][0]
  assert set(plan.keys()) == {"id", "title", "versions", "takeoff", "closed"}
  ver = plan["versions"][0]
  assert set(ver.keys()) == {"v", "file", "created_at", "trigger", "verify_thread", "verify_state", "base"}
  assert ver["v"] == 1
  assert ver["file"] == "artifacts/plan_01.html"
  assert ver["trigger"] == "initial"
  assert ver["verify_state"] == "pending"
  assert ver["base"] == {"repo": "r", "branch": "b", "sha": "s"}
  assert plan["takeoff"] is None
  assert plan["closed"] is None


@pytest.mark.asyncio
async def test_missing_plans_json_is_empty_registry(tmp_path: Path) -> None:
  _cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  listing = await plan_mgr.list_plans(meta.id)
  assert listing == {"plans": []}


@pytest.mark.asyncio
async def test_unregistered_completing_thread_is_noop(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  unregistered = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.COMPLETED, report=CLEAN_REPORT)
  await plan_mgr.flip_on_verify_completion(meta.id, unregistered)
  listing = await plan_mgr.list_plans(meta.id)
  assert listing == {"plans": []}


# ---------------------------------------------------------------------------
# Broadcast
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_broadcast_called_on_present(tmp_path: Path) -> None:
  cfg, session_mgr, thread_mgr, _plan_mgr, meta = await _setup(tmp_path)
  plan_mgr = PlanRegistryManager(cfg, session_mgr, thread_mgr)
  calls: list[tuple[str, dict]] = []

  async def _fake_broadcast(session_id: str, event: dict) -> None:
    calls.append((session_id, event))

  session_mgr.broadcast_only = _fake_broadcast  # type: ignore[method-assign]
  vt1 = await _create_verify_thread(thread_mgr, meta, status=ThreadStatus.IDLE)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, verify_thread=vt1, title="P1")

  assert calls == [(meta.id, {"type": "plan_updated", "session_id": meta.id, "plan_id": 1})]


# ---------------------------------------------------------------------------
# Enum reservations
# ---------------------------------------------------------------------------


def test_verify_state_enum_reserves_zero_unknown() -> None:
  assert _VerifyState.UNKNOWN == 0
  assert _VerifyState.PENDING == 1
  assert _VerifyState.CLEAN == 2
  assert _VerifyState.MISMATCH == 3
  assert _VerifyState.FAILED == 4


def test_derived_state_enum_reserves_zero_unknown() -> None:
  assert _DerivedState.UNKNOWN == 0
  assert _DerivedState.IN_FLIGHT == 1
  assert _DerivedState.APPROVED == 5
  assert _DerivedState.SUPERSEDED == 8
  assert _DerivedState.ABANDONED == 9
