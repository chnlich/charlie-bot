from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_router("src.features.host_auth.api", tags=("host-auth",))
  wiring.register_service("host_auth", "src.features.host_auth.api")
