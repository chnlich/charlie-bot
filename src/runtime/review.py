"""Review prompts, backend selection, and the event-log readers the review Run shares."""

import asyncio
import pathlib
from collections.abc import Collection

from src.infra import config, log_once, models, ndjson
from src.infra import event_types as ET
from src.runtime import backend_pool, message_aggregator, task_errors

log = log_once.LazyStructlogLogger()

# The reviewer contract's stable parts, one maintained home: src/runtime/review.py
# owns the review rules; prompts/ owns the generic templates. review_rules_text
# renders them as the review Run's managed instruction block;
# review_numbered_steps interpolates the same checks into the numbered steps.
_REVIEW_ROLE_TEXT = (
    "## Code Review\n"
    "You are reviewing another worker's code changes.\n"
    "- The repository guide is the repository's `CLAUDE.md` or `AGENTS.md` file and its `.claude/rules/` files.\n"
    "- The repository guide sets the rules for the repository's code, environment and tests.\n"
    "- These instructions set the rules for the task lifecycle, git operations and delegation.\n"
    "- When the repository guide conflicts with these instructions, follow these instructions.")

_REVIEW_CHECKLIST_INTRO = (
    "IMPORTANT: Make minimal changes. Prefer approving the worker's code as-is. "
    "Only fix clear bugs, correctness issues, or scope violations. "
    "Do not refactor, restyle, or improve code that is functionally correct.\n\n"
    "The Context section carries the task spec. When the spec has a `## Source Files` section, read every path "
    "it lists before judging the diff. Apply the spec's `## Reviewer Checklist`. For control-flow or "
    "state-machine tasks, verify the implementation against `## Required Behavior`; do not rely only "
    "on tests.\n\n"
    "The Later Messages section holds each message that the worker received\n"
    "after the task spec, in claim order.\n"
    "When a later message changes the task spec, judge the work against the later\n"
    "message.\n"
    "When the task spec and a skill disagree, follow the task spec.")

# The checklist heading and intro render only in the managed instruction block
# (review_rules_text); only the judgment rules below ride the numbered steps too.
_REVIEW_CHECKLIST_BLOCK = f"## Review Checklist\n{_REVIEW_CHECKLIST_INTRO}\n\n"

# The checklist's stable judgment rules; the volatile cd/fetch/diff/push steps
# carry the run's actual paths and render in review_numbered_steps.
_REVIEW_SCOPE_CHECK = (
    "**Scope check**: Flag any changes NOT requested in the task — extra flags, altered defaults,\n"
    "   new parameters, behavioral changes. Workers must only do what was asked.")
_REVIEW_DIVERGENT_CHECK = (
    "**Think divergently**: Beyond the diff, consider what could go wrong.\n"
    "   - Do changed values make sense? Cross-check against existing defaults and conventions.\n"
    "   - Are there edge cases, regressions, or interactions with other code the worker missed?\n"
    "   - Would this change surprise someone reading the code for the first time?")
_REVIEW_CORRECTNESS_CHECK = ("Check for: correctness, bugs, unintended side effects, missing edge cases.")
_REVIEW_STYLE_CHECK = ("Style: Google Style, 2-space indent, 120-col (only flag if egregious — YAPF handles most).")

_REVIEW_STABLE_RULES = "\n".join(
    f"{label} {body}" for label, body in (
        ("-", _REVIEW_SCOPE_CHECK),
        ("-", _REVIEW_DIVERGENT_CHECK),
        ("-", _REVIEW_CORRECTNESS_CHECK),
        ("-", _REVIEW_STYLE_CHECK),
    ))


def review_rules_text() -> str:
  """The reviewer contract's stable rules (the v2 review Run's managed instruction block).

  The volatile steps (cd/fetch/diff/push with this run's branch, worktree and
  base) are the review's task/input context, rendered by the launch path from
  review_numbered_steps — never part of the stable instruction hash.
  """
  return (f"{_REVIEW_ROLE_TEXT}\n\n"
          f"{_REVIEW_CHECKLIST_BLOCK}"
          f"{_REVIEW_STABLE_RULES}")


