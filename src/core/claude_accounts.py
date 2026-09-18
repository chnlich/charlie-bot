"""Claude account pool: which subscription login serves a Claude Code process.

CharlieBot runs Claude Code under one of several subscription logins, each a
``CLAUDE_CONFIG_DIR`` holding its own credentials and transcript store. A
rate-limited account ends the master turn and the user switches by hand; the
pool moves that choice into the system. ``accounts.claude`` in config.yaml
lists the logins, every selection reads each
login's health (a usable credential file, no recent authentication failure) and
headroom (the newest rate-limit reading, where a live model-scoped weekly window
is never masked by a newer generic one), and a relay to another login is a
transcript copy into the target's ``projects`` tree followed by a same-id
``--resume`` there.

The pool is stateless: a selection reads configuration, credential files and the
newest observations, and persists nothing. The only process-local memory is the
newest rate-limit reading per account (from ``rate_limit_event`` and from the
usage panel poller) and the time of the account's last authentication failure,
all re-learned by any later run.

A backend entry is *pooled* when it is a cc-claude entry and ``accounts.claude``
is non-empty. A cc-claude entry has no login field of its own: with a non-empty
``accounts.claude`` every cc-claude entry is pooled, and with an empty one every
cc-claude entry uses the default login directory.
"""

from __future__ import annotations

import json
import os
import shutil
from collections.abc import Iterable
from collections.abc import Set as AbstractSet
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import TYPE_CHECKING, Any

from src.core.log_once import LazyStructlogLogger
from src.core.models import BackendOption, BackendType, ClaudeAccount

# future-annotations keep every cfg: CharlieBotConfig hint unevaluated; the config
# model stack must stay out of the claude-sub worker binary's import (it imports
# this module for CREDENTIALS_FILE).
if TYPE_CHECKING:
  from src.core.config import CharlieBotConfig

log = LazyStructlogLogger()

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

# Last-active timestamp of an account with no event reading: a never-active
# account sorts before every active one.
_NEVER_ACTIVE = datetime.min.replace(tzinfo=UTC)

# A live window that resets within a day earns its account up to
# ``_RESET_BONUS_SCALE`` of bonus headroom: quota about to lapse is spent
# before it lapses, but only from an account that still has room to spend.
_RESET_BONUS_SCALE = 0.10
_RESET_BONUS_HORIZON = 24 * 3600.0
_RESET_BONUS_MIN_HEADROOM = 0.10

# Scores closer than this are a near-tie, broken by least-recent event activity.
_SCORE_TIE = 0.02


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
# Accounts already reported as needing a login, cleared when they recover.
_login_notified: set[str] = set()


def reset_for_tests() -> None:
  """Drop every process-local reading (test isolation)."""
  _event_readings.clear()
  _panel_readings.clear()
  _auth_failures.clear()
  _login_notified.clear()


def now_or(now: datetime | None) -> datetime:
  """The injected clock, or the wall clock when the caller passed none."""
  return now if now is not None else datetime.now(UTC)


# ---------------------------------------------------------------------------
# Pool membership
# ---------------------------------------------------------------------------


def pool(cfg: CharlieBotConfig) -> list[ClaudeAccount]:
  """The configured accounts with ``config_dir`` expanded to an absolute path."""
  return [
      ClaudeAccount(label=account.label, config_dir=str(Path(account.config_dir).expanduser()))
      for account in cfg.accounts.claude
  ]


def is_pooled(option: BackendOption, cfg: CharlieBotConfig) -> bool:
  """True when *option* draws its login from the pool.

  A cc-claude entry has no login field of its own: a non-empty ``accounts.claude``
  pools every cc-claude entry, and every other backend family has no Claude login
  at all.
  """
  return option.type == BackendType.CC_CLAUDE and bool(cfg.accounts.claude)


def account_by_label(cfg: CharlieBotConfig, label: str | None) -> ClaudeAccount | None:
  if not label:
    return None
  return next((account for account in pool(cfg) if account.label == label), None)


# ---------------------------------------------------------------------------
# Health
# ---------------------------------------------------------------------------


