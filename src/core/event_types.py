"""Canonical event-type string constants.

Every raw event dict in the system carries a ``"type"`` field whose value is
one of the strings defined here.  Import constants from this module instead of
hard-coding the strings at construction and consumption sites.
"""

# -- Core chat events --------------------------------------------------------
ASSISTANT = "assistant"
USER = "user"
ERROR = "error"
RESULT = "result"
SYSTEM = "system"

# -- Tool events -------------------------------------------------------------
TOOL_USE = "tool_use"
TOOL_RESULT = "tool_result"

# -- Master lifecycle --------------------------------------------------------
MASTER_DONE = "master_done"
ASSISTANT_ERROR = "assistant_error"

# -- Worker / delegation -----------------------------------------------------
TASK_DELEGATED = "task_delegated"
WORKER_SUMMARY = "worker_summary"
COMPLETE = "complete"

# -- Scheduler / handler ----------------------------------------------------
HANDLER_RESULT = "handler_result"
SCHEDULED_TRIGGER = "scheduled_trigger"

# -- Agent relay -------------------------------------------------------------
# Cross-session agent message: carries the caller session's provenance and is
# never a real user message (the authorization gate excludes it by type).
AGENT_MESSAGE = "agent_message"

# -- Context -----------------------------------------------------------------
CONTEXT_COMPACTED = "context_compacted"
CONTEXT_COMPACT_FAILED = "context_compact_failed"
RESUME_CONTEXT_DROPPED = "resume_context_dropped"

# -- Clone / fork ------------------------------------------------------------
CLONE_START = "clone_start"

# -- Improve loop ------------------------------------------------------------
IMPROVE_ITERATION_COMPLETED = "improve_iteration_completed"
IMPROVE_COMPLETED = "improve_completed"
IMPROVE_CANCELLED = "improve_cancelled"
IMPROVE_FAILED = "improve_failed"

# -- Sidebar / UI ------------------------------------------------------------
RUNNING_CHANGED = "running_changed"
UNREAD_CHANGED = "unread_changed"
SESSION_RENAMED = "session_renamed"
SESSION_GROUP_CHANGED = "session_group_changed"

# -- LaTeX -------------------------------------------------------------------
TEX_EDIT_PROPOSED = "tex_edit_proposed"

# -- Slash command responses -------------------------------------------------
HELP = "help"
IMPROVE_STOPPED = "improve_stopped"
TASK_TRIGGERED = "task_triggered"
SHELL_RESULT = "shell_result"
PROMPT_DISPATCHED = "prompt_dispatched"

# -- Backend-specific --------------------------------------------------------
THINKING = "thinking"
FILE_WRITE = "file_write"
# Claude Code emits this raw-stream event when the subscription/API answers
# with a rate-limit status; workers persist it verbatim, so the quota-
# detection chain consumes the same type on read-back.
RATE_LIMIT_EVENT = "rate_limit_event"

# -- Session backend switching ----------------------------------------------
BACKEND_SWITCHED = "backend_switched"
