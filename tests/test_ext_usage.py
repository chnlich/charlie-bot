import asyncio
import json
import os
import subprocess
import time
import types
from collections.abc import Callable
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest
from conftest import codex_token_count_event, fresh_state_fixture

from src.api import ext_usage as ext_usage_mod
from src.api.ext_usage import (
    CLAUDE_DEFAULT_DIR,
    CODEX_DEFAULT_DIR,
    ClaudeUsageProvider,
    CodexUsageProvider,
    _derive_accounts,
    _extract_codex_spend_events,
    _latest_token_count_event,
    _list_rollout_files,
    _poll_loop,
    _read_credentials,
    _sum_codex_spend_events,
    _transform_codex_response,
    _transform_response,
)
from src.core.config import CharlieBotConfig
from src.core.models import ClaudeAccount

_fresh_unknown_limit_shape_registry = fresh_state_fixture(ext_usage_mod._reset_unknown_limit_shapes_for_tests)
_fresh_credential_read_warning_registry = fresh_state_fixture(ext_usage_mod._reset_credential_read_warnings_for_tests)
_fresh_usage_cache = fresh_state_fixture(ext_usage_mod._cached_usage.clear)
_fresh_user_agent_cache = fresh_state_fixture(ext_usage_mod._reset_user_agent_for_tests)


def _build_token_count_event(
    *,
    timestamp: str,
    primary_used_percent: float,
    primary_resets_at: int,
    secondary_used_percent: float,
    secondary_resets_at: int,
) -> dict:
  """A legacy two-window Codex payload: a 5h primary plus a 7d secondary."""
  return codex_token_count_event(
      timestamp,
      rate_limits={
          "primary": {
              "used_percent": primary_used_percent,
              "window_minutes": 300,
              "resets_at": primary_resets_at,
          },
          "secondary":
              {
                  "used_percent": secondary_used_percent,
                  "window_minutes": 10080,
                  "resets_at": secondary_resets_at,
              },
      })


def _build_weekly_token_count_event(
    *,
    timestamp: str,
    used_percent: float,
    resets_at: int,
) -> dict:
  """The shape Codex reports today: one weekly window in the primary slot."""
  return codex_token_count_event(
      timestamp,
      rate_limits={
          "primary": {
              "used_percent": used_percent,
              "window_minutes": 10080,
              "resets_at": resets_at,
          },
          "secondary": None,
      })


def _build_turn_context_event(model: str) -> dict:
  """The turn_context event whose payload.model prices the token_count events after it."""
  return {"type": "turn_context", "payload": {"model": model}}


def _build_spend_token_count_event(
    *,
    timestamp: Any,
    input_tokens: Any,
    cached_input_tokens: Any,
    output_tokens: Any,
) -> dict:
  """A per-turn token_count event as the spend aggregation reads it.

  ``payload.info.last_token_usage`` is the block ``_extract_codex_spend_events``
  prices. Every field is ``Any`` so malformed-row probes pass bad values
  through verbatim.
  """
  return codex_token_count_event(
      timestamp,
      info={
          "last_token_usage":
              {
                  "input_tokens": input_tokens,
                  "cached_input_tokens": cached_input_tokens,
                  "output_tokens": output_tokens,
              }
      })


def _token_count_line(now: datetime, primary_used_percent: float) -> str:
  """One jsonl line: a legacy token_count event stamped *now*, resets 1h/1d out, 2% secondary."""
  return json.dumps(
      _build_token_count_event(
          timestamp=now.isoformat().replace("+00:00", "Z"),
          primary_used_percent=primary_used_percent,
          primary_resets_at=int(now.timestamp()) + 3600,
          secondary_used_percent=2.0,
          secondary_resets_at=int(now.timestamp()) + 86400,
      ))


def _write_live_quota_rollout(rollout_dir: Path, now: datetime) -> None:
  """Seed rollout-live.jsonl: one legacy quota event timestamped *now* (8% primary, 2% secondary).

  The utime pins the file's mtime to *now* — the provider's live-file selection reads mtimes.
  """
  live_rollout_path = rollout_dir / "rollout-live.jsonl"
  live_rollout_path.write_text(_token_count_line(now, 8.0) + "\n")
  os.utime(live_rollout_path, (now.timestamp(), now.timestamp()))


def test_codex_usage_transform_adds_token_count_observed_at() -> None:
  fetched_at = "2026-03-27T18:30:00+00:00"
  lines = [
      json.dumps(
          _build_token_count_event(
              timestamp="2026-03-27T18:29:35.694Z",
              primary_used_percent=8.0,
              primary_resets_at=1774653423,
              secondary_used_percent=2.0,
              secondary_resets_at=1775240223,
          ))
  ]

  event = _latest_token_count_event(lines)
  assert event is not None
  usage = _transform_codex_response(event, fetched_at=fetched_at)

  assert usage == {
      "windows":
          [
              {
                  "window_minutes": 300,
                  "utilization": 8.0,
                  "resets_at": datetime.fromtimestamp(1774653423, tz=UTC).isoformat(),
              },
              {
                  "window_minutes": 10080,
                  "utilization": 2.0,
                  "resets_at": datetime.fromtimestamp(1775240223, tz=UTC).isoformat(),
              },
          ],
      "fetched_at": fetched_at,
      "provider": "codex",
      "token_count_observed_at": "2026-03-27T18:29:35.694Z",
  }
  assert "rate_limits_state" not in usage


def test_codex_usage_transform_uses_the_latest_token_count_event() -> None:
  lines = [
      json.dumps(
          _build_token_count_event(
              timestamp="2026-03-27T17:00:00Z",
              primary_used_percent=12.0,
              primary_resets_at=1774650000,
              secondary_used_percent=4.0,
              secondary_resets_at=1775240000,
          )),
      json.dumps(
          _build_token_count_event(
              timestamp="2026-03-27T18:00:00Z",
              primary_used_percent=18.0,
              primary_resets_at=1774653600,
              secondary_used_percent=6.0,
              secondary_resets_at=1775243600,
          )),
      "{not valid json",
      json.dumps(
          {
              "timestamp": "2026-03-27T18:05:00Z",
              "type": "event_msg",
              "payload": {
                  "type": "agent_message",
                  "message": "still not usage"
              },
          }),
  ]

  event = _latest_token_count_event(lines)
  assert event is not None
  usage = _transform_codex_response(event, fetched_at="2026-03-27T18:10:00+00:00")

  assert [w["utilization"] for w in usage["windows"]] == [18.0, 6.0]
  assert [w["window_minutes"] for w in usage["windows"]] == [300, 10080]
  assert usage["token_count_observed_at"] == "2026-03-27T18:00:00Z"


# One row per raw rate_limits payload shape a Codex token_count event can
# carry, and the usage dict the transform must report for it: business
# metadata states the unlimited state, metadata-less null buckets stay
# unstated, an unidentifiable slot is dropped rather than guessed at from
# slot order, and a window with no reported usage is unknown, not zero. The
# credits rows pin the metered balance reading: forwarded as a JSON number,
# stateless, and shaped strictly (an unreadable credits object is dropped
# with a warning, never guessed at).
_CODEX_RATE_LIMIT_SHAPE_ROWS = [
    pytest.param(
        {
            "primary": None,
            "secondary": None,
            "credits": {
                "unlimited": True,
            },
            "plan_type": "business",
        }, {
            "windows": [],
            "credits": {
                "unlimited": True,
            },
            "fetched_at": "2026-03-27T18:40:00+00:00",
            "provider": "codex",
            "rate_limits_state": "business-unlimited",
            "token_count_observed_at": "2026-03-27T18:39:35.694Z",
        },
        id="null-buckets-business-unlimited"),
    pytest.param(
        {
            "primary": None,
            "secondary": None,
        }, {
            "windows": [],
            "fetched_at": "2026-03-27T18:40:00+00:00",
            "provider": "codex",
            "token_count_observed_at": "2026-03-27T18:39:35.694Z",
        },
        id="null-buckets-without-metadata-no-state"),
    pytest.param(
        {
            "primary": None,
            "secondary": None,
            "plan_type": "business",
        }, {
            "windows": [],
            "fetched_at": "2026-03-27T18:40:00+00:00",
            "provider": "codex",
            "rate_limits_state": "business-unlimited",
            "token_count_observed_at": "2026-03-27T18:39:35.694Z",
        },
        id="null-buckets-plan-type-fallback-without-credits"),
    pytest.param(
        {
            "primary": None,
            "secondary": None,
            "credits": {
                "has_credits": True,
                "unlimited": False,
                "balance": "29779.358283042908",
            },
            "plan_type": "business",
        }, {
            "windows": [],
            "credits": {
                "unlimited": False,
                "balance": pytest.approx(29779.358),
            },
            "fetched_at": "2026-03-27T18:40:00+00:00",
            "provider": "codex",
            "token_count_observed_at": "2026-03-27T18:39:35.694Z",
        },
        id="metered-credits-balance"),
    pytest.param(
        {
            "primary": None,
            "secondary": None,
            "credits": {
                "unlimited": False,
            },
        }, {
            "windows": [],
            "credits": {
                "unlimited": False,
            },
            "fetched_at": "2026-03-27T18:40:00+00:00",
            "provider": "codex",
            "token_count_observed_at": "2026-03-27T18:39:35.694Z",
        },
        id="metered-credits-without-balance"),
    pytest.param(
        {
            "primary": None,
            "secondary": None,
            "credits": "yes",
            "plan_type": "business",
        }, {
            "windows": [],
            "fetched_at": "2026-03-27T18:40:00+00:00",
            "provider": "codex",
            "token_count_observed_at": "2026-03-27T18:39:35.694Z",
        },
        id="credits-not-an-object-dropped"),
    pytest.param(
        {
            "primary": None,
            "secondary": None,
            "credits": {
                "unlimited": "false",
            },
            "plan_type": "business",
        }, {
            "windows": [],
            "fetched_at": "2026-03-27T18:40:00+00:00",
            "provider": "codex",
            "token_count_observed_at": "2026-03-27T18:39:35.694Z",
        },
        id="credits-unlimited-not-a-bool-dropped"),
    pytest.param(
        {
            "primary": {
                "used_percent": 96.0,
                "resets_at": 1785016000,
            },
            "secondary": None,
        }, {
            "windows": [],
            "fetched_at": "2026-03-27T18:40:00+00:00",
            "provider": "codex",
            "token_count_observed_at": "2026-03-27T18:39:35.694Z",
        },
        id="slot-without-window-minutes-dropped"),
    pytest.param(
        {
            "primary": {
                "window_minutes": 10080,
                "resets_at": 1785016000,
            },
            "secondary": None,
        }, {
            "windows":
                [
                    {
                        "window_minutes": 10080,
                        "utilization": None,
                        "resets_at": datetime.fromtimestamp(1785016000, tz=UTC).isoformat(),
                    }
                ],
            "fetched_at": "2026-03-27T18:40:00+00:00",
            "provider": "codex",
            "token_count_observed_at": "2026-03-27T18:39:35.694Z",
        },
        id="window-without-used-percent-unknown"),
]


