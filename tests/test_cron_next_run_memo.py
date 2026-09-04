"""Tests for the /scheduled next-run memo: hit, expiry, and key separation."""

from datetime import UTC, datetime

import pytest

import src.api.cron as cron_api
from src.api.cron import next_run_iso


@pytest.fixture(autouse=True)
def clear_next_run_memo():
  cron_api._NEXT_RUN_MEMO.clear()
  yield
  cron_api._NEXT_RUN_MEMO.clear()


def counting_croniter(monkeypatch) -> list:
  """Swap croniter for a counting wrapper around the real one; return the call count list."""
  real = cron_api.croniter
  calls = []

  def wrapped(*args, **kwargs):
    calls.append(1)
    return real(*args, **kwargs)

  monkeypatch.setattr(cron_api, "croniter", wrapped)
  return calls


def test_repeat_call_serves_the_memo(monkeypatch) -> None:
  calls = counting_croniter(monkeypatch)
  now = datetime.now(UTC)
  first = next_run_iso("17 3 * * 2", "America/Los_Angeles", now)
  second = next_run_iso("17 3 * * 2", "America/Los_Angeles", now)
  assert first == second
  assert len(calls) == 1


def test_expired_entry_recomputes(monkeypatch) -> None:
  calls = counting_croniter(monkeypatch)
  fake_now = [datetime(2026, 1, 5, 12, 0, 30, tzinfo=UTC)]  # a Monday

  class FakeDatetime(datetime):

    @classmethod
    def now(cls, tz=None):
      return fake_now[0] if tz is None else fake_now[0].astimezone(tz)

  monkeypatch.setattr(cron_api, "datetime", FakeDatetime)
  first = next_run_iso("*/5 * * * *", "UTC", fake_now[0])
  assert first == "2026-01-05T12:05:00+00:00"
  assert len(calls) == 1
  # Still before the memoized fire time: the hit must serve without a recompute.
  again = next_run_iso("*/5 * * * *", "UTC", datetime(2026, 1, 5, 12, 4, tzinfo=UTC))
  assert again == first
  assert len(calls) == 1
  # Past the fire time: the stale answer must drop and the next one compute.
  fake_now[0] = datetime(2026, 1, 5, 12, 5, 30, tzinfo=UTC)
  later = next_run_iso("*/5 * * * *", "UTC", fake_now[0])
  assert later == "2026-01-05T12:10:00+00:00"
  assert len(calls) == 2


def test_keys_separate_by_cron_and_timezone(monkeypatch) -> None:
  calls = counting_croniter(monkeypatch)
  now = datetime.now(UTC)
  a = next_run_iso("17 3 * * 2", "America/Los_Angeles", now)
  b = next_run_iso("17 3 * * 2", "UTC", now)
  c = next_run_iso("18 3 * * 2", "America/Los_Angeles", now)
  assert len(calls) == 3
  assert a != b  # LA is never UTC
  assert a != c  # different minute
