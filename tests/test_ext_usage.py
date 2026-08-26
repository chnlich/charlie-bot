import asyncio
import json
import os
import time
import types
from collections.abc import Callable
from datetime import date, datetime, timedelta, timezone
from typing import Any

import pytest

from src.api import ext_usage as ext_usage_mod
from src.api.ext_usage import (
    CLAUDE_DEFAULT_DIR,
    CODEX_DEFAULT_DIR,
    ClaudeUsageProvider,
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
  """A legacy two-window Codex payload: a 5h primary plus a 7d secondary."""
  return {
    "timestamp": timestamp,
    "type": "event_msg",
    "payload": {
      "type": "token_count",
      "rate_limits": {
        "primary": {
          "used_percent": primary_used_percent,
          "window_minutes": 300,
          "resets_at": primary_resets_at,
        },
        "secondary": {
          "used_percent": secondary_used_percent,
          "window_minutes": 10080,
          "resets_at": secondary_resets_at,
        },
      },
    },
  }


def _build_weekly_token_count_event(
    *,
    timestamp: str,
    used_percent: float,
    resets_at: int,
) -> dict:
  """The shape Codex reports today: one weekly window in the primary slot."""
  return {
    "timestamp": timestamp,
    "type": "event_msg",
    "payload": {
      "type": "token_count",
      "rate_limits": {
        "primary": {
          "used_percent": used_percent,
          "window_minutes": 10080,
          "resets_at": resets_at,
        },
        "secondary": None,
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
    "windows": [
      {
        "window_minutes": 300,
        "utilization": 8.0,
        "resets_at": datetime.fromtimestamp(1774653423, tz=timezone.utc).isoformat(),
      },
      {
        "window_minutes": 10080,
        "utilization": 2.0,
        "resets_at": datetime.fromtimestamp(1775240223, tz=timezone.utc).isoformat(),
      },
    ],
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
  assert [w["utilization"] for w in usage["windows"]] == [18.0, 6.0]
  assert [w["window_minutes"] for w in usage["windows"]] == [300, 10080]
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
    "windows": [],
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
    "windows": [],
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
  assert [w["utilization"] for w in usage["windows"]] == [8.0, 2.0]
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


def _patch_config(monkeypatch, options: list[BackendOption]) -> None:
  monkeypatch.setattr(ext_usage_mod, "get_config", lambda: CharlieBotConfig(backend_options=options))


def test_account_label_strips_provider_prefix_and_leading_dot() -> None:
  assert _account_label("claude", "/home/u/.claude-invite-1") == "invite-1"
  assert _account_label("codex", "/home/u/.codex-personal") == "personal"
  assert _account_label("claude", "/home/u/claudepersonal") == "personal"


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

  async def _fake_sleep(_):
    state["sleeps"] += 1
    if state["sleeps"] >= n:
      raise _StopAfter()

  async def _track_broadcast(_channel, event):
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


def test_poll_multi_account_keys_and_error_placeholder_for_never_fetched(monkeypatch) -> None:
  main_value = {
      "windows": [{"window_minutes": 300, "utilization": 42.0, "resets_at": ""}],
      "fetched_at": "2026-01-01T00:00:00+00:00",
      "provider": "claude",
  }

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
  original = {
      "windows": [{"window_minutes": 300, "utilization": 42.0, "resets_at": ""}],
      "fetched_at": "2026-01-01T00:00:00+00:00",
      "provider": "claude",
  }

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
      return {
          "windows": [{"window_minutes": 300, "utilization": float(fetch_no["i"]), "resets_at": ""}],
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
      return {
          "windows": [{"window_minutes": 300, "utilization": 1.0, "resets_at": ""}],
          "fetched_at": "2026-01-01T00:00:00+00:00",
          "provider": "claude",
      }
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
    return _FakeProvider(lambda: {
        "windows": [{"window_minutes": 300, "utilization": 1.0, "resets_at": ""}],
        "fetched_at": "2026-01-01T00:00:00+00:00",
        "provider": "claude",
    })

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
  good = {
      "windows": [{"window_minutes": 300, "utilization": 7.0, "resets_at": ""}],
      "fetched_at": "2026-01-01T00:00:00+00:00",
      "provider": "claude",
  }
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
      1: {"claude": [("main", "/fake/main"), ("a", "/fake/a")], "codex": []},
      2: {"claude": [("main", "/fake/main")], "codex": []},
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
    return _FakeProvider(lambda: {})

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
  good = {
      "windows": [{"window_minutes": 300, "utilization": 1.0, "resets_at": ""}],
      "fetched_at": "2026-01-01T00:00:00+00:00",
      "provider": "claude",
  }
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

def test_extract_latest_codex_usage_reports_weekly_only_shape() -> None:
  """Codex now reports a single weekly window in the primary slot.

  The window is identified by its reported length, so it lands on a 7d entry
  rather than inheriting the meaning of the slot it arrived in.
  """
  fetched_at = "2026-07-20T22:10:00+00:00"
  lines = [json.dumps(_build_weekly_token_count_event(
      timestamp="2026-07-20T22:08:57.925Z",
      used_percent=96.0,
      resets_at=1785016000,
  ))]

  usage = _extract_latest_codex_usage(lines, fetched_at=fetched_at)

  assert usage["windows"] == [{
      "window_minutes": 10080,
      "utilization": 96.0,
      "resets_at": datetime.fromtimestamp(1785016000, tz=timezone.utc).isoformat(),
  }]
  assert "rate_limits_state" not in usage


def test_extract_latest_codex_usage_drops_slot_without_window_minutes() -> None:
  """An unidentifiable window is dropped, never guessed at from slot order."""
  lines = [json.dumps({
    "timestamp": "2026-07-20T22:08:57.925Z",
    "type": "event_msg",
    "payload": {
      "type": "token_count",
      "rate_limits": {
        "primary": {"used_percent": 96.0, "resets_at": 1785016000},
        "secondary": None,
      },
    },
  })]

  usage = _extract_latest_codex_usage(lines, fetched_at="2026-07-20T22:10:00+00:00")

  assert usage["windows"] == []


def test_extract_latest_codex_usage_marks_missing_percentage_unknown() -> None:
  """A window with no reported usage is unknown, not zero."""
  lines = [json.dumps({
    "timestamp": "2026-07-20T22:08:57.925Z",
    "type": "event_msg",
    "payload": {
      "type": "token_count",
      "rate_limits": {
        "primary": {"window_minutes": 10080, "resets_at": 1785016000},
        "secondary": None,
      },
    },
  })]

  usage = _extract_latest_codex_usage(lines, fetched_at="2026-07-20T22:10:00+00:00")

  assert usage["windows"] == [{
      "window_minutes": 10080,
      "utilization": None,
      "resets_at": datetime.fromtimestamp(1785016000, tz=timezone.utc).isoformat(),
  }]


def test_transform_response_marks_missing_claude_percentage_unknown() -> None:
  usage = _transform_response({"fiveHour": {}, "sevenDay": {"utilization": 10.0}})

  assert [w["utilization"] for w in usage["windows"]] == [None, 10.0]


def test_transform_scoped_limits_each_reading_bound_to_its_own_source() -> None:
  """Every reading renders as its own window, keyed by its own limit.

  Each window's percent is distinct so a swap between two bars becomes
  observable, and the scoped model name appears nowhere in the source so a
  hardcoded label fails.
  """
  raw = {
    "fiveHour": {"utilization": 11.0, "resetsAt": "2026-08-04T12:00:00+00:00"},
    "sevenDay": {"utilization": 22.0, "resetsAt": "2026-08-04T19:00:00+00:00"},
    "limits": [
      {"kind": "weekly_scoped", "group": "weekly", "percent": 33.0,
       "resets_at": "2026-08-04T19:00:00+00:00",
       "scope": {"model": {"id": None, "display_name": "Nimbus"}, "surface": None}},
    ],
  }

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
  raw = {
    "fiveHour": {"utilization": 11.0, "resetsAt": "2026-08-04T12:00:00+00:00"},
    "sevenDay": {"utilization": 22.0, "resetsAt": "2026-08-04T19:00:00+00:00"},
    "limits": [
      {"kind": "weekly_scoped", "group": "weekly", "percent": 33.0,
       "resets_at": "2026-08-04T19:00:00+00:00",
       "scope": {"model": {"id": None, "display_name": "Nimbus"}, "surface": None}},
    ],
  }

  with_limits = _transform_response(raw, account="main")["windows"]
  without = dict(raw)
  without.pop("limits")
  no_limits = _transform_response(without, account="main")["windows"]

  scoped = [w for w in with_limits if "scope_label" in w]
  unscoped_with = [w for w in with_limits if "scope_label" not in w]
  assert len(scoped) == 1
  assert unscoped_with == no_limits


def test_transform_response_scopes_are_sorted_before_planwide() -> None:
  raw = {
    "fiveHour": {"utilization": 11.0, "resetsAt": "2026-08-04T12:00:00+00:00"},
    "sevenDay": {"utilization": 22.0, "resetsAt": "2026-08-04T19:00:00+00:00"},
    "limits": [
      {"group": "weekly", "percent": 33.0, "resets_at": "",
       "scope": {"model": {"display_name": "Nimbus"}}},
      {"group": "weekly", "percent": 44.0, "resets_at": "",
       "scope": {"model": {"display_name": "Fable"}}},
    ],
  }

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

  raw = {
    "fiveHour": {"utilization": 11.0, "resetsAt": "2026-08-04T12:00:00+00:00"},
    "sevenDay": {"utilization": 22.0, "resetsAt": "2026-08-04T19:00:00+00:00"},
    "limits": [
      {"kind": "weekly_scoped", "group": "bogus", "percent": 33.0, "resets_at": "",
       "scope": {"model": {"display_name": "Nimbus"}}},
      {"kind": "weekly_scoped", "group": "weekly", "percent": 44.0, "resets_at": "",
       "scope": {"model": {}}},
    ],
  }

  windows = _transform_response(raw, account="main")["windows"]

  assert all("scope_label" not in w for w in windows)
  events = [w["event"] for w in warns if w["event"] == "ext_usage_unknown_limit_shape"]
  assert events == ["ext_usage_unknown_limit_shape", "ext_usage_unknown_limit_shape"]


def test_transform_response_absent_limits_produces_exactly_today_windows() -> None:
  raw = {
    "fiveHour": {"utilization": 11.0, "resetsAt": "2026-08-04T12:00:00+00:00"},
    "sevenDay": {"utilization": 22.0, "resetsAt": "2026-08-04T19:00:00+00:00"},
  }

  assert _transform_response(raw, account="main")["windows"] == [
    {"window_minutes": 300, "utilization": 11.0, "resets_at": "2026-08-04T12:00:00+00:00"},
    {"window_minutes": 10080, "utilization": 22.0, "resets_at": "2026-08-04T19:00:00+00:00"},
  ]


@pytest.mark.asyncio
async def test_codex_provider_fetch_reads_newest_rollout_beyond_three_days(tmp_path) -> None:
  """No date cliff: the last known reading stays visible however old it is.

  Under a weekly window a reading from days ago is the only information there
  is; its age is reported rather than used to hide it.
  """
  provider = CodexUsageProvider(label="personal", home_dir=str(tmp_path))
  now = datetime.now(timezone.utc)
  old_day = now - timedelta(days=9)
  rollout_dir = (tmp_path / "sessions" / f"{old_day.year:04d}" / f"{old_day.month:02d}" /
                 f"{old_day.day:02d}")
  rollout_dir.mkdir(parents=True)

  rollout_path = rollout_dir / "rollout-old.jsonl"
  rollout_path.write_text(
      json.dumps(_build_weekly_token_count_event(
          timestamp=old_day.isoformat().replace("+00:00", "Z"),
          used_percent=96.0,
          resets_at=int(now.timestamp()) + 3600,
      )) + "\n"
  )
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


def test_compute_codex_spend_windows_accepts_a_prebuilt_file_list(tmp_path) -> None:
  """The usage scrape and the spend aggregation share one directory walk."""
  now = datetime(2026, 6, 1, 12, 0, 0, tzinfo=timezone.utc)
  rollout_dir = tmp_path / "2026" / "06" / "01"
  rollout_dir.mkdir(parents=True)
  rollout_path = rollout_dir / "rollout-shared.jsonl"
  rollout_path.write_text("\n".join([
      json.dumps({"type": "turn_context", "payload": {"model": "gpt-5.3-codex"}}),
      json.dumps({
          "timestamp": now.isoformat(),
          "type": "event_msg",
          "payload": {
              "type": "token_count",
              "info": {
                  "last_token_usage": {
                      "input_tokens": 1000,
                      "cached_input_tokens": 0,
                      "output_tokens": 100,
                  },
              },
          },
      }),
  ]) + "\n")
  os.utime(rollout_path, (now.timestamp(), now.timestamp()))

  from_list = _compute_codex_spend_windows(rollout_paths=[rollout_path], now=now)
  from_walk = _compute_codex_spend_windows(sessions_dir=tmp_path, now=now)

  assert from_list == from_walk
  assert from_list["last_7d_usd"] > 0.0


def test_poll_seeds_pending_rows_so_no_account_is_missing_from_the_first_broadcast(monkeypatch) -> None:
  """A restart must not hide accounts the round-robin has not reached yet.

  One fetch per round gap means the last account is N-1 gaps behind the first, so
  a strip built only from fetched accounts silently omits real accounts for
  minutes after every restart — always the same ones, since the round order is
  fixed. Seeding costs no extra request: the placeholders ride along in the
  broadcast the first real fetch already sends.
  """
  main_value = {
      "windows": [{"window_minutes": 300, "utilization": 42.0, "resets_at": ""}],
      "fetched_at": "2026-01-01T00:00:00+00:00",
      "provider": "claude",
  }

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
    "five_hour": {"utilization": 12.0, "resets_at": "2026-07-29T23:00:00+00:00"},
    "seven_day": {"utilization": 34.0, "resets_at": "2026-08-03T19:00:00+00:00"},
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

  def __init__(self, get_statuses: list[int], *, renewal: dict | None = None,
               on_get: Callable[[int], None] | None = None,
               renewal_status: int = 200, renewal_body: str = "") -> None:
    self._get_statuses = list(get_statuses)
    self._renewal = renewal if renewal is not None else {"access_token": "tok-new",
                                                         "refresh_token": "ref-new",
                                                         "expires_in": 28800}
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


def _write_credentials(path, *, access="tok-stored", refresh="ref-stored",
                       expires_at=_STALE_EXPIRES_AT_MS) -> None:
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
  assert fake.posts == []
  assert fake.gets[1]["headers"]["Authorization"] == "Bearer tok-from-cli"


@pytest.mark.asyncio
async def test_claude_fetch_backs_off_after_a_second_401_and_then_issues_no_request(
    monkeypatch, tmp_path) -> None:
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
  assert fake.posts == []
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
async def test_claude_renewal_failure_arms_backoff_and_reports_token_refresh_failed(
    monkeypatch, tmp_path) -> None:
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
async def test_claude_rate_limited_arms_backoff_and_reports_rate_limited(
    monkeypatch, tmp_path) -> None:
  fake = _FakeUsageHTTP([429])
  provider = _claude_provider(monkeypatch, tmp_path, fake)

  assert await provider.fetch() is None
  assert provider.last_error == "rate limited"
  assert provider._backoff_until > time.time()

  assert await provider.fetch() is None
  assert provider.last_error == "rate limited"


@pytest.mark.asyncio
async def test_claude_renewal_succeeds_but_retried_get_401_reports_auth_rejected(
    monkeypatch, tmp_path) -> None:
  fake = _FakeUsageHTTP([401, 401])
  provider = _claude_provider(monkeypatch, tmp_path, fake)

  assert await provider.fetch() is None
  assert provider.last_error == "auth rejected"
  assert provider._backoff_until > time.time()
