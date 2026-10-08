"""The template global of the index page: whether this process serves a session-tree preview instance.

The preview module imports inside the call, so it loads when the index page renders, not at server
import and not at the first render of another page.
"""


def preview_mode() -> bool:
  """Whether this process serves a session-tree preview instance (drives the UI indicator)."""
  from src.features.session_tree_preview.session_tree_preview import is_preview_mode
  return is_preview_mode()
