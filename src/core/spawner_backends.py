"""Spawn-time backend+model resolution — explicit requests, session defaults, verify selection."""

import structlog

from src.core import review
from src.core.config import CharlieBotConfig
from src.core.models import (
    BackendOption,
    SessionMetadata,
    ThreadMetadata,
    backend_type_allows_missing_model,
)
from src.core.sessions import SessionManager

log = structlog.get_logger()


def resolve_backend_option(cfg: CharlieBotConfig, backend_id: str, model: str | None) -> BackendOption:
  """Resolve a runtime backend option from explicit backend/model values."""
  if not backend_id:
    raise ValueError("resolved backend is required")
  option = cfg.get_backend_option(backend_id)
  if option is None:
    raise ValueError(f"resolved backend '{backend_id}' is not configured")
  if backend_type_allows_missing_model(option.type):
    resolved_model = None
  elif not model:
    raise ValueError("resolved model is required")
  else:
    resolved_model = model
  return option.model_copy(update={"model": resolved_model})


def _option_default_backend_model(option: BackendOption, *, source: str) -> tuple[str, str | None]:
  """Pair a configured option with its own default model, or raise when it needs one and has none."""
  if backend_type_allows_missing_model(option.type):
    return option.id, None
  if not option.model:
    raise ValueError(f"{source} backend '{option.id}' has no default model")
  return option.id, option.model


def unknown_backend_pin_refusal(backend_id: str, fallback_id: str) -> str:
  """Core wording of the refusal for a session pinned to an unknown backend id.

  Substituting a different backend would silently run on another model and another
  account, so every refusal path wraps this core in its own frame instead of
  rewording it.
  """
  return (f"'{backend_id}' is not in config.yaml backend_options — "
          f"refusing to substitute '{fallback_id}'")


def _resolve_session_default_backend_model(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
) -> tuple[str, str | None]:
  """Resolve backend+model from a session's default.

  A session with no backend recorded takes cfg.backend_options[0] as its default. A pinned
  id that backend_options no longer defines (e.g. after a config.yaml rename) is a hard
  error. Raises when cfg.backend_options is empty, when the pinned id is unknown, or when
  the selected option has no default model.
  """
  option = cfg.get_backend_option(session_meta.backend) if session_meta.backend else None
  if option is None:
    if not cfg.backend_options:
      raise ValueError("session backend resolution requires a configured backend_options entry")
    if session_meta.backend:
      log.error(
          "session_backend_unresolved",
          stored=session_meta.backend,
          refused_substitute=cfg.backend_options[0].id,
          session_id=session_meta.id,
      )
      raise ValueError(
          f"session backend {unknown_backend_pin_refusal(session_meta.backend, cfg.backend_options[0].id)}")
    option = cfg.backend_options[0]
  return _option_default_backend_model(option, source="session")


async def _require_session(session_mgr: SessionManager, session_id: str) -> SessionMetadata:
  """Fetch the session's metadata; raise ValueError when the session doesn't exist."""
  session_meta = await session_mgr.get_session(session_id)
  if session_meta is None:
    raise ValueError(f"session '{session_id}' not found")
  return session_meta


async def resolve_requested_subagent_backend_model(
    session_id: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    requested_backend: str | None,
) -> tuple[str, str | None]:
  """Resolve backend+model from an explicit configured backend or the session default.

  Both paths are strict: an unknown explicit `requested_backend` (a typo in user input) and a
  session pinned to an id config.yaml no longer defines both raise. Only an empty session
  backend defaults, and it defaults to cfg.backend_options[0].
  """
  session_meta = await _require_session(session_mgr, session_id)
  if requested_backend is not None:
    if not requested_backend:
      raise ValueError("requested backend is required")
    option = cfg.get_backend_option(requested_backend)
    if option is None:
      raise ValueError(f"requested backend '{requested_backend}' is not in backend_options")
    return _option_default_backend_model(option, source="requested")
  return _resolve_session_default_backend_model(cfg, session_meta)


async def select_verify_backend(
    session_id: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    tried_backends: list[str],
) -> tuple[str, str | None, list[str]] | None:
  """Select a VERIFY task's checking backend when none was requested.

  Verify checks the session's work, so its default backend moves cross-model via
  model_preference, exactly like the delegation reviewer (select_reviewer_backend).
  Never None with an untried (empty) list; None only when every configured backend
  has already been tried.
  """
  session_meta = await _require_session(session_mgr, session_id)
  session_backend, session_model = _resolve_session_default_backend_model(cfg, session_meta)
  return review.select_reviewer_backend(cfg, session_backend, session_model, tried_backends)


def require_thread_backend_model(thread: ThreadMetadata, cfg: CharlieBotConfig) -> tuple[str, str | None]:
  """Return backend+model from thread metadata or raise."""
  if not thread.backend:
    raise ValueError(f"thread '{thread.id}' missing backend metadata")
  if thread.model:
    return thread.backend, thread.model
  option = cfg.get_backend_option(thread.backend)
  if option is None:
    raise ValueError(f"thread '{thread.id}' backend '{thread.backend}' is not in backend_options")
  if backend_type_allows_missing_model(option.type):
    return thread.backend, None
  raise ValueError(f"thread '{thread.id}' missing model metadata")
