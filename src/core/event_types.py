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
# Payload keys of a MASTER_DONE event, all persisted wire values re-read from
# chat_events.jsonl. still_thinking marks a round end that left another queued
# item running (no separator is rendered for it); the live panel reads it too
# (web/static/js/websocket.js). thinking_seconds is the length of the round's
# continuous busy interval, copied verbatim onto the rendered separator
# message whose name the chat JS reads (src/core/message_aggregator.py,
# web/static/js/chat/rendering.js). input_event_id names the user event the
# round answers: the Slack round audit re-reads it, and the slack_notice and
# slack_backfill payloads carry the same name with the same value
# (src/core/slack_listener.py).
STILL_THINKING = "still_thinking"
THINKING_SECONDS = "thinking_seconds"
INPUT_EVENT_ID = "input_event_id"

# -- Worker / delegation -----------------------------------------------------
TASK_DELEGATED = "task_delegated"
# The TASK_DELEGATED payload key carrying the persisted SpawnRequest fields the
# boot recovery re-reads (src/core/init_worker_recovery.py); the projection
# copies it (src/core/message_aggregator.py). Persisted wire value.
DELEGATE_INVOCATION = "delegate_invocation"
WORKER_SUMMARY = "worker_summary"
COMPLETE = "complete"

# -- Scheduler / handler ----------------------------------------------------
HANDLER_RESULT = "handler_result"
# The handler_result event's ``status`` field: the renderer turns ``ok`` into a
# ✓ system line and anything else into ✗. Persisted wire values; the producer
# (src/core/scheduler.py) and the renderer (src/core/message_aggregator.py)
# share this one spelling, so a one-site edit cannot fork the pair.
HANDLER_STATUS_OK = "ok"
HANDLER_STATUS_ERROR = "error"
SCHEDULED_TRIGGER = "scheduled_trigger"
SCHEDULED_RUN_SKIPPED = "scheduled_run_skipped"

# -- Backend run lifecycle ---------------------------------------------------
# Run-start session adoption: the first event a spawned backend's run stream
# yields names the session id the run attached to (opencode, codex, gemini,
# charlie-code, antigravity). The master funnel persists it as the chat
# history's run-start marker (the stable-history projection's interval key)
# and the worker funnel keeps it as the worker log's session-id record (the
# token tally's codex reconciliation reads it from the raw line); neither
# funnel renders it — the worker projection skips it before row construction.
SESSION_ATTACHED = "session_attached"

# -- Agent relay -------------------------------------------------------------
# Cross-session agent message: carries the caller session's provenance and is
# never a real user message (the authorization gate excludes it by type).
AGENT_MESSAGE = "agent_message"

# -- Slack -------------------------------------------------------------------
# A reply the master posted to its session's Slack thread through
# ``charliebot slack reply``; the same-named ``slack_reply`` payload names the
# summon it answers, which the round-end audit reads
# (src/core/slack_listener.py). Both uses share this one constant, as with
# CONTEXT_READING below.
SLACK_REPLY = "slack_reply"

