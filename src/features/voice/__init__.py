"""Voice dictation: the online transcription backends and the turn note."""

from src.infra import config_registry
from src.runtime.hooks import turn_contributions


def register() -> None:
  turn_contributions.register_turn_contribution("voice", "src.features.voice.turn_contribution:CONTRIBUTION")
  config_registry.register_config_section("voice", "src.features.voice.config:VoiceConfig")
  config_registry.register_config_check("src.features.voice.config_check:check_default_backend")
