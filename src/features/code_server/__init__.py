from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_router("src.features.code_server.api", prefix="/api/code-server", tags=("code-server",))
