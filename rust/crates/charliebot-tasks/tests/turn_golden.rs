use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::backend_pool::BackendPool;
use charliebot_core::config::CREDENTIALS_FILENAME;
use charliebot_core::config::{
  BackendOption, BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
};
use charliebot_core::credentials::load_credentials;
use charliebot_core::event_types;
use charliebot_core::hot_reload::HotReload;
use charliebot_core::repo_root::RepoRoot;
use charliebot_core::run_token::{RunTokenClaims, sign_run_token};
use charliebot_registry::Registry;
use charliebot_registry::backend::{
  AgentBackend, BackendFactory, BackendTraits, BackendTypeRegistration, Env, LaunchArgs, ResumeStyle, Translate,
  TransportKind,
};
use charliebot_registry::caller::{self, CallerIdentity};
use charliebot_registry::context::{FeatureContext, StartupState};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, UtcDateTime};
use charliebot_store::run_records;
use charliebot_store::{
  AccountLabelKeeper, AnchorFieldReconciler, CreateTask, EmptyRuntimeEventHooks, RunKind, RunRecord,
  SequenceArchiveHook, SequenceBindingMover, SequenceListingFields, Store, StoreHookFuture, StoreHooks, StoreResult,
  TaskCaller,
};
use charliebot_tasks::autonamer;
use charliebot_tasks::dispatch;
use charliebot_tasks::execution::{self, TaskExecutionAdapter};
use charliebot_tasks::master_queue::{
  MarkUnreadFn, PersistAccountLabelFn, PersistAndBroadcastFn, PersistNativeIdFn, SessionCallbacks,
};
use charliebot_tasks::master_state::{
  MasterResumeRecord, MasterResumeRequest, MasterTurnRequest, TaskFinishCallback, TaskRunBinding, TaskSpawnCallback,
};
use charliebot_tasks::{master_queue, master_state, routes, task_sessions};
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use serde_yaml::{Mapping, Value as YamlValue};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tower::ServiceExt;

const CASE_COUNT: usize = 12;

mod process_guard;
use process_guard::ProcessGroupGuard;
static CANCEL_ROUTE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Deserialize)]
struct GoldenCase {
  name: String,
  mode: String,
  session_kind: String,
  events: Vec<Value>,
  exit_code: i32,
  stderr: String,
  session_id: String,
  persisted: Vec<Value>,
  mark_unread: Vec<String>,
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

  fn sequence_subtree_roots(&self, _metas: &[SessionMetadata]) -> StoreResult<HashMap<String, String>> {
    Ok(HashMap::new())
  }

  fn view_subtree_roots(&self, _metas: &[SessionMetadata]) -> StoreResult<HashMap<String, HashMap<String, String>>> {
    Ok(HashMap::new())
  }
}

struct ScriptFactory;

impl BackendFactory for ScriptFactory {
  fn build(
    &self,
    option: Option<&BackendOption>,
    _cfg: &CharlieBotConfig,
    _launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let option = option.context("script backend requires an option")?;
    let key = YamlValue::String("script".to_string());
    let script =
      option.type_fields.get(&key).and_then(YamlValue::as_str).context("scripted option has no script")?.to_string();
    let response_key = YamlValue::String("one_shot_response".to_string());
    let one_shot_response = option.type_fields.get(&response_key).and_then(YamlValue::as_str).map(str::to_string);
    let translation_error = option.backend_type == "translation_error_script";
    Ok(Box::new(ScriptBackend { script, one_shot_response, translation_error }))
  }
}

struct ScriptBackend {
  script: String,
  one_shot_response: Option<String>,
  translation_error: bool,
}

impl AgentBackend for ScriptBackend {
  fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
    None
  }

  fn prepare_transport(&mut self, _log_dir: &Path) -> anyhow::Result<()> {
    Ok(())
  }

  fn build_command(&self, _prompt: &str) -> anyhow::Result<Vec<String>> {
    Ok(vec!["/bin/sh".to_string(), "-c".to_string(), self.script.clone()])
  }

  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    Ok(env)
  }

  fn stdin_prompt(&self, _prompt: &str) -> Option<String> {
    None
  }

  fn translator(&self) -> Box<dyn Translate> {
    if self.translation_error { Box::new(FailingTranslate) } else { Box::new(IdentityTranslate) }
  }

  fn transport(&self) -> TransportKind {
    TransportKind::RawLog
  }

  fn one_shot_text<'a>(
    &'a mut self,
    _base_env: &'a Env,
    _prompt: &'a str,
    _system: &'a str,
    _timeout: Option<std::time::Duration>,
  ) -> Option<BoxFuture<'a, anyhow::Result<String>>> {
    let response = self.one_shot_response.clone();
    Some(Box::pin(async move { response.context("scripted backend has no one-shot response") }))
  }
}

struct IdentityTranslate;

impl Translate for IdentityTranslate {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
    Ok(vec![raw])
  }
}

struct FailingTranslate;

impl Translate for FailingTranslate {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
    if raw.get("type").and_then(Value::as_str) == Some("translate_error") {
      anyhow::bail!("synthetic translator failure");
    }
    Ok(vec![raw])
  }
}

fn register_scripted_backend(registry: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
  for (type_name, restart_reattach) in [
    ("codex", false),
    ("covered_script", true),
    ("uncovered_script", false),
    ("native_id_script", false),
    ("translation_error_script", false),
  ] {
    registry.backend_type(BackendTypeRegistration {
      type_name,
      traits: BackendTraits {
        resume: if type_name == "native_id_script" { ResumeStyle::NativeId } else { ResumeStyle::CliFlag },
        restart_reattach,
        preassigned_session_id: false,
        family_prefix: None,
        manual_compact: false,
        compact_hint: None,
      },
      option_model: |_| Ok(()),
      factory: Arc::new(ScriptFactory),
      lifecycle: None,
      translate_fallback: false,
    })?;
  }
  Ok(())
}

fn registry() -> Arc<Registry> {
  let mut registry = Registry::new();
  registry.register_package("test.scripted_backend", register_scripted_backend).unwrap();
  registry
    .register_package("charliebot_agents.claude_code", charliebot_agents::backends::claude_code::register)
    .unwrap();
  registry.register_package("charliebot.web_reply", charliebot_web_reply::register).unwrap();
  Arc::new(registry)
}

fn config(home: &Path, option: Option<BackendOption>) -> CharlieBotConfig {
  let options = option.into_iter().collect::<Vec<_>>();
  let pool = options.iter().map(|backend| backend.id.clone()).collect();
  CharlieBotConfig {
    charliebot_home: home.to_path_buf(),
    headless_chrome_bin: "/bin/false".to_string(),
    server: ServerConfig {
      host: "127.0.0.1".to_string(),
      port: 18650,
      subprocess_buffer_limit_mb: 1,
      session_memory_max_mb: 0,
      session_swap_max_mb: 0,
      session_tasks_max: 0,
      min_free_disk_gib: 0,
    },
    paths: PathsConfig {
      workspace_dirs: Vec::new(),
      worktree_dir: "/synthetic/worktrees".to_string(),
      scratch_dir: "/synthetic/scripts".to_string(),
    },
    backends: BackendsConfig { pool, routing: None, options },
    ui: UiConfig { home_services: Vec::new() },
    telegram: TelegramConfig { chat_id: None },
    sections: Sections::default(),
  }
}

fn backend_option(script: String) -> BackendOption {
  backend_option_for("fake", "codex", script, None)
}

fn backend_option_with_response(script: String, one_shot_response: Option<&str>) -> BackendOption {
  backend_option_for("fake", "codex", script, one_shot_response)
}

fn backend_option_for(id: &str, backend_type: &str, script: String, one_shot_response: Option<&str>) -> BackendOption {
  let mut type_fields = Mapping::new();
  type_fields.insert(YamlValue::String("script".to_string()), YamlValue::String(script));
  if let Some(response) = one_shot_response {
    type_fields.insert(YamlValue::String("one_shot_response".to_string()), YamlValue::String(response.to_string()));
  }
  BackendOption {
    id: id.to_string(),
    label: "Fake".to_string(),
    prompt_overlay: None,
    model: "fake-model".to_string(),
    backend_type: backend_type.to_string(),
    type_fields,
  }
}

fn save_session_metadata(features: &FeatureContext, registry: &Registry, meta: &SessionMetadata) -> anyhow::Result<()> {
  std::fs::create_dir_all(features.store.session_dir(&meta.id))?;
  std::fs::write(features.store.metadata_path(&meta.id), meta.to_json_string(&registry.metadata_slots(), true))?;
  Ok(())
}

