"""External tool usage poller and API route (Claude Code, Codex)."""

import asyncio
import json
import os
import time
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

import structlog
from fastapi import APIRouter

from src.core.codex_pricing import calculate_codex_usage_cost_usd
from src.core.config import get_config
from src.core.http import get_http_client
from src.core.streaming import streaming_manager
from src.core.timeouts import EXT_USAGE_ROUND_GAP_SECONDS, HTTP_OAUTH_TIMEOUT

log = structlog.get_logger()

router = APIRouter()

# ---------------------------------------------------------------------------
# Cached usage data (module-level, keyed by "<provider>:<account>")
# ---------------------------------------------------------------------------

_cached_usage: dict[str, dict] = {}
_poller_task: asyncio.Task | None = None
# Per-instance provider state, keyed by (provider, expanded dir path), kept
# across cycles so per-instance 429 backoff survives between polls.
_instances: dict[tuple[str, str], "_UsageInstance"] = {}

ROUND_GAP_SECONDS = EXT_USAGE_ROUND_GAP_SECONDS
USAGE_URL = "https://api.anthropic.com/api/oauth/usage"
TOKEN_REFRESH_URL = "https://platform.claude.com/v1/oauth/token"
CLIENT_ID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
ANTHROPIC_BETA = "oauth-2025-04-20"
# The usage endpoint rate-limits per access token, and reportedly far more
# generously for the Claude Code user agent than for an unrecognized one. Kept as a
# protocol constant beside ANTHROPIC_BETA rather than probed from the installed CLI,
# which would couple this module to the CLI's install layout.
USER_AGENT = "claude-code/2.1.219"

CLAUDE_DEFAULT_DIR = str(Path.home() / ".claude")
CODEX_DEFAULT_DIR = str(Path.home() / ".codex")

# ---------------------------------------------------------------------------
# Account-set derivation (no registry): run at the start of every poll cycle.
# Reads get_config(), which is mtime-cached, so config.yaml edits take effect
# on the next cycle without a server restart.
# ---------------------------------------------------------------------------


def _account_label(provider: str, expanded_path: str) -> str:
  """Derive an account label from a directory's expanded absolute path.

  basename -> strip a leading '.' -> strip the provider prefix ('claude'/'codex')
  and a following '-'. The provider default dir is labelled 'main' by the
  caller, not by this function.
  """
  name = os.path.basename(expanded_path)
  if name.startswith("."):
    name = name[1:]
  if name.startswith(provider):
    name = name[len(provider):]
    if name.startswith("-"):
      name = name[1:]
  return name


def _derive_provider_accounts(
    provider: str,
    default_dir: str,
    options: list,
    get_dir: Any,
) -> list[tuple[str, str]]:
  """Return ordered [(label, expanded_abs_path)] for one provider.

  Always includes the provider default dir first (label 'main'). Dedupes by
  expanded absolute path, so an entry explicitly pointing at the default
  collapses into the default. Later label collisions fall back to the full
  basename, then fail loud (skip) — never silently overwriting an existing key.
  """
  default_expanded = os.path.abspath(os.path.expanduser(default_dir))
  seen: set[str] = {default_expanded}
  labels: set[str] = {"main"}
  accounts: list[tuple[str, str]] = [("main", default_expanded)]

  for opt in options:
    raw = get_dir(opt)
    if not raw:
      continue
    expanded = os.path.abspath(os.path.expanduser(raw))
    if expanded in seen:
      continue
    label = _account_label(provider, expanded)
    if label in labels:
      label = os.path.basename(expanded)
      if label in labels:
        log.error("ext_usage_account_label_collision_skip", provider=provider, dir=expanded)
        continue
    seen.add(expanded)
    labels.add(label)
    accounts.append((label, expanded))

  return accounts


def _derive_accounts() -> dict[str, list[tuple[str, str]]]:
  """Derive the full account set for both providers from the live config."""
  cfg = get_config()
  claude_opts = [o for o in cfg.backend_options if o.type == "cc-claude"]
  codex_opts = [o for o in cfg.backend_options if o.type == "codex"]
  return {
      "claude": _derive_provider_accounts("claude", CLAUDE_DEFAULT_DIR, claude_opts, lambda o: o.claude_config_dir),
      "codex": _derive_provider_accounts("codex", CODEX_DEFAULT_DIR, codex_opts, lambda o: o.codex_home),
  }


# ---------------------------------------------------------------------------
# Per-account usage providers
# ---------------------------------------------------------------------------


