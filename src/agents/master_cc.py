"""Master CC — spawns a Claude Code subprocess for the master agent.

Facade over the ``master_cc_<part>`` modules: the re-export list carries exactly
the names call sites still reach through ``src.agents.master_cc.<name>``, so
existing import sites and direct calls stay valid. A monkeypatch target must name the
module whose body looks the name up — a part's own bare-name references resolve
in that part (patch ``master_cc_run._run_cc``, not ``master_cc._run_cc``);
only callers that read this module's attribute at call time (e.g.
init_master_recovery.py's ``master_cc.queued_user_event_ids``) stay patchable here. The parts hold the
implementation and must never import this module — that would close an
import cycle.
"""

from src.agents.master_cc_queue import (  # noqa: F401  # re-export: facade import list (see module docstring)
    cancel_master,
    enqueue_master_resume,
    queued_user_event_ids,
    replay_scheduled_trigger,
    replay_user_message,
    run_message,
)
from src.agents.master_cc_run import (  # noqa: F401  # re-export: facade import list (see module docstring)
    _build_fresh_translate,
    _build_instructions_content,
    _build_master_env,
    _cc_transcript_exists,
    _route_resume_session,
    _run_cc,
)
from src.agents.master_cc_state import (  # noqa: F401  # re-export: facade import list (see module docstring)
    _WorkItem,
)
