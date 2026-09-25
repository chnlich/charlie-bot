"""Description prefix + body-memo contract of the thread-row payloads
(src/api/threads.py list_threads, src/api/sessions.py get_session_view)."""

import asyncio
import builtins
import json
import shutil
from datetime import UTC, datetime, timedelta
from pathlib import Path
from unittest.mock import patch

import httpx
import pytest
from conftest import (
    RESPONSES_GZIP_LEVEL1_PATCH_TARGET,
    apply_config_overrides,
    assert_gzip_served,
    fake_backends,
    fresh_state_fixture,
    gzip_explode_compress,
)
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import threads as threads_api
from src.api.deps import (
    get_session_manager,
    get_thread_manager,
    get_trigger_manager,
)
from src.api.sessions import router as sessions_router
from src.api.threads import _LIST_DESCRIPTION_CAP
from src.api.threads import router as threads_router
from src.core import sidebar_state
from src.core import threads as core_threads
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, PendingTrigger, ThreadStatus
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.triggers import TriggerManager

LONG_DESCRIPTION = "spec " * 300  # 1500 chars, over the list cap


def _reset_list_state() -> None:
  """Empty the api module's process-wide list/view memos, gates, and the sidebar
  mark state around every test: the memo-count assertions below read lengths, and
  the sweep-gate countdowns advance per process-wide poll."""
  threads_api._list_body_memo.clear()
  threads_api._sig_gate.clear()
  threads_api._thread_row_memo.clear()
  threads_api._list_gzip_memo.clear()
  threads_api._view_rows_memo.clear()
  threads_api._view_rows_gate.clear()
  sidebar_state.reset_for_tests()


_fresh_list_state = fresh_state_fixture(_reset_list_state)


def _seeded_client(tmp_path: Path) -> tuple[TestClient, str, str]:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends=fake_backends())
  sessions = SessionManager(cfg)

  async def seed() -> tuple[str, str]:
    session = await sessions.create_session(CreateSessionRequest(name="list-payload"))
    threads = ThreadManager(cfg)
    await threads.create_thread(session, "short description")
    long_thread = await threads.create_thread(session, LONG_DESCRIPTION)
    return session.id, long_thread.id

  session_id, long_thread_id = asyncio.run(seed())

  app = FastAPI()
  app.include_router(threads_router, prefix="/api/threads")
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_thread_manager] = lambda: ThreadManager(cfg)
  app.dependency_overrides[get_trigger_manager] = lambda: TriggerManager(cfg, sessions)
  app.dependency_overrides[get_session_manager] = lambda: sessions
  apply_config_overrides(app, cfg)
  return TestClient(app), session_id, long_thread_id


def _seeded_thread_dir(tmp_path: Path, name: str, *thread_texts: str) -> tuple[CharlieBotConfig, str, Path]:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends=fake_backends())
  sessions = SessionManager(cfg)

  async def seed() -> str:
    session = await sessions.create_session(CreateSessionRequest(name=name))
    threads = ThreadManager(cfg)
    for text in thread_texts:
      await threads.create_thread(session, text)
    return session.id

  session_id = asyncio.run(seed())
  return cfg, session_id, cfg.sessions_dir / session_id / "threads"


def test_list_caps_long_descriptions_and_marks_truncation(tmp_path: Path) -> None:
  client, session_id, _ = _seeded_client(tmp_path)

  rows = {row["id"]: row for row in client.get(f"/api/threads/{session_id}/list").json()}
  descriptions = {row["description"] for row in rows.values()}

  assert "short description" in descriptions
  short_row = next(row for row in rows.values() if row["description"] == "short description")
  assert "description_full_len" not in short_row

  long_row = next(row for row in rows.values() if row is not short_row)
  assert long_row["description"] == LONG_DESCRIPTION[:_LIST_DESCRIPTION_CAP]
  assert long_row["description_full_len"] == len(LONG_DESCRIPTION)