def review_numbered_steps(branch_name: str, wt_path: str, base_branch: str) -> str:
  """The review prompt's git steps with this run's actual branch/worktree/base.

  The steps name the published branch (review_published_branch) and its
  origin ref (review_landing_target), so a base recorded as ``origin/<b>``
  renders the same steps as ``<b>``.
  """
  published = review_published_branch(base_branch)
  landing = review_landing_target(base_branch)
  return "\n".join(
      [
          f"1. `cd {wt_path}`",
          f"2. Fetch the latest base branch: `git fetch origin {published}`",
          f"3. Review the changes: `git diff {landing}...{branch_name}`",
          "4. Verify the changes address the user's actual intent (from context research above).",
          f"5. {_REVIEW_SCOPE_CHECK}",
          f"6. {_REVIEW_DIVERGENT_CHECK}",
          f"7. {_REVIEW_CORRECTNESS_CHECK}",
          f"8. {_REVIEW_STYLE_CHECK}",
          "9. If you find issues, fix them and commit with descriptive messages.",
          "10. Before the rebase, commit every change you keep and restore tool-generated files with `git restore <path>`; untracked files stay in place.",
          f"11. Fetch the latest base branch: `git fetch origin {published}`",
          f"12. Rebase onto the remote base: `git rebase {landing}`",
          f"13. Push to remote base branch from the worktree: `git push origin HEAD:{published}`",
          (
              f"14. Verify: `git log --oneline -1 HEAD` and `git log --oneline -1 {landing}` "
              "must show the same commit."),
      ])


def review_published_branch(base_branch: str) -> str:
  """The branch name on origin that a Run's recorded base names.

  A base recorded as ``origin/<b>`` names that same published branch ``<b>``.
  """
  return base_branch.removeprefix('origin/')


def review_landing_target(base_branch: str) -> str:
  """The published branch step 13's push lands on: the reviewed Run's landing target.

  The push publishes the base on origin, so the target is its ``origin/`` ref,
  which git_verify_commit_landed fetches before judging; a local branch of the
  same name may lag it.
  """
  return f"origin/{review_published_branch(base_branch)}"


def review_spec_text(task: models.TaskSpec) -> str:
  """The spec a review judges the work against: the task goal, then its acceptance tests.

  The review reads the same task record the work Run's prompt reads. An empty
  goal raises, because a review with no spec has nothing to judge the work against.
  """
  if not task.goal.strip():
    raise task_errors.TaskInvalidError("review has no task spec: the task record's goal is empty")
  parts = [task.goal]
  if task.acceptance:
    parts.append("\n".join(["## Acceptance Tests", *[f"- {a}" for a in task.acceptance]]))
  return "\n\n".join(parts)


def review_context_lines(
    spec_text: str,
    later_messages: str,
    worker_summary: str | None,
    delegator_hint: str,
) -> list[str]:
  """The review prompt's context lines: the task spec, the Later Messages section when the worker
  received messages after it, the worker summary when one was extracted, and the hint."""
  lines = [f"**Task spec:**\n{spec_text}"]
  if later_messages:
    lines.append(later_messages)
  if worker_summary:
    lines.append(f"**Worker summary:** {worker_summary}")
  lines.append(f"**Delegator hint:** {delegator_hint}")
  return lines


def review_log_pointer(chat_log_path: pathlib.Path, worker_log_path: pathlib.Path) -> str:
  """The context footer that sends the reviewer to the run's full logs."""
  return (
      f"If the summary above is insufficient or you are unsure about intent, "
      f"read the full logs: Session: `{chat_log_path}`, Worker: `{worker_log_path}`")


def review_git_venue(branch_name: str, wt_path: str) -> str:
  """The sentence pinning the numbered git steps to this run's worktree."""
  return (
      f"The work is on branch `{branch_name}` in worktree `{wt_path}`. "
      f"All git operations below run from the worktree.")


