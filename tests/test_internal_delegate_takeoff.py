"""Regression tests for /api/internal/delegate takeoff gate behavior."""

import random
from datetime import UTC, datetime, timedelta
from typing import Any
from unittest.mock import AsyncMock

import pytest
from conftest import (
    FakeSessionManager,
    user_event,
)
from fastapi import HTTPException

from src.api import internal
from src.core import event_types as ET
from src.core.models import (
    DelegateRequest,
    SessionMetadata,
    TaskType,
)
from src.core.takeoff_gate import DelegationBlockedError, check_takeoff_gate


def _reference_takeoff_gate(
    events: list[dict[str, Any]],
    now: datetime,
) -> None:
  """The forward full-history walk the backward scan must match verdict-for-verdict.

  Kept verbatim from the pre-scan implementation (phrase matching, stamp parsing,
  and the 12 h window) as the randomized parity test's reference: any divergence
  between this walk and :func:`check_takeoff_gate` is a semantics bug, not a test
  artifact.
  """
  effective_now = now.astimezone(UTC)
  latest_user_has_takeoff = False
  latest_pre_takeoff_at: datetime | None = None
  for event in events:
    if event.get("type") != ET.USER or not isinstance(event.get("content"), str):
      continue
    normalized = " ".join(event.get("content").casefold().split())
    latest_user_has_takeoff = "take off" in normalized
    if "pre take off" in normalized:
      timestamp = event.get("timestamp")
      if isinstance(timestamp, str) and timestamp:
        try:
          issued_at = datetime.fromisoformat(timestamp)
        except ValueError:
          issued_at = None
        if issued_at is not None:
          issued_at = None if issued_at.tzinfo is None else issued_at.astimezone(UTC)
        if issued_at is not None:
          latest_pre_takeoff_at = issued_at
  pre_takeoff_active = (
      latest_pre_takeoff_at is not None and
      latest_pre_takeoff_at <= effective_now < latest_pre_takeoff_at + timedelta(hours=12))
  if latest_user_has_takeoff or pre_takeoff_active:
    return
  raise DelegationBlockedError("blocked")


def _build_request(
    task_type: TaskType = TaskType.IMPLEMENT,
    repo_path: str | None = "/tmp/repo",
    base_branch: str | None = "main",
    backend: str | None = "codex-o3",
) -> DelegateRequest:
  return DelegateRequest(
      session_id="session-id",
      description="Do work",
      base_branch=base_branch,
      backend=backend,
      repo_path=repo_path,
      task_type=task_type,
  )


def _improve_request() -> internal.ImproveRequest:
  return internal.ImproveRequest(
      session_id="session-id",
      repo_path="/tmp/repo",
      base_branch="main",
      backend="codex-o3",
      goal="Improve this",
  )


def test_takeoff_gate_blocks_takeoff_followed_by_ordinary_user_message() -> None:
  session_mgr = FakeSessionManager([
      user_event("Take Off"),
      user_event("One more ordinary message"),
  ])

  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_nested_tool_result_does_not_mint_takeoff() -> None:
  session_mgr = FakeSessionManager(
      [{
          "type": ET.USER,
          "message": {
              "role": "user",
              "content": [{
                  "type": ET.TOOL_RESULT,
                  "content": "take off"
              }],
          },
      }])

  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_expired_pre_takeoff_before_a_take_off_message_stays_allowed() -> None:
  """The backward scan stops at the file-last real user message; a file-older
  expired pre-takeoff can never change the verdict the takeoff phrase already settles."""
  issued_at = datetime(2026, 7, 18, 12, 0, tzinfo=UTC)
  session_mgr = FakeSessionManager(
      [
          user_event("pre take off", issued_at.isoformat()),
          user_event("some ordinary follow-up"),
          user_event("take off"),
      ])

  check_takeoff_gate("session-id", session_mgr, now=issued_at + timedelta(hours=48))


def _gate_verdict(mgr: Any, now: datetime) -> bool:
  try:
    check_takeoff_gate("session-id", mgr, now=now)
    return True
  except DelegationBlockedError:
    return False


def test_takeoff_gate_memo_matches_forward_walk_across_appended_turns() -> None:
  """Verdict parity across the memo's suffix folds: each round appends one
  event to the same list the live chat-events cache grows in place, and the
  gate's verdict must equal the forward reference over the grown history —
  the streamed-turn shape where the busiest master session appends between
  delegations."""
  rng = random.Random(20260911)
  phrase_pool = [
      "take off",
      "TAKE   OFF",
      "pre take off",
      "please proceed",
      "pre take off then take off",
      "no authorization here",
  ]
  stamp_pool = [
      None,
      "not-a-timestamp",
      "2026-07-18T12:00:00",  # tz-less: fails closed
      "2026-07-18T12:00:00+00:00",
      "2026-07-20T12:00:00+00:00",
  ]
  now_base = datetime(2026, 7, 18, 12, 0, tzinfo=UTC)
  for _ in range(40):
    events = [user_event(rng.choice(phrase_pool), rng.choice(stamp_pool)) for _ in range(rng.randint(0, 6))]
    mgr = FakeSessionManager(events)
    for _ in range(10):
      now = now_base + timedelta(hours=rng.randint(0, 13))
      assert _gate_verdict(mgr, now) == _forward_verdict(events, now), (events, now)
      events.append(user_event(rng.choice(phrase_pool), rng.choice(stamp_pool)))


def _forward_verdict(events: list[dict[str, Any]], now: datetime) -> bool:
  try:
    _reference_takeoff_gate(events, now)
    return True
  except DelegationBlockedError:
    return False


@pytest.mark.asyncio
async def test_delegate_task_returns_403_when_takeoff_gate_blocks(monkeypatch: pytest.MonkeyPatch) -> None:
  req = _build_request()
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id=req.session_id, name="Test")

  def fake_takeoff_gate(session_id: str, mgr: Any) -> None:
    assert session_id == req.session_id
    assert mgr is session_mgr
    raise DelegationBlockedError("blocked")

  monkeypatch.setattr(internal, "check_takeoff_gate", fake_takeoff_gate)

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_mgr=session_mgr)

  assert exc_info.value.status_code == 403
  assert exc_info.value.detail == "blocked"
  session_mgr.persist_and_broadcast.assert_not_awaited()


@pytest.mark.asyncio
async def test_improve_stays_blocked_without_takeoff() -> None:
  req = _improve_request()
  session_mgr = FakeSessionManager([{"type": ET.USER, "content": "please proceed"}])

  with pytest.raises(HTTPException) as exc_info:
    await internal.start_improve_loop(req, session_mgr=session_mgr)

  assert exc_info.value.status_code == 403
  assert "no active authorization" in exc_info.value.detail


@pytest.mark.asyncio
async def test_delegate_task_verify_rejects_repo_path() -> None:
  req = _build_request(task_type=TaskType.VERIFY, repo_path="/tmp/repo", base_branch=None)
  session_mgr = AsyncMock()

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_mgr=session_mgr)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "verify delegations are repo-less; omit repo_path"
  session_mgr.get_session.assert_not_awaited()
