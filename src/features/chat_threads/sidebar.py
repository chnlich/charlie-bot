"""The chat_threads package's sidebar contribution: a Slack or Discord thread session roots the Threads view."""

from src.features.chat_threads.thread_sessions import is_thread_session
from src.infra.models import SessionMetadata
from src.runtime.hooks.sidebar_contributions import SidebarContribution

THREADS_VIEW = "threads"


class ChatThreadsSidebar(SidebarContribution):

  def view_member(self, meta: SessionMetadata) -> str | None:
    return THREADS_VIEW if is_thread_session(meta) else None


contribution = ChatThreadsSidebar()
