"""The host-auth panel: the standing probe poller and its page, status, and probe routes."""

import asyncio
from datetime import datetime

from fastapi import APIRouter, Request
from fastapi.responses import HTMLResponse, JSONResponse, RedirectResponse

from src.api.pages import _templates
from src.core.host_auth import (
    BLOCKED_BACKOFF_SEC,
    PROBE_INTERVAL_SEC,
    STATUS_NEEDS_INTERACTIVE_AUTH,
    STATUS_NEEDS_OKTA,
    STATUS_OK,
    STATUS_UNREACHABLE,
    TRUST_CACHE_TTL_SEC,
    derive_estimate,
    load_state,
    run_round,
)
from src.core.log_once import LazyStructlogLogger
from src.core.models import utc_now
from src.core.tasks import SingleTaskPoller, create_logged_task

log = LazyStructlogLogger()
router = APIRouter()

# One round at a time holds the state file: the flag is set and read
# synchronously on the event loop, so a second probe request that arrives while
# a round runs reuses it instead of starting a second one.
_round_running = False

_STATUS_LABELS = {
    STATUS_OK: "Direct",
    STATUS_NEEDS_OKTA: "Needs Okta",
    STATUS_NEEDS_INTERACTIVE_AUTH: "Needs interactive auth",
    STATUS_UNREACHABLE: "Unreachable",
}
_STATUS_CLASSES = {
    STATUS_OK: "ok",
    STATUS_NEEDS_OKTA: "okta",
    STATUS_NEEDS_INTERACTIVE_AUTH: "iact",
    STATUS_UNREACHABLE: "down",
}
# A countdown under a day is highlighted, so the renewal lands at a moment the
# operator chooses rather than in a pod restart.
_AMBER_UNDER_SEC = 24 * 3600.0


def _local_timestamp(moment_iso: str | None, fmt: str) -> str | None:
  """Render a stored ISO timestamp in the server's own zone, or None when absent."""
  if moment_iso is None:
    return None
  return datetime.fromisoformat(moment_iso).astimezone().strftime(fmt)


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


def _page_rows(state: dict, now: datetime) -> list[dict]:
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
        "baseline": _local_timestamp(baseline, "%Y-%m-%d %H:%M %Z"),
        "last_probe": _local_timestamp(entry.get("last_probe_at"), "%H:%M"),
        "deadline_kind": "not_applicable",
        "remaining": None,
        "expires": None,
        "under_24h": False,
    }
    if status == STATUS_OK:
      estimate = derive_estimate(entry, now)
      row["expires"] = _local_timestamp(estimate["estimated_expires_at"], "%Y-%m-%d %H:%M %Z")
      if baseline is None:
        row["deadline_kind"] = "no_baseline"
      elif estimate["remaining_sec"] <= 0:
        row["deadline_kind"] = "expired"
      else:
        row["deadline_kind"] = "counting"
        row["remaining"] = _format_remaining(estimate["remaining_sec"])
        row["under_24h"] = estimate["remaining_sec"] < _AMBER_UNDER_SEC
    elif status == STATUS_NEEDS_OKTA:
      row["deadline_kind"] = "pending_okta"
    rows.append(row)
  return rows


def _summary(state: dict, now: datetime) -> dict:
  """The page-top summary: the hosts held right now, then the soonest estimated expiry."""
  blocked = [entry["alias"] for entry in state["hosts"] if entry["status"] == STATUS_NEEDS_OKTA]
  soonest: tuple[float, str] | None = None
  for entry in state["hosts"]:
    if entry["status"] != STATUS_OK:
      continue
    remaining = derive_estimate(entry, now)["remaining_sec"]
    if remaining is None or remaining <= 0:
      continue
    if soonest is None or remaining < soonest[0]:
      soonest = (remaining, entry["alias"])
  return {
      "blocked": blocked,
      "soonest_alias": soonest[1] if soonest else None,
      "soonest_remaining": _format_remaining(soonest[0]) if soonest else None,
  }


@router.get("/host-auth", response_class=HTMLResponse)
async def host_auth_page(request: Request) -> HTMLResponse:
  """Render the standing status page from the state file, server-side and page-JavaScript-free."""
  state = load_state()
  now = utc_now()
  return _templates().TemplateResponse(
      request,
      "host_auth.html",
      context={
          "probe_running": bool(state.get("probe_running")),
          "hosts": _page_rows(state, now),
          "summary": _summary(state, now),
          "last_probe": _local_timestamp(state.get("probed_at"), "%Y-%m-%d %H:%M %Z"),
          "interval_label": f"{PROBE_INTERVAL_SEC // 60} min",
          "backoff_label": f"{BLOCKED_BACKOFF_SEC // 3600} h",
          "ttl_label": f"{TRUST_CACHE_TTL_SEC // 86400} days",
      },
  )


@router.get("/api/host-auth/status")
async def host_auth_status() -> JSONResponse:
  """The state file content plus each host's derived ``estimated_expires_at`` / ``remaining_sec``."""
  state = load_state()
  now = utc_now()
  hosts = [{**entry, **derive_estimate(entry, now)} for entry in state["hosts"]]
  return JSONResponse({**state, "hosts": hosts})


@router.post("/api/host-auth/probe")
async def host_auth_probe() -> RedirectResponse:
  """Start a backoff-free probe round and return to the page.

  A round already in flight is reused: the redirect lands on a page that
  meta-refreshes until the round publishes, and the backoff never applies to
  the manual path.
  """
  global _round_running
  if _round_running:
    return RedirectResponse("/host-auth", status_code=303)
  _round_running = True
  create_logged_task(_manual_round(), name="host-auth-manual-round")
  return RedirectResponse("/host-auth", status_code=303)


async def _manual_round() -> None:
  """The manual probe round: force, one at a time, failures logged and absorbed."""
  global _round_running
  try:
    await run_round(force=True)
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
    await run_round(force=False)
  finally:
    _round_running = False


async def _poll_loop() -> None:
  """Run one round at startup, then wake once per standing period; cancelled at shutdown."""
  while True:
    try:
      await _poller_round()
    except Exception:
      log.exception("host_auth_poll_error")
    await asyncio.sleep(PROBE_INTERVAL_SEC)


_poller = SingleTaskPoller(_poll_loop, log, "host_auth_poller_started", "host_auth_poller_stopped")
start_poller = _poller.start
stop_poller = _poller.stop
