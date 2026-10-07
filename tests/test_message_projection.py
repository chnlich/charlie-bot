"""Acceptance tests for session-message-projection.

Covers definitional equivalence, turn-aligned lossless paging, dirty-mark
correctness, no-file-reads on the paging path, and archive fallback.
"""

from __future__ import annotations

from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
    BROADCAST_PATCH_TARGET,
    make_home_session,
    recycle_archive_cutoff_events,
)
from conftest import assistant_event as _assistant_event
from conftest import queued_user_reorder_events as _reorder_events

from src.api.message_utils import events_to_messages
from src.core import event_types as ET
from src.core.message_projection import MessageProjection

# ---------------------------------------------------------------------------
# Fixture event builders
# ---------------------------------------------------------------------------


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
          **_assistant_event("draft response", event_id="assistant-1"), "timestamp": "t2"
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
    events.extend(_assistant_event(f"a{turn_i}-{j}", f"a{turn_i}-{j}") for j in range(length - 1))
    events.append(
        {
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

# ---------------------------------------------------------------------------
# 1. Definitional equivalence
# ---------------------------------------------------------------------------


def _assert_history_equals_events_to_messages(name: str, events: list[dict]) -> None:
  projection = MessageProjection(events)
  reference = events_to_messages(events)
  proj_identities = [_identity_tuple(m) for m in projection.history]
  ref_identities = [_identity_tuple(m) for m in reference]
  assert proj_identities == ref_identities, f"mismatch in '{name}'"


@pytest.mark.parametrize(("name", "events"), FIXTURE_EVENTS)
def test_projection_history_equals_events_to_messages(name: str, events: list[dict]) -> None:
  """projection.history must equal events_to_messages(all_events) by definition."""
  _assert_history_equals_events_to_messages(name, events)


# ---------------------------------------------------------------------------
# 2. Turn-aligned, lossless paging
# ---------------------------------------------------------------------------


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
    assert next_before == 0 or committed[next_before - 1]["role"] == "separator", ("page start must be a turn start")
    assert len(page) >= limit or next_before == 0, (
        "page must hold at least `limit` messages unless history is exhausted")
    assert next_before < before, "next_before must be strictly < before for a non-empty page"
    assert has_more == (next_before > 0)
    collected_ids.extend(m["id"] for m in page)
    before = next_before

  assert len(collected_ids) == len(full_ids), f"collected {len(collected_ids)} but expected {len(full_ids)}"
  assert set(collected_ids) == set(full_ids), "id SET mismatch — missing or duplicate ids"
  assert len(collected_ids) == len(set(collected_ids)), "duplicates found"


# ---------------------------------------------------------------------------
# 3. Cache validity is derived, not tracked
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_first_paint_surfaces_are_disjoint(tmp_path: Path) -> None:
  """The bubble list and the streaming preview never carry the same message."""
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")

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


# ---------------------------------------------------------------------------
# 4. No file reads on the paging path
# ---------------------------------------------------------------------------


def test_paging_path_does_not_call_parse_ndjson_range(monkeypatch: pytest.MonkeyPatch) -> None:
  """slice_before on an already-built projection must not read files."""
  from src.core import ndjson

  def _boom(*args: object, **kwargs: object) -> None:
    raise AssertionError("parse_ndjson_range must not be called on the paging path")

  monkeypatch.setattr(ndjson, "parse_ndjson_range", _boom)

  events = _many_messages_events(100)
  projection = MessageProjection(events)
  # Multiple slice_before calls — none should trigger file I/O.
  for before in [100, 60, 20, 0]:
    projection.slice_before(before, 10)
  projection.tail(10)


# ---------------------------------------------------------------------------
# 6. Worker summary projection: thread_id and origin_session_id
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_projection_memo_hit_archived_session_is_always_miss(tmp_path: Path) -> None:
  """The projection cache never holds an archived session's entry, so the hit
  helper needs no archive check of its own — the miss sends the caller to the
  threaded getter, whose None routes to the legacy cursor path."""
  _cfg, mgr, session = await make_home_session(tmp_path, name="t")

  await recycle_archive_cutoff_events(mgr, session.id)
  meta = await mgr.get_session(session.id)
  assert meta is not None and meta.archive_offset > 0
  assert mgr.get_message_projection(session.id) is None
  assert mgr.projection_memo_hit(session.id) is None
