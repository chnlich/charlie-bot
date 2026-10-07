"""Iterative /improve loop orchestrator."""

from __future__ import annotations

import asyncio
import json
import os
import pathlib
import re
from collections.abc import Iterator

import pydantic

from src.backends.claude_code import claude_relay
from src.infra import config, git, log_once, models, timeouts
from src.infra import event_types as ET
from src.runtime import message_aggregator

log = log_once.LazyStructlogLogger()

# Shared contract strings: the v2 controller (improve_sequence) reproduces this
# module's iteration description, fallback report, and failure payloads verbatim,
# and tests pin the exact strings on both paths.
ITERATION_SUMMARIES_HEADING = "Previous iteration summaries:\n"
RUNNER_FALLBACK_REPORT_MARKER = "<!-- runner fallback: worker wrote no report -->\n"
WORKTREE_CREATE_ERROR_PREFIX = "Failed to create worktree: "
LOOP_FAILURE_ERROR_PREFIX = "Improve loop failed: "

# The first matching substring names the blocker, so an entry contained in an
# earlier one can never be named: keep the shorter form ("out of token" covers
# "out of tokens").
_QUOTA_BLOCKER_TEXT_PATTERNS = (
    "quota exhausted",
    "quota exceeded",
    "insufficient quota",
    "over quota",
    "rate limit exceeded",
    "rate limit reached",
    "rate limit rejected",
    "rate-limit exceeded",
    "rate-limit reached",
    "rate-limit rejected",
    "rate_limited",
    "rate_limit_error",
    "rate limited",
    "too many requests",
    "resource_exhausted",
    "429",
    "out of token",
    "out-of-token",
    "insufficient tokens",
    "tokens exhausted",
    # The pre-spawn pool exhaustion (claude_relay.pool_exhausted_message rides
    # the run's events-log error event): one home for the phrase, so the
    # classification and the message cannot drift apart.
    claude_relay.POOL_EXHAUSTED_PHRASE.lower(),
)

# ---------------------------------------------------------------------------
# State models
# ---------------------------------------------------------------------------


class ImproveState(pydantic.BaseModel):
  loop_id: int
  goal: str
  # running | stopped | completed | failed | blocked | interrupted.
  # "blocked" is the v2 sequence's exhausted-without-proven-delivery verdict;
  # "interrupted" is a controller that died with an old server process (the
  # restart never resumes the loop — see src.features.improve.improve_sequence).
  status: str = "running"
  work_branch: str
  base_branch: str | None = None
  repo_path: str
  merge_back: bool = False
  backend: str | None = None
  model: str | None = None
  created_at: str
  # The server process that owns this loop's controller. A restart finds a
  # "running" state stamped with a dead pid and marks it interrupted instead
  # of leaving a permanently blocking active lock (src.features.improve.improve_sequence).
  server_pid: int | None = None


class ImproveLoopAlreadyRunningError(RuntimeError):
  """Raised when a session already has an active improve loop."""

  def __init__(self, loop_id: int | None) -> None:
    self.loop_id = loop_id
    if loop_id is None:
      super().__init__("An improve loop is already running for this session. Use charliebot improve-stop first.")
      return
    super().__init__(f"Loop {loop_id} is already running for this session. Use charliebot improve-stop first.")


# ---------------------------------------------------------------------------
# State persistence
# ---------------------------------------------------------------------------


def _loops_dir(session_id: str, cfg: config.CharlieBotConfig) -> pathlib.Path:
  return cfg.sessions_dir / session_id / "loops"


def _active_loop_path(session_id: str, cfg: config.CharlieBotConfig) -> pathlib.Path:
  return _loops_dir(session_id, cfg) / "active.lock"


def _loop_state_path(session_id: str, loop_id: int, cfg: config.CharlieBotConfig) -> pathlib.Path:
  return _loops_dir(session_id, cfg) / str(loop_id) / "state.json"


def _goal_file_path(loop_dir: pathlib.Path) -> pathlib.Path:
  return loop_dir / "goal.md"


def _plan_file_path(loop_dir: pathlib.Path) -> pathlib.Path:
  return loop_dir / "plan.md"


def loop_goal_path(session_id: str, loop_id: int, cfg: config.CharlieBotConfig) -> pathlib.Path:
  """Path to the live goal file for a loop (the editable per-iteration goal)."""
  return _goal_file_path(_loops_dir(session_id, cfg) / str(loop_id))


