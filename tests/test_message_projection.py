"""Acceptance tests for session-message-projection.

Covers definitional equivalence, turn-aligned lossless paging, dirty-mark
correctness, no-file-reads on the paging path, and archive fallback.
"""

from __future__ import annotations

import bisect
import json
import time
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import BROADCAST_PATCH_TARGET, make_home_session
from conftest import append_events as _append_events
from conftest import archive_cutoff_events as _archive_cutoff_events
from conftest import assistant_event as _assistant_event

from src.api.message_utils import events_to_messages
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.message_projection import MessageProjection
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager

# ---------------------------------------------------------------------------
# Fixture event builders
# ---------------------------------------------------------------------------


def _reorder_events() -> list[dict]:
  """Events that trigger _stable_history_projection reordering.

  A session_id-bearing event ... master_done interval containing a queued
  user event. The queued user is moved after the completed run.
  """
  return [
      {
          "session_id": "opencode-session"
      },
      {
          "id": "thinking-1",
          "type": ET.THINKING,
          "content": "final thought"
      },
      {
          "id": "tool-1",
          "type": ET.TOOL_USE,
          "name": "Read",
          "input": {
              "file_path": "report.txt"
          }
      },
      {
          "id": "queued-user",
          "type": ET.USER,
          "content": "second question"
      },
      {
          "id": "tool-result-1",
          "type": ET.TOOL_RESULT,
          "content": "report contents"
      },
      _assistant_event("first conclusion", "assistant-1"),
      {
          "id": "done-1",
          "type": ET.MASTER_DONE,
          "thinking_seconds": 4
      },
      {
          "session_id": "opencode-session"
      },
      _assistant_event("second answer", "assistant-2"),
      {
          "id": "done-2",
          "type": ET.MASTER_DONE,
          "thinking_seconds": 2
      },
  ]


def _pending_draft_events() -> list[dict]:
  """Events ending with a non-empty pending draft (un-flushed assistant)."""
  return [
      {
          "id": "user-1",
          "type": ET.USER,
          "content": "hi",
          "timestamp": "t1"
      },
      {
          "id": "assistant-1",
          "type": ET.ASSISTANT,
          "message": {
              "content": [{
                  "type": "text",
                  "text": "draft response"
              }]
          },
          "timestamp": "t2",
      },
  ]


def _many_messages_events(count: int) -> list[dict]:
  """Events that produce exactly *count* user messages (no separator at all)."""
  return [
      {
          "id": f"u{i}",
          "type": ET.USER,
          "content": f"msg-{i}",
          "timestamp": f"2026-01-01T00:{i:02d}:00Z"
      } for i in range(count)
  ]


def _turned_messages_events(turn_lengths: list[int]) -> list[dict]:
  """Events producing one separator-terminated turn per entry of *turn_lengths*.

  Each turn's body is one user message followed by enough assistant blocks to
  reach the given body length, and it closes with a master_done separator, so
  a turn of body length L produces L + 1 committed messages.
  """
  events: list[dict] = []
  for turn_i, length in enumerate(turn_lengths):
    if length < 1:
      raise ValueError(f"turn body length must be >= 1, got {length}")
    events.append({
        "id": f"u{turn_i}",
        "type": ET.USER,
        "content": f"q{turn_i}",
        "timestamp": f"t{turn_i}-u",
    })
    events.extend(
        _assistant_event(f"a{turn_i}-{j}", f"a{turn_i}-{j}") for j in range(length - 1))
    events.append({
        "id": f"done{turn_i}",
        "type": ET.MASTER_DONE,
        "thinking_seconds": 1,
        "timestamp": f"t{turn_i}-done",
    })
  return events


def _identity_tuple(msg: dict) -> tuple:
  return (
      msg.get("id"),
      msg.get("role"),
      len(msg.get("content", "") or ""),
      msg.get("event_index"),
  )


FIXTURE_EVENTS: list[tuple[str, list[dict]]] = [
    ("reorder", _reorder_events()),
    ("pending_draft", _pending_draft_events()),
    ("many_messages", _many_messages_events(60)),
    ("empty", []),
]