def credentials_present(account: ClaudeAccount) -> bool:
  """True when the login's credential file carries a non-empty access token.

  A failed OAuth refresh can rewrite the file with both tokens emptied while the
  metadata survives, so the token field itself is the test;
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
  _auth_failures[label] = now_or(now)


def auth_failed_recently(label: str, now: datetime | None = None) -> bool:
  failed_at = _auth_failures.get(label)
  return failed_at is not None and now_or(now) - failed_at < AUTH_FAILURE_COOLDOWN


def login_notice_due(label: str, *, unhealthy: bool) -> bool:
  """True once per unhealthy spell of *label*: the first call after it goes unhealthy, and
  again only after a call has seen it healthy."""
  if not unhealthy:
    _login_notified.discard(label)
    return False
  if label in _login_notified:
    return False
  _login_notified.add(label)
  return True


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
  moment = now_or(now)
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
    at = datetime.fromisoformat(fetched_at) if isinstance(fetched_at, str) else now_or(now)
  except ValueError:
    at = now_or(now)
  if at.tzinfo is None:
    at = at.replace(tzinfo=UTC)
  _panel_readings[label] = {"at": at, "windows": [w for w in windows if isinstance(w, dict)]}


def parse_iso_utc(value: Any) -> datetime | None:
  """UTC datetime from an ISO-8601 string, or None when missing or unparseable."""
  if not isinstance(value, str) or not value:
    return None
  try:
    parsed = datetime.fromisoformat(value)
  except ValueError:
    return None
  return parsed.replace(tzinfo=UTC) if parsed.tzinfo is None else parsed


def panel_window_expired(window: dict[str, Any], sampled: datetime | None, now: datetime) -> bool:
  """Whether one stored usage-panel window's reading is expired at *now*.

  The single expiry rule shared by the account pool's fold and the usage
  panel's emit-time annotation: a window is expired when its reset has passed
  while the sample predates it, or when the sample is older than the window is
  long. An unparseable ``resets_at`` leaves only the age rule; a missing or
  unparseable sample time reads as live (never expired on a guess).
  """
  if sampled is None:
    return False
  resets_at = parse_iso_utc(window.get("resets_at"))
  if resets_at is not None and resets_at <= now and sampled < resets_at:
    return True
  window_minutes = window.get("window_minutes")
  if isinstance(window_minutes, int) and not isinstance(window_minutes, bool):
    return now - sampled > timedelta(minutes=window_minutes)
  return False


def _live_windows(label: str, model: str | None, now: datetime) -> list[dict[str, Any]]:
  """The stored panel windows of *label* that are live at *now* and readable by *model*.

  Windows the shared ``panel_window_expired`` rule marks expired are dropped,
  so a window whose reset has passed stops pressing the headroom; a scoped
  window counts only when its scope names *model*'s family, and a model-less
  query reads the plan-wide windows alone.
  """
  stored = _panel_readings.get(label)
  if stored is None:
    return []
  family = model_family(model)
  live: list[dict[str, Any]] = []
  for window in stored["windows"]:
    if panel_window_expired(window, stored["at"], now):
      continue
    scope = window.get("scope_label")
    if scope and not (family and family in str(scope).lower()):
      continue
    live.append(window)
  return live


def _panel_reading(label: str, model: str | None, now: datetime | None = None) -> RateLimitReading | None:
  """The panel reading folded for *model* over its live windows."""
  stored = _panel_readings.get(label)
  if stored is None:
    return None
  values: list[float] = []
  for window in _live_windows(label, model, now_or(now)):
    value = window.get("utilization")
    if isinstance(value, (int, float)) and not isinstance(value, bool):
      values.append(float(value) / 100.0)
  if not values:
    return None
  return RateLimitReading(at=stored["at"], utilization=max(values))


def latest_reading(label: str, model: str | None, now: datetime | None = None) -> RateLimitReading | None:
  """The account's effective reading for *model*: its windows fused orthogonally.

  A live model-scoped panel window (the Fable weekly bucket) is never masked by
  a newer generic reading: the effective utilization is the max across the
  general reading -- the newer of the event reading and the plan-wide panel
  fold -- and the model-scoped fold, so a fresh 20 percent 5h event cannot wash
  an 85 percent weekly bucket back down. The rejection of a rejected event
  reading holds the fusion to zero until its reset. With no live scoped window
  the reading is the general one alone.
  """
  moment = now_or(now)
  general = _newest_reading(_event_readings.get(label), _panel_reading(label, None, moment))
  if model is None or not any(window.get("scope_label") for window in _live_windows(label, model, moment)):
    return general
  scoped = _panel_reading(label, model, moment)
  if scoped is None:
    return general
  rejected_until = None
  if general is not None and general.rejected_until is not None and general.rejected_until > moment:
    rejected_until = general.rejected_until
  return RateLimitReading(
      at=max(general.at if general else _NEVER_ACTIVE, scoped.at),
      utilization=max(general.utilization if general else 0.0, scoped.utilization),
      rejected_until=rejected_until,
  )


def _newest_reading(*readings: RateLimitReading | None) -> RateLimitReading | None:
  """The newest of the given readings, ignoring the missing ones."""
  present = [reading for reading in readings if reading is not None]
  return max(present, key=lambda reading: reading.at) if present else None


def headroom(label: str, model: str | None, now: datetime | None = None) -> float:
  """Remaining share of the tightest window, 0 while a rejection's reset is ahead.

  An account nobody has read yet scores a full window: it is tried first and
  its first event supplies the reading.
  """
  reading = latest_reading(label, model, now)
  if reading is None:
    return 1.0
  if reading.rejected_until is not None and reading.rejected_until > now_or(now):
    return 0.0
  return max(0.0, 1.0 - reading.utilization)


def select(
    cfg: CharlieBotConfig,
    model: str | None,
    exclude: Iterable[str] = (),
    busy_accounts: AbstractSet[str] | None = None,
    now: datetime | None = None,
) -> ClaudeAccount | None:
  """The healthy account with the most headroom for *model*, ranked statelessly.

  The score is headroom plus a bonus for a window that resets within a day --
  quota about to lapse is spent before it lapses -- and scores closer than
  ``_SCORE_TIE`` break by least-recent event activity (a never-active account
  sorts first), so turns spread over the pool with no cursor state to restore
  after a restart.

  *busy_accounts* names accounts another running session holds: an idle account
  wins outright, and only when every healthy account is busy does the choice
  fall back to all of them. *exclude* names accounts a relay is leaving. An
  account with no headroom (a rejection whose reset is still ahead) is
  unavailable rather than a last resort: running there would only be rejected
  again. None when no account qualifies, which the caller reports loudly
  together with ``earliest_reset``.
  """
  moment = now_or(now)
  excluded = set(exclude)
  available: list[tuple[ClaudeAccount, float]] = []
  for account in pool(cfg):
    if account.label in excluded or not healthy(account, moment):
      continue
    if (hr := headroom(account.label, model, moment)) > 0.0:
      available.append((account, hr))
  if not available:
    return None
  idle = [(account, hr) for account, hr in available if busy_accounts is None or account.label not in busy_accounts]
  contenders = idle or available
  scored = [(hr + _reset_bonus(account.label, model, moment, hr), account) for account, hr in contenders]
  best = max(score for score, _account in scored)
  tied = [account for score, account in scored if best - score <= _SCORE_TIE]
  return min(tied, key=lambda account: _last_active_at(account.label))


def _last_active_at(label: str) -> datetime:
  """When the account's newest rate-limit event was read; a never-active account sorts first."""
  reading = _event_readings.get(label)
  return reading.at if reading is not None else _NEVER_ACTIVE


