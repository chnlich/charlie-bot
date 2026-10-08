"""The cc-claude backend's reads and writes of its metadata keys, typed.

The keys are registered in ``src/backends/claude_code/__init__.py`` under the owner name ``OWNER``;
this module is the one place the backend calls ``src/infra/metadata_slots.py`` for them.
"""

from src.backends.claude_code import OWNER, ClaudeSessionFields, ClaudeThreadFields
from src.infra import metadata_slots
from src.infra.models import SessionMetadata, ThreadMetadata


def account_of(meta: SessionMetadata) -> str | None:
  """The pool account label that ``meta`` records."""
  fields = metadata_slots.fields_of(meta, OWNER)
  assert isinstance(fields, ClaudeSessionFields)
  return fields.claude_account


def set_account(meta: SessionMetadata, label: str | None) -> None:
  """Record the pool account label on ``meta`` in memory."""
  metadata_slots.set_fields(meta, OWNER, claude_account=label)


def session_id_of(thread: ThreadMetadata) -> str | None:
  """The Claude Code session id that ``thread`` records."""
  fields = metadata_slots.fields_of(thread, OWNER)
  assert isinstance(fields, ClaudeThreadFields)
  return fields.claude_session_id


def set_session_id(thread: ThreadMetadata, session_id: str | None) -> None:
  """Record the Claude Code session id on ``thread`` in memory."""
  metadata_slots.set_fields(thread, OWNER, claude_session_id=session_id)