@pytest.mark.parametrize(("rate_limits", "expected_usage"), _CODEX_RATE_LIMIT_SHAPE_ROWS)
def test_codex_usage_transform_raw_rate_limit_shapes(rate_limits: dict, expected_usage: dict) -> None:
  lines = [json.dumps(codex_token_count_event("2026-03-27T18:39:35.694Z", rate_limits=rate_limits))]

  event = _latest_token_count_event(lines)
  assert event is not None
  usage = _transform_codex_response(event, fetched_at="2026-03-27T18:40:00+00:00")

  assert usage == expected_usage


def test_codex_usage_transform_drops_unreadable_credits_and_warns(monkeypatch) -> None:
  """An unreadable credits shape emits no payload and no state, and warns instead.

  ``plan_type == "business"`` rides along in every case to prove a present
  credits key blocks the unlimited fallback even when the credits object
  itself cannot be read.
  """
  warns: list[dict] = []
  monkeypatch.setattr(ext_usage_mod.log, "warning", lambda event, **kw: warns.append({"event": event, **kw}))

  def transform(credits: Any) -> dict:
    rate_limits = {"primary": None, "secondary": None, "plan_type": "business", "credits": credits}
    event = codex_token_count_event("2026-03-27T18:39:35.694Z", rate_limits=rate_limits)
    return _transform_codex_response(event, fetched_at="2026-03-27T18:40:00+00:00")

  for unreadable in ("yes", {"unlimited": "false"}):
    usage = transform(unreadable)
    assert "credits" not in usage
    assert "rate_limits_state" not in usage
  metered = transform({"unlimited": False, "balance": "n/a"})
  assert metered["credits"] == {"unlimited": False}
  assert "rate_limits_state" not in metered

  reasons = [w["reason"] for w in warns if w["event"] == "ext_usage_unknown_limit_shape"]
  assert reasons == ["credits is not an object", "unlimited is not a bool", "missing or unparseable balance"]


def test_spend_aggregation_prices_recent_turns_by_model(tmp_path) -> None:
  now = datetime(2026, 6, 1, 12, 0, 0, tzinfo=UTC)
  rollout_path = tmp_path / "rollout-recent.jsonl"

  events = [
      _build_turn_context_event(model="gpt-5.5"),
      _build_spend_token_count_event(
          timestamp=(now - timedelta(hours=2)).isoformat().replace("+00:00", "Z"),
          input_tokens=1_000_000,
          cached_input_tokens=100_000,
          output_tokens=10_000),
      _build_spend_token_count_event(
          timestamp=(now - timedelta(days=2)).isoformat().replace("+00:00", "Z"),
          input_tokens=2_000_000,
          cached_input_tokens=500_000,
          output_tokens=20_000),
      _build_spend_token_count_event(
          timestamp=(now - timedelta(days=8)).isoformat().replace("+00:00", "Z"),
          input_tokens=5_000_000,
          cached_input_tokens=0,
          output_tokens=100_000),
  ]
  rollout_path.write_text("\n".join(json.dumps(event) for event in events) + "\n")

  extracted = _extract_codex_spend_events(rollout_path)
  assert extracted is not None
  spend = _sum_codex_spend_events([extracted], now=now)

  assert spend["last_24h_usd"] == pytest.approx(4.85)
  assert spend["last_7d_usd"] == pytest.approx(13.20)


@pytest.mark.asyncio
async def test_codex_provider_fetch_keeps_quota_when_historical_spend_row_is_malformed(tmp_path,) -> None:
  # The provider reads rollout logs from <home_dir>/sessions, so the test seeds
  # that subtree and constructs the instance with home_dir pointing at tmp_path.
  provider = CodexUsageProvider(label="main", home_dir=str(tmp_path))
  now, rollout_dir = _seed_rollout_dir(tmp_path)

  stale_rollout_path = rollout_dir / "rollout-stale.jsonl"
  stale_rollout_path.write_text("{not valid json\n")
  stale_mtime = now.timestamp() - 60
  os.utime(stale_rollout_path, (stale_mtime, stale_mtime))

  _write_live_quota_rollout(rollout_dir, now)

  usage = await provider.fetch()

  assert usage is not None
  assert [w["utilization"] for w in usage["windows"]] == [8.0, 2.0]
  assert usage["spend"] == {
      "last_24h_usd": 0.0,
      "last_7d_usd": 0.0,
  }


@pytest.mark.asyncio
async def test_codex_provider_fetch_returns_quota_when_spend_aggregations_raises(
    tmp_path,
    monkeypatch,
) -> None:
  provider = CodexUsageProvider(label="main", home_dir=str(tmp_path))
  now, rollout_dir = _seed_rollout_dir(tmp_path)

  _write_live_quota_rollout(rollout_dir, now)

  def _broken_compute(self, rollout_paths) -> None:
    raise RuntimeError("simulated spend failure")

  monkeypatch.setattr(CodexUsageProvider, "_compute_spend", _broken_compute)

  usage = await provider.fetch()

  assert usage is not None
  assert [w["utilization"] for w in usage["windows"]] == [8.0, 2.0]
  assert usage["spend"] is None


@pytest.mark.asyncio
async def test_codex_provider_spend_reparses_only_changed_files(tmp_path, monkeypatch) -> None:
  """Steady-state rounds reuse parsed spend events; only a file with a new (mtime, size) re-parses."""
  provider = CodexUsageProvider(label="main", home_dir=str(tmp_path))
  now, rollout_dir = _seed_rollout_dir(tmp_path)

  def spend_event(input_tokens: int) -> str:
    return json.dumps(
        _build_spend_token_count_event(
            timestamp=now.isoformat(), input_tokens=input_tokens, cached_input_tokens=0, output_tokens=0))

  model_line = json.dumps(_build_turn_context_event(model="gpt-5.3-codex"))
  first = rollout_dir / "rollout-first.jsonl"
  second = rollout_dir / "rollout-second.jsonl"
  first.write_text(model_line + "\n" + spend_event(1000) + "\n")
  second.write_text(model_line + "\n" + spend_event(3000) + "\n")

  extracted: list[str] = []
  real_extract = ext_usage_mod._extract_codex_spend_events

  def _counting_extract(path):
    extracted.append(path.name)
    return real_extract(path)

  monkeypatch.setattr(ext_usage_mod, "_extract_codex_spend_events", _counting_extract)

  first_usage = await provider.fetch()
  assert sorted(extracted) == ["rollout-first.jsonl", "rollout-second.jsonl"]

  extracted.clear()
  with first.open("a") as stream:
    stream.write(spend_event(2000) + "\n")
  reapplied_usage = await provider.fetch()

  assert extracted == ["rollout-first.jsonl"]
  assert reapplied_usage is not None
  # 6000 input tokens priced after the append beat the 4000 before it: the
  # changed file's re-parse adds its new events, the cached file's do not double-count.
  assert reapplied_usage["spend"]["last_7d_usd"] > first_usage["spend"]["last_7d_usd"]


def _seed_rollout_dir(tmp_path: Path) -> tuple[datetime, Path]:
  """Create <tmp>/sessions/YYYY/MM/DD dated today, the subtree the provider reads."""
  now = datetime.now(UTC)
  rollout_dir = tmp_path / "sessions" / f"{now.year:04d}" / f"{now.month:02d}" / f"{now.day:02d}"
  rollout_dir.mkdir(parents=True)
  return now, rollout_dir


def _filler_lines(byte_floor: int) -> list[str]:
  """Valid non-token_count JSONL lines (silent on both the usage and spend scans)
  totalling at least *byte_floor* bytes."""
  lines: list[str] = []
  size = 0
  while size < byte_floor:
    line = json.dumps({"type": "event_msg", "payload": {"type": "thinking_delta", "pad": "x" * 180}})
    lines.append(line)
    size += len(line) + 1
  return lines


def _counting_scan(monkeypatch) -> list[int]:
  """Record the line count of every _latest_token_count_event call."""
  scanned: list[int] = []
  real_scan = ext_usage_mod._latest_token_count_event

  def _wrapped(lines, match=None):
    scanned.append(len(lines))
    return real_scan(lines, match)

  monkeypatch.setattr(ext_usage_mod, "_latest_token_count_event", _wrapped)
  return scanned


@pytest.mark.asyncio
async def test_codex_provider_usage_scrape_skips_read_for_unchanged_file(tmp_path, monkeypatch) -> None:
  """Steady-state rounds serve the memoized event with no file read; an append re-reads."""
  provider = CodexUsageProvider(label="main", home_dir=str(tmp_path))
  now, rollout_dir = _seed_rollout_dir(tmp_path)
  rollout_path = rollout_dir / "rollout-live.jsonl"

  rollout_path.write_text(_token_count_line(now, 8.0) + "\n")
  scanned = _counting_scan(monkeypatch)

  first = await provider.fetch()
  assert scanned == [1]
  assert [w["utilization"] for w in first["windows"]] == [8.0, 2.0]

  again = await provider.fetch()
  assert scanned == [1]
  assert [w["utilization"] for w in again["windows"]] == [8.0, 2.0]

  with rollout_path.open("a") as stream:
    stream.write(_token_count_line(now, 9.0) + "\n")
  moved = await provider.fetch()
  assert scanned == [1, 2]
  assert [w["utilization"] for w in moved["windows"]] == [9.0, 2.0]


