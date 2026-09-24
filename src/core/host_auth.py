"""Host login authorization: probing, transition accounting, and estimate derivation.

The panel probes every ssh alias in ``~/.ssh/config`` with one non-interactive
``ssh <alias> true`` and records one of four states. For a host this panel has
itself seen come back after being held, it keeps a renewal baseline and derives
an estimated, upper-bound expiry of the trust-cache entry behind that renewal.
The remote side sees one read-only command per probe; the only file written is
this module's state file under the CharlieBot home directory.
"""

import asyncio
import json
import re
from datetime import datetime, timedelta
from pathlib import Path

from src.core.home import charliebot_home_dir
from src.core.json_utils import write_json_atomically
from src.core.log_once import LazyStructlogLogger
from src.core.models import utc_now
from src.core.ssh import ssh_cmd

log = LazyStructlogLogger()

# The standing probe period. A host currently held is probed at 12x this period:
# every held probe fires a device-verification request on the identity provider
# side, and a held state only changes when the operator renews and presses the
# page's manual probe, so the backoff loses no information.
PROBE_INTERVAL_SEC = 1800
BLOCKED_BACKOFF_FACTOR = 12
BLOCKED_BACKOFF_SEC = BLOCKED_BACKOFF_FACTOR * PROBE_INTERVAL_SEC  # 21600

# Trust-cache lifetime as its local derivation; the deployment source coordinates
# stay in the approved plan page, not in this comment.
TRUST_CACHE_TTL_SEC = 7 * 86400  # 604800

# Overall wait on one ssh subprocess. BatchMode and the connect timeout inside
# ssh_cmd fail fast on prompts and dead routes; the hang that remains is a held
# host's enrollment prompt, which this timeout kills. The kill surfaces as a
# signal exit, not the 124 a timeout(1) wrapper prints -- classification reads
# the output first for exactly that reason.
PROBE_TIMEOUT_SEC = 20.0

STATE_FILENAME = "host_auth.json"
DETAIL_MAX_CHARS = 200

STATUS_OK = "ok"
STATUS_NEEDS_OKTA = "needs-okta"
STATUS_NEEDS_INTERACTIVE_AUTH = "needs-interactive-auth"
STATUS_UNREACHABLE = "unreachable"

# Output markers, matched before any return code: a held host's enrollment prompt
# waits until the probe timeout kills it, so the return code of a held probe says
# nothing while its output names the hold.
_OKTA_MARKERS = ("Okta verification required", "activate?user_code=")
_INTERACTIVE_MARKERS = (
    "requires an additional check",
    "To authenticate, visit:",
    "Host key verification failed",
)
_URL_MARKER = "http"

# A Host pattern is collectable only when it is one plain name: no wildcard, no
# negation, no compound line.
_NON_LITERAL_PATTERN = re.compile(r"[*?!]")


def default_state_path() -> Path:
  """This profile's state file, under the CharlieBot home directory."""
  return charliebot_home_dir() / STATE_FILENAME


def empty_state() -> dict:
  """The state a fresh install serves: nothing probed, nothing running."""
  return {
      "probed_at": None,
      "probe_running": False,
      "interval_sec": PROBE_INTERVAL_SEC,
      "ttl_sec": TRUST_CACHE_TTL_SEC,
      "hosts": [],
  }


def load_state(state_path: Path | None = None) -> dict:
  """Read the state file; an unreadable or corrupt file is logged and answered with empty state."""
  path = state_path if state_path is not None else default_state_path()
  if not path.exists():
    return empty_state()
  try:
    state = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(state, dict) or not isinstance(state.get("hosts"), list):
      raise ValueError("state file does not hold a host_auth state object")
  except (OSError, ValueError) as e:
    log.warning("host_auth_state_read_failed", path=str(path), error=str(e))
    return empty_state()
  return state


def save_state(state: dict, state_path: Path) -> None:
  """Publish the state through a temporary file and an atomic rename."""
  state_path.parent.mkdir(parents=True, exist_ok=True)
  try:
    write_json_atomically(state_path, state, indent=2, newline=True)
  except OSError as e:
    log.error("host_auth_state_write_failed", path=str(state_path), error=str(e))
    raise


def classify(returncode: int | None, output: str) -> str:
  """Classify one probe by its output markers first and its return code second.

  A held host's enrollment prompt is killed by the probe timeout, so a held
  probe's return code describes the kill, not the host; the output is what
  names the hold and it wins. An answering probe (return code 0) is direct
  unless the output already named a hold.
  """
  if any(marker in output for marker in _OKTA_MARKERS):
    return STATUS_NEEDS_OKTA
  if returncode == 0:
    return STATUS_OK
  if any(marker in output for marker in _INTERACTIVE_MARKERS):
    return STATUS_NEEDS_INTERACTIVE_AUTH
  return STATUS_UNREACHABLE