def _real_session_events() -> list[tuple[str, list[dict]]]:
  """Load events from real sessions under ~/.charliebot/sessions if present."""
  sessions_dir = Path.home() / ".charliebot" / "sessions"
  if not sessions_dir.exists():
    return []
  result: list[tuple[str, list[dict]]] = []
  for session_dir in sorted(sessions_dir.iterdir()):
    if not session_dir.is_dir():
      continue
    events_path = session_dir / "data" / "chat_events.jsonl"
    if not events_path.exists():
      continue
    events: list[dict] = []
    try:
      for line in events_path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
          events.append(json.loads(line))
    except (OSError, json.JSONDecodeError):
      continue
    if events:
      result.append((session_dir.name, events))
  return result


# Loaded once so every real-session parametrization shares a single read.
_REAL_SESSION_EVENTS = _real_session_events()


# ---------------------------------------------------------------------------
# 1. Definitional equivalence
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("name,events", FIXTURE_EVENTS)
def test_projection_history_equals_events_to_messages(name: str, events: list[dict]) -> None:
  """projection.history must equal events_to_messages(all_events) by definition."""
  projection = MessageProjection(events)
  reference = events_to_messages(events)
  proj_identities = [_identity_tuple(m) for m in projection.history]
  ref_identities = [_identity_tuple(m) for m in reference]
  assert proj_identities == ref_identities, f"mismatch in fixture '{name}'"


def test_projection_reorder_fixture_reorders_queued_user() -> None:
  """The reorder fixture must actually trigger reordering (queued user after run)."""
  events = _reorder_events()
  messages = events_to_messages(events)
  roles = [m["role"] for m in messages]
  assert roles == ["assistant", "separator", "user", "assistant", "separator"]
  ids = [m["id"] for m in messages]
  assert ids == ["assistant-1", "done-1", "queued-user", "assistant-2", "done-2"]


def test_projection_pending_draft_fixture_has_draft() -> None:
  """The pending_draft fixture must end with a non-empty pending draft."""
  projection = MessageProjection(_pending_draft_events())
  assert projection.pending_draft is not None
  assert projection.pending_draft["content"] == "draft response"
  assert len(projection.history) == 2
  assert projection.history[1] is projection.pending_draft


@pytest.mark.parametrize("name,events", _REAL_SESSION_EVENTS)
def test_projection_history_equals_events_to_messages_real_sessions(name: str, events: list[dict]) -> None:
  """Definitional equivalence on real sessions under ~/.charliebot/sessions."""
  projection = MessageProjection(events)
  reference = events_to_messages(events)
  proj_identities = [_identity_tuple(m) for m in projection.history]
  ref_identities = [_identity_tuple(m) for m in reference]
  assert proj_identities == ref_identities, f"mismatch in real session '{name}'"


# ---------------------------------------------------------------------------
# 2. Turn-aligned, lossless paging
# ---------------------------------------------------------------------------


def _assert_turn_aligned_walk(projection: MessageProjection, limit: int, label: str) -> None:
  """Walk the whole history backwards (tail, then slice_before) and assert the
  paging contract on every page:

  - page-start invariant: ``start == 0`` or ``committed[start - 1]`` is a separator
  - ``len(page) >= limit`` unless the history is exhausted (``start == 0``)
  - ``len(page) <= limit + L - 1`` where L is the turn the raw boundary fell inside
  - strict progression: each next page start is strictly below the previous one
  - pages tile ``[0, total]`` exactly (lossless, ordered, no duplicates)
  """
  committed = projection.committed
  total = len(committed)
  seps = projection.separator_ordinals
  collected: list[dict] = []
  intervals: list[tuple[int, int]] = []
  page, start, has_more = projection.tail(limit)
  before = total
  steps = 0
  while True:
    steps += 1
    assert steps <= max(1, total), f"{label}: walk did not terminate after {total} pages"
    assert start == 0 or committed[start - 1]["role"] == "separator", (
        f"{label}: page start {start} is not a turn start")
    assert len(page) >= limit or start == 0, (
        f"{label}: under-filled page ({len(page)} < {limit}) before history exhaustion")
    # Upper bound: the page may over-run the raw boundary only up to the end
    # of the turn that boundary fell inside.
    raw = max(0, before - limit)
    k = bisect.bisect_left(seps, raw)
    turn_start = seps[k - 1] + 1 if k else 0
    turn_end = seps[k] + 1 if k < len(seps) else total
    assert len(page) <= limit + (turn_end - turn_start) - 1, f"{label}: page over-ran its crossing turn"
    if page:
      intervals.append((start, before))
      collected = page + collected
    if not has_more:
      break
    page, next_start, has_more = projection.slice_before(start, limit)
    assert next_start < start, f"{label}: no strict progress ({next_start} !< {start})"
    before = start
    start = next_start

  intervals.reverse()  # built newest-first; check tiling oldest-first
  cursor = 0
  for lo, hi in intervals:
    assert lo == cursor, f"{label}: gap/overlap at ordinal {lo} (cursor {cursor})"
    assert lo < hi, f"{label}: empty page interval ({lo}, {hi})"
    cursor = hi
  assert cursor == total, f"{label}: walk stopped at {cursor}, history has {total} messages"
  assert [_identity_tuple(m) for m in collected] == [_identity_tuple(m) for m in committed], (
      f"{label}: union of pages != full message list")