# -- Context -----------------------------------------------------------------
CONTEXT_COMPACTED = "context_compacted"
CONTEXT_COMPACT_FAILED = "context_compact_failed"
RESUME_CONTEXT_DROPPED = "resume_context_dropped"
# The resume_context_dropped event's reason field: the resume pre-flight found
# no anchor at all, or an anchor whose transcript is gone. The producer
# (src/agents/master_cc_run.py) and the renderer (src/core/message_aggregator.py)
# share this one spelling, so a one-site edit cannot fork the pair.
RESUME_REASON_ANCHOR_MISSING = "anchor_missing"
RESUME_REASON_TRANSCRIPT_MISSING = "transcript_missing"
# A backend emits this ``subtype`` on a ``system`` event when the conversation
# crosses a compaction boundary; the event carries its ``trigger`` and token
# counts under the ``compact_metadata`` key.  Both are persisted wire values:
# tier resolution re-reads them from chat_events.jsonl history
# (src/core/session_usage.py), so producer and consumers share one definition.
COMPACT_BOUNDARY = "compact_boundary"
# A backend emits this ``subtype`` on a ``system`` event while one of its
# commands is still running at a progress tick, and once more when the command
# is terminated at the cap; ``content`` carries the rendered chat note. The
# aggregator renders the note as a system message (src/core/message_aggregator.py).
COMMAND_PROGRESS = "command_progress"
# Legacy render-only ``system`` subtype: no producer remains, but persisted
# history events still render as system messages. New code never emits it.
TUI_MENU_DISMISSED = "tui_menu_dismissed"
COMPACT_METADATA = "compact_metadata"
# Token counts a compaction event carries. Both are persisted wire values and both live as inner
# keys of a ``compact_metadata`` payload — on a ``compact_boundary`` system event and on the
# synthesized ``context_compacted`` event alike, which passes the upstream payload through whole
# (src/core/streaming.py). ``pre_tokens`` is the count the compaction crossed, which the projection
# reads from the synthesized event's payload (src/core/message_aggregator.py); ``post_tokens`` is
# the post-compaction count the usage resolver re-reads from the boundary payload
# (src/core/session_usage.py). A top-level ``pre_tokens`` on a ``context_compacted`` event is the
# legacy shape events persisted before the payload move carry; the renderer still reads it when the
# payload is absent. The Claude CLI stream and the PostCompact hook payload carry the same names.
COMPACT_PRE_TOKENS = "pre_tokens"
COMPACT_POST_TOKENS = "post_tokens"
# A backend emits this ``subtype`` on a ``system`` event carrying an
# already-resolved context reading; the event carries the reading's model and
# token counts under the same-named ``context_reading`` payload key.  Both are
# persisted wire values re-read from chat_events.jsonl by
# src/core/session_usage.py, so producer and consumers share one definition.
CONTEXT_READING = "context_reading"
# Inner keys of a ``context_reading`` payload and of the usage dict the usage
# resolver serves to the panel (src/core/session_usage.py documents the shape;
# src/core/codex_usage.py builds the Codex variant). Persisted wire values:
# the resolver re-reads the payload keys from chat_events.jsonl, and the panel
# JS (web/static/js/sidebar/session-view.js) reads the usage-dict names, so
# producer and consumers share one definition per name.
CONTEXT_TOKENS = "context_tokens"
CONTEXT_FULL = "context_full"
CONTEXT_COMPACT_AT = "context_compact_at"

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
# Sidebar-channel notification that one node's durable task facts changed. The
# payload names the node and the fact type only; clients re-read the affected
# rows, never trusting the notification itself.
TASK_TREE_CHANGED = "task_tree_changed"
# fact_type label carried by the tree notification a Run launch emits after its
# durable process identity lands (src/core/runs.py record_launch). Launch is a
# metadata write, not a control event, so this label exists only on the wire —
# it is never an appended chat_events fact type.
RUN_LAUNCHED = "run_launched"

# -- Task tree control events (schema_version=2) -----------------------------
# Durable task facts appended to a session's chat_events.jsonl through the
# task-tree owner (src/core/task_sessions.py) and the run owner (src/core/runs.py).
# Every event carries the common header id/type/timestamp/actor/source_session_id;
# request_id fields give duplicate requests a stable, dedup-able identity.
TASK_CREATED = "task_created"
TASK_CLOSE_REQUESTED = "task_close_requested"
TASK_CLOSED = "task_closed"
TASK_REOPENED = "task_reopened"
RUN_STOP_REQUESTED = "run_stop_requested"
RUN_FINISHED = "run_finished"
CHILD_REPORT = "child_report"
PROMPT_CHANGED = "prompt_changed"
TASK_IMPORTED = "task_imported"
# Transcript-projection event types (src/core/worker_transcript.py): the worker
# node's main-chat view synthesizes one header line per Run and one delivery
# close after the last one. They exist only in the projected event list and on
# the wire — never appended to any events file on disk.
RUN_HEADER = "run_header"
RUN_DELIVERY = "run_delivery"
# An operator's durable confirmation that specific task inputs were actually
# handled out-of-band (the terminal-driven TUI node's normal case). The event
# names the exact input ids; the fold treats them like a successful run's
# acknowledged batch, so they stop pending while anything later stays pending.
TASK_INPUT_ACKNOWLEDGED = "task_input_acknowledged"

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
# detection chain consumes the same type on read-back. The event carries the
# status object under the ``rate_limit_info`` payload key — a persisted wire
# value the emit site (src/cli/claude_sub_bridge.py) and every reader
# (src/core/spawner_events.py, src/core/claude_relay.py, src/agents/worker.py,
# src/core/improve_command.py) share through this constant.
RATE_LIMIT_EVENT = "rate_limit_event"
RATE_LIMIT_INFO = "rate_limit_info"

