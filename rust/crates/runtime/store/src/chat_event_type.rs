//! The chat event types and system subtypes that the store folds replay.
//!
//! The matches that decide what a type does name every variant, handled or ignored, with no wildcard arm: `is_builtin`,
//! the rendering of built-in types, the run summaries, the subtype matches of system records, and the folds that replay
//! stored records (the task tree, the run records, the socket frames and the message intervals). A variant that one of
//! these leaves out fails the build.
//!
//! The variants are the names that production writers emit, that the store's recorded chat and subagent logs carry,
//! and that the usage fold names. A `type` value outside them is `Unnamed`: the replay pass that meets it counts it
//! through [`UnnamedPass`], which logs each unnamed name once when the pass ends, and the fold goes on. A writer added
//! later shows up in that log until its name is added here. `ChatEventType::of` and `SystemSubtype::of` are the only
//! places that map a stored string to a variant.

use std::collections::BTreeMap;

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
  WebReplyCheck,
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
  ClaudeAccountLoginRequired,
  CommandLifecycle,
  FeatureEvent,
  RateLimitEvent,
  Stream,
  WebStatusSubtree,
  AiTitle,
  AtisLatch,
  Attachment,
  Command,
  Context,
  CostState,
  FileHistorySnapshot,
  FileWrite,
  ImproveCompleted,
  ImproveStopped,
  LastPrompt,
  Mode,
  Observation,
  PermissionMode,
  PrLink,
  QueueOperation,
  Session,
  Thought,
  ToolProgress,
  /// A record with no `type` field.
  Untyped,
  /// A `type` value that no variant names.
  Unnamed(String),
}

