"""Acceptance tests for session-message-projection.

Covers definitional equivalence, lossless paging, dirty-mark correctness,
no-file-reads on the paging path, and archive fallback.
"""

from __future__ import annotations

import json
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest

from src.api.message_utils import events_to_messages, events_to_view
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest
from src.core.message_projection import MessageProjection
from src.core.sessions import SessionManager

# ---------------------------------------------------------------------------
# Fixture event builders
# ---------------------------------------------------------------------------


def _assistant_event(content: str, event_id: str = "assistant") -> dict:
  return {
      "id": event_id,
      "type": ET.ASSISTANT,
      "message": {
          "content": [{
              "type": "text",
              "text": content
          }]
      },
  }


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
  """Events that produce exactly *count* user messages."""
  return [
      {
          "id": f"u{i}",
          "type": ET.USER,
          "content": f"msg-{i}",
          "timestamp": f"2026-01-01T00:{i:02d}:00Z"
      } for i in range(count)
  ]


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


@pytest.mark.parametrize("name,events", _real_session_events())
def test_projection_history_equals_events_to_messages_real_sessions(name: str, events: list[dict]) -> None:
  """Definitional equivalence on real sessions under ~/.charliebot/sessions."""
  projection = MessageProjection(events)
  reference = events_to_messages(events)
  proj_identities = [_identity_tuple(m) for m in projection.history]
  ref_identities = [_identity_tuple(m) for m in reference]
  assert proj_identities == ref_identities, f"mismatch in real session '{name}'"


# ---------------------------------------------------------------------------
# 2. Lossless paging
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("limit", [1, 7, 40, 100])
def test_lossless_backwards_walk_returns_full_id_set(limit: int) -> None:
  """Full backwards walk with slice_before returns exactly the full message-id set."""
  events = _many_messages_events(1015)
  projection = MessageProjection(events)
  reference = events_to_messages(events)
  full_ids = [m["id"] for m in reference]
  full_id_set = set(full_ids)
  total = len(full_ids)

  collected_ids: list[str] = []
  page_sizes: list[int] = []
  next_befores: list[int] = []
  before = total
  while before > 0:
    page, next_before, has_more = projection.slice_before(before, limit)
    page_ids = [m["id"] for m in page]
    collected_ids.extend(page_ids)
    page_sizes.append(len(page))
    next_befores.append(next_before)
    assert next_before < before or before == 0, "next_before must be strictly < before for a non-empty page"
    assert has_more == (next_before > 0)
    before = next_before

  assert len(collected_ids) == total, f"collected {len(collected_ids)} but expected {total}"
  assert set(collected_ids) == full_id_set, "id SET mismatch — missing or duplicate ids"
  assert len(collected_ids) == len(set(collected_ids)), "duplicates found"
  # Exact page sizes except the last
  expected_full_pages = total // limit
  remainder = total % limit
  for i in range(expected_full_pages):
    assert page_sizes[i] == limit, f"page {i} has {page_sizes[i]} messages, expected {limit}"
  if remainder:
    assert page_sizes[-1] == remainder, f"last page has {page_sizes[-1]}, expected {remainder}"
  # Strictly decreasing next_before
  for i in range(len(next_befores) - 1):
    assert next_befores[i] > next_befores[i + 1], "next_before not strictly decreasing"


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


# ---------------------------------------------------------------------------
# 3. Dirty-mark correctness
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_dirty_mark_prefix_stability_and_rebuild(tmp_path: Path) -> None:
  """A projection built mid-run stays prefix-stable; after master_done it equals fresh aggregation."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))

  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()):
    # Feed a user event and an assistant event (run is still open).
    await mgr.persist_and_broadcast(session.id, {"type": "user", "content": "q1", "timestamp": "t1"})
    await mgr.persist_and_broadcast(
        session.id,
        {
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "draft1"
                }]
            },
            "timestamp": "t2"
        },
    )

  # Build a projection mid-run.
  projection = mgr.get_message_projection(session.id)
  assert projection is not None
  committed_before = list(projection.committed)
  history_len_before = len(projection.history)

  # The committed prefix must be stable: only 1 committed user message, draft is pending.
  assert len(committed_before) == 1
  assert committed_before[0]["role"] == "user"
  assert projection.pending_draft is not None
  assert projection.pending_draft["content"] == "draft1"

  # Projection is cached — second call returns the same object.
  assert mgr.get_message_projection(session.id) is projection

  # Now close the run with master_done — this dirty-marks the projection.
  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()):
    await mgr.persist_and_broadcast(session.id, {"type": "master_done", "thinking_seconds": 1, "timestamp": "t3"})

  # Dirty mark is set.
  assert session.id in mgr._projection_dirty

  # Rebuild — the projection must now equal a fresh full aggregation.
  rebuilt = mgr.get_message_projection(session.id)
  assert rebuilt is not None
  assert rebuilt is not projection, "projection must be rebuilt after dirty mark"
  assert session.id not in mgr._projection_dirty

  fresh_events = mgr.load_chat_events_sync(session.id)
  fresh_reference = events_to_messages(fresh_events)
  rebuilt_identities = [_identity_tuple(m) for m in rebuilt.history]
  ref_identities = [_identity_tuple(m) for m in fresh_reference]
  assert rebuilt_identities == ref_identities
  # The assistant draft is now committed (flushed by master_done).
  assert rebuilt.pending_draft is None
  assert len(rebuilt.committed) == 3  # user + assistant + separator


@pytest.mark.asyncio
async def test_dirty_mark_only_on_master_done(tmp_path: Path) -> None:
  """Non-master_done events do not dirty-mark the projection."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))

  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()):
    await mgr.persist_and_broadcast(session.id, {"type": "user", "content": "hi", "timestamp": "t1"})
  projection = mgr.get_message_projection(session.id)
  assert projection is not None
  assert session.id not in mgr._projection_dirty

  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()):
    await mgr.persist_and_broadcast(
        session.id,
        {
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "x"
                }]
            },
            "timestamp": "t2"
        },
    )
  # assistant event does NOT dirty-mark (only master_done does).
  assert session.id not in mgr._projection_dirty
  # Cached projection is still the same object.
  assert mgr.get_message_projection(session.id) is projection


