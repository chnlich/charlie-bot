"""Transcription backend registry — the only place that maps ids to backends.

Adding a backend is one subclass module plus one line in ``_FACTORIES``. The map
holds factories whose bodies do the imports, so importing this module loads
neither numpy nor websockets (the server import floor, docs/perf_baseline.md
M99): a backend's heavy module loads when that backend is first built.
"""

from __future__ import annotations

from collections.abc import Callable
from typing import TYPE_CHECKING

from src.agents.transcription import base

if TYPE_CHECKING:
  from src.core import config

# id -> factory(cfg, **kwargs) -> backend. Each factory imports its module on
# first call; the registered callables keep this module import-light.
_FACTORIES: dict[str, Callable[..., base.TranscriptionBackend]] = {}


def _local(cfg: config.CharlieBotConfig, **kwargs: object) -> base.TranscriptionBackend:
  from src.agents.transcription import local

  return local.LocalTranscriptionBackend(cfg, **kwargs)


def _gemini(cfg: config.CharlieBotConfig, **kwargs: object) -> base.TranscriptionBackend:
  from src.agents.transcription import gemini

  return gemini.GeminiTranscriptionBackend(cfg, **kwargs)


def _gemini_aigw(cfg: config.CharlieBotConfig, **kwargs: object) -> base.TranscriptionBackend:
  from src.agents.transcription import gemini_aigw

  return gemini_aigw.GeminiAigwTranscriptionBackend(cfg, **kwargs)


def _muse(cfg: config.CharlieBotConfig, **kwargs: object) -> base.TranscriptionBackend:
  from src.agents.transcription import muse

  return muse.MuseTranscriptionBackend(cfg, **kwargs)


_FACTORIES.update({"local": _local, "gemini": _gemini, "gemini-aigw": _gemini_aigw, "muse": _muse})


def backend_ids() -> tuple[str, ...]:
  """The registered backend ids, in registration order."""
  return tuple(_FACTORIES)


def register_transcription_backend(backend_id: str, factory: Callable[..., base.TranscriptionBackend]) -> None:
  """Add one backend to the registry.

  The replay script's variants register here, and tests register fakes the same
  way, so a consumer driven through the registry needs no code change.
  """
  _FACTORIES[backend_id] = factory


def build_transcription_backend(
    backend_id: str, cfg: config.CharlieBotConfig, **kwargs: object) -> base.TranscriptionBackend:
  """Instantiate the backend registered under *backend_id*.

  Building never requires the backend's credential: availability is reported
  by ``unavailable_reason()``.

  Raises:
    ValueError: If the id is unknown, naming the known ids.
  """
  factory = _FACTORIES.get(backend_id)
  if factory is None:
    raise ValueError(f"unknown transcription backend {backend_id!r}; known: {', '.join(_FACTORIES)}")
  return factory(cfg, **kwargs)


def build_transcription_backends(cfg: config.CharlieBotConfig) -> list[base.TranscriptionBackend]:
  """Every registered backend, in registration order (the dropdown's lister)."""
  return [build_transcription_backend(backend_id, cfg) for backend_id in _FACTORIES]
