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
    capture_create_logged_task,
    scheduled_trigger_event,
    user_event,
)
from conftest import THREE_BACKEND_OPTIONS as VERIFY_BACKEND_OPTIONS
from fastapi import HTTPException

from src.api import internal
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import (
    DelegateRequest,
    SessionMetadata,
    SpawnRequest,
    TaskType,
    ThreadMetadata,
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
          if issued_at.tzinfo is None:
            issued_at = None
          else:
            issued_at = issued_at.astimezone(UTC)
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


def _patch_delegate_spawn_rig(
    monkeypatch: pytest.MonkeyPatch,
    req: DelegateRequest,
    session_mgr: Any,
    thread_mgr: Any,
    captured: dict[str, Any],
) -> None:
  """Install the resolve/spawn/create_logged_task/get_config fakes shared by the delegate_task
  flow tests. The resolve fake is awaited directly, so its body asserts the session and requested
  backend at call time. The spawn fake is never awaited — create_logged_task's capture stub closes
  the coroutine — so the capture (not this body) is what pins its bound arguments for assertions."""

  async def fake_resolve_requested_subagent_backend_model(
      session_id: str,
      cfg: Any,
      mgr: Any,
      requested_backend: str | None = None,
  ) -> tuple[str, str]:
    assert session_id == req.session_id
    assert mgr is session_mgr
    assert requested_backend == "codex-o3"
    return "codex-o3", "o3"

  async def fake_spawn_worker(
      session_id: str,
      description: str,
      thread_id: str,
      cfg: Any,
      mgr: Any,
      t_mgr: Any,
      request: SpawnRequest | None = None,
  ) -> None:
    return None

  monkeypatch.setattr(
      internal, "resolve_requested_subagent_backend_model", fake_resolve_requested_subagent_backend_model)
  monkeypatch.setattr(internal, "spawn_worker", fake_spawn_worker)
  monkeypatch.setattr(internal, "create_logged_task", capture_create_logged_task(captured))
  monkeypatch.setattr(internal, "get_config", lambda: object())


def test_takeoff_gate_blocks_takeoff_followed_by_ordinary_user_message() -> None:
  session_mgr = FakeSessionManager([
      user_event("Take Off"),
      user_event("One more ordinary message"),
  ])

  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_allows_takeoff_followed_by_trigger_user_message() -> None:
  session_mgr = FakeSessionManager(
      [
          user_event("take off"),
          scheduled_trigger_event("[Scheduled trigger fired] training completed"),
      ])

  check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_scheduled_trigger_does_not_mint_takeoff() -> None:
  session_mgr = FakeSessionManager([scheduled_trigger_event("[Scheduled trigger fired] take off")])

  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_scheduled_trigger_excluded_by_type_regardless_of_content() -> None:
  """An ET.SCHEDULED_TRIGGER event is excluded by event type, not by content prefix."""
  session_mgr = FakeSessionManager([scheduled_trigger_event("take off")])

  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_real_user_literal_banner_mints_takeoff() -> None:
  """A real user message whose text carries the takeoff phrase mints a takeoff
  window, banner prefix included: the gate excludes by event type only."""
  session_mgr = FakeSessionManager([user_event("[Scheduled trigger fired] take off")])

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


def test_takeoff_gate_nested_tool_result_does_not_cancel_ordinary_takeoff() -> None:
  session_mgr = FakeSessionManager(
      [
          user_event("take off"),
          {
              "type": ET.USER,
              "message": {
                  "role": "user",
                  "content": [{
                      "type": ET.TOOL_RESULT,
                      "content": "not a command"
                  }],
              },
          },
      ])

  check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_ignores_task_delegated_metadata() -> None:
  session_mgr = FakeSessionManager(
      [{
          "type": ET.TASK_DELEGATED,
          "description": "take off",
          "delegate_invocation": {
              "task_type": "implement"
          },
      }])

  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_allows_repeated_ordinary_takeoff_after_task_delegated_event() -> None:
  session_mgr = FakeSessionManager(
      [
          user_event("take off"),
          {
              "type": ET.TASK_DELEGATED,
              "thread_id": "thread-id",
              "description": "Do work",
              "timestamp": "2026-07-18T12:00:00+00:00",
              "backend": "codex-o3",
              "model": "o3",
              "delegate_invocation":
                  {
                      "task_type": "implement",
                      "repo_path": "/tmp/repo",
                      "base_branch": "main",
                      "task_spec_file": None,
                      "reviewer_context_file": None,
                      "keep_worktree": False,
                      "backend": "codex-o3",
                  },
          },
      ])

  check_takeoff_gate("session-id", session_mgr)
  check_takeoff_gate("session-id", session_mgr)


def test_pre_takeoff_window_survives_real_user_messages_and_expires_at_12_hours() -> None:
  issued_at = datetime(2026, 7, 18, 12, 0, tzinfo=UTC)
  session_mgr = FakeSessionManager(
      [
          user_event("PRE\n\t TAKE   OFF", issued_at.isoformat()),
          user_event("A later real user message"),
      ])

  check_takeoff_gate(
      "session-id",
      session_mgr,
      now=issued_at + timedelta(hours=12) - timedelta(microseconds=1),
  )
  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr, now=issued_at + timedelta(hours=12))


