//! End-to-end ACP mid-turn tests against a scripted POSIX-shell `agy` binary: server restart mid-turn, worker
//! queuing, turn-close timing, error deferral, handshake failure, and usage accounting.

mod process_guard;

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use charliebot_agents::agent_environment::agent_environment;
use charliebot_agents::backend_availability::BackendAvailability;
use charliebot_agents::backend_pool::BackendPool;
use charliebot_agents::backend_types;
use charliebot_agents::cli_base::CliHost;
use charliebot_agents::model_pool::RotationPositions;
use charliebot_agents::runner::{AgentRunner, RunRequest};
use charliebot_agents::transport::cgroup::SessionCgroupConfig;
use charliebot_agents::transport::identity;
use charliebot_core::config::{
  BackendOption, BackendsConfig, CallClass, CallTarget, CallsConfig, CharlieBotConfig, PathsConfig, Sections,
  ServerConfig, TelegramConfig, UiConfig,
};
use charliebot_core::credentials::load_credentials;
use charliebot_core::event_types;
use charliebot_core::hot_reload::HotReload;
use charliebot_core::repo_root::RepoRoot;
use charliebot_registry::Registry;
use charliebot_registry::backend::{Env, LaunchArgs};
use charliebot_registry::context::{FeatureContext, StartupState};
use charliebot_registry::sequence::SequenceCompletion;
use charliebot_store::compact::CompactValue;
use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskSpec, TaskType, UtcDateTime};
use charliebot_store::run_records::{MIDTURN_INPUT_FIFO_NAME, MIDTURN_INPUT_LOG_NAME};
use charliebot_store::{
  AccountLabelKeeper, AnchorFieldReconciler, CreateTask, EmptyRuntimeEventHooks, SequenceArchiveHook,
  SequenceBindingMover, SequenceListingFields, Store, StoreHookFuture, StoreHooks, StoreResult, TaskCaller,
};
use charliebot_tasks::completion::{PendingTriggerCancel, TaskCompletion};
use charliebot_tasks::dispatch::{Dispatcher, RunFinishedCompletion};
use charliebot_tasks::execution::TaskExecutionAdapter;
use charliebot_tasks::master_queue::MasterQueue;
use charliebot_tasks::recovery::{self, PassCounters};
use charliebot_tasks::test_support::{NoPendingTriggers, served_checkout};
use process_guard::ProcessGroupGuard;
use serde_json::{Map, Value, json};
use serde_yaml::Mapping;
use tempfile::TempDir;

const TEST_DEADLINE: Duration = Duration::from_secs(10);
const AGY_OPTION_ID: &str = "agy-test";

thread_local! {
  static TEST_HOST: RefCell<Option<CliHost>> = const { RefCell::new(None) };
}

fn register_test_antigravity(reg: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
  let host = TEST_HOST.with(|slot| slot.borrow().clone().expect("TEST_HOST set before register"));
  charliebot_antigravity::register_with_host(reg, host)
}

#[derive(Default)]
struct TestHooks;

impl AccountLabelKeeper for TestHooks {
  fn record_account_label(&self, _meta: &mut SessionMetadata, _label: Option<&str>) -> StoreResult<bool> {
    Ok(false)
  }

  fn account_label(&self, _meta: &SessionMetadata) -> Option<String> {
    None
  }
}

impl AnchorFieldReconciler for TestHooks {
  fn reconcile(&self, disk: &SessionMetadata, attempted: &mut SessionMetadata) -> StoreResult<()> {
    attempted.cc_session_id = disk.cc_session_id.clone();
    attempted.cc_session_started_at = disk.cc_session_started_at;
    attempted.native_backend = disk.native_backend.clone();
    Ok(())
  }
}

impl SequenceArchiveHook for TestHooks {
  fn on_archive<'a>(&'a self, _session_id: &'a str) -> StoreHookFuture<'a, ()> {
    Box::pin(async { Ok(()) })
  }
}

impl SequenceBindingMover for TestHooks {
  fn move_bindings<'a>(&'a self, _source_session_id: &'a str, _new_session_id: &'a str) -> StoreHookFuture<'a, bool> {
    Box::pin(async { Ok(false) })
  }
}

impl SequenceListingFields for TestHooks {
  fn owns_session(&self, _meta: &SessionMetadata) -> bool {
    false
  }

  fn listing_fields(
    &self,
    _session_ids: &[String],
    _now: UtcDateTime,
  ) -> StoreResult<HashMap<String, Map<String, Value>>> {
    Ok(HashMap::new())
  }

  fn sequence_subtree_roots(&self, _metas: &[Arc<SessionMetadata>]) -> StoreResult<HashMap<String, String>> {
    Ok(HashMap::new())
  }

  fn view_subtree_roots(
    &self,
    _metas: &[Arc<SessionMetadata>],
  ) -> StoreResult<HashMap<String, HashMap<String, String>>> {
    Ok(HashMap::new())
  }
}

