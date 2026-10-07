"""Takeoff authorization gate for delegation requests.

A delegation (worker spawn) may proceed only when the session's chat history
carries either an explicit "take off" in the latest real user message or a
"pre take off" issued within the last 12 hours. This module owns the full
gate — phrase matching, timestamp parsing, and the blocking exception —
extracted from src.runtime.spawner, which consumes none of it. The module also
owns the one verify-exemption judgment (``is_verify_exempt``) that every
takeoff-exemption site reads.

The verdict reads two answers out of the chat history: the takeoff phrase in
the file-last real user message, and the file-last parseable pre-takeoff
stamp. Both are complete prefix facts of the event list, so the answers memo
carries them across calls and an appended suffix folds by scanning only the
suffix: a user message in the suffix is the new file-last one, and the
prefix's stored stamp answer is the file-older bound the backward
continuation would stop at. A wholesale list replacement (a new object)
rebuilds from a fresh walk, the same identity contract the usage fold's memo
rides.
"""

import datetime
from collections.abc import Callable

from src.infra import event_types, log_once, memo, models
from src.runtime import sessions

log = log_once.LazyStructlogLogger()

_PRE_TAKEOFF_PHRASE = "pre take off"
_TAKEOFF_PHRASE = "take off"
_PRE_TAKEOFF_WINDOW = datetime.timedelta(hours=12)

# session_id -> (events list, covered length, latest_user_has_takeoff,
# latest_pre_takeoff_at, seen_any_real_user_message). Pinning the list keeps id() stable, so an identity
# match can never be an id-reuse collision with a different list; the
# chat-events cache mutates the list only by in-place append (save_chat_event)
# or wholesale replacement, and a replacement is a new object. The answers are
# a pure function of the list content, so a list object shared by two
# managers serves one entry safely.
_gate_answers_memo: memo.BoundedMemo[str, tuple[list[dict], int, bool, datetime.datetime | None,
                                                bool]] = memo.BoundedMemo(64)


class DelegationBlockedError(Exception):
  """Raised when the takeoff gate rejects a delegation attempt."""


def is_verify_exempt(task: models.TaskSpec | models.TaskType | None) -> bool:
  """Whether *task* carries the read-only verify exemption: no takeoff window.

  The one owner of the judgment: the delegation route's admission and
  tree-delegation checks, the agent-creation check on the task tree, and the
  Run's actual launch all read this function, so no site keeps its own
  task-type comparison for the exemption. A verify delegation is read-only and
  repo-less, which is what the contract excuses from the window. Accepts a
  task type or a whole task spec (a node's ``task``); no spec or no type is
  not exempt.
  """
  if isinstance(task, models.TaskSpec):
    return task.task_type == models.TaskType.VERIFY
  return task == models.TaskType.VERIFY


def _normalize_takeoff_content(content: str) -> str:
  """Normalize case and consecutive whitespace for authorization phrase matching."""
  return " ".join(content.casefold().split())


def _parse_pre_takeoff_timestamp(event: dict, session_id: str) -> datetime.datetime | None:
  """Parse a pre-takeoff event timestamp as UTC, failing closed when it is invalid."""
  timestamp = event.get("timestamp")
  if not isinstance(timestamp, str) or not timestamp:
    log.warning(
        "pre_takeoff_timestamp_missing",
        session=session_id,
        event_id=event.get("id"),
    )
    return None
  try:
    issued_at = datetime.datetime.fromisoformat(timestamp)
  except ValueError:
    log.warning(
        "pre_takeoff_timestamp_unparseable",
        session=session_id,
        event_id=event.get("id"),
        timestamp=timestamp,
    )
    return None
  if issued_at.tzinfo is None:
    log.warning(
        "pre_takeoff_timestamp_timezone_missing",
        session=session_id,
        event_id=event.get("id"),
        timestamp=timestamp,
    )
    return None
  return issued_at.astimezone(datetime.UTC)


def _backward_user_answers(
    events: list[dict],
    session_id: str,
) -> tuple[bool, datetime.datetime | None, bool]:
  """Backward walk over the given span: the span-last real user message's
  takeoff phrase, the span-last parseable pre-takeoff stamp, and whether the
  span held a real user message at all.

  No early break. The stored answers must be complete prefix facts for the
  suffix fold to combine with, and an early break could fire with the stamp
  answer still unset behind a takeoff-allowed verdict — the one under-fill
  walking on removes. Past a settled stamp a break would change nothing
  (both answers settle, and a file-older message cannot overwrite either),
  so walking on only fills that corner and never flips a verdict.
  """
  latest_user_has_takeoff = False
  latest_pre_takeoff_at: datetime.datetime | None = None
  seen_latest_user = False
  for event in reversed(events):
    if not event_types.is_real_user_message(event):
      continue
    normalized = _normalize_takeoff_content(event.get("content"))
    if not seen_latest_user:
      seen_latest_user = True
      latest_user_has_takeoff = _TAKEOFF_PHRASE in normalized
    if latest_pre_takeoff_at is None and _PRE_TAKEOFF_PHRASE in normalized:
      issued_at = _parse_pre_takeoff_timestamp(event, session_id)
      if issued_at is not None:
        latest_pre_takeoff_at = issued_at
  return latest_user_has_takeoff, latest_pre_takeoff_at, seen_latest_user


