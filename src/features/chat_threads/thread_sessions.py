"""The thread-session predicate and the context window a thread session runs on the CLC backend.

A thread session answers one Slack or Discord thread: its metadata carries that
platform's origin field, set at summon creation and never mutated. The
instruction build (which rule file follows prompts/master.md) and the CLC
context-window override both classify the session through
:func:`is_thread_session`, so the test has one definition; the sidebar's
chat-thread subtree rule (src/runtime/scheduled_sessions.py) shares it.
"""

from src.infra.models import SessionMetadata

# The context window a thread session passes to the charlie-code backend in
# place of the backend option's own value. The option entry is shared with main
# sessions and workers, so shrinking it there would cap those too; the value
# lives here because only the thread path reads it. CLC resets a conversation
# at 65% of the window, so 96,000 tokens resets at 62,400.
THREAD_CONTEXT_WINDOW = 96_000


def is_thread_session(meta: SessionMetadata) -> bool:
  """True when *meta* carries a Slack or Discord thread origin."""
  return meta.slack_origin is not None or meta.discord_origin is not None
