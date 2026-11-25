//! The chat event types and system subtypes that the store folds replay.
//!
//! The matches that decide what a type does name every variant, handled or ignored, with no wildcard arm: `is_builtin`,
//! the rendering of built-in types, the run summaries, and the subtype matches of system records. A variant that one
//! of these leaves out fails the build. A `type` value that no variant covers is `Unnamed`; the fold that meets it
//! logs it and goes on. `ChatEventType::of` and `SystemSubtype::of` are the only places that map a stored string to a
//! variant.

use charliebot_core::event_types;

/// The stored `type` of a chat record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChatEventType {
  DiscordReply,
  SlackReply,
  WebAskCheck,
  DraftFailed,
  DraftRegistered,
  DraftSent,
  DiscordAck,
  WebStatusConfirmed,
  Assistant,
  System,
  ContextCompacted,
  Result,
  AgentMessage,
  AssistantError,
  BackendOverlayInactive,
  BackendOverlayUndeclared,
  BackendSwitched,
  ChildReport,
  CloneStart,
  ContextCompactFailed,
  HandlerResult,
  MasterDone,
  ModelFallbackNotice,
  PromptChanged,
  ResumeContextDropped,
  RunDelivery,
  RunFinished,
  RunHeader,
  RunLaunchWithheld,
  RunStopRequested,
  ScheduledRunSkipped,
  ScheduledTrigger,
  SessionAttached,
  SessionRenamed,
  TaskCloseRequested,
  TaskClosed,
  TaskCreated,
  TaskDelegated,
  TaskImported,
  TaskInputAcknowledged,
  TaskReopened,
  TaskTriggered,
  Thinking,
  ToolResult,
  ToolUse,
  User,
  WebReply,
  WebReplyNotice,
  WebStatus,
  TurnHooks,
  ContinuationRan,
  FeishuAck,
  SlackAck,
  InputDelivered,
  ImproveFailed,
  ImproveIterationCompleted,
  Error,
  Message,
  Text,
  QuotaFailover,
  FeishuReply,
  PolicyTableChanged,
  WorkerSummary,
  RunEnd,
  RunError,
  TaskReparented,
  /// A record with no `type` field.
  Untyped,
  /// A `type` value that no writer version here emitted.
  Unnamed(String),
}

impl ChatEventType {
  /// The variant that a record's `type` field names.
  pub(crate) fn of(name: Option<&str>) -> Self {
    match name {
      None => Self::Untyped,
      Some("discord_reply") => Self::DiscordReply,
      Some("slack_reply") => Self::SlackReply,
      Some(event_types::WEB_ASK_CHECK) => Self::WebAskCheck,
      Some(event_types::DRAFT_FAILED) => Self::DraftFailed,
      Some(event_types::DRAFT_REGISTERED) => Self::DraftRegistered,
      Some(event_types::DRAFT_SENT) => Self::DraftSent,
      Some("discord_ack") => Self::DiscordAck,
      Some(event_types::WEB_STATUS_CONFIRMED) => Self::WebStatusConfirmed,
      Some(event_types::ASSISTANT) => Self::Assistant,
      Some(event_types::SYSTEM) => Self::System,
      Some(event_types::CONTEXT_COMPACTED) => Self::ContextCompacted,
      Some(event_types::RESULT) => Self::Result,
      Some(event_types::AGENT_MESSAGE) => Self::AgentMessage,
      Some(event_types::ASSISTANT_ERROR) => Self::AssistantError,
      Some(event_types::BACKEND_OVERLAY_INACTIVE) => Self::BackendOverlayInactive,
      Some(event_types::BACKEND_OVERLAY_UNDECLARED) => Self::BackendOverlayUndeclared,
      Some(event_types::BACKEND_SWITCHED) => Self::BackendSwitched,
      Some(event_types::CHILD_REPORT) => Self::ChildReport,
      Some(event_types::CLONE_START) => Self::CloneStart,
      Some(event_types::CONTEXT_COMPACT_FAILED) => Self::ContextCompactFailed,
      Some(event_types::HANDLER_RESULT) => Self::HandlerResult,
      Some(event_types::MASTER_DONE) => Self::MasterDone,
      Some(event_types::MODEL_FALLBACK_NOTICE) => Self::ModelFallbackNotice,
      Some(event_types::PROMPT_CHANGED) => Self::PromptChanged,
      Some(event_types::RESUME_CONTEXT_DROPPED) => Self::ResumeContextDropped,
      Some(event_types::RUN_DELIVERY) => Self::RunDelivery,
      Some(event_types::RUN_FINISHED) => Self::RunFinished,
      Some(event_types::RUN_HEADER) => Self::RunHeader,
      Some(event_types::RUN_LAUNCH_WITHHELD) => Self::RunLaunchWithheld,
      Some(event_types::RUN_STOP_REQUESTED) => Self::RunStopRequested,
      Some(event_types::SCHEDULED_RUN_SKIPPED) => Self::ScheduledRunSkipped,
      Some(event_types::SCHEDULED_TRIGGER) => Self::ScheduledTrigger,
      Some(event_types::SESSION_ATTACHED) => Self::SessionAttached,
      Some(event_types::SESSION_RENAMED) => Self::SessionRenamed,
      Some(event_types::TASK_CLOSE_REQUESTED) => Self::TaskCloseRequested,
      Some(event_types::TASK_CLOSED) => Self::TaskClosed,
      Some(event_types::TASK_CREATED) => Self::TaskCreated,
      Some(event_types::TASK_DELEGATED) => Self::TaskDelegated,
      Some(event_types::TASK_IMPORTED) => Self::TaskImported,
      Some(event_types::TASK_INPUT_ACKNOWLEDGED) => Self::TaskInputAcknowledged,
      Some(event_types::TASK_REOPENED) => Self::TaskReopened,
      Some(event_types::TASK_TRIGGERED) => Self::TaskTriggered,
      Some(event_types::THINKING) => Self::Thinking,
      Some(event_types::TOOL_RESULT) => Self::ToolResult,
      Some(event_types::TOOL_USE) => Self::ToolUse,
      Some(event_types::USER) => Self::User,
      Some(event_types::WEB_REPLY) => Self::WebReply,
      Some(event_types::WEB_REPLY_NOTICE) => Self::WebReplyNotice,
      Some(event_types::WEB_STATUS) => Self::WebStatus,
      Some(event_types::TURN_HOOKS) => Self::TurnHooks,
      Some("continuation_ran") => Self::ContinuationRan,
      Some("feishu_ack") => Self::FeishuAck,
      Some("slack_ack") => Self::SlackAck,
      Some(event_types::INPUT_DELIVERED) => Self::InputDelivered,
      Some("improve_failed") => Self::ImproveFailed,
      Some("improve_iteration_completed") => Self::ImproveIterationCompleted,
      Some(event_types::ERROR) => Self::Error,
      Some("message") => Self::Message,
      Some("text") => Self::Text,
      Some(event_types::QUOTA_FAILOVER) => Self::QuotaFailover,
      Some("feishu_reply") => Self::FeishuReply,
      Some("policy_table_changed") => Self::PolicyTableChanged,
      Some(event_types::WORKER_SUMMARY) => Self::WorkerSummary,
      Some(event_types::RUN_END) => Self::RunEnd,
      Some(event_types::RUN_ERROR) => Self::RunError,
      Some(event_types::TASK_REPARENTED) => Self::TaskReparented,
      Some(other) => Self::Unnamed(other.to_string()),
    }
  }