@pytest.mark.parametrize("limit", [1, 7, 40, 100])
def test_lossless_backwards_walk_returns_full_id_set(limit: int) -> None:
  """Full backwards walk with slice_before returns exactly the full message-id set."""
  projection = MessageProjection(_turned_messages_events([3, 1, 6, 2] * 60))  # 240 turns, 960 messages
  committed = projection.committed
  full_ids = [m["id"] for m in committed]

  collected_ids: list[str] = []
  before = len(committed)
  while before > 0:
    page, next_before, has_more = projection.slice_before(before, limit)
    assert next_before == 0 or committed[next_before - 1]["role"] == "separator", (
        "page start must be a turn start")
    assert len(page) >= limit or next_before == 0, (
        "page must hold at least `limit` messages unless history is exhausted")
    assert next_before < before, "next_before must be strictly < before for a non-empty page"
    assert has_more == (next_before > 0)
    collected_ids.extend(m["id"] for m in page)
    before = next_before

  assert len(collected_ids) == len(full_ids), f"collected {len(collected_ids)} but expected {len(full_ids)}"
  assert set(collected_ids) == set(full_ids), "id SET mismatch — missing or duplicate ids"
  assert len(collected_ids) == len(set(collected_ids)), "duplicates found"


def test_lossless_walk_on_reorder_session() -> None:
  """Lossless walk on the reorder fixture — ids match exactly (as a set)."""
  events = _reorder_events()
  projection = MessageProjection(events)
  reference = events_to_messages(events)
  full_ids = [m["id"] for m in reference]
  total = len(full_ids)

  collected: list[str] = []
  before = total
  while before > 0:
    page, next_before, _ = projection.slice_before(before, 1)
    collected.extend(m["id"] for m in page)
    before = next_before
  assert set(collected) == set(full_ids)
  assert len(collected) == len(full_ids)


@pytest.mark.parametrize("name,events", _REAL_SESSION_EVENTS)
def test_turn_aligned_backwards_walk_real_sessions(name: str, events: list[dict]) -> None:
  """Every page of a full backwards walk starts at a turn start and the pages
  tile the whole history — on every real session under ~/.charliebot/sessions."""
  _assert_turn_aligned_walk(MessageProjection(events), limit=40, label=f"real session '{name}'")


@pytest.mark.parametrize(
    "turn_lengths",
    [
        [1] * 6,
        [3, 1, 7, 2, 5, 1, 4],
        [12],  # a single turn longer than any limit
        [2, 9, 1, 6, 3, 8, 1, 1, 5],
    ])
@pytest.mark.parametrize("limit", [1, 2, 5, 40])
def test_turn_aligned_backwards_walk_synthetic(turn_lengths: list[int], limit: int) -> None:
  """The full paging contract on generated histories with varied turn lengths."""
  label = f"turns={turn_lengths} limit={limit}"
  _assert_turn_aligned_walk(MessageProjection(_turned_messages_events(turn_lengths)), limit, label)


def test_snap_is_idempotent_on_turn_starts() -> None:
  """A requested start that is already a turn start does not move."""
  projection = MessageProjection(_turned_messages_events([2, 5, 1, 7]))
  committed = projection.committed
  total = len(committed)
  turn_starts = [0] + [s + 1 for s in projection.separator_ordinals]
  assert len(turn_starts) > 2, "fixture must produce several turn starts"
  for t in turn_starts:
    if t >= total:
      continue  # ordinal past the end is not a page start
    page, start, _ = projection.tail(total - t)
    assert start == t, f"already-aligned start {t} moved to {start}"
    assert page == committed[t:]


