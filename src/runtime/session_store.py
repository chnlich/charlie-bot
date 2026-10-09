"""Session metadata store: reads, writes and the in-memory metadata cache of metadata.json.

``SessionStore`` is the one owner of a session's ``metadata.json``. Every write funnels through
``save_metadata``; reads serve the TTL cache (``get_session``) or the disk (``read_metadata_fresh``).
The per-session metadata locks and the single-field updates live here because every block that
mutates metadata shares them. The process builds one store (``store()``); tests build their own and
install it with ``set_store()``.
"""

import asyncio
import contextlib
import os
import time
from pathlib import Path
from typing import Any

import aiofiles

from src.infra import locks
from src.infra.config import CharlieBotConfig, get_config
from src.infra.json_utils import atomic_write_text
from src.infra.log_once import LazyStructlogLogger
from src.infra.memo import stat_signature
from src.infra.models import SessionMetadata, SessionStatus, utc_now, validate_session_metadata
from src.runtime import sidebar_state
from src.runtime.hooks import backend_types
from src.runtime.run_identity import SESSION_METADATA_NAME as METADATA_NAME
from src.runtime.thinking_state import busy_since, run_backend

log = LazyStructlogLogger()

_METADATA_CACHE_TTL = 30.0  # seconds
# Sweep bound for the listings memo. In-process writes bump the revision and
# surface immediately; an out-of-band metadata edit moves neither, and is
# caught by the entry's TTL revalidation on the first sweep walk after expiry —
# at worst _METADATA_CACHE_TTL plus one interval, against the TTL-plus-one-call
# bound the per-call walk gave it.
_LISTINGS_SWEEP_INTERVAL = 10.0  # seconds
# The resume anchors: the metadata fields that name where the conversation
# lives and who produced it (the cc-id and the backend the id came from; the
# pool login holding the transcript is the backend lifecycle's account label,
# guarded beside them). They change only through their authorized channels
# (see save_metadata's guard).
_ANCHOR_FIELDS = ("cc_session_id", "native_backend")

TRANSIENT_METADATA_FIELDS = {
    "has_running_tasks",
    "work_state",
    "has_pending_trigger",
    "pending_trigger_count",
    "next_trigger_at",
    "has_pending_plan_approval",
    "schedule_cron",
    "schedule_enabled",
    "schedule_next_run",
    "schedule_timezone",
    "schedule_project",
    "schedule_allow_failure",
    "thinking_since",
    "run_backend",
}


def stamp_thinking_since(meta: SessionMetadata) -> SessionMetadata:
  """Overwrite thinking_since with the live value before *meta* reaches a caller.

  thinking_since is a derived runtime fact owned by
  :mod:`src.runtime.thinking_state`; it is never persisted (see
  ``TRANSIENT_METADATA_FIELDS``). Every API- and listing-bound return path
  (``get_session``, the listing entry points routed through
  ``load_session_metas``, the spawn returns)
  applies this stamp on the way out; the succession-internal
  readers (``read_metadata_fresh``, ``resolve_successor_chain``) deliberately
  return the disk value unstamped, and a reader that needs live busy state stamps
  the meta itself. The field stays
  declared on the model, so a stale value parsed from an old metadata.json
  or restored into the cache by a post-save rebuild must not leak out
  through an unstamped API path.
  """
  meta.thinking_since = busy_since(meta.id)
  meta.run_backend = run_backend(meta.id)
  return meta


