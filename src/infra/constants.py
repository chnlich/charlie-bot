"""Cross-layer constants shared by the CLI, server, and model layers.

stdlib-only by contract: the CLI import floor (docs/perf_baseline.md@5175adf09 M92) loads
this module on every ``charliebot`` invocation, so nothing here may import
pydantic, config, or any other server stack.
"""

import enum

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
    import pathlib

    root = pathlib.Path(__file__).resolve().parents[2]
    globals()["REPO_ROOT"] = root
    return root
  raise AttributeError(f"module {__name__!r} has no attribute {name!r}")


# Cross-process session-identity wire name: the server writes the master's
# session id into every spawned process env (master_cc_run._build_master_env),
# the backend supervisors strip any inherited value, and the CLIs read it back
# (src.runtime.cli.common.resolve_session_id). One spelling everywhere.
SESSION_ID_ENV_VAR = "CHARLIEBOT_SESSION_ID"

# Run-bearer env wire name: run_token reads the presented bearer from it and the
# task executor writes the signed token into every spawned worker env. One spelling everywhere.
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

# Upper bound on one inter-agent session send, in weighted characters
# (weighted_length: CJK characters, CJK punctuation, and fullwidth forms count
# 2). The session-message endpoint's 422 refusal, the session send --help
# texts, and the generated manager prompt's {{agent_message_limit}} placeholder
# share this constant.
MAX_AGENT_MESSAGE_WEIGHTED_CHARS = 2000

# Upper bound on one posted reply, in characters. prompts/thread_reply_format.md
# states it, and the thread-reply and web-reply readbacks both set their
# ``over_budget`` flag from it. An over-budget reply still posts.
REPLY_BUDGET_CHARS = 500

# Cold-session idle threshold (days): the storage sweep's default judgment and
# the CLI parser's --min-idle-days default and --help text share one number.
MIN_IDLE_DAYS = 14

# Page size, in messages, of one Discord thread read: the ``charliebot discord read --limit``
# parser, its --help text and the discord/read request model share them, so the CLI precheck
# cannot admit a page the endpoint refuses.
DISCORD_READ_LIMIT_MIN = 1
DISCORD_READ_LIMIT_MAX = 100
DISCORD_READ_LIMIT_DEFAULT = 50

# Page size, in rows, of one session listing page: the tree and runs query bounds, the archived
# listing's default and clamp, and the ``charliebot session tree`` parser share them, so a page
# size the CLI sends is one the endpoint serves.
SESSION_PAGE_LIMIT_MIN = 1
SESSION_PAGE_LIMIT_MAX = 500
SESSION_PAGE_LIMIT_DEFAULT = 100

AUTH_STATUS_PATH = "/api/auth/status"

# The operator-facing local-timestamp spelling: the usage page's "as of" and "generated"
# stamps and the host-auth page's baseline, expiry and probe stamps render it, so the two
# pages show one time format.
LOCAL_TIMESTAMP_FORMAT = "%Y-%m-%d %H:%M %Z"


class WatchKind(enum.StrEnum):
  UNKNOWN = "unknown"  # fail-loud sentinel; never a valid target, no default
  LOCAL_PID = "local_pid"
  REMOTE_PID = "remote_pid"
  SLURM_JOB = "slurm_job"
