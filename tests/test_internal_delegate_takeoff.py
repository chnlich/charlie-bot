"""Regression tests for /api/internal/delegate takeoff gate behavior."""

import random
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock

import pytest
from conftest import (
    OPUS_BACKEND_ID,
    OPUS_BACKEND_OPTION,
    FakeSessionManager,
    user_event,
)
from conftest import THREE_BACKEND_OPTIONS as VERIFY_BACKEND_OPTIONS
from fastapi import HTTPException

from src.features.improve import api as improve_api
from src.infra import event_types as ET
from src.infra.config import CharlieBotConfig
from src.infra.models import DelegateRequest, SessionMetadata, TaskType
from src.runtime import spawner_backends
from src.runtime.api import internal
from src.runtime.takeoff_gate import DelegationBlockedError, check_takeoff_gate


def _stub_task_manager():
  """A task-tree manager over the test's session manager (v1 sessions never
  reach its authorization path; the signature keeps one owner for both)."""
  from src.runtime.task_sessions import TaskTreeManager
  return TaskTreeManager(CharlieBotConfig(charliebot_home=Path("/tmp/delegate-takeoff-stub")), _LastSessionManager())


class _LastSessionManager:
  """The no-op session seam the stub task-tree owner reads (never queried on
  these v1-shaped paths, since their sessions carry profile=None)."""

  async def get_session(self, session_id: str):
    return None


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


def _patch_resolve_rig(monkeypatch: pytest.MonkeyPatch) -> object:
  """Install the resolve/config pair the _authorize_spawn_request tests share.

  Returns the stubbed cfg so each test asserts its resolved_cfg is this object.
  """
  cfg = object()

  async def fake_resolve(*args: Any, **kwargs: Any) -> tuple[str, str]:
    del args, kwargs
    return "codex-o3", "o3"

  monkeypatch.setattr(internal, "get_config", lambda: cfg)
  monkeypatch.setattr(spawner_backends, "resolve_requested_subagent_backend_model", fake_resolve)
  return cfg


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
    await improve_api.start_improve_loop(req, session_mgr=session_mgr)

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


@pytest.mark.asyncio
@pytest.mark.parametrize("task_type", [TaskType.IMPLEMENT, TaskType.QUICK_EDIT, TaskType.SCRIPT_RUN])
@pytest.mark.parametrize("half", ["repo", "branch"])
async def test_delegate_task_repo_scoped_types_take_repo_and_base_together(task_type: TaskType, half: str) -> None:
  """Exactly one of repo_path/base_branch is a malformed repo-less delegation."""
  repo_path = "/tmp/repo" if half == "repo" else None
  base_branch = "main" if half == "branch" else None
  req = _build_request(task_type=task_type, repo_path=repo_path, base_branch=base_branch)
  session_mgr = AsyncMock()

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_mgr=session_mgr)

  assert exc_info.value.status_code == 400
  assert "together" in exc_info.value.detail
  session_mgr.get_session.assert_not_awaited()


@pytest.mark.asyncio
@pytest.mark.parametrize("task_type", [TaskType.IMPLEMENT, TaskType.QUICK_EDIT, TaskType.SCRIPT_RUN])
async def test_delegate_task_repo_less_request_passes_the_schema_gate(
    task_type: TaskType, monkeypatch: pytest.MonkeyPatch) -> None:
  """Omitting repo_path and base_branch together is a valid repo-less delegation."""
  req = _build_request(task_type=task_type, repo_path=None, base_branch=None)
  session_mgr = FakeSessionManager([user_event("take off")])
  _patch_resolve_rig(monkeypatch)
  captured: dict = {}

  async def fake_delegate_task_tree(req, task_mgr, session_mgr, caller, backend, model):
    captured["repo_path"] = req.repo_path
    captured["base_branch"] = req.base_branch
    return {
        "session_id": "child",
        "parent_session_id": req.session_id,
        "run_id": "r",
        "thread_id": "r",
        "description": req.description
    }

  monkeypatch.setattr(internal, "_delegate_task_tree", fake_delegate_task_tree)

  result = await internal.delegate_task(req, session_mgr=session_mgr, task_mgr=_stub_task_manager(), caller=None)

  assert result["session_id"] == "child"
  assert captured["repo_path"] is None
  assert captured["base_branch"] is None


