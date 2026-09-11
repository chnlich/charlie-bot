"""Sidebar status snapshot and dirty registry — process memory only.

Single home of the derived sidebar-state cache behind ``/api/sessions/status``:
every writer of the probed state (session metadata, thread metadata, trigger
files, plans.json, thinking busy state) calls :func:`mark_sidebar_dirty` at the
transition, the poll re-probes only dirty sessions, and clean sessions are
served from the snapshot with zero disk access. Disk stays the single source of
truth — the snapshot is a rebuildable derived value and starts cold at boot;
nothing here is ever persisted.

Follows the ``thinking_state`` precedent: module-level set/dict + functions, no
constructor wiring; every accessor is a synchronous set/dict operation (no I/O,
no await), so transition-point calls are cheap and there is no check-then-act
window around a mark.
"""

from collections.abc import Iterable

# The sidebar dict keys, single-homed here: the probe snapshot and the
# per-request derived entry both build and read their dicts by these names,
# and /api/sessions relays the derived entry to web/static/js/sidebar/ verbatim.
# The probe snapshot (what a deep probe measures) carries THREAD_RUNNING; the
# derived entry resolves it into HAS_RUNNING_TASKS (plus the live busy state)
# and HAS_PENDING_TRIGGER.
THREAD_RUNNING = "thread_running"
PENDING_TRIGGER_COUNT = "pending_trigger_count"
NEXT_TRIGGER_AT = "next_trigger_at"
HAS_PENDING_PLAN_APPROVAL = "has_pending_plan_approval"
HAS_RUNNING_TASKS = "has_running_tasks"
HAS_PENDING_TRIGGER = "has_pending_trigger"

# Every Nth populate_sidebar_state call re-probes all active sessions: the
# bounded self-heal window for a state-transition write path that forgets to
# mark its session dirty (active polls ~3s -> stale field heals within ~30s).
_FORCE_FULL_EVERY = 10

# Session ids whose probed state changed since the last re-probe.
_dirty: set[str] = set()
# session id -> the probe snapshot, keyed by the constants above:
# THREAD_RUNNING bool, PENDING_TRIGGER_COUNT int, NEXT_TRIGGER_AT datetime | None,
# HAS_PENDING_PLAN_APPROVAL bool.
# A session without an entry is cold for that poll (probed like a dirty one),
# so an empty dict — a fresh boot — is a full probe.
_snapshot: dict[str, dict] = {}
# session id -> probe-input signature (stat-only identity of every file the
# sidebar probe reads). A selected re-probe whose signature matches the stored
# one — and whose 30-day scan-window rollover has not passed — re-derives from
# unchanged bytes, so the deep probe is skipped (the every-10th-poll self-heal
# sweep drops to a stat-only pass). Built and stored by the poll in
# src.core.sessions; the /status?force=1 escape hatch bypasses it.
_probe_signatures: dict[str, tuple] = {}
# populate_sidebar_state invocation counter (process lifetime).
_poll_count = 0

# session id -> monotone change revision, bumped by every mark_sidebar_dirty
# call. Consumers outside the poll prove a derived value against disk through
# a :class:`RevisionSweepGate` and skip the proof while it stands.
_revisions: dict[str, int] = {}

# session id -> row-source paths the writers marked since the last take. The
# workers-panel list poll proves its stored body against exactly these files
# (one stat per mark) instead of re-walking every row-source file; a mark
# without a path, or a taken-and-dropped race, leaves the poll on the full
# walk. Capped per session: an overflowing burst clears the set, and the next
# proof full-walks — the same verdict an empty set gets.
_MARKED_PATHS_CAP = 64
_marked_paths: dict[str, set[str]] = {}


def mark_sidebar_dirty(session_id: str, path: str | None = None) -> None:
  """Flag *session_id*'s probed sidebar state for re-probe on the next poll.

  *path* is the row-source file the caller just published through its atomic
  rename (thread metadata.json) — the list poll's incremental proof stats
  exactly the marked paths, so the mark must follow the rename.
  """
  _dirty.add(session_id)
  _revisions[session_id] = _revisions.get(session_id, 0) + 1
  if path is not None:
    paths = _marked_paths.setdefault(session_id, set())
    if len(paths) >= _MARKED_PATHS_CAP:
      # The burst outran the cap: drop every pending path, the newest included,
      # so the next poll finds no paths and full-walks — re-proving all row
      # sources at once, the proof a partially-taken set cannot give.
      paths.clear()
    else:
      paths.add(path)


def take_marked_paths(session_id: str) -> list[str]:
  """Consume the row-source paths marked since the last take."""
  return list(_marked_paths.pop(session_id, ()))


def session_revision(session_id: str) -> int:
  """Current change revision of *session_id*'s probed state sources."""
  return _revisions.get(session_id, 0)


