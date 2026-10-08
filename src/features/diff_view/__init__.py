from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_router("src.features.diff_view.api", prefix="/api/git", tags=("git",))
