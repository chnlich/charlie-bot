from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_router(
      "src.backends.openai_compatible.anthropic_proxy", prefix="/api/anthropic-proxy", tags=("anthropic-proxy",))
