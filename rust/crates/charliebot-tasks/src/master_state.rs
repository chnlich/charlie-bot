//! Per-session manager work items and exported run-state reads.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::backend::Env;
use charliebot_registry::context::FeatureContext;
use charliebot_store::metadata::{SessionMetadata, UtcDateTime};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use tokio::sync::{oneshot, watch};

use crate::master_queue::SessionCallbacks;

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

/// Return the current turn's input event ids from the startup-owned `MasterQueue`.
pub fn running_user_event_ids(features: &FeatureContext, session_id: &str) -> anyhow::Result<Vec<String>> {
  let queue = features.startup.require::<crate::master_queue::MasterQueue>().map_err(|error| anyhow!(error))?;
  Ok(queue.running_user_event_ids(session_id))
}
