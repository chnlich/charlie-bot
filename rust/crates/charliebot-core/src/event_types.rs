//! Canonical event wire names shared by event writers and readers.

pub const ASSISTANT: &str = "assistant";
pub const USER: &str = "user";
pub const INPUT_DELIVERED: &str = "input_delivered";
pub const ERROR: &str = "error";
pub const RESULT: &str = "result";
pub const SYSTEM: &str = "system";
pub const TOOL_USE: &str = "tool_use";
pub const TOOL_RESULT: &str = "tool_result";
pub const MASTER_DONE: &str = "master_done";
pub const ASSISTANT_ERROR: &str = "assistant_error";
pub const QUOTA_EXHAUSTED: &str = "quota_exhausted";
pub const STILL_THINKING: &str = "still_thinking";
pub const THINKING_SECONDS: &str = "thinking_seconds";
pub const INPUT_EVENT_ID: &str = "input_event_id";
pub const INPUT_EVENT_IDS: &str = "input_event_ids";
pub const TASK_DELEGATED: &str = "task_delegated";
pub const DELEGATE_INVOCATION: &str = "delegate_invocation";
pub const WORKER_SUMMARY: &str = "worker_summary";
pub const COMPLETE: &str = "complete";
pub const HANDLER_RESULT: &str = "handler_result";
pub const HANDLER_STATUS_OK: &str = "ok";
pub const HANDLER_STATUS_ERROR: &str = "error";
pub const SCHEDULED_TRIGGER: &str = "scheduled_trigger";
pub const SCHEDULED_RUN_SKIPPED: &str = "scheduled_run_skipped";
pub const SESSION_ATTACHED: &str = "session_attached";
pub const AGENT_MESSAGE: &str = "agent_message";
pub const WEB_REPLY: &str = "web_reply";
pub const WEB_REPLY_NOTICE: &str = "web_reply_notice";
pub const WEB_STATUS: &str = "web_status";
pub const WEB_ASK_CHECK: &str = "web_ask_check";
pub const CONTEXT_COMPACTED: &str = "context_compacted";
pub const CONTEXT_COMPACT_FAILED: &str = "context_compact_failed";
pub const RESUME_CONTEXT_DROPPED: &str = "resume_context_dropped";
pub const RESUME_REASON_ANCHOR_MISSING: &str = "anchor_missing";
pub const RESUME_REASON_TRANSCRIPT_MISSING: &str = "transcript_missing";
pub const COMPACT_BOUNDARY: &str = "compact_boundary";
pub const COMMAND_PROGRESS: &str = "command_progress";
pub const COMPACT_METADATA: &str = "compact_metadata";
pub const COMPACT_PRE_TOKENS: &str = "pre_tokens";
pub const COMPACT_POST_TOKENS: &str = "post_tokens";
pub const CONTEXT_READING: &str = "context_reading";
pub const CONTEXT_TOKENS: &str = "context_tokens";
pub const CONTEXT_FULL: &str = "context_full";
pub const CONTEXT_COMPACT_AT: &str = "context_compact_at";
pub const CLONE_START: &str = "clone_start";
pub const RUNNING_CHANGED: &str = "running_changed";
pub const UNREAD_CHANGED: &str = "unread_changed";
pub const SESSION_RENAMED: &str = "session_renamed";
pub const SESSION_GROUP_CHANGED: &str = "session_group_changed";
pub const TASK_TREE_CHANGED: &str = "task_tree_changed";
pub const RUN_LAUNCHED: &str = "run_launched";
pub const TASK_CREATED: &str = "task_created";
pub const TASK_CLOSE_REQUESTED: &str = "task_close_requested";
pub const TASK_CLOSED: &str = "task_closed";
pub const TASK_REOPENED: &str = "task_reopened";
pub const RUN_STOP_REQUESTED: &str = "run_stop_requested";
pub const RUN_FINISHED: &str = "run_finished";
pub const RUN_LAUNCH_WITHHELD: &str = "run_launch_withheld";
pub const CHILD_REPORT: &str = "child_report";
pub const PROMPT_CHANGED: &str = "prompt_changed";
pub const TASK_IMPORTED: &str = "task_imported";
pub const RUN_HEADER: &str = "run_header";
pub const RUN_END: &str = "run_end";
pub const RUN_ERROR: &str = "run_error";
pub const RUN_DELIVERY: &str = "run_delivery";
pub const TASK_INPUT_ACKNOWLEDGED: &str = "task_input_acknowledged";
pub const TASK_TRIGGERED: &str = "task_triggered";
pub const THINKING: &str = "thinking";
pub const RATE_LIMIT_EVENT: &str = "rate_limit_event";
pub const RATE_LIMIT_INFO: &str = "rate_limit_info";
pub const BACKEND_SWITCHED: &str = "backend_switched";
pub const QUOTA_FAILOVER: &str = "quota_failover";
pub const MODEL_FALLBACK_NOTICE: &str = "model_fallback_notice";
pub const BACKEND_OVERLAY_INACTIVE: &str = "backend_overlay_inactive";
pub const OVERLAY_REASON_UNREADABLE: &str = "unreadable";
pub const BACKEND_OVERLAY_UNDECLARED: &str = "backend_overlay_undeclared";
pub const USAGE_INPUT_TOKENS: &str = "input_tokens";
pub const USAGE_OUTPUT_TOKENS: &str = "output_tokens";
pub const USAGE_CACHE_READ_INPUT_TOKENS: &str = "cache_read_input_tokens";
pub const USAGE_CACHE_CREATION_INPUT_TOKENS: &str = "cache_creation_input_tokens";
pub const RESULT_TOTAL_COST_USD: &str = "total_cost_usd";

/// Return whether an event is a real user message rather than a tool-result echo.
pub fn is_real_user_message(event: &serde_json::Value) -> bool {
  event.get("type").and_then(serde_json::Value::as_str) == Some(USER)
    && event.get("content").is_some_and(serde_json::Value::is_string)
}

/// The thread platforms that can summon a session. Each platform listener writes its summon block under its
/// `ThreadPlatform.name`; the Slack and Discord tests pin both names to this list.
pub const THREAD_PLATFORM_KEYS: &[&str] = &["slack", "discord"];

/// Return whether an event is a thread platform summon: an `agent_message` whose `slack` or `discord` block has no
/// `nudge_of`. A nudge wakes a session on a thread it already joined and repeats no request.
pub fn is_platform_summon(event: &serde_json::Value) -> bool {
  event.get("type").and_then(serde_json::Value::as_str) == Some(AGENT_MESSAGE)
    && THREAD_PLATFORM_KEYS.iter().any(|key| {
      event.get(*key).and_then(serde_json::Value::as_object).is_some_and(|block| !block.contains_key("nudge_of"))
    })
}
