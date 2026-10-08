import asyncio
import json
import types
from collections.abc import Callable
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest
from conftest import codex_token_count_event, fresh_state_fixture

from src.backends.claude_code.claude_config import ClaudeAccount
from src.features.usage import ext_usage as ext_usage_mod
from src.features.usage.ext_usage import (
    ClaudeUsageProvider,
    _derive_accounts,
    _extract_codex_spend_events,
    _latest_token_count_event,
    _poll_loop,
    _sum_codex_spend_events,
    _transform_codex_response,
)
from src.infra.config import CharlieBotConfig

_fresh_unknown_limit_shape_registry = fresh_state_fixture(ext_usage_mod._UNKNOWN_LIMIT_SHAPES_SEEN.clear)
_fresh_credential_read_warning_registry = fresh_state_fixture(ext_usage_mod._CREDENTIAL_READ_WARNINGS_SEEN.clear)
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


# One row per raw rate_limits payload shape a Codex token_count event can
# carry, and the usage dict the transform must report for it: business
# metadata states the unlimited state, metadata-less null buckets stay
# unstated, an unidentifiable slot is dropped rather than guessed at from
# slot order, and a window with no reported usage is unknown, not zero. The
# credits rows pin the metered balance reading: forwarded as a JSON number,
# stateless, and shaped strictly (an unreadable credits object is dropped
# with a warning, never guessed at).


def _capture_warnings(monkeypatch: pytest.MonkeyPatch) -> list[dict]:
  """Capture ext_usage's warning lines in order as ``{"event": name, **fields}`` dicts."""
  warns: list[dict] = []
  monkeypatch.setattr(ext_usage_mod.log, "warning", lambda event, **kw: warns.append({"event": event, **kw}))
  return warns


def test_codex_usage_transform_drops_unreadable_credits_and_warns(monkeypatch: pytest.MonkeyPatch) -> None:
  """An unreadable credits shape emits no payload and no state, and warns instead.

  ``plan_type == "business"`` rides along in every case to prove a present
  credits key blocks the unlimited fallback even when the credits object
  itself cannot be read.
  """
  warns = _capture_warnings(monkeypatch)

  def transform(raw_credits: Any) -> dict:
    rate_limits = {"primary": None, "secondary": None, "plan_type": "business", "credits": raw_credits}
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


def test_spend_aggregation_prices_recent_turns_by_model(tmp_path: Path) -> None:
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


# ---------------------------------------------------------------------------
# Account-set derivation (T2)
# ---------------------------------------------------------------------------


def test_derive_accounts_label_collision_skip_fail_loud(monkeypatch: pytest.MonkeyPatch) -> None:
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
  monkeypatch.setattr("src.infra.config.get_config", lambda: cfg)

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


def _run_poll_cycles(
    monkeypatch: pytest.MonkeyPatch,
    *,
    accounts_fn: Callable[[], dict],
    create_provider: Callable[[str, str, str], _FakeProvider],
    n: int,
) -> dict:
  """Drive the real ``_poll_loop`` for ``n`` round-gap sleeps, then return counters.

  Under round-robin scheduling one sleep == one single-account fetch, so a full
  round of N accounts takes N sleeps (plus one sleep per empty round).
  """
  state: dict = {"sleeps": 0, "broadcasts": 0, "payloads": []}

  async def _fake_sleep(delay: float) -> None:
    state["sleeps"] += 1
    if state["sleeps"] >= n:
      raise _StopAfter

  async def _track_broadcast(thread_id: str, event: dict[str, Any]) -> None:
    state["broadcasts"] += 1
    state["payloads"].append(event)

  monkeypatch.setattr(asyncio, "sleep", _fake_sleep)
  monkeypatch.setattr("src.runtime.streaming.streaming_manager", types.SimpleNamespace(broadcast=_track_broadcast))
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


def test_poll_stale_keep_on_fetch_failure(monkeypatch: pytest.MonkeyPatch) -> None:
  fetch_no = {"i": 0}
  original = _claude_fetch_value(42.0)

  def create_provider(provider: str, label: str, dir_path: str) -> _FakeProvider:

    def get_value() -> dict | None:
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

  async def get(self, url: str, headers: dict | None = None, timeout: float | None = None) -> _FakeResponse:
    self.gets.append({"url": url, "headers": dict(headers or {})})
    status = self._get_statuses.pop(0)
    if self._on_get is not None:
      self._on_get(len(self.gets))
    return _FakeResponse(status, _CLAUDE_USAGE_PAYLOAD if status == 200 else {})

  async def post(
      self,
      url: str,
      json: dict | None = None,
      headers: dict | None = None,
      timeout: float | None = None) -> _FakeResponse:
    self.posts.append({"url": url, "json": dict(json or {}), "headers": dict(headers or {})})
    return _FakeResponse(self._renewal_status, self._renewal, text=self._renewal_body)


def _write_credentials(path: Path, *, access: str = "tok-stored", refresh: str = "ref-stored") -> None:
  payload = {"claudeAiOauth": {"accessToken": access, "refreshToken": refresh, "expiresAt": _STALE_EXPIRES_AT_MS}}
  path.write_text(json.dumps(payload))


def _claude_provider(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, fake: _FakeUsageHTTP, **creds: Any) -> ClaudeUsageProvider:
  credentials_path = tmp_path / ".credentials.json"
  _write_credentials(credentials_path, **creds)
  monkeypatch.setattr("src.infra.http.get_http_client", lambda: fake)
  return ClaudeUsageProvider("ext-test", credentials_path)


@pytest.mark.asyncio
async def test_claude_fetch_renews_once_and_retries_once_after_401(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
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
