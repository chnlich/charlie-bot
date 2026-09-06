"""Canonical event wire-name constants.

Every raw event dict in the system carries a ``"type"`` field whose value is
one of the strings defined here; a few constants instead name a subtype value
or payload key (their comments say which).  Import constants from this module
instead of hard-coding the strings at construction and consumption sites.
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
SCHEDULED_RUN_SKIPPED = "scheduled_run_skipped"

# -- Agent relay -------------------------------------------------------------
# Cross-session agent message: carries the caller session's provenance and is
# never a real user message (the authorization gate excludes it by type).
AGENT_MESSAGE = "agent_message"

# -- Slack -------------------------------------------------------------------
# A reply the master posted to its session's Slack thread through
# ``charliebot slack reply``; its ``slack_reply`` payload names the summon it
# answers, which the round-end audit reads (src/core/slack_listener.py).
SLACK_REPLY = "slack_reply"

# -- Context -----------------------------------------------------------------
CONTEXT_COMPACTED = "context_compacted"
CONTEXT_COMPACT_FAILED = "context_compact_failed"
RESUME_CONTEXT_DROPPED = "resume_context_dropped"
# A backend emits this ``subtype`` on a ``system`` event when the conversation
# crosses a compaction boundary; the event carries its ``trigger`` and token
# counts under the ``compact_metadata`` key.  Both are persisted wire values:
# tier resolution re-reads them from chat_events.jsonl history
# (src/core/session_usage.py), so producer and consumers share one definition.
COMPACT_BOUNDARY = "compact_boundary"
COMPACT_METADATA = "compact_metadata"
# A backend emits this ``subtype`` on a ``system`` event carrying an
# already-resolved context reading; the event carries the reading's model and
# token counts under the same-named ``context_reading`` payload key.  Both are
# persisted wire values re-read from chat_events.jsonl by
# src/core/session_usage.py, so producer and consumers share one definition.
CONTEXT_READING = "context_reading"

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

# -- Turn-end model attribution ----------------------------------------------
# Synthetic round-end notice from the master run detector — never a CLI stream
# event (same CharlieBot-synthesized top-level-type family as
# backend_overlay_inactive): a pinned cc-family backend's visible reply was
# served by model(s) outside the configured model's family. Fields: backend,
# configured_model, served_models (raw model names, first-appearance order).
MODEL_FALLBACK_NOTICE = "model_fallback_notice"

# -- Overlay declaration -----------------------------------------------------
# One alert for every fenceless run: undeclared prompt_overlay and
# declared-but-unreadable emit the same backend_overlay_inactive event, told
# apart by its reason field ("undeclared" | "unreadable"). An unreadable
# overlay degrades to a fenceless run — the read failure does NOT raise and
# never kills the wake.
BACKEND_OVERLAY_INACTIVE = "backend_overlay_inactive"
# Legacy render-only constant: history events carry no reason field and render
# as undeclared. New code never emits it.
BACKEND_OVERLAY_UNDECLARED = "backend_overlay_undeclared"
