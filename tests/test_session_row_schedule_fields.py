"""The sidebar lists' row schedule fields (plan 4.1) and the archived page's context rows.

One controller method (keyed on the loaded task binding) stamps every
listed row — GET /api/sessions/, /starred, /archived, and the homepage's
server-rendered sidebar: a node a loaded task binds carries ``schedule_task``
plus the four schedule fields computed from that task config; an unbound row
carries ``schedule_task: null`` and none of the four. The Scheduled listing and
its endpoint are gone (404). Each archived page also carries its rows'
unarchived ancestors as ``context_only`` rows — never counted into page size,
cursor, or group aggregates, each archived row returned exactly once across a
full walk — and the cron-subtree exclusion still keeps the synthetic scheduled
manager node and everything under it out.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import (
    OPERATOR,
    OPUS_BACKEND_ID,
    build_env,
    create_root_session,
    create_task,
    cron_d_dir,
    dump_yaml,
    make_cron_session,
    make_sessions_listing_client,
    make_sessions_listing_page_client,
    page_initial_sessions,
    walk_archived_pages,
)

from src.infra.models import CreateSessionRequest, RunRecord, SessionStatus


def _write_bound_task(home: Path, name: str, session_id: str, *, enabled: bool = True) -> None:
  """One synthetic cron task bound to *session_id* (the join's input shape).

  A cron.d host file carries ``prompt_file`` — the pointer to the file owning
  the prompt body — never an inline prompt (the loader rejects that shape).
  """
  prompt_path = home / "prompts" / f"{name}.md"
  prompt_path.parent.mkdir(parents=True, exist_ok=True)
  prompt_path.write_text(f"synthetic prompt for {name}\n", encoding="utf-8")
  path = cron_d_dir(home) / f"{name}.yaml"
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(
      dump_yaml(
          {
              "cron": "0 3 * * *",
              "prompt_file": str(prompt_path),
              "timezone": "America/Los_Angeles",
              "enabled": enabled,
              "session_id": session_id,
          }),
      encoding="utf-8")


def _assert_bound_row(row: dict, task_name: str, *, enabled: bool) -> None:
  """The join's full answer for one bound node's row."""
  assert row["schedule_task"] == task_name
  assert row["schedule_cron"] == "0 3 * * *"
  assert row["schedule_timezone"] == "America/Los_Angeles"
  assert row["schedule_enabled"] is enabled
  assert row["schedule_next_run"]  # ISO 8601, computed the way the Scheduled rows computed it


def _assert_unbound_row(row: dict) -> None:
  """schedule_task null and none of the four schedule fields."""
  assert row["schedule_task"] is None
  for field in ("schedule_cron", "schedule_timezone", "schedule_enabled", "schedule_next_run"):
    assert field not in row, field


@pytest.mark.asyncio
async def test_bound_and_unbound_rows_carry_the_join_answer_in_every_list(tmp_path: Path, temp_home: Path) -> None:
  cfg, session_blocks, tree = build_env(tmp_path)

  async def manager(name: str, request_id: str, group: str | None = None):
    node = await create_task(tree, parent=None, request_id=request_id, profile="manager", name=name)
    if group is not None:
      node.group = group
      await session_blocks.store.save_metadata(node)
    return node

  bound = await manager("synthetic-daily", "bind-1", "Synthetic")
  disabled_node = await manager("synthetic-paused", "bind-2", "Synthetic")
  unbound = await manager("Plain root", "plain-1", "Synthetic")
  _write_bound_task(temp_home, "synthetic-daily", bound.id)
  _write_bound_task(temp_home, "synthetic-paused", disabled_node.id, enabled=False)
  client = make_sessions_listing_client(cfg, session_blocks, tree)

  for url in ("/api/sessions/", "/api/sessions/starred"):
    if url.endswith("starred"):
      for node in (bound, disabled_node, unbound):
        await session_blocks.lifecycle.star_session(node.id)
    resp = client.get(url)
    assert resp.status_code == 200, (url, resp.text)
    by_id = {row["id"]: row for row in resp.json()}
    assert {bound.id, disabled_node.id, unbound.id} <= set(by_id), url
    _assert_bound_row(by_id[bound.id], "synthetic-daily", enabled=True)
    _assert_bound_row(by_id[disabled_node.id], "synthetic-paused", enabled=False)
    _assert_unbound_row(by_id[unbound.id])

  # The homepage's server-rendered sidebar carries the same answer.
  rows = {
      row["id"]: row
      for row in page_initial_sessions(make_sessions_listing_page_client(cfg, session_blocks, tree), unbound.id)
  }
  _assert_bound_row(rows[bound.id], "synthetic-daily", enabled=True)
  _assert_bound_row(rows[disabled_node.id], "synthetic-paused", enabled=False)
  _assert_unbound_row(rows[unbound.id])


@pytest.mark.asyncio
async def test_join_answer_repeats_until_the_snapshot_or_a_served_fire_moves(
    tmp_path: Path, temp_home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The one-entry memo serves the stored map while the question is unchanged.

  Same id set and fingerprint serves the stored dicts whole (the hit the
  sidebar lists' repeat polls pay); a new id set re-derives; a cron config
  change re-derives through the fingerprint even inside the served fire
  window; a now past a served fire re-derives with the next occurrence.
  """
  from datetime import UTC, datetime, timedelta

  from src.features.cron.sequence_controller import CronSequenceController

  controller = CronSequenceController()

  _cfg, _session_blocks, tree = build_env(tmp_path)
  bound = await create_task(tree, parent=None, request_id="bind-1", profile="manager", name="Bound")
  other = await create_task(tree, parent=None, request_id="plain-1", profile="manager", name="Plain")
  _write_bound_task(temp_home, "synthetic-daily", bound.id)
  ids = (bound.id, other.id)
  now = datetime.now(UTC)

  first = controller.listing_fields(ids, now)
  _assert_bound_row(first[bound.id], "synthetic-daily", enabled=True)
  second = controller.listing_fields(ids, now + timedelta(seconds=1))
  assert second == first
  assert second[other.id] is first[other.id]  # the stored map served whole

  assert set(controller.listing_fields((other.id,), now)) == {other.id}

  _write_bound_task(temp_home, "synthetic-second", other.id)
  rebound = controller.listing_fields(ids, now + timedelta(seconds=1))
  assert rebound[other.id]["schedule_task"] == "synthetic-second"
  assert rebound[bound.id] == first[bound.id]

  # A served fire passing re-derives with the next occurrence: the shift pins
  # the clock next_run_iso computes from (datetime.now under the task's
  # timezone) 5 minutes past the served fire, so the crossing holds at any
  # wall-clock run instant.
  fire = datetime.fromisoformat(first[bound.id]["schedule_next_run"])
  crossed = fire + timedelta(minutes=5)

  class _ShiftedDateTime(datetime):

    @classmethod
    def now(cls, tz=None):
      return crossed.astimezone(tz)

  monkeypatch.setattr("src.features.cron.sequence_controller.datetime", _ShiftedDateTime)
  advanced = controller.listing_fields(ids, crossed)
  assert datetime.fromisoformat(advanced[bound.id]["schedule_next_run"]) > fire


@pytest.mark.asyncio
async def test_archived_bound_row_keeps_the_join_and_the_scheduled_endpoint_is_gone(
    tmp_path: Path, temp_home: Path) -> None:
  cfg, session_blocks, tree = build_env(tmp_path)
  bound = await create_task(tree, parent=None, request_id="bind-1", profile="manager", name="synthetic-daily")
  _write_bound_task(temp_home, "synthetic-daily", bound.id)
  await session_blocks.lifecycle.archive_session(bound.id)
  client = make_sessions_listing_client(cfg, session_blocks, tree)

  resp = client.get("/api/sessions/archived")
  assert resp.status_code == 200
  page = resp.json()
  row = next(r for r in page["sessions"] if r["id"] == bound.id)
  _assert_bound_row(row, "synthetic-daily", enabled=True)
  assert "context_only" not in row  # the mark exists only on context rows

  # The Scheduled listing and its endpoint are deleted.
  assert client.get("/api/sessions/scheduled").status_code == 404


@pytest.mark.asyncio
async def test_archived_pages_carry_active_ancestors_as_context_only_rows(tmp_path: Path) -> None:
  cfg, session_blocks, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root-1", profile="manager", name="Synthetic root")
  root.group = "Alpha"
  await session_blocks.store.save_metadata(root)
  # An archived intermediate (its own close fact): it is itself an archived
  # row, and the walk climbs past it to the active root.
  hidden = await create_task(
      tree, parent=root.id, request_id="mid-1", profile="manager", name="Synthetic archived middle")
  delivered = await create_task(
      tree, parent=hidden.id, request_id="leaf-1", profile="worker", name="Synthetic delivered")
  await tree.runs.register_run(RunRecord(id="run-1", session_id=delivered.id, kind="work"))
  await tree.dispatch.finish_run(delivered.id, "run-1", outcome="success")
  assert tree.task_state(delivered.id) == "completed"  # an end state of its own
  from conftest import OPERATOR
  await tree.archive_subtree(hidden.id, caller=OPERATOR)  # delivered is not open; it stays completed

  # An archived child directly under the active root.
  archived_child = await create_task(
      tree, parent=root.id, request_id="archived-child", profile="manager", name="Synthetic archived child")
  await tree.archive_subtree(archived_child.id, caller=OPERATOR)

  # Fillers push the archived rows past one small page.
  fillers = []
  for i in range(3):
    filler = await create_root_session(
        session_blocks, CreateSessionRequest(name=f"Filler {i}"), backend=OPUS_BACKEND_ID)
    await session_blocks.lifecycle.archive_session(filler.id)
    fillers.append(filler)

  client = make_sessions_listing_client(cfg, session_blocks, tree)
  archived: list[str] = []
  context: list[str] = []
  pages = walk_archived_pages(client)
  for page in pages:
    rows = page["sessions"]
    real = [r for r in rows if not r.get("context_only")]
    # Page size stays an archived-row count: a page never fills with context.
    if page["has_more"]:
      assert len(real) == 2
    archived.extend(r["id"] for r in real)
    page_context = [r["id"] for r in rows if r.get("context_only")]
    # One page never repeats a context row; across pages one repeats (the
    # server cannot see the client's merged tree), and the client dedups by id.
    assert len(page_context) == len(set(page_context))
    for row in rows:
      if row.get("context_only"):
        assert row["status"] != SessionStatus.ARCHIVED
    context.extend(page_context)

  # Every archived row exactly once — the walk's cursor never repeats a row.
  assert len(archived) == len(set(archived))
  assert set(archived) == {delivered.id, hidden.id, archived_child.id, *(f.id for f in fillers)}
  # The active ancestors ride as context_only rows: the root for every branch,
  # climbed past the derived-archived intermediate.
  assert set(context) == {root.id}
  # The aggregates describe the archived rows alone.
  assert pages[-1]["groups"] == [{"group": None, "total": 6}]


@pytest.mark.asyncio
async def test_archived_context_walk_keeps_the_cron_subtree_out(tmp_path: Path, temp_home: Path) -> None:
  cfg, session_blocks, tree = build_env(tmp_path)
  cron = await make_cron_session(session_blocks, "synthetic-cron")
  active_root = await create_task(tree, parent=None, request_id="root-1", profile="manager", name="Synthetic root")
  firing = await create_task(
      tree, parent=cron.id, request_id="firing-1", profile="worker", name="synthetic-cron · firing-1")
  await session_blocks.lifecycle.archive_session(firing.id)
  # An archived row under the ACTIVE root: the walk climbs to the root, while
  # the firing's chain reaches the cron session, so it never becomes a page row.
  delivered = await create_task(
      tree, parent=active_root.id, request_id="leaf-2", profile="worker", name="Synthetic delivered")
  await tree.runs.register_run(RunRecord(id="run-2", session_id=delivered.id, kind="work"))
  await tree.dispatch.finish_run(delivered.id, "run-2", outcome="success")

  client = make_sessions_listing_client(cfg, session_blocks, tree)
  seen: dict[str, dict] = {}
  for page in walk_archived_pages(client):
    for row in page["sessions"]:
      seen[row["id"]] = row

  # The cron-subtree exclusion holds: neither the firing nor its cron session
  # rides any page, as an archived row or as context.
  assert firing.id not in seen and cron.id not in seen
  assert delivered.id in seen and "context_only" not in seen[delivered.id]
  context_rows = [row for row in seen.values() if row.get("context_only")]
  assert [row["id"] for row in context_rows] == [active_root.id]
  # The join answers on context rows too: the root is unbound here.
  assert context_rows[0]["schedule_task"] is None