def test_thread_detail_still_serves_the_full_description(tmp_path: Path) -> None:
  client, session_id, long_thread_id = _seeded_client(tmp_path)

  meta = client.get(f"/api/threads/{session_id}/threads/{long_thread_id}").json()

  assert meta["description"] == LONG_DESCRIPTION


def test_session_view_ships_the_same_truncated_rows(tmp_path: Path) -> None:
  client, session_id, long_thread_id = _seeded_client(tmp_path)

  list_rows = {row["id"]: row for row in client.get(f"/api/threads/{session_id}/list").json()}
  view_rows = {row["id"]: row for row in client.get(f"/api/sessions/{session_id}/view").json()["threads"]}

  assert set(view_rows) == set(list_rows)
  for tid, list_row in list_rows.items():
    view_row = view_rows[tid]
    assert view_row["description"] == list_row["description"]
    assert view_row.get("description_full_len") == list_row.get("description_full_len")
  assert view_rows[long_thread_id]["description"] == LONG_DESCRIPTION[:_LIST_DESCRIPTION_CAP]
  assert view_rows[long_thread_id]["description_full_len"] == len(LONG_DESCRIPTION)


_WALK_SKIP_ENDPOINTS = [
    pytest.param(
        "/api/sessions/{session_id}/view",
        "threads",
        id="session-view",
    ),
    pytest.param(
        "/api/threads/{session_id}/list",
        None,
        id="list-poll",
    ),
]


def _count_walks(monkeypatch: pytest.MonkeyPatch) -> dict[str, int]:
  """Install a counting wrapper over ``threads_api._row_source_stats``.

  Returns the counter dict: ``walks["n"]`` is how many full source walks the
  polled routes ran since the install.
  """
  walks = {"n": 0}
  real = threads_api._row_source_stats

  def counting(threads_dir: str, triggers_dir: str, runs_dir: str | None = None):
    walks["n"] += 1
    return real(threads_dir, triggers_dir, runs_dir)

  monkeypatch.setattr(threads_api, "_row_source_stats", counting)
  return walks


@pytest.mark.parametrize(("url_pattern", "rows_key"), _WALK_SKIP_ENDPOINTS)
def test_rows_skip_the_walk_until_a_mark_or_the_sweep(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    url_pattern: str,
    rows_key: str | None,
) -> None:
  client, session_id, _ = _seeded_client(tmp_path)
  url = url_pattern.format(session_id=session_id)

  def response_rows(response: httpx.Response) -> list[dict]:
    body = response.json()
    return body if rows_key is None else body[rows_key]

  walks = _count_walks(monkeypatch)

  client.get(url)
  assert walks["n"] == 1
  for _ in range(9):
    assert client.get(url).status_code == 200
  assert walks["n"] == 1

  client.get(url)
  assert walks["n"] == 2

  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends=fake_backends())
  rows = {row["id"]: row for row in response_rows(client.get(url))}
  any_id = next(iter(rows))
  asyncio.run(ThreadManager(cfg).update_status(session_id, any_id, ThreadStatus.RUNNING))
  updated = client.get(url)
  assert next(row for row in response_rows(updated) if row["id"] == any_id)["status"] == "running"
  # The writer's mark carries the published path, so the list poll proves the
  # body against exactly that file — no walk. (The session view has no marked
  # proof and still walks.)
  if url_pattern.endswith("/list"):
    assert walks["n"] == 2
    sidebar_state.mark_sidebar_dirty(session_id)
    client.get(url)
    assert walks["n"] == 3
  else:
    assert walks["n"] == 3


def _count_metadata_opens() -> tuple[dict[str, int], object]:
  """Count ``open()`` calls that name a thread metadata.json, the parse memo's
  read path; the caller restores the builtins open around the walked region."""
  opens = {"n": 0}
  real_open = open

  def counting(file, *args, **kwargs):
    if str(file).endswith("/metadata.json"):
      opens["n"] += 1
    return real_open(file, *args, **kwargs)

  return opens, counting


