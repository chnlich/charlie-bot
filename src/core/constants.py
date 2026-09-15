"""Cross-layer constants shared by the CLI, server, and model layers.

stdlib-only by contract: the CLI import floor (docs/perf_baseline.md M92) loads
this module on every ``charliebot`` invocation, so nothing here may import
pydantic, config, or any other server stack.
"""

from enum import StrEnum
from pathlib import Path

# Checkout root (where pyproject.toml lives): this file sits at src/core/, so
# parents[2] is the root; moving this file breaks the depth. Buildinfo's git
# calls, the artifact template reads, the /static mount, and the pages layer's
# git-version cwd, static-tree digest, and Jinja templates directory derive
# from it.
REPO_ROOT = Path(__file__).resolve().parents[2]

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
# contract pins tuple == get_args(Literal). The named close-mode spellings are the
# home for the values plans derives and compares against (src.core.plans _derive_state,
# _DERIVED_STATE_STR): a closed plan's derived state IS its close mode's spelling.
PLAN_AMEND_TRIGGERS = ("auto_amend", "feedback")
PLAN_CLOSE_SUPERSEDED = "superseded"
PLAN_CLOSE_ABANDONED = "abandoned"
PLAN_CLOSE_COMPLETED = "completed"
PLAN_CLOSE_MODES = (PLAN_CLOSE_SUPERSEDED, PLAN_CLOSE_ABANDONED, PLAN_CLOSE_COMPLETED)

# opencode's own compaction output-reserve default ($d = 20000 in the opencode binary,
# applied as `compaction.reserved ?? min($d, maxOutputTokens)`; checkable via
# `grep -ao "compaction?\.reserved.\{0,140\}" <opencode binary>`). The opencode backend
# (src.agents.backends.opencode) and the usage resolver's compact-point math
# (src.core.session_usage) share one spelling, so the usage chain imports no backend
# module for it (the M99 server import floor).
OPENCODE_COMPACT_OUTPUT_RESERVE = 20_000

# File-server URL prefixes: server.py mounts the one files router under each, so both
# spellings reach the same handler. The first entry is the canonical form the UI builds
# and older links carry; the second is the form written into chat text to name absolute
# filesystem paths. Every Python reader derives its form (auth whitelist entries, trace
# parsing, listing roots, slack URL rewriting) from this tuple; the frontend gate
# (web/static/js/chat/artifacts.js) mirrors the set, pinned by
# tests/test_frontend_file_server_prefixes.py.
FILE_SERVER_MOUNTS = ("/files", "/absolute_filepath")


class WatchKind(StrEnum):
  UNKNOWN = "unknown"  # fail-loud sentinel; never a valid target, no default
  LOCAL_PID = "local_pid"
  REMOTE_PID = "remote_pid"
  SLURM_JOB = "slurm_job"
