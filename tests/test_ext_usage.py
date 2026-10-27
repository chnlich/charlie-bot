import datetime
import json
import pathlib
from collections.abc import Callable
from typing import Any

import conftest
import pytest

from src.backends.claude_code import claude_config
from src.backends.claude_code import usage_quota as claude_quota
from src.backends.codex import usage_quota as codex_quota
from src.infra import config
from src.runtime.hooks import usage_sources

# Seed the cache with the fallback so no test probes the real binary.


def _build_token_count_event(
    *,
    timestamp: str,
    primary_used_percent: float,
    primary_resets_at: int,
    secondary_used_percent: float,
    secondary_resets_at: int,
) -> dict:
  """A legacy two-window Codex payload: a 5h primary plus a 7d secondary."""
  return conftest.codex_token_count_event(
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
  return conftest.codex_token_count_event(
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

  event = codex_quota._latest_token_count_event(lines)
  assert event is not None
  usage = codex_quota._transform_codex_response(event, fetched_at=fetched_at)

  assert usage == {
      "windows":
          [
              {
                  "window_minutes": 300,
                  "utilization": 8.0,
                  "resets_at": datetime.datetime.fromtimestamp(1774653423, tz=datetime.UTC).isoformat(),
              },
              {
                  "window_minutes": 10080,
                  "utilization": 2.0,
                  "resets_at": datetime.datetime.fromtimestamp(1775240223, tz=datetime.UTC).isoformat(),
              },
          ],
      "fetched_at": fetched_at,
      "provider": "codex",
      "token_count_observed_at": "2026-03-27T18:29:35.694Z",
  }
  assert "state_label" not in usage


# One row per raw rate_limits payload shape a Codex token_count event can
# carry, and the usage dict the transform must report for it: business
# metadata states the unlimited state, metadata-less null buckets stay
# unstated, an unidentifiable slot is dropped rather than guessed at from
# slot order, and a window with no reported usage is unknown, not zero. The
# credits rows pin the metered balance reading: forwarded as a JSON number,
# stateless, and shaped strictly (an unreadable credits object is dropped
# with a warning, never guessed at).


def _capture_warnings(monkeypatch: pytest.MonkeyPatch) -> list[dict]:
  """Capture the unknown-limit-shape warning lines in order as ``{"event": name, **fields}`` dicts."""
  warns: list[dict] = []
  monkeypatch.setattr(usage_sources.log, "warning", lambda event, **kw: warns.append({"event": event, **kw}))
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
    event = conftest.codex_token_count_event("2026-03-27T18:39:35.694Z", rate_limits=rate_limits)
    return codex_quota._transform_codex_response(event, fetched_at="2026-03-27T18:40:00+00:00")

  for unreadable in ("yes", {"unlimited": "false"}):
    usage = transform(unreadable)
    assert "credits" not in usage
    assert "state_label" not in usage
  metered = transform({"unlimited": False, "balance": "n/a"})
  assert metered["credits"] == {"unlimited": False}
  assert "state_label" not in metered

  reasons = [w["reason"] for w in warns if w["event"] == "ext_usage_unknown_limit_shape"]
  assert reasons == ["credits is not an object", "unlimited is not a bool", "missing or unparseable balance"]


def test_spend_aggregation_prices_recent_turns_by_model(tmp_path: pathlib.Path) -> None:
  now = datetime.datetime(2026, 6, 1, 12, 0, 0, tzinfo=datetime.UTC)
  rollout_path = tmp_path / "rollout-recent.jsonl"

  events = [
      conftest.codex_turn_context_event(model="gpt-5.5"),
      _build_spend_token_count_event(
          timestamp=(now - datetime.timedelta(hours=2)).isoformat().replace("+00:00", "Z"),
          input_tokens=1_000_000,
          cached_input_tokens=100_000,
          output_tokens=10_000),
      _build_spend_token_count_event(
          timestamp=(now - datetime.timedelta(days=2)).isoformat().replace("+00:00", "Z"),
          input_tokens=2_000_000,
          cached_input_tokens=500_000,
          output_tokens=20_000),
      _build_spend_token_count_event(
          timestamp=(now - datetime.timedelta(days=8)).isoformat().replace("+00:00", "Z"),
          input_tokens=5_000_000,
          cached_input_tokens=0,
          output_tokens=100_000),
  ]
  rollout_path.write_text("\n".join(json.dumps(event) for event in events) + "\n")

  extracted = codex_quota._extract_codex_spend_events(rollout_path)
  assert extracted is not None
  spend = codex_quota._sum_codex_spend_events([extracted], now=now)

  assert spend["last_24h_usd"] == pytest.approx(4.85)
  assert spend["last_7d_usd"] == pytest.approx(13.20)


# ---------------------------------------------------------------------------
# Account-set derivation: quota_accounts() reads the live config on every call
# ---------------------------------------------------------------------------


def _use_config(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, *pool: claude_config.ClaudeAccount) -> None:
  """Serve a config whose Claude pool is *pool*, and a HOME under tmp_path so no real login is named."""
  monkeypatch.setenv("HOME", str(tmp_path))
  monkeypatch.setattr(
      conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: config.CharlieBotConfig(accounts={"claude": list(pool)}))


def test_claude_accounts_label_collision_skip_fail_loud(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  # Two distinct dirs both labelled "invite-1": the later one is skipped (logged)
  # rather than overwriting the first.
  _use_config(
      monkeypatch,
      tmp_path,
      claude_config.ClaudeAccount(label="invite-1", config_dir="~/.claude-invite-1"),
      claude_config.ClaudeAccount(label="invite-1", config_dir="~/accounts/invite-1"),
  )

  labels = [account.label for account in claude_quota.quota_accounts()]

  assert labels == ["main", "invite-1"]


def test_claude_accounts_keep_their_backoff_between_rounds(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  """A login that stays in the config comes back as the same account with its backoff armed; a login the
  config dropped is forgotten, and returns fresh."""
  invite = claude_config.ClaudeAccount(label="invite-1", config_dir=str(tmp_path / ".claude-invite-1"))
  _use_config(monkeypatch, tmp_path, invite)
  main, invite_account = claude_quota.quota_accounts()
  invite_account._arm_backoff()
  armed_until = invite_account._backoff_until

  main_again, invite_again = claude_quota.quota_accounts()
  assert (main_again, invite_again) == (main, invite_account)
  assert invite_again._backoff_until == armed_until > 0

  _use_config(monkeypatch, tmp_path)
  assert [account.label for account in claude_quota.quota_accounts()] == ["main"]
  _use_config(monkeypatch, tmp_path, invite)
  _, readded = claude_quota.quota_accounts()
  assert readded is not invite_account
  assert readded._backoff_until == 0.0


def test_claude_account_marks_login_and_expired_windows(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  """A pool login without credentials carries ``login_required`` with its directory; a window whose reset has
  passed since the sample carries ``expired`` on an emit copy, and the cached entry stays as fetched."""
  pool_dir = tmp_path / ".claude-invite-1"
  _use_config(monkeypatch, tmp_path, claude_config.ClaudeAccount(label="invite-1", config_dir=str(pool_dir)))
  _, account = claude_quota.quota_accounts()
  entry = {
      "provider": "claude",
      "account": "invite-1",
      "fetched_at": "2026-01-01T00:00:00+00:00",
      "windows": [
          {"window_minutes": 300, "utilization": 5.0, "resets_at": "2026-01-01T03:00:00+00:00"},
          {"window_minutes": 10080, "utilization": 6.0, "resets_at": "2026-01-07T00:00:00+00:00"},
      ],
  }  # yapf: disable

  account.mark_login_required(entry)
  assert entry["login_required"] == str(pool_dir)
  pool_dir.mkdir()
  _write_credentials(pool_dir / ".credentials.json")
  account.mark_login_required(entry)
  assert "login_required" not in entry

  emitted = account.mark_expired(entry, datetime.datetime(2026, 1, 2, tzinfo=datetime.UTC))
  assert [w.get("expired") for w in emitted["windows"]] == [True, None]
  assert all("expired" not in w for w in entry["windows"])
  main_account, _ = claude_quota.quota_accounts()
  assert main_account.login_dir is None


# ---------------------------------------------------------------------------
# Poller payload semantics (T3): multi-account keys, stale-keep, error
# placeholder, drop-on-removal. Drives the real _poll_loop for N cycles with
# fake accounts and a sleep that stops the loop.
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# ClaudeQuotaAccount: 401-triggered renewal. The provider owns no clock; the
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


def _write_credentials(path: pathlib.Path, *, access: str = "tok-stored", refresh: str = "ref-stored") -> None:
  payload = {"claudeAiOauth": {"accessToken": access, "refreshToken": refresh, "expiresAt": _STALE_EXPIRES_AT_MS}}
  path.write_text(json.dumps(payload))


def _claude_provider(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, fake: _FakeUsageHTTP,
    **creds: Any) -> claude_quota.ClaudeQuotaAccount:
  credentials_path = tmp_path / ".credentials.json"
  _write_credentials(credentials_path, **creds)
  monkeypatch.setattr(conftest.HTTP_GET_HTTP_CLIENT_PATCH_TARGET, lambda: fake)
  return claude_quota.ClaudeQuotaAccount("ext-test", credentials_path)


@pytest.mark.asyncio
async def test_claude_fetch_renews_once_and_retries_once_after_401(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
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
