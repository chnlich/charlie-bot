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

from src.infra.memo import BoundedMemo

# The sidebar dict keys, single-homed here: the probe snapshot and the
# per-request derived entry both build and read their dicts by these names,
# and /api/sessions relays the derived entry to web/static/js/sidebar/ verbatim.
PENDING_TRIGGER_COUNT = "pending_trigger_count"
NEXT_TRIGGER_AT = "next_trigger_at"
HAS_PENDING_PLAN_APPROVAL = "has_pending_plan_approval"
HAS_RUNNING_TASKS = "has_running_tasks"
HAS_PENDING_TRIGGER = "has_pending_trigger"
# A task-tree node's derived activity, carried by the probed snapshot and the
# derived entry alike. The value is the shared derivation's pair:
# (has_running_tasks, work_state) — see TaskTreeActivity in src.runtime.task_sessions.
TASK_TREE_ACTIVITY = "task_tree_activity"
# The derived entry's work_state key for rows with a task activity snapshot.
WORK_STATE = "work_state"

# Every Nth populate_sidebar_state call re-probes all active sessions: the
# bounded self-heal window for a state-transition write path that forgets to
# mark its session dirty (active polls ~3s -> stale field heals within ~30s).
_FORCE_FULL_EVERY = 10

# Session ids whose probed state changed since the last re-probe.
_dirty: set[str] = set()
# session id -> the probe snapshot, keyed by the constants above:
# PENDING_TRIGGER_COUNT int, NEXT_TRIGGER_AT datetime | None,
# HAS_PENDING_PLAN_APPROVAL bool.
# A session without an entry is cold for that poll (probed like a dirty one),
# so an empty dict — a fresh boot — is a full probe.
_snapshot: dict[str, dict] = {}
# session id -> probe-input signature (stat-only identity of every file the
# sidebar probe reads). A selected re-probe whose signature matches the stored
# one — and whose 30-day scan-window rollover has not passed — re-derives from
# unchanged bytes, so the deep probe is skipped (the every-10th-poll self-heal
# sweep drops to a stat-only pass). Built and stored by the poll in
# the session_sidebar block; the /status?force=1 escape hatch bypasses it.
_probe_signatures: dict[str, tuple] = {}
# populate_sidebar_state invocation counter (process lifetime).
_poll_count = 0

# Process-global generation of every input the derived sidebar fold reads:
# the probe snapshot, the busy map (thinking_state marks through here), and
# the sessions' metadata fields the fold partitions on (every metadata write
# publishes through save_metadata's mark). Both writer funnels —
# :func:`mark_sidebar_dirty` and :func:`store_snapshot_entry` — bump it, so
# an unchanged generation is the whole staleness contract for the derived-map
# memo below. Loop-side only, the same convention the dirty set follows.
_derived_generation = 0

# (ids, flags, generation) -> the derived map the fold built at that
# generation. Values are served uncopied to every poll between bumps, so a
# caller may read them but never mutate them — the contract every
# resolve_sidebar_state caller already holds.
_DERIVED_MAP_MEMO_LIMIT = 4
_derived_maps: BoundedMemo[tuple, dict] = BoundedMemo(_DERIVED_MAP_MEMO_LIMIT)


def mark_sidebar_dirty(session_id: str) -> None:
  """Flag *session_id*'s probed sidebar state for re-probe on the next poll."""
  _dirty.add(session_id)
  global _derived_generation
  _derived_generation += 1


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
  global _derived_generation
  _derived_generation += 1
  _snapshot[session_id] = entry


def derived_generation() -> int:
  """Current generation of the probed-state inputs the derived fold reads."""
  return _derived_generation


def peek_derived_map(key: tuple) -> dict | None:
  """The derived map built at *key*'s generation, or None; served value is read-only."""
  return _derived_maps.peek(key)


def store_derived_map(key: tuple, derived: dict) -> None:
  """Store one derived map under its (ids, flags, generation) key."""
  _derived_maps.store(key, derived)


def snapshot_task_activity(session_id: str) -> tuple[bool, str] | None:
  """The stored task-tree activity for *session_id*, or None before its probe.

  ``(has_running_tasks, work_state)`` — the pair the deep probe derived. A
  node whose stored verdict is ``running`` holds a launched Run without a
  terminal fact, so its liveness must be re-checked on the self-heal sweep
  even when no file the probe signature covers has moved (a process death
  writes nothing).
  """
  entry = _snapshot.get(session_id)
  if entry is None:
    return None
  return entry.get(TASK_TREE_ACTIVITY)


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
  """Clear the dirty set, the snapshot, the probe signatures, and the poll counter (tests only)."""
  global _poll_count
  _dirty.clear()
  _snapshot.clear()
  _probe_signatures.clear()
  _poll_count = 0
  global _derived_generation
  _derived_generation = 0
  _derived_maps.clear()