@pytest.mark.asyncio
async def test_codex_provider_usage_scrape_reads_tail_only_on_hit(tmp_path, monkeypatch) -> None:
  """When the tail window holds a token_count, the scan never sees the full file."""
  provider = CodexUsageProvider(label="main", home_dir=str(tmp_path))
  now, rollout_dir = _seed_rollout_dir(tmp_path)
  rollout_path = rollout_dir / "rollout-big.jsonl"

  filler = _filler_lines(ext_usage_mod._USAGE_TAIL_BYTES * 2)
  rollout_path.write_text("\n".join([_token_count_line(now, 99.0), *filler, _token_count_line(now, 8.0)]) + "\n")
  full_lines = len(filler) + 2
  scanned = _counting_scan(monkeypatch)

  usage = await provider.fetch()

  assert [w["utilization"] for w in usage["windows"]] == [8.0, 2.0]
  assert len(scanned) == 1
  assert scanned[0] < full_lines


@pytest.mark.asyncio
async def test_codex_provider_usage_scrape_full_read_on_tail_miss(tmp_path, monkeypatch) -> None:
  """A tail window without a token_count falls back to the full-file read."""
  provider = CodexUsageProvider(label="main", home_dir=str(tmp_path))
  now, rollout_dir = _seed_rollout_dir(tmp_path)
  rollout_path = rollout_dir / "rollout-stale.jsonl"

  event_line = _token_count_line(now, 99.0)
  filler = _filler_lines(ext_usage_mod._USAGE_TAIL_BYTES * 2)
  rollout_path.write_text("\n".join([event_line, *filler]) + "\n")
  scanned = _counting_scan(monkeypatch)

  usage = await provider.fetch()

  assert [w["utilization"] for w in usage["windows"]] == [99.0, 2.0]
  assert len(scanned) == 2
  assert scanned[1] > scanned[0]


def _iso_z(moment: datetime) -> str:
  return moment.isoformat().replace("+00:00", "Z")


def _plan_quota_line(moment: datetime, used_percent: float) -> str:
  """One jsonl line: a plan-pool (limit_id codex) token_count event stamped *moment*."""
  event = _build_weekly_token_count_event(
      timestamp=_iso_z(moment), used_percent=used_percent, resets_at=int(moment.timestamp()) + 86400)
  event["payload"]["rate_limits"]["limit_id"] = "codex"
  event["payload"]["rate_limits"]["limit_name"] = None
  return json.dumps(event)


def _model_pool_line(moment: datetime) -> str:
  """One jsonl line: a model-level (spark) pool token_count event stamped *moment*."""
  return json.dumps(
      codex_token_count_event(
          _iso_z(moment),
          rate_limits={
              "limit_id": "codex_bengalfox",
              "limit_name": "GPT-5.3-Codex-Spark",
              "primary": {
                  "used_percent": 0.0,
                  "window_minutes": 300,
                  "resets_at": int(moment.timestamp()) + 3600,
              },
              "secondary": {
                  "used_percent": 0.0,
                  "window_minutes": 10080,
                  "resets_at": int(moment.timestamp()) + 86400,
              },
          }))


@pytest.mark.asyncio
async def test_codex_provider_fetch_picks_freshest_plan_event_across_recent_files(tmp_path) -> None:
  """The in-flight file holds the newest mtime but an old plan event; the finished file's fresher event wins."""
  provider = CodexUsageProvider(label="main", home_dir=str(tmp_path))
  now, rollout_dir = _seed_rollout_dir(tmp_path)

  inflight = rollout_dir / "rollout-inflight.jsonl"
  inflight.write_text(_plan_quota_line(now - timedelta(minutes=61), 6.0) + "\n")
  os.utime(inflight, (now.timestamp(), now.timestamp()))
  finished_event_at = now - timedelta(seconds=6)
  finished = rollout_dir / "rollout-finished.jsonl"
  finished.write_text(_plan_quota_line(finished_event_at, 18.0) + "\n")
  os.utime(finished, (finished_event_at.timestamp(), finished_event_at.timestamp()))

  usage = await provider.fetch()

  assert usage is not None
  assert usage["windows"]
  assert usage["windows"][0]["utilization"] == 18.0
  assert usage["token_count_observed_at"] == _iso_z(finished_event_at)


@pytest.mark.asyncio
async def test_codex_provider_fetch_keeps_older_file_inside_scan_window(tmp_path) -> None:
  """Every file written inside the window counts; an outside file's fresher event never contributes."""
  provider = CodexUsageProvider(label="main", home_dir=str(tmp_path))
  now, rollout_dir = _seed_rollout_dir(tmp_path)
  window = timedelta(hours=ext_usage_mod._CODEX_USAGE_SCAN_WINDOW_HOURS)

  inside_event_at = now - timedelta(minutes=10)
  inside = rollout_dir / "rollout-inside.jsonl"
  inside.write_text(_plan_quota_line(inside_event_at, 30.0) + "\n")
  inside_mtime = (now - window + timedelta(minutes=5)).timestamp()
  os.utime(inside, (inside_mtime, inside_mtime))
  outside = rollout_dir / "rollout-outside.jsonl"
  outside.write_text(_plan_quota_line(now - timedelta(minutes=1), 90.0) + "\n")
  outside_mtime = (now - window - timedelta(minutes=5)).timestamp()
  os.utime(outside, (outside_mtime, outside_mtime))

  usage = await provider.fetch()

  assert usage is not None
  assert usage["windows"][0]["utilization"] == 30.0
  assert usage["token_count_observed_at"] == _iso_z(inside_event_at)


@pytest.mark.asyncio
async def test_codex_provider_fetch_falls_back_to_single_newest_file_past_scan_window(tmp_path) -> None:
  """All files past the window: the newest-mtime file alone yields its event, fresher events elsewhere are ignored."""
  provider = CodexUsageProvider(label="main", home_dir=str(tmp_path))
  now, rollout_dir = _seed_rollout_dir(tmp_path)

  newest_event_at = now - timedelta(days=2)
  newest = rollout_dir / "rollout-newest.jsonl"
  newest.write_text(_plan_quota_line(newest_event_at, 96.0) + "\n")
  os.utime(newest, (newest_event_at.timestamp(), newest_event_at.timestamp()))
  older = rollout_dir / "rollout-older.jsonl"
  older.write_text(_plan_quota_line(now - timedelta(days=1), 40.0) + "\n")
  older_mtime = (now - timedelta(days=3)).timestamp()
  os.utime(older, (older_mtime, older_mtime))

  usage = await provider.fetch()

  assert usage is not None
  assert usage["windows"][0]["utilization"] == 96.0
  assert usage["token_count_observed_at"] == _iso_z(newest_event_at)


@pytest.mark.asyncio
async def test_codex_provider_fetch_reports_no_plan_reading_when_scan_set_is_model_only(tmp_path) -> None:
  """A scan set with only model-level events is a no-reading state, never an empty-windows payload."""
  provider = CodexUsageProvider(label="main", home_dir=str(tmp_path))
  now, rollout_dir = _seed_rollout_dir(tmp_path)

  spark_only = rollout_dir / "rollout-spark.jsonl"
  spark_only.write_text(_model_pool_line(now) + "\n")
  os.utime(spark_only, (now.timestamp(), now.timestamp()))

  assert await provider.fetch() is None
  assert provider.last_error == "no plan-quota reading in 6h"


@pytest.mark.asyncio
async def test_codex_provider_fetch_finds_plan_event_buried_under_model_events(tmp_path) -> None:
  """A mid-session model switch leaves newer model-level lines above the file's last plan event."""
  provider = CodexUsageProvider(label="main", home_dir=str(tmp_path))
  now, rollout_dir = _seed_rollout_dir(tmp_path)

  plan_event_at = now - timedelta(minutes=10)
  rollout_path = rollout_dir / "rollout-switched.jsonl"
  rollout_path.write_text(
      _plan_quota_line(plan_event_at, 12.0) + "\n" + _model_pool_line(now - timedelta(minutes=5)) + "\n")
  os.utime(rollout_path, (now.timestamp(), now.timestamp()))

  usage = await provider.fetch()

  assert usage is not None
  assert usage["windows"][0]["utilization"] == 12.0
  assert usage["token_count_observed_at"] == _iso_z(plan_event_at)


def test_spend_aggregation_skips_bad_rows_without_poisoning_totals(tmp_path) -> None:
  now = datetime(2026, 6, 1, 12, 0, 0, tzinfo=UTC)
  rollout_path = tmp_path / "rollout-mixed.jsonl"

  events = [
      _build_turn_context_event(model="gpt-5.5"),
      _build_spend_token_count_event(
          timestamp=(now - timedelta(hours=2)).isoformat().replace("+00:00", "Z"),
          input_tokens=1_000_000,
          cached_input_tokens=100_000,
          output_tokens=10_000),
      _build_spend_token_count_event(
          timestamp=(now - timedelta(hours=2)).isoformat().replace("+00:00", "Z"),
          input_tokens="bad",
          cached_input_tokens=0,
          output_tokens=0),
      _build_spend_token_count_event(
          timestamp=(now - timedelta(hours=2)).isoformat().replace("+00:00", "Z"),
          input_tokens=500_000,
          cached_input_tokens=None,
          output_tokens=5_000),
      _build_spend_token_count_event(timestamp=None, input_tokens=1_000_000, cached_input_tokens=0, output_tokens=0),
      _build_spend_token_count_event(timestamp=12345, input_tokens=1_000_000, cached_input_tokens=0, output_tokens=0),
      codex_token_count_event((now - timedelta(hours=2)).isoformat().replace("+00:00", "Z"), info={}),
      _build_spend_token_count_event(
          timestamp=(now - timedelta(hours=2)).isoformat().replace("+00:00", "Z"),
          input_tokens=500_000,
          cached_input_tokens=50_000,
          output_tokens=5_000),
  ]
  rollout_path.write_text("\n".join(json.dumps(event) if isinstance(event, dict) else event for event in events) + "\n")

  extracted = _extract_codex_spend_events(rollout_path)
  assert extracted is not None
  spend = _sum_codex_spend_events([extracted], now=now)

  assert spend["last_24h_usd"] == pytest.approx(7.275)
  assert spend["last_7d_usd"] == pytest.approx(7.275)


