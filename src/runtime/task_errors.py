"""The task tree's error types, the not-found and relation-cycle refusals, the operator check and the walk bounds.

The task-tree modules (``task_sessions``, ``session_dispatch``, ``task_completion``)
and the routes that translate their refusals share these names, so they live below
all of them and import only ``run_token``.
"""

from src.runtime import run_token

# Bound on the ancestor walk: open-ancestor checks and ancestor paths must
# never spin on a corrupted relation.
ANCESTOR_HOP_LIMIT = 1000

# The restore chain walks the same relation; it shares the bound.
RESTORE_CHAIN_HOP_LIMIT = ANCESTOR_HOP_LIMIT

# Bound on the non-archived descendants one subtree walk may hold; archived ones do not count.
DESCENDANT_NODE_LIMIT = 1000


class TaskInvalidError(ValueError):
  """Empty target or illegal relation (API: 400)."""


class TaskNotFoundError(LookupError):
  """The referenced task/Run does not exist (API: 404)."""


class TaskForbiddenError(PermissionError):
  """The caller's identity or role does not allow the operation (API: 403)."""


class TaskConflictError(Exception):
  """Concurrent change or lifecycle conflict with concrete blockers (API: 409)."""

  def __init__(self, blockers: list[str]) -> None:
    self.blockers = blockers
    super().__init__("; ".join(blockers))


class TaskArchivedError(TaskConflictError):
  """The target task node is archived — it accepts no machine input (API: 409).

  The refusal sentence is the API's whole 409 detail (the sender reads exactly
  ``task <id> is archived``), not the blockers-dict shape a plain
  TaskConflictError maps to.
  """

  def __init__(self, session_id: str) -> None:
    self.session_id = session_id
    super().__init__([f"task {session_id} is archived"])


def task_not_found(session_id: str) -> TaskNotFoundError:
  """The refusal for a task-tree node that does not exist; the API reports its sentence as the 404 detail."""
  return TaskNotFoundError(f"task {session_id} not found")


def task_relation_cycle(session_id: str) -> TaskConflictError:
  """The refusal for a task-parent relation that loops; the API reports its sentence among the 409 blockers."""
  return TaskConflictError([f"task relation cycle through {session_id}"])


def require_operator(caller: object, message: str) -> None:
  """Refuse *caller* with TaskForbiddenError(*message*) unless it carries operator credentials."""
  if not isinstance(caller, run_token.CallerIdentity) or not caller.is_operator:
    raise TaskForbiddenError(message)
