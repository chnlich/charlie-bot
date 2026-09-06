"""Claude account pool: which subscription login serves a Claude Code process.

CharlieBot runs Claude Code under one of several subscription logins, each a
``CLAUDE_CONFIG_DIR`` holding its own credentials and transcript store. With the
login baked into the backend entry, a rate-limited account ends the master turn
and the user switches by hand; the pool moves that choice into the system.
``claude_accounts`` in config.yaml lists the logins, every selection reads each
login's health (a usable credential file, no recent authentication failure) and
headroom (the newest rate-limit reading), and a relay to another login is a
transcript copy into the target's ``projects`` tree followed by a same-id
``--resume`` there.

The pool is stateless: a selection reads configuration, credential files and the
newest observations, and persists nothing. The only process-local memory is the
newest rate-limit reading per account (from ``rate_limit_event`` and from the
usage panel poller) and the time of the account's last authentication failure,
all re-learned by any later run.

A backend entry is *pooled* when it is a cc-claude entry without
``claude_config_dir`` and the config declares ``claude_accounts``. An entry with
its own ``claude_config_dir`` stays pinned to that login, and a config without
``claude_accounts`` behaves exactly as it did before this module existed.
"""

from __future__ import annotations

import json
import shutil
from collections.abc import Iterable
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import structlog

from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, BackendType, ClaudeAccount

log = structlog.get_logger()

# Resume domain shared by every pooled cc-claude entry (src/api/sessions.py):
# Fable, Opus and Sonnet switch in place inside the pool without touching accounts.
POOL_DOMAIN = "pool"

# Utilization at which Claude Code starts reporting ``allowed_warning``; an
# account at or above it is one to leave before the window rejects.
WARNING_UTILIZATION = 0.90

# An account whose last authentication failure is younger than this is skipped:
# a failed OAuth refresh is not cleared by retrying within minutes.
AUTH_FAILURE_COOLDOWN = timedelta(minutes=15)

CREDENTIALS_FILE = ".credentials.json"

# The two binding windows every ``rate_limit_event`` carries under
# ``unifiedWindows``; the overage window is not a limit on the subscription.
_EVENT_WINDOWS = ("five_hour", "seven_day")

# A rejected event without ``resetsAt`` keeps the account out for this long.
_REJECTED_WITHOUT_RESET = timedelta(hours=1)


@dataclass(frozen=True)
class RateLimitReading:
  """Newest utilization known for one account, from either reading source.

  ``utilization`` is a fraction (0.92 = 92 percent) over the five-hour window,
  the seven-day window and, when the source reports one, the model-scoped
  weekly bucket. ``rejected_until`` is the reset time of a rejected
  ``rate_limit_event``; None for every other reading.
  """
  at: datetime
  utilization: float
  rejected_until: datetime | None = None


# Newest ``rate_limit_event`` per account label, written by the master and
# worker event loops of any run on that account.
_event_readings: dict[str, RateLimitReading] = {}
# Newest usage-panel reading per account label, keyed by the model family the
# scoped windows were folded for ("" = plan-wide only), written by the ext_usage poller.
_panel_readings: dict[str, dict[str, Any]] = {}
# Time of the last authentication failure per account label.
_auth_failures: dict[str, datetime] = {}


def reset_for_tests() -> None:
  """Drop every process-local reading (test isolation)."""
  _event_readings.clear()
  _panel_readings.clear()
  _auth_failures.clear()


def _now(now: datetime | None) -> datetime:
  return now if now is not None else datetime.now(UTC)


# ---------------------------------------------------------------------------
# Pool membership
# ---------------------------------------------------------------------------


def pool(cfg: CharlieBotConfig) -> list[ClaudeAccount]:
  """The configured accounts with ``config_dir`` expanded to an absolute path."""
  return [
      ClaudeAccount(label=account.label, config_dir=str(Path(account.config_dir).expanduser()))
      for account in cfg.claude_accounts
  ]


def is_pooled(option: BackendOption, cfg: CharlieBotConfig) -> bool:
  """True when *option* draws its login from the pool.

  A cc-claude entry without ``claude_config_dir`` is pooled as soon as the config
  declares ``claude_accounts``; an entry carrying its own directory stays pinned,
  and every other backend family has no Claude login at all.
  """
  return option.type == BackendType.CC_CLAUDE and not option.claude_config_dir and bool(cfg.claude_accounts)


def account_by_label(cfg: CharlieBotConfig, label: str | None) -> ClaudeAccount | None:
  if not label:
    return None
  return next((account for account in pool(cfg) if account.label == label), None)


def account_for_dir(cfg: CharlieBotConfig, config_dir: str | Path) -> ClaudeAccount | None:
  """The pool account whose login directory is *config_dir*, or None."""
  wanted = str(Path(config_dir).expanduser())
  return next((account for account in pool(cfg) if account.config_dir == wanted), None)