def build_detail(returncode: int | None, output: str) -> str:
  """The classification basis: the first output line, plus the first URL line when one follows.

  An empty output carries no line, so the return code stands in. The whole
  detail is capped, so a chatty enrollment banner cannot inflate the state file.
  """
  stripped = output.strip()
  if not stripped:
    return f"exit {returncode}"
  lines = stripped.splitlines()
  parts = [lines[0].strip()]
  for line in lines[1:]:
    if _URL_MARKER in line:
      parts.append(line.strip())
      break
  return "\n".join(parts)[:DETAIL_MAX_CHARS]


async def probe_host(alias: str) -> tuple[int | None, str]:
  """Run one read-only probe of *alias*; ``(returncode, output)``.

  The output is captured even when the timeout kills the probe: a held host
  prints its enrollment notice and then waits, and that notice is the evidence
  the classifier reads. stdout and stderr are combined, since ssh and the
  remote side split their lines across both.
  """
  proc = await asyncio.create_subprocess_exec(
      *ssh_cmd(alias, "true"),
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
  )
  stdout_read = asyncio.create_task(proc.stdout.read())
  stderr_read = asyncio.create_task(proc.stderr.read())
  try:
    returncode = await asyncio.wait_for(proc.wait(), timeout=PROBE_TIMEOUT_SEC)
  except TimeoutError:
    proc.kill()
    returncode = await proc.wait()
  stdout_b = await stdout_read
  stderr_b = await stderr_read
  return returncode, (stdout_b + stderr_b).decode("utf-8", errors="replace")


def host_due(entry: dict | None, now: datetime) -> bool:
  """True when this host's probe backoff has elapsed.

  A host currently held waits 12x the standing period -- every held probe fires
  a device-verification request on the identity provider side, and the held
  state only changes when the operator renews, so the backoff loses nothing. A
  host with no record is always due.
  """
  if entry is None or entry.get("last_probe_at") is None:
    return True
  period = BLOCKED_BACKOFF_SEC if entry.get("status") == STATUS_NEEDS_OKTA else PROBE_INTERVAL_SEC
  last = datetime.fromisoformat(entry["last_probe_at"])
  return (now - last).total_seconds() >= period


def apply_probe_result(entry: dict, *, status: str, detail: str, probed_at: datetime) -> None:
  """Fold one probe result into *entry* under the one-transition baseline rule.

  The baseline moves on exactly one transition -- the probe after a held probe
  answers -- and on no other: passages through unreachable or interactive-auth
  states are not transition evidence, and an answering probe that merely repeats
  never refreshes the baseline, which would turn the upper bound into an
  optimistic value.
  """
  previous = entry.get("status")
  probed = probed_at.isoformat()
  entry["status"] = status
  entry["detail"] = detail
  entry["last_probe_at"] = probed
  if status == STATUS_OK:
    entry["last_ok_at"] = probed
  if status != previous:
    entry["last_change_at"] = probed
  if previous == STATUS_NEEDS_OKTA and status == STATUS_OK:
    entry["enrolled_observed_at"] = probed


def derive_estimate(entry: dict, now: datetime) -> dict:
  """The per-host estimate derivation, computed at read time and never persisted.

  ``estimated_expires_at`` is the renewal baseline plus the trust-cache
  lifetime, and it is an upper bound in both directions: it holds only under
  the page's standing renewal assumption (the operator renews through the
  browser enrollment flow, so the observed transition means the cache entry had
  just been written), and a login pod restart ends the entry sooner. Once the
  deadline has passed the remaining seconds go negative and stay there -- the
  row stops counting, it does not re-arm from a later probe.
  """
  baseline = entry.get("enrolled_observed_at")
  if baseline is None:
    return {"estimated_expires_at": None, "remaining_sec": None}
  expires = datetime.fromisoformat(baseline) + timedelta(seconds=TRUST_CACHE_TTL_SEC)
  return {
      "estimated_expires_at": expires.isoformat(),
      "remaining_sec": (expires - now).total_seconds(),
  }


def _unquote(value: str) -> str:
  if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
    return value[1:-1]
  return value