class ClaudeUsageProvider:
  """Fetches usage data from the Anthropic OAuth usage endpoint for one account."""

  def __init__(self, label: str, credentials_path: Path) -> None:
    self.label = label
    self.credentials_path = Path(credentials_path)
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
    return _transform_response(resp.json(), account=self.label)

  async def _get_usage(self, access_token: str) -> Any:
    client = get_http_client()
    return await client.get(USAGE_URL, headers=_oauth_headers(access_token), timeout=HTTP_OAUTH_TIMEOUT)

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


class CodexUsageProvider:
  """Reads usage from <home_dir>/sessions/ JSONL files for one account."""

  def __init__(self, label: str, home_dir: str) -> None:
    self.label = label
    self.sessions_dir = Path(home_dir) / "sessions"
    self.last_error = "no sessions found"

  async def fetch(self) -> dict[str, Any] | None:
    rollout_paths = await asyncio.to_thread(_list_rollout_files, self.sessions_dir)
    usage, spend = await asyncio.gather(
        asyncio.to_thread(self._fetch_usage, rollout_paths),
        asyncio.to_thread(_compute_codex_spend_windows, rollout_paths=rollout_paths),
        return_exceptions=True,
    )
    if isinstance(usage, Exception):
      log.error("ext_usage_codex_usage_error", account=self.label, error=str(usage))
      self.last_error = "usage read failed"
      return None
    if usage is None:
      self.last_error = "no sessions found"
      return None
    if isinstance(spend, Exception):
      log.error("ext_usage_codex_spend_failed", account=self.label, error=str(spend))
      spend = None
    usage["spend"] = spend
    return usage

  def _fetch_usage(self, rollout_paths: list[Path]) -> dict[str, Any] | None:
    jsonl_path = _newest_rollout(rollout_paths)
    if jsonl_path is None:
      return None
    text = jsonl_path.read_text()
    return _extract_latest_codex_usage(text.splitlines(), account=self.label)


def _list_rollout_files(sessions_dir: Path) -> list[Path]:
  """List every rollout log under one account's sessions dir.

  A single walk feeds both readers: the usage scrape wants the newest file
  whatever its age, while the spend aggregation applies its own mtime cutoff.
  """
  if not sessions_dir.exists():
    return []
  return list(sessions_dir.glob("**/rollout-*.jsonl"))


def _newest_rollout(rollout_paths: list[Path]) -> Path | None:
  """Newest rollout by mtime, with no date bound.

  A reading's age is shown rather than used to hide it: under a weekly window
  the last sample is the only information there is, however old.
  """
  newest: Path | None = None
  newest_mtime = float("-inf")
  for path in rollout_paths:
    try:
      mtime = path.stat().st_mtime
    except OSError as e:
      log.warning("ext_usage_codex_rollout_stat_failed", path=str(path), error=str(e))
      continue
    if mtime > newest_mtime:
      newest, newest_mtime = path, mtime
  return newest


class _UsageInstance:
  """Bundles a derived account's identity with its persistent provider instance."""

  def __init__(self, provider: str, label: str, dir_path: str) -> None:
    self.provider = provider
    self.label = label
    self.provider_instance = _create_provider(provider, label, dir_path)

  async def fetch(self) -> dict[str, Any] | None:
    return await self.provider_instance.fetch()

  @property
  def last_error(self) -> str:
    return self.provider_instance.last_error


def _create_provider(provider: str, label: str, dir_path: str) -> ClaudeUsageProvider | CodexUsageProvider:
  if provider == "claude":
    return ClaudeUsageProvider(label, Path(dir_path) / ".credentials.json")
  if provider == "codex":
    return CodexUsageProvider(label, dir_path)
  raise ValueError(f"unknown usage provider: {provider!r}")


# ---------------------------------------------------------------------------
# Credentials helpers
# ---------------------------------------------------------------------------


def _read_credentials(credentials_path: Path) -> dict[str, Any] | None:
  """Read OAuth credentials from a Claude account's .credentials.json."""
  if not credentials_path.exists():
    log.warning("ext_usage_credentials_not_found", path=str(credentials_path))
    return None

  data = json.loads(credentials_path.read_text())
  oauth = data.get("claudeAiOauth", {})
  access_token = oauth.get("accessToken")
  refresh_token = oauth.get("refreshToken")

  if not access_token:
    log.warning("ext_usage_no_access_token", path=str(credentials_path))
    return None

  # expiresAt is deliberately not read. It is a millisecond stamp, and every renewal
  # decision it used to gate is now made by the server's 401 instead.
  return {
      "access_token": access_token,
      "refresh_token": refresh_token,
  }


