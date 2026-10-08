"""The task tree's error types, the operator check and the ancestor walk bounds.

The task-tree modules (``task_sessions``, ``session_dispatch``, ``task_completion``)
and the routes that translate their refusals share these names, so they live below
all of them and import only ``run_token``.
"""

from src.runtime.run_token import CallerIdentity

# Bound on the ancestor walk: open-ancestor checks and ancestor paths must
# never spin on a corrupted relation.
ANCESTOR_HOP_LIMIT = 1000

# The restore chain walks the same relation; it shares the bound.
RESTORE_CHAIN_HOP_LIMIT = ANCESTOR_HOP_LIMIT


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


def require_operator(caller: object, message: str) -> None:
  """Refuse *caller* with TaskForbiddenError(*message*) unless it carries operator credentials."""
  if not isinstance(caller, CallerIdentity) or not caller.is_operator:
    raise TaskForbiddenError(message)
