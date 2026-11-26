//! The single writer for session metadata and chat event logs.

pub mod aggregator;
mod aggregator_hold;
mod chat_event_type;
pub mod chat_events;
pub mod compact;
mod control;
pub mod control_events;
mod cron_binding;
mod error;
mod event_state;
mod event_types;
mod events;
mod fact_checkpoint;
mod fact_scan;
pub mod for_you;
mod held_run_lists;
pub mod hooks;
pub mod layout;
mod listing;
mod lock;
pub mod message_events;
pub mod message_utils;
pub mod metadata;
mod ndjson;
pub mod projection;
mod read_memos;
pub mod reply_order;
mod run_display_state;
mod run_facts;
pub mod run_identity;
pub mod run_logs;
mod run_record;
pub mod run_records;
mod session_events;
pub mod session_fork;
pub mod sidebar_state;
mod store;
pub mod streaming;
pub mod task_child;
pub mod task_node_absence;
mod task_reparent;
mod task_tree;
mod task_tree_state;
mod thinking;
mod tool_rows;
pub mod trigger_files;
pub mod v1_session_conversion;
pub mod v1_sessions;
mod waiting_clicks;
#[cfg(any(test, feature = "write-faults"))]
mod write_fault_types;
#[cfg(any(test, feature = "write-faults"))]
pub mod write_faults;

pub use aggregator::{MessageAggregator, TOOL_PREVIEW_CHARS};
pub use aggregator_hold::hold_aggregator_builds;
pub use chat_event_type::UnnamedPass;
pub use compact::CompactValue;
pub use control::ControlGuard;
pub use error::{StoreError, StoreResult, path_error};
pub use events::{ChatEvent, ChatEventHistory, ChatEventsDelta, SharedChatEvent, SharedChatEventRange};
pub use fact_scan::ScannedLiveFacts;
pub use held_run_lists::RunListHold;
pub use hooks::{EmptyRuntimeEventHooks, EventRenderer, RuntimeEventHooks};
pub use listing::{ArchivedPage, MetadataListing, RawMetadata, SharedSubtreeRoots};
pub use message_utils::{MessageProvenance, SESSION_NOT_FOUND_DETAIL, build_scheduled_trigger_event};
pub use projection::MessageProjection;
pub use run_records::{
  RetryRunFields, RunBlocker, RunDisplayState, RunFacts, RunKind, RunOrder, RunOutcome, RunOutcomeKind, RunPage,
  RunRecord, SequenceRef, StopRequest, StopResult,
};
pub use store::{
  AccountLabelKeeper, AnchorFieldReconciler, HistoryWork, SequenceArchiveHook, SequenceBindingMover,
  SequenceListingFields, Store, StoreHookFuture, StoreHooks,
};
pub use streaming::{SIDEBAR_CHANNEL, STREAM_COALESCE_INTERVAL, StreamingManager, Subscription, session_channel};
pub use task_reparent::{AGENT_REPARENT_SCOPE_REFUSAL, CALLER_SESSION_ID_FIELD};
pub use task_tree::{
  AGENT_CREATE_SCOPE_REFUSAL, CreateTask, NodeFacts, PatchTask, PromptScope, StructuralBlockers,
  TASK_CREATE_REQUEST_ID_REQUIRED, TaskCaller, TaskFacts, TaskState, TaskTreeActivity, TaskTreePage, TaskTreeRow,
  TaskTreeSnapshot,
};
