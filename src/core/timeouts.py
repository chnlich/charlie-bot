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

# CLI -> CharlieBot server internal endpoints (delegate, improve).
HTTP_INTERNAL_API_TIMEOUT = 30  # seconds — local loopback, generous for cold starts

# Best-effort fetch of /api/internal/version on the CLI error path, plus the local
# `git rev-parse --short HEAD` used to compose the version-skew hint. Both are bounded
# so a hung server or a hung git never stalls CLI error reporting.
HTTP_VERSION_SKEW_TIMEOUT = 2.0  # seconds
SUBPROCESS_VERSION_SKEW_TIMEOUT = 2.0  # seconds

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