def test_parse_memo_serves_across_sessions_scoped_drops(
    tmp_path: Path,
    _fresh_list_state: None,
) -> None:
  """One session's walk drops only its own vanished files: the shared parse
  memo still serves every other session's parses on their next walk.

  The drop predicate keyed the whole memo before the projection consumer
  arrived, so each walk evicted every other session's cached parses and the
  next walk of any session re-read and re-validated all its files.
  """
  cfg_a, _session_a, threads_a = _seeded_thread_dir(tmp_path / "a", "memo-a", "a one", "a two")
  cfg_b, _session_b, threads_b = _seeded_thread_dir(tmp_path / "b", "memo-b", "b one")
  mgr_a = ThreadManager(cfg_a)
  mgr_b = ThreadManager(cfg_b)
  pairs_a = list(core_threads.iter_thread_meta_stats(str(threads_a)))
  pairs_b = list(core_threads.iter_thread_meta_stats(str(threads_b)))

  opens, counting = _count_metadata_opens()
  real_open = builtins.open
  try:
    builtins.open = counting  # type: ignore[assignment]
    mgr_a.list_threads_from_stats(iter(pairs_a), str(threads_a))
    assert opens["n"] == 2
    mgr_b.list_threads_from_stats(iter(pairs_b), str(threads_b))
    assert opens["n"] == 3
    # A repeat walk of the first session after the second session's walk: the
    # scoped drop left its entries resident, so the walk opens no file.
    mgr_a.list_threads_from_stats(iter(pairs_a), str(threads_a))
    assert opens["n"] == 3
  finally:
    builtins.open = real_open  # type: ignore[assignment]

  # The scoped drop still evicts this session's own vanished file: removing a
  # thread and re-walking drops its row while the other session's stays.
  victim_dir = next(
      Path(pair[0]).parent for pair in pairs_a if json.loads(Path(pair[0]).read_text())["description"] == "a two")
  shutil.rmtree(victim_dir)
  fresh_pairs = list(core_threads.iter_thread_meta_stats(str(threads_a)))
  metas = [m for m in mgr_a.list_threads_from_stats(iter(fresh_pairs), str(threads_a)) if m is not None]
  assert {m.description for m in metas} == {"a one"}
  metas_b = [m for m in mgr_b.list_threads_from_stats(iter(pairs_b), str(threads_b)) if m is not None]
  assert {m.description for m in metas_b} == {"b one"}


