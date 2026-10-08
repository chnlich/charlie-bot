"""The ``voice:`` config section model, registered in this package's ``register()``."""

from typing import Literal

from pydantic import BaseModel, ConfigDict


class VoiceConfig(BaseModel):
  """``voice:`` section: transcription backend and local-engine selection."""

  model_config = ConfigDict(extra='forbid')

  # Voice transcription engine. 'sherpa' runs the CPU ONNX pipeline everywhere; 'qwen3_hf'
  # runs the official transformers Qwen3-ASR weights on NVIDIA GPUs (gpu-voice dependency
  # group + weights, provisioned by scripts/setup.sh on hosts with nvidia-smi). Engine
  # changes take effect on server restart. Selects the transcription backend id 'local'
  # engine only; the cloud backends ignore it.
  engine: Literal['sherpa', 'qwen3_hf'] = 'sherpa'

  # Model repository id for the qwen3_hf engine; switching tiers (1.7B <-> 0.6B) is a
  # one-value change.
  model_id: str = 'Qwen/Qwen3-ASR-1.7B-hf'

  # Transcription backend used before the user picks one in the page's dropdown. One of
  # the transcription registry's ids (src/features/voice/transcription/registry.py); the voice
  # package's config check (src/features/voice/config_check.py) fails config load on a typo.
  default_backend: str = 'local'

  # Proper-noun vocabulary passed to the backends that support it (Gemini's
  # customVocabulary, Muse's keywords); the local engine ignores it.
  vocabulary: list[str] = []

  # Language hints as BCP-47 base codes (zh, en), mapped by each backend to its own wire
  # format; empty lets every backend auto-detect.
  languages: list[str] = []

  # aigw gateway root URL for the 'gemini-aigw' backend, which sends the whole
  # recording through the gateway's /gemini pass-through to Gemini 3.5 Transcribe.
  # Empty leaves that backend unavailable. Credential: credentials.yaml aigw.api_key.
  aigw_base_url: str = ''