class SessionStore:
  """Session metadata persistence: metadata.json reads and writes, the cache, the per-session locks."""

  def __init__(self, cfg: CharlieBotConfig) -> None:
    self._cfg = cfg
    # In-memory metadata cache: session_id -> (metadata, monotonic_timestamp, disk signature).
    # The signature is the (st_mtime_ns, st_size) of metadata.json taken BEFORE the read
    # that produced the entry (write-populated entries carry the write's own published
    # signature; None only when the reader could not stat). TTL-based to bound the per-read
    # work within a poll cycle; on expiry the signature revalidates the entry with one stat
    # instead of a re-read — every writer publishes through the atomic tmp rename, so a
    # content change always moves st_mtime_ns, and a same-signature stat proves the parsed
    # bytes current.
    self.metadata_cache: dict[str, tuple[SessionMetadata, float, tuple[int, int] | None]] = {}
    # Per-session asyncio.Lock guarding metadata read-modify-write operations.
    # Prevents clobber races between concurrent mutators (e.g. mark_unread vs
    # update_thinking_state), which both load meta, mutate disjoint fields, and
    # save back — without a lock the second save overwrites the first's change.
    self.metadata_locks: dict[str, asyncio.Lock] = {}
    # Listing-preamble memo: ((mtime_ns, size) of the sessions root, its subdirectory names).
    # The root's own mtime moves exactly when a session entry is created or removed (metadata
    # writes land one level below), so an unchanged signature proves the name set current.
    self._dir_names_memo: tuple[tuple[int, int], list[str]] | None = None
    # Listings-memo revision: every session-metadata write, cache invalidation, or cache
    # eviction bumps it, so a stored listing serves only while no write landed. Out-of-band
    # edits (which bump nothing) are bounded by the sweep walk plus the entries' own TTL
    # revalidation, per the _LISTINGS_SWEEP_INTERVAL note.
    self._listings_revision = 0
    # status -> (revision at walk time, monotonic at store time, root signature, metas).
    # The stored list holds the cache's own meta objects: every consumer copies or
    # stamps on the way out and mutates neither the list nor its rows.
    self._listings_memo: dict[SessionStatus | None, tuple[int, float, tuple[int, int], list[SessionMetadata]]] = {}

  async def get_session(self, session_id: str) -> SessionMetadata | None:
    """Load session metadata, using in-memory cache when available."""
    meta = self._fresh_cached_meta(session_id)
    cache_hit = meta is not None
    sig: tuple[int, int] | None = None
    if meta is None:
      # The signature is taken before the read: a write landing between the two
      # keys the entry under the older signature, which the next expiry stat
      # mismatches — an entry can never be served for bytes it did not parse.
      sig = stat_signature(self.metadata_path(session_id))
      raw = await self._read_metadata_raw(session_id)
      if raw is None:
        return None
      meta = validate_session_metadata(raw, str(self.metadata_path(session_id)))
    # The manual populate covers disk loads only; re-stamping a hit's
    # timestamp would wrongly extend its TTL.
    if not cache_hit:
      self.metadata_cache[session_id] = (meta, time.monotonic(), sig)
    return stamp_thinking_since(meta.model_copy())

  async def get_session_bypassing_cache(self, session_id: str) -> SessionMetadata | None:
    """get_session forced past the TTL cache, so the read lands on disk.

    The single-field mutators (``save_field_fresh``, ``_persist_anchor_fresh``)
    and their post-save read-backs must act on the latest on-disk state, not a
    TTL-cached view: a stale view would clobber a concurrent writer's save.
    Unlike ``read_metadata_fresh`` this stays a ``get_session`` call, so the
    cache is re-populated from the read. Hold ``self.lock_for(session_id)``
    around the whole mutate-save;
    without the lock the fresh view races other writers.
    """
    self.invalidate_cache(session_id)
    return await self.get_session(session_id)

  async def read_metadata_fresh(self, session_id: str) -> SessionMetadata | None:
    """Read metadata.json directly from disk, bypassing ``metadata_cache``.

    The cache is TTL-based and can be stale relative to a concurrent elone, so
    succession resolution always reads fresh. Returns None when the file is
    absent or blank. Does not populate the cache from the read.
    """
    raw = await self._read_metadata_raw(session_id)
    if raw is None:
      return None
    return validate_session_metadata(raw, str(self.metadata_path(session_id)))

  async def _read_metadata_raw(self, session_id: str) -> str | None:
    """Return the raw metadata.json text, or None when the file is missing or blank.

    Single read tail for the one-session-at-a-time metadata readers (cached
    ``get_session`` and bypassing ``read_metadata_fresh``): a blank file must
    warn exactly once per read through the same ``session_metadata_empty``
    event. The batched listing path (``load_session_metas``) cannot route
    through here — it batches all missing metadata reads synchronously in one
    ``asyncio.to_thread`` call — and keeps its own absent/blank handling with
    the same warning event.
    """
    path = self.metadata_path(session_id)
    if not path.exists():
      return None
    async with aiofiles.open(path) as f:
      raw = await f.read()
    if not raw.strip():
      log.warning("session_metadata_empty", session_id=session_id, path=str(path))
      return None
    return raw

  async def save_field_fresh(self, session_id: str, field: str, value: Any) -> None:
    """Set one metadata field on a fresh disk read, under the per-session lock.

    The fresh read inside the save lock is the single-field-mutator contract
    (see ``get_session_bypassing_cache``). No-op when the session does not
    exist.
    """
    async with self.lock_for(session_id):
      fresh = await self.get_session_bypassing_cache(session_id)
      if fresh is None:
        return
      setattr(fresh, field, value)
      await self.save_metadata(fresh, lock_held=True)

  def invalidate_cache(self, session_id: str) -> None:
    """Remove a session from the metadata cache."""
    self.metadata_cache.pop(session_id, None)
    self._listings_revision += 1

  def _fresh_cached_meta(self, session_id: str) -> SessionMetadata | None:
    """Return the cached metadata for *session_id* when the entry is authoritative.

    An archived entry is served regardless of age: archived metadata changes
    only through the in-process write funnel (``save_metadata`` refreshes the
    entry, ``delete_session_permanently`` invalidates it), so a TTL re-read
    buys nothing there and the archived set stays listable without disk scans.
    Active entries keep the ``_METADATA_CACHE_TTL`` freshness window; an expired
    entry revalidates against metadata.json with one stat — a same-signature
    stat proves the parsed bytes unchanged (every writer publishes through the
    atomic tmp rename, so a content change always moves ``st_mtime_ns``) and
    re-times the entry, while a moved or unprovable signature (``None``, a
    stat failure) evicts for the caller's disk read. The stat
    revalidation keeps an active entry serving only while its bytes provably
    stand, not on the clock alone. The two
    TTL-checked metadata readers (``get_session`` and ``load_session_metas``)
    route through this one check, and a stale entry is evicted here, so the
    two cannot drift on freshness semantics.
    """
    cached = self.metadata_cache.get(session_id)
    if cached is None:
      return None
    meta, ts, sig = cached
    if meta.status == SessionStatus.ARCHIVED:
      return meta
    if (time.monotonic() - ts) < _METADATA_CACHE_TTL:
      return meta
    if sig is not None:
      try:
        st = os.stat(self._metadata_path_str(session_id))
        if (st.st_mtime_ns, st.st_size) == sig:
          self.metadata_cache[session_id] = (meta, time.monotonic(), sig)
          return meta
      except OSError:
        pass
    del self.metadata_cache[session_id]
    # The entry's file moved or became unprovable without a write funnel bump
    # (an out-of-band edit, or a stat failure on an expired entry): raise the
    # listings revision so the next listing re-reads instead of serving the
    # memoized rows this eviction just proved stale.
    self._listings_revision += 1
    return None

  def fresh_cached_metas(self) -> dict[str, SessionMetadata]:
    """The authoritative cached metadata entries, keyed by session id.

    One :meth:`_fresh_cached_meta` check per entry — the shared check
    ``get_session`` and ``load_session_metas`` route through — so a caller
    that snapshots this map reads exactly the metadata every other reader
    serves: archived entries regardless of age, active entries while their
    stat signature proves the parsed bytes stand. Entries past revalidation
    re-time or evict here, the same side effects a per-id check has; ids with
    no authoritative entry are absent and the caller reads their files.
    """
    resolved: dict[str, SessionMetadata] = {}
    for session_id in list(self.metadata_cache):
      meta = self._fresh_cached_meta(session_id)
      if meta is not None:
        resolved[session_id] = meta
    return resolved

  async def load_session_metas(self, status: SessionStatus | None = None) -> list[SessionMetadata]:
    """Load session metadata, batching disk reads and parses for cache misses.

    Performs the listing preamble for the entry points routed through here
    (``list_sessions``, ``list_group_names``, ``search_sessions``, and
    ``list_archived_page``):
    (1) return [] if sessions_dir does not exist, (2) list session directories
    under asyncio.to_thread to avoid blocking the event loop, (3) use fresh
    cache entries directly and read+parse all missing metadata files serially
    in one asyncio.to_thread call — the parse stays off the event loop —
    logging and dropping any session that fails to load. Returns the cached
    objects themselves, filtered to *status* when given: callers that hand
    metadata out of the manager copy and stamp on the way out.

    The per-filter result memoizes on (``_listings_revision``, the sessions
    root's signature, a ``_LISTINGS_SWEEP_INTERVAL`` clock): an in-process
    write bumps the revision (``save_metadata`` is the single funnel) and a
    create/delete moves the root signature, so both re-walk on the next call;
    the sweep re-walks at least every interval, which is what bounds an
    out-of-band metadata edit to the entry's ``_METADATA_CACHE_TTL`` expiry
    plus one interval. A hit serves the cached meta objects read-only; the
    walk itself never mutates the stored list.
    """
    if not self._cfg.sessions_dir.exists():
      return []

    def _session_dir_names() -> tuple[tuple[int, int], list[str]]:
      # DirEntry.is_dir() answers from the directory record itself on
      # d_type-aware filesystems, while Path.iterdir() rebuilds a Path per
      # entry and pays one stat() each: ~1 ms vs ~6 ms measured at ~1000
      # session dirs, per listing call. The signature is taken BEFORE the
      # scan: a create/delete racing it moves the root's mtime after the
      # stamp, so the memo keys a possibly-stale name list under a stale
      # signature and the next call rescans; a hit pays one stat (~5 us)
      # and skips the scandir (~1 ms at ~1000 dirs) on every listing call.
      root = os.stat(self._cfg.sessions_dir)
      sig = (root.st_mtime_ns, root.st_size)
      if self._dir_names_memo is not None and self._dir_names_memo[0] == sig:
        return sig, self._dir_names_memo[1]
      with os.scandir(self._cfg.sessions_dir) as entries:
        names = [entry.name for entry in entries if entry.is_dir()]
      self._dir_names_memo = (sig, names)
      return sig, names

    # The hit check reads the revision directly: no await sits between the
    # read and the comparison, so a bump cannot land inside the decision. The
    # store tags the revision read after the miss decision, before the walk —
    # a write landing mid-walk bumps past the tag and the next call re-walks
    # (a mark landing mid-walk only raises the revision).
    hit = self._listings_memo.get(status)
    if (hit is not None and hit[0] == self._listings_revision and time.monotonic() - hit[1] < _LISTINGS_SWEEP_INTERVAL):
      try:
        root = os.stat(self._cfg.sessions_dir)
      except OSError:
        root = None
      if root is not None and (root.st_mtime_ns, root.st_size) == hit[2]:
        return hit[3]

    revision = self._listings_revision
    root_sig, dir_names = await asyncio.to_thread(_session_dir_names)

    cached_metas: dict[str, SessionMetadata] = {}
    missing_ids: list[str] = []
    for session_id in dir_names:
      meta = self._fresh_cached_meta(session_id)
      if meta is None:
        missing_ids.append(session_id)
      else:
        cached_metas[session_id] = meta

    parsed_by_id: dict[str, SessionMetadata] = {}
    parsed_sigs: dict[str, tuple[int, int] | None] = {}
    empty_ids: set[str] = set()
    load_failures: dict[str, Exception] = {}
    if missing_ids:

      def _read_and_parse_missing() -> None:
        for session_id in missing_ids:
          path = self.metadata_path(session_id)
          # Signature before the read, per file: an entry keys only the bytes
          # it parsed (see get_session's same rule), so a write landing between
          # the two is re-read at the next expiry stat instead of served stale.
          sig = stat_signature(path)
          try:
            if not path.exists():
              continue
            raw = path.read_text(encoding="utf-8")
          except Exception as exc:
            load_failures[session_id] = exc
            continue
          if not raw.strip():
            empty_ids.add(session_id)
            continue
          try:
            parsed_by_id[session_id] = validate_session_metadata(raw, str(self.metadata_path(session_id)))
            parsed_sigs[session_id] = sig
          except Exception as exc:
            load_failures[session_id] = exc

      await asyncio.to_thread(_read_and_parse_missing)

    result: list[SessionMetadata] = []
    for session_id in dir_names:
      loaded_from_cache = session_id in cached_metas
      if loaded_from_cache:
        meta = cached_metas[session_id]
      else:
        if session_id in load_failures:
          log.warning("session_load_failed", session_id=session_id, error=str(load_failures[session_id]))
          continue
        if session_id in empty_ids:
          log.warning("session_metadata_empty", session_id=session_id, path=str(self.metadata_path(session_id)))
          continue
        if session_id not in parsed_by_id:
          continue
        meta = parsed_by_id[session_id]

      if not loaded_from_cache:
        self.metadata_cache.setdefault(session_id, (meta, time.monotonic(), parsed_sigs.get(session_id)))
      if status is None or meta.status == status:
        result.append(meta)
    self._listings_memo[status] = (revision, time.monotonic(), root_sig, result)
    return result

  def lock_for(self, session_id: str) -> asyncio.Lock:
    """Return (creating on first use) the per-session metadata RMW lock."""
    return locks.lock_for(self.metadata_locks, session_id)

  async def update_field(
      self, session_id: str, field: str, value: Any, log_event: str, **log_fields: Any) -> SessionMetadata | None:
    """Get a session, set one field, save, and log. Returns None if session not found."""
    async with self.lock_for(session_id):
      meta = await self.get_session(session_id)
      if not meta:
        return None
      setattr(meta, field, value)
      meta.updated_at = utc_now()
      await self.save_metadata(meta, lock_held=True)
    log.info(log_event, session_id=session_id, **log_fields)
    return meta

  async def get_sessions_readonly(self, session_ids: list[str]) -> list[SessionMetadata]:
    """Resolve *session_ids* to metadata for consumers that only read it.

    Warm entries serve the cached objects themselves — the caller must not
    mutate them, the way :meth:`get_session`'s per-row copy would let it — and
    only a cache miss pays a ``get_session`` read, which returns its own copy.
    Ids that no longer resolve are dropped, and request order is preserved.
    """
    resolved: dict[str, SessionMetadata | None] = {}
    for session_id in dict.fromkeys(session_ids):
      resolved[session_id] = self._fresh_cached_meta(session_id)
    for session_id, meta in resolved.items():
      if meta is None:
        resolved[session_id] = await self.get_session(session_id)
    return [meta for meta in resolved.values() if meta is not None]

  async def _reconcile_anchor_fields(self, meta: SessionMetadata) -> None:
    """Correct *meta*'s anchor fields back to the on-disk values before a whole-object save.

    The guard behind the authorized-channel model: the resume anchors change
    only through their channels (``persist_cc_session_id``, ``persist_account_label``,
    ``persist_native_backend``, ``persist_native_anchor_provenance``,
    ``clear_cc_session_anchor``), so a whole-object save built from a stale cached
    meta must not roll them back. Reads metadata.json fresh (a cached view is the
    very staleness this guard exists for) and mutates *meta* in place; a
    correction logs ``session_anchor_write_corrected`` and the save proceeds
    write-through rather than refusing. Runs under the per-session lock: inside
    the caller's critical section for the lock-holding save sites, under the
    acquisition save_metadata performs for everyone else.
    """
    disk = await self.read_metadata_fresh(meta.id)
    if disk is None:
      return
    for field in _ANCHOR_FIELDS:
      on_disk = getattr(disk, field)
      if getattr(meta, field) != on_disk:
        log.warning(
            "session_anchor_write_corrected",
            session_id=meta.id,
            field=field,
            on_disk=on_disk,
            attempted=getattr(meta, field),
        )
        setattr(meta, field, on_disk)
    for lifecycle in backend_types.lifecycles():
      label_on_disk = lifecycle.account_label(disk)
      if lifecycle.account_label(meta) != label_on_disk:
        log.warning(
            "session_anchor_write_corrected",
            session_id=meta.id,
            field=lifecycle.account_source,
            on_disk=label_on_disk,
            attempted=lifecycle.account_label(meta),
        )
        lifecycle.record_account_label(meta, label_on_disk)

  async def save_metadata(
      self,
      meta: SessionMetadata,
      *,
      lock_held: bool = False,
      anchor_write: bool = False,
  ) -> None:
    """Persist *meta* to metadata.json and refresh the TTL cache from the serialized form.

    The write is atomic — a unique-per-call tmp file swapped in by ``os.replace``
    — and excludes ``TRANSIENT_METADATA_FIELDS``. The cache entry stores the meta
    re-validated from that serialized form, so a cached read sees exactly what a
    disk read parses; that is what lets save-callers skip a manual cache
    populate. ``updated_at`` is written as given:
    bumping or preserving it is the caller's decision (``update_field`` bumps,
    ``_set_unread_flag`` does not).

    The anchor fields are reconciled against disk on every save that may carry
    them (``_reconcile_anchor_fields``), always under the per-session lock:

    * ``lock_held`` — the caller already holds the per-session lock (the in-class
      read-modify-write sites). The lock is never reentrant; the reconciliation
      then runs inside the caller's own critical section. Lock-free callers get
      the acquisition here.
    * ``anchor_write`` — this save is an authorized anchor channel
      (``persist_cc_session_id``, ``persist_account_label``,
      ``persist_native_backend``, ``persist_native_anchor_provenance``,
      ``clear_cc_session_anchor``) and legitimately changes an anchor field; the
      reconciliation is skipped, because its whole purpose would revert the
      intended write. Every other caller is reconciled: a stale anchor is
      corrected back to the disk value and the correction is logged.
    """
    async with self.lock_for(meta.id) if not lock_held else contextlib.nullcontext():
      if not anchor_write:
        await self._reconcile_anchor_fields(meta)
      path = self.metadata_path(meta.id)
      path.parent.mkdir(parents=True, exist_ok=True)
      serialized = meta.model_dump_json(indent=2, exclude=TRANSIENT_METADATA_FIELDS)

      sig = await asyncio.to_thread(atomic_write_text, path, serialized)
      # The entry re-keys from the write's own proven signature: the swap
      # publishes the tmp inode the writer just statted, so a later
      # same-signature stat proves the file still carries the funnel's bytes
      # and the expiry revalidates by one stat instead of evicting into a full
      # re-read (whose revision bump also forced the next listing to re-walk).
      # A concurrent publish after this one replaces the inode; the next
      # expiry's stat then evicts and re-reads — the same bound the
      # signature-less entry paid on every expiry.
      self.metadata_cache[meta.id] = (
          validate_session_metadata(serialized, str(self.metadata_path(meta.id))), time.monotonic(), sig)
      # The single funnel for every session-metadata write (35+ call sites, plus
      # the save funnel): status transitions (archive/unarchive)
      # land here, so the sidebar snapshot must re-probe this session.
      sidebar_state.mark_sidebar_dirty(meta.id)
      self._listings_revision += 1

  def session_dir(self, session_id: str) -> Path:
    return self._cfg.sessions_dir / session_id

  def metadata_path(self, session_id: str) -> Path:
    return self.session_dir(session_id) / METADATA_NAME

  def _metadata_path_str(self, session_id: str) -> str:
    """The string form of ``metadata_path`` — same layout, no pathlib construction.

    The expiry revalidation stats every expired entry inline on the event loop
    (one stat per entry per listing past ``_METADATA_CACHE_TTL``), and pathlib's
    two constructions plus the stat call's own path stringification price ~4.7 us
    per entry on top of the ~2.5 us syscall on this host (measured against a
    300-entry aged walk, Python 3.14). The layout stays owned by
    :meth:`metadata_path`; this form only skips the Path objects.
    """
    return f"{self._cfg.sessions_dir}/{session_id}/{METADATA_NAME}"


# The process owner of the metadata store; built on the first ``store()`` call.
_store: SessionStore | None = None


def store() -> SessionStore:
  """The process-wide session metadata store."""
  global _store
  if _store is None:
    _store = SessionStore(get_config())
  return _store


def set_store(replacement: SessionStore | None) -> None:
  """Replace the process store singleton (tests); None restores lazy construction."""
  global _store
  _store = replacement
