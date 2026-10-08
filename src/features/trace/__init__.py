from src.runtime.hooks import page_render, wiring


def register() -> None:
  wiring.register_router("src.features.trace.api", tags=("pages",))
  page_render.register_templates("src.features.trace")
  wiring.register_service("merge_pool", "src.features.trace.api")