def _worker_summary_from_events_log(worker_log: pathlib.Path) -> str | None:
  """The worker's own closing words (message_aggregator.closing_text), or None.

  Streams the log from the end and stops at the first event that settles the
  answer; blank, malformed and missing-file cases follow the shared walk's
  skip contract (None, never an error). Only the closing-text types can
  settle the answer, so the walk parses nothing else — the multi-megabyte
  tool_result lines between the answer and the tail would otherwise parse
  whole.
  """
  summary_types = message_aggregator.CLOSING_TEXT_TYPES
  for event in ndjson.iter_ndjson_events_from_end(worker_log, log_event=ndjson.PARSE_SKIP_LOG_EVENT, log_fields={},
                                                  parse_filter=ndjson.type_line_filter(summary_types)):
    text = message_aggregator.closing_text(event)
    if text is not None:
      return text
  return None


def _worker_error_from_events_log(worker_log: pathlib.Path) -> str | None:
  """The newest non-empty error-event text in a worker's events log, or None.

  A run that failed before its process produced any output (a worktree or
  backend preparation failure) carries its actual error as the log's error
  event — the evidence the parent report's summary must name. Same walk and
  skip contract as :func:`_worker_summary_from_events_log`.
  """
  error_types = frozenset({ET.ERROR})
  for event in ndjson.iter_ndjson_events_from_end(worker_log, log_event=ndjson.PARSE_SKIP_LOG_EVENT, log_fields={},
                                                  parse_filter=ndjson.type_line_filter(error_types)):
    for key in ("message", "content"):
      val = event.get(key)
      if isinstance(val, str):
        stripped = val.strip()
        if stripped:
          return stripped
  return None


async def extract_review_context(session_id: str, thread_id: str, worker_log_path: pathlib.Path) -> str | None:
  """Extract the worker summary from its JSONL log for review context, or None when extraction fails.

  The caller's prompt builder handles a missing summary.
  """
  worker_summary: str | None = None
  try:
    worker_summary = await asyncio.to_thread(_worker_summary_from_events_log, worker_log_path)
  except Exception as e:
    log.warning("review_context_worker_events_failed", session=session_id, thread=thread_id, error=str(e))
  if not worker_summary:
    log.warning("review_context_worker_summary_unavailable", session=session_id, thread=thread_id)
  return worker_summary


def select_reviewer_backend(
    cfg: config.CharlieBotConfig,
    caller: str,
    worker_backend: str,
    worker_model: str | None,
    tried_backends: list[str],
    *,
    used_backends: Collection[str] = (),
) -> tuple[str, str | None, list[str]] | None:
  """Select a checking-role backend (reviewer, verify, cold read) from backends.pool.

  The pool skips the worker's own backend, every *used_backends* member, the *tried_backends*
  members and the cooling members. Returns (resolved_backend, resolved_model,
  updated_tried_backends), or None when every route is spent. An empty pool runs the check on
  the worker's backend, as it always has; so does a spent pool for a caller that names no
  *used_backends* (verify, cold read — the reviewer's exclusion rule is absolute: it never
  reviews on a member the worker node used).
  """
  option = backend_pool.select(cfg, caller=caller, exclude={worker_backend, *used_backends, *tried_backends})
  if option is not None:
    model = models.option_default_model(option, subject="backends.pool entry ")
    return option.id, model, [*tried_backends, option.id]
  if not cfg.backends.pool:
    return worker_backend, worker_model, [*tried_backends, worker_backend]
  if worker_backend in tried_backends or worker_backend in used_backends:
    log.warning("reviewer_all_backends_exhausted", caller=caller, tried=tried_backends)
    return None
  log.info("reviewer_fallback_to_worker_backend", caller=caller, worker_backend=worker_backend, tried=tried_backends)
  return worker_backend, worker_model, [*tried_backends, worker_backend]
