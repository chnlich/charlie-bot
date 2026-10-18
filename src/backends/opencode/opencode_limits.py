"""The opencode backend's timeouts and its context-window compaction reserve.

The module holds constants and one function and imports only the lifecycle hook, so the usage
resolver reads the reserve through ``register()`` in this package's ``__init__.py`` without
importing ``opencode.py``.
"""

from src.runtime.hooks import backend_lifecycle

# httpx client for the shared server's control API: version probe, session
# create and get, environment, prompt, interrupt, shells, model list. The
# long-lived event stream is opened with timeout=None.
OPENCODE_HTTP_API_TIMEOUT = 30.0  # seconds

# Shared server startup: deadline for the listening line and the first
# server.connected event on the global event stream.
OPENCODE_SERVER_START_TIMEOUT = 30.0  # seconds

# Grace for the shared server to exit after SIGTERM; past it the shutdown
# escalates to SIGKILL.
OPENCODE_SERVER_STOP_TIMEOUT = 5.0  # seconds

# Grace for the server's own exit to surface when its event stream ends: a dying
# server also drops the stream, and the failure should name the exit, not the
# symptom. Paid only on the stream-loss path.
OPENCODE_STREAM_EXIT_GRACE = 0.2  # seconds

# The shared server stops after this long with no running turn. The next turn
# starts it again.
OPENCODE_SERVER_IDLE_EXIT_SECONDS = 3600.0  # seconds

# The heavy checkpoint: this long after the last turn ends (and no directory is
# claimed), the shared server reads its own RSS. Above the RSS limit below it
# stops, so the memory each directory release leaves behind is returned at once.
# The rest of the idle limit still applies when it does not stop.
OPENCODE_SERVER_HEAVY_CHECK_SECONDS = 60.0  # seconds
OPENCODE_SERVER_HEAVY_RSS_MB = 400.0  # MiB

# A turn waits this long for the earlier interrupts of its session to report
# session.execution.interrupted before it sends its prompt. Past it the session
# is stuck and the turn fails.
OPENCODE_INTERRUPT_SETTLE_TIMEOUT = 30.0  # seconds

# An opencode /event SSE stream carrying no session-id-bearing event for longer
# than this is declared dead: the turn fails loudly through the normal backend
# failure path. Server-level events (server.heartbeat every ~10 s,
# server.connected) pass through but do not reset the timer. Basis, measured on
# opencode-backend runs: p99 inter-event gap 131 s, longest legitimate gap on a
# completed turn 29.3 min; observed hangs run 25-497 min, so 60 min covers
# every observed hang.
OPENCODE_SSE_PROGRESS_TIMEOUT = 3600.0  # seconds

# opencode's own compaction output-reserve default ($d = 20000 in the opencode binary,
# applied as `compaction.reserved ?? min($d, maxOutputTokens)`; checkable via
# `grep -ao "compaction?\.reserved.\{0,140\}" <opencode binary>`). The only reader is the
# usage resolver's compact-point math (src.runtime.session_usage), through
# snapshot_reading_limits; the opencode backend (src.backends.opencode.opencode) never reads
# it — the binary's own default applies.
OPENCODE_COMPACT_OUTPUT_RESERVE = 20_000


def snapshot_reading_limits() -> backend_lifecycle.ContextLimits:
  """The context limits of a ``snapshot`` reading: the model's limit comes with the snapshot, the reserve is opencode's."""
  return backend_lifecycle.ContextLimits(declared_window=None, compact_reserve=OPENCODE_COMPACT_OUTPUT_RESERVE)