def test_spend_aggregation_skips_unreadable_file(tmp_path) -> None:
  now = datetime(2026, 6, 1, 12, 0, 0, tzinfo=UTC)

  unreadable_path = tmp_path / "rollout-unreadable.jsonl"
  unreadable_path.write_text("should not be read\n")
  os.chmod(unreadable_path, 0o000)

  readable_path = tmp_path / "rollout-readable.jsonl"
  events = [
      _build_turn_context_event(model="gpt-5.5"),
      _build_spend_token_count_event(
          timestamp=now.isoformat().replace("+00:00", "Z"),
          input_tokens=1_000_000,
          cached_input_tokens=0,
          output_tokens=0),
  ]
  readable_path.write_text("\n".join(json.dumps(event) for event in events) + "\n")

  try:
    assert _extract_codex_spend_events(unreadable_path) is None
    extracted = _extract_codex_spend_events(readable_path)
    assert extracted is not None
    spend = _sum_codex_spend_events([extracted], now=now)
  finally:
    os.chmod(unreadable_path, 0o644)

  assert spend["last_24h_usd"] == pytest.approx(5.00)
  assert spend["last_7d_usd"] == pytest.approx(5.00)


def test_codex_provider_spend_prunes_files_untouched_for_a_week(tmp_path) -> None:
  """A fresh-looking event inside a stale-mtime file stays unpriced.

  Rollout logs are append-only, so a file untouched for seven days is skipped
  without a read, even though its events would pass the aggregation window on
  their own timestamps.
  """
  provider = CodexUsageProvider(label="main", home_dir=str(tmp_path))
  now = datetime.now(UTC)
  rollout_path = tmp_path / "rollout-stale.jsonl"
  rollout_path.write_text(
      "\n".join(
          [
              json.dumps(_build_turn_context_event(model="gpt-5.5")),
              json.dumps(
                  _build_spend_token_count_event(
                      timestamp=now.isoformat().replace("+00:00", "Z"),
                      input_tokens=1_000_000,
                      cached_input_tokens=0,
                      output_tokens=0)),
          ]) + "\n")
  stale_mtime = (now - timedelta(days=8)).timestamp()
  os.utime(rollout_path, (stale_mtime, stale_mtime))

  spend = provider._compute_spend([rollout_path])

  assert spend == {"last_24h_usd": 0.0, "last_7d_usd": 0.0}


def test_transform_response_preserves_claude_payload_shape() -> None:
  usage = _transform_response(
      {
          "fiveHour": {
              "utilization": 42.0,
              "resetsAt": "2026-03-27T20:00:00+00:00",
          },
          "sevenDay": {
              "utilization": 10.0,
              "resetsAt": "2026-04-01T20:00:00+00:00",
          },
      })

  assert usage["windows"] == [
      {
          "window_minutes": 300,
          "utilization": 42.0,
          "resets_at": "2026-03-27T20:00:00+00:00",
      },
      {
          "window_minutes": 10080,
          "utilization": 10.0,
          "resets_at": "2026-04-01T20:00:00+00:00",
      },
  ]
  assert usage["provider"] == "claude"
  assert "token_count_observed_at" not in usage


# ---------------------------------------------------------------------------
# Account-set derivation (T2)
# ---------------------------------------------------------------------------


def test_derive_accounts_always_includes_defaults_and_dedupes_explicit_default(monkeypatch) -> None:
  cfg = CharlieBotConfig(
      accounts={
          "claude":
              [
                  ClaudeAccount(label="main", config_dir=CLAUDE_DEFAULT_DIR),
                  ClaudeAccount(label="ext-1", config_dir="~/.claude-invite-1"),
              ]
      })
  monkeypatch.setattr(ext_usage_mod, "get_config", lambda: cfg)

  accounts = _derive_accounts()

  assert accounts["claude"][0] == ("main", CLAUDE_DEFAULT_DIR)
  assert accounts["codex"][0] == ("main", CODEX_DEFAULT_DIR)
  assert [label for label, _ in accounts["claude"]] == ["main", "ext-1"]
  assert [label for label, _ in accounts["codex"]] == ["main"]
  assert accounts["pool"] == {
      "main": CLAUDE_DEFAULT_DIR,
      "ext-1": os.path.abspath(os.path.expanduser("~/.claude-invite-1"))
  }


def test_derive_accounts_label_collision_skip_fail_loud(monkeypatch) -> None:
  # Two distinct dirs both labelled "invite-1": the later one is skipped (logged)
  # rather than overwriting the first.
  cfg = CharlieBotConfig(
      accounts={
          "claude":
              [
                  ClaudeAccount(label="invite-1", config_dir="~/.claude-invite-1"),
                  ClaudeAccount(label="invite-1", config_dir="~/accounts/invite-1"),
              ]
      })
  monkeypatch.setattr(ext_usage_mod, "get_config", lambda: cfg)

  labels = [label for label, _ in _derive_accounts()["claude"]]

  assert labels == ["main", "invite-1"]


# ---------------------------------------------------------------------------
# Poller payload semantics (T3): multi-account keys, stale-keep, error
# placeholder, drop-on-removal. Drives the real _poll_loop for N cycles with
# monkeypatched derivation/providers and a sleep that stops the loop.
# ---------------------------------------------------------------------------


class _StopAfter(BaseException):
  """Control-flow signal that pierces the poller's broad ``except Exception``.

  The round-robin loop sleeps inside its ``try`` block, so a plain ``Exception``
  stop signal would be swallowed by the loop's error handler and the test would
  hang. ``BaseException`` (like ``asyncio.CancelledError``) propagates out.
  """


class _FakeProvider:

  def __init__(self, get_value: Callable[[], Any], error: str = "no data") -> None:
    self._get_value = get_value
    self.last_error = error

  async def fetch(self) -> dict | None:
    value = self._get_value()
    if isinstance(value, Exception):
      raise value
    return value


def _run_poll_cycles(monkeypatch, *, accounts_fn, create_provider, n: int) -> dict:
  """Drive the real ``_poll_loop`` for ``n`` round-gap sleeps, then return counters.

  Under round-robin scheduling one sleep == one single-account fetch, so a full
  round of N accounts takes N sleeps (plus one sleep per empty round).
  """
  state: dict = {"sleeps": 0, "broadcasts": 0, "payloads": []}

  async def _fake_sleep(_) -> None:
    state["sleeps"] += 1
    if state["sleeps"] >= n:
      raise _StopAfter

  async def _track_broadcast(_channel, event) -> None:
    state["broadcasts"] += 1
    state["payloads"].append(event)

  monkeypatch.setattr(asyncio, "sleep", _fake_sleep)
  monkeypatch.setattr(ext_usage_mod, "streaming_manager", types.SimpleNamespace(broadcast=_track_broadcast))
  monkeypatch.setattr(ext_usage_mod, "_derive_accounts", accounts_fn)
  monkeypatch.setattr(ext_usage_mod, "_create_provider", create_provider)
  ext_usage_mod._cached_usage.clear()
  ext_usage_mod._instances.clear()

  with pytest.raises(_StopAfter):
    asyncio.run(_poll_loop())

  return state


def _claude_fetch_value(utilization: float) -> dict:
  """A claude poll fetch result: one 300-minute window at the given utilization plus
  fetched_at/provider metadata. Fresh dict per call, so no two tests share a windows list."""
  return {
      "windows": [{
          "window_minutes": 300,
          "utilization": utilization,
          "resets_at": ""
      }],
      "fetched_at": "2026-01-01T00:00:00+00:00",
      "provider": "claude",
  }


def test_poll_multi_account_keys_and_error_placeholder_for_never_fetched(monkeypatch) -> None:
  main_value = _claude_fetch_value(42.0)

  def create_provider(provider, label, dir_path):
    if label == "main":
      return _FakeProvider(lambda: main_value)
    return _FakeProvider(lambda: None, error="credentials not found")

  accounts = {"claude": [("main", "/fake/main"), ("invite-1", "/fake/invite-1")], "codex": []}
  # Round-robin: one fetch per sleep, so a round of 2 accounts spans 2 sleeps.
  state = _run_poll_cycles(monkeypatch, accounts_fn=lambda: accounts, create_provider=create_provider, n=2)

  assert set(ext_usage_mod._cached_usage.keys()) == {"claude:main", "claude:invite-1"}
  main = ext_usage_mod._cached_usage["claude:main"]
  assert main["provider"] == "claude"
  assert main["account"] == "main"
  assert main["windows"][0]["utilization"] == 42.0
  assert ext_usage_mod._cached_usage["claude:invite-1"] == {
      "provider": "claude",
      "account": "invite-1",
      "error": "credentials not found",
  }
  # Each fetch broadcasts once; 2 fetches -> 2 broadcasts (not 1 per round).
  assert state["broadcasts"] == 2


def test_poll_stale_keep_on_fetch_failure(monkeypatch) -> None:
  fetch_no = {"i": 0}
  original = _claude_fetch_value(42.0)

  def create_provider(provider, label, dir_path):

    def get_value():
      fetch_no["i"] += 1
      return original if fetch_no["i"] == 1 else None

    return _FakeProvider(get_value, error="rate limited")

  accounts = {"claude": [("main", "/fake/main")], "codex": []}
  # 1 account per round, so 2 sleeps == 2 rounds == 2 fetches of the same account.
  state = _run_poll_cycles(monkeypatch, accounts_fn=lambda: accounts, create_provider=create_provider, n=2)

  kept = ext_usage_mod._cached_usage["claude:main"]
  assert kept["windows"][0]["utilization"] == 42.0
  assert kept["fetched_at"] == "2026-01-01T00:00:00+00:00"
  assert "error" not in kept
  assert state["broadcasts"] == 2