def _extract_latest_codex_usage(
    lines: list[str],
    *,
    fetched_at: str | None = None,
    account: str = "",
) -> dict[str, Any] | None:
  """Parse the latest Codex token_count event from a session JSONL file."""
  effective_fetched_at = fetched_at or datetime.now(timezone.utc).isoformat()

  # Scan lines in reverse for the latest token_count event
  for line in reversed(lines):
    line = line.strip()
    if not line:
      continue
    try:
      event = json.loads(line)
    except json.JSONDecodeError:
      continue
    if event.get("type") != "event_msg":
      continue
    payload = event.get("payload", {})
    if payload.get("type") != "token_count":
      continue
    return _transform_codex_response(event, fetched_at=effective_fetched_at, account=account)

  return None


def _parse_codex_timestamp(timestamp: Any) -> datetime:
  if not isinstance(timestamp, str):
    raise ValueError(f"expected string timestamp, got {type(timestamp).__name__}")
  parsed = datetime.fromisoformat(timestamp.replace("Z", "+00:00"))
  if parsed.tzinfo is None:
    raise ValueError(f"Codex timestamp is missing timezone: {timestamp}")
  return parsed.astimezone(timezone.utc)


def _new_token_usage_bucket() -> dict[str, int]:
  return {
      "input_tokens": 0,
      "cached_input_tokens": 0,
      "output_tokens": 0,
  }


def _add_token_usage(accumulator: dict[str, dict[str, int]], model: str, usage: dict[str, Any]) -> None:
  bucket = accumulator.setdefault(model, _new_token_usage_bucket())
  bucket["input_tokens"] += usage["input_tokens"]
  bucket["cached_input_tokens"] += usage["cached_input_tokens"]
  bucket["output_tokens"] += usage["output_tokens"]


def _sum_codex_spend(accumulator: dict[str, dict[str, int]]) -> float:
  total = 0.0
  for model, usage in accumulator.items():
    cost = calculate_codex_usage_cost_usd(model, usage)
    if cost is not None:
      total += cost
  return total


def _log_codex_spend_row_skip(path: Path, line_number: int, error: Exception | str) -> None:
  log.warning(
      "ext_usage_codex_spend_row_skipped",
      path=str(path),
      line_number=line_number,
      error=str(error),
  )


def _compute_codex_spend_windows(
    *,
    rollout_paths: list[Path] | None = None,
    sessions_dir: Path | None = None,
    now: datetime | None = None,
) -> dict[str, float]:
  """Compute rolling Codex spend by summing per-turn token usage from rollout logs."""
  effective_now = now or datetime.now(timezone.utc)
  if effective_now.tzinfo is None:
    raise ValueError("now must be timezone-aware")
  effective_now = effective_now.astimezone(timezone.utc)
  one_day_ago = effective_now - timedelta(days=1)
  seven_days_ago = effective_now - timedelta(days=7)
  min_mtime = seven_days_ago.timestamp()
  if rollout_paths is None:
    rollout_paths = _list_rollout_files(sessions_dir or (Path.home() / ".codex" / "sessions"))

  last_24h_by_model: dict[str, dict[str, int]] = {}
  last_7d_by_model: dict[str, dict[str, int]] = {}

  for path in rollout_paths:
    try:
      if path.stat().st_mtime < min_mtime:
        continue
    except OSError as e:
      log.warning("ext_usage_codex_spend_file_skip", path=str(path), error=str(e))
      continue

    current_model = ""
    try:
      for line_number, line in enumerate(path.read_text().splitlines(), start=1):
        line = line.strip()
        if not line:
          continue
        try:
          event = json.loads(line)
          if not isinstance(event, dict):
            raise ValueError(f"expected JSON object, got {type(event).__name__}")
          event_type = event.get("type")
          payload = event.get("payload") or {}
          if not isinstance(payload, dict):
            raise ValueError(f"payload must be an object, got {type(payload).__name__}")
          if event_type == "turn_context":
            model = payload.get("model")
            if isinstance(model, str):
              current_model = model
            continue
          if event_type != "event_msg" or payload.get("type") != "token_count":
            continue

          info = payload.get("info") or {}
          if not isinstance(info, dict):
            raise ValueError(f"info must be an object, got {type(info).__name__}")
          last_usage = info.get("last_token_usage")
          if not last_usage:
            continue
          if not isinstance(last_usage, dict):
            raise ValueError(f"last_token_usage must be an object, got {type(last_usage).__name__}")
          token_usage = {
              "input_tokens": last_usage["input_tokens"],
              "cached_input_tokens": last_usage["cached_input_tokens"],
              "output_tokens": last_usage["output_tokens"],
          }
          for key, value in token_usage.items():
            if not isinstance(value, int):
              raise ValueError(f"{key} must be an int, got {type(value).__name__}")

          observed_at = _parse_codex_timestamp(event["timestamp"])
          if observed_at < seven_days_ago or observed_at > effective_now:
            continue
          _add_token_usage(last_7d_by_model, current_model, token_usage)
          if observed_at >= one_day_ago:
            _add_token_usage(last_24h_by_model, current_model, token_usage)
        except (json.JSONDecodeError, KeyError, TypeError, ValueError) as e:
          _log_codex_spend_row_skip(path, line_number, e)
    except OSError as e:
      log.warning("ext_usage_codex_spend_file_skip", path=str(path), error=str(e))

  return {
      "last_24h_usd": _sum_codex_spend(last_24h_by_model),
      "last_7d_usd": _sum_codex_spend(last_7d_by_model),
  }


