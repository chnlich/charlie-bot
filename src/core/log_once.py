"""The one warn-once rule: at most one log line per key per process."""

from collections.abc import Callable, Hashable
from typing import Any


class WarnOnceRegistry:
  """Log at most one line per key per process.

  ``log`` emits *event* with *fields* through *emit* the first time the
  process sees *key*, and every later sighting of the same key is a no-op: a
  repeat re-fires a fired alarm rather than earning a second line. A call
  site derives *key* from exactly the fields the line logs, so a change in
  what is reported earns one new line, and nothing outside the log statement
  can drift the key away from what was reported.

  ``clear`` forgets every key and ``forget_where`` forgets the keys a
  predicate accepts, so a later sighting earns one new line again. A
  consumer re-arms the whole registry when a successful read ends every
  broken streak, or one alarm's keys when it ends one streak; tests restore
  the process-start state with ``clear``.
  """

  def __init__(self) -> None:
    self._seen: set[Hashable] = set()

  def log(self, emit: Callable[..., Any], event: str, key: Hashable, **fields: Any) -> None:
    """Emit one *event* line for *key*, the first time the process sees it."""
    if key in self._seen:
      return
    self._seen.add(key)
    emit(event, **fields)

  def clear(self) -> None:
    """Forget every key, restoring the process-start state."""
    self._seen.clear()

  def forget_where(self, match: Callable[[Hashable], bool]) -> None:
    """Forget every key *match* accepts, so its next sighting earns one new line."""
    self._seen = {key for key in self._seen if not match(key)}

  def __bool__(self) -> bool:
    """True when at least one key has fired."""
    return bool(self._seen)