fn config_with_features(
  home: &Path,
  option: Option<BackendOption>,
  registry: Arc<Registry>,
  install_queue: bool,
) -> anyhow::Result<(Arc<CharlieBotConfig>, Arc<FeatureContext>)> {
  let cfg = Arc::new(config(home, option));
  config_with_features_from_config(cfg, registry, install_queue)
}

fn config_with_features_from_config(
  cfg: Arc<CharlieBotConfig>,
  registry: Arc<Registry>,
  install_queue: bool,
) -> anyhow::Result<(Arc<CharlieBotConfig>, Arc<FeatureContext>)> {
  config_with_features_from_config_and_agent_path(cfg, registry, install_queue, "/usr/bin".to_string())
}

fn config_with_features_from_config_and_agent_path(
  cfg: Arc<CharlieBotConfig>,
  registry: Arc<Registry>,
  install_queue: bool,
  agent_path: String,
) -> anyhow::Result<(Arc<CharlieBotConfig>, Arc<FeatureContext>)> {
  std::fs::create_dir_all(&cfg.charliebot_home)?;
  let config_value = Arc::clone(&cfg);
  let config_file = cfg.config_file();
  let config_reload =
    HotReload::new(config_file, "golden_config_reload_failed", Box::new(move || Ok((*config_value).clone())));
  let credential_home = cfg.charliebot_home.clone();
  let credentials_reload = HotReload::new(
    cfg.credentials_file(),
    "golden_credentials_reload_failed",
    Box::new(move || load_credentials(&credential_home)),
  );
  let test_hooks = Arc::new(TestHooks);
  let store = Store::new(
    (*cfg).clone(),
    registry.metadata_slots(),
    StoreHooks {
      account_labels: Some(test_hooks.clone()),
      anchor_reconciler: Some(test_hooks.clone()),
      sequence_listing: Some(test_hooks.clone()),
      sequence_archive: Some(test_hooks.clone()),
      sequence_binding_mover: Some(test_hooks),
      runtime_events: Some(Arc::new(EmptyRuntimeEventHooks)),
    },
  )?;
  let startup = Arc::new(StartupState::default());
  startup.insert(AgentBaseEnvironment {
    values: Env::from([
      ("PATH".to_string(), agent_path),
      ("CLAUDECODE".to_string(), "stale-parent".to_string()),
      (charliebot_agents::launch_loop::SESSION_ID_ENV.to_string(), "stale-session".to_string()),
    ]),
  })?;
  startup.insert(BackendPool::default())?;
  let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
    .ancestors()
    .nth(3)
    .context("tasks crate manifest has no repository root ancestor")?;
  startup.insert(RepoRoot::new(Some(checkout.as_os_str().to_os_string()))?)?;
  if install_queue {
    let features = Arc::new(FeatureContext {
      config: Arc::new(config_reload),
      credentials: Arc::new(credentials_reload),
      store: Arc::new(store),
      registry,
      startup: Arc::clone(&startup),
    });
    let pool = charliebot_agents::backend_pool::backend_pool(&features)?;
    startup.insert(master_queue::MasterQueue::new(pool))?;
    return Ok((Arc::clone(&cfg), features));
  }
  Ok((
    Arc::clone(&cfg),
    Arc::new(FeatureContext {
      config: Arc::new(config_reload),
      credentials: Arc::new(credentials_reload),
      store: Arc::new(store),
      registry,
      startup,
    }),
  ))
}

fn session_meta(session_id: &str, session_kind: &str, mode: &str) -> SessionMetadata {
  let mut meta = SessionMetadata::new(session_id, "Session 1", TaskProfile::Manager);
  if mode == "unknown_pin" {
    meta.backend = "deleted-id".to_string();
  } else if mode != "refused" {
    meta.backend = "fake".to_string();
  }
  if session_kind == "thread" {
    meta.extra.insert(
      "slack_origin".to_string(),
      json!({
        "team_id": "synthetic-team",
        "channel_id": "synthetic-channel",
        "thread_ts": "1700000000.000100"
      }),
    );
  }
  meta
}

fn callbacks(features: Arc<FeatureContext>, unread: Arc<Mutex<Vec<String>>>) -> SessionCallbacks {
  let persist_features = Arc::clone(&features);
  let persist_and_broadcast: PersistAndBroadcastFn = Arc::new(move |session_id, event| {
    let store = Arc::clone(&persist_features.store);
    Box::pin(async move {
      store.persist_and_broadcast(&session_id, event).await?;
      Ok(())
    })
  });
  let unread_features = Arc::clone(&features);
  let mark_unread: MarkUnreadFn = Arc::new(move |session_id| {
    let store = Arc::clone(&unread_features.store);
    let unread = Arc::clone(&unread);
    Box::pin(async move {
      unread.lock().expect("golden unread mutex is not poisoned").push(session_id.clone());
      store.mark_unread(&session_id).await?;
      Ok(())
    })
  });
  let persist_cc_session_id: PersistNativeIdFn =
    Arc::new(move |_session_id, value, _backend| Box::pin(async move { Ok(Some(value)) }));
  let persist_account_label: PersistAccountLabelFn =
    Arc::new(move |_session_id, label| Box::pin(async move { Ok(Some(label)) }));
  SessionCallbacks {
    features,
    persist_and_broadcast,
    mark_unread,
    persist_cc_session_id,
    persist_account_label: Some(persist_account_label),
    context_state: Some(Arc::new(move |_session_id, _session_meta| Box::pin(async { Ok((None, None)) }))),
    task_tree_activity: Arc::new(|_session_id| Ok((false, "".to_string()))),
    after_round: None,
  }
}

fn scripted_process(events: &[Value], exit_code: i32, stderr: &str) -> anyhow::Result<String> {
  let lines = events.iter().map(serde_json::to_string).collect::<Result<Vec<_>, _>>()?;
  let mut script = format!("cat <<'CB_EVENTS'\n{}\nCB_EVENTS\n", lines.join("\n"));
  if !stderr.is_empty() {
    script.push_str(&format!("cat <<'CB_STDERR' >&2\n{stderr}\nCB_STDERR\n"));
  }
  script.push_str(&format!("exit {exit_code}\n"));
  Ok(script)
}

async fn run_fresh_case(
  case: &GoldenCase,
  temp: &TempDir,
  index: usize,
  registry: Arc<Registry>,
) -> anyhow::Result<(Vec<charliebot_store::ChatEvent>, Vec<String>)> {
  let configured_option = if case.mode == "unknown_pin" {
    Some(backend_option(String::new()))
  } else if case.mode == "refused" {
    None
  } else {
    Some(backend_option(scripted_process(&case.events, case.exit_code, &case.stderr)?))
  };
  let turn_option = if case.mode == "unknown_pin" { None } else { configured_option.clone() };
  let home = temp.path().join(format!("home-{index}"));
  let slots = registry.metadata_slots();
  let (cfg, features) = config_with_features(&home, configured_option, Arc::clone(&registry), true)?;
  let meta = session_meta(&case.session_id, &case.session_kind, &case.mode);
  std::fs::create_dir_all(features.store.session_dir(&meta.id))?;
  std::fs::write(features.store.metadata_path(&meta.id), meta.to_json_string(&slots, true))?;
  let unread = Arc::new(Mutex::new(Vec::new()));
  let callbacks = callbacks(Arc::clone(&features), Arc::clone(&unread));
  let queue = features.startup.require::<master_queue::MasterQueue>()?;
  let run_id = format!("golden-run-{index}");
  let transport_dir = home.join("runs").join(&meta.id).join(&run_id);
  std::fs::create_dir_all(&transport_dir)?;
  let on_task_spawn: TaskSpawnCallback = Arc::new(|_pid, _start| Box::pin(async { Ok(()) }));
  let on_task_finish: TaskFinishCallback =
    Arc::new(|_native_id, _exit_code, _extras, _done_event| Box::pin(async { Ok(false) }));
  queue
    .run_message(MasterTurnRequest {
      cfg,
      session_meta: meta.clone(),
      user_content: "hi".to_string(),
      callbacks,
      user_event_ids: Vec::new(),
      task_instructions: "instructions".to_string(),
      task_run: TaskRunBinding { session_id: meta.id.clone(), run_id, transport_dir, fresh_native_context: false },
      on_task_spawn,
      on_task_finish,
      auto_trigger: false,
      backend_option: turn_option,
      extra_claude_flags: None,
      uploaded_files: None,
      extra_env: None,
    })
    .await?;
  queue.shutdown().await?;
  let events = features.store.load_chat_events_sync(&case.session_id)?;
  let unread = unread.lock().expect("golden unread mutex is not poisoned").clone();
  Ok((events, unread))
}