def loop_plan_path(session_id: str, loop_id: int, cfg: config.CharlieBotConfig) -> pathlib.Path:
  """Path to the optional live plan file for a loop."""
  return _plan_file_path(_loops_dir(session_id, cfg) / str(loop_id))


async def read_loop_goal(loop_dir: pathlib.Path) -> str:
  """Read the live goal for a loop, failing loudly if goal.md is missing.

  The goal file is re-read at the start of every iteration so the user can steer
  a running loop by editing it. A missing file mid-loop is a hard failure — there
  is deliberately no fallback to the state.json startup snapshot.
  """
  goal_path = _goal_file_path(loop_dir)
  if not await asyncio.to_thread(goal_path.exists):
    raise RuntimeError(f"improve loop goal file missing: {goal_path}")
  return await asyncio.to_thread(goal_path.read_text)


async def read_loop_plan(loop_dir: pathlib.Path) -> str | None:
  """Read the optional live plan for a loop, returning None when absent."""
  plan_path = _plan_file_path(loop_dir)
  if not await asyncio.to_thread(plan_path.exists):
    return None
  return await asyncio.to_thread(plan_path.read_text)


def _iter_numeric_loop_dirs(loops_dir: pathlib.Path) -> Iterator[tuple[pathlib.Path, int]]:
  # Yields the child path itself, not loops_dir / str(loop_id): zero-padded
  # names ("007") resolve to a different directory through str(int).
  for child in loops_dir.iterdir():
    if not child.is_dir():
      continue
    try:
      yield child, int(child.name)
    except ValueError:
      continue


def _next_loop_id_sync(loops_dir: pathlib.Path) -> int:
  if not loops_dir.exists():
    return 1

  max_loop_id = 0
  for _, loop_id in _iter_numeric_loop_dirs(loops_dir):
    max_loop_id = max(max_loop_id, loop_id)
  return max_loop_id + 1 if max_loop_id else 1


def _find_state_loop_ids_sync(loops_dir: pathlib.Path) -> list[int]:
  if not loops_dir.exists():
    return []

  return [loop_id for child, loop_id in _iter_numeric_loop_dirs(loops_dir) if (child / "state.json").exists()]


def _create_empty_file_exclusive(path: pathlib.Path) -> None:
  with path.open("x"):
    pass


async def load_loop_state(session_id: str, loop_id: int, cfg: config.CharlieBotConfig) -> ImproveState | None:
  """Read the loop state file, returning None if it is missing."""
  path = _loop_state_path(session_id, loop_id, cfg)
  if not await asyncio.to_thread(path.exists):
    return None
  return ImproveState.model_validate_json(await asyncio.to_thread(path.read_text))


async def require_loop_state(session_id: str, loop_id: int, cfg: config.CharlieBotConfig) -> ImproveState:
  """Read the loop state file, raising RuntimeError if it is missing."""
  state = await load_loop_state(session_id, loop_id, cfg)
  if state is None:
    raise RuntimeError(f"missing loop state for session {session_id} loop {loop_id}")
  return state


async def save_loop_state(session_id: str, state: ImproveState, cfg: config.CharlieBotConfig) -> None:
  """Write the loop state file."""
  path = _loop_state_path(session_id, state.loop_id, cfg)
  await asyncio.to_thread(path.parent.mkdir, parents=True, exist_ok=True)
  await asyncio.to_thread(path.write_text, state.model_dump_json(indent=2))


async def clear_active_loop_lock(session_id: str, cfg: config.CharlieBotConfig) -> None:
  """Remove the session's active loop lock file, if present."""
  active_path = _active_loop_path(session_id, cfg)
  if await asyncio.to_thread(active_path.exists):
    await asyncio.to_thread(active_path.unlink)


async def next_loop_id(session_id: str, cfg: config.CharlieBotConfig) -> int:
  """Return the next sequential loop id for a session."""
  loops_dir = _loops_dir(session_id, cfg)
  return await asyncio.to_thread(_next_loop_id_sync, loops_dir)


async def _reserve_loop_dir(session_id: str, cfg: config.CharlieBotConfig) -> tuple[int, pathlib.Path]:
  """Create a unique per-loop directory for this session."""
  loops_dir = _loops_dir(session_id, cfg)
  await asyncio.to_thread(loops_dir.mkdir, parents=True, exist_ok=True)

  while True:
    loop_id = await next_loop_id(session_id, cfg)
    loop_dir = loops_dir / str(loop_id)
    try:
      await asyncio.to_thread(loop_dir.mkdir)
    except FileExistsError:
      continue
    return loop_id, loop_dir


