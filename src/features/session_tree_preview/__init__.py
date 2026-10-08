from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_command("session-tree", "src.features.session_tree_preview.cli")
