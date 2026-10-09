"""The template global of the index page: whether this process serves a session-tree preview instance."""

from src.features.session_tree_preview.session_tree_preview import is_preview_mode


def preview_mode() -> bool:
  """Whether this process serves a session-tree preview instance (drives the UI indicator)."""
  return is_preview_mode()
