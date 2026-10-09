"""The template globals of the index page: the voice dropdown's backends and the default backend."""

from src.features.voice.transcription.registry import build_transcription_backends
from src.infra import config


def voice_backends() -> list[dict]:
  """One entry per transcription backend, in dropdown order."""
  return [
      {
          "id": backend.id,
          "label": backend.label,
          "live_partials": backend.live_partials,
          "unavailable_reason": backend.unavailable_reason(),
      } for backend in build_transcription_backends(config.get_config())
  ]


def voice_default_backend() -> str:
  """The transcription backend id that the dropdown selects before the user picks one."""
  return config.get_config().voice.default_backend
