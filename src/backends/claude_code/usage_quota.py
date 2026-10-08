"""Claude Code's quota accounts for the quota panel (src/runtime/hooks/usage_sources.py).

``usage_logs.quota_accounts()`` imports this module on its first call, so a usage capture and
``charliebot --help`` never load it. One account reads one login directory's usage from the
Anthropic OAuth usage endpoint. The accounts are the default login directory (label ``main``)
followed by the ``accounts.claude`` pool, and every call derives them again from the live
config: an account whose directory stays keeps its object, and with it its 429 backoff.

The account also feeds the panel reading to ``claude_accounts`` (which ranks logins by their
newest reading) and marks its cached entry with ``login_required`` and the expired windows.
"""

import asyncio
import datetime
import json
import os
import pathlib
import re
import subprocess
import time
from typing import Any

from src.backends.claude_code import claude_accounts, login_dirs
from src.backends.claude_code.claude_config import ClaudeAccount
from src.infra import config, http, json_utils, log_once, models, timeouts
from src.runtime.hooks import usage_sources

log = log_once.LazyStructlogLogger()

USAGE_URL = "https://api.anthropic.com/api/oauth/usage"
TOKEN_REFRESH_URL = "https://platform.claude.com/v1/oauth/token"
CLIENT_ID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
ANTHROPIC_BETA = "oauth-2025-04-20"
# The usage endpoint rate-limits per access token, reportedly far more
# generously for the Claude Code user agent than for an unrecognized one. The
# User-Agent itself is resolved at runtime from the installed CLI (_user_agent
# below); this constant is only the fallback for when that probe fails.
USER_AGENT_FALLBACK = "claude-code/2.1.219"

CLAUDE_WINDOW_FIELDS = (
    ("fiveHour", "five_hour", 300),
    ("sevenDay", "seven_day", 10080),
)

# limits[].group -> window length in minutes. Both values the endpoint reports
# today, kept as a table rather than an if/else so an unknown group is skipped
# with a warning instead of guessed at from array position.
LIMIT_GROUP_WINDOW_MINUTES = {"session": 300, "weekly": 10080}

# ---------------------------------------------------------------------------
# Account-set derivation: run by every quota_accounts() call.
# Reads get_config(), which is mtime-cached, so config.yaml edits take effect
# on the next round without a server restart.
# ---------------------------------------------------------------------------


def _derive_login_dirs(default_dir: str, pool: list[tuple[str, str]]) -> list[tuple[str, str]]:
  """Return ordered [(label, expanded_abs_path)] of the Claude logins.

  The default dir comes first (label 'main'), then the configured pool accounts
  (``accounts.claude``: (label, dir) pairs) under their own labels. Dedupes by
  expanded absolute path, so an entry explicitly pointing at the default
  collapses into the default. Label collisions fail loud (skip) — never
  silently overwriting an existing key.
  """
  default_expanded = os.path.abspath(os.path.expanduser(default_dir))
  seen: set[str] = {default_expanded}
  labels: set[str] = {"main"}
  accounts: list[tuple[str, str]] = [("main", default_expanded)]

  for label, raw in pool:
    expanded = os.path.abspath(os.path.expanduser(raw))
    if expanded in seen:
      continue
    if label in labels:
      log.error("ext_usage_account_label_collision_skip", provider="claude", dir=expanded, label=label)
      continue
    seen.add(expanded)
    labels.add(label)
    accounts.append((label, expanded))

  return accounts


# ---------------------------------------------------------------------------
# Credentials helpers
# ---------------------------------------------------------------------------

# The poller re-reads the credentials file every round, so one sighting of a
# missing or tokenless file is the whole alarm; every later round in the same
# broken streak repeats a fired alarm. A read that returns a token re-arms the
# path: a relapse after it is a new onset, not a repeat.
_CREDENTIAL_READ_WARNINGS_SEEN = log_once.WarnOnceRegistry()


def _warn_credential_read_once(event: str, credentials_path: pathlib.Path) -> None:
  """Log one *event* per credentials path until a read of it returns a token.

  A caller relies on at most one line per (event, path) per broken streak: the
  poller's next round re-reading an unchanged broken file is not a new state,
  while a relapse after a successful read is.
  """
  path = str(credentials_path)
  _CREDENTIAL_READ_WARNINGS_SEEN.log(log.warning, event, (event, path), path=path)


