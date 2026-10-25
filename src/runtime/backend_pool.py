"""The backend pool: the one selector for unpinned work, its rotation, and each member's cooldown.

``backends.pool`` lists the members in rotation order. Every caller that picks a backend without a
name asks :func:`select`: the four worker task types, the improve loop, the reviewer and the cold-read
chain, the session namer and the page check. A caller that retries down the pool on failure asks
:func:`iter_members` instead. The rotation position and the cooldowns live in process memory, like the
Claude account readings, so a restart clears both.

Vocabulary:

- A *member* is a backend id from ``backends.pool``.
- A *cooldown* keeps a member out of selection until its reset time, or for :data:`DEFAULT_COOLDOWN`
  when the quota error named no reset.
- The *position* is where the next selection starts. A selection moves it past the member it chose.
- A *route* is the ordered backend list a host-configured router program returns for one role
  (:func:`route`); the first available member of the route runs the work, the pool takes over
  when the route is empty or spent.
"""

import asyncio
import datetime
import json
import threading
from collections.abc import Collection, Iterator

import pydantic

from src.infra import config, log_once, models, timeouts

log = log_once.LazyStructlogLogger()

# Substrings of an error's text that mean the quota is spent. Matching is case-insensitive. The first
# matching entry names the blocker in an improve loop report, so an entry contained in an earlier one
# is left out: "quota" covers every "quota ..." phrase.
QUOTA_ERROR_PATTERNS: tuple[str, ...] = (
    "quota exhausted",
    "quota exceeded",
    "insufficient quota",
    "over quota",
    "rate limit exceeded",
    "rate limit reached",
    "rate limit rejected",
    "rate-limit exceeded",
    "rate-limit reached",
    "rate-limit rejected",
    "rate_limited",
    "rate_limit_error",
    "rate limited",
    "too many requests",
    "resource_exhausted",
    "429",
    "out of token",
    "out-of-token",
    "insufficient tokens",
    "tokens exhausted",
    "quota",
    "rate limit",
    "usage limit",  # Codex: "You've hit your usage limit"
    "budget has been exceeded",  # LiteLLM: "Budget has been exceeded!"
)

# These substrings identify temporary provider and gateway errors or no-output stalls for pool failover.
# They exclude bare HTTP status digits (also found in request ids) and BadRequest, HTTP 400, or launch failures
# (request or configuration errors that must fail fast).
BACKEND_TEMPORARY_UNAVAILABLE_PATTERNS: tuple[str, ...] = (
    "at capacity",
    "bad gateway",
    "badgateway",
    "internal server error",
    "internalservererror",
    "service unavailable",
    "serviceunavailable",
    "gateway timeout",
    "apiconnectionerror",
    "connection error",
    "peer closed connection",
    "no output for",
    "no session progress for",
)


# Classify Claude Code result errors for pool failover; status comes first so other 4xx result text is never inspected.
def is_temporary_unavailable_result(event: dict) -> bool:
  """True when a Claude Code result event reports a temporary backend failure."""
  status = event.get("api_error_status")
  if status is not None:
    if isinstance(status, int) and not isinstance(status, bool):
      return status == 429 or status >= 500
    return False
  result_text = str(event.get("result", "")).lower()
  return any(pattern in result_text for pattern in BACKEND_TEMPORARY_UNAVAILABLE_PATTERNS)


# A quota error that names no reset keeps its member out this long.
DEFAULT_COOLDOWN = datetime.timedelta(minutes=30)

# The whole router call — process start, its work, the stdout read — is bounded under the delegate
# client's HTTP_INTERNAL_API_TIMEOUT: the delegation calls the router before the child task exists,
# so a router call outliving that timeout would turn a known failure into a lost result.
ROUTER_TIMEOUT = timeouts.ROUTER_TIMEOUT_SECONDS

# How much of a failed router's stderr an error message carries.
ROUTER_STDERR_TAIL_CHARS = 500

_lock = threading.Lock()
_position = 0
# backend id -> the moment the member is selectable again (UTC)
_cooling_until: dict[str, datetime.datetime] = {}

# The metadata.json owner of the pool's node keys (src/features/backend_pool registers the model).
METADATA_OWNER = "backend_pool"


class BackendRoute(pydantic.BaseModel):
  """The router stdout object as returned: the ordered backend list and its one-line reason."""

  backends: list[str]
  reason: str


