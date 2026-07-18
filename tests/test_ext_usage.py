import asyncio
import json
import os
import types
from datetime import date, datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Callable

import pytest

from src.api import ext_usage as ext_usage_mod
from src.api.ext_usage import (
    CLAUDE_DEFAULT_DIR,
    CODEX_DEFAULT_DIR,
    CodexUsageProvider,
    _account_label,
    _compute_codex_spend_windows,
    _derive_accounts,
    _extract_latest_codex_usage,
    _poll_loop,
    _transform_response,
)
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption


def _build_token_count_event(
    *,
    timestamp: str,
    primary_used_percent: float,
    primary_resets_at: int,
    secondary_used_percent: float,
    secondary_resets_at: int,
) -> dict:
  return {
    "timestamp": timestamp,
    "type": "event_msg",
    "payload": {
      "type": "token_count",
      "rate_limits": {
        "primary": {
          "used_percent": primary_used_percent,
          "resets_at": primary_resets_at,
        },
        "secondary": {
          "used_percent": secondary_used_percent,
          "resets_at": secondary_resets_at,
        },
      },
    },
  }


def test_extract_latest_codex_usage_adds_token_count_observed_at() -> None:
  fetched_at = "2026-03-27T18:30:00+00:00"
  lines = [json.dumps(_build_token_count_event(
      timestamp="2026-03-27T18:29:35.694Z",
      primary_used_percent=8.0,
      primary_resets_at=1774653423,
      secondary_used_percent=2.0,
      secondary_resets_at=1775240223,
  ))]

  usage = _extract_latest_codex_usage(lines, fetched_at=fetched_at)

  assert usage == {
    "five_hour": {
      "utilization": 8.0,
      "resets_at": datetime.fromtimestamp(1774653423, tz=timezone.utc).isoformat(),
    },
    "seven_day": {
      "utilization": 2.0,
      "resets_at": datetime.fromtimestamp(1775240223, tz=timezone.utc).isoformat(),
    },
    "fetched_at": fetched_at,
    "provider": "codex",
    "token_count_observed_at": "2026-03-27T18:29:35.694Z",
  }
  assert "rate_limits_state" not in usage


def test_extract_latest_codex_usage_uses_latest_token_count_event() -> None:
  lines = [
    json.dumps(_build_token_count_event(
        timestamp="2026-03-27T17:00:00Z",
        primary_used_percent=12.0,
        primary_resets_at=1774650000,
        secondary_used_percent=4.0,
        secondary_resets_at=1775240000,
    )),
    json.dumps(_build_token_count_event(
        timestamp="2026-03-27T18:00:00Z",
        primary_used_percent=18.0,
        primary_resets_at=1774653600,
        secondary_used_percent=6.0,
        secondary_resets_at=1775243600,
    )),
    "{not valid json",
    json.dumps({
      "timestamp": "2026-03-27T18:05:00Z",
      "type": "event_msg",
      "payload": {"type": "agent_message", "message": "still not usage"},
    }),
  ]

  usage = _extract_latest_codex_usage(lines, fetched_at="2026-03-27T18:10:00+00:00")

  assert usage is not None
  assert usage["five_hour"]["utilization"] == 18.0
  assert usage["seven_day"]["utilization"] == 6.0
  assert usage["token_count_observed_at"] == "2026-03-27T18:00:00Z"


def test_extract_latest_codex_usage_handles_null_rate_limit_buckets() -> None:
  fetched_at = "2026-03-27T18:40:00+00:00"
  lines = [json.dumps({
    "timestamp": "2026-03-27T18:39:35.694Z",
    "type": "event_msg",
    "payload": {
      "type": "token_count",
      "rate_limits": {
        "primary": None,
        "secondary": None,
        "credits": {
          "unlimited": True,
        },
        "plan_type": "business",
      },
    },
  })]

  usage = _extract_latest_codex_usage(lines, fetched_at=fetched_at)

  assert usage == {
    "five_hour": {
      "utilization": 0.0,
      "resets_at": "",
    },
    "seven_day": {
      "utilization": 0.0,
      "resets_at": "",
    },
    "fetched_at": fetched_at,
    "provider": "codex",
    "rate_limits_state": "business-unlimited",
    "token_count_observed_at": "2026-03-27T18:39:35.694Z",
  }