def _time_to_reset(label: str, model: str | None, now: datetime) -> float | None:
  """Seconds until the binding panel window for *model* resets, or None when unknown.

  The binding window is the live window with the highest utilization -- the one
  the headroom is pressed by, a model-scoped weekly bucket competing with the
  plan-wide ones. A missing or unparseable ``resets_at`` reads as no
  information rather than a guess.
  """
  limiting: tuple[float, dict[str, Any]] | None = None
  for window in _live_windows(label, model, now):
    value = window.get("utilization")
    if not isinstance(value, (int, float)) or isinstance(value, bool):
      continue
    if limiting is None or float(value) > limiting[0]:
      limiting = (float(value), window)
  if limiting is None:
    return None
  resets_at = parse_iso_utc(limiting[1].get("resets_at"))
  return (resets_at - now).total_seconds() if resets_at is not None else None


def _reset_bonus(label: str, model: str | None, moment: datetime, headroom_left: float) -> float:
  """Bonus for a live window that resets within a day: spend the quota before it lapses."""
  if headroom_left <= _RESET_BONUS_MIN_HEADROOM:
    return 0.0
  ttr = _time_to_reset(label, model, moment)
  if ttr is None or not 0.0 <= ttr < _RESET_BONUS_HORIZON:
    return 0.0
  return _RESET_BONUS_SCALE * (1.0 - ttr / _RESET_BONUS_HORIZON)


def earliest_reset(cfg: CharlieBotConfig, now: datetime | None = None) -> datetime | None:
  """The nearest rejection reset among pool accounts, for the pool-exhausted error."""
  moment = now_or(now)
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


