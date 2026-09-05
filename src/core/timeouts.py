"""Centralized timeout constants grouped by domain.

Every numeric timeout= literal in the codebase should reference a constant here
so that adjusting a timeout policy (e.g. relaxing subprocess timeouts during
debugging) requires changing exactly one file.
"""

# ---------------------------------------------------------------------------
# Git subprocess timeouts
# ---------------------------------------------------------------------------

# Fast, read-only git commands (rev-parse, branch --list).
SUBPROCESS_GIT_READ_TIMEOUT = 10  # seconds — synchronous subprocess.run
SUBPROCESS_GIT_READ_TIMEOUT_ASYNC = 30.0  # seconds — async subprocess via asyncio.wait_for

# `git diff` between two refs — may scan a large amount of history.
SUBPROCESS_GIT_DIFF_TIMEOUT = 30  # seconds — synchronous subprocess.run

# Mutating / heavier git commands (worktree add, add+commit+push).
SUBPROCESS_GIT_WRITE_TIMEOUT = 60.0  # seconds — worktree creation, commit+push sequences

# Git version info used at startup (rev-parse --short HEAD, git log).
SUBPROCESS_GIT_VERSION_TIMEOUT = 5  # seconds — synchronous; only blocks server startup

# ---------------------------------------------------------------------------
# LaTeX compilation
# ---------------------------------------------------------------------------

# Full LaTeX build via `make pdf`; may run pdflatex + bibtex multiple times.
LATEX_COMPILE_TIMEOUT = 60  # seconds

# ---------------------------------------------------------------------------
# Nsight Compute report parsing
# ---------------------------------------------------------------------------

# Fallback `ncu --import <file> --csv --page details` when the ncu_report
# Python module is unavailable; large reports can take a while to re-import.
SUBPROCESS_NCU_CSV_IMPORT_TIMEOUT = 120  # seconds

# ---------------------------------------------------------------------------
# Autonamer (session title generation)
# ---------------------------------------------------------------------------

# Claude CLI subprocess that generates a 3-6 word title.
AUTONAMER_TIMEOUT = 30.0  # seconds

# ---------------------------------------------------------------------------
# Artifact cold-read probe
# ---------------------------------------------------------------------------

# One-shot model pass over an entire artifact page (sitrep / debug / explain
# cold-read gate); the page text alone dwarfs a naming prompt, so the 30 s
# autonamer budget does not apply.
ARTIFACT_PROBE_TIMEOUT = 300.0  # seconds

# ---------------------------------------------------------------------------
# HTTP client timeouts (outbound requests)
# ---------------------------------------------------------------------------

# Telegram Bot API notification POST.
NOTIFICATION_TIMEOUT = 10.0  # seconds — fast external API, fail quickly

# Anthropic OAuth usage API and token refresh.
HTTP_OAUTH_TIMEOUT = 30  # seconds — remote API may be slow under load

# CLI -> CharlieBot server internal endpoints (delegate, improve).
HTTP_INTERNAL_API_TIMEOUT = 30  # seconds — local loopback, generous for cold starts

# Best-effort fetch of /api/internal/version on the CLI error path, bounded so a hung
# server never stalls CLI error reporting.
HTTP_VERSION_SKEW_TIMEOUT = 2.0  # seconds

# Best-effort `git rev-parse --short HEAD` read behind build identity: the startup capture
# serving /api/internal/version and the CLI's local read for the version-skew hint. The bound
# keeps a hung git from stalling startup or CLI error reporting.
SUBPROCESS_GIT_SHA_TIMEOUT = 2.0  # seconds

# sherpa-onnx speech-model artifact download via urllib; a one-shot per host,
# but the tarball is large.
HTTP_MODEL_DOWNLOAD_TIMEOUT = 60  # seconds

# ---------------------------------------------------------------------------
# CLI remote launch (ssh + setsid)
# ---------------------------------------------------------------------------

# Whole ssh wrapper run: connect (ssh's own ConnectTimeout applies inside) plus
# the remote mkdir/setsid/pid-print sequence. The failure message prints this
# value, so budget and message cannot drift.
SSH_LAUNCH_TIMEOUT = 30  # seconds

# ---------------------------------------------------------------------------
# Session websocket (browser push channel)
# ---------------------------------------------------------------------------

# First message of cursor negotiation on /ws/sessions: the browser must send its
# {"type": "cursor"} replay point within this window or the subscription starts
# from event 0. Expiry is logged as a parse failure, not fatal.
SESSION_WS_CURSOR_TIMEOUT = 5.0  # seconds

# ---------------------------------------------------------------------------
# Hung-subprocess diagnostics capture
# ---------------------------------------------------------------------------

# One probe in _capture_proc_diagnostics (ps tree, fds, /proc status, children).
# The snapshot is best-effort by contract: each probe failure is recorded in
# the returned dict, never raised.
SUBPROCESS_DIAG_CAPTURE_TIMEOUT = 2.0  # seconds