def test_new_pre_takeoff_starts_a_new_window() -> None:
  first_issued_at = datetime(2026, 7, 17, 12, 0, tzinfo=UTC)
  second_issued_at = datetime(2026, 7, 18, 12, 0, tzinfo=UTC)
  session_mgr = FakeSessionManager(
      [
          user_event("pre take off", first_issued_at.isoformat()),
          user_event("normal follow-up"),
          user_event("pre take off", second_issued_at.isoformat()),
          user_event("another normal follow-up"),
      ])

  check_takeoff_gate("session-id", session_mgr, now=second_issued_at + timedelta(hours=11))


def test_ordinary_takeoff_matching_is_independent_from_pre_matching() -> None:
  session_mgr = FakeSessionManager([user_event("pre\n take\t off")])

  check_takeoff_gate("session-id", session_mgr)


def test_ordinary_takeoff_needs_no_timestamp_and_is_not_expiring() -> None:
  session_mgr = FakeSessionManager([user_event("take\n off")])

  check_takeoff_gate("session-id", session_mgr, now=datetime(2099, 1, 1, tzinfo=UTC))


@pytest.mark.parametrize("timestamp", [None, "not-a-timestamp"])
def test_pre_takeoff_with_missing_or_unparseable_timestamp_fails_closed(timestamp: str | None) -> None:
  session_mgr = FakeSessionManager([
      user_event("pre take off", timestamp),
      user_event("a later real user message"),
  ])

  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_blocks_when_no_user_string_message_contains_takeoff() -> None:
  session_mgr = FakeSessionManager(
      [
          {
              "type": ET.USER,
              "content": "please proceed"
          },
          {
              "type": ET.USER,
              "content": [{
                  "type": ET.TOOL_RESULT,
                  "content": "take off"
              }]
          },
      ])

  with pytest.raises(DelegationBlockedError) as exc_info:
    check_takeoff_gate("session-id", session_mgr)

  assert str(exc_info.value) == (
      'Delegation blocked: no active authorization. A valid "pre take off" within 12 hours or '
      '"take off" in the latest real user message is required before delegating.')


@pytest.mark.parametrize("events", [
    [],
    [{
        "type": ET.ASSISTANT,
        "content": "take off"
    }],
])
def test_takeoff_gate_blocks_with_empty_history_or_no_user_messages(events: list[dict[str, Any]]) -> None:
  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", FakeSessionManager(events))


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


def test_takeoff_gate_scan_stops_at_file_last_parseable_pre_takeoff() -> None:
  """A file-older parseable pre-takeoff never overrides the file-last one, so the
  scan may stop at it: both walks must expire the window by the same stamp."""
  first_issued_at = datetime(2026, 7, 17, 12, 0, tzinfo=UTC)
  last_issued_at = datetime(2026, 7, 18, 12, 0, tzinfo=UTC)
  session_mgr = FakeSessionManager(
      [
          user_event("pre take off", first_issued_at.isoformat()),
          user_event("work in progress"),
          user_event("pre take off", last_issued_at.isoformat()),
          user_event("please continue with the plan"),
      ])

  check_takeoff_gate("session-id", session_mgr, now=last_issued_at + timedelta(hours=11))
  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr, now=last_issued_at + timedelta(hours=12))


