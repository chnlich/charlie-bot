"""The chat_threads package's sidebar contribution: a Slack or Discord thread session roots the Threads view."""

from src.features.chat_threads import thread_sessions
from src.infra import models
from src.runtime.hooks import sidebar_contributions

THREADS_VIEW = "threads"


class ChatThreadsSidebar(sidebar_contributions.SidebarContribution):

  def view_member(self, meta: models.SessionMetadata) -> str | None:
    return THREADS_VIEW if thread_sessions.is_thread_session(meta) else None


contribution = ChatThreadsSidebar()