def test_list_projection_walks_stop_once_the_view_memo_holds_the_set(
    tmp_path: Path,
    _fresh_list_state: None,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """The sidebar list's projected worker leaves keep every legacy session's
  rows in the view-rows memo: after the first fetch walks the set, the next
  nine fetches walk nothing (the gate's sweep lands on the tenth poll)."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends=fake_backends())
  sessions = SessionManager(cfg)

  async def seed() -> None:
    threads = ThreadManager(cfg)
    for i in range(12):
      session = await sessions.create_session(CreateSessionRequest(name=f"projection-{i}"))
      await threads.create_thread(session, f"thread {i}")

  asyncio.run(seed())

  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_session_manager] = lambda: sessions
  app.dependency_overrides[get_thread_manager] = lambda: ThreadManager(cfg)
  app.dependency_overrides[get_trigger_manager] = lambda: TriggerManager(cfg, sessions)
  apply_config_overrides(app, cfg)
  client = TestClient(app)

  walks = _count_walks(monkeypatch)
  first = client.get("/api/sessions/")
  assert first.status_code == 200
  leaves = [row for row in first.json() if row.get("profile") == "worker"]
  assert len(leaves) == 12
  cold_walks = walks["n"]
  assert cold_walks >= 12  # one source walk per legacy session, cold

  for _ in range(9):
    assert client.get("/api/sessions/").status_code == 200
  assert walks["n"] == cold_walks  # served from the view-rows memo; nothing re-walked

  client.get("/api/sessions/")
  assert walks["n"] > cold_walks  # every session's tenth poll sweeps once


def test_session_view_rows_match_the_list_rows_order(tmp_path: Path) -> None:
  """The view's rows are the list's thread rows: same fields, newest-first."""
  client, session_id, _ = _seeded_client(tmp_path)

  list_rows = [row for row in client.get(f"/api/threads/{session_id}/list").json() if row["type"] == "thread"]
  view_rows = client.get(f"/api/sessions/{session_id}/view").json()["threads"]

  assert view_rows == list_rows


def test_list_body_memo_invalidates_on_metadata_rewrite(tmp_path: Path) -> None:
  client, session_id, _ = _seeded_client(tmp_path)
  url = f"/api/threads/{session_id}/list"

  first = client.get(url)
  second = client.get(url)
  assert second.content == first.content

  rows = {row["id"]: row for row in first.json()}
  any_id = next(iter(rows))
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends=fake_backends())
  asyncio.run(ThreadManager(cfg).update_status(session_id, any_id, ThreadStatus.RUNNING))

  third = client.get(url)
  assert third.json()[next(i for i, row in enumerate(third.json()) if row["id"] == any_id)]["status"] == "running"
  assert third.content != first.content


def test_list_poll_repeating_the_rendered_etag_gets_a_bodyless_204(tmp_path: Path) -> None:
  client, session_id, _ = _seeded_client(tmp_path)
  url = f"/api/threads/{session_id}/list"

  first = client.get(url)
  etag = first.headers["ETag"]
  assert first.headers["Cache-Control"] == "no-store"
  conditional = client.get(url, params={"etag": etag})
  assert conditional.status_code == 204
  assert conditional.content == b""
  assert conditional.headers["ETag"] == etag

  # A signature move publishes a new body and tag; the stale tag re-serves 200.
  rows = {row["id"]: row for row in first.json()}
  any_id = next(iter(rows))
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends=fake_backends())
  asyncio.run(ThreadManager(cfg).update_status(session_id, any_id, ThreadStatus.RUNNING))
  stale = client.get(url, params={"etag": etag})
  assert stale.status_code == 200
  assert stale.content != first.content
  assert stale.headers["ETag"] != etag
  assert client.get(url, params={"etag": stale.headers["ETag"]}).status_code == 204


def test_marked_rebuild_reuses_rows_and_parses_from_one_walk(tmp_path: Path) -> None:
  """A marked rebuild rebuilds only the moved file's row, and the rows parse from
  the walked pairs (the signature and the rows describe one file instant)."""
  client, session_id, _ = _seeded_client(tmp_path)
  url = f"/api/threads/{session_id}/list"

  first = client.get(url)
  rows_first = {row["id"]: row for row in first.json()}
  any_id = next(iter(rows_first))

  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends=fake_backends())
  thread_mgr = ThreadManager(cfg)
  asyncio.run(thread_mgr.update_status(session_id, any_id, ThreadStatus.RUNNING))

  second = client.get(url)
  rows_second = {row["id"]: row for row in second.json()}
  assert rows_second[any_id]["status"] == "running"
  assert second.content != first.content
  # Every row is byte-identical to the whole-parse rows (the memo's identity is
  # the parse memo's), and the moved row is the only one rebuilt.
  assert rows_second.keys() == rows_first.keys()
  for tid, row in rows_second.items():
    if tid == any_id:
      assert row != rows_first[tid]
    else:
      assert row == rows_first[tid]

  # The stored row dicts are the ones the next rebuild serves: after another
  # mark, the unmoved files' rows are the same objects in the memo, the moved
  # file's row is a fresh dict.
  rows_objects = threads_api._thread_row_memo.get(session_id)
  assert rows_objects is not None
  stored = {v[2]["id"]: v[2] for v in rows_objects.values()}
  asyncio.run(thread_mgr.update_status(session_id, any_id, ThreadStatus.COMPLETED))
  third = client.get(url)
  assert next(row for row in third.json() if row["id"] == any_id)["status"] == "completed"
  rows_objects_third = threads_api._thread_row_memo.get(session_id)
  assert rows_objects_third is not None
  stored_third = {v[2]["id"]: v[2] for v in rows_objects_third.values()}
  for tid, row in stored_third.items():
    if tid == any_id:
      assert row is not stored[tid]
    else:
      assert row is stored[tid]


def test_list_rows_ship_epoch_ms_timestamps(tmp_path: Path) -> None:
  """Row timestamps ride the epoch-ms wire form the client's new Date() reads."""
  client, session_id, long_thread_id = _seeded_client(tmp_path)

  rows = {row["id"]: row for row in client.get(f"/api/threads/{session_id}/list").json()}
  detail = client.get(f"/api/threads/{session_id}/threads/{long_thread_id}").json()

  created = datetime.fromisoformat(detail["created_at"])
  assert isinstance(rows[long_thread_id]["created_at"], int)
  assert rows[long_thread_id]["created_at"] == int(created.timestamp() * 1000)
  assert rows[long_thread_id]["completed_at"] is None


def test_list_body_sorts_thread_and_trigger_rows_by_one_epoch_ms_key(tmp_path: Path) -> None:
  """The mixed body sort compares int against int: both row kinds convert their timestamps."""
  cfg, session_id, threads_dir = _seeded_thread_dir(tmp_path, "mixed-sort", "the thread row")
  mgr = ThreadManager(cfg)
  pairs = list(core_threads.iter_thread_meta_stats(str(threads_dir)))
  metas = mgr.list_threads_from_stats(iter(pairs), str(threads_dir))
  thread_item, thread_fragment = threads_api._thread_list_items(session_id, pairs, metas)[0]
  trigger = PendingTrigger(
      session_id=session_id,
      fire_at=datetime.now(UTC) + timedelta(hours=1),
      message="the trigger row",
      watch_targets=[],
  )

  body = json.loads(threads_api._list_body([(thread_item, thread_fragment)], [trigger]))

  stamps = [row["created_at"] for row in body]
  assert stamps == sorted(stamps, reverse=True)
  assert all(isinstance(stamp, int) for stamp in stamps)
  trigger_row = next(row for row in body if row["type"] == "trigger")
  assert isinstance(trigger_row["fire_at"], int)
  assert trigger_row["fire_at"] == int(trigger.fire_at.timestamp() * 1000)


def test_list_body_splice_matches_whole_dump() -> None:
  """The joined per-row fragments are byte-identical to the whole-array dump they replaced.

  The splice is only safe because the encoder's per-element text is context-free;
  this pins that property for the list body's options across the row shapes the
  payload carries (nested dicts, None, unicode, float timestamps, escapes).
  """
  rows: list[dict] = [
      {
          "type": "thread",
          "id": "t1",
          "description": "plain ascii",
          "status": "running",
          "created_at": 1700,
          "completed_at": None,
          "backend": "cc-claude"
      },
      {
          "type": "thread",
          "id": "t2",
          "description": "üñïçødé 与中文 \"quoted\" back\\slash",
          "status": "completed",
          "created_at": 1699.5,
          "completed_at": 1701,
          "backend": "opencode",
          "description_full_len": 42
      },
      {
          "type": "trigger",
          "id": "tr1",
          "message": "multi\nline\ttab",
          "status": "pending",
          "fire_at": 1702,
          "created_at": 1702
      },
      {
          "type": "thread",
          "id": "t3",
          "description": "",
          "status": "pending",
          "created_at": 1698,
          "completed_at": None,
          "backend": None
      },
      {
          "type": "thread",
          "id": "t4",
          "description": "nested",
          "status": "running",
          "created_at": 1697,
          "completed_at": None,
          "backend": "cc-claude",
          "extra": {
              "watch": ["a", "b"],
              "deep": {
                  "k": [1, {
                      "x": None
                  }]
              }
          }
      },
  ]
  pairs = [(row, threads_api._row_fragment(row)) for row in rows]
  trigger = PendingTrigger(
      session_id="s", fire_at=datetime.now(UTC) + timedelta(hours=1), message="the trigger row", watch_targets=[])
  spliced = threads_api._list_body(pairs, [trigger])
  dicts = [pair[0] for pair in pairs] + [threads_api._trigger_list_item(trigger)]
  dicts.sort(key=lambda row: row["created_at"], reverse=True)
  whole = json.dumps(dicts, ensure_ascii=False, allow_nan=False, separators=(",", ":")).encode("utf-8")
  assert spliced == whole


def test_rebuild_tolerates_file_vanished_between_walk_and_read(tmp_path: Path) -> None:
  """A pair the walk statted but whose file vanished before its read parses as no row, not a crash."""
  cfg, session_id, threads_dir = _seeded_thread_dir(tmp_path, "vanished", "survivor")
  mgr = ThreadManager(cfg)
  pairs = list(core_threads.iter_thread_meta_stats(str(threads_dir)))
  # A pair from the signature's walk whose file the session GC removed before
  # the rebuild's parse-merge read it.
  stale = [*pairs, (str(threads_dir / "vanished" / "metadata.json"), pairs[0][1])]

  metas = mgr.list_threads_from_stats(stale, str(threads_dir))
  assert metas[1] is None
  items = threads_api._thread_list_items(session_id, stale, metas)
  assert len(items) == 1
  assert {t.id for t in mgr._metas_from_stats(iter(pairs), str(threads_dir))} == {t.id for t in metas[:1]}


def test_list_threads_from_stats_matches_list_threads(tmp_path: Path) -> None:
  """The shared parse-merge serves the same metas from pre-walked pairs as from its own scan."""
  cfg, session_id, threads_dir = _seeded_thread_dir(tmp_path, "from-stats", "one", "two")
  mgr = ThreadManager(cfg)
  scanned = asyncio.run(mgr.list_threads(session_id))

  pairs = list(core_threads.iter_thread_meta_stats(str(threads_dir)))
  from_stats = mgr.list_threads_from_stats(pairs, str(threads_dir))

  assert {t.id for t in from_stats} == {t.id for t in scanned}
  assert {t.description for t in from_stats} == {"one", "two"}


def test_marked_incremental_body_matches_the_full_walk_body(tmp_path: Path) -> None:
  """The marked poll's spliced body is byte-identical to the full walk's, tag included."""
  client, session_id, _ = _seeded_client(tmp_path)
  url = f"/api/threads/{session_id}/list"

  client.get(url)
  rows = {row["id"]: row for row in client.get(url).json()}
  any_id = next(iter(rows))
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends=fake_backends())
  asyncio.run(ThreadManager(cfg).update_status(session_id, any_id, ThreadStatus.RUNNING))

  incremental = client.get(url)
  assert next(row for row in incremental.json() if row["id"] == any_id)["status"] == "running"

  sidebar_state.mark_sidebar_dirty(session_id)  # path-less mark: the poll full-walks
  full = client.get(url)
  assert full.content == incremental.content
  assert full.headers["ETag"] == incremental.headers["ETag"]


