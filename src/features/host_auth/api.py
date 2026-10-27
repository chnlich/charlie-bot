"""The host-auth panel: the standing probe poller, its page, status, and probe routes, and the browser-login page."""

import asyncio
import datetime

import fastapi
from fastapi import responses

from src.features.host_auth import browser_login, host_auth
from src.features.host_auth import config as host_auth_config
from src.infra import config, constants, log_once, models, tasks
from src.runtime import templating
from src.runtime.api import deps
from src.runtime.hooks import wiring

log = log_once.LazyStructlogLogger()
router = fastapi.APIRouter()

# One round at a time holds the state file: the flag is set and read
# synchronously on the event loop, so a second probe request that arrives while
# a round runs reuses it instead of starting a second one.
_round_running = False

# The page shows at most this many characters of a readback run at page load.
_READBACK_SHOWN_CHARS = 300

_STATUS_LABELS = {
    host_auth.STATUS_OK: "Direct",
    host_auth.STATUS_NEEDS_OKTA: "Needs Okta",
    host_auth.STATUS_NEEDS_INTERACTIVE_AUTH: "Needs interactive auth",
    host_auth.STATUS_UNREACHABLE: "Unreachable",
}
_STATUS_CLASSES = {
    host_auth.STATUS_OK: "ok",
    host_auth.STATUS_NEEDS_OKTA: "okta",
    host_auth.STATUS_NEEDS_INTERACTIVE_AUTH: "iact",
    host_auth.STATUS_UNREACHABLE: "down",
}
# A countdown under a day is highlighted, so the renewal lands at a moment the
# operator chooses rather than in a pod restart.
_AMBER_UNDER_SEC = 24 * 3600.0


def _local_timestamp(moment_iso: str | None, fmt: str) -> str | None:
  """Render a stored ISO timestamp in the server's own zone, or None when absent."""
  if moment_iso is None:
    return None
  return datetime.datetime.fromisoformat(moment_iso).astimezone().strftime(fmt)


def _format_remaining(seconds: float) -> str:
  """The countdown in its two most significant units: ``3 d 21 h``, ``12 h 36 m``, ``9 m``."""
  days, rest = divmod(int(seconds), 86400)
  hours, rest = divmod(rest, 3600)
  minutes = rest // 60
  if days:
    return f"{days} d {hours} h"
  if hours:
    return f"{hours} h {minutes} m"
  return f"{minutes} m"


def _page_rows(state: dict, now: datetime.datetime) -> list[dict]:
  """One template row per state entry, with the deadline column's mode resolved.

  The deadline column counts only for a host that answers and carries a
  baseline. A host whose deadline has passed stops counting and shows the
  expired badge -- the baseline stays where it was rather than being refreshed
  from a later probe.
  """
  rows = []
  for entry in state["hosts"]:
    status = entry["status"]
    detail = entry.get("detail") or ""
    baseline = entry.get("enrolled_observed_at")
    row = {
        "alias": entry["alias"],
        "hostname": entry["hostname"],
        "status": status,
        "status_label": _STATUS_LABELS[status],
        "status_class": _STATUS_CLASSES[status],
        "detail_lines": detail.split("\n"),
        "renew_cmd": f"ssh {entry['alias']} true",
        "has_url": "http" in detail,
        "baseline": _local_timestamp(baseline, constants.LOCAL_TIMESTAMP_FORMAT),
        "last_probe": _local_timestamp(entry.get("last_probe_at"), "%H:%M"),
        "deadline_kind": "not_applicable",
        "remaining": None,
        "expires": None,
        "under_24h": False,
    }
    if status == host_auth.STATUS_OK:
      estimate = host_auth.derive_estimate(entry, now)
      row["expires"] = _local_timestamp(estimate["estimated_expires_at"], constants.LOCAL_TIMESTAMP_FORMAT)
      if baseline is None:
        row["deadline_kind"] = "no_baseline"
      elif estimate["remaining_sec"] <= 0:
        row["deadline_kind"] = "expired"
      else:
        row["deadline_kind"] = "counting"
        row["remaining"] = _format_remaining(estimate["remaining_sec"])
        row["under_24h"] = estimate["remaining_sec"] < _AMBER_UNDER_SEC
    elif status == host_auth.STATUS_NEEDS_OKTA:
      row["deadline_kind"] = "pending_okta"
    rows.append(row)
  return rows


