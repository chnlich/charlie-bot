"""Turn contributions: how a feature package adds to a master turn.

A master turn runs one path for every session: it builds the instruction segments, prepares state before the turn,
picks the context window, renders chat events, and reacts when the turn ends. A feature package that takes part in
one of these steps registers a ``TurnContribution`` from its ``register()`` function (the packages listed in
``src/app/registrations.py``). The runtime asks every registered contribution at each step and names no feature.

Vocabulary:

- A *contribution* is one ``TurnContribution`` instance. Each method is one step and has a default that adds nothing.
- The *workflow rules file* is the file under ``prompts/`` that follows ``master.md`` in a manager's instructions.
  ``DEFAULT_WORKFLOW_RULES_FILE`` serves a session that no contribution names a file for.

A registration holds a "module:attr" string, and the attr is the contribution instance. ``turn_contributions()``
imports it on first use, so a registration costs no feature import and this module imports no feature module.
"""

from __future__ import annotations

import importlib
from collections.abc import Callable, Mapping
from typing import TYPE_CHECKING, TypeVar

if TYPE_CHECKING:
  from src.infra.config import CharlieBotConfig
  from src.infra.models import SessionMetadata
  from src.runtime.sessions import SessionManager
  from src.runtime.task_prompts import RuleSegment

DEFAULT_WORKFLOW_RULES_FILE = "manager_workflows.md"

_Answer = TypeVar("_Answer")


class TurnContribution:
  """One feature package's part in a master turn; subclasses override the steps they take part in."""

  def instruction_segments(self, meta: SessionMetadata, kind: str, cfg: CharlieBotConfig) -> list[RuleSegment]:
    """Segments for the managed instructions of a run of *kind*.

    They follow the model overlay segments and precede the local rule segments, in registration order.
    """
    return []

  def workflow_rules_file(self, meta: SessionMetadata) -> str | None:
    """The name of the workflow rules file for this session, or None to name none.

    At most one contribution answers non-None for one session.
    """
    return None

  async def before_turn(self, meta: SessionMetadata, cfg: CharlieBotConfig) -> None:
    """Runs when a turn's input is queued, before the user event is appended."""
    return

  async def after_turn(
      self, meta: SessionMetadata, done_event: dict, *, cfg: CharlieBotConfig, sessions: SessionManager) -> None:
    """Runs as its own logged task after ``done_event`` (a MASTER_DONE) is appended and broadcast.

    ``sessions`` appends further events to the session.
    """
    return

  def context_window(self, meta: SessionMetadata) -> int | None:
    """The context window in tokens for this session, or None to keep the backend option's own.

    At most one contribution answers non-None for one session. A backend that reads no context window ignores it.
    """
    return None

  def event_renderers(self) -> Mapping[str, Callable[[dict], dict]]:
    """Chat renderers by event type: each takes the persisted event and returns the message dict.

    An event type that the aggregator or another contribution already renders raises ValueError there.
    """
    return {}


_registered: dict[str, str] = {}
_instances: dict[str, TurnContribution] = {}


def register_turn_contribution(name: str, contribution: str) -> None:
  """Register one contribution under *name*; *contribution* is "module:attr" naming a ``TurnContribution`` instance.

  A second registration of one name raises ValueError.
  """
  if name in _registered:
    raise ValueError(f"turn contribution {name!r} is already registered by {_registered[name]}")
  _registered[name] = contribution


def _import_contribution(name: str, path: str) -> TurnContribution:
  module_name, separator, attr = path.partition(":")
  if not separator or not module_name or not attr:
    raise ValueError(f"turn contribution {name!r}: {path!r} is not a 'module:attr' string")
  instance = getattr(importlib.import_module(module_name), attr)
  if not isinstance(instance, TurnContribution):
    raise ValueError(f"turn contribution {name!r}: {path} is a {type(instance).__name__}, not a TurnContribution")
  return instance


def turn_contributions() -> tuple[TurnContribution, ...]:
  """Every registered contribution in registration order; imports each on first use."""
  for name, path in _registered.items():
    if name not in _instances:
      _instances[name] = _import_contribution(name, path)
  return tuple(_instances[name] for name in _registered)


def _the_one_answer(question: str, answers: list[tuple[TurnContribution, _Answer | None]]) -> _Answer | None:
  given = [(type(contribution).__name__, answer) for contribution, answer in answers if answer is not None]
  if len(given) > 1:
    raise ValueError(
        f"{question}: more than one turn contribution answers: " +
        ", ".join(f"{owner} -> {answer!r}" for owner, answer in given))
  return given[0][1] if given else None


def resolve_workflow_rules_file(meta: SessionMetadata) -> str:
  """The workflow rules file for *meta*: the one a contribution names, else ``DEFAULT_WORKFLOW_RULES_FILE``.

  Two contributions that name a file raise ValueError.
  """
  named = _the_one_answer(
      "workflow rules file",
      [(contribution, contribution.workflow_rules_file(meta)) for contribution in turn_contributions()])
  return DEFAULT_WORKFLOW_RULES_FILE if named is None else named


def resolve_context_window(meta: SessionMetadata) -> int | None:
  """The context window a contribution sets for *meta*, or None when none does.

  Two contributions that set one raise ValueError.
  """
  return _the_one_answer(
      "context window", [(contribution, contribution.context_window(meta)) for contribution in turn_contributions()])