# The move guard's refusal marker, carried by every refusal message; the relay
# consumers (place_turn, move_to_next_account) match it to tell the
# newer-transcript refusal -- the one an adoption may answer -- from every other
# copy failure.
GUARD_REFUSAL_MARKER = "refusing to overwrite newer transcript"

# Tail-window size of the placement probe's lineage check (see
# ``transcript_lineage_split``): two KB-scale reads per placement.
PROBE_TAIL_BYTES = 16 * 1024

# A staged copy's name suffix. The staging file lives in the destination
# directory but outside every path the transcript readers match
# (``*/<cc-id>.jsonl`` and the ``<cc-id>/`` sidecar), so a half-copied file is
# never visible as the transcript or as sidecar content.
_STAGING_SUFFIX = ".staging"


def is_newer_transcript_refusal(error: BaseException) -> bool:
  """True when *error* is the move guard's newer-transcript refusal."""
  return isinstance(error, TranscriptMoveError) and GUARD_REFUSAL_MARKER in str(error)


def transcript_matches(config_dir: str | Path, cc_session_id: str) -> list[Path]:
  """Every top-level conversation transcript for *cc_session_id* under *config_dir*, sorted.

  Top-level conversations live at ``projects/<cwd-slug>/<uuid>.jsonl``; the glob
  avoids depending on Claude Code's undocumented cwd-slug rule, and files nested
  deeper are subagent logs named ``agent-*.jsonl`` that cannot collide.
  """
  return sorted((Path(config_dir).expanduser() / "projects").glob(f"*/{cc_session_id}.jsonl"))


def transcript_path(config_dir: str | Path, cc_session_id: str) -> Path | None:
  """The first sorted transcript match from :func:`transcript_matches`, or None."""
  matches = transcript_matches(config_dir, cc_session_id)
  return matches[0] if matches else None


def find_transcript_account(cfg: CharlieBotConfig, cc_session_id: str) -> ClaudeAccount | None:
  """The first pool account whose transcript store holds *cc_session_id*."""
  return next((account for account in pool(cfg) if transcript_path(account.config_dir, cc_session_id)), None)


def _dst_holds_newer_transcript(src: Path, dst: Path) -> bool:
  """True when copying *src* over *dst* would overwrite a strictly newer transcript.

  The single-writer invariant grounds the mtime reading: the only writer of a
  live transcript copy is the claude process running on that login (every write
  stamps a fresh mtime), and this move's own ``copy2`` propagates the source's
  mtime to its destination -- so a destination whose mtime is strictly newer
  than the source holds a state the source never reached. Equal mtimes with a
  larger destination are the same refusal (it grew under the same stamp); equal
  mtimes AND equal size are the same copy re-moved and pass through.
  """
  if not dst.exists():
    return False
  src_stat, dst_stat = src.stat(), dst.stat()
  if dst_stat.st_mtime_ns > src_stat.st_mtime_ns:
    return True
  return dst_stat.st_mtime_ns == src_stat.st_mtime_ns and dst_stat.st_size > src_stat.st_size


def _refusal_detail(src: Path, dst: Path) -> str:
  src_stat, dst_stat = src.stat(), dst.stat()
  return (
      f"dst {dst} (mtime {datetime.fromtimestamp(dst_stat.st_mtime, UTC).isoformat()}, size {dst_stat.st_size})"
      f" vs src {src} (mtime {datetime.fromtimestamp(src_stat.st_mtime, UTC).isoformat()}, size {src_stat.st_size})")


def _is_staging_name(name: str) -> bool:
  return name.endswith(_STAGING_SUFFIX)


def _sidecar_files(root: Path) -> set[Path]:
  """Relative paths of *root*'s files, staged half-products excluded."""
  if not root.is_dir():
    return set()
  return {path.relative_to(root) for path in root.rglob("*") if path.is_file() and not _is_staging_name(path.name)}


def _sidecar_file_diff(sidecar: Path, dst_sidecar: Path) -> str:
  """The two sidecar trees' file-set diff: missing at the destination, born at the
  destination, or present on both sides with different sizes."""
  src_rels, dst_rels = _sidecar_files(sidecar), _sidecar_files(dst_sidecar)
  parts: list[str] = []
  if missing := sorted(src_rels - dst_rels):
    parts.append(f"missing at dst: {[str(rel) for rel in missing]}")
  if extra := sorted(dst_rels - src_rels):
    parts.append(f"only at dst: {[str(rel) for rel in extra]}")
  if differing := sorted(
      rel for rel in src_rels & dst_rels if (sidecar / rel).stat().st_size != (dst_sidecar / rel).stat().st_size):
    parts.append(f"size differs: {[str(rel) for rel in differing]}")
  return "; ".join(parts) if parts else "file sets equal"