def test_poll_drops_removed_account_on_next_rebuild(monkeypatch) -> None:
  fetch_no = {"i": 0}

  def create_provider(provider, label, dir_path):

    def get_value():
      fetch_no["i"] += 1
      return _claude_fetch_value(float(fetch_no["i"]))

    return _FakeProvider(get_value)

  call = {"i": 0}
  accounts_by_cycle = {
      1: {
          "claude": [("main", "/fake/main"), ("invite-1", "/fake/invite-1")],
          "codex": []
      },
      2: {
          "claude": [("main", "/fake/main")],
          "codex": []
      },
  }

  def accounts_fn():
    call["i"] += 1
    return accounts_by_cycle[call["i"]]

  # Round 1 spans 2 fetches (2 sleeps); round 2 re-derives to 1 account, prunes
  # the dropped key, then fetches once (3rd sleep) before stopping.
  _run_poll_cycles(monkeypatch, accounts_fn=accounts_fn, create_provider=create_provider, n=3)

  assert set(ext_usage_mod._cached_usage.keys()) == {"claude:main"}


# ---------------------------------------------------------------------------
# Round-robin scheduling (T4): per-account fetch order, per-fetch broadcast,
# cache pruning at the round boundary, and the empty-round guard.
# ---------------------------------------------------------------------------


def test_poll_round_robin_fetches_accounts_in_derivation_order(monkeypatch) -> None:
  fetch_order: list[str] = []

  def create_provider(provider, label, dir_path):

    def get_value():
      fetch_order.append(label)
      return _claude_fetch_value(1.0)

    return _FakeProvider(get_value)

  accounts = {
      "claude": [("main", "/fake/main"), ("invite-1", "/fake/invite-1"), ("invite-2", "/fake/invite-2")],
      "codex": [],
  }
  # One full round of 3 accounts == 3 fetches == 3 sleeps.
  state = _run_poll_cycles(monkeypatch, accounts_fn=lambda: accounts, create_provider=create_provider, n=3)

  assert fetch_order == ["main", "invite-1", "invite-2"]
  assert state["sleeps"] == 3
  assert state["broadcasts"] == 3


def test_poll_broadcasts_once_per_fetch_not_per_round(monkeypatch) -> None:

  def create_provider(provider, label, dir_path):
    return _FakeProvider(lambda: _claude_fetch_value(1.0))

  accounts = {"claude": [("main", "/fake/main"), ("invite-1", "/fake/invite-1")], "codex": []}
  derive_count = {"i": 0}

  def accounts_fn():
    derive_count["i"] += 1
    return accounts

  # 2 accounts x 2 rounds == 4 fetches == 4 sleeps; 2 derivations (rounds).
  state = _run_poll_cycles(monkeypatch, accounts_fn=accounts_fn, create_provider=create_provider, n=4)

  # Broadcast count tracks fetches, not rounds.
  assert state["broadcasts"] == 4
  assert derive_count["i"] == 2
  assert state["broadcasts"] != derive_count["i"]


def test_poll_prunes_removed_account_cache_key_at_round_boundary(monkeypatch) -> None:
  good = _claude_fetch_value(7.0)
  main_calls = {"i": 0}

  def main_get():
    main_calls["i"] += 1
    return good if main_calls["i"] == 1 else None

  def create_provider(provider, label, dir_path):
    if label == "main":
      return _FakeProvider(main_get, error="rate limited")
    return _FakeProvider(lambda: good)

  call = {"i": 0}
  accounts_by_cycle = {
      1: {
          "claude": [("main", "/fake/main"), ("a", "/fake/a")],
          "codex": []
      },
      2: {
          "claude": [("main", "/fake/main")],
          "codex": []
      },
  }

  def accounts_fn():
    call["i"] += 1
    return accounts_by_cycle[call["i"]]

  # Round 1: main + a both fetch good values (sleeps 1, 2). Round 2 re-derives to
  # just main, prunes "claude:a" at the boundary, then fetches main (None ->
  # stale-keep) on the 3rd sleep.
  _run_poll_cycles(monkeypatch, accounts_fn=accounts_fn, create_provider=create_provider, n=3)

  # Removed account "a" is pruned even though it held a good (non-error) entry.
  assert set(ext_usage_mod._cached_usage.keys()) == {"claude:main"}
  kept = ext_usage_mod._cached_usage["claude:main"]
  assert kept["windows"][0]["utilization"] == 7.0
  assert "error" not in kept


def test_poll_empty_round_guard_sleeps_once_before_rederiving(monkeypatch) -> None:

  def create_provider(provider, label, dir_path):
    return _FakeProvider(dict)

  derive_count = {"i": 0}

  def accounts_fn():
    derive_count["i"] += 1
    return {"claude": [], "codex": []}

  # An empty derived round must sleep once (not busy-spin) before re-deriving.
  state = _run_poll_cycles(monkeypatch, accounts_fn=accounts_fn, create_provider=create_provider, n=2)

  assert derive_count["i"] == 2
  assert state["sleeps"] == 2
  assert state["broadcasts"] == 0


def test_poll_outer_exception_still_backs_off_before_retrying(monkeypatch) -> None:
  """A non-fetch exception (e.g. from ``_derive_accounts``) must hit a backoff
  sleep, not spin the outer loop with no await point."""
  good = _claude_fetch_value(1.0)
  accounts = {"claude": [("main", "/fake/main")], "codex": []}
  derive_count = {"i": 0}

  def accounts_fn():
    derive_count["i"] += 1
    if derive_count["i"] == 1:
      raise RuntimeError("boom")
    return accounts

  def create_provider(provider, label, dir_path):
    return _FakeProvider(lambda: good)

  # If the outer except swallowed the exception without sleeping, derivation
  # would be retried a 3rd time before the 2nd fake sleep fires; the backoff
  # sleep bounds it to exactly 2 derivations.
  state = _run_poll_cycles(monkeypatch, accounts_fn=accounts_fn, create_provider=create_provider, n=2)

  assert derive_count["i"] == 2
  assert state["sleeps"] == 2


# ---------------------------------------------------------------------------
# Emit-time expiry annotation (broadcast + GET route): the shared claude
# predicate judged on the server clock at every emit, on emit copies only.
# ---------------------------------------------------------------------------


def _cached_claude_snapshot(windows: list[dict], fetched_at: str) -> dict:
  return {"provider": "claude", "account": "main", "windows": windows, "fetched_at": fetched_at}


def test_annotation_flips_across_a_reset_crossing_on_injected_clocks() -> None:
  """Death-decoupling: the flip needs no fresh fetch — one frozen snapshot annotates
  differently once the server clock crosses the window's reset."""
  reset_at = datetime(2026, 9, 10, 5, 50, tzinfo=UTC)
  ext_usage_mod._cached_usage["claude:main"] = _cached_claude_snapshot(
      [
          {
              "window_minutes": 300,
              "utilization": 91.0,
              "resets_at": _iso_z(reset_at)
          },
          {
              "window_minutes": 10080,
              "utilization": 17.0,
              "resets_at": _iso_z(reset_at + timedelta(days=3))
          },
      ],
      fetched_at=_iso_z(reset_at - timedelta(hours=2)))

  before = ext_usage_mod._annotated_providers(now=reset_at - timedelta(minutes=1))
  after = ext_usage_mod._annotated_providers(now=reset_at + timedelta(minutes=1))

  assert "expired" not in before["claude:main"]["windows"][0]
  assert after["claude:main"]["windows"][0]["expired"] is True
  assert "expired" not in after["claude:main"]["windows"][1]
  # The judgement is emit-time state: the cache itself stays unannotated.
  assert all("expired" not in w for w in ext_usage_mod._cached_usage["claude:main"]["windows"])


def test_annotation_leaves_codex_pending_and_error_entries_untouched() -> None:
  codex = {
      "provider": "codex",
      "account": "main",
      "windows":
          [{
              "window_minutes": 10080,
              "utilization": 96.0,
              "resets_at": _iso_z(datetime(2020, 1, 8, tzinfo=UTC)),
          }],
      "fetched_at": _iso_z(datetime(2020, 1, 1, tzinfo=UTC)),
      "token_count_observed_at": _iso_z(datetime(2020, 1, 1, tzinfo=UTC)),
  }
  pending = {"provider": "claude", "account": "unread", "pending": True}
  error = {"provider": "claude", "account": "broken", "error": "rate limited"}
  ext_usage_mod._cached_usage.update({"codex:main": codex, "claude:unread": pending, "claude:broken": error})

  providers = ext_usage_mod._annotated_providers()

  # A codex reading this stale would fail the browser predicate; the annotation
  # is claude-only, so codex windows never carry the key.
  assert all("expired" not in w for w in providers["codex:main"]["windows"])
  assert providers["claude:unread"] == pending
  assert providers["claude:broken"] == error


@pytest.mark.asyncio
async def test_get_ext_usage_annotates_claude_windows_at_read_time() -> None:
  moment = datetime.now(UTC)
  dead = _cached_claude_snapshot(
      [{
          "window_minutes": 300,
          "utilization": 91.0,
          "resets_at": _iso_z(moment - timedelta(hours=1)),
      }],
      fetched_at=_iso_z(moment - timedelta(hours=2)))
  live = _cached_claude_snapshot(
      [{
          "window_minutes": 300,
          "utilization": 12.0,
          "resets_at": _iso_z(moment + timedelta(hours=1)),
      }],
      fetched_at=_iso_z(moment))
  ext_usage_mod._cached_usage.update({"claude:main": dead, "claude:ext-1": live})

  result = await ext_usage_mod.get_ext_usage()

  assert result["providers"]["claude:main"]["windows"][0]["expired"] is True
  assert "expired" not in result["providers"]["claude:ext-1"]["windows"][0]
  assert all("expired" not in w for w in ext_usage_mod._cached_usage["claude:main"]["windows"])