def test_extract_latest_codex_usage_does_not_assume_business_state_without_metadata() -> None:
  fetched_at = "2026-03-27T18:40:00+00:00"
  lines = [json.dumps({
    "timestamp": "2026-03-27T18:39:35.694Z",
    "type": "event_msg",
    "payload": {
      "type": "token_count",
      "rate_limits": {
        "primary": None,
        "secondary": None,
      },
    },
  })]

  usage = _extract_latest_codex_usage(lines, fetched_at=fetched_at)

  assert usage == {
    "five_hour": {
      "utilization": 0.0,
      "resets_at": "",
    },
    "seven_day": {
      "utilization": 0.0,
      "resets_at": "",
    },
    "fetched_at": fetched_at,
    "provider": "codex",
    "token_count_observed_at": "2026-03-27T18:39:35.694Z",
  }


def test_compute_codex_spend_windows_prices_recent_turns_by_model(tmp_path) -> None:
  now = datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc)
  rollout_dir = tmp_path / "2026" / "06" / "01"
  rollout_dir.mkdir(parents=True)
  rollout_path = rollout_dir / "rollout-recent.jsonl"

  def token_count_event(offset: timedelta, input_tokens: int, cached_input_tokens: int,
                        output_tokens: int) -> dict:
    return {
      "timestamp": (now - offset).isoformat().replace("+00:00", "Z"),
      "type": "event_msg",
      "payload": {
        "type": "token_count",
        "info": {
          "last_token_usage": {
            "input_tokens": input_tokens,
            "cached_input_tokens": cached_input_tokens,
            "output_tokens": output_tokens,
          },
        },
      },
    }

  events = [
    {
      "type": "turn_context",
      "payload": {"model": "gpt-5.5"},
    },
    token_count_event(timedelta(hours=2), 1_000_000, 100_000, 10_000),
    token_count_event(timedelta(days=2), 2_000_000, 500_000, 20_000),
    token_count_event(timedelta(days=8), 5_000_000, 0, 100_000),
  ]
  rollout_path.write_text("\n".join(json.dumps(event) for event in events) + "\n")
  os.utime(rollout_path, (now.timestamp(), now.timestamp()))

  old_rollout_path = rollout_dir / "rollout-old-mtime.jsonl"
  old_rollout_path.write_text("{not valid json\n")
  old_mtime = (now - timedelta(days=8)).timestamp()
  os.utime(old_rollout_path, (old_mtime, old_mtime))

  spend = _compute_codex_spend_windows(sessions_dir=tmp_path, now=now)

  assert spend["last_24h_usd"] == pytest.approx(4.85)
  assert spend["last_7d_usd"] == pytest.approx(13.20)


@pytest.mark.asyncio
async def test_codex_provider_fetch_keeps_quota_when_historical_spend_row_is_malformed(
    tmp_path,
) -> None:
  # The provider reads rollout logs from <home_dir>/sessions, so the test seeds
  # that subtree and constructs the instance with home_dir pointing at tmp_path.
  provider = CodexUsageProvider(label="main", home_dir=str(tmp_path))
  today = date.today()
  now = datetime.now(timezone.utc)
  sessions_dir = tmp_path / "sessions"
  rollout_dir = sessions_dir / f"{today.year:04d}" / f"{today.month:02d}" / f"{today.day:02d}"
  rollout_dir.mkdir(parents=True)

  stale_rollout_path = rollout_dir / "rollout-stale.jsonl"
  stale_rollout_path.write_text("{not valid json\n")
  stale_mtime = now.timestamp() - 60
  os.utime(stale_rollout_path, (stale_mtime, stale_mtime))

  live_rollout_path = rollout_dir / "rollout-live.jsonl"
  live_rollout_path.write_text(
      json.dumps(_build_token_count_event(
          timestamp=now.isoformat().replace("+00:00", "Z"),
          primary_used_percent=8.0,
          primary_resets_at=int(now.timestamp()) + 3600,
          secondary_used_percent=2.0,
          secondary_resets_at=int(now.timestamp()) + 86400,
      )) + "\n"
  )
  os.utime(live_rollout_path, (now.timestamp(), now.timestamp()))

  usage = await provider.fetch()

  assert usage is not None
  assert usage["five_hour"]["utilization"] == 8.0
  assert usage["seven_day"]["utilization"] == 2.0
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
  today = date.today()
  now = datetime.now(timezone.utc)
  sessions_dir = tmp_path / "sessions"
  rollout_dir = sessions_dir / f"{today.year:04d}" / f"{today.month:02d}" / f"{today.day:02d}"
  rollout_dir.mkdir(parents=True)

  live_rollout_path = rollout_dir / "rollout-live.jsonl"
  live_rollout_path.write_text(
      json.dumps(_build_token_count_event(
          timestamp=now.isoformat().replace("+00:00", "Z"),
          primary_used_percent=8.0,
          primary_resets_at=int(now.timestamp()) + 3600,
          secondary_used_percent=2.0,
          secondary_resets_at=int(now.timestamp()) + 86400,
      )) + "\n"
  )
  os.utime(live_rollout_path, (now.timestamp(), now.timestamp()))

  def _broken_compute(*, sessions_dir=None, now=None):
    raise RuntimeError("simulated spend failure")

  monkeypatch.setattr("src.api.ext_usage._compute_codex_spend_windows", _broken_compute)

  usage = await provider.fetch()

  assert usage is not None
  assert usage["five_hour"]["utilization"] == 8.0
  assert usage["seven_day"]["utilization"] == 2.0
  assert usage["spend"] is None


