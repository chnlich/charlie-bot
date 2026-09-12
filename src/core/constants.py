"""Cross-layer constants shared by the argparse/CLI layer and the model layer.

stdlib-only by contract: the CLI import floor (docs/perf_baseline.md M92) loads
this module on every ``charliebot`` invocation, so nothing here may import
pydantic, config, or any other server stack.
"""

from enum import StrEnum

# Cross-process session-identity wire name: the server writes the master's
# session id into every spawned process env (master_cc_run._build_master_env),
# the backend supervisors strip any inherited value, and the CLIs read it back
# (src.cli.common.resolve_session_id). One spelling everywhere.
SESSION_ID_ENV_VAR = "CHARLIEBOT_SESSION_ID"

# Upper bound on trigger --message length. The message is a short label naming
# which watch fired (runbook steps and readback commands live in session
# artifacts), so the CLI argparse precheck and --help text share this constant.
MAX_TRIGGER_MESSAGE_CHARS = 200

# Plan-registry verb vocabularies: the CLI's argparse choices (src.cli.plan) and the
# registry verbs' validation (src.core.plans) share one tuple per vocabulary, so the
# plan chain imports no pydantic to parse args. The request models' Literal types
# (src.core.models PlanAmendTrigger / PlanCloseMode) are the type home; the import
# contract pins tuple == get_args(Literal).
PLAN_AMEND_TRIGGERS = ("auto_amend", "feedback")
PLAN_CLOSE_MODES = ("superseded", "abandoned", "completed")

# Memory-replay mode vocabulary: the memory CLI's argparse choices (src.cli.memory) and the
# replay runner's validation (src.core.memory_replay.runner) share one tuple, so the memory
# query/add/lint verbs import no replay stack to build the parser. One spelling everywhere.
REPLAY_MODES = ("editor-only", "editor-review")


class WatchKind(StrEnum):
  UNKNOWN = "unknown"  # fail-loud sentinel; never a valid target, no default
  LOCAL_PID = "local_pid"
  REMOTE_PID = "remote_pid"
  SLURM_JOB = "slurm_job"
