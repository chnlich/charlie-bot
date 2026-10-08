"""Cross-layer constants shared by the CLI, server, and model layers.

stdlib-only by contract: the CLI import floor (docs/perf_baseline.md@5175adf09 M92) loads
this module on every ``charliebot`` invocation, so nothing here may import
pydantic, config, or any other server stack.
"""

from enum import StrEnum

# Checkout root (where pyproject.toml lives): this file sits at src/infra/, so
# parents[2] is the root; moving this file breaks the depth. Buildinfo's git
# calls, the artifact template reads, the /static mount, and the pages layer's
# git-version cwd, static-tree digest, and Jinja templates directory derive
# from it. Built on first access, not at import: this module loads on every
# ``charliebot`` verb (the M92 CLI import floor), and pathlib's import chain
# (~5 ms) prices every verb's parser build while only the REPO_ROOT readers
# (the server pages, the artifact writers, the config re-export) touch it.


def __getattr__(name: str):
  if name == "REPO_ROOT":
    from pathlib import Path

    root = Path(__file__).resolve().parents[2]
    globals()["REPO_ROOT"] = root
    return root
  raise AttributeError(f"module {__name__!r} has no attribute {name!r}")


# Cross-process session-identity wire name: the server writes the master's
# session id into every spawned process env (master_cc_run._build_master_env),
# the backend supervisors strip any inherited value, and the CLIs read it back
# (src.runtime.cli.common.resolve_session_id). One spelling everywhere.
SESSION_ID_ENV_VAR = "CHARLIEBOT_SESSION_ID"

# Run-bearer env wire name: run_token reads the presented bearer from it and the
# task executor writes the signed token into every spawned worker env. One
# spelling everywhere, so the isolation boundary (src/infra/identity_env.py) names the constant.
RUN_TOKEN_ENV = "CHARLIEBOT_RUN_TOKEN"

# Request-header wire name of the calling session on internal-API calls: the
# CLI sends it from SESSION_ID_ENV_VAR and require_caller records it on
# operator caller identities (an agent's session is token-verified, never the
# header). One spelling everywhere, shared by the CLI and the API.
CALLER_SESSION_HEADER = "X-CharlieBot-Caller-Session"

# Upper bound on trigger --message length. The message is a short label naming
# which watch fired (runbook steps and readback commands live in session
# artifacts), so the CLI argparse precheck and --help text share this constant.
MAX_TRIGGER_MESSAGE_CHARS = 200

# Cold-session idle threshold (days): the storage sweep's default judgment and
# the CLI parser's --min-idle-days default and --help text share one number.
MIN_IDLE_DAYS = 14

# Plan-registry verb vocabularies: the CLI's argparse choices (src.features.artifacts.plan_cli) and the
# registry verbs' validation (src.features.artifacts.plans) share one tuple per vocabulary, so the
# plan chain imports no pydantic to parse args. The request models' Literal types
# (src.infra.models PlanAmendTrigger / PlanCloseMode) are the type home; the import
# contract pins tuple == get_args(Literal). The named close-mode spellings are the
# home for the values plans derives and compares against (src.features.artifacts.plans _derive_state,
# _DERIVED_STATE_STR): a closed plan's derived state IS its close mode's spelling.
PLAN_AMEND_TRIGGERS = ("auto_amend", "feedback")
PLAN_CLOSE_SUPERSEDED = "superseded"
PLAN_CLOSE_ABANDONED = "abandoned"
PLAN_CLOSE_COMPLETED = "completed"
PLAN_CLOSE_MODES = (PLAN_CLOSE_SUPERSEDED, PLAN_CLOSE_ABANDONED, PLAN_CLOSE_COMPLETED)

# Artifact genre vocabulary: the artifact CLI's argparse choices (src.features.artifacts.cli) and
# the assertion registry (src.features.artifacts.artifact_check _ASSERTION_SETS) share one tuple, so
# the artifact chain parses args without loading the assertion machinery (the M102 wrap
# wall). The registry is the home of what a genre means: adding a genre means registering
# its assertion set there AND naming it here; artifact_check's import-time equality check
# makes a missed step fail loud.
ARTIFACT_GENRES = ("plan", "understanding", "sitrep", "debug", "explain")

# File-server URL prefix: server.py mounts the one files router under it. The prefix names
# what has to follow it — the absolute filesystem path with its leading `/` removed — so a
# path that dropped its leading segments reads as wrong where it is written. The legacy /files
# (and singular /file) spellings are hard-offline: nothing is mounted there, both answer 404.
# Every Python reader derives its form from this tuple — the pages URLs and trace inputs,
# the files listing root and entry URLs, the slack URL rewriting. The auth whitelist
# deliberately derives nothing: the file server sits behind the access key, and deriving
# the prefix there would re-open the gate. The frontend gate (web/static/js/chat/artifacts.js)
# mirrors the single element.
FILE_SERVER_MOUNTS = ("/absolute_filepath",)

# Viewer route paths: the trace and ncu packages declare each route with its spelling. The auth
# whitelist (src.runtime.api.auth) does not admit them — they read local trace/report files, so
# they sit behind the access key like the file server. The merged path is additionally the
# special case server.py's gzip middleware skips (the body is already-compressed
# trace bytes) and the URL the trace package builds for merged traces.
PERFETTO_VIEWER_PATH = "/perfetto"
PERFETTO_MERGED_PATH = "/perfetto/merged"
NCU_VIEWER_PATH = "/ncu"
AUTH_STATUS_PATH = "/api/auth/status"

# Usage-source vocabulary: the ledger's source values. Each backend package's register() names
# its usage source (src/runtime/hooks/usage_sources.py) and its log reader tags every record with
# the same value; the token tally (src/features/usage/token_tally.py) tags the records of
# CharlieBot's own logs USAGE_SOURCE_CHARLIE_BOT. The usage page (src/features/usage/api.py)
# attributes each charlie-bot row's accounts to the CLI that ran the call, so its cards key on the
# registered names, never on USAGE_SOURCE_CHARLIE_BOT.
USAGE_SOURCE_CLAUDE_CODE = "Claude Code"
USAGE_SOURCE_CODEX = "Codex"
USAGE_SOURCE_OPENCODE = "opencode"
USAGE_SOURCE_CHARLIE_BOT = "charlie-bot"
# The panel's CLC source: Charlie Code's own usage, which only CharlieBot's own logs hold.
# The constant spells the full name; the value is the CLC spelling the interface uses.
# Not to be confused with USAGE_SOURCE_CLAUDE_CODE above, the Claude Code CLI's source.
USAGE_SOURCE_CHARLIE_CODE = "CLC"


class WatchKind(StrEnum):
  UNKNOWN = "unknown"  # fail-loud sentinel; never a valid target, no default
  LOCAL_PID = "local_pid"
  REMOTE_PID = "remote_pid"
  SLURM_JOB = "slurm_job"