def test_poll_broadcast_carries_emit_time_expiry_annotation(monkeypatch) -> None:
  stale_value = {
      "windows":
          [{
              "window_minutes": 300,
              "utilization": 91.0,
              "resets_at": _iso_z(datetime(2020, 1, 1, 1, 0, tzinfo=UTC)),
          }],
      "fetched_at": _iso_z(datetime(2020, 1, 1, 0, 0, tzinfo=UTC)),
      "provider": "claude",
  }

  def create_provider(provider, label, dir_path):
    if provider == "claude":
      return _FakeProvider(lambda: stale_value)
    return _FakeProvider(lambda: None, error="no sessions found")

  accounts = {"claude": [("main", "/fake/main")], "codex": [("main", "/fake/codex")]}
  state = _run_poll_cycles(monkeypatch, accounts_fn=lambda: accounts, create_provider=create_provider, n=1)

  assert state["payloads"][0]["providers"]["claude:main"]["windows"][0]["expired"] is True
  # The annotation rides on the emit copy only.
  assert "expired" not in ext_usage_mod._cached_usage["claude:main"]["windows"][0]


def test_codex_usage_transform_reports_weekly_only_shape() -> None:
  """Codex now reports a single weekly window in the primary slot.

  The window is identified by its reported length, so it lands on a 7d entry
  rather than inheriting the meaning of the slot it arrived in.
  """
  fetched_at = "2026-07-20T22:10:00+00:00"
  lines = [
      json.dumps(
          _build_weekly_token_count_event(
              timestamp="2026-07-20T22:08:57.925Z",
              used_percent=96.0,
              resets_at=1785016000,
          ))
  ]

  event = _latest_token_count_event(lines)
  assert event is not None
  usage = _transform_codex_response(event, fetched_at=fetched_at)

  assert usage["windows"] == [
      {
          "window_minutes": 10080,
          "utilization": 96.0,
          "resets_at": datetime.fromtimestamp(1785016000, tz=UTC).isoformat(),
      }
  ]
  assert "rate_limits_state" not in usage


def test_transform_response_marks_missing_claude_percentage_unknown() -> None:
  usage = _transform_response({"fiveHour": {}, "sevenDay": {"utilization": 10.0}})

  assert [w["utilization"] for w in usage["windows"]] == [None, 10.0]


def _plan_raw(**extra: Any) -> dict:
  """The plan-wide usage payload the scoped-window tests start from.

  The fiveHour/sevenDay readings are shared verbatim so a test's only
  visible variation is what it passes as keyword arguments on top (scoped
  ``limits``, unknown window keys, or nothing at all).
  """
  raw: dict[str, Any] = {
      "fiveHour": {
          "utilization": 11.0,
          "resetsAt": "2026-08-04T12:00:00+00:00"
      },
      "sevenDay": {
          "utilization": 22.0,
          "resetsAt": "2026-08-04T19:00:00+00:00"
      },
  }
  raw.update(extra)
  return raw


# The fully-formed scoped limit both scoped-window tests start from; the tests
# that exercise malformed shapes build their own entries with the broken field
# removed or altered, so they stay literal.
_NIMBUS_SCOPED_LIMIT = {
    "kind": "weekly_scoped",
    "group": "weekly",
    "percent": 33.0,
    "resets_at": "2026-08-04T19:00:00+00:00",
    "scope": {
        "model": {
            "id": None,
            "display_name": "Nimbus"
        },
        "surface": None
    },
}


def test_transform_scoped_limits_each_reading_bound_to_its_own_source() -> None:
  """Every reading renders as its own window, keyed by its own limit.

  Each window's percent is distinct so a swap between two bars becomes
  observable, and the scoped model name appears nowhere in the source so a
  hardcoded label fails.
  """
  raw = _plan_raw(limits=[_NIMBUS_SCOPED_LIMIT])

  windows = _transform_response(raw, account="main")["windows"]

  by_label = {}
  for window in windows:
    label = window.get("scope_label", "")
    by_label.setdefault(label, []).append(window["utilization"])
  assert by_label == {
      "": [11.0, 22.0],
      "Nimbus": [33.0],
  }


def test_transform_scoped_windows_leaves_unscoped_untouched_when_limits_removed() -> None:
  """Removing ``limits`` must not change the plan-wide windows at all."""
  raw = _plan_raw(limits=[_NIMBUS_SCOPED_LIMIT])

  with_limits = _transform_response(raw, account="main")["windows"]
  without = dict(raw)
  without.pop("limits")
  no_limits = _transform_response(without, account="main")["windows"]

  scoped = [w for w in with_limits if "scope_label" in w]
  unscoped_with = [w for w in with_limits if "scope_label" not in w]
  assert len(scoped) == 1
  assert unscoped_with == no_limits


def test_transform_response_scopes_are_sorted_before_planwide() -> None:
  raw = _plan_raw(
      limits=[
          {
              "group": "weekly",
              "percent": 33.0,
              "resets_at": "",
              "scope": {
                  "model": {
                      "display_name": "Nimbus"
                  }
              }
          },
          {
              "group": "weekly",
              "percent": 44.0,
              "resets_at": "",
              "scope": {
                  "model": {
                      "display_name": "Fable"
                  }
              }
          },
      ],)

  windows = _transform_response(raw, account="main")["windows"]

  assert [(w["window_minutes"], w.get("scope_label", "")) for w in windows] == [
      (300, ""),
      (10080, ""),
      (10080, "Fable"),
      (10080, "Nimbus"),
  ]


def test_transform_response_scoped_skip_and_warn_paths(monkeypatch) -> None:
  warns: list[dict] = []
  monkeypatch.setattr(ext_usage_mod.log, "warning", lambda event, **kw: warns.append({"event": event, **kw}))

  raw = _plan_raw(
      limits=[
          {
              "kind": "weekly_scoped",
              "group": "bogus",
              "percent": 33.0,
              "resets_at": "",
              "scope": {
                  "model": {
                      "display_name": "Nimbus"
                  }
              }
          },
          {
              "kind": "weekly_scoped",
              "group": "weekly",
              "percent": 44.0,
              "resets_at": "",
              "scope": {
                  "model": {}
              }
          },
      ],)

  windows = _transform_response(raw, account="main")["windows"]

  assert all("scope_label" not in w for w in windows)
  events = [w["event"] for w in warns if w["event"] == "ext_usage_unknown_limit_shape"]
  assert events == ["ext_usage_unknown_limit_shape", "ext_usage_unknown_limit_shape"]


def test_transform_response_unknown_shape_warns_once_per_process(monkeypatch) -> None:
  """An unchanged response re-transformed every poll round fires its alarm once, not every round."""
  warns: list[dict] = []
  monkeypatch.setattr(ext_usage_mod.log, "warning", lambda event, **kw: warns.append({"event": event, **kw}))

  raw = _plan_raw(
      nimbus_quill={
          "utilization": 3.0,
          "resetsAt": "2026-08-04T20:00:00+00:00"
      },
      extra_usage={
          "utilization": 5.0,
          "resets_at": "2026-08-05T00:00:00+00:00"
      },
      limits=[
          {
              "kind": "weekly_scoped",
              "group": "weekly",
              "resets_at": "",
              "scope": {
                  "model": {
                      "display_name": "Nimbus"
                  }
              }
          },
      ],
  )

  first_windows = _transform_response(raw, account="main")["windows"]
  first_events = [(w["slot"], w["reason"]) for w in warns if w["event"] == "ext_usage_unknown_limit_shape"]
  warns.clear()
  for _ in range(60):
    repeat_windows = _transform_response(raw, account="main")["windows"]
  repeat_events = [w for w in warns if w["event"] == "ext_usage_unknown_limit_shape"]

  assert repeat_windows == first_windows
  assert sorted(first_events) == [
      ("extra_usage", "unrecognized window field"),
      ("nimbus_quill", "unrecognized window field"),
      ("weekly_scoped", "missing percent"),
  ]
  assert repeat_events == []


def test_read_credentials_tokenless_file_warns_once_per_streak(monkeypatch, tmp_path) -> None:
  """A tokenless file re-read every poll round fires its alarm once, not every round."""
  warns: list[dict] = []
  monkeypatch.setattr(ext_usage_mod.log, "warning", lambda event, **kw: warns.append({"event": event, **kw}))
  credentials_path = tmp_path / ".credentials.json"
  _write_credentials(credentials_path, access="")

  assert _read_credentials(credentials_path) is None
  first_events = [w["event"] for w in warns]
  warns.clear()
  for _ in range(60):
    assert _read_credentials(credentials_path) is None

  assert first_events == ["ext_usage_no_access_token"]
  assert warns == []


def test_read_credentials_recovery_rearms_the_warning(monkeypatch, tmp_path) -> None:
  """A token read clears the streak: a later relapse is a new onset and earns one new line."""
  warns: list[dict] = []
  monkeypatch.setattr(ext_usage_mod.log, "warning", lambda event, **kw: warns.append({"event": event, **kw}))
  credentials_path = tmp_path / ".credentials.json"
  _write_credentials(credentials_path, access="")
  _read_credentials(credentials_path)

  _write_credentials(credentials_path)
  assert _read_credentials(credentials_path) is not None

  _write_credentials(credentials_path, access="")
  warns.clear()
  for _ in range(60):
    assert _read_credentials(credentials_path) is None

  assert [w["event"] for w in warns] == ["ext_usage_no_access_token"]


def test_read_credentials_missing_and_tokenless_are_separate_alarms(monkeypatch, tmp_path) -> None:
  """A path whose failure state changes warns once per state, not once per path forever."""
  warns: list[dict] = []
  monkeypatch.setattr(ext_usage_mod.log, "warning", lambda event, **kw: warns.append({"event": event, **kw}))
  credentials_path = tmp_path / ".credentials.json"

  for _ in range(60):
    assert _read_credentials(credentials_path) is None

  _write_credentials(credentials_path, access="")
  for _ in range(60):
    assert _read_credentials(credentials_path) is None

  assert [w["event"] for w in warns] == [
      "ext_usage_credentials_not_found",
      "ext_usage_no_access_token",
  ]


