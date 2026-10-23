"""Voice dictation: the recording endpoints, the online transcription backends and the turn note.

Deleting this package also deletes ``skills/voice-notes/``: the skill's
``scripts/decode_audio.py`` imports the shared recording transcription, so the
skill goes with the package.
"""

from src.infra import config_registry
from src.runtime.hooks import page_render, turn_contributions, wiring


def register() -> None:
  wiring.register_command("voice-replay-eval", "src.features.voice.replay_eval")
  wiring.register_router("src.features.voice.api", prefix="/api/voice", tags=("voice",))
  wiring.register_router("src.features.voice.api", attr="ws_router")
  turn_contributions.register_turn_contribution("voice", "src.features.voice.turn_contribution:CONTRIBUTION")
  page_render.register_template_global("voice_backends", "src.features.voice.page_globals", attr="voice_backends")
  page_render.register_template_global(
      "voice_default_backend", "src.features.voice.page_globals", attr="voice_default_backend")
  config_registry.register_config_section("voice", "src.features.voice.config:VoiceConfig")
  config_registry.register_config_check("src.features.voice.config_check:check_default_backend")
