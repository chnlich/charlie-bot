"""The backend pool: the one selector for unpinned work, its rotation, and each member's cooldown.

``backends.pool`` lists the members in rotation order. Every caller that picks a backend without a
name asks :func:`select`: the four worker task types, the improve loop, the reviewer and the cold-read
chain, the session namer and the page check. The rotation position and the cooldowns live in process
memory, like the Claude account readings, so a restart clears both.

Vocabulary:

- A *member* is a backend id from ``backends.pool``.
- A *cooldown* keeps a member out of selection until its reset time, or for :data:`DEFAULT_COOLDOWN`
  when the quota error named no reset.
- The *position* is where the next selection starts. A selection moves it past the member it chose.
"""

import datetime
import threading
from collections.abc import Collection

import pydantic

from src.infra import config, log_once, models

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

# A quota error that names no reset keeps its member out this long.
DEFAULT_COOLDOWN = datetime.timedelta(minutes=30)

_lock = threading.Lock()
_position = 0
# backend id -> the moment the member is selectable again (UTC)
_cooling_until: dict[str, datetime.datetime] = {}

# The metadata.json owner of the pool's node keys (src/features/backend_pool registers the model).
METADATA_OWNER = "backend_pool"


class PoolNodeMetadata(pydantic.BaseModel):
  """The pool's key in a worker node's metadata.json: whether the node's backend was chosen from the pool."""

  backend_from_pool: bool = False


def reset_for_tests() -> None:
  """Clear the rotation position and every cooldown; the suite calls it around each test."""
  global _position
  with _lock:
    _position = 0
    _cooling_until.clear()


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
      if member in exclude:
        log.info("backend_pool_skipped", caller=caller, member=member, reason="excluded by caller")
        continue
      until = _cooling_until.get(member)
      if until is not None and until > now:
        log.info(
            "backend_pool_skipped",
            caller=caller,
            member=member,
            reason=f"cooling until {until.isoformat(timespec='minutes')}")
        continue
      _position = (index + 1) % len(members)
      log.info("backend_pool_selected", caller=caller, member=member, position=index)
      return config.require_backend_option(cfg, member, subject="backends.pool entry ")
  log.warning("backend_pool_exhausted", caller=caller, members=list(members), excluded=sorted(exclude))
  return None


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
