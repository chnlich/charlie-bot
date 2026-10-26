"""Runtime gate for delegation requests.

A delegation (worker spawn) serves an open line that a user request started.
The gate judges that from the task tree alone: the calling node is a manager,
every node from the caller to the root is an open task, and some node on that
chain holds a real user message (``event_types.is_real_user_message``) or a
thread platform summon (``event_types.is_platform_summon``). Agent messages that
are not a summon, scheduled triggers and child reports carry their own event
types and never count. The read-only task types (verify, cold-read) skip the
gate through ``is_read_only_exempt``, the one judgment every exemption site
reads.

A fork/clone child's log opens with its source's copied lines, so a node's user
requests count only after its newest ``clone_start`` marker: the user's words
to the source belong to the source, never the child.

The per-node answer is a forward fold over the event list (a marker clears it,
a real user message or a platform summon sets it), so a memo keyed on the
chat-events cache's list identity carries it across calls and folds only the
appended suffix. A wholesale list replacement (a new object) folds from the
start.
"""

from collections.abc import Callable

from src.infra import event_types, memo, models
from src.runtime import task_errors

# session_id -> (events list, covered length, holds_user_request). Pinning the
# list keeps id() stable, so an identity match can never be an id-reuse
# collision with a different list; the chat-events cache mutates the list only
# by in-place append (save_chat_event) or wholesale replacement, and a
# replacement is a new object.
_user_request_memo: memo.BoundedMemo[str, tuple[list[dict], int, bool]] = memo.BoundedMemo(64)


class DelegationBlockedError(Exception):
  """Raised when the runtime gate rejects a delegation attempt."""


# The task types the read-only exemption covers: verify and cold-read are both
# read-only and repo-less, which is what the contract excuses from the gate.
READ_ONLY_EXEMPT_TYPES = frozenset({models.TaskType.VERIFY, models.TaskType.COLD_READ})


def is_read_only_exempt(task: models.TaskSpec | models.TaskType | None) -> bool:
  """Whether *task* carries the read-only exemption: no user request needed on the chain.

  The one owner of the judgment: the delegation route's admission and
  tree-delegation checks, the agent-creation check on the task tree, and the
  agent-message gate all read this function, so no site keeps its own
  task-type comparison for the exemption. A verify or cold-read delegation is
  read-only and repo-less, which is what the contract excuses from the gate.
  Accepts a task type or a whole task spec (a node's ``task``); no spec or no
  type is not exempt.
  """
  if isinstance(task, models.TaskSpec):
    return task.task_type in READ_ONLY_EXEMPT_TYPES
  return task in READ_ONLY_EXEMPT_TYPES


def _fold_user_request(held: bool, events: list[dict]) -> bool:
  """Fold *events* forward: a ``clone_start`` marker clears the answer, a user request sets it."""
  for event in events:
    if event.get("type") == event_types.CLONE_START:
      held = False
    elif event_types.is_real_user_message(event) or event_types.is_platform_summon(event):
      held = True
  return held


def _holds_user_request(session_id: str, events: list[dict]) -> bool:
  """Whether *events* holds a user request after its newest ``clone_start`` marker.

  A user request is a real user message or a thread platform summon.

  The store claims exactly the folded slice, so an append landing between the
  slice and the store is folded by the next call instead of being claimed
  unseen.
  """
  cached = _user_request_memo.get(session_id)
  covered, held = (cached[1], cached[2]) if cached is not None and cached[0] is events else (0, False)
  suffix = events[covered:]
  held = _fold_user_request(held, suffix)
  _user_request_memo.store(session_id, (events, covered + len(suffix), held))
  return held


def check_takeoff_gate_for_task(
    start_session_id: str,
    *,
    load_events: Callable[[str], list[dict]],
    task_meta_of: Callable[[str], tuple[str | None, str | None]],
    task_state_of: Callable[[str], str],
) -> str:
  """The runtime gate for a task caller: an open manager chain that a user request started.

  Walks from the calling node to the root. The calling node must be a manager
  and every node on the chain an open task. The delegation passes when any
  node on the chain holds a real user message or a thread platform summon.

  Returns the nearest node on the chain that holds a user request; raises
  :class:`DelegationBlockedError` otherwise. ``task_meta_of`` returns
  ``(task_parent_id, profile)`` for one node, or (None, None) when unknown.
  """
  request_node: str | None = None
  current = start_session_id
  for _ in range(task_errors.ANCESTOR_HOP_LIMIT):
    task_parent_id, profile = task_meta_of(current)
    is_start = current == start_session_id
    if is_start and profile != "manager":
      raise DelegationBlockedError(f"task {current} is not a manager; agent calls run only under a manager task")
    state = task_state_of(current)
    if state != "open":
      raise DelegationBlockedError(
          f"Delegation blocked: {'task' if is_start else 'ancestor task'} {current} is {state}; open it first")
    if request_node is None and _holds_user_request(current, load_events(current)):
      request_node = current
    if task_parent_id is None:
      break
    current = task_parent_id
  else:
    raise DelegationBlockedError(
        f"Delegation blocked: the task chain from task {start_session_id} exceeds "
        f"{task_errors.ANCESTOR_HOP_LIMIT} hops without reaching a root")
  if request_node is None:
    raise DelegationBlockedError(
        f"Delegation blocked: no user request on the task chain from task {start_session_id} to its root. "
        "A delegation serves a line that a real user message or a thread platform summon started; other "
        "agent messages, scheduled triggers and child reports do not count.")
  return request_node
