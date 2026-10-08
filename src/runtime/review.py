"""Review prompts, backend selection, and the event-log readers the review Run shares."""

import asyncio
import pathlib

from src.infra import config, log_once, models, ndjson
from src.infra import event_types as ET
from src.runtime import chat_events, message_aggregator

log = log_once.LazyStructlogLogger()

# The reviewer contract's stable parts, one maintained home: src/runtime/review.py
# owns the review rules; prompts/ owns the generic templates. review_rules_text
# renders them as the review Run's managed instruction block;
# review_numbered_steps interpolates the same checks into the numbered steps.
_REVIEW_ROLE_TEXT = "## Code Review\nYou are reviewing another worker's code changes."

_REVIEW_CHECKLIST_INTRO = (
    "IMPORTANT: Make minimal changes. Prefer approving the worker's code as-is. "
    "Only fix clear bugs, correctness issues, or scope violations. "
    "Do not refactor, restyle, or improve code that is functionally correct.\n\n"
    "If the user request contains task spec sections, read every path listed under `## Source Files` "
    "before judging the diff. Apply the task spec's `## Reviewer Checklist`. For control-flow or "
    "state-machine tasks, verify the implementation against `## Required Behavior`; do not rely only "
    "on tests.")

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


def review_context_lines(
    user_request: str | None,
    worker_summary: str | None,
    delegator_hint: str,
) -> list[str]:
  """The review prompt's context lines: the extracted evidence, or the fallback, plus the hint.

  The unavailable line appears only when neither the request nor the summary
  was extracted, so the reviewer always knows why the context is thin.
  """
  lines: list[str] = []
  if user_request:
    lines.append(f"**User request:** {user_request}")
  if worker_summary:
    lines.append(f"**Worker summary:** {worker_summary}")
  if not lines:
    lines.append("*(Log extraction unavailable — review based on delegator hint and diff only.)*")
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


def _first_delegation_description(chat_log: pathlib.Path, thread_id: str) -> str | None:
  """First task_delegated description naming *thread_id*, in file order, or None.

  Stops at the first matching event whether its description carries text or
  not; blank and malformed lines are skipped by the shared reader skip
  contract. A missing file means no match, never an error. Thread ids are
  ``str(uuid.uuid4())`` — ASCII, serialized verbatim — so the raw id bytes
  are the needle the containing reader may skip by.
  """
  needle = thread_id.encode("utf-8")
  for event in ndjson.iter_ndjson_events_containing(chat_log, needle, log_event=ndjson.PARSE_SKIP_LOG_EVENT,
                                                    log_fields={}):
    if event.get("type") == ET.TASK_DELEGATED and event.get("thread_id") == thread_id:
      value = event.get("description")
      if isinstance(value, str):
        normalized = value.strip()
        if normalized:
          return normalized
      return None
  return None


def _worker_summary_from_events_log(worker_log: pathlib.Path) -> str | None:
  """The worker's own closing words: the newest non-empty result-or-assistant
  text, whichever kind is newer, or None.

  Streams the log from the end and stops at the first event that settles the
  answer; blank, malformed and missing-file cases follow the shared walk's
  skip contract (None, never an error). Only result and assistant events can
  settle the answer, so the walk parses nothing else — the multi-megabyte
  tool_result lines between the answer and the tail would otherwise parse
  whole.
  """
  summary_types = frozenset({ET.RESULT, ET.ASSISTANT})
  for event in ndjson.iter_ndjson_events_from_end(worker_log, log_event=ndjson.PARSE_SKIP_LOG_EVENT, log_fields={},
                                                  parse_filter=ndjson.type_line_filter(summary_types)):
    ev_type = event.get("type")
    if ev_type == ET.RESULT:
      val = event.get("result")
      if isinstance(val, str):
        stripped = val.strip()
        if stripped:
          return stripped
      # empty / non-string: fall through to look for assistant text
      continue
    if ev_type == ET.ASSISTANT:
      msg = event.get("message") if isinstance(event.get("message"), dict) else None
      text = message_aggregator.extract_text_from_message(msg).strip()
      if text:
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


async def extract_review_context(
    session_id: str,
    thread_id: str,
    sessions_dir: pathlib.Path,
    worker_log_path: pathlib.Path,
) -> tuple[str | None, str | None]:
  """Extract user request and worker summary from JSONL logs for review context.

  Returns (user_request, worker_summary); each may independently be None when extraction fails.
  The caller's prompt builder handles partial context.
  """
  user_request: str | None = None
  worker_summary: str | None = None
  has_user_request = False
  has_worker_summary = False
  session_dir = sessions_dir / session_id

  try:
    chat_log = chat_events.chat_events_path(session_dir)
    description = await asyncio.to_thread(_first_delegation_description, chat_log, thread_id)
    if description is not None:
      user_request = description
      has_user_request = True
  except Exception as e:
    log.warning("review_context_chat_events_failed", session=session_id, thread=thread_id, error=str(e))
  if not has_user_request:
    log.warning("review_context_user_request_unavailable", session=session_id, thread=thread_id)

  try:
    chosen = await asyncio.to_thread(_worker_summary_from_events_log, worker_log_path)
    if chosen:
      worker_summary = chosen
      has_worker_summary = True
  except Exception as e:
    log.warning("review_context_worker_events_failed", session=session_id, thread=thread_id, error=str(e))
  if not has_worker_summary:
    log.warning("review_context_worker_summary_unavailable", session=session_id, thread=thread_id)

  return user_request, worker_summary


def _resolve_preference_option(cfg: config.CharlieBotConfig, option_id: str) -> models.BackendOption:
  """Resolve a backends.preference entry to its BackendOption with default model.

  Raises ValueError if the option_id is not in backends.options or requires but lacks a model.
  """
  option = config.require_backend_option(cfg, option_id, subject="backends.preference entry ")
  return option.model_copy(update={"model": models.option_default_model(option, subject="backends.preference entry ")})


def select_reviewer_backend(
    cfg: config.CharlieBotConfig,
    worker_backend: str,
    worker_model: str | None,
    tried_backends: list[str],
) -> tuple[str, str | None, list[str]] | None:
  """Select a checking-role backend (reviewer, verify default) via backends.preference, skipping already-tried backends.

  Returns (resolved_backend, resolved_model, updated_tried_backends) or None if exhausted.
  """
  resolved_backend, resolved_model = worker_backend, worker_model

  for pref_id in cfg.backends.preference:
    if pref_id == worker_backend:
      log.debug("reviewer_skip_same_backend", preference=pref_id)
      continue
    if pref_id in tried_backends:
      log.debug("reviewer_skip_tried_backend", preference=pref_id)
      continue
    try:
      pref_option = _resolve_preference_option(cfg, pref_id)
      log.info(
          "reviewer_backend_selected",
          preference=pref_id,
          worker_backend=worker_backend,
          reviewer_model=pref_option.model,
          retry_attempt=len(tried_backends),
      )
      resolved_backend = pref_option.id
      resolved_model = pref_option.model
      break
    except Exception as e:
      log.warning("reviewer_preference_failed", preference=pref_id, error=str(e))
  else:
    if cfg.backends.preference:
      if worker_backend not in tried_backends:
        log.info("reviewer_fallback_to_worker_backend", worker_backend=worker_backend, tried=tried_backends)
      else:
        log.warning("reviewer_all_backends_exhausted", tried=tried_backends)
        return None

  return resolved_backend, resolved_model, [*tried_backends, resolved_backend]