def _read_credentials(credentials_path: pathlib.Path) -> dict[str, Any] | None:
  """Read OAuth credentials from a Claude account's .credentials.json."""
  if not credentials_path.exists():
    _warn_credential_read_once("ext_usage_credentials_not_found", credentials_path)
    return None

  data = json.loads(credentials_path.read_text(encoding="utf-8"))
  oauth = data.get("claudeAiOauth", {})
  access_token = oauth.get("accessToken")
  refresh_token = oauth.get("refreshToken")

  if not access_token:
    _warn_credential_read_once("ext_usage_no_access_token", credentials_path)
    return None

  _CREDENTIAL_READ_WARNINGS_SEEN.forget_where(lambda key: key[1] == str(credentials_path))
  # expiresAt is deliberately not read: token renewal keys off the server's 401
  # in ClaudeQuotaAccount.fetch, never a local expiry check.
  return {
      "access_token": access_token,
      "refresh_token": refresh_token,
  }


# ---------------------------------------------------------------------------
# The account
# ---------------------------------------------------------------------------


class ClaudeQuotaAccount(usage_sources.QuotaAccount):
  """Fetches usage data from the Anthropic OAuth usage endpoint for one login directory.

  ``login_dir`` is the pool entry's directory, or None for an account that is not a pool entry;
  only a pool entry carries the ``login_required`` mark.
  """

  provider = "claude"

  def __init__(self, label: str, credentials_path: pathlib.Path) -> None:
    self.label = label
    self.credentials_path = pathlib.Path(credentials_path)
    self.login_dir: str | None = None
    self._backoff_seconds = 0.0
    self._backoff_until = 0.0
    self.last_error = "no data"

  async def fetch(self) -> dict[str, Any] | None:
    if time.time() < self._backoff_until:
      # Last_error keeps whatever armed the backoff; a backed-off account is
      # silent and reports the cause, not that it was skipped this round.
      return None

    creds = await asyncio.to_thread(_read_credentials, self.credentials_path)
    if creds is None:
      self.last_error = "credentials not found"
      return None

    resp = await self._get_usage(creds["access_token"])

    # A 401 is the only authoritative signal that the stored token is unusable: it
    # covers expiry and revocation alike and needs no clock arithmetic to be trusted.
    # Renew once and retry once; a second 401 is an account state the next poll cannot
    # fix either, so it backs off instead of retrying every round.
    if resp.status_code == 401:
      access_token = await self._reauthenticate(creds["access_token"])
      if access_token is None:
        self._arm_backoff()
        return None
      resp = await self._get_usage(access_token)
      if resp.status_code == 401:
        self.last_error = "auth rejected"
        self._arm_backoff()
        log.warning("ext_usage_auth_rejected_after_renewal", account=self.label, backoff_seconds=self._backoff_seconds)
        return None

    if resp.status_code == 429:
      self.last_error = "rate limited"
      self._arm_backoff()
      log.warning("ext_usage_rate_limited", account=self.label, backoff_seconds=self._backoff_seconds)
      return None

    self._backoff_seconds = 0.0
    resp.raise_for_status()
    usage = _transform_response(resp.json(), account=self.label)
    # The account pool ranks logins by their newest reading; the panel
    # poll is the reading source between Claude Code runs.
    claude_accounts.observe_usage_panel(self.label, usage)
    return usage

  def mark_login_required(self, entry: dict[str, Any]) -> None:
    """Mark a pool account's entry with the login directory while it needs a new login.

    The pool is the judge (empty credential store or a recent authentication
    failure); the chat notice for the same condition names no account, so this
    marker is where the operator learns which directory to `claude /login` in.
    """
    if self.login_dir is None:
      return
    if claude_accounts.healthy(ClaudeAccount(label=self.label, config_dir=self.login_dir)):
      entry.pop("login_required", None)
    else:
      entry["login_required"] = self.login_dir

  def mark_expired(self, entry: dict[str, Any], now: datetime.datetime) -> dict[str, Any]:
    """The entry with each window the shared ``claude_accounts.panel_window_expired`` rule marks expired at *now*.

    Expired windows carry ``expired: true`` and live windows carry no key. An entry without
    windows (a pending marker or an error) comes back unchanged.
    """
    windows = entry.get(usage_sources.PANEL_WINDOWS)
    if not isinstance(windows, list):
      return entry
    sampled = claude_accounts.parse_iso_utc(entry.get(usage_sources.PANEL_FETCHED_AT))
    annotated = [
        {
            **window, "expired": True
        } if claude_accounts.panel_window_expired(window, sampled, now) else window for window in windows
    ]
    return {**entry, usage_sources.PANEL_WINDOWS: annotated}

  async def _get_usage(self, access_token: str) -> Any:
    client = http.get_http_client()
    return await client.get(USAGE_URL, headers=await _oauth_headers(access_token), timeout=timeouts.HTTP_OAUTH_TIMEOUT)

  async def _reauthenticate(self, failed_token: str) -> str | None:
    """Return a usable access token after a 401, yielding to whoever renewed first.

    The credentials file is shared with the external Claude CLI, which rotates the
    refresh token whenever it renews. Re-reading the file first means the common case
    -- the CLI renewed while this poller held a stale copy -- spends no refresh call
    and rotates nothing out from under it.
    """
    creds = await asyncio.to_thread(_read_credentials, self.credentials_path)
    if creds is None:
      self.last_error = "credentials not found"
      return None
    if creds["access_token"] != failed_token:
      return creds["access_token"]
    if not creds["refresh_token"]:
      log.warning("ext_usage_no_refresh_token", account=self.label)
      self.last_error = "token refresh failed"
      return None
    access_token = await _refresh_access_token(self.credentials_path, creds["refresh_token"])
    if access_token is None:
      self.last_error = "token refresh failed"
      return None
    return access_token

  def _arm_backoff(self) -> None:
    """Advance the shared backoff ladder: 60s first, doubling, capped at 30 minutes."""
    self._backoff_seconds = min((self._backoff_seconds or 30) * 2, 30 * 60)
    self._backoff_until = time.time() + self._backoff_seconds


