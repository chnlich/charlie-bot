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
/// The key of an `agent_message` block that marks the message as a nudge and names the event it nudges.
pub const NUDGE_OF: &str = "nudge_of";
pub const WEB_REPLY: &str = "web_reply";
pub const WEB_REPLY_NOTICE: &str = "web_reply_notice";
pub const WEB_STATUS: &str = "web_status";
/// A confirmation through `charliebot web status confirm` that the stored status needs no change this turn. The
/// event records that the turn ran a status command; the For-you view hides it.
pub const WEB_STATUS_CONFIRMED: &str = "web_status_confirmed";
/// The round-end status reminder is an agent_message whose same-named key marks it and names the turn it was posted
/// for. A turn that answers such a reminder posts no further status reminder. A combined reminder also carries the
/// web_reply payload key with nudge_of.
pub const WEB_STATUS_REMINDER: &str = "web_status_reminder";
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
pub const TASK_CREATED: &str = "task_created";
pub const TASK_CLOSE_REQUESTED: &str = "task_close_requested";
pub const TASK_CLOSED: &str = "task_closed";
pub const TASK_REOPENED: &str = "task_reopened";
/// A task moved to another parent. The moved task's chat events carry it; the chat renders nothing for it.
pub const TASK_REPARENTED: &str = "task_reparented";
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
/// The configured model in a `model_fallback_notice` payload.
pub const CONFIGURED_MODEL: &str = "configured_model";
/// The models the backend served instead, in the same payload.
pub const SERVED_MODELS: &str = "served_models";
pub const BACKEND_OVERLAY_INACTIVE: &str = "backend_overlay_inactive";
pub const OVERLAY_REASON_UNREADABLE: &str = "unreadable";
pub const BACKEND_OVERLAY_UNDECLARED: &str = "backend_overlay_undeclared";
pub const USAGE_INPUT_TOKENS: &str = "input_tokens";
pub const USAGE_OUTPUT_TOKENS: &str = "output_tokens";
pub const USAGE_CACHE_READ_INPUT_TOKENS: &str = "cache_read_input_tokens";
pub const USAGE_CACHE_CREATION_INPUT_TOKENS: &str = "cache_creation_input_tokens";
pub const RESULT_TOTAL_COST_USD: &str = "total_cost_usd";
/// The result event's context snapshot: the model, its context limit and the token counts at the result.
pub const CONTEXT_SNAPSHOT: &str = "context_snapshot";

/// Field reads shared by `serde_json::Value` and the store's compact parsed events, so the
/// event-type helpers and the message aggregator read either form in place.
pub trait EventFields: Sized {
  /// The value of an object field, or `None` when this is not an object with that key.
  fn field(&self, key: &str) -> Option<&Self>;

  /// The string content of the value, or `None` for other forms.
  fn as_str_field(&self) -> Option<&str>;

  /// Whether the value is a string.
  fn is_str(&self) -> bool {
    self.as_str_field().is_some()
  }

  /// Whether the value is an object.
  fn is_object(&self) -> bool;

  /// Whether this is an object holding `key`.
  fn has_field(&self, key: &str) -> bool {
    self.is_object() && self.field(key).is_some()
  }

  /// The string items of an array value; other forms have none.
  fn string_items(&self) -> Vec<&str>;

  /// Whether the value is JSON `null`.
  fn is_null(&self) -> bool;

  /// The boolean, or `None` for other forms.
  fn as_bool_field(&self) -> Option<bool>;

  /// The number as an `i64`, or `None` for other forms.
  fn as_i64_field(&self) -> Option<i64>;

  /// The number as a `u64`, or `None` for other forms.
  fn as_u64_field(&self) -> Option<u64>;

  /// The number as an `f64`, or `None` for other forms.
  fn as_f64_field(&self) -> Option<f64>;

  /// The items of an array value, or `None` for other forms.
  fn array_items(&self) -> Option<&[Self]>;

  /// The field count of an object value, or `None` for other forms.
  fn object_len(&self) -> Option<usize>;

  /// An owned `serde_json::Value` copy of this value, for output that must own its JSON.
  fn to_owned_value(&self) -> serde_json::Value;
}

impl EventFields for serde_json::Value {
  fn field(&self, key: &str) -> Option<&Self> {
    self.get(key)
  }

  fn as_str_field(&self) -> Option<&str> {
    self.as_str()
  }

  fn is_object(&self) -> bool {
    self.is_object()
  }

  fn string_items(&self) -> Vec<&str> {
    self.as_array().map(|items| items.iter().filter_map(serde_json::Value::as_str).collect()).unwrap_or_default()
  }

  fn is_null(&self) -> bool {
    self.is_null()
  }

  fn as_bool_field(&self) -> Option<bool> {
    self.as_bool()
  }

  fn as_i64_field(&self) -> Option<i64> {
    self.as_i64()
  }

  fn as_u64_field(&self) -> Option<u64> {
    self.as_u64()
  }

  fn as_f64_field(&self) -> Option<f64> {
    self.as_f64()
  }

  fn array_items(&self) -> Option<&[Self]> {
    self.as_array().map(Vec::as_slice)
  }

  fn object_len(&self) -> Option<usize> {
    self.as_object().map(serde_json::Map::len)
  }

  fn to_owned_value(&self) -> serde_json::Value {
    self.clone()
  }
}

/// Python's truth value of a JSON value: `None`, `False`, zero, and empty strings, arrays and objects are false.
pub fn is_truthy(value: &impl EventFields) -> bool {
  if value.is_null() {
    return false;
  }
  if let Some(flag) = value.as_bool_field() {
    return flag;
  }
  if let Some(number) = value.as_f64_field() {
    return number != 0.0;
  }
  if let Some(text) = value.as_str_field() {
    return !text.is_empty();
  }
  if let Some(items) = value.array_items() {
    return !items.is_empty();
  }
  value.object_len().is_some_and(|len| len > 0)
}

/// Return whether an event is a real user message rather than a tool-result echo.
pub fn is_real_user_message(event: &impl EventFields) -> bool {
  event.field("type").and_then(EventFields::as_str_field) == Some(USER)
    && event.field("content").is_some_and(EventFields::is_str)
}

/// The thread platforms that can summon a session. Each platform listener writes its summon block under its
/// `ThreadPlatform.name`; the Slack and Discord tests pin both names to this list.
pub const THREAD_PLATFORM_KEYS: &[&str] = &["slack", "discord"];

/// Return whether an event is a thread platform summon: an `agent_message` whose `slack` or `discord` block has no
/// `nudge_of`. A nudge wakes a session on a thread it already joined and repeats no request.
pub fn is_platform_summon(event: &impl EventFields) -> bool {
  event.field("type").and_then(EventFields::as_str_field) == Some(AGENT_MESSAGE)
    && THREAD_PLATFORM_KEYS
      .iter()
      .any(|key| event.field(key).is_some_and(|block| block.is_object() && !block.has_field(NUDGE_OF)))
}
