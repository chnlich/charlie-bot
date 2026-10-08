"""Voice dictation: the recording endpoints, the transcription backends and the engine setup.

Deleting this package also deletes ``skills/voice-notes/``: the skill's
``scripts/decode_audio.py`` imports the voice transcriber, so the skill goes
with the package.
"""

from src.infra import config_registry
from src.runtime.hooks import page_render, wiring


def register() -> None:
  wiring.register_router("src.features.voice.api", prefix="/api/voice", tags=("voice",))
  wiring.register_router("src.features.voice.api", attr="ws_router")
  wiring.register_service("speech", "src.features.voice.service", phase="early")
  page_render.register_template_global("voice_backends", "src.features.voice.page_globals", attr="voice_backends")
  page_render.register_template_global(
      "voice_default_backend", "src.features.voice.page_globals", attr="voice_default_backend")
  config_registry.register_config_section(
      "voice",
      "src.features.voice.config:VoiceConfig",
      legacy_keys={
          "voice_engine": "voice.engine",
          "voice_model_id": "voice.model_id",
      },
  )
  config_registry.register_config_check("src.features.voice.config_check:check_default_backend")
  wiring.register_setup_step("src.features.voice.voice_setup", attr="setup_step")