# Account objects by expanded login dir, kept across quota_accounts() calls so a 429 backoff survives.
_accounts: dict[str, ClaudeQuotaAccount] = {}


def quota_accounts() -> list[usage_sources.QuotaAccount]:
  """The Claude logins in panel order: the default login, then the ``accounts.claude`` pool.

  A login whose directory is in the config keeps its account object; one that left the config
  is forgotten.
  """
  cfg = config.get_config()
  pool = [(account.label, account.config_dir) for account in cfg.accounts.claude]
  pool_dirs = {label: os.path.abspath(os.path.expanduser(raw)) for label, raw in pool}
  logins = _derive_login_dirs(str(login_dirs.default_claude_dir()), pool)
  accounts: list[usage_sources.QuotaAccount] = []
  for label, dir_path in logins:
    account = _accounts.get(dir_path)
    if account is None:
      account = ClaudeQuotaAccount(label, pathlib.Path(dir_path) / login_dirs.CREDENTIALS_FILE)
      _accounts[dir_path] = account
    account.label = label
    account.login_dir = pool_dirs.get(label)
    accounts.append(account)
  live_dirs = {dir_path for _, dir_path in logins}
  for dir_path in list(_accounts):
    if dir_path not in live_dirs:
      del _accounts[dir_path]
  return accounts


# ---------------------------------------------------------------------------
# User-Agent: the installed CLI's version, probed once per process
# ---------------------------------------------------------------------------

# None until the first OAuth/token request triggers the probe; holds the
# resolved User-Agent afterwards so later requests cost no subprocess.
_user_agent_cache: str | None = None

_CLI_VERSION_RE = re.compile(r"\d+\.\d+\.\d+")


def _extract_cli_version(output: str) -> str | None:
  """The first ``\\d+\\.\\d+\\.\\d+`` token in *output*, or None when absent."""
  match = _CLI_VERSION_RE.search(output)
  return match.group(0) if match else None


async def _probe_user_agent() -> tuple[str, str]:
  """One ``claude --version`` probe, as (user_agent, source).

  No shell, PATH lookup. A missing binary, a non-zero exit, a probe timeout,
  or output without a version all fall back to ``USER_AGENT_FALLBACK`` with
  source "fallback"; only a parsed version returns source "probe".
  """
  try:
    proc = await asyncio.to_thread(
        subprocess.run, ["claude", "--version"], capture_output=True, timeout=timeouts.EXT_USAGE_VERSION_PROBE_TIMEOUT)
  except (OSError, subprocess.SubprocessError):
    return USER_AGENT_FALLBACK, "fallback"
  if proc.returncode != 0:
    return USER_AGENT_FALLBACK, "fallback"
  version = _extract_cli_version(proc.stdout.decode(errors="replace"))
  if version is None:
    return USER_AGENT_FALLBACK, "fallback"
  return f"claude-code/{version}", "probe"


