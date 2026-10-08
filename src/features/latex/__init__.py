from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_router("src.features.latex.api", prefix="/api/latex", tags=("latex",))