fn agy_option(model: &str) -> BackendOption {
  BackendOption {
    id: AGY_OPTION_ID.to_string(),
    label: "Antigravity Test".to_string(),
    model: model.to_string(),
    prompt_overlay: None,
    billing: None,
    backend_type: "antigravity".to_string(),
    type_fields: Mapping::new(),
    visible: true,
  }
}

fn test_config(home: &Path, options: Vec<BackendOption>, calls: CallsConfig) -> CharlieBotConfig {
  CharlieBotConfig {
    charliebot_home: home.to_path_buf(),
    headless_chrome_bin: "/bin/false".to_string(),
    server: ServerConfig {
      host: "127.0.0.1".to_string(),
      port: 18651,
      subprocess_buffer_limit_mb: 1,
      session_memory_max_mb: 0,
      session_swap_max_mb: 0,
      public_url: None,
      session_tasks_max: 0,
      min_free_disk_gib: 0,
      enable_session_cpu_weight: true,
      enable_session_io_weight: true,
      enable_local_work_guard: false,
      enable_offline_shell: false,
    },
    paths: PathsConfig {
      workspace_dirs: Vec::new(),
      worktree_dir: home.join("worktrees").display().to_string(),
      scratch_dir: home.join("scripts").display().to_string(),
    },
    backends: BackendsConfig { routing: None, options, pools: Vec::new(), calls },
    ui: UiConfig { home_services: Vec::new() },
    telegram: TelegramConfig { chat_id: None },
    remote_run: charliebot_core::config::RemoteRunConfig::default(),
    github: Default::default(),
    sections: Sections::default(),
  }
}

struct AcpStack {
  _home: Arc<TempDir>,
  _bin_dir: PathBuf,
  _cfg: Arc<CharlieBotConfig>,
  features: Arc<FeatureContext>,
  _adapter: TaskExecutionAdapter,
  dispatch: Arc<Dispatcher>,
  queue: Arc<MasterQueue>,
}

impl AcpStack {
  async fn start(home: Arc<TempDir>, bin_dir: PathBuf, script: &str) -> anyhow::Result<Self> {
    fs::create_dir_all(&bin_dir)?;
    let agy_path = bin_dir.join("agy");
    fs::write(&agy_path, script)?;
    fs::set_permissions(&agy_path, fs::Permissions::from_mode(0o755))?;
    Self::assemble(home, bin_dir).await
  }

