"""The one warn-once rule: at most one log line per key per process.

Also home to the structlog-deferring logger proxy and the server's lean
log-line renderer. Every resident stays stdlib-only at import: config and
memory bind them at import, on CLI chains whose measured floors depend on
structlog staying out until first use.
"""

import os
import re
import sys
import time
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


# The characters whose presence sends a str value through repr(): the dev
# KeyValueColumnFormatter's quote rule, matched by one search instead of a
# per-value set build on the hot render path.
_QUOTE_NEEDED = re.compile(r"[ \t=\r\n\"']")


def _render_log_value(value: object) -> str:
  """structlog.dev's KeyValueColumnFormatter value rule for the non-color path.

  A str renders bare unless it carries whitespace, =, a newline, or a quote;
  everything else renders through repr().
  """
  if isinstance(value, str):
    if _QUOTE_NEEDED.search(value):
      return repr(value)
    return value
  return repr(value)


# The stamp's year-through-minute prefix, memoized per (year, month, day, hour,
# minute): the prefix moves once a minute while the stamp renders per request
# (measured 1.4 us full-format vs 0.4 us prefix+seconds). One immutable
# (key, prefix) tuple swapped atomically; a reader whose minute differs from
# the memo's builds its own prefix from its own struct, so an interleaved
# minute rollover never mislabels a line.
_stamp_prefix_memo: tuple[tuple[int, int, int, int, int], str] = ((0, 0, 0, 0, 0), "")


def _local_timestamp() -> str:
  """The chain's stamp: local wall time in the TimeStamper's %Y-%m-%d %H:%M:%S shape.

  ``time.localtime`` reads the same tz rules ``datetime.now().astimezone()``
  does and formats without strftime — the stamp is on every log line's hot
  path (the M3 access line renders one per request), so the year-through-minute
  prefix renders once a minute (see ``_stamp_prefix_memo``) and only the
  seconds format per call.
  """
  global _stamp_prefix_memo
  t = time.localtime()
  key = (t.tm_year, t.tm_mon, t.tm_mday, t.tm_hour, t.tm_min)
  memo = _stamp_prefix_memo
  if memo[0] == key:
    return f"{memo[1]}{t.tm_sec:02d}"
  prefix = f"{t.tm_year:04d}-{t.tm_mon:02d}-{t.tm_mday:02d} {t.tm_hour:02d}:{t.tm_min:02d}:"
  _stamp_prefix_memo = (key, prefix)
  return f"{prefix}{t.tm_sec:02d}"


class _LocalStampProcessor:
  """The chain's TimeStamper equivalent: stamps ``timestamp`` with :func:`_local_timestamp`.

  The default chain's TimeStamper(utc=False) stamps local time; this processor
  produces the same string faster (the format is fixed, so strftime's parse of
  it is dead work). The utc=True variant would shift every served stamp to UTC.
  """

  def __call__(self, logger: object, name: str, event_dict: dict) -> dict:
    event_dict["timestamp"] = _local_timestamp()
    return event_dict


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
    # structlog._config's exact color decision for the default chain, mirrored
    # so the lean path and the dev fallback agree under NO_COLOR/FORCE_COLOR
    # (the server's redirected log file takes the non-color shape on linux,
    # where structlog.dev._has_colors is always true).
    no_colors = os.environ.get("NO_COLOR", "") != ""
    force_colors = os.environ.get("FORCE_COLOR", "") != ""
    self._colors = not no_colors and (
        force_colors or (sys.stdout is not None and hasattr(sys.stdout, "isatty") and sys.stdout.isatty()))
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
_lean_renderer: _LeanLineRenderer | None = None


def ensure_lean_renderer() -> None:
  """Install the lean log-line renderer once per process.

  structlog's default chain ends in the dev ConsoleRenderer, which every
  http_request line pays on the request path. Called from the server's
  lifespan startup — never at import, where it would tax the CLI floors the
  M92/M98 collectors measure, and never inside a structlog.testing.capture_logs
  context, whose exit restores the config it entered with.
  """
  global _lean_renderer_installed, _lean_renderer
  if _lean_renderer_installed:
    return
  _lean_renderer_installed = True
  import structlog

  _lean_renderer = _LeanLineRenderer()
  structlog.configure(
      processors=[
          structlog.contextvars.merge_contextvars,
          structlog.processors.add_log_level,
          structlog.processors.StackInfoRenderer(),
          structlog.dev.set_exc_info,
          _LocalStampProcessor(),
          _lean_renderer,
      ])


# The access line's two fixed columns: the level and event pads are constants
# of the line's shape, and the byte-identity test against _LeanLineRenderer
# fails loud when a structlog upgrade moves either width.
_ACCESS_LEVEL_COLUMN = _pad_log_field("info", _LEVEL_WIDTH)
_ACCESS_EVENT_COLUMN = _pad_log_field("http_request", _EVENT_WIDTH)


def log_http_request_line(
    method: str, path: str, status: object, duration_ms: object, client: object, error: str | None = None) -> None:
  """Print the access log line the configured chain renders for this event.

  The chain's other processors do nothing for this shape — the middleware
  never sets exc_info or stack_info, and nothing in the process binds
  contextvars (a future bind_contextvars would silently drop its keys from
  this line) — so the line composes directly: no per-line dict merge, sort,
  or field scan, and one stdout write where print's unbuffered shape pays
  two (measured: the renderer round trip ~7.6 us of the raw-ASGI 401 floor
  the M3 sub-reading prices, the composed form ~2.5 us). The parameter set
  fixes the field order — client, duration_ms, error, method, path, status —
  the sorted order the renderer would emit, and byte identity against
  _LeanLineRenderer is pinned per value shape in tests/test_log_line_renderer.py
  and end to end in tests/test_request_logging.py. Capture-based readers see
  the line on stdout, not in structlog's capture list.
  """
  fields = f"client={_render_log_value(client)} duration_ms={_render_log_value(duration_ms)}"
  if error is not None:
    fields += f" error={_render_log_value(error)}"
  fields += f" method={_render_log_value(method)} path={_render_log_value(path)} status={_render_log_value(status)}"
  sys.stdout.write(f"{_local_timestamp()} [{_ACCESS_LEVEL_COLUMN}] {_ACCESS_EVENT_COLUMN} {fields}\n")