def test_separator_free_history_is_one_documented_page() -> None:
  """A history with no separator at all snaps to 0: the whole history is one page."""
  projection = MessageProjection(_many_messages_events(25))
  total = len(projection.committed)
  assert projection.separator_ordinals == []

  page, start, has_more = projection.tail(5)
  assert (len(page), start, has_more) == (total, 0, False)

  page, start, has_more = projection.slice_before(20, 3)
  assert (len(page), start, has_more) == (20, 0, False)


def test_tail_snaps_mid_turn_boundary_to_the_turn_start() -> None:
  """Regression for the outline orphan defect: a tail boundary landing strictly
  inside a turn (here: on its task_delegated) returns a page beginning at that
  turn's user message, not at the interior."""
  events = _turned_messages_events([2, 2, 2])  # 3 earlier turns, 3 messages each
  defect_turn = [
      {"id": "ask", "type": ET.USER, "content": "take off using kimi", "timestamp": "t-u"},
      _assistant_event("plan approved, step 1 starts", "a-plan"),
      _assistant_event("kimi resolves to opencode-kimi-k3", "a-kimi"),
      {"id": "td", "type": ET.TASK_DELEGATED, "thread_id": "th1", "description": "d", "timestamp": "t-td"},
      {"id": "ws", "type": ET.WORKER_SUMMARY, "content": "merged", "timestamp": "t-ws"},
      _assistant_event("Take off recorded, plan approved", "a-done"),
      {"id": "sep", "type": ET.MASTER_DONE, "thinking_seconds": 96, "timestamp": "t-sep"},
  ]
  events += defect_turn
  projection = MessageProjection(events)
  committed = projection.committed
  assert [m["role"] for m in committed[-7:]] == [
      "user", "assistant", "assistant", "task_delegated", "worker_summary", "assistant", "separator"
  ]

  defect_turn_start = len(committed) - 7
  interior = committed.index(next(m for m in committed if m["id"] == "td"))
  assert defect_turn_start < interior < len(committed) - 1, "raw boundary must land strictly inside the turn"

  # limit chosen so the raw start max(0, total - limit) == interior.
  page, start, has_more = projection.tail(len(committed) - interior)
  assert start == defect_turn_start, f"snapped start {start} != turn start {defect_turn_start}"
  assert committed[start - 1]["role"] == "separator"
  assert page[0]["id"] == "ask", "page must begin at the turn's user message"
  assert page[-1]["role"] == "separator"
  assert has_more is True


# ---------------------------------------------------------------------------
# 3. Cache validity is derived, not tracked
# ---------------------------------------------------------------------------

# Every event type the aggregator can commit a message for. The assertions below
# never branch on this list -- it only makes the fixture exercise the aggregator
# broadly. A new event type cannot regress the invariant, because validity is
# derived from the consumed event count rather than from a per-type hook.
_COMMITTING_EVENT_SEQUENCE: list[dict] = [
    {"type": ET.USER, "content": "q1", "timestamp": "t1"},
    _assistant_event("first block", "a1"),
    _assistant_event("second block", "a2"),
    {"type": ET.TOOL_USE, "name": "Read", "input": {}, "timestamp": "t2"},
    {"type": ET.TOOL_RESULT, "content": "out", "timestamp": "t3"},
    {"type": ET.TASK_DELEGATED, "thread_id": "th1", "description": "d", "timestamp": "t4"},
    {"type": ET.WORKER_SUMMARY, "content": "merged", "timestamp": "t5"},
    {"type": ET.HANDLER_RESULT, "task": "x", "message": "y", "status": "ok", "timestamp": "t6"},
    {"type": ET.CONTEXT_COMPACTED, "trigger": "auto", "pre_tokens": 1000, "timestamp": "t7"},
    {"type": ET.USER, "content": "q2 queued", "timestamp": "t8"},
    {"type": ET.MASTER_DONE, "still_thinking": True, "timestamp": "t9"},
    _assistant_event("second run", "a3"),
    {"type": ET.ERROR, "content": "boom", "timestamp": "t10"},
    {"type": ET.MASTER_DONE, "thinking_seconds": 3, "timestamp": "t11"},
    {"type": ET.USER, "content": "q3 after the run", "timestamp": "t12"},
]