async fn run_resume_case(
  case: &GoldenCase,
  temp: &TempDir,
  index: usize,
  registry: Arc<Registry>,
) -> anyhow::Result<(Vec<charliebot_store::ChatEvent>, Vec<String>)> {
  let option = backend_option(String::new());
  let home = temp.path().join(format!("home-{index}"));
  let slots = registry.metadata_slots();
  let (cfg, features) = config_with_features(&home, Some(option), Arc::clone(&registry), true)?;
  let meta = session_meta(&case.session_id, &case.session_kind, &case.mode);
  std::fs::create_dir_all(features.store.session_dir(&meta.id))?;
  std::fs::write(features.store.metadata_path(&meta.id), meta.to_json_string(&slots, true))?;
  let unread = Arc::new(Mutex::new(Vec::new()));
  let callbacks = callbacks(Arc::clone(&features), Arc::clone(&unread));
  let queue = features.startup.require::<master_queue::MasterQueue>()?;
  let raw_path = home.join("resume").join("agent.raw.ndjson");
  std::fs::create_dir_all(raw_path.parent().context("resume log parent is missing")?)?;
  let raw = case.events.iter().map(serde_json::to_string).collect::<Result<Vec<_>, _>>()?.join("\n");
  std::fs::write(&raw_path, format!("{raw}\n"))?;
  let record = MasterResumeRecord {
    pid: None,
    pid_start: None,
    started_at: UtcDateTime::now(),
    raw_log: raw_path,
    user_event_ids: Vec::new(),
  };
  let run_id = format!("golden-resume-{index}");
  let transport_dir = home.join("runs").join(&meta.id).join(&run_id);
  std::fs::create_dir_all(&transport_dir)?;
  let on_task_spawn: TaskSpawnCallback = Arc::new(|_pid, _start| Box::pin(async { Ok(()) }));
  let on_task_finish: TaskFinishCallback =
    Arc::new(|_native_id, _exit_code, _extras, _done_event| Box::pin(async { Ok(false) }));
  let receiver = queue
    .enqueue_master_resume(MasterResumeRequest {
      cfg,
      session_meta: meta,
      record,
      callbacks,
      is_alive: Arc::new(|| false),
      task_run: TaskRunBinding {
        session_id: case.session_id.clone(),
        run_id,
        transport_dir,
        fresh_native_context: false,
      },
      on_task_spawn,
      on_task_finish,
      extra_env: None,
    })
    .await?;
  receiver.await.context("resume consumer closed the turn result")??;
  queue.shutdown().await?;
  let events = features.store.load_chat_events_sync(&case.session_id)?;
  let unread = unread.lock().expect("golden unread mutex is not poisoned").clone();
  Ok((events, unread))
}

fn normalize_event(event: &Value) -> Value {
  let mut event = event.clone();
  let Some(fields) = event.as_object_mut() else { return event };
  for key in ["id", "timestamp", "event_index"] {
    fields.remove(key);
  }
  if fields.get("type").and_then(Value::as_str) == Some("master_done") {
    for key in ["total_ms", "spawn_to_first_event_ms", "first_assistant_ms", "thinking_seconds"] {
      if fields.contains_key(key) {
        fields.insert(key.to_string(), json!("<elapsed>"));
      }
    }
  }
  event
}

fn noop_task_spawn() -> TaskSpawnCallback {
  Arc::new(|_pid, _start| Box::pin(async { Ok(()) }))
}

fn noop_task_finish() -> TaskFinishCallback {
  Arc::new(|_native_id, _exit_code, _extras, _done_event| Box::pin(async { Ok(false) }))
}

async fn run_turn(
  queue: Arc<master_queue::MasterQueue>,
  cfg: Arc<CharlieBotConfig>,
  meta: SessionMetadata,
  callbacks: SessionCallbacks,
  event_id: String,
  option: BackendOption,
) -> anyhow::Result<Option<String>> {
  run_turn_with_spawn(queue, cfg, meta, callbacks, event_id, option, noop_task_spawn()).await
}

async fn run_turn_with_spawn(
  queue: Arc<master_queue::MasterQueue>,
  cfg: Arc<CharlieBotConfig>,
  meta: SessionMetadata,
  callbacks: SessionCallbacks,
  event_id: String,
  option: BackendOption,
  on_task_spawn: TaskSpawnCallback,
) -> anyhow::Result<Option<String>> {
  let run_id = format!("run-{event_id}");
  let transport_dir = cfg.sessions_dir().join(&meta.id).join("runs").join(&run_id);
  std::fs::create_dir_all(&transport_dir)?;
  queue
    .run_message(MasterTurnRequest {
      cfg,
      session_meta: meta.clone(),
      user_content: "synthetic prompt".to_string(),
      callbacks,
      user_event_ids: vec![event_id],
      task_instructions: "synthetic instructions".to_string(),
      task_run: TaskRunBinding { session_id: meta.id, run_id, transport_dir, fresh_native_context: false },
      on_task_spawn,
      on_task_finish: noop_task_finish(),
      auto_trigger: false,
      backend_option: Some(option),
      extra_claude_flags: None,
      uploaded_files: None,
      extra_env: None,
    })
    .await
}

fn python_golden_cases() -> anyhow::Result<Vec<GoldenCase>> {
  let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let helper = manifest.join("tests/python_turn_golden.py");
  let root = manifest
    .parent()
    .context("tasks crate has no crates directory")?
    .parent()
    .context("Rust directory is missing")?
    .parent()
    .context("repository root is missing")?;
  let output = Command::new("uv").args(["run", "python"]).arg(helper).current_dir(root).output()?;
  if !output.status.success() {
    bail!("Python golden helper failed with {}: {}", output.status, String::from_utf8_lossy(&output.stderr));
  }
  let json_line = output
    .stdout
    .split(|byte| *byte == b'\n')
    .rev()
    .find(|line| line.starts_with(b"["))
    .context("Python golden helper printed no JSON array")?;
  let cases = serde_json::from_slice(json_line).context("Python golden helper printed invalid JSON")?;
  Ok(cases)
}

#[tokio::test]
async fn manager_turn_matches_python_event_stream_matrix() -> anyhow::Result<()> {
  let cases = python_golden_cases()?;
  assert_eq!(cases.len(), CASE_COUNT, "turn golden matrix size changed");
  let registry = registry();
  let temp = tempfile::tempdir()?;
  for (zero_index, case) in cases.iter().enumerate() {
    let index = zero_index + 1;
    let (actual, unread) = if case.mode == "resume" {
      run_resume_case(case, &temp, index, Arc::clone(&registry)).await?
    } else {
      run_fresh_case(case, &temp, index, Arc::clone(&registry)).await?
    };
    let actual = actual.iter().map(|event| normalize_event(&event.to_value())).collect::<Vec<_>>();
    let expected = case.persisted.iter().map(normalize_event).collect::<Vec<_>>();
    assert_eq!(actual, expected, "persisted events differ for {}", case.name);
    assert_eq!(unread, case.mark_unread, "unread calls differ for {}", case.name);
  }
  Ok(())
}

