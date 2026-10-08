from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_router("src.features.recap.api", prefix="/api/sessions", tags=("sessions",))