def _summary(state: dict, now: datetime.datetime) -> dict:
  """The page-top summary: the hosts held right now, then the soonest estimated expiry."""
  blocked = [entry["alias"] for entry in state["hosts"] if entry["status"] == host_auth.STATUS_NEEDS_OKTA]
  soonest: tuple[float, str] | None = None
  for entry in state["hosts"]:
    if entry["status"] != host_auth.STATUS_OK:
      continue
    remaining = host_auth.derive_estimate(entry, now)["remaining_sec"]
    if remaining is None or remaining <= 0:
      continue
    if soonest is None or remaining < soonest[0]:
      soonest = (remaining, entry["alias"])
  return {
      "blocked": blocked,
      "soonest_alias": soonest[1] if soonest else None,
      "soonest_remaining": _format_remaining(soonest[0]) if soonest else None,
  }


@router.get("/host-auth", response_class=responses.HTMLResponse)
async def host_auth_page(request: fastapi.Request) -> responses.HTMLResponse:
  """Render the standing status page from the state file, server-side and page-JavaScript-free."""
  state = host_auth.load_state()
  now = models.utc_now()
  return templating.templates().TemplateResponse(
      request,
      "host_auth.html",
      context={
          "probe_running": bool(state.get("probe_running")),
          "hosts": _page_rows(state, now),
          "summary": _summary(state, now),
          "last_probe": _local_timestamp(state.get("probed_at"), constants.LOCAL_TIMESTAMP_FORMAT),
          "interval_label": f"{host_auth.PROBE_INTERVAL_SEC // 60} min",
          "backoff_label": f"{host_auth.BLOCKED_BACKOFF_SEC // 3600} h",
          "ttl_label": f"{host_auth.TRUST_CACHE_TTL_SEC // 86400} days",
      },
  )


@router.get("/api/host-auth/status")
async def host_auth_status() -> responses.JSONResponse:
  """The state file content plus each host's derived ``estimated_expires_at`` / ``remaining_sec``."""
  state = host_auth.load_state()
  now = models.utc_now()
  hosts = [{**entry, **host_auth.derive_estimate(entry, now)} for entry in state["hosts"]]
  return responses.JSONResponse({**state, "hosts": hosts})


@router.post("/api/host-auth/probe")
async def host_auth_probe() -> responses.RedirectResponse:
  """Start a backoff-free probe round and return to the page.

  A round already in flight is reused: the redirect lands on a page that
  meta-refreshes until the round publishes, and the backoff never applies to
  the manual path.
  """
  global _round_running
  if _round_running:
    return responses.RedirectResponse("/host-auth", status_code=303)
  _round_running = True
  tasks.create_logged_task(_manual_round(), name="host-auth-manual-round")
  return responses.RedirectResponse("/host-auth", status_code=303)


def _login_named(cfg: config.CharlieBotConfig, name: str) -> host_auth_config.BrowserLoginConfig:
  for login in cfg.host_auth.browser_logins:
    if login.name == name:
      return login
  raise fastapi.HTTPException(status_code=404, detail=f"no browser login named {name!r}")


def _live_view(attempt: browser_login.Attempt) -> dict:
  return {
      "user_code": attempt.user_code,
      "approval_url": attempt.approval_url,
      "started_at": _local_timestamp(attempt.started_at.isoformat(), constants.LOCAL_TIMESTAMP_FORMAT),
      "phase": attempt.phase.label,
  }


