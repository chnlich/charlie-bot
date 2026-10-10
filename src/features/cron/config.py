"""Cron task models: one cron.d file body is a ``ScheduledTaskConfig``, a failed file a ``ScheduledTaskError``."""

from typing import Any, Literal

import pydantic

from src.infra import config
from src.runtime.hooks import scheduled_handlers

# The API request model TaskCreate (src/features/cron/api.py) inherits this default through
# ScheduledTaskFields; the web UI re-pins the value in literals (index.html and
# fallbacks in sidebar/modals.js) that cannot import from Python — a change moves
# every re-pinning site.
DEFAULT_TIMEZONE = config.HOUSE_TIMEZONE


class StepConfig(pydantic.BaseModel):
  """One step of a ``steps`` cron task: a named worker in an ordered chain.

  ``prompt_file`` is the pre-resolution path string the host cron.d file
  declared — an in-process field for transport to the API and UI only, exactly
  like the task-level ``prompt_file``; ``prompt`` is the body the loader
  resolved from it on this load.
  """

  model_config = pydantic.ConfigDict(extra='forbid')

  name: str = pydantic.Field(min_length=1)
  prompt_file: str | None = None
  prompt: str | None = None
  backend: str | None = None
  # Name of a step listed earlier in the same task whose backend must stay
  # distinct from this one's: a reviewer must not ride the same backend as the
  # drafter it reviews. Load time validates the written ids; firing time
  # re-resolves both (src/features/cron/cron_sequence.py) and stops the firing when the
  # resolved backend type and model still match.
  distinct_backend_from: str | None = None


class ScheduledTaskFields(pydantic.BaseModel):
  """Field block every scheduled task carries, shared by the loader's task model
  and the API's create-request model so a new task field ships to both with one edit.

  pydantic merges a parent's config into each child, so every subclass pins its
  own extra-keys policy: the loader model rejects unknown keys
  (``extra='forbid'``), the create-request body keeps ignoring them
  (``extra='ignore'``).
  """

  name: str
  cron: str
  # Pre-resolution path string a host cron.d file declared. It is an in-process
  # field for transport to the API and UI only; no write path persists it.
  prompt_file: str | None = None
  repo: str | None = None
  backend: str | None = None
  timezone: str = DEFAULT_TIMEZONE
  enabled: bool = True
  project: str | None = None
  allow_failure: bool = False
  # Explicit task-tree binding (schema_version=2): the stable session id this
  # task fires against. The binding IS the session — a missing, closed,
  # non-manager or otherwise invalid binding fails the fire visibly instead
  # of creating a replacement session.
  session_id: str | None = None
  # Execution mode of a bound task: 'master' admits the task's prompt as one
  # scheduled input to the bound manager node and dispatches it once;
  # 'worker' (the default when absent) creates one worker leaf per firing. A
  # 'mode' on an unbound task is a load error.
  mode: Literal['master', 'worker'] | None = None


class ScheduledTaskConfig(ScheduledTaskFields):
  """Configuration for a single scheduled (cron-like) task.

  ``name`` is supplied by the loader (from the host file stem) and is required,
  but the persisted per-job file body never carries ``name``. ``extra='forbid'``
  turns an unknown key (a typo such as ``promt_file:``) into that file's error
  instead of silently dropping it.
  """

  model_config = pydantic.ConfigDict(extra='forbid')

  prompt: str | None = None
  handler: str | None = None
  # An instance of scheduled_handlers.loop_model(), the model that the package owning the
  # loop action registered; the validator below builds it from the file's `loop:` mapping.
  loop: Any | None = None
  # Ordered worker chain: each step is one scheduled_step Run on the firing's
  # leaf, launched after the previous one's durable success, and the parent is
  # woken once at the end (src/features/cron/cron_sequence.py).
  steps: list[StepConfig] | None = None

  @pydantic.field_validator('loop', mode='before')
  @classmethod
  def validate_loop_section(cls, value: Any) -> Any:
    """Validate the `loop:` mapping against the registered loop model."""
    if value is None:
      return None
    try:
      model = scheduled_handlers.loop_model()
    except LookupError as e:
      raise ValueError("no package registered a loop section") from e
    return model.model_validate(value)

  @pydantic.model_validator(mode='after')
  def check_sources_and_mode(self) -> ScheduledTaskConfig:
    sources = sum([bool(self.prompt), bool(self.steps), bool(self.handler), bool(self.loop)])
    if sources != 1:
      raise ValueError("task must have exactly one of 'prompt', 'prompt_file', 'steps', 'handler', or 'loop'")
    if self.mode is not None and not self.session_id:
      raise ValueError("'mode' requires 'session_id' (mode selects how a bound task fires)")
    if self.mode == 'master':
      # A prompt_file-style entry is resolved into prompt before model
      # validation, so an empty prompt here means the manager would wake up
      # with no message at all.
      if not self.prompt:
        raise ValueError("mode 'master' requires a prompt source ('prompt' or 'prompt_file')")
      if self.steps is not None or self.handler or self.loop:
        raise ValueError("mode 'master' forbids 'steps', 'handler', and 'loop'; the manager wake is a prompt")
    if self.steps is not None and not self.steps:
      raise ValueError("steps must be a non-empty list")
    if self.steps:
      seen: set[str] = set()
      for step in self.steps:
        if step.name in seen:
          raise ValueError(f"duplicate step name '{step.name}'")
        seen.add(step.name)
        if not step.prompt:
          raise ValueError(
              f"step '{step.name}' has no prompt body; the loader resolves each step's "
              "'prompt_file' before validation")
      self._check_distinct_backends()
    return self

  def _check_distinct_backends(self) -> None:
    """Every ``distinct_backend_from`` names an earlier step, and written backends differ.

    Only the *written* ids are compared here: an effective backend left unset
    is allowed at load time (the repo default names no host-local backend ids,
    and ``seed_default_cron_tasks`` validates every default entry before
    seeding); the firing-time check in ``src/features/cron/cron_sequence.py`` covers the
    unset case by resolving what each step actually runs.
    """
    positions = {step.name: i for i, step in enumerate(self.steps or [])}
    for i, step in enumerate(self.steps or []):
      if step.distinct_backend_from is None:
        continue
      source = positions.get(step.distinct_backend_from)
      if source is None:
        raise ValueError(
            f"step '{step.name}' declares distinct_backend_from '{step.distinct_backend_from}', "
            "which is not a step of this task")
      if source >= i:
        raise ValueError(
            f"step '{step.name}' declares distinct_backend_from '{step.distinct_backend_from}', "
            "which must name a step listed earlier in the same task")
      own = step.backend or self.backend
      prior = self.steps[source].backend or self.backend
      if own is not None and prior is not None and own == prior:
        raise ValueError(
            f"step '{step.name}' and its distinct_backend_from source '{self.steps[source].name}' "
            f"both declare backend '{own}'; the two steps must name different backends")


class ScheduledTaskError(pydantic.BaseModel):
  """A per-file cron load failure surfaced through the API without raising.

  ``enabled`` is the failing file's own raw ``enabled`` value, read best-effort
  at load-failure time — ``None`` when the body cannot be parsed at all (a
  syntax-error yaml gives no truthful answer, and guessing "on" would
  misstate the file).
  """

  name: str
  path: str
  error: str
  enabled: bool | None = None
