//! Fixtures for the manager turn and idle compaction tests: a cc-claude session in a real store.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::backends::claude_code::login_dirs;
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::backend::Env;
use charliebot_registry::context::{FeatureContext, StartupState};
use charliebot_registry::{Registry, store_hooks};
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_store::run_records::{RunKind, RunRecord};
use charliebot_store::{CreateTask, Store, TaskCaller};
use charliebot_usage::SessionUsage;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::sync::watch;

use crate::master_queue::MasterQueue;
use crate::master_state::{TaskRunBinding, WorkItem};

/// The config.yaml of one cc-claude option `fable` on model `claude-fable-5-1`. With `account_dir`, the option draws
/// on the pool `alpha`, whose one login `acct-a` keeps its transcripts in `account_dir`; without it, the option
/// runs on the process login directory.
pub fn claude_config(account_dir: Option<&Path>) -> String {
  let option = "backends:\n  options:\n    - id: fable\n      label: Fable\n      type: cc-claude\n      model: claude-fable-5-1\n";
  match account_dir {
    Some(dir) => format!(
      "{option}      account_pool: alpha\naccounts:\n  claude:\n    - label: acct-a\n      config_dir: '{}'\n  claude_pools:\n    alpha: [acct-a]\n",
      dir.display()
    ),
    None => option.to_string(),
  }
}

/// The feature context over `home`, whose config.yaml is `config_body`, with the cc-claude backend registered.
pub fn claude_features(home: &Path, config_body: &str) -> Arc<FeatureContext> {
  std::fs::create_dir_all(home).unwrap();
  std::fs::write(home.join("config.yaml"), config_body).unwrap();
  let mut registry = Registry::new();
  registry.register_package("src.backends.claude_code", charliebot_agents::backends::claude_code::register).unwrap();
  let registry = Arc::new(registry);
  let config = registry.load_config(home).unwrap();
  let store_config = config.clone();
  let reload_config = config.clone();
  let config = Arc::new(HotReload::new(
    config.config_file(),
    "claude_fixture_config_reload_failed",
    Box::new(move || Ok(reload_config.clone())),
  ));
  let store = Arc::new(
    Store::new(store_config, registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry))).unwrap(),
  );
  let credential_home = home.to_path_buf();
  let credentials = Arc::new(HotReload::new(
    home.join("credentials.yaml"),
    "claude_fixture_credentials_reload_failed",
    Box::new(move || load_credentials(&credential_home)),
  ));
  let startup = Arc::new(StartupState::default());
  startup.insert(SessionUsage::new(Arc::clone(&config), Arc::clone(&store), Arc::clone(&registry))).unwrap();
  startup.insert(AgentBaseEnvironment { values: Env::from([("PATH".to_string(), "/usr/bin".to_string())]) }).unwrap();
  Arc::new(FeatureContext { config, credentials, store, registry, startup })
}

/// A manager session on `backend` in the store, holding `account` and the Claude session `cc_session_id`.
/// The session id derives from `request_id`, so each test passes its own name: the idle state is process-wide.
pub async fn manager_session(
  features: &FeatureContext,
  request_id: &str,
  backend: &str,
  account: Option<&str>,
  cc_session_id: &str,
) -> SessionMetadata {
  let created = features
    .store
    .create_task(
      CreateTask {
        request_id: request_id.to_string(),
        task_parent_id: None,
        profile: TaskProfile::Manager,
        task: None,
        name: Some("Synthetic idle session".to_string()),
        backend: Some(backend.to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::System,
      Map::new(),
    )
    .await
    .unwrap();
  if let Some(label) = account {
    features.store.persist_account_label(&created.id, label).await.unwrap();
  }
  features.store.persist_cc_session_id(&created.id, cc_session_id, None).await.unwrap();
  features.store.get_session(&created.id).await.unwrap().unwrap()
}

/// Chat events that give the session a context reading of `context_tokens` and a last request at `request`.
pub async fn append_reading_and_request(
  features: &FeatureContext,
  session_id: &str,
  context_tokens: i64,
  request: &str,
) {
  let events = [
    json!({"type": "system", "subtype": "context_reading", "context_reading": {"context_tokens": context_tokens}}),
    json!({"type": "result", "timestamp": request}),
  ];
  for event in events {
    features.store.append_chat_event(session_id, event).await.unwrap();
  }
}

/// A login directory with credentials, so the pool can select it.
pub fn write_login(config_dir: &Path) {
  std::fs::create_dir_all(config_dir).unwrap();
  std::fs::write(config_dir.join(login_dirs::CREDENTIALS_FILE), r#"{"claudeAiOauth":{"accessToken":"synthetic"}}"#)
    .unwrap();
}

/// The transcript of `cc_session_id` in the login directory `config_dir`, where the session's resume looks for it.
pub fn write_transcript(config_dir: &Path, cc_session_id: &str) {
  let path = config_dir.join("projects/synthetic-project").join(format!("{cc_session_id}.jsonl"));
  std::fs::create_dir_all(path.parent().unwrap()).unwrap();
  std::fs::write(path, "{\"type\":\"user\"}\n").unwrap();
}

/// A run record of `kind` on `session_id`: the shape the delivery and idle tests register.
pub fn run_record(session_id: &str, run_id: &str, kind: RunKind) -> RunRecord {
  RunRecord {
    id: run_id.to_string(),
    session_id: session_id.to_string(),
    kind,
    input_event_ids: Vec::new(),
    task_spec_hash: None,
    task_spec_ref: None,
    prompt_snapshot_ref: None,
    retry_of_run_id: None,
    review_of_run_id: None,
    backend: None,
    model: None,
    native_session_id: None,
    pid: None,
    pid_start: None,
    started_at: None,
    ended_at: None,
    exit_code: None,
    repo_path: None,
    base_branch: None,
    branch_name: None,
    worktree_path: None,
    sequence_ref: None,
    raw_log_ref: None,
    events_ref: None,
    result_ref: None,
  }
}

/// A queued work item of `session_meta`: the turn a session holds while it waits for its consumer.
pub fn work_item(queue: &Arc<MasterQueue>, features: &Arc<FeatureContext>, session_meta: SessionMetadata) -> WorkItem {
  let session_id = session_meta.id.clone();
  let callbacks = queue.session_callbacks(Arc::clone(features), Arc::new(|_| Ok((false, String::new()))));
  let (_cancel_sender, cancel) = watch::channel(false);
  WorkItem {
    cfg: features.config.get().unwrap(),
    session_meta: Arc::new(Mutex::new(session_meta)),
    user_content: "synthetic turn".to_string(),
    callbacks,
    auto_trigger: false,
    backend_option: None,
    extra_claude_flags: None,
    task_run: TaskRunBinding {
      session_id,
      run_id: "synthetic-run".to_string(),
      transport_dir: PathBuf::from("synthetic-transport"),
      fresh_native_context: false,
    },
    user_event_ids: Vec::new(),
    uploaded_files: None,
    resume_record: None,
    resume_is_alive: None,
    task_instructions: Some("instructions".to_string()),
    on_task_spawn: Arc::new(|_: u32, _: Option<String>| -> BoxFuture<'static, anyhow::Result<()>> {
      Box::pin(async { Ok(()) })
    }),
    on_task_finish: Arc::new(
      |_: Option<String>, _: i32, _: Map<String, Value>, _: Value| -> BoxFuture<'static, anyhow::Result<bool>> {
        Box::pin(async { Ok(false) })
      },
    ),
    extra_env: None,
    cancel,
    completion: None,
  }
}