  async fn assemble(home: Arc<TempDir>, bin_dir: PathBuf) -> anyhow::Result<Self> {
    let calls = CallsConfig::from_iter([
      (CallClass::NewSession, CallTarget::Backend(AGY_OPTION_ID.to_string())),
      (CallClass::Worker, CallTarget::Backend(AGY_OPTION_ID.to_string())),
    ]);
    let cfg = Arc::new(test_config(home.path(), vec![agy_option("gemini-3.8-flash-low")], calls));
    let mut registry = Registry::new();
    let host = CliHost { path: format!("{}:/usr/bin:/bin", bin_dir.display()), home: home.path().to_path_buf() };
    TEST_HOST.with(|slot| *slot.borrow_mut() = Some(host));
    registry.register_package(charliebot_antigravity::PACKAGE, register_test_antigravity)?;
    registry.register_package("features.web_reply", charliebot_web_reply::register)?;
    registry.register_package("runtime.tasks", charliebot_tasks::register)?;
    let registry = Arc::new(registry);
    let hooks = Arc::new(TestHooks);
    let store = Arc::new(Store::new(
      (*cfg).clone(),
      registry.metadata_slots(),
      StoreHooks {
        account_labels: Some(hooks.clone()),
        anchor_reconciler: Some(hooks.clone()),
        sequence_listing: Some(hooks.clone()),
        sequence_archive: Some(hooks.clone()),
        sequence_binding_mover: Some(hooks),
        runtime_events: Some(Arc::new(EmptyRuntimeEventHooks)),
      },
    )?);
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
      .ancestors()
      .nth(4)
      .context("tasks crate manifest has no repository root ancestor")?;
    let served_repo = served_checkout(checkout, &cfg.charliebot_home)?;
    let server_bin_dir = home.path().join("release");
    fs::create_dir_all(&server_bin_dir)?;
    let cli = server_bin_dir.join("charliebot");
    fs::write(&cli, "#!/bin/sh\n")?;
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o755))?;
    let base_env = agent_environment(
      &Env::from([("PATH".to_string(), format!("{}:/usr/bin:/bin", bin_dir.display()))]),
      &server_bin_dir,
      &charliebot_agents::gh_wrapper::dir_under(&served_repo),
      &charliebot_agents::ssh_guard::dir_under(&served_repo),
    )?;
    let startup = Arc::new(StartupState::default());
    startup.insert(base_env)?;
    startup.insert(BackendAvailability::load(&cfg.charliebot_home)?)?;
    startup.insert(RotationPositions::default())?;
    startup.insert(BackendPool::new(
      startup.require::<BackendAvailability>()?,
      Arc::clone(&registry),
      startup.require::<RotationPositions>()?,
    ))?;
    startup.insert(RepoRoot::new(Some(served_repo.into_os_string()))?)?;
    let cfg_copy = (*cfg).clone();
    let config_reload = HotReload::new(cfg.config_file(), "acp_test_cfg", Box::new(move || Ok(cfg_copy.clone())));
    fs::write(cfg.credentials_file(), "charliebot:\n  access_key: test-access-key\n")?;
    let cred_home = cfg.charliebot_home.clone();
    let credentials_reload =
      HotReload::new(cfg.credentials_file(), "acp_test_creds", Box::new(move || load_credentials(&cred_home)));
    let features = Arc::new(FeatureContext {
      config: Arc::new(config_reload),
      credentials: Arc::new(credentials_reload),
      store: Arc::clone(&store),
      registry: Arc::clone(&registry),
      startup,
    });
    features.registry.bind_feature_context(&features)?;
    let pool = charliebot_agents::backend_pool::backend_pool(&features)?;
    features.startup.insert(MasterQueue::new(pool))?;
    let queue = features.startup.require::<MasterQueue>()?;
    features.startup.insert(TaskCompletion::new(Arc::clone(&features)))?;
    let completion = features.startup.require::<TaskCompletion>()?;
    let pending_triggers: Arc<dyn PendingTriggerCancel> = Arc::new(NoPendingTriggers);
    features.startup.insert(pending_triggers)?;
    let sequence_completion: Arc<dyn SequenceCompletion> = completion.clone();
    let run_completion: Arc<dyn RunFinishedCompletion> = completion.clone();
    features.startup.insert(Dispatcher::new(
      Arc::clone(&features.store),
      Arc::clone(&features),
      sequence_completion,
      run_completion,
    ))?;
    let dispatch = features.startup.require::<Dispatcher>()?;
    let adapter = TaskExecutionAdapter::new(Arc::clone(&features), Arc::clone(&queue), Arc::clone(&dispatch));
    dispatch.set_executor(Some(Arc::new(adapter.clone())));
    features.startup.insert(adapter.clone())?;
    Ok(Self { _home: home, _bin_dir: bin_dir, _cfg: cfg, features, _adapter: adapter, dispatch, queue })
  }

  async fn create_session(&self, id: &str, profile: TaskProfile, task: Option<TaskSpec>) -> anyhow::Result<()> {
    self
      .features
      .store
      .create_task(
        CreateTask {
          request_id: uuid::Uuid::new_v4().to_string(),
          task_parent_id: None,
          profile,
          task,
          name: Some(id.to_string()),
          backend: Some(AGY_OPTION_ID.to_string()),
          group: None,
          session_id: Some(id.to_string()),
        },
        TaskCaller::System,
        Map::new(),
      )
      .await?;
    Ok(())
  }

  async fn send_user(&self, session_id: &str, content: &str) -> anyhow::Result<Value> {
    let event = self
      .features
      .store
      .append_chat_event(
        session_id,
        json!({
          "id": uuid::Uuid::new_v4().to_string(),
          "type": event_types::USER,
          "content": content,
        }),
      )
      .await?;
    let decision = self.dispatch.dispatch_pending(session_id).await?;
    Ok(json!({"event_id": event.get("id").and_then(CompactValue::as_str), "decision": decision}))
  }
}

