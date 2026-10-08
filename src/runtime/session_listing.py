"""Session lists: the status, starred and scheduled listings, the archived page, group names and the subtree maps.

``SessionListing`` reads the store's shared cached metadata and derives each list's sidebar state through the sidebar
block. The task-tree owner registers its fact-derived archive as ``archive_overlay``; every list applies it after the
store's stored-status memo. The process builds one block (``listing()``); tests build their own and install it with
``set_listing()``.
"""

from collections.abc import Awaitable, Callable
from datetime import datetime
from typing import Any

from src.infra.config import CharlieBotConfig, get_config
from src.infra.models import SessionMetadata, SessionStatus
from src.runtime import session_sidebar, session_store
from src.runtime.scheduled_sessions import sequence_subtree_roots, view_subtree_roots
from src.runtime.thinking_state import busy_since, run_backend


def _listing_row_copy(meta: SessionMetadata, update: dict[str, Any]) -> SessionMetadata:
  """Return *meta* as a caller-safe listing row carrying *update*, as ``model_copy`` would.

  SessionMetadata keeps its unregistered and package-registered keys as
  extras, and has no private attrs and no computed fields — so a row's
  ancillary state is the extras dict plus ``__pydantic_fields_set__`` and the
  copy reduces to the field dict plus those two. The listing row test asserts
  that config fact directly and pins the result dump- and field-set-equal to
  ``model_copy(update=...)``; a config change that adds private state must
  extend the copy with that state in the same change.
  """
  row = SessionMetadata.__new__(SessionMetadata)
  object.__setattr__(row, "__dict__", {**meta.__dict__, **update})
  object.__setattr__(row, "__pydantic_extra__", dict(meta.model_extra or {}))
  object.__setattr__(row, "__pydantic_fields_set__", meta.__pydantic_fields_set__ | update.keys())
  object.__setattr__(row, "__pydantic_private__", None)
  return row


