"""The cc-claude backend's metadata keys and its typed reads and writes of them.

``ClaudeSessionFields`` are keys of a session's ``metadata.json`` and ``ClaudeThreadFields`` are keys of a
thread's. ``src/backends/claude_code/__init__.py`` registers both under the owner name ``OWNER`` by their
"module:Class" strings, so registering imports this module only on the first metadata read or write. This
module is the one place the backend calls ``src/infra/metadata_slots.py`` for them.
"""

import pydantic

from src.backends import claude_code
from src.infra import metadata_slots, models


class ClaudeSessionFields(pydantic.BaseModel):
  # Label (claude_accounts[].label) of the pool account whose transcript store
  # holds this session's Claude Code conversation. None until the pool assigns
  # one, and always None for a pinned or non-cc-claude backend.
  claude_account: str | None = None


class ClaudeThreadFields(pydantic.BaseModel):
  # The Claude Code session id the runtime chose for the task before its first process started.
  claude_session_id: str | None = None


def account_of(meta: models.SessionMetadata) -> str | None:
  """The pool account label that ``meta`` records."""
  fields = metadata_slots.fields_of(meta, claude_code.OWNER)
  assert isinstance(fields, ClaudeSessionFields)
  return fields.claude_account


def set_account(meta: models.SessionMetadata, label: str | None) -> None:
  """Record the pool account label on ``meta`` in memory."""
  metadata_slots.set_fields(meta, claude_code.OWNER, claude_account=label)


def session_id_of(thread: models.ThreadMetadata) -> str | None:
  """The Claude Code session id that ``thread`` records."""
  fields = metadata_slots.fields_of(thread, claude_code.OWNER)
  assert isinstance(fields, ClaudeThreadFields)
  return fields.claude_session_id


def set_session_id(thread: models.ThreadMetadata, session_id: str | None) -> None:
  """Record the Claude Code session id on ``thread`` in memory."""
  metadata_slots.set_fields(thread, claude_code.OWNER, claude_session_id=session_id)
