from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_router("src.features.diag.api", prefix="/api/diag", tags=("diag",))