impl ChatEventType {
  /// The variant that a record's `type` field names.
  pub(crate) fn of(name: Option<&str>) -> Self {
    match name {
      None => Self::Untyped,
      Some(event_types::DISCORD_REPLY) => Self::DiscordReply,
      Some(event_types::SLACK_REPLY) => Self::SlackReply,
      Some(event_types::WEB_ASK_CHECK) => Self::WebAskCheck,
      Some(event_types::DRAFT_FAILED) => Self::DraftFailed,
      Some(event_types::DRAFT_REGISTERED) => Self::DraftRegistered,
      Some(event_types::DRAFT_SENT) => Self::DraftSent,
      Some(event_types::DISCORD_ACK) => Self::DiscordAck,
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
      Some(event_types::WEB_REPLY_CHECK) => Self::WebReplyCheck,
      Some(event_types::WEB_STATUS) => Self::WebStatus,
      Some(event_types::TURN_HOOKS) => Self::TurnHooks,
      Some(event_types::CONTINUATION_RAN) => Self::ContinuationRan,
      Some(event_types::FEISHU_ACK) => Self::FeishuAck,
      Some(event_types::SLACK_ACK) => Self::SlackAck,
      Some(event_types::INPUT_DELIVERED) => Self::InputDelivered,
      Some(event_types::IMPROVE_FAILED) => Self::ImproveFailed,
      Some(event_types::IMPROVE_ITERATION_COMPLETED) => Self::ImproveIterationCompleted,
      Some(event_types::ERROR) => Self::Error,
      Some("message") => Self::Message,
      Some("text") => Self::Text,
      Some(event_types::QUOTA_FAILOVER) => Self::QuotaFailover,
      Some(event_types::FEISHU_REPLY) => Self::FeishuReply,
      Some(event_types::POLICY_TABLE_CHANGED) => Self::PolicyTableChanged,
      Some(event_types::WORKER_SUMMARY) => Self::WorkerSummary,
      Some(event_types::RUN_END) => Self::RunEnd,
      Some(event_types::RUN_ERROR) => Self::RunError,
      Some(event_types::TASK_REPARENTED) => Self::TaskReparented,
      Some(event_types::CLAUDE_ACCOUNT_LOGIN_REQUIRED) => Self::ClaudeAccountLoginRequired,
      Some(event_types::COMMAND_LIFECYCLE) => Self::CommandLifecycle,
      Some(event_types::FEATURE_EVENT) => Self::FeatureEvent,
      Some(event_types::RATE_LIMIT_EVENT) => Self::RateLimitEvent,
      Some("stream") => Self::Stream,
      Some(event_types::WEB_STATUS_SUBTREE) => Self::WebStatusSubtree,
      Some(event_types::AI_TITLE) => Self::AiTitle,
      Some(event_types::ATIS_LATCH) => Self::AtisLatch,
      Some(event_types::ATTACHMENT) => Self::Attachment,
      Some(event_types::COMMAND) => Self::Command,
      Some(event_types::CONTEXT) => Self::Context,
      Some(event_types::COST_STATE) => Self::CostState,
      Some(event_types::FILE_HISTORY_SNAPSHOT) => Self::FileHistorySnapshot,
      Some(event_types::FILE_WRITE) => Self::FileWrite,
      Some(event_types::IMPROVE_COMPLETED) => Self::ImproveCompleted,
      Some(event_types::IMPROVE_STOPPED) => Self::ImproveStopped,
      Some(event_types::LAST_PROMPT) => Self::LastPrompt,
      Some(event_types::MODE) => Self::Mode,
      Some(event_types::OBSERVATION) => Self::Observation,
      Some(event_types::PERMISSION_MODE) => Self::PermissionMode,
      Some(event_types::PR_LINK) => Self::PrLink,
      Some(event_types::QUEUE_OPERATION) => Self::QueueOperation,
      Some(event_types::SESSION) => Self::Session,
      Some(event_types::THOUGHT) => Self::Thought,
      Some(event_types::TOOL_PROGRESS) => Self::ToolProgress,
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
      | Self::WebReplyCheck
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
      | Self::ClaudeAccountLoginRequired
      | Self::CommandLifecycle
      | Self::FeatureEvent
      | Self::RateLimitEvent
      | Self::Stream
      | Self::WebStatusSubtree
      | Self::AiTitle
      | Self::AtisLatch
      | Self::Attachment
      | Self::Command
      | Self::Context
      | Self::CostState
      | Self::FileHistorySnapshot
      | Self::FileWrite
      | Self::ImproveCompleted
      | Self::ImproveStopped
      | Self::LastPrompt
      | Self::Mode
      | Self::Observation
      | Self::PermissionMode
      | Self::PrLink
      | Self::QueueOperation
      | Self::Session
      | Self::Thought
      | Self::ToolProgress
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
  Stderr,
  TaskUpdated,
  ApiRetry,
  CodeChangePublished,
  CommandsChanged,
  HookProgress,
  HookResponse,
  HookStarted,
  Informational,
  ModelConsentFallback,
  ModelRefusalFallback,
  PermissionDenied,
  TuiMenuDismissed,
  TurnDuration,
  VcsStateChanged,
  /// A system record with no `subtype` field.
  Plain,
  /// A `subtype` value that no variant names.
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
      Some(event_types::HANG_DIAGNOSTICS) => Self::HangDiagnostics,
      Some("thinking_tokens") => Self::ThinkingTokens,
      Some("stderr") => Self::Stderr,
      Some(event_types::TASK_UPDATED) => Self::TaskUpdated,
      Some(event_types::API_RETRY) => Self::ApiRetry,
      Some(event_types::CODE_CHANGE_PUBLISHED) => Self::CodeChangePublished,
      Some(event_types::COMMANDS_CHANGED) => Self::CommandsChanged,
      Some(event_types::HOOK_PROGRESS) => Self::HookProgress,
      Some(event_types::HOOK_RESPONSE) => Self::HookResponse,
      Some(event_types::HOOK_STARTED) => Self::HookStarted,
      Some(event_types::INFORMATIONAL) => Self::Informational,
      Some(event_types::MODEL_CONSENT_FALLBACK) => Self::ModelConsentFallback,
      Some(event_types::MODEL_REFUSAL_FALLBACK) => Self::ModelRefusalFallback,
      Some(event_types::PERMISSION_DENIED) => Self::PermissionDenied,
      Some(event_types::TUI_MENU_DISMISSED) => Self::TuiMenuDismissed,
      Some(event_types::TURN_DURATION) => Self::TurnDuration,
      Some(event_types::VCS_STATE_CHANGED) => Self::VcsStateChanged,
      Some(other) => Self::Unnamed(other.to_string()),
    }
  }
}

