"""prompts/worker.md loader and builder contracts: fresh read on every
_build_worker_prompt call (see spawner.spawner_prompt.load_worker_prompt_sections), split on
`<!-- section: <id> -->` marker lines, injected with `{{token}}` sequential
str.replace substitution, fail-loud loader semantics (no caching, no
embedded-text fallback), section assembly order, and reviewer-prompt sourcing
from the same file.
"""

from pathlib import Path

import pytest
from conftest import build_worker_prompt
from conftest import cfg_with_repo as _cfg_with_repo

from src.core import spawner
from src.core.config import CharlieBotConfig

# --- Fail-loud loader semantics -----------------------------------------------


def _real_task_base_prompt_text() -> str:
  return (
      CharlieBotConfig(charliebot_home=Path("/tmp/unused"), paths={
          "worktree_dir": "/tmp/worktrees"
      }).charlie_bot_repo / "prompts" / "task_base.md").read_text(encoding="utf-8")


def _real_worker_prompt_text() -> str:
  return (
      CharlieBotConfig(charliebot_home=Path("/tmp/unused"), paths={
          "worktree_dir": "/tmp/worktrees"
      }).charlie_bot_repo / "prompts" / "worker.md").read_text(encoding="utf-8")


def test_missing_worker_prompt_file_raises_with_path_and_cause(tmp_path: Path) -> None:
  cfg = _cfg_with_repo(tmp_path)
  missing_path = tmp_path / "prompts" / "worker.md"

  with pytest.raises(FileNotFoundError, match="predates the worker-prompt extraction commit") as exc_info:
    spawner.spawner_prompt.load_worker_prompt_sections(cfg)
  assert str(missing_path) in str(exc_info.value)


def test_unresolved_token_in_assembled_output_raises(tmp_path: Path) -> None:
  text = _real_worker_prompt_text()
  mutated = text.replace(
      "<!-- section: role -->\n## Role",
      "<!-- section: role -->\n## Role\n{{unbound_token}}",
  )
  assert mutated != text  # sanity: the injection point exists

  prompts_dir = tmp_path / "prompts"
  prompts_dir.mkdir()
  (prompts_dir / "worker.md").write_text(mutated, encoding="utf-8")
  (prompts_dir / "task_base.md").write_text(_real_task_base_prompt_text(), encoding="utf-8")

  with pytest.raises(ValueError, match="unresolved"):
    build_worker_prompt("desc", cfg=_cfg_with_repo(tmp_path))