def test_takeoff_gate_scan_matches_forward_walk_on_randomized_histories() -> None:
  """Verdict parity between the backward scan and the forward reference over
  randomized histories: phrase variants, stamp variants, and event kinds in
  random file order, judged at randomized `now` points."""
  rng = random.Random(20260909)
  phrase_pool = [
      "take off",
      "TAKE   OFF",
      "pre take off",
      "PRE\n\t TAKE   OFF",
      "please proceed",
      "pre take off then take off",
      "let us begin",
      "take\toff",
      "no authorization here",
      "x" * 200 + " take off",
  ]
  stamp_pool = [
      None,
      "not-a-timestamp",
      "2026-07-18T12:00:00",  # tz-less: fails closed
      "2026-07-18T12:00:00+00:00",
      "2026-07-20T12:00:00+00:00",
      "2026-07-10T12:00:00+00:00",
  ]

  def random_event() -> dict[str, Any]:
    kind = rng.random()
    if kind < 0.55:
      return user_event(rng.choice(phrase_pool), rng.choice(stamp_pool))
    if kind < 0.7:
      return scheduled_trigger_event(rng.choice(phrase_pool), rng.choice(stamp_pool))
    if kind < 0.85:
      return {
          "type": ET.USER,
          "message": {
              "role": "user",
              "content": [{
                  "type": ET.TOOL_RESULT,
                  "content": rng.choice(phrase_pool)
              }],
          },
      }
    if kind < 0.95:
      return {
          "type": ET.TASK_DELEGATED,
          "description": rng.choice(phrase_pool),
          "timestamp": rng.choice(stamp_pool) or "2026-07-18T12:00:00+00:00",
      }
    return {"type": ET.ASSISTANT, "content": rng.choice(phrase_pool)}

  now_base = datetime(2026, 7, 18, 12, 0, tzinfo=UTC)
  offsets = [
      -timedelta(hours=13), -timedelta(hours=12),
      timedelta(0),
      timedelta(hours=11, minutes=59),
      timedelta(hours=12),
      timedelta(hours=48)
  ]
  for _ in range(400):
    events = [random_event() for _ in range(rng.randint(0, 14))]
    now = now_base + rng.choice(offsets)
    mgr = FakeSessionManager(events)
    reference_verdict: tuple[bool, str] = (False, "")
    try:
      _reference_takeoff_gate(events, now)
      reference_verdict = (True, "")
    except DelegationBlockedError as e:
      reference_verdict = (False, str(e))
    scan_verdict: tuple[bool, str] = (False, "")
    try:
      check_takeoff_gate("session-id", mgr, now=now)
      scan_verdict = (True, "")
    except DelegationBlockedError as e:
      scan_verdict = (False, str(e))
    assert scan_verdict[0] == reference_verdict[0], (events, now, scan_verdict, reference_verdict)


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


def test_takeoff_gate_memo_suffix_user_message_with_older_stamp_stays_allowed_until_expiry() -> None:
  """The corner the early-break scan under-filled: the file-last user message
  carries the takeoff phrase, so the stored stamp answer stayed unset; a later
  ordinary user message must fall back to the older stamp's window, not to
  blocked."""
  issued_at = datetime(2026, 7, 18, 12, 0, tzinfo=UTC)
  mgr = FakeSessionManager(
      [
          user_event("pre take off", issued_at.isoformat()),
          user_event("work in progress"),
          user_event("take off"),
      ])
  assert _gate_verdict(mgr, issued_at + timedelta(hours=48))
  mgr.events.append(user_event("an ordinary follow-up"))
  assert _gate_verdict(mgr, issued_at + timedelta(hours=11))
  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", mgr, now=issued_at + timedelta(hours=12))