/// The unnamed stored types and subtypes that one replay pass meets.
///
/// A pass counts each unnamed name it meets. When the pass ends, it logs each name once, at error level, with the fold
/// that ran the pass, the kind (`event type` or `system subtype`), the name, the number of records that carried it and
/// the source of the first of them. The fold goes on past each unnamed record. Dropping the pass ends it, so an early
/// return logs the names met so far.
pub struct UnnamedPass {
  fold: &'static str,
  names: BTreeMap<(&'static str, String), UnnamedCount>,
}

/// The records of one unnamed name in a pass, and the source of the first one.
struct UnnamedCount {
  count: u64,
  first_source: Option<String>,
}

impl UnnamedPass {
  /// Starts a pass of `fold`, the function that runs it. Each line the pass logs names that fold.
  pub fn new(fold: &'static str) -> Self {
    Self { fold, names: BTreeMap::new() }
  }

  /// Counts one record whose `kind` value `name` no variant names. `source` says where the record came from: its id,
  /// or the session whose log holds it.
  pub fn note(&mut self, kind: &'static str, name: &str, source: Option<&str>) {
    let counted = self
      .names
      .entry((kind, name.to_owned()))
      .or_insert_with(|| UnnamedCount { count: 0, first_source: source.map(str::to_owned) });
    counted.count += 1;
  }

  /// Counts a record of a replay loop when its `type` names no variant. The loop calls this once for each record it
  /// visits, so each record counts once in the pass.
  pub fn note_type(&mut self, record_type: Option<&str>, source: Option<&str>) {
    if let ChatEventType::Unnamed(name) = ChatEventType::of(record_type) {
      self.note("event type", &name, source);
    }
  }
}

impl Drop for UnnamedPass {
  fn drop(&mut self) {
    for ((kind, name), counted) in &self.names {
      tracing::error!(
        target: "charliebot_store",
        fold = self.fold,
        kind = *kind,
        name = name.as_str(),
        count = counted.count,
        source = ?counted.first_source,
        "store_fold_unnamed_type"
      );
    }
  }
}

/// Logs a stored type or subtype that no pass counts, with the fold that met it and the record it came from. A caller
/// that replays many records uses [`UnnamedPass`] instead, which logs each name once per pass.
pub(crate) fn log_unnamed(fold: &str, kind: &str, name: &str, source: Option<&str>) {
  tracing::error!(target: "charliebot_store", fold, kind, name, source = ?source, "store_fold_unnamed_type");
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The stored types the folds replay without a log line. A name missing from `of` would log at every record.
  const NAMED_TYPES: &[&str] = &[
    event_types::AI_TITLE,
    event_types::ATIS_LATCH,
    event_types::ATTACHMENT,
    event_types::COMMAND,
    event_types::CONTEXT,
    event_types::COST_STATE,
    event_types::FILE_HISTORY_SNAPSHOT,
    event_types::FILE_WRITE,
    event_types::IMPROVE_COMPLETED,
    event_types::IMPROVE_STOPPED,
    event_types::LAST_PROMPT,
    event_types::MODE,
    event_types::OBSERVATION,
    event_types::PERMISSION_MODE,
    event_types::PR_LINK,
    event_types::QUEUE_OPERATION,
    event_types::SESSION,
    event_types::THOUGHT,
    event_types::TOOL_PROGRESS,
  ];

  const NAMED_SUBTYPES: &[&str] = &[
    event_types::API_RETRY,
    event_types::CODE_CHANGE_PUBLISHED,
    event_types::COMMANDS_CHANGED,
    event_types::HOOK_PROGRESS,
    event_types::HOOK_RESPONSE,
    event_types::HOOK_STARTED,
    event_types::INFORMATIONAL,
    event_types::MODEL_CONSENT_FALLBACK,
    event_types::MODEL_REFUSAL_FALLBACK,
    event_types::PERMISSION_DENIED,
    event_types::TUI_MENU_DISMISSED,
    event_types::TURN_DURATION,
    event_types::VCS_STATE_CHANGED,
  ];

  #[test]
  fn every_stored_name_the_folds_replay_maps_to_a_variant() {
    for name in NAMED_TYPES {
      assert!(!matches!(ChatEventType::of(Some(name)), ChatEventType::Unnamed(_)), "{name}");
    }
    for name in NAMED_SUBTYPES {
      assert!(!matches!(SystemSubtype::of(Some(name)), SystemSubtype::Unnamed(_)), "{name}");
    }
  }
}
