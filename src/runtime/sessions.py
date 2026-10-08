"""Session blocks of CharlieBot, held together until their callers take them one by one."""

from src.infra.config import CharlieBotConfig, get_config
from src.infra.models import SessionCallbacks
from src.runtime import (
    session_anchors,
    session_events,
    session_fork,
    session_lifecycle,
    session_listing,
    session_search,
    session_sidebar,
    session_store,
    session_successor,
)


class SessionManager:
  """The process's session blocks as attributes, and the run callback bundle built from them."""

  def __init__(
      self,
      cfg: CharlieBotConfig,
      store: session_store.SessionStore,
      events: session_events.SessionEvents,
      sidebar: session_sidebar.SessionSidebar,
      listing: session_listing.SessionListing,
      search: session_search.SessionSearch,
      lifecycle: session_lifecycle.SessionLifecycle,
      fork: session_fork.SessionFork,
      anchors: session_anchors.SessionAnchors,
      successor: session_successor.SessionSuccessor,
  ) -> None:
    self._cfg = cfg
    self.task_tree_manager = None
    self.store = store
    self.events = events
    self.sidebar = sidebar
    self.listing = listing
    self.search = search
    self.lifecycle = lifecycle
    self.fork = fork
    self.anchors = anchors
    self.successor = successor
    events.turn_sessions = self

  def callbacks(self) -> SessionCallbacks:
    """Return a bundle of session-related callbacks for run_message()."""
    return SessionCallbacks(
        persist_and_broadcast=self.events.persist_and_broadcast,
        mark_unread=self.lifecycle.mark_unread,
        persist_cc_session_id=self.anchors.persist_cc_session_id,
        persist_account_label=self.anchors.persist_account_label,
        context_state=self.anchors.context_state,
        task_tree_activity=self.sidebar.task_tree_activity,
    )


# The process owner of the session manager; built on the first ``session_manager()`` call.
_session_manager: SessionManager | None = None


def session_manager() -> SessionManager:
  global _session_manager
  if _session_manager is None:
    _session_manager = SessionManager(
        get_config(),
        session_store.store(),
        session_events.events(),
        session_sidebar.sidebar(),
        session_listing.listing(),
        session_search.search(),
        session_lifecycle.lifecycle(),
        session_fork.fork(),
        session_anchors.anchors(),
        session_successor.successor(),
    )
  return _session_manager