async def find_running_loop(session_id: str, cfg: config.CharlieBotConfig) -> ImproveState | None:
  """Return the running loop for a session, if any."""
  loops_dir = _loops_dir(session_id, cfg)
  state_loop_ids = await asyncio.to_thread(_find_state_loop_ids_sync, loops_dir)
  for loop_id in sorted(state_loop_ids):
    state = await load_loop_state(session_id, loop_id, cfg)
    if state is not None and state.status == "running":
      return state
  return None


# ---------------------------------------------------------------------------
# Stop signal
# ---------------------------------------------------------------------------


async def stop_improve_loop(session_id: str, cfg: config.CharlieBotConfig) -> bool:
  """Set improve loop status to stopped. Returns True if there was an active loop."""
  state = await find_running_loop(session_id, cfg)
  if state is None:
    return False
  state.status = "stopped"
  await save_loop_state(session_id, state, cfg)
  return True


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _commits_section_first_none(text: str) -> bool:
  """Return True when the report's `### Commits` section lists `none` first."""
  lines = text.splitlines()
  for idx, line in enumerate(lines):
    if line.strip().startswith("### Commits"):
      for entry in lines[idx + 1:]:
        stripped = entry.strip()
        if not stripped:
          continue
        if stripped.startswith("- "):
          return re.match(r"^none\b", stripped[2:].strip()) is not None
        break
      break
  return False


async def _iter_report_validity(
    report_path: pathlib.Path,
    iteration: int,
    commits_added: int,
) -> tuple[bool, str | None]:
  """Mechanically judge an iteration's report validity.

  Valid ⇔ the report exists, contains a `## Iter <n>` heading (em dash or
  hyphen tolerated), and either commits were added or the report carries an
  explicit zero-progress verdict (`### Commits` with `- none — <verdict>`).
  Returns (valid, invalid_reason), invalid_reason in
  {missing_report, malformed_report, no_commit_no_verdict} or None when valid.
  """
  if not await asyncio.to_thread(report_path.exists):
    return False, "missing_report"
  text = await asyncio.to_thread(report_path.read_text)
  if not re.search(rf"^## Iter {iteration}\b", text, flags=re.MULTILINE):
    return False, "malformed_report"
  if commits_added > 0 or _commits_section_first_none(text):
    return True, None
  return False, "no_commit_no_verdict"


def _invalid_iteration_summary(
    iteration: int,
    reason: str,
    commits_added: int,
    tip_before: str,
    tip_after: str,
    report_path: pathlib.Path,
) -> str:
  """Build the fixed single-line placeholder for an invalid iteration."""
  return (
      f"Iteration {iteration} INVALID ({reason}); commits_added={commits_added}; "
      f"tip {tip_before}..{tip_after}; report: {report_path}")


async def _worktree_commit_delta(wt_path: pathlib.Path, tip_before: str) -> tuple[str, int, str]:
  """Compute (tip_after, commits_added, diffstat) for the iteration's commits."""
  tip_after = await git._git_rev_parse(wt_path, "HEAD") or ""
  commits_added = 0
  diffstat = ""
  if tip_before and tip_after:
    ok, out, _ = await git._git_stdout(
        wt_path,
        "rev-list",
        "--count",
        f"{tip_before}..{tip_after}",
        timeout=timeouts.SUBPROCESS_GIT_READ_TIMEOUT_ASYNC,
        timeout_label="git rev-list --count",
    )
    if ok and out.isdigit():
      commits_added = int(out)
    if commits_added > 0:
      ok, ds, _ = await git._git_stdout(
          wt_path,
          "diff",
          "--shortstat",
          f"{tip_before}..{tip_after}",
          timeout=timeouts.SUBPROCESS_GIT_READ_TIMEOUT_ASYNC,
          timeout_label="git diff --shortstat",
      )
      if ok:
        diffstat = ds
  return tip_after, commits_added, diffstat


def _summary_text(event: dict) -> str | None:
  """The summary text *event* carries, or None — the one definition of a
  summary-bearing event, shared by the single-judgment scan and the fused
  failed-iteration pass.
  """
  if event.get("type") == ET.RESULT and event.get("result"):
    return event["result"][:500]
  if event.get("type") == ET.ASSISTANT:
    text = message_aggregator.extract_text_from_message(event.get("message"))
    if text:
      return text[:500]
  return None


