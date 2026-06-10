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
# HTTP client timeouts (outbound requests)
# ---------------------------------------------------------------------------

# Telegram Bot API notification POST.
NOTIFICATION_TIMEOUT = 10.0  # seconds — fast external API, fail quickly

# Anthropic OAuth usage API and token refresh.
HTTP_OAUTH_TIMEOUT = 30  # seconds — remote API may be slow under load

# CLI → CharlieBot server internal endpoints (delegate, improve).
HTTP_INTERNAL_API_TIMEOUT = 30  # seconds — local loopback, generous for cold starts

# ---------------------------------------------------------------------------
# Slash commands
# ---------------------------------------------------------------------------

# Default timeout for user-defined slash commands (shell scope).
SLASH_COMMAND_DEFAULT_TIMEOUT = 10  # seconds — overridable per-command in YAML config

# ---------------------------------------------------------------------------
# Polling intervals
# ---------------------------------------------------------------------------

# Background poller for external usage data (Claude Code / Codex quotas).
EXT_USAGE_POLL_INTERVAL = 10 * 60  # 600 seconds (10 minutes)
