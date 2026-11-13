//! Canonical wire names for events, tools and log lines, shared by their writers and readers.

pub const ASSISTANT: &str = "assistant";
pub const USER: &str = "user";
pub const INPUT_DELIVERED: &str = "input_delivered";
pub const ERROR: &str = "error";
pub const RESULT: &str = "result";
pub const SYSTEM: &str = "system";
pub const TOOL_USE: &str = "tool_use";
pub const TOOL_RESULT: &str = "tool_result";
/// The id that pairs a `tool_result` block with the `tool_use` block it answers.
pub const TOOL_USE_ID: &str = "tool_use_id";
/// The field a top-level `tool_result` event names its tool in; a `tool_use` block names it in `name`.
pub const TOOL_NAME: &str = "tool_name";
/// The tool_use id whose subagent transcript line an event belongs to; an event without it belongs to the turn.
pub const PARENT_TOOL_USE_ID: &str = "parent_tool_use_id";
/// The `input` key of a file tool call: the path a `Read`, `Edit` or `Write` tool names.
pub const TOOL_INPUT_FILE_PATH: &str = "file_path";
/// The Claude Code tool that reads a file. The read-budget hook counts its `file.content` and a read-only one-shot
/// disallows it.
pub const READ_TOOL: &str = "Read";
/// The Claude Code tool that writes a whole file. Its output echoes the written content, which the model never
/// reads, so the read-budget hook counts nothing for it.
pub const WRITE_TOOL: &str = "Write";
/// The Claude Code tool that replaces a string in a file. Its input names the file through `TOOL_INPUT_FILE_PATH`.
pub const EDIT_TOOL: &str = "Edit";
/// The Claude Code tool that applies several string replacements in one call. Its output echoes the written
/// content, like `WRITE_TOOL`.
pub const MULTI_EDIT_TOOL: &str = "MultiEdit";
/// The Claude Code tool that edits a notebook. Its output echoes the written content, like `WRITE_TOOL`.
pub const NOTEBOOK_EDIT_TOOL: &str = "NotebookEdit";
/// The Claude Code tool that lists files matching a pattern.
pub const GLOB_TOOL: &str = "Glob";
/// The Claude Code tool that searches file contents for a pattern.
pub const GREP_TOOL: &str = "Grep";
/// The Claude Code tool that loads a skill's instructions by name.
pub const SKILL_TOOL: &str = "Skill";
/// The JSON pointer to the content blocks of an event's message: the array a `user` or `assistant` event carries.
pub const MESSAGE_CONTENT_POINTER: &str = "/message/content";
/// The committed message's flag that the page already holds its content from the stream frames, so the message frame
/// carries none of it.
pub const MESSAGE_CONTENT_STREAMED: &str = "content_streamed";
pub const MASTER_DONE: &str = "master_done";
pub const ASSISTANT_ERROR: &str = "assistant_error";
pub const QUOTA_EXHAUSTED: &str = "quota_exhausted";
pub const STILL_THINKING: &str = "still_thinking";
pub const THINKING_SECONDS: &str = "thinking_seconds";
pub const INPUT_EVENT_ID: &str = "input_event_id";
pub const INPUT_EVENT_IDS: &str = "input_event_ids";
/// The task-input event's attachment list: the file records the operator attached to the message.
pub const UPLOADED_FILES: &str = "uploaded_files";
/// The task-input event's input kind, such as `voice`.
pub const INPUT_MODE: &str = "input_mode";
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
/// The id of one pending input: a task's queued message that a wake admits.
pub const INPUT_ID: &str = "input_id";
/// The session an `agent_message` came from.
pub const FROM_SESSION: &str = "from_session";
/// The display name of the session an `agent_message` came from.
pub const FROM_SESSION_NAME: &str = "from_session_name";
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
pub const WEB_REPLY_CHECK: &str = "web_reply_check";
pub const CONTEXT_COMPACTED: &str = "context_compacted";
pub const CONTEXT_COMPACT_FAILED: &str = "context_compact_failed";
pub const RESUME_CONTEXT_DROPPED: &str = "resume_context_dropped";
pub const RESUME_REASON_ANCHOR_MISSING: &str = "anchor_missing";
pub const RESUME_REASON_TRANSCRIPT_MISSING: &str = "transcript_missing";
pub const COMPACT_BOUNDARY: &str = "compact_boundary";
pub const COMMAND_PROGRESS: &str = "command_progress";
pub const TASK_STARTED: &str = "task_started";
pub const TASK_PROGRESS: &str = "task_progress";
pub const TASK_NOTIFICATION: &str = "task_notification";
pub const COMPACT_METADATA: &str = "compact_metadata";
pub const COMPACT_PRE_TOKENS: &str = "pre_tokens";
pub const COMPACT_POST_TOKENS: &str = "post_tokens";
/// The `system` event's `status` subtype field naming how a compaction ended; a failed compaction carries `failed`.
pub const COMPACT_RESULT: &str = "compact_result";
/// The `system` event's `status` subtype field carrying a failed compaction's error text.
pub const COMPACT_ERROR: &str = "compact_error";
pub const CONTEXT_READING: &str = "context_reading";
pub const CONTEXT_TOKENS: &str = "context_tokens";
pub const CONTEXT_FULL: &str = "context_full";
pub const CONTEXT_COMPACT_AT: &str = "context_compact_at";
pub const CLONE_START: &str = "clone_start";
/// The agent registered a message or publication for the operator to approve. Its card carries the full body.
pub const DRAFT_REGISTERED: &str = "draft_registered";
/// The operator's send went out; the event wakes the session that registered the draft.
pub const DRAFT_SENT: &str = "draft_sent";
/// A send failed. `outcome` reads `rejected` or `unknown`, and the server never retries it.
pub const DRAFT_FAILED: &str = "draft_failed";
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
/// The run header's display label for the backend that ran.
pub const BACKEND_LABEL: &str = "backend_label";
/// The run header's reference to the run's launch prompt file.
pub const LAUNCH_PROMPT_REF: &str = "launch_prompt_ref";
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
/// The `quota_failover` field naming why the backend changed.
pub const QUOTA_FAILOVER_REASON: &str = "reason";
/// The `quota_failover` field naming the `backends.pools` entry that moved the work to another member.
pub const QUOTA_FAILOVER_SET: &str = "set";
/// The `quota_failover` field carrying the handoff summary state of the retry the change started.
pub const QUOTA_FAILOVER_SUMMARY: &str = "summary";
/// The [`QUOTA_FAILOVER_SUMMARY`] value of a retry whose handoff summary was assembled.
pub const QUOTA_SUMMARY_OK: &str = "ok";
/// The [`QUOTA_FAILOVER_SUMMARY`] value that opens an unassembled summary; the reason follows a colon and a space.
pub const QUOTA_SUMMARY_UNAVAILABLE: &str = "unavailable";
/// The reason of a change forced by a spent quota.
pub const QUOTA_REASON_QUOTA_ERROR: &str = "quota_error";
/// The reason of a change forced by a member that refused authentication or model access.
pub const QUOTA_REASON_ACCESS_REFUSED: &str = "access_refused";
/// The reason of a change away from a member whose call outlived its time cap.
pub const QUOTA_REASON_TIMEOUT: &str = "timeout";
/// The reason of a change away from a pool member whose iteration failed for a reason other than its quota.
pub const QUOTA_REASON_ITERATION_FAILURE: &str = "iteration_failure";
/// The reason of a turn-start change away from a member near exhaustion.
pub const QUOTA_REASON_NEAR_EXHAUSTION: &str = "near_exhaustion";
/// The reason of a turn-start change back to a subscription that reset.
pub const QUOTA_REASON_SUBSCRIPTION_RESET: &str = "subscription_reset";
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
/// The share of the input tokens that the provider served from its cache.
pub const USAGE_CACHED_INPUT_TOKENS: &str = "cached_input_tokens";
pub const RESULT_TOTAL_COST_USD: &str = "total_cost_usd";
/// The native session a result event names: the backend's own session id, such as Claude Code's session or a Codex
/// thread. Its `total_cost_usd` is that session's running total, so the usage fold counts only the increase.
pub const NATIVE_SESSION_ID: &str = "session_id";
/// The `modelUsage` entry field that says how the model's cost was priced.
pub const COST_BASIS: &str = "costBasis";
/// The [`COST_BASIS`] value of a model whose cost the backend cannot price.
pub const COST_BASIS_UNKNOWN: &str = "unknown";
/// The session usage field that names a cost error: a native session's cumulative reading fell.
pub const TOTAL_COST_ERROR: &str = "total_cost_error";
/// The per-model usage table, keyed by model name, that a result event or a cost-state row carries.
pub const MODEL_USAGE: &str = "modelUsage";
/// The result event's context snapshot: the model, its context limit and the token counts at the result.
pub const CONTEXT_SNAPSHOT: &str = "context_snapshot";

/// The server's ready log event: the startup path logs it once the ready-phase services have started, and the
/// recovery check's digest drops it as a timing-only line.
pub const SERVER_READY_EVENT: &str = "server_ready";
/// One startup segment's log event: the startup clock logs one per segment, and the recovery check's digest drops it
/// as a timing-only line.
pub const STARTUP_SEGMENT_EVENT: &str = "startup_segment";

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

/// The truth value of a JSON value: `None`, `False`, zero, and empty strings, arrays and objects are false.
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
/// platform key; the Slack, Discord and Feishu tests pin every key to this list.
pub const THREAD_PLATFORM_KEYS: &[&str] = &["slack", "discord", "feishu"];

/// Return whether an event is a thread platform summon: an `agent_message` whose `slack`, `discord` or `feishu` block
/// has no `nudge_of`. A nudge wakes a session on a thread it already joined and repeats no request.
pub fn is_platform_summon(event: &impl EventFields) -> bool {
  event.field("type").and_then(EventFields::as_str_field) == Some(AGENT_MESSAGE)
    && THREAD_PLATFORM_KEYS
      .iter()
      .any(|key| event.field(key).is_some_and(|block| block.is_object() && !block.has_field(NUDGE_OF)))
}