# -- Claude account pool -----------------------------------------------------
# Operator notice from the account pool (src/core/claude_accounts.py): one login
# lost its credentials or failed to authenticate and needs an interactive
# `claude /login` in ``config_dir``. Fields: account, config_dir, reason
# ("auth_failed" | "empty_credentials"). The chat renders it account-free; the
# account and directory are for the server log and the usage panel. The emit
# sites also label their server-log lines with this constant, so the chat event
# type and the log label stay the same string.
CLAUDE_ACCOUNT_LOGIN_REQUIRED = "claude_account_login_required"

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
# apart by its reason field. An unreadable overlay degrades to a fenceless run
# — the read failure does NOT raise and never kills the wake.
BACKEND_OVERLAY_INACTIVE = "backend_overlay_inactive"
# The reason field's two values; the producer (src/agents/master_cc_run.py) and
# the renderer (src/core/message_aggregator.py) share this one spelling, so a
# one-site edit cannot fork the pair.
OVERLAY_REASON_UNDECLARED = "undeclared"
OVERLAY_REASON_UNREADABLE = "unreadable"
# Legacy render-only constant: history events carry no reason field and render
# as undeclared. New code never emits it.
BACKEND_OVERLAY_UNDECLARED = "backend_overlay_undeclared"

# -- Usage block -------------------------------------------------------------
# Keys of a result event's ``usage`` dict (``make_result_event`` in
# src/agents/backends/base.py builds it). The names are the Anthropic Messages
# API's usage-block names, and they are persisted wire values: the token tally
# re-reads them from Claude Code transcripts (src/core/token_tally.py) and the
# proxy answers carry the same shape (src/api/anthropic_proxy.py). The Codex
# rollout wire carries same-named ``input_tokens``/``output_tokens`` from a
# different upstream (src/core/codex_usage.py, src/core/codex_pricing.py);
# those readers keep their literals.
USAGE_INPUT_TOKENS = "input_tokens"
USAGE_OUTPUT_TOKENS = "output_tokens"
USAGE_CACHE_READ_INPUT_TOKENS = "cache_read_input_tokens"
USAGE_CACHE_CREATION_INPUT_TOKENS = "cache_creation_input_tokens"
# Top-level cost field of a result event (the same CC-compatible envelope
# make_result_event builds; src/cli/claude_sub.py emits the same shape). A
# persisted wire value the cost fold re-reads (src/core/session_usage.py);
# the resolver's usage dict reuses the name for the panel.
RESULT_TOTAL_COST_USD = "total_cost_usd"

# -- Shared event predicates -------------------------------------------------
# One definition, two consumers: the takeoff authorization gate
# (src/core/takeoff_gate.py) and the task-tree fold's input candidacy
# (src/core/task_sessions.py) both judge USER events through this function,
# so no site can fork the rule.


def is_real_user_message(event: dict) -> bool:
  """Real user message = ET.USER with string content.

  Excludes trigger events (a different type) and the Claude CLI's tool-result
  echoes: the CLI persists each tool result as a ``user``-type event whose
  content is a list of tool_result blocks (src/cli/claude_sub_bridge.py), and
  that echo is tool output, never a message someone sent.
  """
  if event.get("type") != USER:
    return False
  return isinstance(event.get("content"), str)