class RevisionSweepGate:
  """Per-consumer revision gate with the every-Nth-poll sweep.

  A consumer proves a derived value against disk and serves the stored value
  while :meth:`serve_hit` says the proof stands: the session's change revision
  still matches the one the proof was taken at, and fewer than *sweep_every*
  polls passed since the proof. A hit bumps the poll count and never resets
  it, so the sweep arrives on schedule even when every poll hits;
  :meth:`mark_proven` resets the count at a fresh proof.

  The revision enters only through the caller, which reads it from
  :func:`session_revision` before its walk: a mark landing mid-walk or
  mid-rebuild only raises the live revision past the stored one, so the next
  poll re-walks instead of serving a value missing that write.
  """

  def __init__(self, sweep_every: int) -> None:
    self._sweep_every = sweep_every
    self._gates: dict[str, tuple[int, int]] = {}

  def serve_hit(self, session_id: str, revision: int) -> bool:
    """Consume one poll against the stored proof; True when it still stands."""
    gate = self._gates.get(session_id)
    if gate is None or gate[0] != revision or gate[1] + 1 >= self._sweep_every:
      return False
    self._gates[session_id] = (revision, gate[1] + 1)
    return True

  def mark_proven(self, session_id: str, revision: int, reset_sweep: bool = True) -> None:
    """Store a fresh proof taken at *revision*, resetting the sweep countdown.

    *reset_sweep=False* (the list poll's incremental proof, which covered only
    the marked files) advances the countdown by this poll instead: the full-walk
    sweep still arrives on its schedule under continuous marked polls.
    """
    count = 0 if reset_sweep else self._gates.get(session_id, (revision, 0))[1] + 1
    self._gates[session_id] = (revision, count)

  def marked_since_proof(self, session_id: str, revision: int) -> bool:
    """True when a proof stands for an older revision and the sweep is not due.

    The list poll's incremental branch requires both: a mark moved the revision
    since the proof (the marked paths say where), and the countdown still
    stands, so the scheduled full walk is never postponed by marked polls.
    """
    gate = self._gates.get(session_id)
    return gate is not None and gate[0] != revision and gate[1] + 1 < self._sweep_every

  def drop(self, session_id: str) -> None:
    """Forget the session's proof (its stored value failed the walk)."""
    self._gates.pop(session_id, None)

  def clear(self) -> None:
    """Forget every proof (the tests' cross-test pollution reset)."""
    self._gates.clear()


def is_dirty(session_id: str) -> bool:
  """True if *session_id* is flagged for re-probe."""
  return session_id in _dirty


def discard_dirty(session_ids: Iterable[str]) -> None:
  """Drop *session_ids* from the dirty set (no-op for unknown ids).

  The poll calls this with exactly the ids it selected for re-probe, at
  selection time: a transition mark landing while the probe runs re-adds the
  id, so that write is picked up by the next poll even if the in-flight probe
  raced it.
  """
  for session_id in session_ids:
    _dirty.discard(session_id)


def snapshot_entry(session_id: str) -> dict | None:
  """The probed sidebar-state entry for *session_id*, or None when cold for it."""
  return _snapshot.get(session_id)


def required_snapshot_entry(session_id: str) -> dict:
  """The probed sidebar-state entry for *session_id*.

  Indexes directly — a poll has either just probed the session or found a prior
  entry, so a KeyError here means the poll's probe-or-serve invariant broke and
  must fail loud, not serve a fabricated default.
  """
  return _snapshot[session_id]


def store_snapshot_entry(session_id: str, entry: dict) -> None:
  """Refresh the snapshot entry for *session_id* with fresh probe results."""
  _snapshot[session_id] = entry


def probe_signature(session_id: str) -> tuple | None:
  """The stored probe-input signature for *session_id*, or None when never probed."""
  return _probe_signatures.get(session_id)


def store_probe_signature(session_id: str, signature: tuple) -> None:
  """Store the probe-input signature under which *session_id* was last deep-probed."""
  _probe_signatures[session_id] = signature


def register_poll(force: bool) -> bool:
  """Count one populate_sidebar_state invocation; return True when it must re-probe everything.

  Every ``_FORCE_FULL_EVERY``-th call, and any call with *force* set (the
  ``/status?force=1`` escape hatch), is a full re-probe of all active sessions
  in that call.
  """
  global _poll_count
  _poll_count += 1
  return force or _poll_count % _FORCE_FULL_EVERY == 0


def reset_for_tests() -> None:
  """Clear the dirty set, the snapshot, the probe signatures, the marked paths, and the poll counter (tests only)."""
  global _poll_count
  _dirty.clear()
  _snapshot.clear()
  _probe_signatures.clear()
  _marked_paths.clear()
  _poll_count = 0