# ---------------------------------------------------------------------------
# 4. No file reads on the paging path
# ---------------------------------------------------------------------------


def test_paging_path_does_not_call_parse_ndjson_range(monkeypatch: pytest.MonkeyPatch) -> None:
  """slice_before on an already-built projection must not read files."""
  from src.core import ndjson

  def _boom(*args, **kwargs):
    raise AssertionError("parse_ndjson_range must not be called on the paging path")

  monkeypatch.setattr(ndjson, "parse_ndjson_range", _boom)
  monkeypatch.setattr("src.api.message_utils.parse_ndjson_range", _boom, raising=False)

  events = _many_messages_events(100)
  projection = MessageProjection(events)
  # Multiple slice_before calls — none should trigger file I/O.
  for before in [100, 60, 20, 0]:
    projection.slice_before(before, 10)
  projection.tail(10)


def test_page_latency_does_not_grow_with_session_size() -> None:
  """slice_before cost is O(page), independent of history length."""
  small = MessageProjection(_many_messages_events(100))
  large = MessageProjection(_many_messages_events(5000))

  # Warm up
  small.slice_before(100, 40)
  large.slice_before(5000, 40)

  small_times = []
  large_times = []
  for _ in range(20):
    t0 = time.perf_counter()
    small.slice_before(100, 40)
    small_times.append(time.perf_counter() - t0)

    t0 = time.perf_counter()
    large.slice_before(5000, 40)
    large_times.append(time.perf_counter() - t0)

  avg_small = sum(small_times) / len(small_times)
  avg_large = sum(large_times) / len(large_times)
  # The large projection is 50x bigger but slice cost should not grow
  # proportionally — allow a generous bound (10x) to avoid flakiness.
  assert avg_large < avg_small * 10, f"large={avg_large:.6f}s small={avg_small:.6f}s — latency grew with size"


# ---------------------------------------------------------------------------
# 5. Archive fallback
# ---------------------------------------------------------------------------


def _append_events(path: Path, events: list[dict]) -> None:
  path.parent.mkdir(parents=True, exist_ok=True)
  with open(path, "a", encoding="utf-8") as f:
    for ev in events:
      f.write(json.dumps(ev) + "\n")


@pytest.mark.asyncio
async def test_archive_offset_returns_none_from_projection(tmp_path: Path) -> None:
  """A session with archive_offset > 0 returns None from get_message_projection."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))

  base = datetime(2026, 5, 10, 0, 0, 0, tzinfo=timezone.utc)
  cutoff = base + timedelta(days=3)
  events = [
      {
          "type": "user",
          "content": f"e{i}",
          "timestamp": (base + timedelta(hours=i)).isoformat()
      } for i in range(5)
  ]
  events += [
      {
          "type": "user",
          "content": f"f{i}",
          "timestamp": (cutoff + timedelta(hours=i)).isoformat()
      } for i in range(3)
  ]
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

  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))

  base = datetime(2026, 5, 10, 0, 0, 0, tzinfo=timezone.utc)
  cutoff = base + timedelta(days=3)
  events = [
      {
          "type": "user",
          "content": f"e{i}",
          "timestamp": (base + timedelta(hours=i)).isoformat()
      } for i in range(5)
  ]
  events += [
      {
          "type": "user",
          "content": f"f{i}",
          "timestamp": (cutoff + timedelta(hours=i)).isoformat()
      } for i in range(3)
  ]
  _append_events(mgr.get_chat_events_path(session.id), events)
  await mgr.recycle_scheduled_session(session.id, cutoff)

  # The events endpoint should use the fallback path (event-index cursor).
  # archive_offset=5, live has 3 events at global indices 5,6,7.
  # before=8 (global event index), limit=3 → events [5,8) = 3 messages.
  page = await get_session_events_page(session.id, before=8, limit=3, session_mgr=mgr)
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
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t"))

  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()):
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
  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()):
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