# ---------------------------------------------------------------------------
# Slash commands
# ---------------------------------------------------------------------------

# Default timeout for user-defined slash commands (shell scope).
SLASH_COMMAND_DEFAULT_TIMEOUT = 10  # seconds — overridable per-command in YAML config

# ---------------------------------------------------------------------------
# Polling intervals
# ---------------------------------------------------------------------------

# Per-account gap for the external-usage round-robin poller (Claude Code / Codex
# quotas). The poller fetches one derived account per gap, so each account
# refreshes every N x 60 s where N is the derived account count.
EXT_USAGE_ROUND_GAP_SECONDS = 60  # seconds between consecutive single-account fetches

# ---------------------------------------------------------------------------
# Restart-safe agent runtime
# ---------------------------------------------------------------------------

# A live run whose raw log has not grown for longer than this is reported as
# suspected-hung at server startup (report only — never auto-killed). Empirically
# 6x the p99 inter-event gap (1200 s), ~0.2% false positives on long silent runs.
NO_OUTPUT_REPORT_THRESHOLD = 2 * 3600  # seconds

# Total budget for a CLI to retry a POST whose connection never got established
# (server down/restarting); exponential backoff inside. Measured server cold
# start is <0.5 s, so 60 s is two orders of magnitude of headroom.
CLI_CONNECT_TOTAL_TIMEOUT = 60  # seconds

# ---------------------------------------------------------------------------
# OpenCode backend
# ---------------------------------------------------------------------------

# httpx client for the per-run control API: health probe, model limit, session
# create, prompt send. The long-lived SSE stream overrides this with
# timeout=None (its liveness is the watchdog constant below).
OPENCODE_HTTP_API_TIMEOUT = 30.0  # seconds

# POST to /session/{id}/abort when a turn is cancelled. Best-effort: a failure
# is logged and cleanup proceeds without it.
OPENCODE_ABORT_TIMEOUT = 5.0  # seconds

# Grace wait for the stdout log-tail task to reach EOF after the run ends; past
# it the task is cancelled. The tail loop polls on a sub-second interval, so EOF
# arrives within milliseconds of the process exiting.
OPENCODE_STDOUT_DRAIN_TIMEOUT = 5.0  # seconds

# An opencode /event SSE stream carrying no session-id-bearing event for longer
# than this is declared dead: the turn fails loudly through the normal backend
# failure path. Server-level events (server.heartbeat every ~10 s,
# server.connected) pass through but do not reset the timer. Basis, measured on
# opencode-backend runs: p99 inter-event gap 131 s, longest legitimate gap on a
# completed turn 29.3 min; observed hangs run 25-497 min, so 60 min covers
# every observed hang.
OPENCODE_SSE_PROGRESS_TIMEOUT = 3600.0  # seconds

# ---------------------------------------------------------------------------
# Process-group kill escalation
# ---------------------------------------------------------------------------

# SIGTERM grace before a still-alive process group is SIGKILLed (kill_group_escalating
# in src/core/process.py). The poll interval bounds only how late past the grace the
# escalation can fire; it does not shorten the grace.
KILL_ESCALATION_GRACE_SECONDS = 5.0  # seconds between SIGTERM and the SIGKILL decision
KILL_ESCALATION_POLL_SECONDS = 0.2  # seconds between liveness probes during the grace

# ---------------------------------------------------------------------------
# Master-run identity barrier (boot)
# ---------------------------------------------------------------------------

# How long the lifespan startup waits on reconcile_master_identity before
# falling through to the doors that can create a new turn; the pass itself is
# shielded and keeps running, re-awaited by the crash-recovery task. What the
# barrier bounds: an active-session metadata scan (~11 ms measured) plus
# roughly 10 ms per in-flight master_run record. The bound exists so a stalled
# mount degrades to a raw log line instead of holding boot forever.
MASTER_IDENTITY_BARRIER_TIMEOUT = 5.0  # seconds

# ---------------------------------------------------------------------------
# Uvicorn shutdown
# ---------------------------------------------------------------------------

# How long live HTTP handlers and WebSockets get to wind down after the
# shutdown signal before uvicorn exits the process anyway.
SERVER_GRACEFUL_SHUTDOWN_TIMEOUT = 5  # seconds

# ---------------------------------------------------------------------------
# SQLite lock waits (storage_cool's opencode-db pass)
# ---------------------------------------------------------------------------

# sqlite3.connect(timeout=...) installs the connection's busy handler, so the
# seconds value governs every statement until _vacuum_opencode_db issues the
# busy_timeout pragma below: connect, PRAGMA foreign_keys=ON, and the DELETE
# loop. The pragma then lowers the bound for its freelist check and VACUUM,
# which take the lock for longer stretches — hence the shorter ms value.
SQLITE_LOCK_WAIT_SECONDS = 5.0  # seconds — connect through the DELETE loop
SQLITE_LOCK_WAIT_MS = 2000  # milliseconds — PRAGMA busy_timeout for VACUUM
