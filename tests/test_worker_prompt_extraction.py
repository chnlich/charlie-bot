"""prompts/worker.md loader and builder contracts: fresh read on every
_build_worker_prompt call (see spawner.load_worker_prompt_sections), split on
`<!-- section: <id> -->` marker lines, injected with `{{token}}` sequential
str.replace substitution, fail-loud loader semantics (no caching, no
embedded-text fallback), section assembly order, and reviewer-prompt sourcing
from the same file.
"""

from pathlib import Path

import pytest

from src.core import review, spawner
from src.core.config import CharlieBotConfig
from src.core.models import SessionMetadata, TaskType

REPO_PATH = Path("/tmp/charliebot-fixture-repo")
BASE_BRANCH = "main"
BRANCH_NAME = "charliebot/task-fixture-0001"
WT_PATH = "/tmp/worktrees/charliebot-task-fixture-0001"
LOOP_DIR = "/tmp/charliebot-fixture-loops/iter-loop"
ITERATION_NUMBER = 7  # raw "## Iter 7" vs zero-padded "iter_0007.md"
DESCRIPTION = "## Goal\nDo the fixture task.\n\n## Source Files\n- /tmp/source.md\n"


def test_loop_axis_renders_both_iteration_number_forms() -> None:
  """Iter 7 -> raw '## Iter 7' heading and zero-padded 'iter_0007.md' filename, both present."""
  prompt = spawner._build_worker_prompt(
      description=DESCRIPTION,
      repo_path=REPO_PATH,
      base_branch=BASE_BRANCH,
      branch_name=BRANCH_NAME,
      wt_path=WT_PATH,
      session_meta=SessionMetadata(id="s", name="s"),
      cfg=CharlieBotConfig(charliebot_home=Path("/tmp/unused"), worktree_dir="/tmp/worktrees"),
      task_type=TaskType.IMPLEMENT,
      loop_dir=LOOP_DIR,
      iteration_number=ITERATION_NUMBER,
      is_continuation=False,
      keep_worktree=False,
      start_point=None,
  )
  assert "## Iter 7 — {completed|failed}" in prompt
  assert "iter_0007.md" in prompt


def test_continuation_intro_selected_over_new_session_intro() -> None:
  cfg = CharlieBotConfig(charliebot_home=Path("/tmp/unused"), worktree_dir="/tmp/worktrees")
  sections = spawner.load_worker_prompt_sections(cfg)

  prompt = spawner._build_worker_prompt(
      description=DESCRIPTION,
      repo_path=REPO_PATH,
      base_branch=BASE_BRANCH,
      branch_name=BRANCH_NAME,
      wt_path=WT_PATH,
      session_meta=SessionMetadata(id="s", name="s"),
      cfg=cfg,
      task_type=TaskType.IMPLEMENT,
      loop_dir=None,
      iteration_number=None,
      is_continuation=True,
      keep_worktree=False,
      start_point=None,
  )
  assert sections["intro_continuation"] in prompt
  assert sections["intro_new"] not in prompt


def test_memory_block_renders_into_prompt(tmp_path: Path) -> None:
  charliebot_home = tmp_path / "with-memory"
  memory_dir = charliebot_home / "memory"
  memory_dir.mkdir(parents=True)
  (memory_dir / "topics").write_text("fixture-topic\n", encoding="utf-8")

  prompt = spawner._build_worker_prompt(
      description=DESCRIPTION,
      repo_path=REPO_PATH,
      base_branch=BASE_BRANCH,
      branch_name=BRANCH_NAME,
      wt_path=WT_PATH,
      session_meta=SessionMetadata(id="s", name="s"),
      cfg=CharlieBotConfig(charliebot_home=charliebot_home, worktree_dir="/tmp/worktrees"),
      task_type=TaskType.IMPLEMENT,
      loop_dir=None,
      iteration_number=None,
      is_continuation=False,
      keep_worktree=False,
      start_point=None,
  )
  # A broken {{memory_block}} token fails the unresolved-token sweep instead of
  # reaching the assertions, and an inverted memory gate drops the heading.
  assert "## Memory" in prompt
  assert "{{" not in prompt


# --- Fail-loud loader semantics -----------------------------------------------


def _real_worker_prompt_text() -> str:
  return (
      CharlieBotConfig(charliebot_home=Path("/tmp/unused"), worktree_dir="/tmp/worktrees").charlie_bot_repo /
      "prompts" / "worker.md").read_text(encoding="utf-8")


def _cfg_with_repo(repo_root: Path) -> CharlieBotConfig:
  """A cfg-like object whose charlie_bot_repo points at *repo_root* (real CharlieBotConfig's
  charlie_bot_repo is a derived property tied to the installed package location, so a plain
  namespace stand-in is used to redirect it for these isolated fail-loud tests)."""

  class _Cfg:
    charlie_bot_repo = repo_root
    memory_dir = repo_root / "memory"

  return _Cfg()  # type: ignore[return-value]