async fn wait_until<F>(label: &str, mut predicate: F) -> anyhow::Result<()>
where
  F: FnMut() -> anyhow::Result<bool>,
{
  let deadline = Instant::now() + TEST_DEADLINE;
  while Instant::now() < deadline {
    if predicate()? {
      return Ok(());
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
  anyhow::bail!("timed out waiting for {label}")
}

fn chat_events(store: &Store, session_id: &str) -> anyhow::Result<Vec<Value>> {
  Ok(store.load_chat_events_sync(session_id)?.into_iter().map(|event| event.to_value()).collect())
}

async fn release_gate(store: &Store, session_id: &str, run_id: &str, state_dir: &Path) -> anyhow::Result<()> {
  use tokio::io::AsyncWriteExt;
  fs::write(state_dir.join("finish.gate"), "")?;
  let fifo = store.run_dir(session_id, run_id).join(MIDTURN_INPUT_FIFO_NAME);
  if let Ok(mut writer) = charliebot_agents::transport::spawn::open_fifo_writer(&fifo) {
    let _ = writer.write_all(b"{\"method\":\"test/finish\"}\n").await;
  }
  Ok(())
}

fn dump_state(store: &Store, session_id: &str, run_id: &str, state_dir: &Path) -> String {
  let run_dir = store.run_dir(session_id, run_id);
  let events = chat_events(store, session_id).unwrap_or_default();
  let run = store.read_run_sync(session_id, run_id).ok().flatten();
  let input_log = fs::read_to_string(run_dir.join(MIDTURN_INPUT_LOG_NAME)).unwrap_or_default();
  let reqs = fs::read_to_string(state_dir.join("requests.log")).unwrap_or_default();
  let raw = fs::read_to_string(run_dir.join(charliebot_store::run_records::RAW_LOG_NAME)).unwrap_or_default();
  let err = fs::read_to_string(run_dir.join(charliebot_store::run_records::STDERR_LOG_NAME)).unwrap_or_default();
  format!("run={run:?}\nreqs={reqs:?}\ninput_log={input_log:?}\nraw={raw:?}\nstderr={err:?}\nevents={events:?}")
}

/// Scripted `agy` ACP server supporting `initialize`, `session/new`, `session/set_model`, and `session/prompt`.
///
/// Logs each received `session/prompt` request id to `$LOG`, records marker files per prompt (`prompt.<id>`), answers
/// `id=5` with an error when `$REJECT` exists, and holds final answers until `$GATE` exists. Once all prompts are
/// answered, it blocks on `sleep` until stopped by CharlieBot (simulating `O_RDWR` FIFO survival).
fn scripted_acp_script(state_dir: &Path) -> String {
  format!(
    r#"#!/bin/sh
set -u
STATE='{state}'
GATE="$STATE/finish.gate"
LOG="$STATE/requests.log"
REJECT="$STATE/reject_id5"
pending=""
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":1}}}}\n' "$id"
      ;;
    session/new|session/load)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"acp-sess-1"}}}}\n' "$id"
      ;;
    session/set_model)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id"
      ;;
    session/prompt)
      printf '%s\n' "$id" >> "$LOG"
      : > "$STATE/prompt.$id"
      if [ "$id" = "5" ] && [ -e "$REJECT" ]; then
        printf '{{"jsonrpc":"2.0","id":5,"error":{{"code":-32000,"message":"midturn rejected"}}}}\n'
        break
      fi
      if [ -n "$pending" ]; then
        printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"end_turn","usage":{{"inputTokens":10,"outputTokens":5,"cachedReadTokens":0,"cachedWriteTokens":0,"thoughtTokens":2,"totalTokens":15}}}}}}\n' "$pending"
      fi
      pending="$id"
      if [ -e "$GATE" ]; then
        printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"acp-sess-1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"done"}}}}}}}}\n'
        printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"end_turn","usage":{{"inputTokens":20,"outputTokens":12,"cachedReadTokens":4,"cachedWriteTokens":0,"thoughtTokens":7,"totalTokens":36}}}}}}\n' "$pending"
        pending=""
      fi
      ;;
    test/finish)
      if [ -e "$GATE" ] && [ -n "$pending" ]; then
        printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"acp-sess-1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"done"}}}}}}}}\n'
        printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"end_turn","usage":{{"inputTokens":20,"outputTokens":12,"cachedReadTokens":4,"cachedWriteTokens":0,"thoughtTokens":7,"totalTokens":36}}}}}}\n' "$pending"
        pending=""
      fi
      ;;
  esac
done
while [ ! -e "$GATE" ]; do
  sleep 0.02
done
if [ -n "$pending" ]; then
  printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"acp-sess-1","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"done"}}}}}}}}\n'
  printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"end_turn","usage":{{"inputTokens":20,"outputTokens":12,"cachedReadTokens":4,"cachedWriteTokens":0,"thoughtTokens":7,"totalTokens":36}}}}}}\n' "$pending"
fi
while :; do
  sleep 1
done
"#,
    state = state_dir.display()
  )
}