CODEX_LIMIT_SLOTS = ("primary", "secondary")

CLAUDE_WINDOW_FIELDS = (
    ("fiveHour", "five_hour", 300),
    ("sevenDay", "seven_day", 10080),
)

# limits[].group -> window length in minutes. Both values the endpoint reports
# today, kept as a table rather than an if/else so an unknown group is skipped
# with a warning instead of guessed at from array position.
LIMIT_GROUP_WINDOW_MINUTES = {"session": 300, "weekly": 10080}


def _as_utilization(value: Any) -> float | None:
  """Percentage used, or None when upstream did not report one.

  Absent usage stays absent: rendering it as 0.0 would claim a full quota,
  which is the most dangerous wrong answer this strip can give.
  """
  if isinstance(value, bool) or not isinstance(value, (int, float)):
    return None
  return float(value)


def _codex_windows(rate_limits: dict[str, Any], *, account: str) -> list[dict[str, Any]]:
  """Turn every non-null rate-limit slot into a self-describing window entry.

  Slot position carries no meaning; a window is identified by the
  ``window_minutes`` it reports. A slot that omits it is dropped with a warning
  rather than guessed at, because inferring a window from slot order is exactly
  the failure this shape exists to remove.
  """
  windows: list[dict[str, Any]] = []
  for slot in CODEX_LIMIT_SLOTS:
    limit = rate_limits.get(slot)
    if not isinstance(limit, dict):
      continue
    window_minutes = limit.get("window_minutes")
    if isinstance(window_minutes, bool) or not isinstance(window_minutes, int):
      log.warning(
          "ext_usage_unknown_limit_shape",
          provider="codex",
          account=account,
          slot=slot,
          reason="missing window_minutes",
      )
      continue
    utilization = _as_utilization(limit.get("used_percent"))
    if utilization is None:
      log.warning(
          "ext_usage_unknown_limit_shape",
          provider="codex",
          account=account,
          slot=slot,
          reason="missing used_percent",
      )
    resets_at = limit.get("resets_at")
    windows.append({
        "window_minutes": window_minutes,
        "utilization": utilization,
        "resets_at":
            datetime.fromtimestamp(resets_at, tz=timezone.utc).isoformat()
            if isinstance(resets_at, (int, float)) and not isinstance(resets_at, bool) else "",
    })
  windows.sort(key=lambda w: w["window_minutes"])
  return windows


def _transform_codex_response(
    event: dict[str, Any],
    *,
    fetched_at: str,
    account: str = "",
) -> dict[str, Any]:
  """Transform a Codex token_count event into our cached usage format."""
  payload = event.get("payload", {})
  rate_limits = payload.get("rate_limits") or {}
  primary = rate_limits.get("primary")
  secondary = rate_limits.get("secondary")
  credits = rate_limits.get("credits")

  usage = {
      "windows": _codex_windows(rate_limits, account=account),
      "fetched_at": fetched_at,
      "provider": "codex",
      "token_count_observed_at": event.get("timestamp", ""),
  }
  if ("primary" in rate_limits and "secondary" in rate_limits and primary is None and secondary is None and
      ((isinstance(credits, dict) and credits.get("unlimited") is True) or rate_limits.get("plan_type") == "business")):
    usage["rate_limits_state"] = "business-unlimited"
  return usage