  /// Whether the built-in message table renders this type. Any other type renders through a registered renderer, or
  /// not at all.
  pub(crate) fn is_builtin(&self) -> bool {
    match self {
      Self::System
      | Self::ContextCompacted
      | Self::AgentMessage
      | Self::AssistantError
      | Self::BackendOverlayInactive
      | Self::BackendOverlayUndeclared
      | Self::BackendSwitched
      | Self::ChildReport
      | Self::CloneStart
      | Self::ContextCompactFailed
      | Self::HandlerResult
      | Self::MasterDone
      | Self::ModelFallbackNotice
      | Self::ResumeContextDropped
      | Self::RunDelivery
      | Self::RunHeader
      | Self::ScheduledTrigger
      | Self::TaskClosed
      | Self::TaskDelegated
      | Self::TaskInputAcknowledged
      | Self::TaskReopened
      | Self::Error
      | Self::QuotaFailover
      | Self::WorkerSummary
      | Self::RunEnd
      | Self::RunError => true,
      Self::DiscordReply
      | Self::SlackReply
      | Self::WebAskCheck
      | Self::DraftFailed
      | Self::DraftRegistered
      | Self::DraftSent
      | Self::DiscordAck
      | Self::WebStatusConfirmed
      | Self::Assistant
      | Self::Result
      | Self::PromptChanged
      | Self::RunFinished
      | Self::RunLaunchWithheld
      | Self::RunStopRequested
      | Self::ScheduledRunSkipped
      | Self::SessionAttached
      | Self::SessionRenamed
      | Self::TaskCloseRequested
      | Self::TaskCreated
      | Self::TaskImported
      | Self::TaskTriggered
      | Self::Thinking
      | Self::ToolResult
      | Self::ToolUse
      | Self::User
      | Self::WebReply
      | Self::WebReplyNotice
      | Self::WebStatus
      | Self::TurnHooks
      | Self::ContinuationRan
      | Self::FeishuAck
      | Self::SlackAck
      | Self::InputDelivered
      | Self::ImproveFailed
      | Self::ImproveIterationCompleted
      | Self::Message
      | Self::Text
      | Self::FeishuReply
      | Self::PolicyTableChanged
      | Self::TaskReparented
      | Self::Untyped
      | Self::Unnamed(_) => false,
    }
  }
}

/// The `subtype` of a system record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SystemSubtype {
  Init,
  TaskNotification,
  TaskProgress,
  TaskStarted,
  CompactBoundary,
  ContextReading,
  CommandProgress,
  Status,
  HangDiagnostics,
  ThinkingTokens,
  /// A system record with no `subtype` field.
  Plain,
  /// A `subtype` value that no writer version here emitted.
  Unnamed(String),
}

impl SystemSubtype {
  /// The variant that a system record's `subtype` field names.
  pub(crate) fn of(name: Option<&str>) -> Self {
    match name {
      None => Self::Plain,
      Some("init") => Self::Init,
      Some(event_types::TASK_NOTIFICATION) => Self::TaskNotification,
      Some(event_types::TASK_PROGRESS) => Self::TaskProgress,
      Some(event_types::TASK_STARTED) => Self::TaskStarted,
      Some(event_types::COMPACT_BOUNDARY) => Self::CompactBoundary,
      Some(event_types::CONTEXT_READING) => Self::ContextReading,
      Some(event_types::COMMAND_PROGRESS) => Self::CommandProgress,
      Some("status") => Self::Status,
      Some("hang_diagnostics") => Self::HangDiagnostics,
      Some("thinking_tokens") => Self::ThinkingTokens,
      Some(other) => Self::Unnamed(other.to_string()),
    }
  }
}

/// Logs a stored type or subtype that no writer version emitted, with the fold that met it and the record it came from.
pub(crate) fn log_unnamed(fold: &str, kind: &str, name: &str, source: Option<&str>) {
  tracing::error!(target: "charliebot_store", fold, kind, name, source = ?source, "store_fold_unnamed_type");
}