def test_compute_codex_spend_windows_skips_bad_rows_without_poisoning_totals(tmp_path) -> None:
  now = datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc)
  rollout_dir = tmp_path / "2026" / "06" / "01"
  rollout_dir.mkdir(parents=True)
  rollout_path = rollout_dir / "rollout-mixed.jsonl"

  def token_count_event(timestamp: str, input_tokens: Any, cached_input_tokens: Any,
                        output_tokens: Any) -> dict:
    return {
      "timestamp": timestamp,
      "type": "event_msg",
      "payload": {
        "type": "token_count",
        "info": {
          "last_token_usage": {
            "input_tokens": input_tokens,
            "cached_input_tokens": cached_input_tokens,
            "output_tokens": output_tokens,
          },
        },
      },
    }

  events = [
    {"type": "turn_context", "payload": {"model": "gpt-5.5"}},
    token_count_event((now - timedelta(hours=2)).isoformat().replace("+00:00", "Z"), 1_000_000, 100_000, 10_000),
    token_count_event((now - timedelta(hours=2)).isoformat().replace("+00:00", "Z"), "bad", 0, 0),
    token_count_event((now - timedelta(hours=2)).isoformat().replace("+00:00", "Z"), 500_000, None, 5_000),
    token_count_event(None, 1_000_000, 0, 0),
    token_count_event(12345, 1_000_000, 0, 0),
    {"timestamp": (now - timedelta(hours=2)).isoformat().replace("+00:00", "Z"), "type": "event_msg", "payload": {"type": "token_count", "info": {}}},
    token_count_event((now - timedelta(hours=2)).isoformat().replace("+00:00", "Z"), 500_000, 50_000, 5_000),
  ]
  rollout_path.write_text("\n".join(json.dumps(event) if isinstance(event, dict) else event for event in events) + "\n")
  os.utime(rollout_path, (now.timestamp(), now.timestamp()))

  spend = _compute_codex_spend_windows(sessions_dir=tmp_path, now=now)

  assert spend["last_24h_usd"] == pytest.approx(7.275)
  assert spend["last_7d_usd"] == pytest.approx(7.275)


def test_compute_codex_spend_windows_skips_unreadable_file(tmp_path) -> None:
  now = datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc)
  rollout_dir = tmp_path / "2026" / "06" / "01"
  rollout_dir.mkdir(parents=True)

  unreadable_path = rollout_dir / "rollout-unreadable.jsonl"
  unreadable_path.write_text("should not be read\n")
  os.utime(unreadable_path, (now.timestamp(), now.timestamp()))
  os.chmod(unreadable_path, 0o000)

  readable_path = rollout_dir / "rollout-readable.jsonl"
  events = [
    {"type": "turn_context", "payload": {"model": "gpt-5.5"}},
    {
      "timestamp": now.isoformat().replace("+00:00", "Z"),
      "type": "event_msg",
      "payload": {
        "type": "token_count",
        "info": {
          "last_token_usage": {
            "input_tokens": 1_000_000,
            "cached_input_tokens": 0,
            "output_tokens": 0,
          },
        },
      },
    },
  ]
  readable_path.write_text("\n".join(json.dumps(event) for event in events) + "\n")
  os.utime(readable_path, (now.timestamp(), now.timestamp()))

  try:
    spend = _compute_codex_spend_windows(sessions_dir=tmp_path, now=now)
  finally:
    os.chmod(unreadable_path, 0o644)

  assert spend["last_24h_usd"] == pytest.approx(5.00)
  assert spend["last_7d_usd"] == pytest.approx(5.00)


