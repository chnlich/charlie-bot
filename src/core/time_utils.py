"""Time helpers shared across layers, stdlib-only by contract.

``utc_now`` is the timestamp stamp every layer reaches for, and the CLI verbs
that stamp times (remote-launch, gc-trash, the plan verb paths) must not load
``src.core.models`` — the session/API model stack — to call it. The helper
therefore lives here and ``src.core.models`` re-exports it for its established
import path; only ``datetime`` may ride this module's import.
"""

from datetime import UTC, datetime


def utc_now() -> datetime:
  """Return the current UTC datetime as a tz-aware value."""
  return datetime.now(UTC)