def test_read_credentials_flip_without_success_stays_one_line_per_event(monkeypatch, tmp_path) -> None:
  """A state flip inside one broken streak adds a line for the new state only.

  A streak ends on a token, not on a state flip: missing -> tokenless ->
  missing logs the missing alarm once, the tokenless alarm once, and no
  third line, because the relapsed missing sighting repeats a fired alarm.
  """
  warns: list[dict] = []
  monkeypatch.setattr(ext_usage_mod.log, "warning", lambda event, **kw: warns.append({"event": event, **kw}))
  credentials_path = tmp_path / ".credentials.json"

  for _ in range(60):
    assert _read_credentials(credentials_path) is None

  _write_credentials(credentials_path, access="")
  for _ in range(60):
    assert _read_credentials(credentials_path) is None

  credentials_path.unlink()
  for _ in range(60):
    assert _read_credentials(credentials_path) is None

  assert [w["event"] for w in warns] == [
      "ext_usage_credentials_not_found",
      "ext_usage_no_access_token",
  ]


def test_transform_response_absent_limits_produces_exactly_today_windows() -> None:
  raw = _plan_raw()

  assert _transform_response(
      raw, account="main")["windows"] == [
          {
              "window_minutes": 300,
              "utilization": 11.0,
              "resets_at": "2026-08-04T12:00:00+00:00"
          },
          {
              "window_minutes": 10080,
              "utilization": 22.0,
              "resets_at": "2026-08-04T19:00:00+00:00"
          },
      ]


@pytest.mark.asyncio
async def test_codex_provider_fetch_reads_newest_rollout_beyond_three_days(tmp_path) -> None:
  """No date cliff: the last known reading stays visible however old it is.

  Under a weekly window a reading from days ago is the only information there
  is; its age is reported rather than used to hide it.
  """
  provider = CodexUsageProvider(label="personal", home_dir=str(tmp_path))
  now = datetime.now(UTC)
  old_day = now - timedelta(days=9)
  rollout_dir = (tmp_path / "sessions" / f"{old_day.year:04d}" / f"{old_day.month:02d}" / f"{old_day.day:02d}")
  rollout_dir.mkdir(parents=True)

  rollout_path = rollout_dir / "rollout-old.jsonl"
  rollout_path.write_text(
      json.dumps(
          _build_weekly_token_count_event(
              timestamp=old_day.isoformat().replace("+00:00", "Z"),
              used_percent=96.0,
              resets_at=int(now.timestamp()) + 3600,
          )) + "\n")
  os.utime(rollout_path, (old_day.timestamp(), old_day.timestamp()))

  usage = await provider.fetch()

  assert usage is not None
  assert [w["window_minutes"] for w in usage["windows"]] == [10080]
  assert usage["windows"][0]["utilization"] == 96.0


@pytest.mark.asyncio
async def test_codex_provider_fetch_reports_no_sessions_for_empty_home(tmp_path) -> None:
  provider = CodexUsageProvider(label="personal", home_dir=str(tmp_path))

  assert await provider.fetch() is None
  assert provider.last_error == "no sessions found"


def test_list_rollout_files_finds_nested_rollout_logs(tmp_path) -> None:
  """The usage scrape and the spend aggregation share one directory walk."""
  sessions_dir = tmp_path / "sessions"
  rollout_dir = sessions_dir / "2026" / "06" / "01"
  rollout_dir.mkdir(parents=True)
  rollout_path = rollout_dir / "rollout-shared.jsonl"
  rollout_path.write_text(json.dumps(_build_turn_context_event(model="gpt-5.3-codex")) + "\n")

  assert _list_rollout_files(sessions_dir) == [rollout_path]


def test_poll_seeds_pending_rows_so_no_account_is_missing_from_the_first_broadcast(monkeypatch) -> None:
  """A restart must not hide accounts the round-robin has not reached yet.

  One fetch per round gap means the last account is N-1 gaps behind the first, so
  a strip built only from fetched accounts silently omits real accounts for
  minutes after every restart — always the same ones, since the round order is
  fixed. Seeding costs no extra request: the placeholders ride along in the
  broadcast the first real fetch already sends.
  """
  main_value = _claude_fetch_value(42.0)

  def create_provider(provider, label, dir_path):
    if provider == "claude" and label == "main":
      return _FakeProvider(lambda: main_value)
    return _FakeProvider(lambda: None, error="credentials not found")

  accounts = {
      "claude": [("main", "/fake/main"), ("invite-1", "/fake/invite-1")],
      "codex": [("main", "/fake/codex")],
  }
  # One sleep == one fetch, so this stops right after the first account's fetch.
  state = _run_poll_cycles(monkeypatch, accounts_fn=lambda: accounts, create_provider=create_provider, n=1)

  assert state["broadcasts"] == 1
  providers = state["payloads"][0]["providers"]
  # Seeding before any fetch resolves also fixes row order at derivation order.
  assert list(providers) == ["claude:main", "claude:invite-1", "codex:main"]
  assert providers["claude:main"]["windows"][0]["utilization"] == 42.0
  assert "pending" not in providers["claude:main"]
  assert providers["claude:invite-1"] == {"provider": "claude", "account": "invite-1", "pending": True}
  assert providers["codex:main"] == {"provider": "codex", "account": "main", "pending": True}


def test_poll_pending_placeholder_is_replaced_by_the_real_error(monkeypatch) -> None:
  """A pending row is a not-yet-read marker, not data worth keeping."""

  def create_provider(provider, label, dir_path):
    return _FakeProvider(lambda: None, error="credentials not found")

  accounts = {"claude": [("main", "/fake/main")], "codex": []}
  _run_poll_cycles(monkeypatch, accounts_fn=lambda: accounts, create_provider=create_provider, n=1)

  assert ext_usage_mod._cached_usage["claude:main"] == {
      "provider": "claude",
      "account": "main",
      "error": "credentials not found",
  }


# ---------------------------------------------------------------------------
# ClaudeUsageProvider: 401-triggered renewal. The provider owns no clock; the
# server's 401 is the only signal that a stored token is unusable.
# ---------------------------------------------------------------------------

_CLAUDE_USAGE_PAYLOAD = {
    "five_hour": {
        "utilization": 12.0,
        "resets_at": "2026-07-29T23:00:00+00:00"
    },
    "seven_day": {
        "utilization": 34.0,
        "resets_at": "2026-08-03T19:00:00+00:00"
    },
}

# Long past in milliseconds, so any surviving clock check would fire on it.
_STALE_EXPIRES_AT_MS = 1_700_000_000_000


class _FakeResponse:

  def __init__(self, status_code: int, payload: dict, text: str = "") -> None:
    self.status_code = status_code
    self._payload = payload
    self.text = text

  def json(self) -> dict:
    return self._payload

  def raise_for_status(self) -> None:
    if self.status_code >= 400:
      raise RuntimeError(f"HTTP {self.status_code}")


class _FakeUsageHTTP:
  """Scripted stand-in for the shared client that records every outbound call."""

  def __init__(
      self,
      get_statuses: list[int],
      *,
      renewal: dict | None = None,
      on_get: Callable[[int], None] | None = None,
      renewal_status: int = 200,
      renewal_body: str = "") -> None:
    self._get_statuses = list(get_statuses)
    self._renewal = renewal if renewal is not None else {
        "access_token": "tok-new",
        "refresh_token": "ref-new",
        "expires_in": 28800
    }
    self._on_get = on_get
    self._renewal_status = renewal_status
    self._renewal_body = renewal_body
    self.gets: list[dict] = []
    self.posts: list[dict] = []

  async def get(self, url, headers=None, timeout=None):
    self.gets.append({"url": url, "headers": dict(headers or {})})
    status = self._get_statuses.pop(0)
    if self._on_get is not None:
      self._on_get(len(self.gets))
    return _FakeResponse(status, _CLAUDE_USAGE_PAYLOAD if status == 200 else {})

  async def post(self, url, json=None, headers=None, timeout=None):
    self.posts.append({"url": url, "json": dict(json or {}), "headers": dict(headers or {})})
    return _FakeResponse(self._renewal_status, self._renewal, text=self._renewal_body)


def _write_credentials(path, *, access="tok-stored", refresh="ref-stored", expires_at=_STALE_EXPIRES_AT_MS) -> None:
  payload = {"claudeAiOauth": {"accessToken": access, "refreshToken": refresh}}
  if expires_at is not None:
    payload["claudeAiOauth"]["expiresAt"] = expires_at
  path.write_text(json.dumps(payload))


def _claude_provider(monkeypatch, tmp_path, fake, **creds) -> ClaudeUsageProvider:
  credentials_path = tmp_path / ".credentials.json"
  _write_credentials(credentials_path, **creds)
  monkeypatch.setattr(ext_usage_mod, "get_http_client", lambda: fake)
  return ClaudeUsageProvider("ext-test", credentials_path)


@pytest.mark.asyncio
async def test_claude_fetch_renews_once_and_retries_once_after_401(monkeypatch, tmp_path) -> None:
  fake = _FakeUsageHTTP([401, 200])
  provider = _claude_provider(monkeypatch, tmp_path, fake)

  result = await provider.fetch()

  assert result is not None
  assert [w["utilization"] for w in result["windows"]] == [12.0, 34.0]
  # The mechanism, not a literal: exactly one renewal and one retry, no loop.
  assert len(fake.posts) == 1
  assert len(fake.gets) == 2
  assert fake.gets[0]["headers"]["Authorization"] == "Bearer tok-stored"
  assert fake.gets[1]["headers"]["Authorization"] == "Bearer tok-new"


@pytest.mark.asyncio
async def test_claude_fetch_yields_to_a_concurrent_renewal_without_rotating(monkeypatch, tmp_path) -> None:
  credentials_path = tmp_path / ".credentials.json"

  def _cli_renews_after_first_get(call_number: int) -> None:
    if call_number == 1:
      _write_credentials(credentials_path, access="tok-from-cli", refresh="ref-from-cli")

  fake = _FakeUsageHTTP([401, 200], on_get=_cli_renews_after_first_get)
  provider = _claude_provider(monkeypatch, tmp_path, fake)

  result = await provider.fetch()

  assert result is not None
  # Someone else already renewed, so nothing was rotated out from under them.
  assert not fake.posts
  assert fake.gets[1]["headers"]["Authorization"] == "Bearer tok-from-cli"