def _extract_iteration_summary(events_newest_first: Iterator[dict], iteration: int, status: str) -> str:
  """Extract a summary from worker events: newest-first scan, the first event
  carrying a result text or assistant text wins. *events_newest_first* is the
  from-the-end walk's stream (:func:`_newest_first_events`), consumed to the
  first answer.
  """
  for event in events_newest_first:
    text = _summary_text(event)
    if text is not None:
      return text
  return f"Iteration {iteration} {status} (no summary available)."


def _build_summary_payload(payload_type: str, goal: str, summaries: list[str]) -> dict:
  """Build the shared JSON structure for completion/failure broadcasts."""
  return {
      "type": payload_type,
      "goal": goal,
      "iterations_completed": len(summaries),
      "summaries": summaries,
  }


async def _land_work_branch_after_loop(
    resolved_repo: pathlib.Path,
    work_branch: str,
    base_branch: str | None,
    merge_back: bool,
    stopped_by_user: bool,
    previous_summaries: list[str],
    session_id: str,
) -> dict | None:
  """Decide how to land the work branch after the iteration loop finishes.

  Fast-forwards work_branch onto base_branch when merge_back is set and the loop
  produced summaries without being stopped. On FF push failure, falls back to
  pushing the bare work branch so the master agent can land it manually (PR,
  manual rebase, etc.). Returns the merge_result dict to attach to the payload,
  or None when no landing decision was recorded (best-effort branch push only).
  """
  # Worktree was created from origin/<base>, so the FF push only fails if origin/<base>
  # advanced during the loop. On failure, hand the work branch back to the master agent
  # via the trigger payload — no rebase-retry, no fallback subagent — so it can decide
  # how to land (e.g. open a PR, manual rebase).
  if merge_back and not stopped_by_user and previous_summaries:
    ok, push_err = await git.git_push_refspec(resolved_repo, work_branch, base_branch)
    if ok:
      return {'merged': True, 'base_branch': base_branch}
    log.warning("improve_loop_landing_ff_push_failed", session=session_id, error=push_err)
    # Keep the work branch on origin so the master agent can act on it.
    ok_push, push_branch_err = await git.git_push_branch(resolved_repo, work_branch)
    if not ok_push:
      log.warning("improve_loop_work_branch_push_failed", session=session_id, error=push_branch_err)
    return {
        'merged': False,
        'error': push_err,
        'work_branch': work_branch,
        'base_branch': base_branch,
    }
  # Best-effort push work_branch to remote
  ok, push_err = await git.git_push_branch(resolved_repo, work_branch)
  if not ok:
    log.warning("improve_loop_push_failed", session=session_id, error=push_err)
  return None


def blocked_loop_summary(iteration: int, reason: str) -> str:
  """The reader-facing blocked-loop sentence: what blocked, and the decision left to the reader.

  Both launch paths compose through this one definition -- the v1 loop's failed
  payload and the v2 sequence's failed summary -- so the instruction to the
  reader cannot fork between them.
  """
  return (
      f"Improve loop blocked on iteration {iteration}: {reason}. "
      "No further iterations were spawned; decide whether to wait, switch backend, or relaunch.")


async def reserve_loop_state(
    session_id: str,
    goal: str,
    work_branch: str,
    repo_path: str,
    cfg: config.CharlieBotConfig,
    *,
    plan: str | None = None,
    base_branch: str | None = None,
    merge_back: bool = False,
    resolved_backend: str = "",
    resolved_model: str = "",
) -> ImproveState:
  """Reserve a unique loop id and persist running state before background work begins."""
  loops_dir = _loops_dir(session_id, cfg)
  await asyncio.to_thread(loops_dir.mkdir, parents=True, exist_ok=True)
  active_path = _active_loop_path(session_id, cfg)

  try:
    await asyncio.to_thread(_create_empty_file_exclusive, active_path)
  except FileExistsError as exc:
    running = await find_running_loop(session_id, cfg)
    raise ImproveLoopAlreadyRunningError(running.loop_id if running else None) from exc

  try:
    loop_id, loop_dir = await _reserve_loop_dir(session_id, cfg)
    # Write the live goal exactly once at reservation. Iterations re-read this
    # file; state.json's goal stays as the startup snapshot for display/summary.
    await asyncio.to_thread(_goal_file_path(loop_dir).write_text, goal)
    if plan is not None:
      await asyncio.to_thread(_plan_file_path(loop_dir).write_text, plan)
    state = ImproveState(
        loop_id=loop_id,
        goal=goal,
        status="running",
        work_branch=work_branch,
        base_branch=base_branch,
        repo_path=str(pathlib.Path(repo_path).resolve()),
        merge_back=merge_back,
        backend=resolved_backend or None,
        model=resolved_model or None,
        created_at=models.utc_now_iso(),
        server_pid=os.getpid(),
    )
    await save_loop_state(session_id, state, cfg)
    await asyncio.to_thread(active_path.write_text, f"{loop_id}\n")
    return state
  except Exception:
    await clear_active_loop_lock(session_id, cfg)
    raise


