from pathlib import Path

import pytest

from src.core.config import CharlieBotConfig
from src.core.review import build_review_prompt
from src.core.task_prompts import review_task_context


def _cfg() -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-review-prompt-test"),
      paths={"worktree_dir": "/tmp/worktrees"},
  )


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


def _v1_prompt(base_branch: str) -> str:
  return build_review_prompt(
      branch_name="feature/review-fix",
      wt_path="/tmp/review-worktree",
      base_branch=base_branch,
      cfg=_cfg(),
      session_id="session-1",
      original_thread_id="thread-1",
      sessions_dir=Path("/tmp/sessions"),
      context="review context",
      user_request="fix the prompt",
      worker_summary="changed review prompt",
  )


def _v2_context(base_branch: str) -> str:
  return review_task_context(
      branch_name="feature/review-fix",
      wt_path="/tmp/review-worktree",
      base_branch=base_branch,
      session_id="session-1",
      chat_log_path=Path("/tmp/sessions/session-1/chat_events.jsonl"),
      worker_log_path=Path("/tmp/sessions/session-1/runs/run-1/events.jsonl"),
      context_section="review context",
  )


@pytest.mark.parametrize("render", [_v1_prompt, _v2_context], ids=["v1", "v2"])
@pytest.mark.parametrize("published", ["main", "dev/feature-x"])
def test_review_steps_name_the_published_base_for_an_origin_prefixed_base(render, published) -> None:
  bare = render(published)

  assert render(f"origin/{published}") == bare
  for line in (
      f"2. Fetch the latest base branch: `git fetch origin {published}`",
      f"3. Review the changes: `git diff origin/{published}...feature/review-fix`",
      f"11. Fetch the latest base branch: `git fetch origin {published}`",
      f"12. Rebase onto the remote base: `git rebase origin/{published}`",
      f"13. Push to remote base branch from the worktree: `git push origin HEAD:{published}`",
      (f"14. Verify: `git log --oneline -1 HEAD` and `git log --oneline -1 origin/{published}` "
       "must show the same commit."),
  ):
    assert line in bare
  assert "origin/origin/" not in bare