class PoolNodeMetadata(pydantic.BaseModel):
  """The pool's key in a worker node's metadata.json: whether the node's backend was chosen from the pool."""

  backend_from_pool: bool = False
  # The router's stdout object for the node's role; None when no router ran (a named backend,
  # no backends.routing section, a caller outside the routed roles).
  route: BackendRoute | None = None


class RouterError(RuntimeError):
  """A router program call failed: it could not start, exited non-zero, timed out, returned
  stdout outside the contract, or named an id outside the candidates."""


def is_quota_error(text: str) -> bool:
  """True when *text* names a spent quota: one of :data:`QUOTA_ERROR_PATTERNS` appears in it."""
  lowered = text.lower()
  return any(pattern in lowered for pattern in QUOTA_ERROR_PATTERNS)


def cool(backend_id: str, resets_at: datetime.datetime | None) -> datetime.datetime:
  """Keep *backend_id* out of selection until *resets_at*, or for :data:`DEFAULT_COOLDOWN` when it is None.

  Returns the moment the member becomes selectable again.
  """
  now = models.utc_now()
  until = resets_at if resets_at is not None else now + DEFAULT_COOLDOWN
  with _lock:
    _cooling_until[backend_id] = until
  log.warning(
      "backend_pool_cooled",
      member=backend_id,
      until=until.isoformat(timespec="minutes"),
      reset_known=resets_at is not None)
  return until


def _skip_reason(member: str, exclude: Collection[str], now: datetime.datetime) -> str | None:
  """Why a selector must skip *member* at *now*, or None when it may take it.

  The caller holds :data:`_lock`, so the cooldown read agrees with the position that same lock guards.
  """
  if member in exclude:
    return "excluded by caller"
  until = _cooling_until.get(member)
  if until is not None and until > now:
    return f"cooling until {until.isoformat(timespec='minutes')}"
  return None


def select(
    cfg: config.CharlieBotConfig,
    *,
    caller: str,
    exclude: Collection[str] = (),
) -> models.BackendOption | None:
  """The first member from the rotation position that is neither cooling nor excluded, or None.

  The position then moves past the chosen member. Each skipped member is logged with its reason.
  """
  global _position
  now = models.utc_now()
  members = cfg.backends.pool
  with _lock:
    for offset in range(len(members)):
      index = (_position + offset) % len(members)
      member = members[index]
      reason = _skip_reason(member, exclude, now)
      if reason is not None:
        log.info("backend_pool_skipped", caller=caller, member=member, reason=reason)
        continue
      _position = (index + 1) % len(members)
      log.info("backend_pool_selected", caller=caller, member=member, position=index)
      return config.require_backend_option(cfg, member, subject="backends.pool entry ")
  log.warning("backend_pool_exhausted", caller=caller, members=list(members), excluded=sorted(exclude))
  return None


def iter_members(cfg: config.CharlieBotConfig, *, caller: str) -> Iterator[models.BackendOption]:
  """Yield one available member per call, in rotation order, each member at most once.

  A caller that fails on a member asks for the next: :func:`select` skips both the members
  already yielded and the cooling ones.
  """
  tried: list[str] = []
  while (option := select(cfg, caller=caller, exclude=tried)) is not None:
    tried.append(option.id)
    yield option


def cooling_until(backend_id: str) -> datetime.datetime | None:
  """The moment *backend_id* becomes selectable again, or None when it is not cooling."""
  with _lock:
    return _cooling_until.get(backend_id)


def pick_ordered(
    cfg: config.CharlieBotConfig,
    *,
    caller: str,
    backends: list[str],
    exclude: Collection[str] = (),
) -> models.BackendOption | None:
  """The first member of *backends* that is neither cooling nor excluded, or None.

  The route order is fixed — the position never moves — so a spent route returns the same None the
  pool fallback then answers. The cooldown table and lock are :func:`select`'s own, so a member the
  router ordered skips exactly when the pool would skip it.
  """
  now = models.utc_now()
  with _lock:
    for member in backends:
      reason = _skip_reason(member, exclude, now)
      if reason is not None:
        log.info("backend_pool_skipped", caller=caller, member=member, reason=reason)
        continue
      log.info("backend_pool_selected", caller=caller, member=member, source="route")
      return config.require_backend_option(cfg, member, subject="routed backend ")
  log.warning("backend_pool_route_exhausted", caller=caller, backends=backends, excluded=sorted(exclude))
  return None