def _move_sidecar(sidecar: Path, dst_sidecar: Path) -> tuple[int, int, int] | None:
  """Stage *sidecar*'s files into *dst_sidecar* one by one; report the file-set outcome.

  Per file: copy2 to a staging file in the destination tree, verify the size
  against the source, then ``os.replace`` into place -- the destination never
  holds a half-written sidecar file. The pass condition is the file set: every
  source file present and size-equal at the destination, which the staged
  per-file copies guarantee on completion. Files born at the destination
  (absent from the source) are retained and counted; the aggregate byte sum is
  report-only. A failure raises TranscriptMoveError carrying the file-set diff
  and leaves no staged half-products behind.
  """
  if not sidecar.is_dir():
    return None
  staged: list[Path] = []
  try:
    for src_file in sorted(path for path in sidecar.rglob("*") if path.is_file()):
      dst_file = dst_sidecar / src_file.relative_to(sidecar)
      dst_file.parent.mkdir(parents=True, exist_ok=True)
      tmp = dst_file.with_name(dst_file.name + _STAGING_SUFFIX)
      shutil.copy2(src_file, tmp)
      staged.append(tmp)
      if tmp.stat().st_size != src_file.stat().st_size:
        raise TranscriptMoveError(f"transcript sidecar copy size mismatch: {src_file} -> {dst_file}")
      os.replace(tmp, dst_file)
      staged.pop()
  except (OSError, TranscriptMoveError) as exc:
    raise TranscriptMoveError(
        f"transcript sidecar file-set mismatch: {sidecar} -> {dst_sidecar}"
        f" ({_sidecar_file_diff(sidecar, dst_sidecar)}); last error: {exc}") from exc
  finally:
    for tmp in staged:
      tmp.unlink(missing_ok=True)
  src_rels, dst_rels = _sidecar_files(sidecar), _sidecar_files(dst_sidecar)
  dst_only = dst_rels - src_rels
  total_bytes = sum((dst_sidecar / rel).stat().st_size for rel in src_rels)
  return len(src_rels), len(dst_only), total_bytes


def move_transcript(cc_session_id: str, src_dir: str | Path, dst_dir: str | Path) -> Path:
  """Copy the conversation ``<uuid>.jsonl`` and its ``<uuid>/`` sidecar into *dst_dir*.

  The destination keeps the source's cwd-slug directory, so a ``--resume`` from
  the same cwd under the new login finds it. The jsonl lands through a staged
  copy verified against the source and swapped in with ``os.replace`` -- the
  destination never holds a partial transcript. The guard refuses to overwrite
  a strictly newer destination copy (the kill between a relay's move and its
  label persist, or a forked lineage); the consumers of that refusal adopt the
  newer holder and continue from it, and the move layer itself never redirects.
  Equal mtime and size is the same copy re-moved and passes through. The source
  copy stays in place -- a failed relay falls back to it, and the queue
  consumer's post-round :func:`retire_transcript_copies` retires it once the
  round has ended soundly with its account label on disk. Raises
  TranscriptMoveError when the source is missing, the guard refuses, or a copy
  differs from its source.
  """
  src = transcript_path(src_dir, cc_session_id)
  if src is None:
    raise TranscriptMoveError(f"no transcript {cc_session_id}.jsonl under {Path(src_dir).expanduser() / 'projects'}")
  dst = Path(dst_dir).expanduser() / "projects" / src.parent.name / src.name
  dst.parent.mkdir(parents=True, exist_ok=True)
  if _dst_holds_newer_transcript(src, dst):
    src_stat, dst_stat = src.stat(), dst.stat()
    log.warning(
        "master_cc_transcript_move_stale_refused",
        cc_session_id=cc_session_id,
        src=str(src),
        dst=str(dst),
        src_mtime_ns=src_stat.st_mtime_ns,
        src_size=src_stat.st_size,
        dst_mtime_ns=dst_stat.st_mtime_ns,
        dst_size=dst_stat.st_size,
    )
    raise TranscriptMoveError(f"{GUARD_REFUSAL_MARKER}: {_refusal_detail(src, dst)}")
  staged = dst.with_name(dst.name + _STAGING_SUFFIX)
  try:
    shutil.copy2(src, staged)
    if staged.stat().st_size != src.stat().st_size:
      raise TranscriptMoveError(f"transcript copy size mismatch: {src} -> {dst}")
    os.replace(staged, dst)
  finally:
    staged.unlink(missing_ok=True)
  sidecar_report = _move_sidecar(src.with_suffix(""), dst.with_suffix(""))
  log.info(
      "claude_account_transcript_moved",
      cc_session_id=cc_session_id,
      src=str(src),
      dst=str(dst),
      sidecar_src_files=sidecar_report[0] if sidecar_report else None,
      sidecar_dst_only_files=sidecar_report[1] if sidecar_report else None,
      sidecar_bytes=sidecar_report[2] if sidecar_report else None,
  )
  return dst


