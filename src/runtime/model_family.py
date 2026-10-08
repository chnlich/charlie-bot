"""The family word of a model id.

The compaction note in the chat (``src/runtime/message_aggregator.py``) and the Claude account pool
(``src/backends/claude_code``) read model ids of the Claude naming scheme through this one function.
"""


def model_family(model: str | None) -> str:
  """The family word of a Claude model id: ``claude-fable-5-1`` -> ``fable``."""
  if not model:
    return ""
  parts = model.lower().split("-")
  return parts[1] if len(parts) > 1 and parts[0] == "claude" else parts[0]