def test_takeoff_gate_memo_suffix_stamp_overrides_older_prefix_stamp() -> None:
  first_issued_at = datetime(2026, 7, 17, 12, 0, tzinfo=UTC)
  second_issued_at = datetime(2026, 7, 18, 12, 0, tzinfo=UTC)
  mgr = FakeSessionManager([
      user_event("pre take off", first_issued_at.isoformat()),
      user_event("ordinary"),
  ])
  assert _gate_verdict(mgr, first_issued_at + timedelta(hours=11))
  mgr.events.append(user_event("pre take off", second_issued_at.isoformat()))
  mgr.events.append(user_event("ordinary again"))
  assert _gate_verdict(mgr, second_issued_at + timedelta(hours=11))
  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", mgr, now=second_issued_at + timedelta(hours=12))
  # The older prefix stamp must not resurrect once the newer one expires.
  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", mgr, now=second_issued_at + timedelta(hours=13))


def test_takeoff_gate_memo_suffix_without_user_message_keeps_takeoff_window() -> None:
  issued_at = datetime(2026, 7, 18, 12, 0, tzinfo=UTC)
  mgr = FakeSessionManager([
      user_event("take off"),
      user_event("pre take off", issued_at.isoformat()),
  ])
  assert _gate_verdict(mgr, issued_at + timedelta(hours=48))
  for _ in range(3):
    mgr.events.append({"type": ET.ASSISTANT, "content": "streamed delta"})
  assert _gate_verdict(mgr, issued_at + timedelta(hours=11))
  assert _gate_verdict(mgr, issued_at + timedelta(hours=12))


def test_takeoff_gate_memo_rebuilds_on_list_replacement() -> None:
  """A wholesale list replacement (a new object, the archive-recycle shape)
  rebuilds from a fresh walk: the replaced list's answers must not serve."""
  issued_at = datetime(2026, 7, 18, 12, 0, tzinfo=UTC)
  mgr = FakeSessionManager([user_event("take off")])
  assert _gate_verdict(mgr, issued_at + timedelta(hours=48))
  replaced = [user_event("please proceed")]
  mgr.events = replaced
  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", mgr, now=issued_at + timedelta(hours=48))


def test_takeoff_gate_memo_empty_history_blocks_and_stays_blocked_on_append() -> None:
  mgr = FakeSessionManager([])
  issued_at = datetime(2026, 7, 18, 12, 0, tzinfo=UTC)
  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", mgr, now=issued_at)
  mgr.events.append({"type": ET.ASSISTANT, "content": "take off"})
  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", mgr, now=issued_at)
  mgr.events.append(user_event("take off"))
  assert _gate_verdict(mgr, issued_at)


@pytest.mark.asyncio
async def test_delegate_task_returns_403_when_takeoff_gate_blocks(monkeypatch: pytest.MonkeyPatch) -> None:
  req = _build_request()
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id=req.session_id, name="Test")
  thread_mgr = AsyncMock()

  def fake_takeoff_gate(session_id: str, mgr: Any) -> None:
    assert session_id == req.session_id
    assert mgr is session_mgr
    raise DelegationBlockedError("blocked")

  monkeypatch.setattr(internal, "check_takeoff_gate", fake_takeoff_gate)

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 403
  assert exc_info.value.detail == "blocked"
  thread_mgr.create_thread.assert_not_awaited()
  session_mgr.persist_and_broadcast.assert_not_awaited()


@pytest.mark.asyncio
@pytest.mark.parametrize("task_type", [TaskType.IMPLEMENT, TaskType.QUICK_EDIT, TaskType.SCRIPT_RUN])
async def test_delegate_task_repo_task_types_block_without_takeoff(task_type: TaskType) -> None:
  req = _build_request(task_type=task_type)
  session_mgr = FakeSessionManager([{"type": ET.USER, "content": "please proceed"}])
  thread_mgr = AsyncMock()

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 403
  assert "no active authorization" in exc_info.value.detail
  thread_mgr.create_thread.assert_not_awaited()
  session_mgr.persist_and_broadcast.assert_not_awaited()


