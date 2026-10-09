"""Transcription backend registry — the only place that maps ids to backends.

Adding a backend is one subclass module plus one line in ``_FACTORIES``. Importing
this module loads every backend module but neither numpy nor websockets: the local
backend loads the engine module when it first decodes, and the streaming backends
import websockets when they open a connection.
"""

from __future__ import annotations

from collections.abc import Callable
from typing import TYPE_CHECKING

from src.features.voice.transcription import base, gemini, local, muse

if TYPE_CHECKING:
  from src.infra import config

# id -> factory(cfg, **kwargs) -> backend.
_FACTORIES: dict[str, Callable[..., base.TranscriptionBackend]] = {}


def _local(cfg: config.CharlieBotConfig, **kwargs: object) -> base.TranscriptionBackend:
  return local.LocalTranscriptionBackend(cfg, **kwargs)


def _gemini(cfg: config.CharlieBotConfig, **kwargs: object) -> base.TranscriptionBackend:
  return gemini.GeminiTranscriptionBackend(cfg, **kwargs)


def _muse(cfg: config.CharlieBotConfig, **kwargs: object) -> base.TranscriptionBackend:
  return muse.MuseTranscriptionBackend(cfg, **kwargs)


_FACTORIES.update({"local": _local, "gemini": _gemini, "muse": _muse})


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
