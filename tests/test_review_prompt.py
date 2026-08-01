from pathlib import Path

from src.core.config import CharlieBotConfig
from src.core.review import build_review_prompt


def _cfg() -> CharlieBotConfig:
  return CharlieBotConfig(charliebot_home=Path("/tmp/charliebot-review-prompt-test"), worktree_dir="/tmp/worktrees")


def test_review_prompt_fetches_remote_base_before_scope_diff() -> None:
  prompt = build_review_prompt(
      branch_name="feature/review-fix",
      wt_path="/tmp/review-worktree",
      base_branch="main",
      cfg=_cfg(),
      session_id="session-1",
      original_thread_id="thread-1",
      sessions_dir=Path("/tmp/sessions"),
      context="review context",
      user_request="fix the prompt",
      worker_summary="changed review prompt",
  )

  fetch_line = "2. Fetch the latest base branch: `git fetch origin main`"
  diff_line = "3. Review the changes: `git diff origin/main...feature/review-fix`"

  assert fetch_line in prompt
  assert diff_line in prompt
  assert prompt.index(fetch_line) < prompt.index(diff_line)


def test_review_prompt_instructs_task_spec_review_contract() -> None:
  prompt = build_review_prompt(
      branch_name="feature/review-fix",
      wt_path="/tmp/review-worktree",
      base_branch="main",
      cfg=_cfg(),
      session_id="session-1",
      original_thread_id="thread-1",
      sessions_dir=Path("/tmp/sessions"),
      context="review context",
      user_request=(
          "## Goal\nFix it\n\n"
          "## Source Files\n- /tmp/source.md\n\n"
          "## Required Behavior\nPreserve state-machine transitions.\n\n"
          "## Reviewer Checklist\nVerify transitions."),
      worker_summary="changed review prompt",
  )

  assert "read every path listed under `## Source Files`" in prompt
  assert "Apply the task spec's `## Reviewer Checklist`" in prompt
  assert "verify the implementation against `## Required Behavior`" in prompt
  assert "do not rely only on tests" in prompt
