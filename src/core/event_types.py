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

# -- Context -----------------------------------------------------------------
CONTEXT_COMPACTED = "context_compacted"

# -- Improve loop ------------------------------------------------------------
IMPROVE_ITERATION_COMPLETED = "improve_iteration_completed"
IMPROVE_COMPLETED = "improve_completed"
IMPROVE_FAILED = "improve_failed"

# -- Sidebar / UI ------------------------------------------------------------
RUNNING_CHANGED = "running_changed"
UNREAD_CHANGED = "unread_changed"
SESSION_RENAMED = "session_renamed"

# -- LaTeX -------------------------------------------------------------------
TEX_EDIT_PROPOSED = "tex_edit_proposed"

# -- Slash command responses -------------------------------------------------
HELP = "help"
IMPROVE_STARTED = "improve_started"
IMPROVE_STOPPED = "improve_stopped"
TASK_TRIGGERED = "task_triggered"
SHELL_RESULT = "shell_result"
PROMPT_DISPATCHED = "prompt_dispatched"

# -- Backend-specific --------------------------------------------------------
THINKING = "thinking"
FILE_WRITE = "file_write"