def test_marked_vanished_file_drops_the_row(tmp_path: Path) -> None:
  """A mark whose file vanished between the mark and the poll drops the row, and the full walk agrees."""
  client, session_id, _ = _seeded_client(tmp_path)
  url = f"/api/threads/{session_id}/list"
  rows = {row["id"]: row for row in client.get(url).json()}
  thread_id = next(iter(rows))
  meta_path = next(
      candidate for candidate in (tmp_path / "home" / "sessions" / session_id / "threads").glob("*/metadata.json")
      if thread_id in str(candidate))
  meta_path.unlink()
  sidebar_state.mark_sidebar_dirty(session_id, str(meta_path))

  incremental = client.get(url)
  assert {row["id"] for row in incremental.json()} == set(rows) - {thread_id}

  sidebar_state.mark_sidebar_dirty(session_id)  # path-less mark: the poll full-walks
  assert client.get(url).content == incremental.content


def test_sweep_survives_continuous_marked_polls(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Incremental proofs advance the sweep countdown: a full walk still lands within the 10-poll window."""
  client, session_id, _ = _seeded_client(tmp_path)
  url = f"/api/threads/{session_id}/list"

  walks = _count_walks(monkeypatch)
  client.get(url)
  rows = {row["id"] for row in client.get(url).json()}
  any_id = next(iter(rows))

  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends=fake_backends())
  thread_mgr = ThreadManager(cfg)
  for _ in range(10):
    asyncio.run(thread_mgr.update_status(session_id, any_id, ThreadStatus.RUNNING))
    client.get(url)  # marked poll: incremental proof unless the sweep is due
    if walks["n"] == 2:
      break
  assert walks["n"] == 2, "no full walk within 10 continuously marked polls"


def test_list_gzip_ships_precompressed_body(tmp_path: Path) -> None:
  """The 3 s poll's gzip form rides the body-keyed memo: the served bytes
  decompress to the plain body, the tag names the plain render, and the
  conditional 204 stays bodyless under a gzip-accepting client."""
  client, session_id, _ = _seeded_client(tmp_path)
  url = f"/api/threads/{session_id}/list"

  gz = client.get(url)
  plain = client.get(url, headers={"accept-encoding": "identity"})
  assert_gzip_served(gz)
  assert gz.headers["ETag"] == plain.headers["ETag"]
  assert gz.json() == plain.json()

  conditional = client.get(url, params={"etag": plain.headers["ETag"]})
  assert conditional.status_code == 204
  assert conditional.content == b""
  assert conditional.headers["ETag"] == plain.headers["ETag"]


def test_list_gzip_repeat_serves_memo_without_recompress(tmp_path: Path) -> None:
  """A repeat poll of the same body serves the memo's bytes and re-compresses nothing."""
  client, session_id, _ = _seeded_client(tmp_path)
  url = f"/api/threads/{session_id}/list"

  first = client.get(url)

  with patch(RESPONSES_GZIP_LEVEL1_PATCH_TARGET, gzip_explode_compress("repeat list poll re-ran the deflate")):
    second = client.get(url)
  assert second.headers["content-encoding"] == "gzip"
  assert second.content == first.content
  assert len(threads_api._list_gzip_memo) == 1


def test_list_gzip_changed_body_recompresses(tmp_path: Path) -> None:
  """A row-source rewrite changes the body: the next gzip poll compresses that
  body once and its decompressed bytes carry the new status."""
  client, session_id, _ = _seeded_client(tmp_path)
  url = f"/api/threads/{session_id}/list"

  first = client.get(url)
  rows = {row["id"] for row in first.json()}
  any_id = next(iter(rows))
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends=fake_backends())
  asyncio.run(ThreadManager(cfg).update_status(session_id, any_id, ThreadStatus.RUNNING))

  second = client.get(url)
  assert second.headers["content-encoding"] == "gzip"
  plain = client.get(url, headers={"accept-encoding": "identity"})
  assert second.content == plain.content
  assert next(row for row in plain.json() if row["id"] == any_id)["status"] == "running"


def test_list_plain_request_stays_uncompressed(tmp_path: Path) -> None:
  """A client sending no Accept-Encoding reads the plain render, and the gzip
  memo gains no entry."""
  client, session_id, _ = _seeded_client(tmp_path)
  url = f"/api/threads/{session_id}/list"

  plain = client.get(url, headers={"accept-encoding": "identity"})
  assert "content-encoding" not in plain.headers
  assert len(threads_api._list_gzip_memo) == 0