def test_transform_response_preserves_claude_payload_shape() -> None:
  usage = _transform_response({
    "fiveHour": {
      "utilization": 42.0,
      "resetsAt": "2026-03-27T20:00:00+00:00",
    },
    "sevenDay": {
      "utilization": 10.0,
      "resetsAt": "2026-04-01T20:00:00+00:00",
    },
  })

  assert usage["five_hour"] == {
    "utilization": 42.0,
    "resets_at": "2026-03-27T20:00:00+00:00",
  }
  assert usage["seven_day"] == {
    "utilization": 10.0,
    "resets_at": "2026-04-01T20:00:00+00:00",
  }
  assert usage["provider"] == "claude"
  assert "token_count_observed_at" not in usage


# ---------------------------------------------------------------------------
# Account-set derivation (T2)
# ---------------------------------------------------------------------------


def _patch_config(monkeypatch, options: list[BackendOption]) -> None:
  monkeypatch.setattr(ext_usage_mod, "get_config", lambda: CharlieBotConfig(backend_options=options))


def test_account_label_strips_provider_prefix_and_leading_dot() -> None:
  assert _account_label("claude", "/home/u/.claude-invite-1") == "invite-1"
  assert _account_label("codex", "/home/u/.codex-personal") == "personal"


def test_account_label_keeps_non_conventional_basename() -> None:
  assert _account_label("claude", "/home/u/accounts/foo") == "foo"


def test_derive_accounts_always_includes_defaults_and_dedupes_explicit_default(monkeypatch) -> None:
  options = [
      BackendOption(id="cc1", label="a", type="cc-claude", model="m", claude_config_dir="~/.claude"),
      BackendOption(id="cc2", label="b", type="cc-claude", model="m", claude_config_dir="~/.claude-invite-1"),
      BackendOption(id="cx1", label="c", type="codex", model="m", codex_home="~/.codex"),
      BackendOption(id="cx2", label="d", type="codex", model="m", codex_home="~/.codex-personal"),
  ]
  _patch_config(monkeypatch, options)

  accounts = _derive_accounts()

  assert accounts["claude"][0] == ("main", CLAUDE_DEFAULT_DIR)
  assert accounts["codex"][0] == ("main", CODEX_DEFAULT_DIR)
  assert [label for label, _ in accounts["claude"]] == ["main", "invite-1"]
  assert [label for label, _ in accounts["codex"]] == ["main", "personal"]


def test_derive_accounts_label_collision_falls_back_to_full_basename(monkeypatch) -> None:
  options = [
      BackendOption(id="cc1", label="a", type="cc-claude", model="m", claude_config_dir="~/.claude-invite-1"),
      BackendOption(id="cc2", label="b", type="cc-claude", model="m", claude_config_dir="~/x/claude-invite-1"),
  ]
  _patch_config(monkeypatch, options)

  labels = [label for label, _ in _derive_accounts()["claude"]]

  assert labels == ["main", "invite-1", "claude-invite-1"]


def test_derive_accounts_label_collision_skip_fail_loud(monkeypatch) -> None:
  # Two distinct dirs both derive label "invite-1"; the later one's full basename
  # also collides, so it is skipped (logged) rather than overwriting the first.
  options = [
      BackendOption(id="cc1", label="a", type="cc-claude", model="m", claude_config_dir="~/.claude-invite-1"),
      BackendOption(id="cc2", label="b", type="cc-claude", model="m", claude_config_dir="~/accounts/invite-1"),
  ]
  _patch_config(monkeypatch, options)

  labels = [label for label, _ in _derive_accounts()["claude"]]

  assert labels == ["main", "invite-1"]


# ---------------------------------------------------------------------------
# Poller payload semantics (T3): multi-account keys, stale-keep, error
# placeholder, drop-on-removal. Drives the real _poll_loop for N cycles with
# monkeypatched derivation/providers and a sleep that stops the loop.
# ---------------------------------------------------------------------------


class _StopAfter(Exception):
  pass


