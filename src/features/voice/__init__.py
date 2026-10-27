"""Voice dictation: the online transcription backends, the turn note and the replay-eval command.

Deleting this package also deletes ``skills/voice-notes/``: the skill's
``scripts/decode_audio.py`` imports the shared recording transcription, so the
skill goes with the package.
"""

from src.infra import config_registry
from src.runtime.hooks import turn_contributions, wiring


def register() -> None:
  wiring.register_command("voice-replay-eval", "src.features.voice.replay_eval")
  turn_contributions.register_turn_contribution("voice", "src.features.voice.turn_contribution:CONTRIBUTION")
  config_registry.register_config_section("voice", "src.features.voice.config:VoiceConfig")
  config_registry.register_config_check("src.features.voice.config_check:check_default_backend")