@pytest.mark.asyncio
async def test_claude_fetch_backs_off_after_a_second_401_and_then_issues_no_request(monkeypatch, tmp_path) -> None:
  fake = _FakeUsageHTTP([401, 401])
  provider = _claude_provider(monkeypatch, tmp_path, fake)

  assert await provider.fetch() is None
  assert provider._backoff_until > time.time()
  calls_after_first_fetch = (len(fake.gets), len(fake.posts))

  assert await provider.fetch() is None
  # A backed-off account is silent: no request may leave while the gate is closed.
  assert (len(fake.gets), len(fake.posts)) == calls_after_first_fetch


@pytest.mark.asyncio
async def test_claude_fetch_does_not_renew_on_a_long_past_stored_expiry(monkeypatch, tmp_path) -> None:
  fake = _FakeUsageHTTP([200])
  provider = _claude_provider(monkeypatch, tmp_path, fake)

  assert await provider.fetch() is not None
  # No clock-driven renewal survives: a stale expiresAt alone changes nothing.
  assert not fake.posts
  assert len(fake.gets) == 1


@pytest.mark.asyncio
async def test_claude_renewal_writes_back_advancing_expiry_and_rotated_token(monkeypatch, tmp_path) -> None:
  fake = _FakeUsageHTTP([401, 200])
  provider = _claude_provider(monkeypatch, tmp_path, fake)

  await provider.fetch()

  stored = json.loads(provider.credentials_path.read_text())["claudeAiOauth"]
  assert stored["accessToken"] == "tok-new"
  assert stored["refreshToken"] == "ref-new"
  # Milliseconds, and in the future: the field the external CLI schedules from.
  assert stored["expiresAt"] > time.time() * 1000
  assert oct(os.stat(provider.credentials_path).st_mode & 0o777) == "0o600"


@pytest.mark.asyncio
async def test_claude_renewal_without_expiry_keeps_the_stored_value(monkeypatch, tmp_path) -> None:
  fake = _FakeUsageHTTP([401, 200], renewal={"access_token": "tok-new"})
  provider = _claude_provider(monkeypatch, tmp_path, fake)

  await provider.fetch()

  stored = json.loads(provider.credentials_path.read_text())["claudeAiOauth"]
  assert stored["accessToken"] == "tok-new"
  assert stored["expiresAt"] == _STALE_EXPIRES_AT_MS


@pytest.mark.asyncio
async def test_claude_requests_identify_as_claude_code(monkeypatch, tmp_path) -> None:
  fake = _FakeUsageHTTP([401, 200])
  provider = _claude_provider(monkeypatch, tmp_path, fake)

  await provider.fetch()

  assert all(call["headers"]["User-Agent"].startswith("claude-code/") for call in fake.gets)
  assert all(call["headers"]["User-Agent"].startswith("claude-code/") for call in fake.posts)


@pytest.mark.asyncio
async def test_claude_renewal_posts_to_the_platform_token_endpoint(monkeypatch, tmp_path) -> None:
  fake = _FakeUsageHTTP([401, 200])
  provider = _claude_provider(monkeypatch, tmp_path, fake)

  await provider.fetch()

  # The recorded request, not the module constant: proving the constant is
  # itself proves nothing.
  assert fake.posts[0]["url"] == "https://platform.claude.com/v1/oauth/token"


@pytest.mark.asyncio
async def test_claude_renewal_failure_arms_backoff_and_reports_token_refresh_failed(monkeypatch, tmp_path) -> None:
  fake = _FakeUsageHTTP([401], renewal_body="boom", renewal_status=400)
  provider = _claude_provider(monkeypatch, tmp_path, fake)

  assert await provider.fetch() is None
  assert provider.last_error == "token refresh failed"
  assert provider._backoff_until > time.time()
  calls_after_first_fetch = (len(fake.gets), len(fake.posts))

  assert await provider.fetch() is None
  # A backed-off account is silent and reports what armed the backoff.
  assert provider.last_error == "token refresh failed"
  assert (len(fake.gets), len(fake.posts)) == calls_after_first_fetch


@pytest.mark.asyncio
async def test_claude_rate_limited_arms_backoff_and_reports_rate_limited(monkeypatch, tmp_path) -> None:
  fake = _FakeUsageHTTP([429])
  provider = _claude_provider(monkeypatch, tmp_path, fake)

  assert await provider.fetch() is None
  assert provider.last_error == "rate limited"
  assert provider._backoff_until > time.time()

  assert await provider.fetch() is None
  assert provider.last_error == "rate limited"


@pytest.mark.asyncio
async def test_claude_renewal_succeeds_but_retried_get_401_reports_auth_rejected(monkeypatch, tmp_path) -> None:
  fake = _FakeUsageHTTP([401, 401])
  provider = _claude_provider(monkeypatch, tmp_path, fake)

  assert await provider.fetch() is None
  assert provider.last_error == "auth rejected"
  assert provider._backoff_until > time.time()


# ---------------------------------------------------------------------------
# User-Agent resolution: both OAuth consumers (usage GET and refresh POST)
# share one runtime-probed claude-code/<version>; any probe failure falls
# back to the pinned constant with a loud warning, resolved once per process.
# ---------------------------------------------------------------------------


def _capture_user_agent_resolutions(monkeypatch) -> list[dict]:
  """Record every log call at the two levels the resolution event can take."""
  events: list[dict] = []

  def record(level: str):

    def sink(event, **kw) -> None:
      events.append({"level": level, "event": event, **kw})

    return sink

  monkeypatch.setattr(ext_usage_mod.log, "info", record("info"))
  monkeypatch.setattr(ext_usage_mod.log, "warning", record("warning"))
  return events


def _arm_user_agent_probe(
    monkeypatch,
    *,
    stdout: bytes = b"",
    returncode: int = 0,
    error: Exception | None = None,
) -> list[tuple[list[str], dict]]:
  """Arm the probe: clear the cache and script a fake ``subprocess.run``.

  Returns the recorded calls so a test can assert the exact subprocess
  mechanism (argv, capture, timeout, no shell) and that it ran only once.
  """
  ext_usage_mod._user_agent_cache = None
  calls: list[tuple[list[str], dict]] = []

  def fake_run(args, **kwargs):
    calls.append((list(args), kwargs))
    if error is not None:
      raise error
    return subprocess.CompletedProcess(args, returncode, stdout=stdout, stderr=b"")

  monkeypatch.setattr(ext_usage_mod.subprocess, "run", fake_run)
  return calls


def _user_agent_resolution_events(events: list[dict]) -> list[dict]:
  return [event for event in events if event["event"] == "ext_usage_user_agent_resolved"]


@pytest.mark.asyncio
async def test_claude_requests_carry_the_probed_cli_version(monkeypatch, tmp_path) -> None:
  """Usage GET and refresh POST share the probed version, probed exactly once."""
  fake = _FakeUsageHTTP([401, 200, 200])
  provider = _claude_provider(monkeypatch, tmp_path, fake)
  events = _capture_user_agent_resolutions(monkeypatch)
  probe_calls = _arm_user_agent_probe(monkeypatch, stdout=b"2.9.9 (Claude Code)")

  await provider.fetch()
  await provider.fetch()

  assert fake.gets[0]["headers"]["User-Agent"] == "claude-code/2.9.9"
  assert fake.posts[0]["headers"]["User-Agent"] == "claude-code/2.9.9"
  # The probe mechanism, exactly as pinned: fixed argv, captured output, 5s
  # timeout, no shell -- and only one subprocess across both requests.
  assert probe_calls == [(["claude", "--version"], {"capture_output": True, "timeout": 5})]
  assert _user_agent_resolution_events(events) == [
      {"level": "info", "event": "ext_usage_user_agent_resolved", "version": "2.9.9", "source": "probe"}
  ]


@pytest.mark.asyncio
async def test_user_agent_probe_missing_binary_falls_back_with_warning(monkeypatch, tmp_path) -> None:
  fake = _FakeUsageHTTP([200])
  provider = _claude_provider(monkeypatch, tmp_path, fake)
  events = _capture_user_agent_resolutions(monkeypatch)
  _arm_user_agent_probe(monkeypatch, error=FileNotFoundError(2, "No such file or directory: 'claude'"))

  await provider.fetch()

  assert fake.gets[0]["headers"]["User-Agent"] == "claude-code/2.1.219"
  assert _user_agent_resolution_events(events) == [
      {"level": "warning", "event": "ext_usage_user_agent_resolved", "version": "2.1.219", "source": "fallback"}
  ]


@pytest.mark.asyncio
async def test_user_agent_probe_nonzero_exit_falls_back_with_warning(monkeypatch, tmp_path) -> None:
  fake = _FakeUsageHTTP([200])
  provider = _claude_provider(monkeypatch, tmp_path, fake)
  events = _capture_user_agent_resolutions(monkeypatch)
  _arm_user_agent_probe(monkeypatch, returncode=1)

  await provider.fetch()

  assert fake.gets[0]["headers"]["User-Agent"] == "claude-code/2.1.219"
  assert _user_agent_resolution_events(events) == [
      {"level": "warning", "event": "ext_usage_user_agent_resolved", "version": "2.1.219", "source": "fallback"}
  ]


@pytest.mark.asyncio
async def test_user_agent_probe_unparseable_output_falls_back_with_warning(monkeypatch, tmp_path) -> None:
  fake = _FakeUsageHTTP([200])
  provider = _claude_provider(monkeypatch, tmp_path, fake)
  events = _capture_user_agent_resolutions(monkeypatch)
  _arm_user_agent_probe(monkeypatch, stdout=b"claude is unavailable right now")

  await provider.fetch()

  assert fake.gets[0]["headers"]["User-Agent"] == "claude-code/2.1.219"
  assert _user_agent_resolution_events(events) == [
      {"level": "warning", "event": "ext_usage_user_agent_resolved", "version": "2.1.219", "source": "fallback"}
  ]