class _FakeProvider:
  def __init__(self, get_value: Callable[[], Any], error: str = "no data") -> None:
    self._get_value = get_value
    self.last_error = error

  async def fetch(self) -> dict | None:
    value = self._get_value()
    if isinstance(value, Exception):
      raise value
    return value


def _run_poll_cycles(monkeypatch, *, accounts_fn, create_provider, n: int) -> None:
  counter = {"i": 0}

  async def _fake_sleep(_):
    counter["i"] += 1
    if counter["i"] >= n:
      raise _StopAfter()

  async def _noop_broadcast(*a, **k):
    pass

  monkeypatch.setattr(asyncio, "sleep", _fake_sleep)
  monkeypatch.setattr(ext_usage_mod, "streaming_manager", types.SimpleNamespace(broadcast=_noop_broadcast))
  monkeypatch.setattr(ext_usage_mod, "_derive_accounts", accounts_fn)
  monkeypatch.setattr(ext_usage_mod, "_create_provider", create_provider)
  ext_usage_mod._cached_usage.clear()
  ext_usage_mod._instances.clear()

  with pytest.raises(_StopAfter):
    asyncio.run(_poll_loop())


def test_poll_multi_account_keys_and_error_placeholder_for_never_fetched(monkeypatch) -> None:
  main_value = {
      "five_hour": {"utilization": 42.0, "resets_at": ""},
      "seven_day": {"utilization": 1.0, "resets_at": ""},
      "fetched_at": "2026-01-01T00:00:00+00:00",
      "provider": "claude",
  }

  def create_provider(provider, label, dir_path):
    if label == "main":
      return _FakeProvider(lambda: main_value)
    return _FakeProvider(lambda: None, error="credentials not found")

  accounts = {"claude": [("main", "/fake/main"), ("invite-1", "/fake/invite-1")], "codex": []}
  _run_poll_cycles(monkeypatch, accounts_fn=lambda: accounts, create_provider=create_provider, n=1)

  assert set(ext_usage_mod._cached_usage.keys()) == {"claude:main", "claude:invite-1"}
  main = ext_usage_mod._cached_usage["claude:main"]
  assert main["provider"] == "claude"
  assert main["account"] == "main"
  assert main["five_hour"]["utilization"] == 42.0
  assert ext_usage_mod._cached_usage["claude:invite-1"] == {
      "provider": "claude",
      "account": "invite-1",
      "error": "credentials not found",
  }


def test_poll_stale_keep_on_fetch_failure(monkeypatch) -> None:
  fetch_no = {"i": 0}
  original = {
      "five_hour": {"utilization": 42.0, "resets_at": ""},
      "seven_day": {"utilization": 1.0, "resets_at": ""},
      "fetched_at": "2026-01-01T00:00:00+00:00",
      "provider": "claude",
  }

  def create_provider(provider, label, dir_path):
    def get_value():
      fetch_no["i"] += 1
      return original if fetch_no["i"] == 1 else None
    return _FakeProvider(get_value, error="rate limited")

  accounts = {"claude": [("main", "/fake/main")], "codex": []}
  _run_poll_cycles(monkeypatch, accounts_fn=lambda: accounts, create_provider=create_provider, n=2)

  kept = ext_usage_mod._cached_usage["claude:main"]
  assert kept["five_hour"]["utilization"] == 42.0
  assert kept["fetched_at"] == "2026-01-01T00:00:00+00:00"
  assert "error" not in kept


def test_poll_drops_removed_account_on_next_rebuild(monkeypatch) -> None:
  fetch_no = {"i": 0}

  def create_provider(provider, label, dir_path):
    def get_value():
      fetch_no["i"] += 1
      return {
          "five_hour": {"utilization": float(fetch_no["i"]), "resets_at": ""},
          "seven_day": {"utilization": 0.0, "resets_at": ""},
          "fetched_at": "2026-01-01T00:00:00+00:00",
          "provider": "claude",
      }
    return _FakeProvider(get_value)

  call = {"i": 0}
  accounts_by_cycle = {
      1: {"claude": [("main", "/fake/main"), ("invite-1", "/fake/invite-1")], "codex": []},
      2: {"claude": [("main", "/fake/main")], "codex": []},
  }

  def accounts_fn():
    call["i"] += 1
    return accounts_by_cycle[call["i"]]

  _run_poll_cycles(monkeypatch, accounts_fn=accounts_fn, create_provider=create_provider, n=2)

  assert set(ext_usage_mod._cached_usage.keys()) == {"claude:main"}
