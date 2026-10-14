"""prompts/worker.md loader contracts used by task prompt assembly: fresh reads,
split on
`<!-- section: <id> -->` marker lines, injected with `{{token}}` sequential
str.replace substitution, fail-loud loader semantics (no caching, no
embedded-text fallback), section assembly order, and reviewer-prompt sourcing
from the same file.
"""

import pathlib

import conftest
import pytest

from src.runtime import spawner

# --- Fail-loud loader semantics -----------------------------------------------


def test_missing_worker_prompt_file_raises_with_path_and_cause(tmp_path: pathlib.Path) -> None:
  cfg = conftest.cfg_with_repo(tmp_path)
  missing_path = tmp_path / "prompts" / "worker.md"

  with pytest.raises(FileNotFoundError, match="predates the worker-prompt extraction commit") as exc_info:
    spawner.spawner_prompt.load_worker_prompt_sections(cfg)
  assert str(missing_path) in str(exc_info.value)