async def route(cfg: config.CharlieBotConfig, *, role: str, text: str, candidates: list[str]) -> BackendRoute:
  """Ask the host-configured router program to order *candidates* for one *role*; return its stdout.

  The subprocess reads one JSON object on stdin (role, text, each candidate's id and cooling
  deadline) and answers one JSON object on stdout (``backends``, ``reason``) with exit 0 — the
  contract of ``configs/config.example.yaml``'s ``backends.routing`` example. Every other outcome —
  a command that cannot start, a non-zero exit, a timeout, stdout outside the contract, an id
  outside *candidates* — raises :class:`RouterError` naming the command; the caller stops, it never
  falls back to the pool silently.
  """
  command = cfg.backends.routing.command
  if not command:
    raise RouterError("backends.routing.command is empty")
  now = models.utc_now()
  candidate_entries = []
  for candidate in candidates:
    until = cooling_until(candidate)
    active = until if until is not None and until > now else None
    candidate_entries.append({"id": candidate, "cooling_until": active.isoformat() if active else None})
  payload = {"role": role, "text": text, "candidates": candidate_entries}
  try:
    proc = await asyncio.create_subprocess_exec(
        *command, stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
  except OSError as e:
    raise RouterError(f"backend router command {command[0]} could not start: {e}") from e
  try:
    stdout, stderr = await asyncio.wait_for(proc.communicate(json.dumps(payload).encode()), ROUTER_TIMEOUT)
  except asyncio.TimeoutError as e:
    proc.kill()
    await proc.wait()
    raise RouterError(f"backend router command {command[0]} timed out after {ROUTER_TIMEOUT}s") from e
  if proc.returncode != 0:
    raise RouterError(f"backend router command {command[0]} exited {proc.returncode}; stderr: {_stderr_tail(stderr)}")
  try:
    parsed = json.loads(stdout)
  except (json.JSONDecodeError, UnicodeDecodeError) as e:
    raise RouterError(
        f"backend router command {command[0]} exited {proc.returncode} with stdout outside the router "
        f"contract ({e}); stderr: {_stderr_tail(stderr)}") from e
  if (not isinstance(parsed, dict) or not isinstance(parsed.get("backends"), list) or
      not all(isinstance(b, str) for b in parsed["backends"]) or not isinstance(parsed.get("reason"), str)):
    raise RouterError(
        f"backend router command {command[0]} exited {proc.returncode} with stdout outside the router "
        f"contract (backends: list[str], reason: str expected); stderr: {_stderr_tail(stderr)}")
  unknown = [b for b in parsed["backends"] if b not in candidates]
  if unknown:
    raise RouterError(
        f"backend router command {command[0]} returned ids outside the candidates: {unknown}; "
        f"stderr: {_stderr_tail(stderr)}")
  route_out = BackendRoute(backends=parsed["backends"], reason=parsed["reason"])
  log.info("backend_router_routed", role=role, backends=route_out.backends, reason=route_out.reason)
  return route_out


def _stderr_tail(stderr: bytes) -> str:
  """The last :data:`ROUTER_STDERR_TAIL_CHARS` characters of a router's stderr, for an error message."""
  return stderr.decode(errors="replace").strip()[-ROUTER_STDERR_TAIL_CHARS:]


def recovery_report(cfg: config.CharlieBotConfig) -> str:
  """Each member and its state, for a caller that found no member available.

  A cooling member names the moment it becomes selectable again.
  """
  now = models.utc_now()
  parts = []
  for member in cfg.backends.pool:
    until = _cooling_until.get(member)
    if until is not None and until > now:
      parts.append(f"{member} cooling until {until.isoformat(timespec='minutes')}")
    else:
      parts.append(f"{member} not cooling")
  return "; ".join(parts) if parts else "backends.pool is empty"


def no_member_message(cfg: config.CharlieBotConfig) -> str:
  """The line a caller reports when :func:`select` found no member.

  Subagent resolution, the improve loop's per-iteration pick and the quota failover all
  report it, and an operator reads the same wording whichever one refused.
  """
  return f"no backends.pool member is available: {recovery_report(cfg)}"