def _settled_user_answers(
    events: list[dict],
    session_id: str,
) -> tuple[bool, datetime.datetime | None, bool]:
  """Return the two gate answers plus whether *events* holds any real user message.

  A cold or replaced list pays one full backward walk and stores the answers
  with the list and its length. An identity match folds only the appended
  suffix — the chat-events cache grows the list in place, so the answers as
  of the covered length stay valid and the suffix holds the file-last user
  message when it holds one at all; the store claims exactly the scanned
  span, so an append landing between the slice and the store is scanned by
  the next call instead of being claimed unseen.
  """
  cached = _gate_answers_memo.get(session_id)
  if cached is not None and cached[0] is events:
    covered, has_takeoff, pre_takeoff_at, seen_any_user = cached[1], cached[2], cached[3], cached[4]
    suffix = events[covered:]
    if suffix:
      suffix_has_takeoff, suffix_pre_takeoff_at, seen_user = _backward_user_answers(suffix, session_id)
      if seen_user:
        has_takeoff = suffix_has_takeoff
        seen_any_user = True
      if suffix_pre_takeoff_at is not None:
        pre_takeoff_at = suffix_pre_takeoff_at
      _gate_answers_memo.store(session_id, (events, covered + len(suffix), has_takeoff, pre_takeoff_at, seen_any_user))
    return has_takeoff, pre_takeoff_at, seen_any_user
  count = len(events)
  span = events[:count]  # the walked span is the claimed span: an append landing mid-walk is not claimed unseen
  has_takeoff, pre_takeoff_at, seen_any_user = _backward_user_answers(span, session_id)
  _gate_answers_memo.store(session_id, (events, count, has_takeoff, pre_takeoff_at, seen_any_user))
  return has_takeoff, pre_takeoff_at, seen_any_user


def _effective_utc_now(now: datetime.datetime | None) -> datetime.datetime:
  """The gate's wall clock in UTC; a naive *now* is rejected, never guessed."""
  effective = now if now is not None else datetime.datetime.now(datetime.UTC)
  if effective.tzinfo is None:
    raise ValueError("authorization check time must be timezone-aware")
  return effective.astimezone(datetime.UTC)


def _authorization_window_open(
    has_takeoff: bool, pre_takeoff_at: datetime.datetime | None, effective_now: datetime.datetime) -> bool:
  """The one window verdict both gates apply: the latest real user message
  carries "take off", or a "pre take off" stamp still sits inside the window."""
  pre_takeoff_active = (
      pre_takeoff_at is not None and pre_takeoff_at <= effective_now < pre_takeoff_at + _PRE_TAKEOFF_WINDOW)
  return has_takeoff or pre_takeoff_active


def _delegation_blocked(*, task_id: str | None) -> DelegationBlockedError:
  """The shared no-authorization verdict; the task gate names the session it judged."""
  scope = "" if task_id is None else f" of task {task_id}"
  return DelegationBlockedError(
      'Delegation blocked: no active authorization. A valid "pre take off" within 12 hours or '
      f'"take off" in the latest real user message{scope} is required before delegating.')


def check_takeoff_gate(
    session_id: str,
    session_mgr: sessions.SessionManager,
    now: datetime.datetime | None = None,
) -> None:
  """Verify an active pre-takeoff or ordinary takeoff authorization window."""
  effective_now = _effective_utc_now(now)

  events = session_mgr.load_chat_events_sync(session_id)
  latest_user_has_takeoff, latest_pre_takeoff_at, _ = _settled_user_answers(events, session_id)

  if _authorization_window_open(latest_user_has_takeoff, latest_pre_takeoff_at, effective_now):
    return

  raise _delegation_blocked(task_id=None)


_TASK_ANCESTOR_HOP_LIMIT = 1000


def check_takeoff_gate_for_task(
    start_session_id: str,
    *,
    load_events: Callable[[str], list[dict]],
    task_meta_of: Callable[[str], tuple[str | None, str | None]],
    task_state_of: Callable[[str], str],
    now: datetime.datetime | None = None,
) -> str:
  """The v2 task-caller gate: nearest-real-user-ancestor lookup over the task tree.

  From the calling node upward, the first node whose chat history carries a
  real user instruction is where the existing gate applies; a failed gate
  there blocks — the walk never borrows from a higher ancestor past a node
  that holds a real user message. Every ancestor on the way must be an open
  task, and the calling node itself must be a manager. Agent messages, cron
  inputs, and child reports never mint or revoke a user authorization window;
  only real user messages (the same judgment the legacy gate rides) count.

  Returns the session id whose gate authorized; raises
  :class:`DelegationBlockedError` otherwise. ``task_meta_of`` returns
  ``(task_parent_id, profile)`` for one node, or (None, None) when unknown.
  """
  effective_now = _effective_utc_now(now)

  current = start_session_id
  for _ in range(_TASK_ANCESTOR_HOP_LIMIT):
    task_parent_id, profile = task_meta_of(current)
    is_start = current == start_session_id
    if is_start and profile != "manager":
      raise DelegationBlockedError(f"task {current} is not a manager; agent calls run only under a manager task")
    if task_state_of(current) != "open":
      raise DelegationBlockedError(
          f"{'ancestor task' if not is_start else 'task'} {current} is "
          f"{task_state_of(current)}; open it first")

    events = load_events(current)
    has_takeoff, pre_takeoff_at, seen_user = _settled_user_answers(events, current)
    if seen_user:
      # The nearest node with a real user instruction: apply the existing gate
      # here and never borrow past it (a local instruction blocks higher ones).
      if _authorization_window_open(has_takeoff, pre_takeoff_at, effective_now):
        return current
      raise _delegation_blocked(task_id=current)
    if task_parent_id is None:
      break
    current = task_parent_id
  raise DelegationBlockedError(
      "Delegation blocked: no real user instruction found along the task ancestor chain; "
      "authorization cannot be borrowed past the root.")