def test_missing_worker_prompt_file_raises_with_path_and_cause(tmp_path: Path) -> None:
  cfg = _cfg_with_repo(tmp_path)
  missing_path = tmp_path / "prompts" / "worker.md"

  with pytest.raises(FileNotFoundError, match="predates the worker-prompt extraction commit"):
    spawner.load_worker_prompt_sections(cfg)

  try:
    spawner.load_worker_prompt_sections(cfg)
  except FileNotFoundError as e:
    assert str(missing_path) in str(e)


def test_missing_required_section_raises(tmp_path: Path) -> None:
  text = _real_worker_prompt_text()
  start = text.index("<!-- section: role -->")
  end = text.index("<!-- section: memory -->")
  mutated = text[:start] + text[end:]  # drop the entire "role" section

  prompts_dir = tmp_path / "prompts"
  prompts_dir.mkdir()
  (prompts_dir / "worker.md").write_text(mutated, encoding="utf-8")

  with pytest.raises(ValueError, match="role"):
    spawner.load_worker_prompt_sections(_cfg_with_repo(tmp_path))


def test_missing_remote_scratch_section_raises(tmp_path: Path) -> None:
  text = _real_worker_prompt_text()
  start = text.index("<!-- section: remote_scratch -->")
  end = text.index("<!-- section: role -->")
  mutated = text[:start] + text[end:]  # drop the entire "remote_scratch" section

  prompts_dir = tmp_path / "prompts"
  prompts_dir.mkdir()
  (prompts_dir / "worker.md").write_text(mutated, encoding="utf-8")

  with pytest.raises(ValueError, match="remote_scratch"):
    spawner.load_worker_prompt_sections(_cfg_with_repo(tmp_path))


def test_remote_scratch_assembled_directly_after_skills_discovery() -> None:
  cfg = CharlieBotConfig(charliebot_home=Path("/tmp/unused"), worktree_dir="/tmp/worktrees")
  sections = spawner.load_worker_prompt_sections(cfg)

  prompt = spawner._build_worker_prompt(
      description=DESCRIPTION,
      repo_path=REPO_PATH,
      base_branch=BASE_BRANCH,
      branch_name=BRANCH_NAME,
      wt_path=WT_PATH,
      session_meta=SessionMetadata(id="s", name="s"),
      cfg=cfg,
      task_type=TaskType.IMPLEMENT,
      loop_dir=None,
      iteration_number=None,
      is_continuation=False,
      keep_worktree=False,
      start_point=None,
  )
  assert sections["skills_discovery"] + "\n" + sections["remote_scratch"] in prompt
  assert sections["remote_scratch"] + "\n" + sections["role"] in prompt


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

  class _Cfg:
    charlie_bot_repo = tmp_path
    memory_dir = tmp_path / "memory"  # missing -> memory_section empty, irrelevant here

  with pytest.raises(ValueError, match="unresolved"):
    spawner._build_worker_prompt(
        description="desc",
        repo_path=Path("/tmp/repo"),
        base_branch="main",
        branch_name="branch",
        wt_path="/tmp/wt",
        session_meta=SessionMetadata(id="s", name="s"),
        cfg=_Cfg(),  # type: ignore[arg-type]
        task_type=TaskType.IMPLEMENT,
        loop_dir=None,
        iteration_number=None,
        is_continuation=False,
        keep_worktree=False,
        start_point=None,
    )


# --- Reviewer-prompt sourcing --------------------------------------------------


def test_reviewer_prompt_sources_coding_principles_from_worker_prompt_file(tmp_path: Path) -> None:
  text = _real_worker_prompt_text()
  marker = "<!-- section: coding_principles -->"
  next_marker = "<!-- section: skills_discovery -->"
  start = text.index(marker) + len(marker) + 1
  end = text.index(next_marker)
  custom_body = "## Coding Principles\nCUSTOM MARKER TEXT XYZ ONLY IN THIS TEMP COPY.\n\n"
  mutated = text[:start] + custom_body + text[end:]

  prompts_dir = tmp_path / "prompts"
  prompts_dir.mkdir()
  (prompts_dir / "worker.md").write_text(mutated, encoding="utf-8")

  cfg = _cfg_with_repo(tmp_path)
  prompt = review.build_review_prompt(
      branch_name="feature/x",
      wt_path="/tmp/review-worktree",
      base_branch="main",
      cfg=cfg,  # type: ignore[arg-type]
      session_id="session-1",
      original_thread_id="thread-1",
      sessions_dir=Path("/tmp/sessions"),
      context="ctx",
      user_request="req",
      worker_summary="summary",
  )
  assert "CUSTOM MARKER TEXT XYZ ONLY IN THIS TEMP COPY." in prompt
  assert "The codebase has a single user." not in prompt
