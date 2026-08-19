"""Master CC — spawns a Claude Code subprocess for the master agent.

Facade over the ``master_cc_<part>`` modules: every pre-split top-level def/class
keeps resolving at its original ``src.agents.master_cc.<name>`` path, so existing
import sites and direct calls stay valid. A monkeypatch target must name the
module whose body looks the name up — a part's own bare-name references resolve
in that part (patch ``master_cc_run._run_cc``, not ``master_cc._run_cc``);
only callers that read this module's attribute at call time (e.g. init.py's
``master_cc.queued_user_event_ids``) stay patchable here. The parts hold the
implementation and must never import this module — that would close an
import cycle.
"""

from src.agents.master_cc_queue import (  # noqa: F401  # re-export: facade import list (see module docstring)
  _enqueue_work_item,
  _session_consumer,
  cancel_master,
  enqueue_master_resume,
  queued_user_event_ids,
  replay_user_message,
  run_message,
)
from src.agents.master_cc_run import (  # noqa: F401  # re-export: facade import list (see module docstring)
  _build_fresh_translate,
  _build_instructions_content,
  _build_master_env,
  _build_prompt,
  _cc_transcript_exists,
  _handle_event,
  _is_manual_compact_boundary,
  _kill_run_group_escalating,
  _resolve_resume_id,
  _resolve_resume_option,
  _resume_cc,
  _route_resume_session,
  _run_cc,
  _RunTimingTracker,
  _salvage_silent_turn,
)
from src.agents.master_cc_state import (  # noqa: F401  # re-export: facade import list (see module docstring)
  _WorkItem,
)