async def _user_agent() -> str:
  """The User-Agent every OAuth request carries: the installed CLI's version.

  Probed lazily on the first OAuth/token request and cached module-level, so
  the subprocess cost is paid once per process. The one resolution is logged
  as ``ext_usage_user_agent_resolved``; a fallback is the loud warning path.
  """
  global _user_agent_cache
  if _user_agent_cache is None:
    user_agent, source = await _probe_user_agent()
    _user_agent_cache = user_agent
    version = user_agent.removeprefix("claude-code/")
    if source == "fallback":
      log.warning("ext_usage_user_agent_resolved", version=version, source=source)
    else:
      log.info("ext_usage_user_agent_resolved", version=version, source=source)
  return _user_agent_cache


def _reset_user_agent_for_tests() -> None:
  """Seed the cache with the fallback so no test probes the real binary.

  A test exercising the probe path clears ``_user_agent_cache`` first and
  monkeypatches ``subprocess.run``; the module-level cache would otherwise
  leak a resolved value between tests.
  """
  global _user_agent_cache
  _user_agent_cache = USER_AGENT_FALLBACK


async def _oauth_headers(access_token: str) -> dict[str, str]:
  """Headers every OAuth-authenticated call to this API carries."""
  return {
      "Authorization": f"Bearer {access_token}",
      "anthropic-beta": ANTHROPIC_BETA,
      "User-Agent": await _user_agent(),
  }


def _expires_at_ms(token_data: dict[str, Any]) -> int | None:
  """Expiry of a renewed token as the millisecond stamp the CLI's file format uses.

  The grant may report an absolute ``expires_at`` or a relative ``expires_in``, and an
  absolute value may itself arrive in seconds. Nothing here reads the field back; it is
  written only so the external CLI keeps its own renewal schedule.
  """
  absolute = token_data.get("expires_at")
  if isinstance(absolute, (int, float)) and not isinstance(absolute, bool):
    return int(absolute if absolute > 1e11 else absolute * 1000)
  relative = token_data.get("expires_in")
  if isinstance(relative, (int, float)) and not isinstance(relative, bool):
    return int((time.time() + relative) * 1000)
  return None


def _write_credentials_atomically(path: pathlib.Path, value: dict[str, Any]) -> None:
  """Replace a credentials file in one step, never exposing a half-written token."""
  json_utils.write_json_atomically(path, value, indent=2, private=True)


async def _refresh_access_token(credentials_path: pathlib.Path, refresh_token: str) -> str | None:
  """Renew the OAuth access token and save new credentials to the account's file.

  A failure returns None after logging the token endpoint's status and a
  truncated response-body prefix. Only the error response body is logged —
  never a request body, an access token, or a refresh token.
  """
  client = http.get_http_client()
  try:
    resp = await client.post(
        TOKEN_REFRESH_URL,
        json={
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
        },
        headers={"User-Agent": await _user_agent()},
        timeout=timeouts.HTTP_OAUTH_TIMEOUT,
    )
  except Exception:
    log.exception("ext_usage_token_refresh_failed", path=str(credentials_path))
    return None
  if resp.status_code >= 400:
    log.warning(
        "ext_usage_token_refresh_failed",
        path=str(credentials_path),
        status_code=resp.status_code,
        body=resp.text[:200],
    )
    return None
  token_data = resp.json()

  new_access = token_data["access_token"]
  new_refresh = token_data.get("refresh_token", refresh_token)
  new_expires = _expires_at_ms(token_data)
  if new_expires is None:
    log.warning("ext_usage_renewal_without_expiry", path=str(credentials_path))

  def _update_creds() -> None:
    creds_data = json.loads(credentials_path.read_text(encoding="utf-8"))
    creds_data["claudeAiOauth"]["accessToken"] = new_access
    creds_data["claudeAiOauth"]["refreshToken"] = new_refresh
    if new_expires is not None:
      creds_data["claudeAiOauth"]["expiresAt"] = new_expires
    _write_credentials_atomically(credentials_path, creds_data)

  await asyncio.to_thread(_update_creds)

  log.info("ext_usage_token_refreshed", path=str(credentials_path))
  return new_access