def _last_view(attempt: browser_login.Attempt) -> dict:
  """The last ended attempt; only the two outcomes that ran a readback carry its output."""
  shows_readback = attempt.outcome in (browser_login.Outcome.APPROVED, browser_login.Outcome.READBACK_FAILED)
  return {
      "outcome": attempt.outcome.label,
      "finished_at": _local_timestamp(attempt.finished_at.isoformat(), constants.LOCAL_TIMESTAMP_FORMAT),
      "last_line": next((line for line in reversed(attempt.output) if line.strip()), ""),
      "readback_exit": attempt.readback_exit if shows_readback else None,
      "readback_output": attempt.readback_output.strip() if shows_readback else None,
  }


def _readback_view(result: tuple[int, str]) -> dict:
  exit_code, output = result
  return {"exit": exit_code, "output": output.strip()[:_READBACK_SHOWN_CHARS]}


def _login_rows(logins: list[host_auth_config.BrowserLoginConfig], readbacks: dict[str, tuple[int, str]]) -> list[dict]:
  """One template row per login: its attempt in progress, else its last ended attempt and this load's readback."""
  rows = []
  for login in logins:
    live = browser_login.live_attempt(login.name)
    last = browser_login.last_attempt(login.name)
    readback = readbacks.get(login.name) if live is None else None
    rows.append(
        {
            "name": login.name,
            "label": login.label,
            "live": _live_view(live) if live is not None else None,
            "last": _last_view(last) if last is not None else None,
            "readback": _readback_view(readback) if readback is not None else None,
        })
  return rows


@router.get("/host-auth/logins", response_class=responses.HTMLResponse)
async def browser_logins_page(
    request: fastapi.Request,
    cfg: config.CharlieBotConfig = fastapi.Depends(deps.get_config_on_loop),
) -> responses.HTMLResponse:
  """The browser-login page. Only idle logins run their readback here; no page view starts a login."""
  logins = cfg.host_auth.browser_logins
  idle = [login for login in logins if browser_login.live_attempt(login.name) is None]
  results = await asyncio.gather(*(browser_login.page_readback(login) for login in idle))
  readbacks = {login.name: result for login, result in zip(idle, results, strict=True)}
  rows = _login_rows(logins, readbacks)
  return templating.templates().TemplateResponse(
      request,
      "browser_login.html",
      context={
          "logins": rows,
          "any_live": any(row["live"] is not None for row in rows)
      },
  )


@router.post("/api/host-auth/logins/{name}/start")
async def browser_login_start(
    name: str,
    cfg: config.CharlieBotConfig = fastapi.Depends(deps.get_config_on_loop),
) -> responses.Response:
  """Start an attempt of the named login and redirect to its approval link; 502 when the login gives none."""
  login = _login_named(cfg, name)
  try:
    link = await browser_login.start_login(login)
  except browser_login.StartFailedError as exc:
    return responses.PlainTextResponse(str(exc), status_code=502)
  return responses.RedirectResponse(link, status_code=303)


async def _manual_round() -> None:
  """The manual probe round: force, one at a time, failures logged and absorbed."""
  global _round_running
  try:
    await host_auth.run_round(force=True)
  except Exception:
    log.exception("host_auth_manual_round_failed")
  finally:
    _round_running = False


async def _poller_round() -> None:
  """One scheduled round, skipped while a manual round holds the state file."""
  global _round_running
  if _round_running:
    return
  _round_running = True
  try:
    await host_auth.run_round(force=False)
  finally:
    _round_running = False


async def _poll_loop() -> None:
  """Run one round at startup, then wake once per standing period; cancelled at shutdown."""
  while True:
    try:
      await _poller_round()
    except Exception:
      log.exception("host_auth_poll_error")
    await asyncio.sleep(host_auth.PROBE_INTERVAL_SEC)


_poller = tasks.SingleTaskPoller(_poll_loop, log, "host_auth_poller_started", "host_auth_poller_stopped")
start_poller = _poller.start
stop_poller = _poller.stop


async def start_service(ctx: wiring.ServiceContext) -> None:
  """Start the standing probe poller."""
  await start_poller()


async def stop_service() -> None:
  """Stop the poller and end every live browser login; stopping a poller that never started does nothing."""
  await stop_poller()
  await browser_login.stop_all()
