"""Direct worker spawner — creates a task, enriches the prompt, and runs the worker.

Facade over the ``spawner_<part>`` modules: every pre-split top-level def/class
keeps resolving at its original ``src.core.spawner.<name>`` path, so existing
import sites and monkeypatch targets on this module stay valid. The parts hold
the implementation and must never import this module — that would close an
import cycle.
"""

from src.core.spawner_backends import (  # noqa: F401  # re-export: facade import list (see module docstring)
  _option_default_backend_model,
  _require_session,
  _resolve_session_default_backend_model,
  require_thread_backend_model,
  resolve_backend_option,
  resolve_requested_subagent_backend_model,
  select_verify_backend,
)
from src.core.spawner_events import (  # noqa: F401  # re-export: facade import list (see module docstring)
  _extract_event_content,
  _read_thread_events,
  _thread_worker_event,
  _worker_locator_summary,
  _worker_summary_timestamp,
  read_events_summary,
)
from src.core.spawner_finalize import (  # noqa: F401  # re-export: facade import list (see module docstring)
  _broadcast_completion,
  _cleanup_worker_directory,
  _exit_status_label,
  _finalize_worker,
  _finalize_worker_safely,
  _maybe_override_exit_code_from_result,
  _notify_completion,
  _persist_worker_summary_once,
  _run_finalize_effects,
  _should_skip_worktree_cleanup,
  _stream_worker_events,
  _thread_cancelled,
  _verify_report_for_task,
  _WorkerRunOutcome,
  recomplete_finalize_effects,
)
from src.core.spawner_launch import (  # noqa: F401  # re-export: facade import list (see module docstring)
  _construct_worker,
  _create_repoless_process,
  _create_worktree_and_process,
)
from src.core.spawner_lifecycle import (  # noqa: F401  # re-export: facade import list (see module docstring)
  resume_worker,
  spawn_worker,
)
from src.core.spawner_prompt import (  # noqa: F401  # re-export: facade import list (see module docstring)
  _build_worker_prompt,
  _load_prompt_sections,
  _require_tokens_resolved,
  _substitute_tokens,
  load_worker_prompt_sections,
)
