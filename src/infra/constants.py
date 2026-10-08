"""Cross-layer constants shared by the CLI, server, and model layers.

stdlib-only by contract: the CLI import floor (docs/perf_baseline.md@5175adf09 M92) loads
this module on every ``charliebot`` invocation, so nothing here may import
pydantic, config, or any other server stack.
"""

from enum import StrEnum

# Checkout root (where pyproject.toml lives): this file sits at src/infra/, so
# parents[2] is the root; moving this file breaks the depth. Buildinfo's git
# calls, the HTML page-template reads, the /static mount, and the pages layer's
# git-version cwd, static-tree digest, and Jinja templates directory derive
# from it. Built on first access, not at import: this module loads on every
# ``charliebot`` verb (the M92 CLI import floor), and pathlib's import chain
# (~5 ms) prices every verb's parser build while only the REPO_ROOT readers
# (the server pages, the HTML page writers, the config re-export) touch it.


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

# The merged-trace route path. The auth whitelist (src.runtime.api.auth) does not admit it — it
# reads local trace files, so it sits behind the access key like the file server. It is also the
# special case server.py's gzip middleware skips (the body is already-compressed
# trace bytes) and the URL built for merged traces.
PERFETTO_MERGED_PATH = "/perfetto/merged"
AUTH_STATUS_PATH = "/api/auth/status"


class WatchKind(StrEnum):
  UNKNOWN = "unknown"  # fail-loud sentinel; never a valid target, no default
  LOCAL_PID = "local_pid"
  REMOTE_PID = "remote_pid"
  SLURM_JOB = "slurm_job"