# ---------------------------------------------------------------------------
# Response transform
# ---------------------------------------------------------------------------


def _scoped_windows(raw: dict[str, Any], *, account: str) -> list[dict[str, Any]]:
  """Turn every usable per-model ``limits`` entry into a window.

  Entries carry ``scope``; a null/absent scope is the plan-wide limit already
  covered by the top-level fields, so it is skipped silently. A scoped entry
  names one model via ``scope.model.display_name`` and is emitted with a
  ``scope_label`` so it can sit beside the plan-wide window of the same
  length. Entries that cannot be identified are skipped with a warning rather
  than guessed at.
  """
  windows: list[dict[str, Any]] = []
  limits = raw.get("limits")
  if not isinstance(limits, list):
    if limits is not None:
      usage_sources.warn_unknown_limit_shape(
          provider="claude", account=account, slot="limits", reason="limits is not a list")
    return windows
  for index, entry in enumerate(limits):
    slot = entry.get("kind") if isinstance(entry, dict) and isinstance(entry.get("kind"), str) else index
    if not isinstance(entry, dict):
      usage_sources.warn_unknown_limit_shape(
          provider="claude", account=account, slot=slot, reason="entry is not an object")
      continue
    scope = entry.get("scope")
    if scope is None:
      continue
    group = entry.get("group")
    window_minutes = LIMIT_GROUP_WINDOW_MINUTES.get(group)
    if window_minutes is None:
      usage_sources.warn_unknown_limit_shape(
          provider="claude", account=account, slot=slot, reason="unknown limit group")
      continue
    model = scope.get("model") if isinstance(scope, dict) else None
    display_name = model.get("display_name") if isinstance(model, dict) else None
    if not isinstance(display_name, str) or not display_name:
      usage_sources.warn_unknown_limit_shape(
          provider="claude", account=account, slot=slot, reason="missing model display_name")
      continue
    utilization = usage_sources.as_utilization(entry.get("percent"))
    if utilization is None:
      usage_sources.warn_unknown_limit_shape(provider="claude", account=account, slot=slot, reason="missing percent")
    windows.append(
        {
            usage_sources.PANEL_WINDOW_MINUTES: window_minutes,
            usage_sources.PANEL_SCOPE_LABEL: display_name,
            usage_sources.PANEL_UTILIZATION: utilization,
            usage_sources.PANEL_RESETS_AT: entry.get("resets_at") or "",
        })
  return windows


def _transform_response(raw: dict[str, Any], *, account: str) -> dict[str, Any]:
  """Transform the raw Anthropic usage API response into our cached format.

  Claude reports its two windows under fixed field names, so their lengths are
  known here; everything downstream still reads them off ``window_minutes``.
  """
  now = models.utc_now_iso()

  windows: list[dict[str, Any]] = []
  for camel, snake, window_minutes in CLAUDE_WINDOW_FIELDS:
    bucket = raw.get(camel, raw.get(snake)) or {}
    windows.append(
        {
            usage_sources.PANEL_WINDOW_MINUTES: window_minutes,
            usage_sources.PANEL_UTILIZATION: usage_sources.as_utilization(bucket.get("utilization")),
            usage_sources.PANEL_RESETS_AT: bucket.get("resetsAt", bucket.get("resets_at", "")),
        })
  windows.extend(_scoped_windows(raw, account=account))
  windows.sort(key=lambda w: (w[usage_sources.PANEL_WINDOW_MINUTES], w.get(usage_sources.PANEL_SCOPE_LABEL, "")))

  known = {name for camel, snake, _ in CLAUDE_WINDOW_FIELDS for name in (camel, snake)}
  for key, value in raw.items():
    if key not in known and isinstance(value, dict) and "utilization" in value:
      usage_sources.warn_unknown_limit_shape(
          provider="claude", account=account, slot=key, reason="unrecognized window field")

  return {
      usage_sources.PANEL_WINDOWS: windows,
      usage_sources.PANEL_FETCHED_AT: now,
      usage_sources.PANEL_PROVIDER: "claude",
  }
