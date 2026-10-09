"""The voice package's config check: ``voice.default_backend`` names a transcription backend."""

from typing import TYPE_CHECKING

from src.features.voice.transcription import registry

if TYPE_CHECKING:
  from src.infra import config


def check_default_backend(cfg: config.CharlieBotConfig) -> None:
  """A default_backend typo must fail at startup: validate against the registry's ids."""
  known = registry.backend_ids()
  if cfg.voice.default_backend not in known:
    raise ValueError(
        f"voice.default_backend {cfg.voice.default_backend!r} is not a transcription backend; "
        f"known: {', '.join(known)}")
