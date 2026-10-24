"""The ``voice:`` config section model, registered in this package's ``register()``."""

import pydantic


class VoiceConfig(pydantic.BaseModel):
  """``voice:`` section: the transcription backend selection, vocabulary and language hints."""

  model_config = pydantic.ConfigDict(extra='forbid')

  # Transcription backend used before the user picks one in the page's dropdown, and the one the upload endpoint and
  # the voice-notes skill use when a request names none. One of the transcription registry's ids
  # (src/features/voice/transcription/registry.py); the voice package's config check
  # (src/features/voice/config_check.py) fails config load on a typo.
  default_backend: str = 'gemini'

  # Proper-noun vocabulary passed to the backends that support it (Gemini's customVocabulary, Muse's keywords).
  vocabulary: list[str] = []

  # Language hints as BCP-47 base codes (zh, en), mapped by each backend to its own wire format; empty lets every
  # backend auto-detect.
  languages: list[str] = []
