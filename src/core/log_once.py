"""The one warn-once rule: at most one log line per key per process.

Also home to the structlog-deferring logger proxy. Both residents stay
stdlib-only: config and memory bind them at import, on CLI chains whose
measured floors depend on structlog staying out until first use.
"""

import sys
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


# The column keys the dev ConsoleRenderer renders itself; the lean renderer
# skips them when it sorts the remaining key=value fields.
_COLUMN_KEYS = frozenset({"timestamp", "level", "event", "logger", "logger_name"})

# A line carrying any of these keeps the dev renderer: its traceback formatter
# and the [logger] column are the shapes the lean renderer does not reproduce.
_DEV_FALLBACK_KEYS = frozenset({"exc_info", "exception", "stack", "logger", "logger_name"})

# structlog 25.5.0's ConsoleRenderer pads the level to its longest level-style
# name ("exception", 9) and the event to _EVENT_WIDTH; the byte-identity test
# pins both against the live renderer, so an upgrade that moves them fails loud.
_LEVEL_WIDTH = 9
_EVENT_WIDTH = 30


def _pad_log_field(field: str, width: int) -> str:
  """Right-pad *field* to *width* with spaces; structlog.dev._pad's exact shape."""
  missing = width - len(field)
  return field + " " * max(0, missing)


def _render_log_value(value: object) -> str:
  """structlog.dev's KeyValueColumnFormatter value rule for the non-color path.

  A str renders bare unless it carries whitespace, =, a newline, or a quote;
  everything else renders through repr().
  """
  if isinstance(value, str):
    if set(value) & {" ", "\t", "=", "\r", "\n", '"', "'"}:
      return repr(value)
    return value
  return repr(value)


class _LeanLineRenderer:
  """The dev ConsoleRenderer's non-color line, rendered inline.

  ConsoleRenderer's pad/repr machinery prices ~43 us per request log line —
  73% of the raw-ASGI 401 floor the M3 sub-reading prices in
  docs/perf_baseline.md — while the common line is a timestamp, a level, the
  event, and sorted key=value fields. Exception, stack, and logger-name lines
  keep the dev renderer, whose output this class mirrors byte for byte
  (pinned by tests/test_log_line_renderer.py).
  """

  def __init__(self) -> None:
    # The dev renderer colorizes only when stdout is a terminal; the server's
    # redirected log file always takes the non-color shape this class mirrors.
    self._colors = sys.stdout.isatty()
    self._dev: Any = None

  def __call__(self, logger: object, name: str, event_dict: dict) -> str:
    if (self._colors or event_dict.keys() & _DEV_FALLBACK_KEYS):
      return self._dev_renderer()(logger, name, event_dict)
    timestamp = event_dict.get("timestamp")
    level = event_dict.get("level")
    if not isinstance(timestamp, str) or not isinstance(level, str):
      return self._dev_renderer()(logger, name, event_dict)
    line = (
        f"{timestamp} [{_pad_log_field(level, _LEVEL_WIDTH)}] "
        f"{_pad_log_field(str(event_dict.get('event', '')), _EVENT_WIDTH)}")
    fields = " ".join(
        f"{key}={_render_log_value(event_dict[key])}" for key in sorted(event_dict) if key not in _COLUMN_KEYS)
    if fields:
      line += " " + fields
    return line.rstrip(" ")

  def _dev_renderer(self) -> Any:
    # structlog.dev carries rich and pygments; only the fallback shapes pay it.
    if self._dev is None:
      from structlog.dev import ConsoleRenderer

      self._dev = ConsoleRenderer(colors=self._colors)
    return self._dev


_lean_renderer_installed = False


def ensure_lean_renderer() -> None:
  """Install the lean log-line renderer once per process.

  structlog's default chain ends in the dev ConsoleRenderer, which every
  http_request line pays on the request path. Called from the server's
  lifespan startup — never at import, where it would tax the CLI floors the
  M92/M98 collectors measure, and never inside a structlog.testing.capture_logs
  context, whose exit restores the config it entered with.
  """
  global _lean_renderer_installed
  if _lean_renderer_installed:
    return
  _lean_renderer_installed = True
  import structlog

  structlog.configure(
      processors=[
          structlog.contextvars.merge_contextvars,
          structlog.processors.add_log_level,
          structlog.processors.StackInfoRenderer(),
          structlog.dev.set_exc_info,
          structlog.processors.TimeStamper(fmt="%Y-%m-%d %H:%M:%S"),
          _LeanLineRenderer(),
      ])