def _oauth_headers(access_token: str) -> dict[str, str]:
  """Headers every OAuth-authenticated call to this API carries."""
  return {
      "Authorization": f"Bearer {access_token}",
      "anthropic-beta": ANTHROPIC_BETA,
      "User-Agent": USER_AGENT,
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


def _write_credentials_atomically(path: Path, value: dict[str, Any]) -> None:
  """Replace a credentials file in one step, never exposing a half-written token."""
  temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
  temporary.write_text(json.dumps(value, indent=2))
  os.chmod(temporary, 0o600)
  os.replace(temporary, path)


async def _refresh_access_token(credentials_path: Path, refresh_token: str) -> str | None:
  """Renew the OAuth access token and save new credentials to the account's file.

  A failure returns None after logging the token endpoint's status and a
  truncated response-body prefix. Only the error response body is logged —
  never a request body, an access token, or a refresh token.
  """
  client = get_http_client()
  try:
    resp = await client.post(
        TOKEN_REFRESH_URL,
        json={
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
        },
        headers={"User-Agent": USER_AGENT},
        timeout=HTTP_OAUTH_TIMEOUT,
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
    creds_data = json.loads(credentials_path.read_text())
    creds_data["claudeAiOauth"]["accessToken"] = new_access
    creds_data["claudeAiOauth"]["refreshToken"] = new_refresh
    if new_expires is not None:
      creds_data["claudeAiOauth"]["expiresAt"] = new_expires
    _write_credentials_atomically(credentials_path, creds_data)

  await asyncio.to_thread(_update_creds)

  log.info("ext_usage_token_refreshed", path=str(credentials_path))
  return new_access


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
      log.warning("ext_usage_unknown_limit_shape", provider="claude", account=account,
                  slot="limits", reason="limits is not a list")
    return windows
  for index, entry in enumerate(limits):
    slot = entry.get("kind") if isinstance(entry, dict) and isinstance(entry.get("kind"), str) else index
    if not isinstance(entry, dict):
      log.warning("ext_usage_unknown_limit_shape", provider="claude", account=account,
                  slot=slot, reason="entry is not an object")
      continue
    scope = entry.get("scope")
    if scope is None:
      continue
    group = entry.get("group")
    window_minutes = LIMIT_GROUP_WINDOW_MINUTES.get(group)
    if window_minutes is None:
      log.warning("ext_usage_unknown_limit_shape", provider="claude", account=account,
                  slot=slot, reason="unknown limit group")
      continue
    model = scope.get("model") if isinstance(scope, dict) else None
    display_name = model.get("display_name") if isinstance(model, dict) else None
    if not isinstance(display_name, str) or not display_name:
      log.warning("ext_usage_unknown_limit_shape", provider="claude", account=account,
                  slot=slot, reason="missing model display_name")
      continue
    utilization = _as_utilization(entry.get("percent"))
    if utilization is None:
      log.warning("ext_usage_unknown_limit_shape", provider="claude", account=account,
                  slot=slot, reason="missing percent")
    windows.append({
        "window_minutes": window_minutes,
        "scope_label": display_name,
        "utilization": utilization,
        "resets_at": entry.get("resets_at") or "",
    })
  return windows


def _transform_response(raw: dict[str, Any], *, account: str = "") -> dict[str, Any]:
  """Transform the raw Anthropic usage API response into our cached format.

  Claude reports its two windows under fixed field names, so their lengths are
  known here; everything downstream still reads them off ``window_minutes``.
  """
  now = datetime.now(timezone.utc).isoformat()

  windows: list[dict[str, Any]] = []
  for camel, snake, window_minutes in CLAUDE_WINDOW_FIELDS:
    bucket = raw.get(camel, raw.get(snake)) or {}
    windows.append({
        "window_minutes": window_minutes,
        "utilization": _as_utilization(bucket.get("utilization")),
        "resets_at": bucket.get("resetsAt", bucket.get("resets_at", "")),
    })
  windows.extend(_scoped_windows(raw, account=account))
  windows.sort(key=lambda w: (w["window_minutes"], w.get("scope_label", "")))

  known = {name for camel, snake, _ in CLAUDE_WINDOW_FIELDS for name in (camel, snake)}
  for key, value in raw.items():
    if key not in known and isinstance(value, dict) and "utilization" in value:
      log.warning("ext_usage_unknown_limit_shape", provider="claude", account=account, slot=key,
                  reason="unrecognized window field")

  return {
      "windows": windows,
      "fetched_at": now,
      "provider": "claude",
  }


# ---------------------------------------------------------------------------
# Background poller
# ---------------------------------------------------------------------------


async def _poll_loop() -> None:
  """Background loop that refreshes one derived account per round-gap sleep.

  Accounts are fetched round-robin in derivation order: each fetch updates that
  account's ``_cached_usage`` entry and broadcasts the full cache over the sidebar
  websocket, then sleeps ``EXT_USAGE_ROUND_GAP_SECONDS`` before the next account.
  The account set is re-derived once per full round (every N fetches) so config
  edits apply at round boundaries; dropped accounts are pruned from both
  ``_instances`` and ``_cached_usage``. A zero-account round still sleeps once
  before re-deriving so the loop never busy-spins. Accounts not yet read carry a
  pending marker so the strip never understates the account set.
  """
  while True:
    try:
      accounts = _derive_accounts()
      cycle: list[_UsageInstance] = []
      live_keys: set[tuple[str, str]] = set()
      for provider in ("claude", "codex"):
        for label, dir_path in accounts.get(provider, []):
          key = (provider, dir_path)
          live_keys.add(key)
          inst = _instances.get(key)
          if inst is None:
            inst = _UsageInstance(provider, label, dir_path)
            _instances[key] = inst
          inst.label = label
          cycle.append(inst)
      for key in list(_instances):
        if key not in live_keys:
          del _instances[key]

      live_cache_keys = {f"{inst.provider}:{inst.label}" for inst in cycle}
      for cache_key in list(_cached_usage):
        if cache_key not in live_cache_keys:
          del _cached_usage[cache_key]

      # Seed a pending marker for every account not yet read, so the strip lists
      # the whole account set from the first broadcast instead of growing one row
      # per round gap after a restart. Costs no extra request: the markers ride
      # along in the broadcast the first real fetch already sends. Seeding in
      # cycle order also fixes row order at derivation order rather than at
      # fetch-completion order.
      for inst in cycle:
        seed_key = f"{inst.provider}:{inst.label}"
        if seed_key not in _cached_usage:
          _cached_usage[seed_key] = {
              "provider": inst.provider,
              "account": inst.label,
              "pending": True,
          }

      if not cycle:
        await asyncio.sleep(ROUND_GAP_SECONDS)
        continue

      for inst in cycle:
        cache_key = f"{inst.provider}:{inst.label}"
        try:
          result = await inst.fetch()
        except Exception as e:
          log.error("ext_usage_poll_error", provider=inst.provider, account=inst.label, error=str(e))
          result = None
        if result is not None:
          _cached_usage[cache_key] = {**result, "account": inst.label}
        else:
          prev = _cached_usage.get(cache_key)
          # A pending marker is a not-yet-read placeholder, so it gives way to the
          # real error the same way a previous error does; only a real reading is
          # worth keeping when a later fetch fails.
          if prev is None or "error" in prev or "pending" in prev:
            _cached_usage[cache_key] = {
                "provider": inst.provider,
                "account": inst.label,
                "error": inst.last_error,
            }
        if _cached_usage:
          await streaming_manager.broadcast("sidebar", {"type": "ext_usage", "providers": dict(_cached_usage)})
          log.info("ext_usage_fetched", providers=list(_cached_usage.keys()))
        await asyncio.sleep(ROUND_GAP_SECONDS)
    except Exception:
      log.exception("ext_usage_poll_error")
      # All sleeps now live inside the try block, so an exception raised before
      # reaching one (e.g. from _derive_accounts()) would otherwise skip every
      # await point and busy-spin the event loop instead of backing off.
      await asyncio.sleep(ROUND_GAP_SECONDS)


# ---------------------------------------------------------------------------
# API route
# ---------------------------------------------------------------------------


@router.get("/ext-usage")
async def get_ext_usage() -> dict[str, Any]:
  """Return cached external tool usage data."""
  if not _cached_usage:
    return {"error": "Usage data not yet available"}
  return {"providers": dict(_cached_usage)}


# ---------------------------------------------------------------------------
# Startup integration
# ---------------------------------------------------------------------------


async def start_poller() -> None:
  """Start the background usage poller. Call from the app lifespan."""
  global _poller_task
  _poller_task = asyncio.create_task(_poll_loop())
  log.info("ext_usage_poller_started")


async def stop_poller() -> None:
  """Cancel the background usage poller. Call from the app lifespan shutdown."""
  global _poller_task
  task = _poller_task
  if task is not None:
    _poller_task = None
    task.cancel()
    try:
      await task
    except asyncio.CancelledError:
      pass
    log.info("ext_usage_poller_stopped")