#[tokio::test]
async fn dispatcher_launches_manager_turns_with_registered_identity_and_serializes_pending_inputs() -> anyhow::Result<()>
{
  const ACCESS_KEY: &str = "synthetic-manager-access-key";
  let temp = tempfile::tempdir()?;
  let guard = ProcessGroupGuard::new();
  let home = temp.path().join("manager-adapter-home");
  std::fs::create_dir_all(&home)?;
  std::fs::write(home.join(CREDENTIALS_FILENAME), format!("charliebot:\n  access_key: {ACCESS_KEY}\n"))?;

  let first_started = home.join("first-started");
  let second_started = home.join("second-started");
  let release_first = home.join("release-first");
  let mut script = "printf '%s\\n' \"$CHARLIEBOT_RUN_TOKEN\" >> \"$CHARLIEBOT_HOME/run-tokens\"\n".to_string();
  script.push_str(&format!(
    "if [ ! -e '{}' ]; then touch '{}'; while [ ! -e '{}' ] && [ -d '{}' ]; do /bin/sleep 0.01; done; else touch '{}'; fi\n",
    first_started.display(),
    first_started.display(),
    release_first.display(),
    temp.path().display(),
    second_started.display(),
  ));
  script.push_str(&scripted_process(
    &[
      json!({"type":"system","subtype":"init","session_id":"synthetic-native-session"}),
      json!({"type":"assistant","message":{"content":[{"type":"text","text":"synthetic answer"}]}}),
      json!({"type":"result","subtype":"success","result":"","usage":{"input_tokens":1,"output_tokens":1}}),
    ],
    0,
    "",
  )?);
  let option = backend_option(script);
  let mut cfg = config(&home, Some(option));
  cfg.server.min_free_disk_gib = i64::MAX;
  let registry = registry();
  let (cfg, features) = config_with_features_from_config(Arc::new(cfg), Arc::clone(&registry), false)?;
  execution::pre_recovery_step(Arc::clone(&features)).await?;

  let meta = features
    .store
    .create_task(
      CreateTask {
        request_id: "synthetic-manager-adapter".to_string(),
        task_parent_id: None,
        profile: TaskProfile::Manager,
        task: None,
        name: Some("Synthetic manager".to_string()),
        backend: Some("fake".to_string()),
        group: None,
        session_id: Some("synthetic-manager-adapter-session".to_string()),
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  let dispatcher = dispatch::dispatcher(&features)?;
  let first_input = dispatcher
    .admit_input(&meta.id, event_types::USER, "first synthetic input", "user", json!({"input_id":"first-input"}))
    .await?;
  let first_decision = dispatcher.dispatch_pending(&meta.id).await?;
  assert_eq!(first_decision["launch"], true);
  let first_run_id = first_decision["run_id"].as_str().context("first dispatch has no run id")?.to_string();
  assert_eq!(
    features
      .store
      .read_run_sync(&meta.id, &first_run_id)?
      .context("first run record was not registered")?
      .input_event_ids,
    [first_input["id"].as_str().context("first input has no id")?.to_string()]
  );

  tokio::time::timeout(Duration::from_secs(5), async {
    while !first_started.exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .context("first manager child did not reach its gate")?;
  let launched =
    features.store.read_run_sync(&meta.id, &first_run_id)?.context("first run record disappeared after launch")?;
  guard.track(launched.pid.context("first manager child did not persist a pid")?);
  assert!(launched.pid_start.is_some());
  let token = std::fs::read_to_string(home.join("run-tokens"))?;
  let token = token.lines().next().context("manager child did not receive a run token")?;
  let mut headers = HeaderMap::new();
  headers.insert(header::AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {token}"))?);
  assert!(matches!(caller::resolve_caller(&headers, &features), Ok(CallerIdentity::Agent { .. })));

  let second_input = dispatcher
    .admit_input(&meta.id, event_types::USER, "second synthetic input", "user", json!({"input_id":"second-input"}))
    .await?;
  let waiting_decision = dispatcher.dispatch_pending(&meta.id).await?;
  assert_eq!(waiting_decision["launch"], false);
  std::fs::write(&release_first, b"continue")?;

  tokio::time::timeout(Duration::from_secs(8), async {
    loop {
      let facts = features.store.load_all_facts(&meta.id)?;
      let finished = facts
        .iter()
        .filter(|event| event["type"] == event_types::RUN_FINISHED)
        .filter_map(|event| event["run_id"].as_str())
        .collect::<Vec<_>>();
      if finished.len() == 2 && second_started.exists() {
        break anyhow::Ok(());
      }
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
  })
  .await
  .context("the manager adapter did not dispatch the input queued during the first turn")??;

  let records = features.store.list_run_records_sync(&meta.id)?;
  assert_eq!(records.len(), 2, "the two durable inputs must produce exactly two manager runs");
  let second_run = records.iter().find(|run| run.id != first_run_id).context("second run was not registered")?;
  assert_eq!(second_run.input_event_ids, [second_input["id"].as_str().context("second input has no id")?.to_string()]);
  let finished = features
    .store
    .load_all_facts(&meta.id)?
    .into_iter()
    .filter(|event| event["type"] == event_types::RUN_FINISHED)
    .collect::<Vec<_>>();
  assert_eq!(finished.len(), 2);
  assert!(finished.iter().all(|event| event["outcome"] == "success"));
  features.startup.require::<master_queue::MasterQueue>()?.shutdown().await?;
  drop(cfg);
  Ok(())
}

async fn cancel_gated_manager_runs_through_route(chat_cancel: bool) -> anyhow::Result<()> {
  let _serial = CANCEL_ROUTE_TEST_LOCK.lock().await;
  const ACCESS_KEY: &str = "synthetic-cancel-route-access-key";
  const SESSION_ID: &str = "synthetic-cancel-loop-session";
  const ITERATIONS: usize = 50;

  let temp = tempfile::tempdir()?;
  let guard = ProcessGroupGuard::new();
  let home = temp.path().join("home");
  std::fs::create_dir_all(&home)?;
  std::fs::write(home.join(CREDENTIALS_FILENAME), format!("charliebot:\n  access_key: {ACCESS_KEY}\n"))?;
  let control_dir = temp.path().join("fake-agent-control");
  std::fs::create_dir_all(&control_dir)?;
  // The fake agent runs inline through `/bin/sh -c`, like the other scripted backends of this file. It waits
  // until its release file appears or its control directory is removed.
  let script = format!(
    "counter=0; if [ -f '{0}/count' ]; then counter=$(/bin/cat '{0}/count'); fi; counter=$((counter + 1)); printf '%s' \"$counter\" > '{0}/count'; touch '{0}/started-'\"$counter\"; printf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"synthetic-native-session\"}}'; while [ ! -e '{0}/release-'\"$counter\" ] && [ -d '{0}' ]; do /bin/sleep 0.01; done",
    control_dir.display()
  );
  let option = backend_option(script);
  let cfg = Arc::new(config(&home, Some(option)));
  let registry = registry();
  let (cfg, features) = config_with_features_from_config(cfg, Arc::clone(&registry), false)?;
  execution::pre_recovery_step(Arc::clone(&features)).await?;
  features
    .store
    .create_task(
      CreateTask {
        request_id: "synthetic-cancel-loop-task".to_string(),
        task_parent_id: None,
        profile: TaskProfile::Manager,
        task: None,
        name: Some("Synthetic cancellation loop".to_string()),
        backend: Some("fake".to_string()),
        group: None,
        session_id: Some(SESSION_ID.to_string()),
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  let dispatcher = dispatch::dispatcher(&features)?;
  let app = routes::routes(&features);

  for iteration in 1..=ITERATIONS {
    let request_id = format!("synthetic-cancel-input-{iteration}");
    let admitted = dispatcher
      .admit_input(
        SESSION_ID,
        event_types::USER,
        "Hold this synthetic manager turn for cancellation.",
        "user",
        json!({"input_id": request_id}),
      )
      .await?;
    let input_event_id = admitted["id"].as_str().context("admitted manager input has no id")?.to_string();
    let decision = dispatcher.dispatch_pending(SESSION_ID).await?;
    assert_eq!(decision["launch"], true, "iteration {iteration} did not launch");
    let run_id = decision["run_id"].as_str().context("manager dispatch did not return its run id")?.to_string();
    let started = control_dir.join(format!("started-{iteration}"));
    tokio::time::timeout(Duration::from_secs(30), async {
      while !started.exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    })
    .await
    .with_context(|| format!("fake agent {iteration} did not reach its gate"))?;
    let run = features
      .store
      .read_run_sync(SESSION_ID, &run_id)?
      .context("iteration {iteration} lost its run record before its gate")?;
    guard.track(run.pid.context("iteration {iteration} did not persist the child pid before its gate")?);

    let (uri, body) = if chat_cancel {
      (format!("/api/chat/{SESSION_ID}/cancel"), String::new())
    } else {
      (
        format!("/api/sessions/{SESSION_ID}/runs/{run_id}/cancel"),
        format!(r#"{{"request_id":"synthetic-cancel-request-{iteration}"}}"#),
      )
    };
    let response = app
      .clone()
      .oneshot(
        Request::builder()
          .method("POST")
          .uri(uri)
          .header(header::AUTHORIZATION, format!("Bearer {ACCESS_KEY}"))
          .header(header::CONTENT_TYPE, "application/json")
          .body(Body::from(body))?,
      )
      .await?;
    assert_eq!(response.status(), StatusCode::OK, "cancel failed on iteration {iteration}");
    if !chat_cancel {
      let body = to_bytes(response.into_body(), usize::MAX).await?;
      assert_eq!(serde_json::from_slice::<Value>(&body)?["stop_requested"], true);
    }

    tokio::time::timeout(Duration::from_secs(30), async {
      loop {
        let facts = features.store.load_all_facts(SESSION_ID)?;
        // Python does not order these two writes. The master queue persists MASTER_DONE when the CC
        // round ends (src/runtime/master_cc_queue.py:298-309), while the stop path records the
        // interrupted run_finished only after the child process exits (src/runtime/runs.py:1218-1224).
        // The chat cancel takes the same stop path (src/runtime/api/chat.py:106). The two writers race,
        // so accept either order and check both events once both exist.
        let done = facts.iter().position(|event| {
          event["type"] == event_types::MASTER_DONE && event["input_event_ids"].to_value() == json!([input_event_id])
        });
        let finished =
          facts.iter().position(|event| event["type"] == event_types::RUN_FINISHED && event["run_id"] == run_id);
        if let (Some(_), Some(finished_index)) = (done, finished) {
          assert_eq!(facts[finished_index]["outcome"], "interrupted", "iteration {iteration}");
          break anyhow::Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    })
    .await
    .with_context(|| format!("manager run {run_id} did not finish with both required events"))??;
  }

  drop(guard);
  tokio::time::timeout(Duration::from_secs(10), async {
    loop {
      if !has_process_for_home_and_session(&home, SESSION_ID)? {
        break anyhow::Ok(());
      }
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
  })
  .await
  .context("fake agents outlived the guard")??;

  features.startup.require::<master_queue::MasterQueue>()?.shutdown().await?;
  drop(cfg);
  Ok(())
}

fn has_process_for_home_and_session(home: &Path, session_id: &str) -> anyhow::Result<bool> {
  let home_environment = format!("CHARLIEBOT_HOME={}", home.display());
  let session_environment = format!("CHARLIEBOT_SESSION_ID={session_id}");
  for entry in std::fs::read_dir("/proc")? {
    let entry = entry?;
    if entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()).is_none() {
      continue;
    }
    let environment = match std::fs::read(entry.path().join("environ")) {
      Ok(environment) => environment,
      Err(error)
        if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied)
          || matches!(
            error.raw_os_error(),
            Some(libc::ENOENT) | Some(libc::ESRCH) | Some(libc::EACCES) | Some(libc::EPERM)
          ) =>
      {
        continue;
      }
      Err(error) => return Err(error.into()),
    };
    let variables = environment.split(|byte| *byte == 0).collect::<Vec<_>>();
    if variables.contains(&home_environment.as_bytes()) && variables.contains(&session_environment.as_bytes()) {
      return Ok(true);
    }
  }
  Ok(false)
}

#[tokio::test]
async fn chat_cancel_records_master_done_and_an_interrupted_finish_for_50_gated_manager_runs() -> anyhow::Result<()> {
  cancel_gated_manager_runs_through_route(true).await
}

#[tokio::test]
async fn run_cancel_records_master_done_and_an_interrupted_finish_for_50_gated_manager_runs() -> anyhow::Result<()> {
  cancel_gated_manager_runs_through_route(false).await
}

#[tokio::test]
async fn worker_execution_requires_a_registered_run() -> anyhow::Result<()> {
  let home = tempfile::tempdir()?;
  let option = backend_option(scripted_process(&[json!({"type":"result","subtype":"success"})], 0, "")?);
  let registry = registry();
  let (_cfg, features) = config_with_features(home.path(), Some(option), Arc::clone(&registry), false)?;
  execution::pre_recovery_step(Arc::clone(&features)).await?;
  let worker = features
    .store
    .create_task(
      CreateTask {
        request_id: "synthetic-worker-refusal".to_string(),
        task_parent_id: None,
        profile: TaskProfile::Worker,
        task: None,
        name: Some("Synthetic worker".to_string()),
        backend: Some("fake".to_string()),
        group: None,
        session_id: Some("synthetic-worker-refusal-session".to_string()),
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  let before = features.store.load_all_facts(&worker.id)?;
  let adapter = features.startup.require::<TaskExecutionAdapter>()?;
  assert!(adapter.call(&worker.id, &[], None).await?.is_none());
  let error = adapter.execute_run(&worker.id, "synthetic-unknown-run").await.unwrap_err().to_string();
  assert!(error.contains("synthetic-unknown-run"), "{error}");
  assert!(error.contains("not found"), "{error}");
  assert!(features.store.list_run_records_sync(&worker.id)?.is_empty());
  assert_eq!(features.store.load_all_facts(&worker.id)?, before);
  features.startup.require::<master_queue::MasterQueue>()?.shutdown().await?;
  Ok(())
}

#[tokio::test]
async fn agent_run_caller_cannot_retry_or_cancel_another_run() -> anyhow::Result<()> {
  const ACCESS_KEY: &str = "synthetic-route-access-key";
  let home = tempfile::tempdir()?;
  std::fs::write(home.path().join(CREDENTIALS_FILENAME), format!("charliebot:\n  access_key: {ACCESS_KEY}\n"))?;
  let option = backend_option(String::new());
  let registry = registry();
  let (_cfg, features) = config_with_features(home.path(), Some(option), Arc::clone(&registry), false)?;
  let session_id = "synthetic-agent-route-session";
  features
    .store
    .create_task(
      CreateTask {
        request_id: "synthetic-agent-route-node".to_string(),
        task_parent_id: None,
        profile: TaskProfile::Manager,
        task: None,
        name: Some("Synthetic route node".to_string()),
        backend: Some("fake".to_string()),
        group: None,
        session_id: Some(session_id.to_string()),
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  let claims = RunTokenClaims {
    session_id: session_id.to_string(),
    run_id: "synthetic-own-run".to_string(),
    agent: "synthetic-agent".to_string(),
  };
  let run = RunRecord {
    id: claims.run_id.clone(),
    session_id: claims.session_id.clone(),
    kind: RunKind::ManagerTurn,
    input_event_ids: Vec::new(),
    task_spec_hash: None,
    task_spec_ref: None,
    prompt_snapshot_ref: None,
    retry_of_run_id: None,
    review_of_run_id: None,
    backend: Some("fake".to_string()),
    model: Some("fake-model".to_string()),
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
  };
  let guard = features.store.control_lock().await;
  features.store.register_run_locked(&guard, session_id, run, None).await?;
  drop(guard);
  features.store.record_launch(session_id, &claims.run_id, 424242, "12345").await?;

  let token = sign_run_token(&claims, ACCESS_KEY)?;
  let app = routes::routes(&features);
  let retry = app
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/api/sessions/{session_id}/retry"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"request_id":"synthetic-agent-retry","run_id":"synthetic-own-run"}"#))?,
    )
    .await?;
  assert_eq!(retry.status(), StatusCode::FORBIDDEN);
  let retry_body = to_bytes(retry.into_body(), usize::MAX).await?;
  assert_eq!(
    serde_json::from_slice::<Value>(&retry_body)?,
    json!({"detail":"retrying a run requires operator credentials"})
  );

  let cancel = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(format!("/api/sessions/{session_id}/runs/synthetic-other-run/cancel"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"request_id":"synthetic-agent-cancel"}"#))?,
    )
    .await?;
  assert_eq!(cancel.status(), StatusCode::FORBIDDEN);
  let cancel_body = to_bytes(cancel.into_body(), usize::MAX).await?;
  assert_eq!(
    serde_json::from_slice::<Value>(&cancel_body)?,
    json!({"detail":"an agent may only stop its own bound run"})
  );
  Ok(())
}

#[tokio::test]
async fn retry_record_reuses_the_request_and_preserves_the_manager_input_batch() -> anyhow::Result<()> {
  let home = tempfile::tempdir()?;
  let option = backend_option(String::new());
  let registry = registry();
  let (cfg, features) = config_with_features(home.path(), Some(option), Arc::clone(&registry), false)?;
  let session_id = "synthetic-retry-session";
  features
    .store
    .create_task(
      CreateTask {
        request_id: "synthetic-retry-node".to_string(),
        task_parent_id: None,
        profile: TaskProfile::Manager,
        task: None,
        name: Some("Synthetic retry node".to_string()),
        backend: Some("fake".to_string()),
        group: None,
        session_id: Some(session_id.to_string()),
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  let original = RunRecord {
    id: "synthetic-failed-manager-run".to_string(),
    session_id: session_id.to_string(),
    kind: RunKind::ManagerTurn,
    input_event_ids: vec!["synthetic-manager-input".to_string()],
    task_spec_hash: None,
    task_spec_ref: None,
    prompt_snapshot_ref: None,
    retry_of_run_id: None,
    review_of_run_id: None,
    backend: Some("fake".to_string()),
    model: Some("fake-model".to_string()),
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
  };
  let guard = features.store.control_lock().await;
  features.store.register_run_locked(&guard, session_id, original, None).await?;
  drop(guard);

  let first = task_sessions::create_retry(
    &features.store,
    &cfg,
    session_id,
    "synthetic-retry-request",
    "synthetic-failed-manager-run",
    None,
    None,
  )
  .await?;
  let replay = task_sessions::create_retry(
    &features.store,
    &cfg,
    session_id,
    "synthetic-retry-request",
    "synthetic-failed-manager-run",
    None,
    None,
  )
  .await?;
  assert_eq!(first.id, replay.id);
  assert_eq!(first.retry_of_run_id.as_deref(), Some("synthetic-failed-manager-run"));
  assert_eq!(first.kind, RunKind::ManagerTurn);
  assert_eq!(first.input_event_ids, ["synthetic-manager-input"]);
  assert_eq!(first.backend.as_deref(), Some("fake"));
  assert_eq!(first.model.as_deref(), Some("fake-model"));
  let miss = task_sessions::create_retry(
    &features.store,
    &cfg,
    session_id,
    "synthetic-retry-request",
    "synthetic-missing-run",
    None,
    None,
  )
  .await
  .expect_err("retrying an unknown run refuses");
  assert_eq!(miss.to_string(), format!("run synthetic-missing-run not found in task {session_id}"));
  Ok(())
}

#[tokio::test]
async fn exported_queue_reads_fail_when_startup_has_no_master_queue() -> anyhow::Result<()> {
  let home = tempfile::tempdir()?;
  let registry = registry();
  let (_cfg, features) = config_with_features(home.path(), None, registry, false)?;
  assert!(master_queue::queued_user_event_ids(&features, "synthetic-session").is_err());
  assert!(master_state::running_user_event_ids(&features, "synthetic-session").is_err());
  Ok(())
}

#[tokio::test]
async fn queue_serializes_enqueues_and_exposes_running_and_queued_event_ids() -> anyhow::Result<()> {
  let temp = tempfile::tempdir()?;
  let registry = registry();
  let assistant = json!({"type":"assistant","message":{"content":[{"type":"text","text":"synthetic answer"}]}});
  let result = json!({
    "type":"result",
    "result":"",
    "usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},
    "total_cost_usd":0
  });
  let first_line = serde_json::to_string(&assistant)?;
  let last_line = serde_json::to_string(&result)?;
  let script =
    format!("cat <<'CB_FIRST'\n{first_line}\nCB_FIRST\n/bin/sleep 0.7\ncat <<'CB_LAST'\n{last_line}\nCB_LAST\n");
  let option = backend_option(script);
  let home = temp.path().join("queue-home");
  let (cfg, features) = config_with_features(&home, Some(option.clone()), Arc::clone(&registry), true)?;
  let meta = session_meta("serialized-session", "web", "fresh");
  save_session_metadata(&features, &registry, &meta)?;
  let queue = features.startup.require::<master_queue::MasterQueue>()?;
  let first_callbacks = callbacks(Arc::clone(&features), Arc::new(Mutex::new(Vec::new())));
  let first = tokio::spawn(run_turn(
    Arc::clone(&queue),
    Arc::clone(&cfg),
    meta.clone(),
    first_callbacks,
    "first-input".to_string(),
    option.clone(),
  ));
  tokio::time::timeout(std::time::Duration::from_secs(2), async {
    loop {
      if master_state::running_user_event_ids(&features, &meta.id)? == ["first-input"] {
        break anyhow::Ok(());
      }
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
  })
  .await??;

  let second_callbacks = callbacks(Arc::clone(&features), Arc::new(Mutex::new(Vec::new())));
  let second = tokio::spawn(run_turn(
    Arc::clone(&queue),
    Arc::clone(&cfg),
    meta.clone(),
    second_callbacks,
    "second-input".to_string(),
    option,
  ));
  let expected = std::collections::HashSet::from(["first-input".to_string(), "second-input".to_string()]);
  tokio::time::timeout(std::time::Duration::from_secs(2), async {
    loop {
      if master_queue::queued_user_event_ids(&features, &meta.id)? == expected {
        break anyhow::Ok(());
      }
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
  })
  .await??;
  assert_eq!(master_state::running_user_event_ids(&features, &meta.id)?, ["first-input"]);
  assert!(features.store.busy_since(&meta.id).is_some());
  first.await??;
  tokio::time::timeout(std::time::Duration::from_secs(2), async {
    loop {
      if master_state::running_user_event_ids(&features, &meta.id)? == ["second-input"] {
        break anyhow::Ok(());
      }
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
  })
  .await??;
  second.await??;
  queue.shutdown().await?;
  assert!(features.store.busy_since(&meta.id).is_none());
  assert!(master_queue::queued_user_event_ids(&features, &meta.id)?.is_empty());
  Ok(())
}

#[tokio::test]
async fn translation_failure_reports_the_turn_and_keeps_the_session_consumer_alive() -> anyhow::Result<()> {
  let temp = tempfile::tempdir()?;
  let guard = Arc::new(ProcessGroupGuard::new());
  let spawn_guard = Arc::clone(&guard);
  let registry = registry();
  let home = temp.path().join("translate-error-home");
  let gate = home.join("release-translation-error");
  let gate_text = gate.to_str().context("synthetic gate path is not UTF-8")?;
  let home_text = home.to_str().context("synthetic home path is not UTF-8")?;
  let thinking = json!({"type":"thinking","content":"private thought"});
  let result = json!({"type":"result","usage":{"input_tokens":1,"output_tokens":1}});
  let translation_error = json!({"type":"translate_error"});
  let first_script = format!(
    "cat <<'CB_EVENTS'\n{}\n{}\nCB_EVENTS\nwhile [ ! -e '{gate}' ] && [ -d '{home}' ]; do /bin/sleep 0.01; done\ncat <<'CB_FAILURE'\n{}\nCB_FAILURE\n",
    serde_json::to_string(&thinking)?,
    serde_json::to_string(&result)?,
    serde_json::to_string(&translation_error)?,
    gate = gate_text,
    home = home_text,
  );
  let assistant = json!({"type":"assistant","message":{"content":[{"type":"text","text":"next turn answer"}]}});
  let next_result = json!({"type":"result","subtype":"success"});
  let second_script = scripted_process(&[assistant, next_result], 0, "")?;
  let failing_option = backend_option_for("translation-error", "translation_error_script", first_script, None);
  let next_option = backend_option_for("next-turn", "translation_error_script", second_script, None);
  let (cfg, features) = config_with_features(&home, Some(failing_option.clone()), Arc::clone(&registry), true)?;
  let mut meta = session_meta("translation-error-session", "web", "fresh");
  meta.backend = failing_option.id.clone();
  save_session_metadata(&features, &registry, &meta)?;
  let queue = features.startup.require::<master_queue::MasterQueue>()?;
  let unread = Arc::new(Mutex::new(Vec::new()));
  let callback_guard = Arc::clone(&spawn_guard);
  let first_spawn: TaskSpawnCallback = Arc::new(move |pid, _start| {
    let spawn_guard = Arc::clone(&callback_guard);
    Box::pin(async move {
      spawn_guard.track(pid);
      Ok(())
    })
  });
  let first = tokio::spawn(run_turn_with_spawn(
    Arc::clone(&queue),
    Arc::clone(&cfg),
    meta.clone(),
    callbacks(Arc::clone(&features), Arc::clone(&unread)),
    "failing-input".to_string(),
    failing_option,
    first_spawn,
  ));
  tokio::time::timeout(std::time::Duration::from_secs(2), async {
    loop {
      if master_state::running_user_event_ids(&features, &meta.id)? == ["failing-input"] {
        break anyhow::Ok(());
      }
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
  })
  .await??;

  let second = tokio::spawn(run_turn(
    Arc::clone(&queue),
    Arc::clone(&cfg),
    meta.clone(),
    callbacks(Arc::clone(&features), Arc::clone(&unread)),
    "next-input".to_string(),
    next_option,
  ));
  let expected = std::collections::HashSet::from(["failing-input".to_string(), "next-input".to_string()]);
  tokio::time::timeout(std::time::Duration::from_secs(2), async {
    loop {
      if master_queue::queued_user_event_ids(&features, &meta.id)? == expected {
        break anyhow::Ok(());
      }
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
  })
  .await??;
  std::fs::write(gate, b"release")?;
  tokio::time::timeout(std::time::Duration::from_secs(5), first)
    .await
    .context("translator failure left the first turn blocked")???;
  tokio::time::timeout(std::time::Duration::from_secs(5), second)
    .await
    .context("the session consumer did not process the queued turn")???;
  queue.shutdown().await?;

  let events = features.store.load_chat_events_sync(&meta.id)?;
  let error_events = events
    .iter()
    .filter(|event| {
      event.get("type").and_then(charliebot_store::compact::CompactValue::as_str) == Some(event_types::ASSISTANT_ERROR)
    })
    .collect::<Vec<_>>();
  assert_eq!(error_events.len(), 1);
  assert!(error_events[0]["content"].as_str().unwrap().contains("synthetic translator failure"));
  assert_eq!(events.iter().filter(|event| event["type"] == "master_done").count(), 2);
  assert!(events.iter().any(|event| event["type"] == "thinking"));
  assert!(events.iter().any(|event| event["type"] == "result"));
  let event_values = events.iter().map(|event| event.to_value()).collect::<Vec<_>>();
  let assistant_text = event_values
    .iter()
    .filter_map(|event| event.pointer("/message/content/0/text").and_then(serde_json::Value::as_str))
    .collect::<Vec<_>>();
  assert_eq!(assistant_text, ["next turn answer"]);
  assert_eq!(unread.lock().expect("golden unread mutex is not poisoned").as_slice(), [meta.id.as_str()]);
  Ok(())
}

#[tokio::test]
async fn manager_persists_backend_session_anchor_from_init_event() -> anyhow::Result<()> {
  let temp = tempfile::tempdir()?;
  let registry = registry();
  let init = json!({"type":"system","subtype":"init","session_id":"native-synthetic"});
  let result = json!({
    "type":"result",
    "result":"",
    "usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},
    "total_cost_usd":0
  });
  let option = backend_option(scripted_process(&[init, result], 0, "")?);
  let home = temp.path().join("anchor-home");
  let (cfg, features) = config_with_features(&home, Some(option.clone()), Arc::clone(&registry), true)?;
  let meta = session_meta("anchor-session", "web", "fresh");
  save_session_metadata(&features, &registry, &meta)?;
  let queue = features.startup.require::<master_queue::MasterQueue>()?;
  let session_callbacks =
    queue.session_callbacks(Arc::clone(&features), Arc::new(|_session_id| Ok((false, "".to_string()))));
  run_turn(Arc::clone(&queue), cfg, meta.clone(), session_callbacks, "anchor-input".to_string(), option).await?;
  queue.shutdown().await?;
  let saved = features.store.get_session(&meta.id).await?.context("session metadata disappeared")?;
  assert_eq!(saved.cc_session_id.as_deref(), Some("native-synthetic"));
  Ok(())
}

#[tokio::test]
async fn manager_child_environment_uses_current_session_and_inherited_path() -> anyhow::Result<()> {
  let temp = tempfile::tempdir()?;
  let registry = registry();
  let result = json!({
    "type":"result",
    "result":"",
    "usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},
    "total_cost_usd":0
  });
  let mut script = r#"printf '{"type":"system","subtype":"environment_probe","session":"%s","path":"%s","ceiling":"%s","claudecode":"%s","auto_memory":"%s"}\n' "$CHARLIEBOT_SESSION_ID" "$PATH" "$GIT_CEILING_DIRECTORIES" "${CLAUDECODE:-unset}" "${CLAUDE_CODE_DISABLE_AUTO_MEMORY:-unset}"
cat <<'CB_RESULT'
"#
  .to_string();
  script.push_str(&serde_json::to_string(&result)?);
  script.push_str("\nCB_RESULT\n");
  let option = backend_option(script);
  let home = temp.path().join("environment-home");
  let (cfg, features) = config_with_features(&home, Some(option.clone()), Arc::clone(&registry), true)?;
  let meta = session_meta("environment-session", "web", "fresh");
  save_session_metadata(&features, &registry, &meta)?;
  let queue = features.startup.require::<master_queue::MasterQueue>()?;
  run_turn(
    Arc::clone(&queue),
    cfg,
    meta.clone(),
    callbacks(Arc::clone(&features), Arc::new(Mutex::new(Vec::new()))),
    "environment-input".to_string(),
    option,
  )
  .await?;
  queue.shutdown().await?;
  let events = features.store.load_chat_events_sync(&meta.id)?;
  let environment = events
    .iter()
    .find(|event| {
      event.get("subtype").and_then(charliebot_store::compact::CompactValue::as_str) == Some("environment_probe")
    })
    .context("scripted backend did not report its child environment")?;
  assert_eq!(
    environment.get("session").and_then(charliebot_store::compact::CompactValue::as_str),
    Some("environment-session")
  );
  assert_eq!(environment.get("path").and_then(charliebot_store::compact::CompactValue::as_str), Some("/usr/bin"));
  assert_eq!(
    environment.get("ceiling").and_then(charliebot_store::compact::CompactValue::as_str),
    Some(home.to_string_lossy().as_ref())
  );
  assert_eq!(environment.get("claudecode").and_then(charliebot_store::compact::CompactValue::as_str), Some("unset"));
  assert_eq!(environment.get("auto_memory").and_then(charliebot_store::compact::CompactValue::as_str), Some("1"));
  Ok(())
}

#[tokio::test]
async fn after_round_names_default_session_and_reuses_group_case() -> anyhow::Result<()> {
  let temp = tempfile::tempdir()?;
  let registry = registry();
  let events = [
    json!({"type":"assistant","message":{"content":[{"type":"text","text":"Synthetic planning summary."}]}}),
    json!({
      "type":"result",
      "result":"",
      "usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},
      "total_cost_usd":0
    }),
  ];
  let option = backend_option_with_response(
    scripted_process(&events, 0, "")?,
    Some(r#"{"name":"Synthetic planning","group":"engineering"}"#),
  );
  let home = temp.path().join("naming-home");
  let (cfg, features) = config_with_features(&home, Some(option.clone()), Arc::clone(&registry), true)?;
  let mut meta = session_meta("naming-session", "web", "fresh");
  meta.name = "Session 7".to_string();
  save_session_metadata(&features, &registry, &meta)?;
  let group_meta = SessionMetadata {
    group: Some("Engineering".to_string()),
    ..SessionMetadata::new("existing-group", "Grouped", TaskProfile::Manager)
  };
  save_session_metadata(&features, &registry, &group_meta)?;
  features
    .store
    .persist_and_broadcast(&meta.id, json!({"type":"user","content":"Plan the synthetic project."}))
    .await?;
  let queue = features.startup.require::<master_queue::MasterQueue>()?;
  let session_callbacks =
    queue.session_callbacks(Arc::clone(&features), Arc::new(|_session_id| Ok((false, "".to_string()))));
  run_turn(Arc::clone(&queue), Arc::clone(&cfg), meta.clone(), session_callbacks, "naming-input".to_string(), option)
    .await?;
  tokio::time::timeout(std::time::Duration::from_secs(2), async {
    loop {
      let current = features.store.get_session(&meta.id).await?.context("naming session disappeared")?;
      if current.name == "7: Synthetic planning" && current.group.as_deref() == Some("Engineering") {
        break anyhow::Ok(());
      }
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
  })
  .await??;
  queue.shutdown().await?;
  Ok(())
}

#[test]
fn autonamer_iterates_a_synthetic_pool_with_the_shared_backend_pool() -> anyhow::Result<()> {
  let first = backend_option_for("light-a", "codex", String::new(), None);
  let second = backend_option_for("light-b", "codex", String::new(), None);
  let mut cfg = config(Path::new("/synthetic/home"), Some(first));
  cfg.backends.options.push(second);
  cfg.backends.pool.push("light-b".to_string());
  let pool = Arc::new(BackendPool::default());
  let selected = autonamer::iter_light_backends(&cfg, pool)
    .collect::<anyhow::Result<Vec<_>>>()?
    .into_iter()
    .map(|option| option.id)
    .collect::<Vec<_>>();
  assert_eq!(selected, ["light-a", "light-b"]);
  Ok(())
}

#[tokio::test]
async fn shutdown_detaches_covered_turns_and_terminates_uncovered_turns() -> anyhow::Result<()> {
  // Python: tests/test_turn_contributions.py::test_a_let_go_turn_appends_no_master_done_and_runs_no_after_turn.
  let temp = tempfile::tempdir()?;
  let guard = Arc::new(ProcessGroupGuard::new());
  let registry = registry();
  for (index, backend_type, backend_id, expected_unread) in
    [(1, "covered_script", "covered-option", false), (2, "uncovered_script", "uncovered-option", true)]
  {
    let assistant = json!({"type":"assistant","message":{"content":[{"type":"text","text":"in-flight answer"}]}});
    let line = serde_json::to_string(&assistant)?;
    let script = format!("cat <<'CB_FIRST'\n{line}\nCB_FIRST\n/bin/sleep 1\n");
    let option = backend_option_for(backend_id, backend_type, script, None);
    let home = temp.path().join(format!("cancel-home-{index}"));
    let (cfg, features) = config_with_features(&home, Some(option.clone()), Arc::clone(&registry), true)?;
    let mut meta = session_meta(&format!("cancel-session-{index}"), "web", "fresh");
    meta.backend = backend_id.to_string();
    save_session_metadata(&features, &registry, &meta)?;
    let queue = features.startup.require::<master_queue::MasterQueue>()?;
    let unread = Arc::new(Mutex::new(Vec::new()));
    let callbacks = callbacks(Arc::clone(&features), Arc::clone(&unread));
    let run_id = format!("cancel-run-{index}");
    let transport_dir = home.join("runs").join(&run_id);
    std::fs::create_dir_all(&transport_dir)?;
    let (spawned_sender, spawned_receiver) = oneshot::channel();
    let spawned_sender = Arc::new(Mutex::new(Some(spawned_sender)));
    let spawn_guard = Arc::clone(&guard);
    let on_task_spawn: TaskSpawnCallback = Arc::new(move |pid, _start| {
      let sender = Arc::clone(&spawned_sender);
      let spawn_guard = Arc::clone(&spawn_guard);
      Box::pin(async move {
        spawn_guard.track(pid);
        let sender =
          sender.lock().expect("spawn signal mutex is not poisoned").take().context("spawn signal sent twice")?;
        sender.send(()).map_err(|_| anyhow::anyhow!("spawn signal receiver closed"))?;
        Ok(())
      })
    });
    let on_task_finish = noop_task_finish();
    let turn = tokio::spawn({
      let queue = Arc::clone(&queue);
      let cfg = Arc::clone(&cfg);
      let meta = meta.clone();
      async move {
        queue
          .run_message(MasterTurnRequest {
            cfg,
            session_meta: meta.clone(),
            user_content: "cancel this turn".to_string(),
            callbacks,
            user_event_ids: vec!["cancel-input".to_string()],
            task_instructions: "synthetic instructions".to_string(),
            task_run: TaskRunBinding { session_id: meta.id, run_id, transport_dir, fresh_native_context: false },
            on_task_spawn,
            on_task_finish,
            auto_trigger: false,
            backend_option: Some(option),
            extra_claude_flags: None,
            uploaded_files: None,
            extra_env: None,
          })
          .await
      }
    });
    tokio::time::timeout(std::time::Duration::from_secs(30), spawned_receiver)
      .await
      .context("scripted manager process did not spawn")?
      .context("manager process spawn signal was dropped")?;
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    queue.shutdown().await?;
    assert!(turn.await?.is_err(), "shutdown should cancel the waiting turn");
    let events = features.store.load_chat_events_sync(&meta.id)?;
    assert!(events.iter().all(|event| event.get("type").and_then(charliebot_store::compact::CompactValue::as_str) != Some("master_done")));
    assert_eq!(!unread.lock().expect("golden unread mutex is not poisoned").is_empty(), expected_unread);
    if !expected_unread {
      tokio::time::sleep(std::time::Duration::from_millis(1_050)).await;
    }
  }
  Ok(())
}

async fn assert_task_run_shutdown_shape(backend_type: &str, expected_unread: bool) -> anyhow::Result<()> {
  let temp = tempfile::tempdir()?;
  let groups = ProcessGroupGuard::new();
  let home = temp.path().join("home");
  std::fs::create_dir_all(&home)?;
  std::fs::write(home.join(CREDENTIALS_FILENAME), "charliebot:\n  access_key: synthetic-shutdown-access-key\n")?;
  let started = temp.path().join("agent-started");
  let release = temp.path().join("release-agent");
  let exited = temp.path().join("agent-exited");
  let event_lines = [
    json!({"type":"system","subtype":"init","session_id":"synthetic-shutdown-session"}),
    json!({"type":"assistant","message":{"content":[{"type":"text","text":"in-flight answer"}]}}),
  ]
  .iter()
  .map(serde_json::to_string)
  .collect::<Result<Vec<_>, _>>()?
  .join("\n");
  let result_line = serde_json::to_string(
    &json!({"type":"result","subtype":"success","result":"","usage":{"input_tokens":1,"output_tokens":1}}),
  )?;
  let script = format!(
    "touch '{}'\ncat <<'CB_EVENTS'\n{}\nCB_EVENTS\nwhile [ ! -e '{}' ]; do /bin/sleep 0.01; done\ncat <<'CB_RESULT'\n{}\nCB_RESULT\ntouch '{}'\n",
    started.display(),
    event_lines,
    release.display(),
    result_line,
    exited.display(),
  );
  let option = backend_option_for("fake", backend_type, script, None);
  let registry = registry();
  let (cfg, features) = config_with_features(&home, Some(option), Arc::clone(&registry), false)?;
  execution::pre_recovery_step(Arc::clone(&features)).await?;
  let meta = features
    .store
    .create_task(
      CreateTask {
        request_id: "synthetic-shutdown-task".to_string(),
        task_parent_id: None,
        profile: TaskProfile::Manager,
        task: None,
        name: Some("Synthetic shutdown task".to_string()),
        backend: Some("fake".to_string()),
        group: None,
        session_id: Some("synthetic-shutdown-task-session".to_string()),
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  let dispatcher = dispatch::dispatcher(&features)?;
  dispatcher.admit_input(&meta.id, event_types::USER, "in-flight input", "user", json!({})).await?;
  let run_id = "synthetic-shutdown-run";
  let guard = features.store.control_lock().await;
  features
    .store
    .register_run_locked(
      &guard,
      &meta.id,
      RunRecord {
        id: run_id.to_string(),
        session_id: meta.id.clone(),
        kind: RunKind::ManagerTurn,
        input_event_ids: Vec::new(),
        task_spec_hash: None,
        task_spec_ref: None,
        prompt_snapshot_ref: None,
        retry_of_run_id: None,
        review_of_run_id: None,
        backend: Some("fake".to_string()),
        model: Some("fake-model".to_string()),
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
      },
      None,
    )
    .await?;
  dispatcher.claim_input_batch_locked(&guard, &meta.id, run_id, None)?;
  drop(guard);

  let adapter = features.startup.require::<TaskExecutionAdapter>()?;
  let queue = features.startup.require::<master_queue::MasterQueue>()?;
  let task_session = meta.id.clone();
  let task = tokio::spawn(async move { adapter.execute_run(&task_session, run_id).await });
  // Record the group as soon as the launch callback persists the pid. A failure to reach the script's gate must still
  // leave the drop guard able to kill the process.
  let pid = tokio::time::timeout(Duration::from_secs(5), async {
    loop {
      if let Some(pid) = features.store.read_run_sync(&meta.id, run_id)?.and_then(|run| run.pid) {
        break anyhow::Ok(pid);
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .context("task manager agent's pid was not persisted")??;
  groups.track(pid);
  let started_result = tokio::time::timeout(Duration::from_secs(5), async {
    while !started.exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await;
  if started_result.is_err() {
    let _ = queue.shutdown().await;
    let _ = task.await;
    anyhow::bail!("task manager agent did not reach its shutdown gate");
  }
  tokio::time::timeout(Duration::from_secs(5), async {
    loop {
      if features.store.load_chat_events_sync(&meta.id)?.iter().any(|event| {
        event.get("type").and_then(charliebot_store::compact::CompactValue::as_str) == Some(event_types::ASSISTANT)
      }) {
        break anyhow::Ok(());
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .context("task manager agent's in-flight event was not persisted")??;
  let shutdown_result = queue.shutdown().await;
  let task_result = task.await?;
  let events = features.store.load_chat_events_sync(&meta.id)?;
  let facts = features.store.load_all_facts(&meta.id)?;
  let run = features.store.read_run_sync(&meta.id, run_id)?.context("shutdown run record disappeared")?;
  let host_boot = charliebot_agents::transport::identity::read_host_boot_time()?;
  let alive = charliebot_agents::runs::run_alive_probe(
    run.pid,
    run.pid_start.clone(),
    run.started_at.map(UtcDateTime::into_inner),
    host_boot,
  )();
  let has_unread = features.store.load_meta(&meta.id).await?.context("shutdown task metadata disappeared")?.has_unread;
  std::fs::write(&release, "continue")?;
  if alive {
    tokio::time::timeout(Duration::from_secs(5), async {
      while !exited.exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    })
    .await
    .context("detached task manager agent did not finish after the test released it")?;
  }
  shutdown_result?;
  let task_error = task_result.unwrap_err().to_string();
  assert!(task_error.contains("manager turn"), "{task_error}");
  assert_eq!(alive, !expected_unread, "the process state must match the covered/uncovered backend trait");
  assert_eq!(has_unread, expected_unread, "only an uncovered run marks the task unread");
  assert!(run.ended_at.is_none(), "shutdown must leave end metadata for startup recovery");
  assert!(run_records::terminal_outcome(&facts, run_id)?.is_none(), "shutdown must not write a terminal run fact");
  assert!(events.iter().all(|event| {
    !matches!(
      event.get("type").and_then(charliebot_store::compact::CompactValue::as_str),
      Some(event_types::MASTER_DONE | event_types::ASSISTANT_ERROR | event_types::RUN_FINISHED)
    )
  }));
  drop(cfg);
  Ok(())
}

#[tokio::test]
async fn execution_shutdown_cancellation_keeps_both_run_shapes_nonterminal() -> anyhow::Result<()> {
  // Python: tests/test_turn_contributions.py::test_a_let_go_turn_appends_no_master_done_and_runs_no_after_turn.
  // The execution adapter must also avoid turning an uncovered shutdown into a launch failure.
  assert_task_run_shutdown_shape("covered_script", false).await?;
  assert_task_run_shutdown_shape("uncovered_script", true).await?;
  Ok(())
}