# ---------------------------------------------------------------------------
# Quota/token/rate-limit blocker helpers
# ---------------------------------------------------------------------------


def _event_text(event: dict) -> str:
  parts: list[str] = []
  for key in ("message", "content", "result", "error", "api_error_status"):
    value = event.get(key)
    if value is None:
      continue
    if key == "message" and isinstance(value, dict):
      text = message_aggregator.extract_text_from_message(value)
      if text:
        parts.append(text)
      continue
    if isinstance(value, (dict, list)):
      parts.append(json.dumps(value, default=str))
    else:
      parts.append(str(value))
  return "\n".join(parts)


def _quota_blocker_match(ev: dict) -> str | None:
  """The quota blocker *ev* names, or None when the event carries none — the
  one definition of the quota-shaped event, shared by the fused failed-iteration
  pass's two judgments.
  """
  event_type = ev.get('type')
  if event_type == ET.RATE_LIMIT_EVENT:
    rli = ev.get(ET.RATE_LIMIT_INFO, {})
    status = str(rli.get('status', '')).lower()
    overage_status = str(rli.get('overageStatus', '')).lower()
    if status == 'rejected' or overage_status == 'rejected':
      rate_type = rli.get('rateLimitType') or 'rate limit'
      return f"rate-limit rejection ({rate_type})"

  if event_type not in (ET.ERROR, ET.ASSISTANT_ERROR, ET.RESULT):
    return None
  if (event_type == ET.RESULT and ev.get('is_error') is not True and ev.get('api_error_status') is None and
      'error' not in str(ev.get('subtype', '')).lower()):
    return None

  text = _event_text(ev).lower()
  for pattern in _QUOTA_BLOCKER_TEXT_PATTERNS:
    if pattern in text:
      return f"provider quota/token/rate-limit rejection ({pattern})"
  return None


def _failed_iteration_judgments(events_newest_first: Iterator[dict], iteration: int,
                                status: str) -> tuple[str | None, str]:
  """Both failed-iteration judgments from one newest-first pass: the blocker at
  the first quota-shaped event or exhaustion, the summary at the first
  result/assistant text. The walk stops once both are settled, so a no-match
  exhaustion parses the log once.
  """
  blocker_reason = None
  summary = None
  for ev in events_newest_first:
    if blocker_reason is None:
      blocker_reason = _quota_blocker_match(ev)
    if summary is None:
      summary = _summary_text(ev)
    if blocker_reason is not None and summary is not None:
      break
  if summary is None:
    summary = f"Iteration {iteration} {status} (no summary available)."
  return blocker_reason, summary


# ---------------------------------------------------------------------------
# Single-iteration helper
# ---------------------------------------------------------------------------


def _newest_first_events(events_path: pathlib.Path) -> Iterator[dict]:
  """The iteration thread's events log, newest line first — the stream both
  iteration judgments scan (the from-the-end walk parses only the bytes the
  answer needs)."""
  from src.infra import ndjson

  # Both judgments match on these five types alone (_quota_blocker_match,
  # _summary_text), so the walk parses nothing else — the multi-megabyte
  # tool_result lines a no-match exhaustion would otherwise parse whole.
  candidate_types = frozenset({ET.RESULT, ET.ASSISTANT, ET.ASSISTANT_ERROR, ET.ERROR, ET.RATE_LIMIT_EVENT})
  return ndjson.iter_ndjson_events_from_end(
      events_path,
      log_event=ndjson.PARSE_SKIP_LOG_EVENT,
      log_fields={},
      parse_filter=ndjson.type_line_filter(candidate_types))