@pytest.mark.asyncio
async def test_delegate_task_returns_400_for_invalid_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  req = _build_request()
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id=req.session_id, name="Test")

  def fake_takeoff_gate(session_id: str, mgr: Any) -> None:
    assert session_id == req.session_id
    assert mgr is session_mgr

  async def fake_resolve_requested_subagent_backend_model(*args: Any, **kwargs: Any) -> tuple[str, str]:
    raise ValueError("requested backend 'codex-o3' is not in backends.options")

  monkeypatch.setattr(internal, "check_takeoff_gate", fake_takeoff_gate)
  monkeypatch.setattr(
      spawner_backends, "resolve_requested_subagent_backend_model", fake_resolve_requested_subagent_backend_model)
  monkeypatch.setattr(internal, "get_config", lambda: object())

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_mgr=session_mgr)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "requested backend 'codex-o3' is not in backends.options"


# --- verify default backend via backends.preference ---


def _build_verify_cfg(preference: list[str]) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      paths={"worktree_dir": "/tmp/worktrees"},
      backends={
          "options": VERIFY_BACKEND_OPTIONS,
          "preference": preference
      },
  )


class BackendFakeSessionManager:

  def __init__(self, backend: str) -> None:
    self.backend = backend

  async def get_session(self, session_id: str) -> SessionMetadata:
    return SessionMetadata(id=session_id, name="Test", backend=self.backend)


async def _authorize_verify(
    monkeypatch: pytest.MonkeyPatch,
    session_backend: str,
    preference: list[str],
    backend: str | None = None,
) -> tuple[str | None, str | None]:
  req = _build_request(task_type=TaskType.VERIFY, repo_path=None, base_branch=None, backend=backend)
  monkeypatch.setattr(internal, "get_config", lambda: _build_verify_cfg(preference))
  session_mgr = BackendFakeSessionManager(session_backend)
  resolved_backend, resolved_model = await internal._authorize_spawn_request(req, session_mgr, _stub_task_manager())
  return resolved_backend, resolved_model


@pytest.mark.asyncio
async def test_verify_no_backend_defaults_to_first_differing_preference(monkeypatch: pytest.MonkeyPatch) -> None:
  """Session backend is the first preference entry -> the second (first differing) entry wins."""
  resolved = await _authorize_verify(
      monkeypatch, session_backend=OPUS_BACKEND_ID, preference=[OPUS_BACKEND_ID, "codex-o3"])
  assert resolved == ("codex-o3", "o3")


@pytest.mark.asyncio
async def test_verify_no_backend_session_backend_not_in_preference_uses_first_entry(
    monkeypatch: pytest.MonkeyPatch) -> None:
  resolved = await _authorize_verify(monkeypatch, session_backend="kimi-k2.5", preference=[OPUS_BACKEND_ID, "codex-o3"])
  assert resolved == (OPUS_BACKEND_ID, OPUS_BACKEND_OPTION.model)


@pytest.mark.asyncio
async def test_verify_no_backend_empty_preference_keeps_session_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  resolved = await _authorize_verify(monkeypatch, session_backend="codex-o3", preference=[])
  assert resolved == ("codex-o3", "o3")


@pytest.mark.asyncio
async def test_verify_explicit_backend_wins_over_preference(monkeypatch: pytest.MonkeyPatch) -> None:
  resolved = await _authorize_verify(
      monkeypatch,
      session_backend=OPUS_BACKEND_ID,
      preference=[OPUS_BACKEND_ID, "codex-o3"],
      backend="kimi-k2.5",
  )
  assert resolved == ("kimi-k2.5", "kimi-k2.5")


@pytest.mark.asyncio
async def test_verify_unknown_explicit_backend_returns_400(monkeypatch: pytest.MonkeyPatch) -> None:
  with pytest.raises(HTTPException) as exc_info:
    await _authorize_verify(
        monkeypatch,
        session_backend=OPUS_BACKEND_ID,
        preference=[OPUS_BACKEND_ID, "codex-o3"],
        backend="nonexistent",
    )

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "requested backend 'nonexistent' is not in backends.options"