@pytest.mark.asyncio
async def test_improve_stays_blocked_without_takeoff() -> None:
  req = internal.ImproveRequest(
      session_id="session-id",
      repo_path="/tmp/repo",
      base_branch="main",
      backend="codex-o3",
      goal="Improve this",
  )
  session_mgr = FakeSessionManager([{"type": ET.USER, "content": "please proceed"}])
  thread_mgr = AsyncMock()

  with pytest.raises(HTTPException) as exc_info:
    await internal.start_improve_loop(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 403
  assert "no active authorization" in exc_info.value.detail


@pytest.mark.asyncio
@pytest.mark.parametrize("task_type", [TaskType.IMPLEMENT, TaskType.QUICK_EDIT, TaskType.SCRIPT_RUN])
async def test_all_nonverify_delegate_types_can_reuse_ordinary_takeoff(
    task_type: TaskType,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  req = _build_request(task_type=task_type)
  session_mgr = FakeSessionManager([user_event("take off")])
  cfg = object()

  async def fake_resolve(*args: Any, **kwargs: Any) -> tuple[str, str]:
    del args, kwargs
    return "codex-o3", "o3"

  monkeypatch.setattr(internal, "get_config", lambda: cfg)
  monkeypatch.setattr(internal, "resolve_requested_subagent_backend_model", fake_resolve)

  for _ in range(3):
    _meta, resolved_cfg, resolved_backend, resolved_model = await internal._authorize_spawn_request(req, session_mgr)
    assert resolved_cfg is cfg
    assert (resolved_backend, resolved_model) == ("codex-o3", "o3")


@pytest.mark.asyncio
async def test_improve_uses_the_same_pre_takeoff_gate(monkeypatch: pytest.MonkeyPatch) -> None:
  issued_at = datetime.now(UTC) - timedelta(hours=1)
  req = internal.ImproveRequest(
      session_id="session-id",
      repo_path="/tmp/repo",
      base_branch="main",
      backend="codex-o3",
      goal="Improve this",
  )
  session_mgr = FakeSessionManager(
      [
          user_event("pre take off", issued_at.isoformat()),
          user_event("continue with the approved work"),
      ])
  cfg = object()

  async def fake_resolve(*args: Any, **kwargs: Any) -> tuple[str, str]:
    del args, kwargs
    return "codex-o3", "o3"

  monkeypatch.setattr(internal, "get_config", lambda: cfg)
  monkeypatch.setattr(internal, "resolve_requested_subagent_backend_model", fake_resolve)
  _meta, resolved_cfg, resolved_backend, resolved_model = await internal._authorize_spawn_request(req, session_mgr)

  assert resolved_cfg is cfg
  assert (resolved_backend, resolved_model) == ("codex-o3", "o3")


@pytest.mark.asyncio
async def test_delegate_task_verify_skips_takeoff_gate_and_spawns_repoless(monkeypatch: pytest.MonkeyPatch) -> None:
  req = _build_request(task_type=TaskType.VERIFY, repo_path=None, base_branch=None)
  session_mgr = FakeSessionManager([{"type": ET.USER, "content": "please proceed"}])
  thread_mgr = AsyncMock()
  thread_mgr.create_thread.return_value = ThreadMetadata(
      id="thread-id",
      session_id=req.session_id,
      description=req.description,
  )
  captured: dict[str, Any] = {}

  def fail_if_gate_runs(session_id: str) -> list[dict[str, Any]]:
    raise AssertionError(f"takeoff gate should not run for verify: {session_id}")

  session_mgr.load_chat_events_sync = fail_if_gate_runs  # type: ignore[method-assign]
  _patch_delegate_spawn_rig(monkeypatch, req, session_mgr, thread_mgr, captured)

  result = await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert result == {"thread_id": "thread-id", "description": req.description}
  assert thread_mgr.create_thread.call_args.kwargs["require_review"] is False
  assert captured["request"] == SpawnRequest(
      repo_path=None,
      base_branch=None,
      context=req.context,
      resolved_backend="codex-o3",
      resolved_model="o3",
      task_type=TaskType.VERIFY,
  )
  session_mgr.persist_and_broadcast.assert_awaited_once()
  task_event = session_mgr.persist_and_broadcast.await_args.args[1]
  assert task_event["type"] == ET.TASK_DELEGATED
  assert task_event["thread_id"] == "thread-id"
  assert task_event["description"] == req.description
  assert task_event["backend"] == "codex-o3"
  assert task_event["model"] == "o3"
  assert task_event["delegate_invocation"] == {
      "task_type": "verify",
      "repo_path": None,
      "base_branch": None,
      "task_spec_file": None,
      "reviewer_context_file": None,
      "keep_worktree": False,
      "backend": "codex-o3",
  }


@pytest.mark.asyncio
async def test_delegate_task_verify_rejects_repo_path() -> None:
  req = _build_request(task_type=TaskType.VERIFY, repo_path="/tmp/repo", base_branch=None)
  session_mgr = AsyncMock()
  thread_mgr = AsyncMock()

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "verify delegations are repo-less; omit repo_path"
  session_mgr.get_session.assert_not_awaited()
  thread_mgr.create_thread.assert_not_awaited()


@pytest.mark.asyncio
async def test_delegate_task_does_not_pass_takeoff_gate_to_spawn_worker(monkeypatch: pytest.MonkeyPatch) -> None:
  req = _build_request()
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id=req.session_id, name="Test")
  thread_mgr = AsyncMock()
  thread_mgr.create_thread.return_value = ThreadMetadata(
      id="thread-id",
      session_id=req.session_id,
      description=req.description,
  )

  captured: dict[str, Any] = {}

  def fake_takeoff_gate(session_id: str, mgr: Any) -> None:
    assert session_id == req.session_id
    assert mgr is session_mgr

  monkeypatch.setattr(internal, "check_takeoff_gate", fake_takeoff_gate)
  _patch_delegate_spawn_rig(monkeypatch, req, session_mgr, thread_mgr, captured)

  result = await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert result == {"thread_id": "thread-id", "description": req.description}
  assert captured["session_id"] == req.session_id
  assert captured["description"] == req.description
  assert captured["thread_id"] == "thread-id"
  assert captured["request"] == SpawnRequest(
      repo_path=req.repo_path,
      base_branch=req.base_branch,
      context=req.context,
      resolved_backend="codex-o3",
      resolved_model="o3",
      task_type=TaskType.IMPLEMENT,
  )
  assert not hasattr(captured["request"], "require_takeoff")
  session_mgr.persist_and_broadcast.assert_awaited_once()
  task_event = session_mgr.persist_and_broadcast.await_args.args[1]
  assert task_event["type"] == ET.TASK_DELEGATED
  assert task_event["thread_id"] == "thread-id"
  assert task_event["description"] == req.description
  assert task_event["backend"] == "codex-o3"
  assert task_event["model"] == "o3"
  assert task_event["delegate_invocation"] == {
      "task_type": "implement",
      "repo_path": "/tmp/repo",
      "base_branch": "main",
      "task_spec_file": None,
      "reviewer_context_file": None,
      "keep_worktree": False,
      "backend": "codex-o3",
  }


@pytest.mark.asyncio
async def test_delegate_task_returns_400_for_invalid_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  req = _build_request()
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id=req.session_id, name="Test")
  thread_mgr = AsyncMock()

  def fake_takeoff_gate(session_id: str, mgr: Any) -> None:
    assert session_id == req.session_id
    assert mgr is session_mgr

  async def fake_resolve_requested_subagent_backend_model(*args: Any, **kwargs: Any) -> tuple[str, str]:
    raise ValueError("requested backend 'codex-o3' is not in backends.options")

  monkeypatch.setattr(internal, "check_takeoff_gate", fake_takeoff_gate)
  monkeypatch.setattr(
      internal, "resolve_requested_subagent_backend_model", fake_resolve_requested_subagent_backend_model)
  monkeypatch.setattr(internal, "get_config", lambda: object())

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "requested backend 'codex-o3' is not in backends.options"
  thread_mgr.create_thread.assert_not_awaited()


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
  _meta, _cfg, resolved_backend, resolved_model = await internal._authorize_spawn_request(req, session_mgr)
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