# ---------------------------------------------------------------------------
# Health
# ---------------------------------------------------------------------------


def credentials_present(account: ClaudeAccount) -> bool:
  """True when the login's credential file carries a non-empty access token.

  A failed OAuth refresh can rewrite the file with both tokens emptied while the
  metadata survives (LESSONS 2026-08-03), so the token field itself is the test;
  a missing or unreadable file counts as absent credentials.
  """
  path = Path(account.config_dir) / CREDENTIALS_FILE
  try:
    data = json.loads(path.read_text(encoding="utf-8"))
  except (OSError, ValueError):
    return False
  oauth = data.get("claudeAiOauth") if isinstance(data, dict) else None
  return bool(isinstance(oauth, dict) and oauth.get("accessToken"))


def record_auth_failure(label: str, now: datetime | None = None) -> None:
  """Remember that a process on *label* failed to authenticate at *now*."""
  _auth_failures[label] = _now(now)


def auth_failed_recently(label: str, now: datetime | None = None) -> bool:
  failed_at = _auth_failures.get(label)
  return failed_at is not None and _now(now) - failed_at < AUTH_FAILURE_COOLDOWN


def healthy(account: ClaudeAccount, now: datetime | None = None) -> bool:
  """An account with credentials on disk and no authentication failure in the cooldown."""
  return credentials_present(account) and not auth_failed_recently(account.label, now)


# ---------------------------------------------------------------------------
# Headroom
# ---------------------------------------------------------------------------


def model_family(model: str | None) -> str:
  """The family word of a Claude model id: ``claude-fable-5-1`` -> ``fable``."""
  if not model:
    return ""
  parts = model.lower().split("-")
  return parts[1] if len(parts) > 1 and parts[0] == "claude" else parts[0]


def observe_rate_limit(label: str, info: dict, now: datetime | None = None) -> RateLimitReading | None:
  """Fold one ``rate_limit_info`` payload into the account's newest event reading.

  Returns the reading stored, or None when the payload carries neither a
  utilization nor a rejection (nothing to learn from it).
  """
  moment = _now(now)
  windows = info.get("unifiedWindows") if isinstance(info, dict) else None
  values: list[float] = []
  if isinstance(windows, dict):
    for name in _EVENT_WINDOWS:
      window = windows.get(name)
      value = window.get("utilization") if isinstance(window, dict) else None
      if isinstance(value, (int, float)) and not isinstance(value, bool):
        values.append(float(value))
  rejected = info.get("status") == "rejected"
  if not values and not rejected:
    return None
  rejected_until: datetime | None = None
  if rejected:
    resets_at = info.get("resetsAt")
    if isinstance(resets_at, (int, float)) and not isinstance(resets_at, bool):
      rejected_until = datetime.fromtimestamp(float(resets_at), UTC)
    else:
      rejected_until = moment + _REJECTED_WITHOUT_RESET
  reading = RateLimitReading(at=moment, utilization=max(values) if values else 1.0, rejected_until=rejected_until)
  _event_readings[label] = reading
  return reading


def observe_usage_panel(label: str, usage: dict, now: datetime | None = None) -> None:
  """Store a usage-panel result (``ext_usage`` window list) as the account's panel reading.

  Panel utilizations are percentages; they are kept as reported and scaled when
  read, together with the ``scope_label`` that names a model-scoped window.
  """
  windows = usage.get("windows") if isinstance(usage, dict) else None
  if not isinstance(windows, list):
    return
  fetched_at = usage.get("fetched_at")
  try:
    at = datetime.fromisoformat(fetched_at) if isinstance(fetched_at, str) else _now(now)
  except ValueError:
    at = _now(now)
  if at.tzinfo is None:
    at = at.replace(tzinfo=UTC)
  _panel_readings[label] = {"at": at, "windows": [w for w in windows if isinstance(w, dict)]}


def _panel_reading(label: str, model: str | None) -> RateLimitReading | None:
  """The panel reading folded for *model*: plan-wide windows plus its scoped bucket."""
  stored = _panel_readings.get(label)
  if stored is None:
    return None
  family = model_family(model)
  values: list[float] = []
  for window in stored["windows"]:
    value = window.get("utilization")
    if not isinstance(value, (int, float)) or isinstance(value, bool):
      continue
    scope = window.get("scope_label")
    if scope and not (family and family in str(scope).lower()):
      continue
    values.append(float(value) / 100.0)
  if not values:
    return None
  return RateLimitReading(at=stored["at"], utilization=max(values))


def latest_reading(label: str, model: str | None) -> RateLimitReading | None:
  """The newer of the account's event reading and its panel reading, or None."""
  candidates = [reading for reading in (_event_readings.get(label), _panel_reading(label, model)) if reading]
  if not candidates:
    return None
  return max(candidates, key=lambda reading: reading.at)


