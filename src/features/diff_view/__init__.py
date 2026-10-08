from src.runtime.hooks import page_render, wiring


def register() -> None:
  wiring.register_router("src.features.diff_view.api", prefix="/api/git", tags=("git",))
  wiring.register_router("src.features.diff_view.page", tags=("pages",))
  page_render.register_templates("src.features.diff_view")
  page_render.register_home_card("Diff viewer", "/diff", "Browse a repository diff between two refs.")
