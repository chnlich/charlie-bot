"""Spawn-time backend, model, and prompt resolution for worker Runs.

Facade over the ``spawner_<part>`` modules: the re-export list carries exactly the
names call sites still reach through ``src.core.spawner.<name>``, so existing
import sites and monkeypatch targets on this module keep resolving. The parts hold
the implementation and must never import this module — that would close an
import cycle.
"""

from src.core.spawner_backends import (  # noqa: F401  # re-export: facade import list (see module docstring)
    _resolve_session_default_backend_model,
    resolve_backend_option,
    resolve_requested_subagent_backend_model,
    select_verify_backend,
)
from src.core.spawner_prompt import (  # noqa: F401  # re-export: facade import list (see module docstring)
    _build_worker_prompt,
    load_worker_prompt_sections,
)