class SessionListing:
  """Session lists over the session metadata store and the sidebar block."""

  def __init__(
      self,
      cfg: CharlieBotConfig,
      store: session_store.SessionStore,
      sidebar: session_sidebar.SessionSidebar,
  ) -> None:
    self._cfg = cfg
    self._store = store
    self._sidebar = sidebar
    # The task tree derives a node's archive state from facts (a delivered
    # worker archives once its parent's receipt is on disk) and never writes
    # it into status; the sidebar lists read status. The task-tree owner
    # registers this read-time overlay at wiring time: the ids it returns list
    # as archived. None before that wiring exists (stored status only).
    self.archive_overlay: Callable[[], Awaitable[set[str]]] | None = None
    # The derived sequence-subtree map, memoized on the metas list identity the
    # listings memo already bounds: a listings hit serves the same list object
    # until a write bumps the revision, a create/delete moves the root
    # signature, or the sweep re-walks, so the map re-derives exactly when its
    # inputs can have moved and never wider.
    self._sequence_subtree_memo: tuple[list[SessionMetadata], dict[str, str]] | None = None
    self._view_subtree_memo: tuple[list[SessionMetadata], dict[str, dict[str, str]]] | None = None

  async def list_sessions(
      self,
      status: SessionStatus | None = None,
      starred: bool | None = None,
      scheduled: bool | None = None,
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
      include_pending_plan_approval: bool = False,
  ) -> list[SessionMetadata]:
    """List sessions, newest first. Optionally filter by status, starred, and/or scheduled.

    The starred/scheduled filters run against the shared cached metas (read-only),
    so only surviving rows pay the model_copy that marks a meta as having left
    the manager. The thinking stamp and resolve_sidebar_state's derived fields
    ride that one copy's ``update`` dict: pydantic writes update keys straight
    into ``__dict__``, so the row build stays one copy instead of a copy plus a
    per-field setattr chain (the M61 listing's dominant term at corpus scale).
    """
    rows, derived = await self.list_sessions_readonly(
        status,
        starred,
        scheduled,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
        include_pending_plan_approval=include_pending_plan_approval,
    )
    return [
        _listing_row_copy(
            meta, {
                "thinking_since": busy_since(meta.id),
                "run_backend": run_backend(meta.id),
                **derived[meta.id],
            }) for meta in rows
    ]

  async def list_sessions_readonly(
      self,
      status: SessionStatus | None = None,
      starred: bool | None = None,
      scheduled: bool | None = None,
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
      include_pending_plan_approval: bool = False,
  ) -> tuple[list[SessionMetadata], dict[str, dict]]:
    """List sessions newest-first without copying: ``(rows, derived)`` for
    consumers that only read.

    Rows are the shared cached metadata references — the caller must not mutate
    them (an unfiltered listing's derived-archive rows are copies). ``derived``
    maps each row's id to the sidebar-state fields the copy path
    (:meth:`list_sessions`) stamps onto its copies; ``thinking_since`` is not
    among them (the caller reads :func:`busy_since` itself).
    """
    metas = await self._with_derived_archive(await self._store.load_session_metas(status), status)
    if scheduled is None:
      controllers = ()
    else:
      from src.runtime.hooks.sequence_controllers import sequence_controllers

      controllers = sequence_controllers()
    rows = [
        meta for meta in metas if (starred is None or meta.starred == starred) and
        (scheduled is None or any(controller.owns_session(meta) for controller in controllers) == scheduled)
    ]
    rows.sort(key=lambda meta: meta.updated_at, reverse=True)
    derived = await self._sidebar.resolve_sidebar_state(
        rows,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
        include_pending_plan_approval=include_pending_plan_approval,
    )
    return rows, derived

  async def list_group_names(self) -> list[str]:
    """Return sorted distinct group names across all sessions.

    The name set is a read-only reduction of the shared cached metas, so no row
    copies or thinking stamps leave the manager, unlike ``list_sessions``.
    """
    return sorted({meta.group for meta in await self._store.load_session_metas() if meta.group})

  async def sequence_subtree_roots(self) -> dict[str, str]:
    """The sequence-subtree membership map over every session's stored metadata."""
    metas = await self._store.load_session_metas()
    cached = self._sequence_subtree_memo
    if cached is not None and cached[0] is metas:
      return cached[1]
    roots = sequence_subtree_roots(metas)
    self._sequence_subtree_memo = (metas, roots)
    return roots

  async def view_subtree_roots(self) -> dict[str, dict[str, str]]:
    """Each sidebar view's subtree membership map over every session's stored metadata.

    See :func:`src.runtime.scheduled_sessions.view_subtree_roots` for the
    rule. The map reads the shared cached metas — chain and view-root
    marks only, statuses never matter — so the sidebar lists classify their
    rows with no second read and no copy, memoized on unchanged metas exactly
    like :meth:`sequence_subtree_roots` beside which it lives.
    """
    metas = await self._store.load_session_metas()
    cached = self._view_subtree_memo
    if cached is not None and cached[0] is metas:
      return cached[1]
    roots = view_subtree_roots(metas)
    self._view_subtree_memo = (metas, roots)
    return roots

  async def list_archived_page(
      self,
      *,
      group: str | None = None,
      limit: int = 100,
      before: str | None = None,
      before_id: str | None = None,
  ) -> dict:
    """One page of archived sessions, newest first, plus group aggregates.

    ``group`` picks membership: None = every archived session, "" = the
    ungrouped ones, a name = that group. ``limit`` clamps to 1..500. The
    keyset cursor ``(before, before_id)`` names the previous page's last row;
    rows strictly after it in ``(updated_at, id)``-descending order form the
    next page, so a row archived or deleted between fetches never shifts the
    page boundary. ``groups`` aggregates the whole archived set (not the
    current filter) for the filter strip: named groups alphabetically, the
    ungrouped bucket (group=None) last. Ordering and aggregation are computed
    per request from the cache — archived entries never expire there
    (``SessionStore._fresh_cached_meta``), so the warm request path reads no metadata
    files. A cursor that fails to parse raises ValueError: the caller's
    explicit cursor stops with the error instead of silently serving page 1.
    Sequence-subtree rows are excluded before aggregation and pagination; the
    owned sessions themselves keep their rows.
    """
    limit = max(1, min(500, limit))
    metas = await self._with_derived_archive(
        await self._store.load_session_metas(status=SessionStatus.ARCHIVED), SessionStatus.ARCHIVED)
    # Sequence-subtree rows stay out of the Archived list; the owned sessions
    # themselves keep their rows. The exclusion runs before the group
    # aggregates and the keyset slice, so both describe the rows the page can
    # actually return.
    sequence_subtree = await self.sequence_subtree_roots()
    metas = [meta for meta in metas if meta.id not in sequence_subtree]

    counts: dict[str | None, int] = {}
    for meta in metas:
      key = meta.group or None
      counts[key] = counts.get(key, 0) + 1
    groups = [{"group": name, "total": counts[name]} for name in sorted(k for k in counts if k is not None)]
    if None in counts:
      groups.append({"group": None, "total": counts[None]})

    if group is None:
      rows = list(metas)
    elif group == "":
      rows = [meta for meta in metas if not meta.group]
    else:
      rows = [meta for meta in metas if meta.group == group]
    rows.sort(key=lambda meta: (meta.updated_at, meta.id), reverse=True)

    if (before is None) != (before_id is None):
      raise ValueError("before and before_id form one cursor: pass both or neither")
    if before is not None:
      cursor_time = datetime.fromisoformat(before)
      if cursor_time.tzinfo is None:
        raise ValueError("before must be a timezone-aware ISO timestamp")
      cursor = (cursor_time, before_id)
      rows = [meta for meta in rows if (meta.updated_at, meta.id) < cursor]

    page = rows[:limit]
    has_more = len(rows) > limit
    sessions = [session_store.stamp_thinking_since(meta.model_copy()) for meta in page]
    await self._sidebar.populate_sidebar_state(
        sessions,
        include_running_status=True,
        include_pending_trigger_status=True,
    )
    return {
        "sessions": sessions,
        "has_more": has_more,
        "next_before": page[-1].updated_at.isoformat() if page and has_more else None,
        "next_before_id": page[-1].id if page and has_more else None,
        "groups": groups,
    }

  async def archived_context_rows(self, rows: list[SessionMetadata]) -> list[SessionMetadata]:
    """Every unarchived ancestor of *rows* that *rows* do not already carry.

    The archived listing's context rows (plan 4.2): a page's rows nest into the
    client's project-grouped tree, so a row whose parent is still active needs
    that parent delivered or the child flattens to a root. The walk follows
    ``task_parent_id`` from every row through the metadata owner's own reads
    and collects each ancestor the effective archive still lists as active —
    a stored-archived ancestor or a derived-archived one (the tree's archive
    overlay) is itself an archived listing row and never becomes context, but
    the walk continues past it so the page carries the active ancestors above
    it too. Ancestors already among *rows* are skipped (their own ancestors
    are walked when they are processed); a missing ancestor ends that walk
    rather than guessing past the gap. The caller serves the result beside its
    page rows: the rows never enter the page's size, cursor, or aggregates.
    """
    present = {row.id for row in rows}
    overlay = self.archive_overlay
    derived = await overlay() if overlay is not None else set()
    out: list[SessionMetadata] = []
    seen: set[str] = set()
    for row in rows:
      parent_id = row.task_parent_id
      while parent_id is not None and parent_id not in present and parent_id not in seen:
        seen.add(parent_id)
        ancestor = await self._store.get_session(parent_id)
        if ancestor is None:
          break
        if ancestor.status != SessionStatus.ARCHIVED and ancestor.id not in derived:
          out.append(ancestor)
        parent_id = ancestor.task_parent_id
    return out

  async def _with_derived_archive(self, metas: list[SessionMetadata],
                                  status: SessionStatus | None) -> list[SessionMetadata]:
    """Apply the task tree's fact-derived archive to one stored-status listing.

    The listing memo keys stored status; the derived set moves with facts, so
    the overlay applies after the memo, per call. An active listing drops the
    derived ids, an archived listing gains them (their rows copied with status
    archived, the stored objects untouched), and an unfiltered listing shows
    them as archived. Without a registered overlay the listing stands as is.
    """
    if self.archive_overlay is None:
      return metas
    derived = await self.archive_overlay()
    if not derived:
      return metas

    def as_archived(meta: SessionMetadata) -> SessionMetadata:
      return meta.model_copy(update={"status": SessionStatus.ARCHIVED})

    if status == SessionStatus.ACTIVE:
      return [meta for meta in metas if meta.id not in derived]
    if status == SessionStatus.ARCHIVED:
      active = await self._store.load_session_metas(SessionStatus.ACTIVE)
      return metas + [as_archived(meta) for meta in active if meta.id in derived]
    return [
        as_archived(meta) if meta.id in derived and meta.status != SessionStatus.ARCHIVED else meta for meta in metas
    ]


# The process owner of the listing block; built on the first ``listing()`` call.
_listing: SessionListing | None = None


def listing() -> SessionListing:
  """The process-wide session listing block."""
  global _listing
  if _listing is None:
    _listing = SessionListing(get_config(), session_store.store(), session_sidebar.sidebar())
  return _listing


def set_listing(replacement: SessionListing | None) -> None:
  """Replace the process listing singleton (tests); None restores lazy construction."""
  global _listing
  _listing = replacement
