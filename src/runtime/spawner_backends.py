"""Spawn-time backend+model resolution — explicit requests, the worker default, session defaults, verify selection."""

from typing import Literal

from src.infra import config, log_once, models
from src.runtime import review, session_store

log = log_once.LazyStructlogLogger()

# The improve loop's worker child carries no task type; its spawn names this role instead.
IMPROVE_ROLE = "improve"

SubagentRole = models.TaskType | Literal["improve"]

# The roles that run on backends.worker_default when the request names no
# backend. Verify and cold-read pick a cross-model backend (select_verify_backend)
# and the reviewer its own (review.select_reviewer_backend).
_WORKER_DEFAULT_ROLES: frozenset[str] = frozenset(
    {models.TaskType.IMPLEMENT, models.TaskType.QUICK_EDIT, models.TaskType.SCRIPT_RUN, IMPROVE_ROLE})


def resolve_backend_option(cfg: config.CharlieBotConfig, backend_id: str, model: str | None) -> models.BackendOption:
  """Resolve a runtime backend option from explicit backend/model values."""
  if not backend_id:
    raise ValueError("resolved backend is required")
  option = config.require_backend_option(cfg, backend_id, subject="resolved ")
  if models.backend_type_allows_missing_model(option.type):
    resolved_model = None
  elif not model:
    raise ValueError("resolved model is required")
  else:
    resolved_model = model
  return option.model_copy(update={"model": resolved_model})


def unknown_backend_pin_refusal(backend_id: str, fallback_id: str) -> str:
  """Core wording of the refusal for a session pinned to an unknown backend id.

  Substituting a different backend would silently run on another model and another
  account, so every refusal path wraps this core in its own frame instead of
  rewording it.
  """
  return (f"'{backend_id}' is not in config.yaml backends.options — "
          f"refusing to substitute '{fallback_id}'")


# The run-context preview route (src/runtime/api/sessions.py) reproduces this refusal
# verbatim as its 400 detail: the preview must refuse exactly where a launch
# would.
EMPTY_BACKENDS_OPTIONS_REFUSAL = "session backend resolution requires a configured backends.options entry"


def _resolve_session_default_backend_model(
    cfg: config.CharlieBotConfig,
    session_meta: models.SessionMetadata,
) -> tuple[str, str | None]:
  """Resolve backend+model from a session's default.

  A session with no backend recorded takes cfg.backends.options[0] as its default. A pinned
  id that backends.options no longer defines (e.g. after a config.yaml rename) is a hard
  error. Raises when cfg.backends.options is empty, when the pinned id is unknown, or when
  the selected option has no default model.
  """
  option = cfg.get_backend_option(session_meta.backend) if session_meta.backend else None
  if option is None:
    if not cfg.backends.options:
      raise ValueError(EMPTY_BACKENDS_OPTIONS_REFUSAL)
    if session_meta.backend:
      log.error(
          "session_backend_unresolved",
          stored=session_meta.backend,
          refused_substitute=cfg.backends.options[0].id,
          session_id=session_meta.id,
      )
      raise ValueError(
          f"session backend {unknown_backend_pin_refusal(session_meta.backend, cfg.backends.options[0].id)}")
    option = cfg.backends.options[0]
  return option.id, models.option_default_model(option, subject="session backend ")


async def _require_session(store: session_store.SessionStore, session_id: str) -> models.SessionMetadata:
  """Fetch the session's metadata; raise ValueError when the session doesn't exist."""
  session_meta = await store.get_session(session_id)
  if session_meta is None:
    raise ValueError(f"session '{session_id}' not found")
  return session_meta


async def resolve_requested_subagent_backend_model(
    session_id: str,
    cfg: config.CharlieBotConfig,
    store: session_store.SessionStore,
    requested_backend: str | None,
    role: SubagentRole,
) -> tuple[str, str | None]:
  """Resolve a subagent's backend+model: the explicit request, the worker default, or the session default.

  An explicit `requested_backend` wins. Without one, an implementation worker
  (a `_WORKER_DEFAULT_ROLES` role) runs on cfg.backends.worker_default when that
  key is set; every other case inherits the session's backend. Every path is
  strict: an unknown explicit `requested_backend` (a typo in user input), a
  worker_default id config.yaml no longer defines, and a session pinned to an
  unknown id all raise. Only an empty session backend defaults, and it defaults
  to cfg.backends.options[0].
  """
  session_meta = await _require_session(store, session_id)
  if requested_backend is not None:
    if not requested_backend:
      raise ValueError("requested backend is required")
    option = config.require_backend_option(cfg, requested_backend, subject="requested ")
    return option.id, models.option_default_model(option, subject="requested backend ")
  if role in _WORKER_DEFAULT_ROLES and cfg.backends.worker_default is not None:
    option = config.require_backend_option(cfg, cfg.backends.worker_default, subject="worker default ")
    return option.id, models.option_default_model(option, subject="worker default backend ")
  return _resolve_session_default_backend_model(cfg, session_meta)


async def select_verify_backend(
    session_id: str,
    cfg: config.CharlieBotConfig,
    store: session_store.SessionStore,
    tried_backends: list[str],
) -> tuple[str, str | None, list[str]] | None:
  """Select a VERIFY task's checking backend when none was requested.

  Verify checks the session's work, so its default backend moves cross-model via
  model_preference, exactly like the delegation reviewer (select_reviewer_backend).
  Never None with an untried (empty) list; None only when every configured backend
  has already been tried.
  """
  session_meta = await _require_session(store, session_id)
  session_backend, session_model = _resolve_session_default_backend_model(cfg, session_meta)
  return review.select_reviewer_backend(cfg, session_backend, session_model, tried_backends)
