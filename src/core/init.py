"""Initialize ~/.charliebot/ directory structure on first run.

Facade over the ``init_<part>`` modules: every pre-split top-level def/class
keeps resolving at its original ``src.core.init.<name>`` path, so existing
import sites and monkeypatch targets on this module stay valid. The parts hold
the implementation and must never import this module — that would close an
import cycle.

The three assignments at the bottom serve module-level names that tests reach
through this module (the two scan-window/quarantine constants and the
boot-scoped silence once-key); they are assignments, not imports, so the
export-list evidence check sees only def/class names. ``_silence_reported_``
is bound by reference: the code and the tests must mutate one shared set.
"""

import src.core.init_worker_recovery as _init_worker_recovery
from src.core.init_master_recovery import (  # noqa: F401  # re-export: facade import list (see module docstring)
  _await_reattach,
  _master_alive_unfollowable_message,
  _MasterScanFailed,
  _reconcile_master_runs,
  _replay_unanswered_user_messages,
  reconcile_master_identity,
  run_crash_recovery,
  unanswered_user_events,
)
from src.core.init_seed import (  # noqa: F401  # re-export: facade import list (see module docstring)
  _default_config_yaml,
  _seed_if_missing,
  _seed_memory_scaffold,
  init_charliebot_home,
  seed_default_cron_tasks,
)
from src.core.init_worker_recovery import (  # noqa: F401  # re-export: facade import list (see module docstring)
  _complete_finalize_effects,
  _effects_maybe_missing,
  _follow_silence_recheck,
  _InterruptedRun,
  _liveness_probe,
  _maybe_respawn,
  _parse_started_at,
  _quarantine_stale_failed_worktrees,
  _reconcile_interrupted_runs,
  _reconcile_one,
  _report_recovery_event,
  _scan_interrupted_runs,
  _started_before_boot,
  _translate_for_thread,
  iter_recent_thread_metas,
)

# Serve module-level names reachable through this module pre-split; the once-key
# set is bound by reference so code and tests mutate the same object.
FAILED_WORKTREE_QUARANTINE_DAYS = _init_worker_recovery.FAILED_WORKTREE_QUARANTINE_DAYS
RUNNING_SCAN_WINDOW = _init_worker_recovery.RUNNING_SCAN_WINDOW
_silence_reported_thread_ids = _init_worker_recovery._silence_reported_thread_ids
