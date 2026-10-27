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

import pathlib

import conftest
import pytest


def _write_bound_task(home: pathlib.Path, name: str, session_id: str, *, enabled: bool = True) -> None:
  """One synthetic cron task bound to *session_id* (the join's input shape).

  A cron.d host file carries ``prompt_file`` — the pointer to the file owning
  the prompt body — never an inline prompt (the loader rejects that shape).
  """
  prompt_path = home / "prompts" / f"{name}.md"
  prompt_path.parent.mkdir(parents=True, exist_ok=True)
  prompt_path.write_text(f"synthetic prompt for {name}\n", encoding="utf-8")
  path = conftest.cron_d_dir(home) / f"{name}.yaml"
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(
      conftest.dump_yaml(
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


@pytest.mark.asyncio
async def test_join_answer_repeats_until_the_snapshot_or_a_served_fire_moves(
    tmp_path: pathlib.Path, temp_home: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The one-entry memo serves the stored map while the question is unchanged.

  Same id set and fingerprint serves the stored dicts whole (the hit the
  sidebar lists' repeat polls pay); a new id set re-derives; a cron config
  change re-derives through the fingerprint even inside the served fire
  window; a now past a served fire re-derives with the next occurrence.
  """
  import datetime
  import types

  from src.features.cron import sequence_controller

  controller = sequence_controller.CronSequenceController()

  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  bound = await conftest.create_task(tree, parent=None, request_id="bind-1", profile="manager", name="Bound")
  other = await conftest.create_task(tree, parent=None, request_id="plain-1", profile="manager", name="Plain")
  _write_bound_task(temp_home, "synthetic-daily", bound.id)
  ids = (bound.id, other.id)
  now = datetime.datetime.now(datetime.UTC)

  first = controller.listing_fields(ids, now)
  _assert_bound_row(first[bound.id], "synthetic-daily", enabled=True)
  second = controller.listing_fields(ids, now + datetime.timedelta(seconds=1))
  assert second == first
  assert second[other.id] is first[other.id]  # the stored map served whole

  assert set(controller.listing_fields((other.id,), now)) == {other.id}

  _write_bound_task(temp_home, "synthetic-second", other.id)
  rebound = controller.listing_fields(ids, now + datetime.timedelta(seconds=1))
  assert rebound[other.id]["schedule_task"] == "synthetic-second"
  assert rebound[bound.id] == first[bound.id]

  # A served fire passing re-derives with the next occurrence: the shift pins
  # the clock next_run_iso computes from (datetime.now under the task's
  # timezone) 5 minutes past the served fire, so the crossing holds at any
  # wall-clock run instant.
  fire = datetime.datetime.fromisoformat(first[bound.id]["schedule_next_run"])
  crossed = fire + datetime.timedelta(minutes=5)

  class _ShiftedDateTime(datetime.datetime):

    @classmethod
    def now(cls, tz=None):
      return crossed.astimezone(tz)

  # sequence_controller qualifies through the datetime module, so the stand-in binds the
  # module attribute to a namespace carrying the fake class.
  monkeypatch.setattr(
      "src.features.cron.sequence_controller.datetime", types.SimpleNamespace(datetime=_ShiftedDateTime))
  advanced = controller.listing_fields(ids, crossed)
  assert datetime.datetime.fromisoformat(advanced[bound.id]["schedule_next_run"]) > fire
