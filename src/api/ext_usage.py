"""External tool usage poller and API route (Claude Code, Codex)."""

import asyncio
import json
import os
import time
from datetime import datetime, date, timedelta, timezone
from pathlib import Path
from typing import Any

import structlog
from fastapi import APIRouter

from src.core.codex_pricing import calculate_codex_usage_cost_usd
from src.core.config import get_config
from src.core.http import get_http_client
from src.core.streaming import streaming_manager
from src.core.timeouts import EXT_USAGE_POLL_INTERVAL, HTTP_OAUTH_TIMEOUT

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

POLL_INTERVAL_SECONDS = EXT_USAGE_POLL_INTERVAL
USAGE_URL = "https://api.anthropic.com/api/oauth/usage"
TOKEN_REFRESH_URL = "https://console.anthropic.com/v1/oauth/token"
CLIENT_ID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
ANTHROPIC_BETA = "oauth-2025-04-20"

CLAUDE_DEFAULT_DIR = str(Path.home() / ".claude")
CODEX_DEFAULT_DIR = str(Path.home() / ".codex")

# ---------------------------------------------------------------------------
# Account-set derivation (no registry): run at the start of every poll cycle.
# Reads get_config(), which is mtime-cached, so config.yaml edits take effect
# on the next cycle without a server restart.
# ---------------------------------------------------------------------------


def _account_label(provider: str, expanded_path: str) -> str:
  """Derive an account label from a directory's expanded absolute path.

  basename -> strip a leading '.' -> strip the provider prefix ('claude-'/
  'codex-') when followed by '-'. The provider default dir is labelled 'main'
  by the caller, not by this function.
  """
  name = os.path.basename(expanded_path)
  if name.startswith("."):
    name = name[1:]
  prefix = provider + "-"
  if name.startswith(prefix):
    name = name[len(prefix):]
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
      self.last_error = "rate limited"
      return None

    creds = await asyncio.to_thread(_read_credentials, self.credentials_path)
    if creds is None:
      self.last_error = "credentials not found"
      return None

    access_token = creds["access_token"]
    expires_at = creds["expires_at"]

    if expires_at and time.time() >= expires_at:
      log.info("ext_usage_token_expired_refreshing", account=self.label)
      access_token = await _refresh_access_token(self.credentials_path, creds["refresh_token"])
      if access_token is None:
        self.last_error = "token refresh failed"
        return None

    client = get_http_client()
    resp = await client.get(
        USAGE_URL,
        headers={
            "Authorization": f"Bearer {access_token}",
            "anthropic-beta": ANTHROPIC_BETA,
        },
        timeout=HTTP_OAUTH_TIMEOUT,
    )

    if resp.status_code == 429:
      self._backoff_seconds = min((self._backoff_seconds or 30) * 2, 30 * 60)
      self._backoff_until = time.time() + self._backoff_seconds
      self.last_error = "rate limited"
      log.warning("ext_usage_rate_limited", account=self.label, backoff_seconds=self._backoff_seconds)
      return None

    self._backoff_seconds = 0.0
    resp.raise_for_status()
    return _transform_response(resp.json())


class CodexUsageProvider:
  """Reads usage from <home_dir>/sessions/ JSONL files for one account."""

  def __init__(self, label: str, home_dir: str) -> None:
    self.label = label
    self.sessions_dir = Path(home_dir) / "sessions"
    self.last_error = "no recent sessions"

  async def fetch(self) -> dict[str, Any] | None:
    usage, spend = await asyncio.gather(
        asyncio.to_thread(self._fetch_usage),
        asyncio.to_thread(_compute_codex_spend_windows, sessions_dir=self.sessions_dir),
        return_exceptions=True,
    )
    if isinstance(usage, Exception):
      log.error("ext_usage_codex_usage_error", account=self.label, error=str(usage))
      self.last_error = "usage read failed"
      return None
    if usage is None:
      self.last_error = "no recent sessions"
      return None
    if isinstance(spend, Exception):
      log.error("ext_usage_codex_spend_failed", account=self.label, error=str(spend))
      spend = None
    usage["spend"] = spend
    return usage

  def _fetch_usage(self) -> dict[str, Any] | None:
    jsonl_path = self._find_latest_session_file()
    if jsonl_path is None:
      return None
    text = jsonl_path.read_text()
    return _extract_latest_codex_usage(text.splitlines())

  def _find_latest_session_file(self) -> Path | None:
    """Walk sessions_dir/YYYY/MM/DD/ backwards from today, checking 3 days."""
    if not self.sessions_dir.exists():
      return None

    today = date.today()
    for days_ago in range(3):
      d = today - timedelta(days=days_ago)
      day_dir = self.sessions_dir / f"{d.year:04d}" / f"{d.month:02d}" / f"{d.day:02d}"
      if not day_dir.exists():
        continue

      candidates = sorted(day_dir.glob("rollout-*.jsonl"), key=lambda p: p.stat().st_mtime, reverse=True)
      if candidates:
        return candidates[0]

    return None


class _UsageInstance:
  """Bundles a derived account's identity with its persistent provider instance."""

  def __init__(self, provider: str, label: str, dir_path: str) -> None:
    self.provider = provider
    self.label = label
    self.dir_path = dir_path
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
  expires_at = oauth.get("expiresAt")

  if not access_token:
    log.warning("ext_usage_no_access_token", path=str(credentials_path))
    return None

  return {
      "access_token": access_token,
      "refresh_token": refresh_token,
      "expires_at": expires_at,
  }


