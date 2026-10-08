from src.runtime.hooks import page_render, wiring


def register() -> None:
  wiring.register_router("src.features.ncu.api", tags=("pages",))
  page_render.register_templates("src.features.ncu")
