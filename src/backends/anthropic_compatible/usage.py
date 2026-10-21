"""Context limits for cc-anthropic-compatible entries."""

from src.backends.anthropic_compatible import options
from src.backends.claude_code import claude_launch
from src.infra import models
from src.runtime.hooks import backend_lifecycle


def claude_reading_limits(
    option: options.CcAnthropicCompatibleBackend,
    run_kind: models.RunKind,
) -> backend_lifecycle.ContextLimits:
  """Return this entry's context window and Claude Code compaction reserves."""
  del run_kind
  return backend_lifecycle.ContextLimits(
      declared_window=option.context_window,
      compact_reserve=claude_launch.CLAUDE_COMPACT_OUTPUT_RESERVE + claude_launch.CLAUDE_COMPACT_CONTEXT_RESERVE,
  )
