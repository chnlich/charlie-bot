from src.infra import config_registry
from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_router("src.features.voice.api", prefix="/api/voice", tags=("voice",))
  wiring.register_router("src.features.voice.api", attr="ws_router")
  wiring.register_service("speech", "src.features.voice.service", phase="early")
  config_registry.register_config_check("src.features.voice.config_check:check_default_backend")
