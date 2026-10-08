from src.runtime.hooks import page_render, wiring


def register() -> None:
  wiring.register_command("session-tree", "src.features.session_tree_preview.cli")
  page_render.register_template_global(
      "preview_mode", "src.features.session_tree_preview.page_globals", attr="preview_mode")