def newest_transcript_copy(cfg: CharlieBotConfig, cc_session_id: str) -> tuple[ClaudeAccount, Path] | None:
  """The pool account holding the mtime-newest copy of *cc_session_id*'s transcript, with its path.

  The same per-cc-id ``*/<cc-id>.jsonl`` traversal across every pool root the
  resume fallback uses; the newest copy is where the session's live content
  continues, and equal mtimes break by path so the answer is deterministic.
  """
  best_key: tuple[int, Path] | None = None
  best: tuple[ClaudeAccount, Path] | None = None
  for account in pool(cfg):
    for path in transcript_matches(account.config_dir, cc_session_id):
      key = (path.stat().st_mtime_ns, path)
      if best_key is None or key > best_key:
        best_key, best = key, (account, path)
  return best


def _tail_window(path: Path) -> str:
  """The file's last ``PROBE_TAIL_BYTES``, decoded leniently."""
  with path.open("rb") as fh:
    fh.seek(0, os.SEEK_END)
    end = fh.tell()
    fh.seek(max(0, end - PROBE_TAIL_BYTES))
    return fh.read(PROBE_TAIL_BYTES).decode("utf-8", errors="replace")


def transcript_lineage_split(label_copy: Path, newest_copy: Path) -> bool:
  """True when the label copy's tail line is absent from the newest copy's tail window.

  The placement probe's lineage reading, additive to mtime: mtime alone proves
  newer-or-older, never who is whose successor. A label copy whose tail line the
  newest copy still contains is that copy's ancestor or equal -- one lineage;
  a tail line the newest copy does not contain means the two copies diverged
  (forked, or the label stale past the window). Tail windows keep the probe at
  two KB-scale reads; a label that grew past the window since it forked reads as
  forked, and the adoption reaction is the same either way. A blank label
  window has no tail line to compare and never reads as split.
  """
  tail_line = transcript_tail_line(label_copy)
  if tail_line is None:
    return False
  return tail_line not in _tail_window(newest_copy)


def transcript_tail_line(path: Path) -> str | None:
  """The last non-empty line inside *path*'s tail window, or None when it is blank."""
  lines = [line for line in _tail_window(path).splitlines() if line.strip()]
  return lines[-1] if lines else None


def retire_transcript_copies(cfg: CharlieBotConfig, cc_session_id: str, keep: int = 2) -> None:
  """Retire every pool copy of *cc_session_id*'s transcript except the newest *keep*.

  The live-session counterpart of storage_cool's cold-session sweep
  (``src/core/storage_cool.py::_sweep_claude_transcripts``): storage_cool
  retires whole transcript trees of sessions no reader can reach again, this
  retires the redundant copies a relay leaves behind for one still-live
  session -- the two deletion sets are disjoint, so neither can delete what the
  other protects. The queue consumer calls this only after a round that ended
  soundly and whose account label the funnel persisted, so the newest copies
  are the ones the label names; a failed round keeps every copy as its
  fallback.
  """
  copies = [
      (path.stat().st_mtime_ns, path)
      for account in pool(cfg)
      for path in transcript_matches(account.config_dir, cc_session_id)
  ]
  newest_first = sorted(copies, key=lambda entry: entry[0], reverse=True)
  for _mtime_ns, path in newest_first[keep:]:
    sidecar = path.with_suffix("")
    try:
      path.unlink()
      if sidecar.is_dir():
        shutil.rmtree(sidecar)
    except OSError as exc:
      log.warning("claude_account_transcript_retire_failed", path=str(path), error=str(exc))
      continue
    log.info("claude_account_transcript_retired", cc_session_id=cc_session_id, path=str(path))