def headroom(label: str, model: str | None, now: datetime | None = None) -> float:
  """Remaining share of the tightest window, 0 while a rejection's reset is ahead.

  An account nobody has read yet scores a full window: it is tried first and
  its first event supplies the reading.
  """
  reading = latest_reading(label, model)
  if reading is None:
    return 1.0
  if reading.rejected_until is not None and reading.rejected_until > _now(now):
    return 0.0
  return max(0.0, 1.0 - reading.utilization)


def select(
    cfg: CharlieBotConfig,
    model: str | None,
    current: str | None = None,
    exclude: Iterable[str] = (),
    now: datetime | None = None,
) -> ClaudeAccount | None:
  """The healthy account with the most headroom for *model*; ties keep *current*.

  *exclude* names accounts a relay is leaving. An account with no headroom (a
  rejection whose reset is still ahead) is unavailable rather than a last resort:
  running there would only be rejected again. None when no account qualifies,
  which the caller reports loudly together with ``earliest_reset``.
  """
  excluded = set(exclude)
  ranked = [
      (headroom(account.label, model, now), account.label == current, account)
      for account in pool(cfg)
      if account.label not in excluded and healthy(account, now)
  ]
  available = [entry for entry in ranked if entry[0] > 0.0]
  if not available:
    return None
  return max(available, key=lambda entry: entry[:2])[2]


def earliest_reset(cfg: CharlieBotConfig, now: datetime | None = None) -> datetime | None:
  """The nearest rejection reset among pool accounts, for the pool-exhausted error."""
  moment = _now(now)
  resets = [
      reading.rejected_until for account in pool(cfg) if (reading := _event_readings.get(account.label)) is not None and
      reading.rejected_until is not None and reading.rejected_until > moment
  ]
  return min(resets) if resets else None


# ---------------------------------------------------------------------------
# Transcripts
# ---------------------------------------------------------------------------


class TranscriptMoveError(RuntimeError):
  """A transcript copy between logins did not land byte-for-byte."""


def transcript_path(config_dir: str | Path, cc_session_id: str) -> Path | None:
  """The conversation transcript for *cc_session_id* under *config_dir*, or None.

  Top-level conversations live at ``projects/<cwd-slug>/<uuid>.jsonl``; the glob
  avoids depending on Claude Code's undocumented cwd-slug rule, and files nested
  deeper are subagent logs named ``agent-*.jsonl`` that cannot collide.
  """
  matches = sorted((Path(config_dir).expanduser() / "projects").glob(f"*/{cc_session_id}.jsonl"))
  return matches[0] if matches else None


def find_transcript_account(cfg: CharlieBotConfig, cc_session_id: str) -> ClaudeAccount | None:
  """The first pool account whose transcript store holds *cc_session_id*."""
  return next((account for account in pool(cfg) if transcript_path(account.config_dir, cc_session_id)), None)


def _tree_bytes(root: Path) -> int:
  return sum(path.stat().st_size for path in root.rglob("*") if path.is_file())


def move_transcript(cc_session_id: str, src_dir: str | Path, dst_dir: str | Path) -> Path:
  """Copy the conversation ``<uuid>.jsonl`` and its ``<uuid>/`` sidecar into *dst_dir*.

  The destination keeps the source's cwd-slug directory, so a ``--resume`` from
  the same cwd under the new login finds it. Sizes are checked after the copy
  and the source stays in place (a failed relay can fall back to it; the cold
  storage sweep retires the old copy later). Raises TranscriptMoveError when
  the source is missing or the copy differs in size.
  """
  src = transcript_path(src_dir, cc_session_id)
  if src is None:
    raise TranscriptMoveError(f"no transcript {cc_session_id}.jsonl under {Path(src_dir).expanduser() / 'projects'}")
  dst = Path(dst_dir).expanduser() / "projects" / src.parent.name / src.name
  dst.parent.mkdir(parents=True, exist_ok=True)
  shutil.copy2(src, dst)
  if dst.stat().st_size != src.stat().st_size:
    raise TranscriptMoveError(f"transcript copy size mismatch: {src} -> {dst}")
  sidecar = src.with_suffix("")
  if sidecar.is_dir():
    dst_sidecar = dst.with_suffix("")
    shutil.copytree(sidecar, dst_sidecar, dirs_exist_ok=True)
    if _tree_bytes(dst_sidecar) != _tree_bytes(sidecar):
      raise TranscriptMoveError(f"transcript sidecar size mismatch: {sidecar} -> {dst_sidecar}")
  log.info("claude_account_transcript_moved", cc_session_id=cc_session_id, src=str(src), dst=str(dst))
  return dst
