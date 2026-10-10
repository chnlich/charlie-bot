"""The chat-threads package's turn contribution: a thread session gets the thread brief and a narrower context window."""

from src.features.chat_threads import thread_sessions
from src.infra import models
from src.runtime.hooks import turn_contributions


class ChatThreadsTurnContribution(turn_contributions.TurnContribution):
  """Names ``thread_session.md`` as the workflow rules file and sets the thread context window."""

  def workflow_rules_file(self, meta: models.SessionMetadata) -> str | None:
    return "thread_session.md" if thread_sessions.is_thread_session(meta) else None

  def context_window(self, meta: models.SessionMetadata) -> int | None:
    return thread_sessions.THREAD_CONTEXT_WINDOW if thread_sessions.is_thread_session(meta) else None


CONTRIBUTION = ChatThreadsTurnContribution()
