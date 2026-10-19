use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::backend_pool::BackendPool;
use charliebot_core::config::{
  BackendOption, BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
};
use charliebot_core::credentials::load_credentials;
use charliebot_core::event_types;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::Registry;
use charliebot_registry::backend::{
  AgentBackend, BackendFactory, BackendTraits, BackendTypeRegistration, Env, LaunchArgs, ResumeStyle, Translate,
  TransportKind,
};
use charliebot_registry::context::{FeatureContext, StartupState};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, UtcDateTime};
use charliebot_store::{
  AccountLabelKeeper, AnchorFieldReconciler, EmptyRuntimeEventHooks, SequenceArchiveHook, SequenceBindingMover,
  SequenceListingFields, Store, StoreHookFuture, StoreHooks, StoreResult,
};
use charliebot_tasks::autonamer;
use charliebot_tasks::master_queue::{
  MarkUnreadFn, PersistAccountLabelFn, PersistAndBroadcastFn, PersistNativeIdFn, SessionCallbacks,
};
use charliebot_tasks::master_state::{
  MasterResumeRecord, MasterResumeRequest, MasterTurnRequest, TaskFinishCallback, TaskRunBinding, TaskSpawnCallback,
};
use charliebot_tasks::{master_queue, master_state};
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use serde_yaml::{Mapping, Value as YamlValue};
use tempfile::TempDir;
use tokio::sync::oneshot;

const CASE_COUNT: usize = 12;

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
  ) -> BoxFuture<'a, anyhow::Result<String>> {
    let response = self.one_shot_response.clone();
    Box::pin(async move { response.context("scripted backend has no one-shot response") })
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
    paths: PathsConfig { workspace_dirs: Vec::new(), worktree_dir: "/synthetic/worktrees".to_string() },
    backends: BackendsConfig { pool, options },
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
      ("PATH".to_string(), "/usr/bin".to_string()),
      ("CLAUDECODE".to_string(), "stale-parent".to_string()),
      (charliebot_agents::launch_loop::SESSION_ID_ENV.to_string(), "stale-session".to_string()),
    ]),
  })?;
  startup.insert(BackendPool::default())?;
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
) -> anyhow::Result<(Vec<Value>, Vec<String>)> {
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
  let on_task_finish: TaskFinishCallback = Arc::new(|_native_id, _exit_code, _extras| Box::pin(async { Ok(()) }));
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
) -> anyhow::Result<(Vec<Value>, Vec<String>)> {
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
  let on_task_finish: TaskFinishCallback = Arc::new(|_native_id, _exit_code, _extras| Box::pin(async { Ok(()) }));
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
  Arc::new(|_native_id, _exit_code, _extras| Box::pin(async { Ok(()) }))
}

async fn run_turn(
  queue: Arc<master_queue::MasterQueue>,
  cfg: Arc<CharlieBotConfig>,
  meta: SessionMetadata,
  callbacks: SessionCallbacks,
  event_id: String,
  option: BackendOption,
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
      on_task_spawn: noop_task_spawn(),
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
    let actual = actual.iter().map(normalize_event).collect::<Vec<_>>();
    let expected = case.persisted.iter().map(normalize_event).collect::<Vec<_>>();
    assert_eq!(actual, expected, "persisted events differ for {}", case.name);
    assert_eq!(unread, case.mark_unread, "unread calls differ for {}", case.name);
  }
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
  let registry = registry();
  let home = temp.path().join("translate-error-home");
  let gate = home.join("release-translation-error");
  let gate_text = gate.to_str().context("synthetic gate path is not UTF-8")?;
  let thinking = json!({"type":"thinking","content":"private thought"});
  let result = json!({"type":"result","usage":{"input_tokens":1,"output_tokens":1}});
  let translation_error = json!({"type":"translate_error"});
  let first_script = format!(
    "cat <<'CB_EVENTS'\n{}\n{}\nCB_EVENTS\nwhile [ ! -e '{}' ]; do /bin/sleep 0.01; done\ncat <<'CB_FAILURE'\n{}\nCB_FAILURE\n",
    serde_json::to_string(&thinking)?,
    serde_json::to_string(&result)?,
    gate_text,
    serde_json::to_string(&translation_error)?,
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
  let first = tokio::spawn(run_turn(
    Arc::clone(&queue),
    Arc::clone(&cfg),
    meta.clone(),
    callbacks(Arc::clone(&features), Arc::clone(&unread)),
    "failing-input".to_string(),
    failing_option,
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
    .filter(|event| event.get("type").and_then(Value::as_str) == Some(event_types::ASSISTANT_ERROR))
    .collect::<Vec<_>>();
  assert_eq!(error_events.len(), 1);
  assert!(error_events[0]["content"].as_str().unwrap().contains("synthetic translator failure"));
  assert_eq!(events.iter().filter(|event| event["type"] == "master_done").count(), 2);
  assert!(events.iter().any(|event| event["type"] == "thinking"));
  assert!(events.iter().any(|event| event["type"] == "result"));
  let assistant_text = events
    .iter()
    .filter_map(|event| event.pointer("/message/content/0/text").and_then(Value::as_str))
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
    .find(|event| event.get("subtype").and_then(Value::as_str) == Some("environment_probe"))
    .context("scripted backend did not report its child environment")?;
  assert_eq!(environment.get("session").and_then(Value::as_str), Some("environment-session"));
  assert_eq!(environment.get("path").and_then(Value::as_str), Some("/usr/bin"));
  assert_eq!(environment.get("ceiling").and_then(Value::as_str), Some(home.to_string_lossy().as_ref()));
  assert_eq!(environment.get("claudecode").and_then(Value::as_str), Some("unset"));
  assert_eq!(environment.get("auto_memory").and_then(Value::as_str), Some("1"));
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
  let temp = tempfile::tempdir()?;
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
    let on_task_spawn: TaskSpawnCallback = Arc::new(move |_pid, _start| {
      let sender = Arc::clone(&spawned_sender);
      Box::pin(async move {
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
    tokio::time::timeout(std::time::Duration::from_secs(2), spawned_receiver)
      .await
      .context("scripted manager process did not spawn")?
      .context("manager process spawn signal was dropped")?;
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    queue.shutdown().await?;
    assert!(turn.await?.is_err(), "shutdown should cancel the waiting turn");
    let events = features.store.load_chat_events_sync(&meta.id)?;
    assert!(events.iter().all(|event| event.get("type").and_then(Value::as_str) != Some("master_done")));
    assert_eq!(!unread.lock().expect("golden unread mutex is not poisoned").is_empty(), expected_unread);
    if !expected_unread {
      tokio::time::sleep(std::time::Duration::from_millis(1_050)).await;
    }
  }
  Ok(())
}
