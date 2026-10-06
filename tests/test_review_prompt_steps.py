"""The reviewer prompt's numbered git steps: the no-stash rewrite (plan 3 v3).

Review step 10 used to order `git stash --include-untracked`; every worktree
of a repository shares one stash stack, so the step trained agents into the
habit behind the cross-task stash incidents. The landed contract: step 10
reads exactly the replacement line, "stash" appears nowhere in the rendered
prompt (the volatile steps or the stable rules), and every other step and the
numbering stay unchanged.
"""

import re

from src.core import review

_NEW_STEP_10 = (
    "10. Before the rebase, commit every change you keep and restore "
    "tool-generated files with `git restore <path>`; untracked files stay in place.")


def _expected_steps() -> list[str]:
  """The full step list with only step 10 rewritten.

    Steps 1-14 other than step 10 compose from the same constants the builder
    renders, so this pins the rewrite as the only change to the step list.
    """
  published = review.review_published_branch("origin/main")
  landing = review.review_landing_target("origin/main")
  return [
      "1. `cd /wt`",
      f"2. Fetch the latest base branch: `git fetch origin {published}`",
      f"3. Review the changes: `git diff {landing}...task-1`",
      "4. Verify the changes address the user's actual intent (from context research above).",
      f"5. {review._REVIEW_SCOPE_CHECK}",
      f"6. {review._REVIEW_DIVERGENT_CHECK}",
      f"7. {review._REVIEW_CORRECTNESS_CHECK}",
      f"8. {review._REVIEW_STYLE_CHECK}",
      "9. If you find issues, fix them and commit with descriptive messages.",
      _NEW_STEP_10,
      f"11. Fetch the latest base branch: `git fetch origin {published}`",
      f"12. Rebase onto the remote base: `git rebase {landing}`",
      f"13. Push to remote base branch from the worktree: `git push origin HEAD:{published}`",
      (f"14. Verify: `git log --oneline -1 HEAD` and `git log --oneline -1 {landing}` "
       "must show the same commit."),
  ]


def test_step_ten_commits_instead_of_stashing():
  steps = review.review_numbered_steps("task-1", "/wt", "origin/main").splitlines()
  assert _NEW_STEP_10 in steps


def test_step_count_numbering_and_other_steps_unchanged():
  rendered = review.review_numbered_steps("task-1", "/wt", "origin/main")
  assert rendered == "\n".join(_expected_steps())
  numbered = [line for line in rendered.splitlines() if re.match(r"\d+\. ", line)]
  assert [line.split(".", 1)[0] for line in numbered] == [str(n) for n in range(1, 15)]


def test_no_stash_remains_in_the_rendered_prompt():
  rendered = review.review_numbered_steps("task-1", "/wt", "origin/main") + review.review_rules_text()
  assert "stash" not in rendered.lower()
