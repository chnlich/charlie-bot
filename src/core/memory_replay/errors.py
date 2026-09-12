"""Replay error vocabulary: every failure that must surface as a CLI error.

The CLI turns any :class:`ReplayError` into one stderr line and a nonzero exit;
anything else escaping :func:`src.core.memory_replay.runner.run_replay` is a bug
in the pipeline itself and keeps its traceback.
"""


class ReplayError(Exception):
  """Base class for every replay failure a caller can act on."""


class ReplayManifestError(ReplayError):
  """The replay manifest is missing, unreadable, malformed, or inconsistent."""


class ReplayIsolationError(ReplayError):
  """The requested output root would overlap the live store or a frozen input."""


class ReplayBackendError(ReplayError):
  """The explicitly selected backend is unconfigured or unsupported for replay."""


class ReplayTransportError(ReplayError):
  """The model transport failed: endpoint, HTTP status, or response envelope."""


class ReplayModelOutputError(ReplayError):
  """A model response could not be parsed into the required JSON shape."""


class ReplayValidationError(ReplayError):
  """A model response failed mechanical validation (refs, paths, formats, patch)."""
