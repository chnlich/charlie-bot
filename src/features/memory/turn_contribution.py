"""The memory package's turn contribution: the applicable memory joins a run's managed instructions."""

from pathlib import Path

from src.features.memory import memory
from src.infra.config import CharlieBotConfig
from src.infra.models import SessionMetadata
from src.runtime import task_prompts
from src.runtime.hooks import turn_contributions


def memory_selection_for(meta: SessionMetadata, kind: str, cfg: CharlieBotConfig) -> memory.MemorySelection | None:
  """The audience mapping: a manager maps to master; every worker-kind run to worker.

  The selection (filtering and formatting) stays owned by :mod:`src.features.memory.memory`.
  Repo-less workers get the worker index only — never a guessed project.
  """
  if kind in task_prompts.MANAGER_KINDS:
    return memory.select_master_memory(cfg.memory_dir)
  # A repo-less worker matches no repo topic: worker index only, never a guessed project.
  repo_basename = Path(meta.task.repo_path).name if (meta.task is not None and meta.task.repo_path) else ""
  return memory.select_worker_memory(cfg.memory_dir, repo_basename)


def _memory_rule_segments(selection: memory.MemorySelection) -> list[task_prompts.RuleSegment]:
  """The memory selection's segments as ordered rule segments (scope=memory)."""
  segments: list[task_prompts.RuleSegment] = []
  for delivery, text, sources in selection.segments:
    segments.append(
        task_prompts.RuleSegment(
            text=text,
            sources=tuple(
                task_prompts.PromptSource(scope=task_prompts.SCOPE_MEMORY, source_ref=s.source_ref) for s in sources),
            delivery=delivery,
        ))
  return segments


class MemoryTurnContribution(turn_contributions.TurnContribution):
  """Adds the memory segments after the overlay segments and before the local rules."""

  def instruction_segments(self, meta: SessionMetadata, kind: str,
                           cfg: CharlieBotConfig) -> list[task_prompts.RuleSegment]:
    selection = memory_selection_for(meta, kind, cfg)
    return [] if selection is None else _memory_rule_segments(selection)


CONTRIBUTION = MemoryTurnContribution()
