"""Offline memory-curation replay: frozen evidence in, complete-entry proposal out.

This package owns the whole replay pipeline (manifest, retrieval, model
exchange, mechanical validation, proposal bundle); ``src/cli/memory.py`` is its
thin entry point. Nothing here touches the live memory store: current entries,
the topics vocabulary, and every piece of evidence arrive frozen in the
manifest, and the only writes go to the requested output root.
"""

from src.core.memory_replay.errors import (
    ReplayBackendError,
    ReplayError,
    ReplayIsolationError,
    ReplayManifestError,
    ReplayModelOutputError,
    ReplayTransportError,
    ReplayValidationError,
)
from src.core.memory_replay.runner import MODES, ReplayOptions, ReplayOutcome, run_replay

__all__ = [
    "MODES",
    "ReplayBackendError",
    "ReplayError",
    "ReplayIsolationError",
    "ReplayManifestError",
    "ReplayModelOutputError",
    "ReplayOptions",
    "ReplayOutcome",
    "ReplayTransportError",
    "ReplayValidationError",
    "run_replay",
]
