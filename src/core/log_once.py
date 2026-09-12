"""The one warn-once rule: at most one log line per key per process.

Also home to the structlog-deferring logger proxy. Both residents stay
stdlib-only: config and memory bind them at import, on CLI chains whose
measured floors depend on structlog staying out until first use.
"""

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


class LazyStructlogLogger:
  """Forwards every attribute to structlog's logger, importing structlog on first use.

  ``import structlog`` eagerly pulls structlog.dev (rich, pygments, the traceback
  formatter) — ~67 ms of the CLI import floor the M92 collector measures and ~97 ms
  of the memory-CLI invocation wall the M98 collector measures — while the modules
  binding ``log`` emit only on log lines those CLI invocations never reach. A test
  may monkeypatch an attribute on a module's ``log``: the patch lands on this
  object, which every later lookup reaches.
  """

  def __getattr__(self, name: str) -> Any:
    import structlog

    return getattr(structlog.get_logger(), name)