def _extract_latest_codex_usage(
    lines: list[str],
    *,
    fetched_at: str | None = None,
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
    return _transform_codex_response(event, fetched_at=effective_fetched_at)

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
  root = sessions_dir or (Path.home() / ".codex" / "sessions")

  last_24h_by_model: dict[str, dict[str, int]] = {}
  last_7d_by_model: dict[str, dict[str, int]] = {}
  if not root.exists():
    return {"last_24h_usd": 0.0, "last_7d_usd": 0.0}

  for path in root.glob("**/rollout-*.jsonl"):
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


def _transform_codex_response(
    event: dict[str, Any],
    *,
    fetched_at: str,
) -> dict[str, Any]:
  """Transform a Codex token_count event into our cached usage format."""
  payload = event.get("payload", {})
  rate_limits = payload.get("rate_limits") or {}
  primary = rate_limits.get("primary")
  secondary = rate_limits.get("secondary")
  credits = rate_limits.get("credits")

  usage = {
      "five_hour":
          {
              "utilization": (primary or {}).get("used_percent", 0.0),
              "resets_at":
                  datetime.fromtimestamp(primary["resets_at"], tz=timezone.utc).isoformat()
                  if isinstance(primary, dict) and "resets_at" in primary else "",
          },
      "seven_day":
          {
              "utilization": (secondary or {}).get("used_percent", 0.0),
              "resets_at":
                  datetime.fromtimestamp(secondary["resets_at"], tz=timezone.utc).isoformat()
                  if isinstance(secondary, dict) and "resets_at" in secondary else "",
          },
      "fetched_at": fetched_at,
      "provider": "codex",
      "token_count_observed_at": event.get("timestamp", ""),
  }
  if ("primary" in rate_limits and "secondary" in rate_limits and primary is None and secondary is None and
      ((isinstance(credits, dict) and credits.get("unlimited") is True) or rate_limits.get("plan_type") == "business")):
    usage["rate_limits_state"] = "business-unlimited"
  return usage


async def _refresh_access_token(credentials_path: Path, refresh_token: str) -> str | None:
  """Refresh the OAuth access token and save new credentials to the account's file."""
  client = get_http_client()
  resp = await client.post(
      TOKEN_REFRESH_URL,
      json={
          "grant_type": "refresh_token",
          "refresh_token": refresh_token,
          "client_id": CLIENT_ID,
      },
      timeout=HTTP_OAUTH_TIMEOUT,
  )
  resp.raise_for_status()
  token_data = resp.json()

  new_access = token_data["access_token"]
  new_refresh = token_data.get("refresh_token", refresh_token)
  new_expires = token_data.get("expires_at")

  def _update_creds() -> None:
    creds_data = json.loads(credentials_path.read_text())
    creds_data["claudeAiOauth"]["accessToken"] = new_access
    creds_data["claudeAiOauth"]["refreshToken"] = new_refresh
    if new_expires:
      creds_data["claudeAiOauth"]["expiresAt"] = new_expires
    credentials_path.write_text(json.dumps(creds_data, indent=2))

  await asyncio.to_thread(_update_creds)

  log.info("ext_usage_token_refreshed", path=str(credentials_path))
  return new_access


def _transform_response(raw: dict[str, Any]) -> dict[str, Any]:
  """Transform the raw Anthropic usage API response into our cached format."""
  now = datetime.now(timezone.utc).isoformat()
  five_hour = raw.get("fiveHour", raw.get("five_hour", {}))
  seven_day = raw.get("sevenDay", raw.get("seven_day", {}))

  return {
      "five_hour":
          {
              "utilization": five_hour.get("utilization", 0.0),
              "resets_at": five_hour.get("resetsAt", five_hour.get("resets_at", "")),
          },
      "seven_day":
          {
              "utilization": seven_day.get("utilization", 0.0),
              "resets_at": seven_day.get("resetsAt", seven_day.get("resets_at", "")),
          },
      "fetched_at": now,
      "provider": "claude",
  }


# ---------------------------------------------------------------------------
# Background poller
# ---------------------------------------------------------------------------


async def _poll_loop() -> None:
  """Background loop that fetches usage data at a fixed cadence for every derived account."""
  global _cached_usage

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

      results = await asyncio.gather(
          *(inst.fetch() for inst in cycle),
          return_exceptions=True,
      )

      new_cache: dict[str, dict] = {}
      for inst, result in zip(cycle, results):
        cache_key = f"{inst.provider}:{inst.label}"
        if isinstance(result, Exception):
          log.error("ext_usage_poll_error", provider=inst.provider, account=inst.label, error=str(result))
          result = None
        if result is not None:
          new_cache[cache_key] = {**result, "account": inst.label}
        else:
          prev = _cached_usage.get(cache_key)
          if prev is not None and "error" not in prev:
            new_cache[cache_key] = prev
          else:
            new_cache[cache_key] = {
                "provider": inst.provider,
                "account": inst.label,
                "error": inst.last_error,
            }

      _cached_usage = new_cache

      if _cached_usage:
        await streaming_manager.broadcast("sidebar", {"type": "ext_usage", "providers": dict(_cached_usage)})
        log.info("ext_usage_fetched", providers=list(_cached_usage.keys()))
    except Exception:
      log.exception("ext_usage_poll_error")

    await asyncio.sleep(POLL_INTERVAL_SECONDS)


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
