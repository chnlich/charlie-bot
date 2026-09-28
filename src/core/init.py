"""Initialize ~/.charliebot/ directory structure on first run.

Facade over the ``init_<part>`` modules: the re-export list carries exactly the
names call sites still reach through ``src.core.init.<name>``, so existing
import sites and monkeypatch targets on this module keep resolving. The parts
hold the implementation and must never import this module — that would close an
import cycle.

The two assignments at the bottom serve module-level names that tests reach
through this module (the scan-window and quarantine constants); they are
assignments, not imports, so the export-list evidence check sees only
def/class names.
"""

import src.core.init_worker_recovery as _init_worker_recovery
from src.core.init_master_recovery import (  # noqa: F401  # re-export: facade import list (see module docstring)
    reconcile_master_identity,
    run_crash_recovery,
)
from src.core.init_seed import (  # noqa: F401  # re-export: facade import list (see module docstring)
    init_charliebot_home,
    seed_default_cron_tasks,
)
from src.core.init_worker_recovery import (  # noqa: F401  # re-export: facade import list (see module docstring)
    _quarantine_stale_failed_worktrees,
    _report_recovery_event,
    iter_recent_thread_metas,
)

# Serve module-level names reachable through this module pre-split.
FAILED_WORKTREE_QUARANTINE_DAYS = _init_worker_recovery.FAILED_WORKTREE_QUARANTINE_DAYS
RUNNING_SCAN_WINDOW = _init_worker_recovery.RUNNING_SCAN_WINDOW