#[tokio::test]
#[ignore = "integration"]
async fn acp_manager_restart_midturn_delivers_before_and_after_restart_once() -> anyhow::Result<()> {
  let guard = ProcessGroupGuard::new();
  let home = Arc::new(tempfile::tempdir()?);
  let state_dir = home.path().join("acp-state");
  fs::create_dir_all(&state_dir)?;
  let bin_dir = home.path().join("bin");
  let stack = AcpStack::start(Arc::clone(&home), bin_dir.clone(), &scripted_acp_script(&state_dir)).await?;
  let session_id = "acp-restart-mgr";
  stack.create_session(session_id, TaskProfile::Manager, None).await?;

  let first = stack.send_user(session_id, "Start turn").await?;
  assert_eq!(first["decision"]["launch"], json!(true));
  let run_id = first["decision"]["run_id"].as_str().unwrap().to_string();

  wait_until("initial prompt 4 reached agy", || Ok(state_dir.join("prompt.4").exists()))
    .await
    .with_context(|| dump_state(&stack.features.store, session_id, &run_id, &state_dir))?;
  let run = stack.features.store.read_run_sync(session_id, &run_id)?.context("run recorded")?;
  let pid = run.pid.context("run recorded pid")?;
  let pid_start = run.pid_start.clone().context("run recorded pid_start")?;
  guard.track(pid);

  // Mid-turn message before restart (`id=5`).
  let m2 = stack.send_user(session_id, "Midturn before restart").await?;
  assert_eq!(m2["decision"]["launch"], json!(false));
  let m2_id = m2["event_id"].as_str().unwrap().to_string();
  wait_until("prompt 5 reached agy", || Ok(state_dir.join("prompt.5").exists()))
    .await
    .with_context(|| format!("m2={m2}\n{}", dump_state(&stack.features.store, session_id, &run_id, &state_dir)))?;
  wait_until("m2 input_delivered persisted", || {
    Ok(
      chat_events(&stack.features.store, session_id)?
        .iter()
        .any(|event| event["type"] == event_types::INPUT_DELIVERED && event[event_types::INPUT_EVENT_ID] == m2_id),
    )
  })
  .await?;

  // Simulate server shutdown (`let_go`) keeping the agy process alive on its O_RDWR FIFO.
  recovery::let_go_step(Arc::clone(&stack.features)).await?;
  stack.queue.shutdown().await?;
  drop(stack);
  let stat = identity::read_pid_stat(pid).context("agy survived shutdown")?;
  assert_eq!(stat.pid_start, pid_start);
  assert_ne!(stat.state, "Z");

  // Boot server 2 on the same home and reattach.
  let stack2 = AcpStack::assemble(Arc::clone(&home), bin_dir).await?;
  let mut counters = PassCounters::default();
  recovery::reconcile_node(&stack2.features, &stack2._adapter, session_id, &mut counters, None).await?;
  assert_eq!(counters.resumed, 1, "reattach resumed the live ACP turn: {counters:?}");

  // Send a post-restart mid-turn message (`id=6`); retry dispatch until `resume_cc` binds `DeliveryState`.
  let m3 = stack2.send_user(session_id, "Midturn after restart").await?;
  assert_eq!(m3["decision"]["launch"], json!(false));
  let m3_id = m3["event_id"].as_str().unwrap().to_string();
  let deadline = Instant::now() + TEST_DEADLINE;
  while !state_dir.join("prompt.6").exists() && Instant::now() < deadline {
    let _ = stack2.dispatch.dispatch_pending(session_id).await?;
    tokio::time::sleep(Duration::from_millis(25)).await;
  }
  assert!(state_dir.join("prompt.6").exists(), "post-restart prompt 6 reached agy over reopened FIFO");
  wait_until("m3 input_delivered persisted", || {
    Ok(
      chat_events(&stack2.features.store, session_id)?
        .iter()
        .any(|event| event["type"] == event_types::INPUT_DELIVERED && event[event_types::INPUT_EVENT_ID] == m3_id),
    )
  })
  .await?;

  // Release finish gate -> agy emits final answer and response 6, then server stops agy cleanly.
  release_gate(&stack2.features.store, session_id, &run_id, &state_dir).await?;
  wait_until("resumed run finished successfully", || {
    let events = chat_events(&stack2.features.store, session_id)?;
    Ok(events.iter().any(|event| event["type"] == event_types::RUN_FINISHED && event["run_id"] == run_id))
  })
  .await?;
  wait_until("agy process exited within grace period", || {
    Ok(identity::read_pid_stat(pid).is_none_or(|s| s.pid_start != pid_start || s.state == "Z"))
  })
  .await?;

  let runs = stack2.features.store.list_run_records_sync(session_id)?;
  assert_eq!(runs.len(), 1, "no second turn launched: {runs:?}");
  assert_eq!(runs[0].exit_code, Some(0));

  let events = chat_events(&stack2.features.store, session_id)?;
  let m2_delivered = events
    .iter()
    .filter(|e| e["type"] == event_types::INPUT_DELIVERED && e[event_types::INPUT_EVENT_ID] == m2_id)
    .count();
  let m3_delivered = events
    .iter()
    .filter(|e| e["type"] == event_types::INPUT_DELIVERED && e[event_types::INPUT_EVENT_ID] == m3_id)
    .count();
  assert_eq!(m2_delivered, 1, "m2 delivered once");
  assert_eq!(m3_delivered, 1, "m3 delivered once");
  let result = events.iter().find(|e| e["type"] == event_types::RESULT).context("result persisted")?;
  assert_eq!(result["usage"][event_types::USAGE_INPUT_TOKENS], 40);
  assert_eq!(result["usage"][event_types::USAGE_OUTPUT_TOKENS], 22, "thoughtTokens are not double-counted");
  assert_eq!(fs::read_to_string(state_dir.join("requests.log"))?.lines().collect::<Vec<_>>(), ["4", "5", "6"]);
  stack2.queue.shutdown().await?;
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn acp_worker_turn_defers_new_message_until_next_turn() -> anyhow::Result<()> {
  let guard = ProcessGroupGuard::new();
  let home = Arc::new(tempfile::tempdir()?);
  let state_dir = home.path().join("acp-worker-state");
  fs::create_dir_all(&state_dir)?;
  let bin_dir = home.path().join("bin");
  let stack = AcpStack::start(Arc::clone(&home), bin_dir, &scripted_acp_script(&state_dir)).await?;
  let session_id = "acp-worker-session";
  let spec =
    TaskSpec { goal: "Run worker over ACP".to_string(), task_type: Some(TaskType::ScriptRun), ..TaskSpec::default() };
  stack.create_session(session_id, TaskProfile::Worker, Some(spec)).await?;

  let first = stack.send_user(session_id, "First worker input").await?;
  assert_eq!(first["decision"]["launch"], json!(true));
  let run_1 = first["decision"]["run_id"].as_str().unwrap().to_string();

  wait_until("worker prompt 4 reached agy", || {
    if state_dir.join("prompt.4").exists() {
      return Ok(true);
    }
    let events = chat_events(&stack.features.store, session_id)?;
    if events.iter().any(|e| e["type"] == event_types::RUN_FINISHED) {
      let run_dir = stack.features.store.run_dir(session_id, &run_1);
      let run = stack.features.store.read_run_sync(session_id, &run_1)?;
      let run_events =
        fs::read_to_string(run_dir.join(charliebot_store::run_records::RUN_EVENTS_NAME)).unwrap_or_default();
      let raw = fs::read_to_string(run_dir.join(charliebot_store::run_records::RAW_LOG_NAME)).unwrap_or_default();
      let err = fs::read_to_string(run_dir.join(charliebot_store::run_records::STDERR_LOG_NAME)).unwrap_or_default();
      anyhow::bail!(
        "worker finished early! run={run:?} run_events={run_events:?} events={events:?} raw={raw:?} stderr={err:?}"
      );
    }
    Ok(false)
  })
  .await?;
  let run = stack.features.store.read_run_sync(session_id, &run_1)?.context("worker run recorded")?;
  let pid = run.pid.context("worker run pid")?;
  let pid_start = run.pid_start.clone().context("worker run pid_start")?;
  guard.track(pid);

  let second = stack.send_user(session_id, "Deferred message").await?;
  assert_eq!(second["decision"]["launch"], json!(false));
  let run_dir = stack.features.store.run_dir(session_id, &run_1);
  let logged = fs::read_to_string(run_dir.join(MIDTURN_INPUT_LOG_NAME))?;
  assert_eq!(logged.lines().count(), 1, "worker run only logged its initial prompt");
  assert!(!state_dir.join("prompt.5").exists(), "worker did not receive mid-turn message");

  release_gate(&stack.features.store, session_id, &run_1, &state_dir).await?;
  wait_until("first worker run finished", || Ok(stack.features.store.run_has_terminal_outcome(session_id, &run_1)?))
    .await?;
  wait_until("second worker run finished for deferred message", || {
    let runs = stack.features.store.list_run_records_sync(session_id)?;
    Ok(runs.len() == 2 && runs.iter().all(|r| r.exit_code == Some(0)))
  })
  .await?;
  wait_until("first worker agy exited", || {
    Ok(identity::read_pid_stat(pid).is_none_or(|s| s.pid_start != pid_start || s.state == "Z"))
  })
  .await?;
  stack.queue.shutdown().await?;
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn acp_midturn_error_response_shows_error_and_defers_to_next_turn() -> anyhow::Result<()> {
  let guard = ProcessGroupGuard::new();
  let home = Arc::new(tempfile::tempdir()?);
  let state_dir = home.path().join("acp-error-state");
  fs::create_dir_all(&state_dir)?;
  fs::write(state_dir.join("reject_id5"), "")?;
  let bin_dir = home.path().join("bin");
  let stack = AcpStack::start(Arc::clone(&home), bin_dir, &scripted_acp_script(&state_dir)).await?;
  let session_id = "acp-error-mgr";
  stack.create_session(session_id, TaskProfile::Manager, None).await?;

  let first = stack.send_user(session_id, "Start turn").await?;
  let run_1 = first["decision"]["run_id"].as_str().unwrap().to_string();
  wait_until("prompt 4 reached agy", || Ok(state_dir.join("prompt.4").exists())).await?;
  let run = stack.features.store.read_run_sync(session_id, &run_1)?.context("run recorded")?;
  guard.track(run.pid.context("run pid")?);

  let m2 = stack.send_user(session_id, "Will be rejected mid-turn").await?;
  let m2_id = m2["event_id"].as_str().unwrap().to_string();
  wait_until("error event shown in session", || {
    Ok(chat_events(&stack.features.store, session_id)?.iter().any(|event| {
      event["type"] == event_types::ERROR
        && event["message"].as_str().is_some_and(|message| message.contains("midturn rejected"))
    }))
  })
  .await?;

  // Allow turn 1 to complete; remove rejection before turn 2 runs m2.
  fs::remove_file(state_dir.join("reject_id5"))?;
  fs::write(state_dir.join("finish.gate"), "")?;

  wait_until("turn 2 finishes for deferred message", || {
    let runs = stack.features.store.list_run_records_sync(session_id)?;
    Ok(runs.len() == 2 && runs.iter().all(|r| r.exit_code == Some(0)))
  })
  .await?;
  let events = chat_events(&stack.features.store, session_id)?;
  assert!(
    !events.iter().any(|e| e["type"] == event_types::INPUT_DELIVERED && e[event_types::INPUT_EVENT_ID] == m2_id),
    "errored mid-turn request was not marked delivered in turn 1"
  );
  let runs = stack.features.store.list_run_records_sync(session_id)?;
  assert!(runs[1].input_event_ids.contains(&m2_id), "deferred message claimed by turn 2: {runs:?}");
  stack.queue.shutdown().await?;
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn acp_message_during_turn_close_is_never_lost_or_duplicated() -> anyhow::Result<()> {
  let guard = ProcessGroupGuard::new();
  let home = Arc::new(tempfile::tempdir()?);
  let state_dir = home.path().join("acp-close-state");
  fs::create_dir_all(&state_dir)?;
  let bin_dir = home.path().join("bin");
  let stack = AcpStack::start(Arc::clone(&home), bin_dir, &scripted_acp_script(&state_dir)).await?;
  let session_id = "acp-close-mgr";
  stack.create_session(session_id, TaskProfile::Manager, None).await?;

  let first = stack.send_user(session_id, "Start turn").await?;
  let run_1 = first["decision"]["run_id"].as_str().unwrap().to_string();
  wait_until("prompt 4 reached agy", || Ok(state_dir.join("prompt.4").exists())).await?;
  let run = stack.features.store.read_run_sync(session_id, &run_1)?.context("run recorded")?;
  guard.track(run.pid.context("run pid")?);

  // Trigger turn close concurrently with sending a message.
  release_gate(&stack.features.store, session_id, &run_1, &state_dir).await?;
  let m2 = stack.send_user(session_id, "Arrived at close").await?;
  let m2_id = m2["event_id"].as_str().unwrap().to_string();

  wait_until("m2 is claimed by either turn 1 or turn 2", || {
    let runs = stack.features.store.list_run_records_sync(session_id)?;
    let claimed_count: usize = runs
      .iter()
      .filter(|r| r.exit_code == Some(0))
      .map(|r| r.input_event_ids.iter().filter(|id| *id == &m2_id).count())
      .sum();
    let pending = stack.dispatch.pending_inputs(session_id)?;
    Ok(claimed_count == 1 && pending.is_empty())
  })
  .await?;
  stack.queue.shutdown().await?;
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn acp_handshake_refusal_fails_promptly_and_stops_process() -> anyhow::Result<()> {
  let root = tempfile::tempdir()?;
  let bin = root.path().join("bin");
  fs::create_dir_all(&bin)?;
  let agy = bin.join("agy");
  fs::write(
    &agy,
    r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1}}\n' "$id" ;;
    session/new) printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s1"}}\n' "$id" ;;
    session/set_model)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"Unknown model ID"}}\n' "$id"
      while :; do sleep 1; done
      ;;
  esac