def parse_ssh_config_hosts(path: Path) -> list[tuple[str, str]]:
  """``(alias, hostname)`` for every collectable Host block in the ssh config.

  A block is collected only when its ``Host`` line names exactly one alias with
  no wildcard or negation and the block carries a ``HostName``; compound,
  wildcard, quoted-compound, and Match-governed blocks are skipped. The first
  block for an alias wins, matching ssh's own first-value rule. Under-collection
  is the accepted failure mode: the panel may miss a host, never misreport one.
  """
  if not path.exists():
    return []
  hosts: list[tuple[str, str]] = []
  seen: set[str] = set()
  patterns: list[str] = []
  hostname: str | None = None
  collectable = False

  def flush() -> None:
    if collectable and hostname is not None and patterns and patterns[0] not in seen:
      hosts.append((patterns[0], hostname))
      seen.add(patterns[0])

  for raw_line in path.read_text(encoding="utf-8").splitlines():
    line = raw_line.split("#", 1)[0].strip()
    if not line:
      continue
    tokens = line.replace("=", " ").split()
    keyword = tokens[0].lower()
    if keyword == "host":
      flush()
      patterns = [_unquote(token) for token in tokens[1:]]
      hostname = None
      collectable = len(patterns) == 1 and _NON_LITERAL_PATTERN.search(patterns[0]) is None
    elif keyword == "hostname":
      if len(tokens) > 1:
        hostname = _unquote(tokens[1])
    elif keyword == "match":
      flush()
      patterns, hostname, collectable = [], None, False
  flush()
  return hosts


def _new_entry(alias: str, hostname: str) -> dict:
  return {
      "alias": alias,
      "hostname": hostname,
      "status": None,
      "detail": "",
      "last_probe_at": None,
      "last_change_at": None,
      "last_ok_at": None,
      "enrolled_observed_at": None,
  }


def _merge_hosts(state: dict, host_list: list[tuple[str, str]]) -> dict[str, dict]:
  """Rebuild the state's host list in config order; return the entries by alias.

  The ssh config is the only source of the host set, so aliases it dropped fall
  out of the state and aliases it gained enter it unprobed. Existing entries
  keep their history; the config's HostName refreshes in place.
  """
  entries = {entry["alias"]: entry for entry in state["hosts"]}
  merged: list[dict] = []
  by_alias: dict[str, dict] = {}
  for alias, hostname in host_list:
    entry = entries.get(alias)
    if entry is None:
      entry = _new_entry(alias, hostname)
    entry["hostname"] = hostname
    merged.append(entry)
    by_alias[alias] = entry
  state["hosts"] = merged
  state["interval_sec"] = PROBE_INTERVAL_SEC
  state["ttl_sec"] = TRUST_CACHE_TTL_SEC
  return by_alias


async def run_round(
    *,
    force: bool = False,
    state_path: Path | None = None,
    ssh_config_path: Path | None = None,
) -> dict:
  """Probe every host whose backoff has elapsed (all of them when *force*), then publish the state.

  The round claims the state file with ``probe_running`` before probing, so a
  page rendered mid round knows to refresh itself, and the claim is always
  released in the finally. ``probed_at`` moves only when the round actually
  probed something: a tick that finds every host inside its backoff publishes
  the refreshed host list and nothing else.
  """
  # This is the only caller that can omit either path, so both resolve here,
  # once; the helpers take them required.
  path = state_path if state_path is not None else default_state_path()
  config_path = ssh_config_path if ssh_config_path is not None else Path.home() / ".ssh" / "config"
  state = load_state(path)
  host_list = parse_ssh_config_hosts(config_path)
  now = utc_now()
  entries = {entry["alias"]: entry for entry in state["hosts"]}
  due = [alias for alias, _ in host_list if force or host_due(entries.get(alias), now)]
  if not due:
    _merge_hosts(state, host_list)
    state["probe_running"] = False
    save_state(state, path)
    log.info("host_auth_probe_round", hosts=len(host_list), probed=0, forced=force)
    return state
  state["probe_running"] = True
  save_state(state, path)
  try:
    results = await asyncio.gather(*(probe_host(alias) for alias in due))
    moment = utc_now()
    merged = _merge_hosts(state, host_list)
    for alias, (returncode, output) in zip(due, results, strict=True):
      apply_probe_result(
          merged[alias],
          status=classify(returncode, output),
          detail=build_detail(returncode, output),
          probed_at=moment,
      )
    state["probed_at"] = moment.isoformat()
    log.info("host_auth_probe_round", hosts=len(host_list), probed=len(due), forced=force)
  finally:
    state["probe_running"] = False
    save_state(state, path)
  return state