@pytest.mark.asyncio
async def test_projection_equals_reference_after_every_append(tmp_path: Path) -> None:
  """After EVERY persisted event the projection equals a fresh full aggregation.

  This asserts the mechanism (derived validity) rather than a dirty-mark policy:
  no event type is special, so adding one later cannot reintroduce staleness.
  """
  cfg, mgr, session = await make_home_session(tmp_path, name="t")

  for i, event in enumerate(_COMMITTING_EVENT_SEQUENCE):
    with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()):
      await mgr.persist_and_broadcast(session.id, dict(event))
    # Read the projection between every append, so a stale cache would be served.
    projection = mgr.get_message_projection(session.id)
    assert projection is not None
    all_events = mgr.load_chat_events_sync(session.id)
    reference = events_to_messages(all_events)
    assert [_identity_tuple(m) for m in projection.history] == [_identity_tuple(m) for m in reference], (
        f"projection diverged from the reference after event {i} ({event['type']})")
    assert projection.event_count == len(all_events)


@pytest.mark.asyncio
async def test_first_paint_surfaces_are_disjoint(tmp_path: Path) -> None:
  """The bubble list and the streaming preview never carry the same message."""
  cfg, mgr, session = await make_home_session(tmp_path, name="t")

  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()):
    await mgr.persist_and_broadcast(session.id, {"type": ET.USER, "content": "q1", "timestamp": "t1"})
    await mgr.persist_and_broadcast(session.id, _assistant_event("reply1", "a1"))
    await mgr.persist_and_broadcast(session.id, {"type": ET.MASTER_DONE, "thinking_seconds": 1, "timestamp": "t2"})
    await mgr.persist_and_broadcast(session.id, {"type": ET.USER, "content": "q2", "timestamp": "t3"})
    await mgr.persist_and_broadcast(session.id, _assistant_event("IN PROGRESS", "a2"))

  projection = mgr.get_message_projection(session.id)
  assert projection is not None
  assert projection.pending_draft is not None
  assert projection.pending_draft["content"] == "IN PROGRESS"

  page, _oldest, _has_more = projection.tail(40)
  assert projection.pending_draft not in page
  assert "IN PROGRESS" not in [m.get("content") for m in page]

  # slice_before shares the committed ordinal domain with tail.
  older, _next_before, _more = projection.slice_before(len(projection.committed), 40)
  assert "IN PROGRESS" not in [m.get("content") for m in older]

  # history keeps its definitional meaning: committed + draft.
  assert projection.history[-1] is projection.pending_draft


@pytest.mark.asyncio
async def test_event_count_is_the_snapshot_the_projection_consumed(tmp_path: Path) -> None:
  """event_count is the boundary the first-paint cursor is derived from."""
  cfg, mgr, session = await make_home_session(tmp_path, name="t")

  for n in range(1, 5):
    with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()):
      await mgr.persist_and_broadcast(session.id, _assistant_event(f"chunk {n}", f"a{n}"))
    projection = mgr.get_message_projection(session.id)
    assert projection is not None
    assert projection.event_count == n

  assert MessageProjection([], event_index_offset=7).event_count == 7


# ---------------------------------------------------------------------------
# 4. No file reads on the paging path
# ---------------------------------------------------------------------------


def test_paging_path_does_not_call_parse_ndjson_range(monkeypatch: pytest.MonkeyPatch) -> None:
  """slice_before on an already-built projection must not read files."""
  from src.core import ndjson

  def _boom(*args, **kwargs):
    raise AssertionError("parse_ndjson_range must not be called on the paging path")

  monkeypatch.setattr(ndjson, "parse_ndjson_range", _boom)
  # chat_events binds parse_ndjson_range via from-import, so the ndjson-home patch
  # cannot reach the read path; each production binding needs its own patch.
  monkeypatch.setattr("src.core.chat_events.parse_ndjson_range", _boom)

  events = _many_messages_events(100)
  projection = MessageProjection(events)
  # Multiple slice_before calls — none should trigger file I/O.
  for before in [100, 60, 20, 0]:
    projection.slice_before(before, 10)
  projection.tail(10)


