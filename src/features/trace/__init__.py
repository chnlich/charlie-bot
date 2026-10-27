from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_router("src.features.trace.api", tags=("pages",))
  wiring.register_service("merge_pool", "src.features.trace.api")
