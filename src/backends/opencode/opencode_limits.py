"""The opencode backend's timeouts and its context-window compaction reserve.

The module holds constants and one function and imports only the lifecycle hook, so the usage
resolver reads the reserve through ``register()`` in this package's ``__init__.py`` without
importing ``opencode.py``.
"""

from src.runtime.hooks import backend_lifecycle

# httpx client for the per-run control API: health probe, model limit, session
# create, prompt send. The long-lived SSE stream overrides this with
# timeout=None (its liveness is the watchdog constant below).
OPENCODE_HTTP_API_TIMEOUT = 30.0  # seconds

# Spawned `opencode serve` startup: deadline for the server URL to appear on
# the subprocess's stdout.
OPENCODE_SERVER_START_TIMEOUT = 30.0  # seconds

# Grace for the spawned server to exit after SIGTERM; past it the shutdown
# escalates to SIGKILL (base._graceful_shutdown).
OPENCODE_SERVER_STOP_TIMEOUT = 5.0  # seconds

# POST to /session/{id}/abort when a turn is cancelled. Best-effort: a failure
# is logged and cleanup proceeds without it.
OPENCODE_ABORT_TIMEOUT = 5.0  # seconds

# Grace wait for the stdout log-tail task to reach EOF after the run ends; past
# it the task is cancelled. The tail loop polls on a sub-second interval, so EOF
# arrives within milliseconds of the process exiting.
OPENCODE_STDOUT_DRAIN_TIMEOUT = 5.0  # seconds

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
