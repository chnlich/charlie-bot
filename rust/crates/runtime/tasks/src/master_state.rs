//! Per-session manager work items: the work item and request types, the session block callbacks, and the
//! run-tracking protocol the live turn and delivery modules share.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::backend::Env;
use charliebot_registry::context::FeatureContext;
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use tokio::sync::{oneshot, watch};

pub type PersistAndBroadcastFn = Arc<dyn Fn(String, Value) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;
pub type MarkUnreadFn = Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;
pub type PersistNativeIdFn =
  Arc<dyn Fn(String, String, Option<String>) -> BoxFuture<'static, anyhow::Result<Option<String>>> + Send + Sync>;
pub type PersistAccountLabelFn =
  Arc<dyn Fn(String, String) -> BoxFuture<'static, anyhow::Result<Option<String>>> + Send + Sync>;
pub type ContextStateFn = Arc<
  dyn Fn(String, SessionMetadata) -> BoxFuture<'static, anyhow::Result<(Option<i64>, Option<UtcDateTime>)>>
    + Send
    + Sync,
>;
pub type TaskTreeActivityFn = Arc<dyn Fn(&str) -> anyhow::Result<(bool, String)> + Send + Sync>;
pub type AfterRoundFn =
  Arc<dyn Fn(Arc<CharlieBotConfig>, String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

/// Session block callbacks used by a manager turn.
#[derive(Clone)]
pub struct SessionCallbacks {
  pub features: Arc<FeatureContext>,
  pub persist_and_broadcast: PersistAndBroadcastFn,
  pub mark_unread: MarkUnreadFn,
  pub persist_cc_session_id: PersistNativeIdFn,
  pub persist_account_label: Option<PersistAccountLabelFn>,
  pub context_state: Option<ContextStateFn>,
  pub task_tree_activity: TaskTreeActivityFn,
  pub after_round: Option<AfterRoundFn>,
}

/// The run-tracking operations one live manager turn performs on the queue's shared state: the other sessions'
/// running turns and the running turn's web-reply input ids. The `master_cc_state` module dicts hold the same state in
/// Python. The queue implements it; the run and delivery modules work through it, so they never name the queue type.
pub trait RunTracking: Send + Sync {
  /// The metadata of the turns running in other sessions.
  fn running_turns_except(&self, session_id: &str) -> Vec<SessionMetadata>;
  /// Append one acknowledged input id to the running turn's web-reply binding.
  fn append_running_user_event_id(&self, session_id: &str, input_id: &str);
}

/// The task-tree run whose manager turn owns one work item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRunBinding {
  pub session_id: String,
  pub run_id: String,
  pub transport_dir: PathBuf,
  pub fresh_native_context: bool,
}

/// The in-process identity used to reattach a live manager turn after restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MasterResumeRecord {
  pub pid: Option<u32>,
  pub pid_start: Option<String>,
  pub started_at: UtcDateTime,
  pub raw_log: PathBuf,
  pub user_event_ids: Vec<String>,
}

pub type TaskSpawnCallback = Arc<dyn Fn(u32, Option<String>) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;
/// Persist a manager finish transaction and return whether it also persisted `master_done`.
pub type TaskFinishCallback =
  Arc<dyn Fn(Option<String>, i32, Map<String, Value>, Value) -> BoxFuture<'static, anyhow::Result<bool>> + Send + Sync>;
pub type ResumeLiveness = Arc<dyn Fn() -> bool + Send + Sync>;

/// The inputs for one newly submitted manager turn.
pub struct MasterTurnRequest {
  pub cfg: Arc<CharlieBotConfig>,
  pub session_meta: SessionMetadata,
  pub user_content: String,
  pub callbacks: SessionCallbacks,
  pub user_event_ids: Vec<String>,
  pub task_instructions: String,
  pub task_run: TaskRunBinding,
  pub on_task_spawn: TaskSpawnCallback,
  pub on_task_finish: TaskFinishCallback,
  pub auto_trigger: bool,
  pub backend_option: Option<BackendOption>,
  pub extra_claude_flags: Option<Vec<String>>,
  pub uploaded_files: Option<Vec<Value>>,
  pub extra_env: Option<Env>,
}

/// The inputs needed to reattach a persisted manager turn.
pub struct MasterResumeRequest {
  pub cfg: Arc<CharlieBotConfig>,
  pub session_meta: SessionMetadata,
  pub record: MasterResumeRecord,
  pub callbacks: SessionCallbacks,
  pub is_alive: ResumeLiveness,
  pub task_run: TaskRunBinding,
  pub on_task_spawn: TaskSpawnCallback,
  pub on_task_finish: TaskFinishCallback,
  pub extra_env: Option<Env>,
}

pub(crate) struct WorkItem {
  pub cfg: Arc<CharlieBotConfig>,
  pub session_meta: Arc<Mutex<SessionMetadata>>,
  pub user_content: String,
  pub callbacks: SessionCallbacks,
  pub auto_trigger: bool,
  pub backend_option: Option<BackendOption>,
  pub extra_claude_flags: Option<Vec<String>>,
  pub task_run: TaskRunBinding,
  pub user_event_ids: Vec<String>,
  pub uploaded_files: Option<Vec<Value>>,
  pub resume_record: Option<MasterResumeRecord>,
  pub resume_is_alive: Option<ResumeLiveness>,
  pub task_instructions: Option<String>,
  pub on_task_spawn: TaskSpawnCallback,
  pub on_task_finish: TaskFinishCallback,
  pub extra_env: Option<Env>,
  pub cancel: watch::Receiver<bool>,
  pub completion: Option<oneshot::Sender<anyhow::Result<Option<String>>>>,
}

impl WorkItem {
  pub(crate) fn session_metadata(&self) -> SessionMetadata {
    self.session_meta.lock().expect("work item metadata mutex is not poisoned").clone()
  }
}