def test_page_latency_does_not_grow_with_session_size() -> None:
  """slice_before cost is O(page), independent of history length."""
  # Bounded turns (3 messages each) keep snapped page sizes independent of
  # history length — the property this test exists to measure.
  small = MessageProjection(_turned_messages_events([2] * 34))  # 102 messages
  large = MessageProjection(_turned_messages_events([2] * 1700))  # 5100 messages

  # Warm up
  small.slice_before(102, 40)
  large.slice_before(5100, 40)

  small_times = []
  large_times = []
  for _ in range(20):
    t0 = time.perf_counter()
    small.slice_before(102, 40)
    small_times.append(time.perf_counter() - t0)

    t0 = time.perf_counter()
    large.slice_before(5100, 40)
    large_times.append(time.perf_counter() - t0)

  avg_small = sum(small_times) / len(small_times)
  avg_large = sum(large_times) / len(large_times)
  # The large projection is 50x bigger but slice cost should not grow
  # proportionally — allow a generous bound (10x) to avoid flakiness.
  assert avg_large < avg_small * 10, f"large={avg_large:.6f}s small={avg_small:.6f}s — latency grew with size"


# ---------------------------------------------------------------------------
# 5. Archive fallback
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_archive_offset_returns_none_from_projection(tmp_path: Path) -> None:
  """A session with archive_offset > 0 returns None from get_message_projection."""
  cfg, mgr, session = await make_home_session(tmp_path, name="t")

  cutoff, events = _archive_cutoff_events()
  _append_events(mgr.get_chat_events_path(session.id), events)
  await mgr.recycle_scheduled_session(session.id, cutoff)

  meta = await mgr.get_session(session.id)
  assert meta is not None
  assert meta.archive_offset == 5
  assert mgr.get_message_projection(session.id) is None


@pytest.mark.asyncio
async def test_archive_fallback_serves_from_old_path(tmp_path: Path) -> None:
  """An archived session serves entirely from the event-index cursor path."""
  from src.api.sessions import get_session_events_page

  cfg, mgr, session = await make_home_session(tmp_path, name="t")

  cutoff, events = _archive_cutoff_events()
  _append_events(mgr.get_chat_events_path(session.id), events)
  await mgr.recycle_scheduled_session(session.id, cutoff)

  # The events endpoint should use the fallback path (event-index cursor).
  # archive_offset=5, live has 3 events at global indices 5,6,7.
  # before=8 (global event index), limit=3 → events [5,8) = 3 messages.
  meta = await mgr.get_session(session.id)
  page = await get_session_events_page(session.id, before=8, limit=3, meta=meta, session_mgr=mgr)
  assert page["next_before"] == 5
  assert len(page["messages"]) == 3
  assert [m["content"] for m in page["messages"]] == ["f0", "f1", "f2"]
  assert [m["event_index"] for m in page["messages"]] == [5, 6, 7]