done
"#,
  )?;
  fs::set_permissions(&agy, fs::Permissions::from_mode(0o755))?;
  let mut registry = Registry::new();
  let host = CliHost { path: format!("{}:/usr/bin:/bin", bin.display()), home: root.path().to_path_buf() };
  TEST_HOST.with(|slot| *slot.borrow_mut() = Some(host));
  registry.register_package(charliebot_antigravity::PACKAGE, register_test_antigravity)?;
  let registry = Arc::new(registry);
  let option = agy_option("no-such-model");
  let cfg = test_config(root.path(), vec![option.clone()], CallsConfig::default());
  let log_dir = root.path().join("run");
  let built = backend_types::build_backend(
    Arc::clone(&registry),
    &option,
    &cfg,
    LaunchArgs { log_dir: Some(log_dir), ..Default::default() },
  )?;
  let runner = AgentRunner::new(&built.backend_type, built.backend, built.launch);
  let mut request = RunRequest::new(
    "hi".to_string(),
    root.path().to_path_buf(),
    charliebot_agents::agent_environment::AgentBaseEnvironment {
      values: charliebot_agents::gh_wrapper::stub_environment(root.path(), &format!("{}:/usr/bin:/bin", bin.display())),
    },
    SessionCgroupConfig::disabled(),
    registry,
  );
  request.cleanup_timeout = Duration::from_secs(2);
  let err = tokio::time::timeout(Duration::from_secs(10), runner.run(request))
    .await
    .context("handshake refusal did not hang")?
    .expect_err("handshake refusal failed the run");
  assert!(format!("{err:#}").contains("refused request 3"), "{err:#}");
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn acp_worker_restart_reattaches_and_finishes_cleanly_without_hang_diagnostics() -> anyhow::Result<()> {
  let guard = ProcessGroupGuard::new();
  let home = Arc::new(tempfile::tempdir()?);
  let state_dir = home.path().join("acp-worker-restart-state");
  fs::create_dir_all(&state_dir)?;
  let bin_dir = home.path().join("bin");
  let stack = AcpStack::start(Arc::clone(&home), bin_dir.clone(), &scripted_acp_script(&state_dir)).await?;
  let session_id = "acp-worker-restart";
  let spec =
    TaskSpec { goal: "Run worker over ACP".to_string(), task_type: Some(TaskType::ScriptRun), ..TaskSpec::default() };
  stack.create_session(session_id, TaskProfile::Worker, Some(spec)).await?;

  let first = stack.send_user(session_id, "Worker task").await?;
  let run_id = first["decision"]["run_id"].as_str().unwrap().to_string();
  wait_until("worker prompt 4 reached agy", || Ok(state_dir.join("prompt.4").exists())).await?;
  let run = stack.features.store.read_run_sync(session_id, &run_id)?.context("worker run recorded")?;
  let pid = run.pid.context("worker pid")?;
  let pid_start = run.pid_start.clone().context("worker pid_start")?;
  guard.track(pid);

  recovery::let_go_step(Arc::clone(&stack.features)).await?;
  stack.queue.shutdown().await?;
  drop(stack);

  let stack2 = AcpStack::assemble(Arc::clone(&home), bin_dir).await?;
  let mut counters = PassCounters::default();
  recovery::reconcile_node(&stack2.features, &stack2._adapter, session_id, &mut counters, None).await?;
  assert_eq!(counters.resumed, 1, "reattach resumed the live ACP worker: {counters:?}");

  release_gate(&stack2.features.store, session_id, &run_id, &state_dir).await?;
  wait_until("resumed worker finished", || Ok(stack2.features.store.run_has_terminal_outcome(session_id, &run_id)?))
    .await?;
  wait_until("resumed worker agy exited", || {
    Ok(identity::read_pid_stat(pid).is_none_or(|s| s.pid_start != pid_start || s.state == "Z"))
  })
  .await?;

  let run = stack2.features.store.read_run_sync(session_id, &run_id)?.context("worker run")?;
  assert_eq!(run.exit_code, Some(0));
  let run_dir = stack2.features.store.run_dir(session_id, &run_id);
  assert!(!run_dir.join("hang_diagnostics.json").exists(), "clean ACP stop writes no hang diagnostics");
  stack2.queue.shutdown().await?;
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn acp_crash_without_turn_ended_stays_a_failure() -> anyhow::Result<()> {
  let guard = ProcessGroupGuard::new();
  let home = Arc::new(tempfile::tempdir()?);
  let state_dir = home.path().join("acp-crash-state");
  fs::create_dir_all(&state_dir)?;
  let bin_dir = home.path().join("bin");
  let stack = AcpStack::start(Arc::clone(&home), bin_dir, &scripted_acp_script(&state_dir)).await?;
  let session_id = "acp-crash-mgr";
  stack.create_session(session_id, TaskProfile::Manager, None).await?;

  let first = stack.send_user(session_id, "Start and crash").await?;
  let run_id = first["decision"]["run_id"].as_str().unwrap().to_string();
  wait_until("prompt 4 reached agy", || Ok(state_dir.join("prompt.4").exists())).await?;
  let run = stack.features.store.read_run_sync(session_id, &run_id)?.context("run recorded")?;
  let pid = run.pid.context("run pid")?;
  guard.track(pid);

  // Kill agy before TurnEnded is signaled.
  charliebot_agents::transport::kill::kill_process_group(pid, nix::sys::signal::Signal::SIGKILL);
  wait_until("crashed run finished", || {
    let events = chat_events(&stack.features.store, session_id)?;
    Ok(events.iter().any(|e| e["type"] == event_types::RUN_FINISHED && e["run_id"] == run_id))
  })
  .await?;

  let run = stack.features.store.read_run_sync(session_id, &run_id)?.context("run recorded")?;
  assert_ne!(run.exit_code, Some(0), "crash without TurnEnded must fail: {run:?}");
  let events = chat_events(&stack.features.store, session_id)?;
  let finished = events.iter().find(|e| e["type"] == event_types::RUN_FINISHED && e["run_id"] == run_id).unwrap();
  assert_eq!(finished["outcome"], "failed");
  stack.queue.shutdown().await?;
  Ok(())
}