# ---------------------------------------------------------------------------
# LRU eviction
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_lru_eviction_drops_oldest_projection(tmp_path: Path) -> None:
  """LRU cap of 8 evicts the least-recently-used projection."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)

  session_ids: list[str] = []
  for i in range(10):
    session = await mgr.create_session(CreateSessionRequest(name=f"t{i}"))
    session_ids.append(session.id)
    _append_events(
        mgr.get_chat_events_path(session.id),
        [{
            "type": "user",
            "content": f"msg{i}",
            "timestamp": f"2026-01-01T00:{i:02d}:00Z"
        }],
    )

  # Build projections for all 10 sessions.
  for sid in session_ids:
    proj = mgr.get_message_projection(sid)
    assert proj is not None

  # Only 8 should be cached (LRU cap).
  assert len(mgr._projection_cache) == 8
  # The first 2 (least recently used) should have been evicted.
  assert session_ids[0] not in mgr._projection_cache
  assert session_ids[1] not in mgr._projection_cache
  assert session_ids[2] in mgr._projection_cache

  # Re-access session_ids[2] to make it most-recently-used.
  mgr.get_message_projection(session_ids[2])
  # Add a new session to trigger eviction — session_ids[3] (now LRU) should be evicted.
  new_session = await mgr.create_session(CreateSessionRequest(name="new"))
  _append_events(
      mgr.get_chat_events_path(new_session.id),
      [{
          "type": "user",
          "content": "new",
          "timestamp": "2026-01-01T00:30:00Z"
      }],
  )
  mgr.get_message_projection(new_session.id)
  assert len(mgr._projection_cache) == 8
  assert session_ids[3] not in mgr._projection_cache
  assert session_ids[2] in mgr._projection_cache


@pytest.mark.asyncio
async def test_lru_eviction_cannot_serve_stale_after_dirty_mark(tmp_path: Path) -> None:
  """A dirty-marked projection that gets evicted is rebuilt fresh on next access."""
  cfg, mgr, session = await make_home_session(tmp_path, name="t")

  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()):
    await mgr.persist_and_broadcast(session.id, {"type": "user", "content": "first", "timestamp": "t1"})
    await mgr.persist_and_broadcast(
        session.id,
        {
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "draft"
                }]
            },
            "timestamp": "t2"
        },
    )

  projection = mgr.get_message_projection(session.id)
  assert projection is not None
  assert projection.pending_draft is not None

  # Dirty-mark via master_done.
  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()):
    await mgr.persist_and_broadcast(session.id, {"type": "master_done", "thinking_seconds": 1, "timestamp": "t3"})

  # Evict by filling the cache with other sessions.
  for i in range(8):
    other = await mgr.create_session(CreateSessionRequest(name=f"other{i}"))
    _append_events(
        mgr.get_chat_events_path(other.id),
        [{
            "type": "user",
            "content": f"m{i}",
            "timestamp": f"2026-01-01T00:{i:02d}:00Z"
        }],
    )
    mgr.get_message_projection(other.id)

  # Original session should be evicted (dirty but not cached).
  assert session.id not in mgr._projection_cache

  # Rebuild — must be fresh, not stale.
  rebuilt = mgr.get_message_projection(session.id)
  assert rebuilt is not None
  assert rebuilt.pending_draft is None, "stale projection served after dirty mark + eviction"
  assert len(rebuilt.committed) == 3  # user + assistant + separator


# ---------------------------------------------------------------------------
# 6. Worker summary projection: thread_id and origin_session_id
# ---------------------------------------------------------------------------


def _worker_summary_event(origin_session_id: str | None = None, thread_id: str = "th-1") -> dict:
  """A worker_summary event with all fields the projection today carries."""
  ev = {
      "type": ET.WORKER_SUMMARY,
      "thread_id": thread_id,
      "content": "locator",
      "full_content": "full",
      "status": "completed",
  }
  if origin_session_id is not None:
    ev["origin_session_id"] = origin_session_id
  return ev


def test_worker_summary_with_origin_projects_both_extra_fields() -> None:
  """A redirected worker_summary carries origin_session_id AND thread_id."""
  events = [_worker_summary_event(origin_session_id="origin-1")]
  messages = events_to_messages(events)
  summary = messages[0]
  assert summary["role"] == "worker_summary"
  assert summary["origin_session_id"] == "origin-1"
  assert summary["thread_id"] == "th-1"


def test_worker_summary_without_origin_keeps_fields_unchanged() -> None:
  """A non-redirected worker_summary must be indistinguishable from today.

  Its projected key set matches the pre-change shape exactly (no
  origin_session_id key at all), and the carried content fields are unchanged.
  """
  event = _worker_summary_event()
  event["timestamp"] = "t-ws"
  messages = events_to_messages([event])
  summary = messages[0]
  assert summary["role"] == "worker_summary"
  assert summary["content"] == "locator"
  assert summary["full_content"] == "full"
  assert summary["timestamp"] == "t-ws"
  # The event has no origin_session_id, so the projected message must not
  # present the key at all -- indistinguishable from today's projection.
  assert "origin_session_id" not in summary
  assert summary["thread_id"] == "th-1"


@pytest.mark.asyncio
async def test_worker_summary_delivered_to_successor_projects_origin_and_thread(tmp_path: Path) -> None:
  """End-to-end: a worker_summary delivered through deliver_to_successor into an
  eloned session projects with the origin session id and thread id."""
  cfg, mgr, parent = await make_home_session(tmp_path, name="parent", backend="claude-opus-4.6")
  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()):
    await mgr.persist_and_broadcast(parent.id, {"type": "user", "content": "q", "timestamp": "t0"})
  child = await mgr.elone_session(parent.id, event_index=0)

  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()):
    delivered = await mgr.deliver_to_successor(
        parent.id, _worker_summary_event(thread_id="th-e2e"))

  assert delivered == child.id

  projection = mgr.get_message_projection(child.id)
  assert projection is not None
  summary = next(m for m in projection.committed if m["role"] == "worker_summary")
  assert summary["origin_session_id"] == parent.id
  assert summary["thread_id"] == "th-e2e"
