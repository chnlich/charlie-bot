#![allow(dead_code)]
//! Fixtures of the startup-recovery tests (`tests/recovery.rs`): the task-execution stack over a temporary home, the
//! scripted `/bin/sh` backend, a fault seam on the run-finish completion, and a capture of the node pass's log events.

#[path = "support/recovery_process_guard.rs"]
mod process_guard;

pub use charliebot_core::test_support::LogCapture;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::Context;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::backend_pool::BackendPool;
use charliebot_core::config::{
  BackendOption, BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
};
use charliebot_core::credentials::load_credentials;
use charliebot_core::event_types;
use charliebot_core::hot_reload::HotReload;
use charliebot_core::repo_root::RepoRoot;
use charliebot_registry::RegisterFn;
use charliebot_registry::Registry;
use charliebot_registry::RegistryError;
use charliebot_registry::backend::{
  AgentBackend, BackendFactory, BackendLifecycle, BackendTraits, BackendTypeRegistration, Env, LaunchArgs, ResumeStyle,
  Translate, TransportKind,
};
use charliebot_registry::context::{FeatureContext, StartupState};
use charliebot_registry::sequence::{SequenceBinding, SequenceCompletion, SequenceController, SequenceTree};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskSpec, UtcDateTime};
use charliebot_store::run_records;
use charliebot_store::run_records::{RunKind, RunOutcome, RunOutcomeKind, RunRecord};
use charliebot_store::{
  AccountLabelKeeper, AnchorFieldReconciler, CreateTask, EmptyRuntimeEventHooks, SequenceArchiveHook,
  SequenceBindingMover, SequenceListingFields, Store, StoreError, StoreHookFuture, StoreHooks, StoreResult, TaskCaller,
};
use charliebot_tasks::completion::TaskCompletion;
use charliebot_tasks::dispatch::{Dispatcher, RunFinishedCompletion};
use charliebot_tasks::execution::TaskExecutionAdapter;
use charliebot_tasks::master_queue::MasterQueue;
use charliebot_tasks::recovery::{self, PassCounters};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use serde_yaml::{Mapping, Value as YamlValue};
use tempfile::TempDir;
use tower::ServiceExt;

/// The backend type every synthetic option of these tests uses.
pub const BACKEND_TYPE: &str = "recovery_script";
/// The option the synthetic nodes are created with.
pub const BACKEND_ID: &str = "fake";

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

/// The option field of a fixed script (`script_option`).
const SCRIPT_FIELD: &str = "script";
/// The option field that names a test's script queue (`Scripts`).
pub const TOKEN_FIELD: &str = "token";

/// One test's scripted processes: the script each launched process takes, in order, and what each process was given.
#[derive(Default)]
struct ScriptRun {
  scripts: Mutex<VecDeque<String>>,
  builds: Mutex<Vec<Build>>,
}

static SCRIPT_RUNS: LazyLock<Mutex<BTreeMap<String, Arc<ScriptRun>>>> = LazyLock::new(Mutex::default);
static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(0);

/// One build of a scripted backend: the launch it was given, and the prompt its process was sent.
#[derive(Clone)]
pub struct Build {
  pub launch: LaunchArgs,
  pub prompt: Arc<Mutex<Option<String>>>,
}

impl Build {
  /// Whether the prompt its process was sent contains `text`.
  pub fn prompt_contains(&self, text: &str) -> bool {
    self.prompt.lock().expect("prompt mutex is not poisoned").as_deref().is_some_and(|prompt| prompt.contains(text))
  }
}

/// The scripts of one test: one script per launched process, taken in order by every option that names this queue's
/// token.
pub struct Scripts {
  token: String,
  run: Arc<ScriptRun>,
}

impl Scripts {
  pub fn new(scripts: Vec<String>) -> Self {
    let token = format!("recovery-test-{}", NEXT_TOKEN.fetch_add(1, Ordering::Relaxed));
    let run = Arc::new(ScriptRun { scripts: Mutex::new(scripts.into()), builds: Mutex::default() });
    SCRIPT_RUNS.lock().expect("script registry is not poisoned").insert(token.clone(), Arc::clone(&run));
    Self { token, run }
  }

  /// A backend option of `backend_type` whose builds take this queue's scripts.
  pub fn option(&self, id: &str, backend_type: &str) -> BackendOption {
    let mut type_fields = Mapping::new();
    type_fields.insert(YamlValue::String(TOKEN_FIELD.to_string()), YamlValue::String(self.token.clone()));
    BackendOption {
      id: id.to_string(),
      label: id.to_string(),
      prompt_overlay: None,
      model: format!("{id}-model"),
      backend_type: backend_type.to_string(),
      type_fields,
    }
  }

  /// The token a config file names this queue with.
  pub fn token(&self) -> &str {
    &self.token
  }

  pub fn builds(&self) -> Vec<Build> {
    self.run.builds.lock().expect("builds mutex is not poisoned").clone()
  }
}

/// Builds a scripted backend: from the option's fixed `script` field, or from the next script of the queue its token
/// names. A queue's process is recorded with its launch arguments when it launches.
pub struct ScriptFactory;

impl BackendFactory for ScriptFactory {
  fn build(
    &self,
    option: Option<&BackendOption>,
    _cfg: &CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let option = option.context("scripted backend requires an option")?;
    let prompt = Arc::new(Mutex::new(None));
    if let Some(script) = option.type_fields.get(YamlValue::String(SCRIPT_FIELD.to_string())) {
      let script = script.as_str().context("scripted option has a non-string script")?.to_string();
      return Ok(Box::new(ScriptBackend { script: Script::Fixed(script), launch, prompt }));
    }
    let token = option
      .type_fields
      .get(YamlValue::String(TOKEN_FIELD.to_string()))
      .and_then(YamlValue::as_str)
      .context("scripted option names neither a script nor a token")?;
    let run = SCRIPT_RUNS
      .lock()
      .expect("script registry is not poisoned")
      .get(token)
      .cloned()
      .with_context(|| format!("no scripts registered for token {token}"))?;
    Ok(Box::new(ScriptBackend { script: Script::Queue(run), launch, prompt }))
  }
}

/// Where a build's script comes from. A queue's next script is taken when the build launches a process; a build that
/// only translates output never launches one, as the worker's translator does.
enum Script {
  Fixed(String),
  Queue(Arc<ScriptRun>),
}

struct ScriptBackend {
  script: Script,
  launch: LaunchArgs,
  prompt: Arc<Mutex<Option<String>>>,
}

impl AgentBackend for ScriptBackend {
  fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
    None
  }

  fn prepare_transport(&mut self, _log_dir: &Path) -> anyhow::Result<()> {
    Ok(())
  }

  fn build_command(&self, prompt: &str) -> anyhow::Result<Vec<String>> {
    let script = match &self.script {
      Script::Fixed(script) => script.clone(),
      Script::Queue(run) => {
        let script = run.scripts.lock().expect("scripts mutex is not poisoned").pop_front();
        let script = script.context("the scripted backend ran out of scripts")?;
        run
          .builds
          .lock()
          .expect("builds mutex is not poisoned")
          .push(Build { launch: self.launch.clone(), prompt: Arc::clone(&self.prompt) });
        script
      }
    };
    *self.prompt.lock().expect("prompt mutex is not poisoned") = Some(prompt.to_string());
    Ok(vec!["/bin/sh".to_string(), "-c".to_string(), script])
  }

  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    Ok(env)
  }

  fn stdin_prompt(&self, _prompt: &str) -> Option<String> {
    None
  }

  fn translator(&self) -> Box<dyn Translate> {
    Box::new(IdentityTranslate)
  }

  fn transport(&self) -> TransportKind {
    TransportKind::RawLog
  }
}

struct IdentityTranslate;

impl Translate for IdentityTranslate {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
    Ok(vec![raw])
  }
}

fn register_scripted_backend(registry: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
  registry.backend_type(BackendTypeRegistration {
    type_name: BACKEND_TYPE,
    traits: BackendTraits {
      resume: ResumeStyle::CliFlag,
      restart_reattach: false,
      preassigned_session_id: false,
      family_prefix: None,
      manual_compact: false,
      compact_hint: None,
    },
    option_model: |_| Ok(()),
    factory: Arc::new(ScriptFactory),
    lifecycle: None,
    translate_fallback: false,
  })
}

/// The recovery stack's registry: the scripted backend type, the synthetic loop controller and the backend pool's
/// metadata slot (as the server registers it), plus the test's `extra` package when it has one.
fn registry(extra: Option<(&'static str, RegisterFn)>) -> anyhow::Result<Registry> {
  let mut registry = Registry::new();
  registry.register_package("test.recovery_backend", register_scripted_backend)?;
  registry.register_package("test.synthetic_loop", register_loop_controller)?;
  registry.register_package("test.synthetic_probe", register_probe_controller)?;
  registry.register_package("src.features.backend_pool", charliebot_agents::backend_pool::register)?;
  if let Some((package, register)) = extra {
    registry.register_package(package, register)?;
  }
  Ok(registry)
}

/// A backend type over the scripted factory, for a test's own type names (the Claude account type takes its lifecycle).
pub fn scripted_type(
  type_name: &'static str,
  traits: BackendTraits,
  lifecycle: Option<Arc<dyn BackendLifecycle>>,
) -> BackendTypeRegistration {
  BackendTypeRegistration {
    type_name,
    traits,
    option_model: |_| Ok(()),
    factory: Arc::new(ScriptFactory),
    lifecycle,
    translate_fallback: false,
  }
}

/// A script that prints one successful result event carrying `text`.
pub fn result_script(text: &str) -> String {
  format!("printf '%s\\n' '{}'", json!({"type": "result", "subtype": "success", "result": text}))
}

/// The synthetic option a node runs on: a `/bin/sh` script that prints nothing.
pub fn script_option(id: &str, script: &str) -> BackendOption {
  let mut type_fields = Mapping::new();
  type_fields.insert(YamlValue::String("script".to_string()), YamlValue::String(script.to_string()));
  BackendOption {
    id: id.to_string(),
    label: id.to_string(),
    prompt_overlay: None,
    model: format!("{id}-model"),
    backend_type: BACKEND_TYPE.to_string(),
    type_fields,
  }
}

fn config(home: &Path, options: Vec<BackendOption>, pool: Vec<String>, min_free_disk_gib: i64) -> CharlieBotConfig {
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
      min_free_disk_gib,
    },
    paths: PathsConfig {
      workspace_dirs: Vec::new(),
      worktree_dir: home.join("worktrees").display().to_string(),
      scratch_dir: home.join("scripts").display().to_string(),
    },
    backends: BackendsConfig { pool, routing: None, options },
    ui: UiConfig { home_services: Vec::new() },
    telegram: TelegramConfig { chat_id: None },
    sections: Sections::default(),
  }
}

/// The run-finish completion with a one-shot fault: the next successful finish's follow-up fails with the given
/// message, after the terminal fact and the run record have landed. Every other call delegates to the real completion.
pub struct FollowUpFault {
  inner: Arc<TaskCompletion>,
  remaining: AtomicUsize,
  message: Mutex<String>,
}

impl RunFinishedCompletion for FollowUpFault {
  fn after_run_finished<'a>(&'a self, session_id: &'a str, run_id: &'a str) -> BoxFuture<'a, StoreResult<()>> {
    Box::pin(async move {
      if self.remaining.load(Ordering::SeqCst) > 0 {
        self.remaining.fetch_sub(1, Ordering::SeqCst);
        return Err(StoreError::new(self.message.lock().expect("fault message is not poisoned").clone()));
      }
      RunFinishedCompletion::after_run_finished(self.inner.as_ref(), session_id, run_id).await
    })
  }
}

/// The task-execution stack a startup pass runs on: the store, the registry, the dispatcher with its adapter, and the
/// completion owner. The wiring matches `execution::pre_recovery_step`, with the run-finish completion wrapped.
pub struct Stack {
  /// Shared with a stack that boots on the same home (`Stack::start_on_home`).
  pub home: Arc<TempDir>,
  pub cfg: Arc<CharlieBotConfig>,
  /// The configuration the reload serves: the file holds no values, so a test changes the served copy through
  /// `set_min_free_disk_gib`.
  served: Arc<Mutex<CharlieBotConfig>>,
  pub features: Arc<FeatureContext>,
  pub adapter: TaskExecutionAdapter,
  pub dispatch: Arc<Dispatcher>,
  pub completion: Arc<TaskCompletion>,
  pub fault: Arc<FollowUpFault>,
  process_guard: process_guard::ProcessGroupGuard,
}

impl Drop for Stack {
  fn drop(&mut self) {
    let mut session_ids = HashSet::new();
    match self.store().list_all_run_records_sync() {
      Ok(records) => {
        for (session_id, run) in records {
          session_ids.insert(session_id);
          if let (Some(pid), Some(pid_start)) = (run.pid, run.pid_start.as_deref()) {
            self.process_guard.track_if_start(pid, pid_start);
          }
        }
      }
      Err(error) => eprintln!("test process cleanup could not read run records: {error:#}"),
    }
    track_processes_for_sessions(&self.process_guard, &self.cfg.charliebot_home, &session_ids);
  }
}

fn track_processes_for_sessions(guard: &process_guard::ProcessGroupGuard, home: &Path, session_ids: &HashSet<String>) {
  if session_ids.is_empty() {
    return;
  }
  let session_environment = session_ids
    .iter()
    .map(|session_id| format!("CHARLIEBOT_SESSION_ID={session_id}").into_bytes())
    .collect::<HashSet<_>>();
  let home_environment = format!("CHARLIEBOT_HOME={}", home.display()).into_bytes();
  let entries = match std::fs::read_dir("/proc") {
    Ok(entries) => entries,
    Err(error) => {
      let message = format!("test process cleanup could not read /proc: {error}");
      if std::thread::panicking() {
        eprintln!("{message}");
      } else {
        panic!("{message}");
      }
      return;
    }
  };
  for entry in entries {
    let entry = match entry {
      Ok(entry) => entry,
      Err(error) => {
        report_process_scan_error(format!("test process cleanup could not read a /proc entry: {error}"));
        return;
      }
    };
    let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else { continue };
    let Some(stat) = charliebot_core::proc_identity::read_pid_stat(pid) else { continue };
    let environment = match std::fs::read(entry.path().join("environ")) {
      Ok(environment) => environment,
      // Processes can exit during this scan, and other users' /proc entries are not readable.
      Err(error)
        if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied)
          || matches!(
            error.raw_os_error(),
            Some(libc::ENOENT) | Some(libc::ESRCH) | Some(libc::EACCES) | Some(libc::EPERM)
          ) =>
      {
        continue;
      }
      Err(error) => {
        report_process_scan_error(format!("test process cleanup could not read pid {pid} environment: {error}"));
        return;
      }
    };
    let variables = environment.split(|byte| *byte == 0);
    if variables.clone().any(|variable| variable == home_environment)
      && variables.into_iter().any(|variable| session_environment.contains(variable))
    {
      guard.track_if_start(pid, &stat.pid_start);
    }
  }
}

fn report_process_scan_error(message: String) {
  if std::thread::panicking() {
    eprintln!("{message}");
  } else {
    panic!("{message}");
  }
}

impl Stack {
  /// A stack whose only backend option is `fake`, with the given script.
  pub async fn start(script: &str) -> anyhow::Result<Stack> {
    Stack::start_with(vec![script_option(BACKEND_ID, script)], vec![BACKEND_ID.to_string()]).await
  }

  pub async fn start_with(options: Vec<BackendOption>, pool: Vec<String>) -> anyhow::Result<Stack> {
    Stack::start_with_min_free_disk_gib(options, pool, 0).await
  }

  /// A stack with scripted backend options and one additional package registration.
  pub async fn start_with_package(
    options: Vec<BackendOption>,
    pool: Vec<String>,
    extra: (&'static str, RegisterFn),
  ) -> anyhow::Result<Stack> {
    let home = tempfile::tempdir()?;
    let cfg = config(home.path(), options, pool, 0);
    Stack::assemble(Arc::new(home), cfg, registry(Some(extra))?).await
  }

  pub async fn start_with_min_free_disk_gib(
    options: Vec<BackendOption>,
    pool: Vec<String>,
    min_free_disk_gib: i64,
  ) -> anyhow::Result<Stack> {
    let home = tempfile::tempdir()?;
    let cfg = config(home.path(), options, pool, min_free_disk_gib);
    Stack::assemble(Arc::new(home), cfg, registry(None)?).await
  }

  /// A recovery stack with a registered cron controller, used to exercise the recovery dispatch hook.
  pub async fn start_with_cron_controller(script: &str) -> anyhow::Result<Stack> {
    let home = tempfile::tempdir()?;
    let options = vec![script_option(BACKEND_ID, script)];
    let pool = vec![BACKEND_ID.to_string()];
    let cfg = config(home.path(), options, pool, 0);
    let registry = registry(Some(("test.cron_recovery", register_cron_recovery)))?;
    Stack::assemble(Arc::new(home), cfg, registry).await
  }

  /// A stack over the `config.yaml` the test writes, loaded through a registry that also holds `extra`: the test's own
  /// backend types and config sections, such as the Claude account type and its accounts section.
  pub async fn start_from_config(config_text: &str, extra: (&'static str, RegisterFn)) -> anyhow::Result<Stack> {
    let home = tempfile::tempdir()?;
    std::fs::write(home.path().join("config.yaml"), config_text)?;
    let registry = registry(Some(extra))?;
    let cfg = registry.load_config(home.path())?;
    Stack::assemble(Arc::new(home), cfg, registry).await
  }

  /// A stack on the home of an earlier stack, as the next server boot finds it: the earlier stack's records and facts
  /// stay, and this stack's backend options replace the earlier ones.
  pub async fn start_on_home(
    home: Arc<TempDir>,
    options: Vec<BackendOption>,
    pool: Vec<String>,
  ) -> anyhow::Result<Stack> {
    let cfg = config(home.path(), options, pool, 0);
    Stack::assemble(home, cfg, registry(None)?).await
  }

  async fn assemble(home: Arc<TempDir>, cfg: CharlieBotConfig, registry: Registry) -> anyhow::Result<Stack> {
    let cfg = Arc::new(cfg);
    let registry = Arc::new(registry);
    let hooks = Arc::new(TestHooks);
    let store = Store::new(
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
    )?;
    let startup = Arc::new(StartupState::default());
    startup.insert(AgentBaseEnvironment {
      values: charliebot_registry::backend::Env::from([("PATH".to_string(), "/usr/bin".to_string())]),
    })?;
    startup.insert(BackendPool::default())?;
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
      .ancestors()
      .nth(3)
      .context("tasks crate manifest has no repository root ancestor")?;
    startup.insert(RepoRoot::new(Some(checkout.as_os_str().to_os_string()))?)?;
    let served = Arc::new(Mutex::new((*cfg).clone()));
    let config_source = Arc::clone(&served);
    let config_reload = HotReload::new(
      cfg.config_file(),
      "recovery_config_reload_failed",
      Box::new(move || Ok(config_source.lock().expect("served configuration is not poisoned").clone())),
    );
    let credential_home = cfg.charliebot_home.clone();
    let credentials_reload = HotReload::new(
      cfg.credentials_file(),
      "recovery_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    );
    let features = Arc::new(FeatureContext {
      config: Arc::new(config_reload),
      credentials: Arc::new(credentials_reload),
      store: Arc::new(store),
      registry,
      startup,
    });

    // The wiring of `execution::pre_recovery_step`, with the run-finish completion behind a fault seam.
    let pool = charliebot_agents::backend_pool::backend_pool(&features)?;
    features.startup.insert(MasterQueue::new(pool))?;
    let queue = features.startup.require::<MasterQueue>()?;
    features.startup.insert(TaskCompletion::new(Arc::clone(&features)))?;
    let completion = features.startup.require::<TaskCompletion>()?;
    let fault = Arc::new(FollowUpFault {
      inner: Arc::clone(&completion),
      remaining: AtomicUsize::new(0),
      message: Mutex::new(String::new()),
    });
    let sequence_completion: Arc<dyn SequenceCompletion> = completion.clone();
    let run_completion: Arc<dyn RunFinishedCompletion> = fault.clone();
    features.startup.insert(Dispatcher::new(
      Arc::clone(&features.store),
      Arc::clone(&features),
      sequence_completion,
      run_completion,
    ))?;
    let dispatch = features.startup.require::<Dispatcher>()?;
    let adapter = TaskExecutionAdapter::new(Arc::clone(&features), queue, Arc::clone(&dispatch));
    dispatch.set_executor(Some(Arc::new(adapter.clone())));
    features.startup.insert(adapter.clone())?;
    Ok(Stack {
      home,
      cfg,
      served,
      features,
      adapter,
      dispatch,
      completion,
      fault,
      process_guard: process_guard::ProcessGroupGuard::new(),
    })
  }

  /// Change the served disk-headroom threshold. The config file carries the change as well, so the reload check sees a
  /// new fingerprint and re-runs the loader, which serves the new value.
  pub fn set_min_free_disk_gib(&self, min_free_disk_gib: i64) -> anyhow::Result<()> {
    self.served.lock().expect("served configuration is not poisoned").server.min_free_disk_gib = min_free_disk_gib;
    std::fs::write(self.cfg.config_file(), format!("server:\n  min_free_disk_gib: {min_free_disk_gib}\n"))?;
    Ok(())
  }

  /// The operator's access key on disk: run-token signing reads it, so a stack that launches agent processes needs it.
  pub fn write_access_key(&self) -> anyhow::Result<()> {
    std::fs::write(self.cfg.credentials_file(), "charliebot:\n  access_key: synthetic-access-key\n")?;
    Ok(())
  }

  pub fn store(&self) -> &Store {
    &self.features.store
  }

  /// Change the hot-reload source config and move its fingerprint so the next runtime read sees it.
  pub fn replace_config(&self, config: CharlieBotConfig) -> anyhow::Result<()> {
    *self.served.lock().expect("served configuration is not poisoned") = config;
    std::fs::write(self.cfg.config_file(), "synthetic test config generation\n")?;
    Ok(())
  }
  /// Make the next `times` successful finishes fail their follow-up with `message`.
  pub fn fail_follow_up(&self, times: usize, message: &str) {
    *self.fault.message.lock().expect("fault message is not poisoned") = message.to_string();
    self.fault.remaining.store(times, Ordering::SeqCst);
  }

  /// One create through the store, under the operator's caller.
  pub async fn node(
    &self,
    request_id: &str,
    parent: Option<&str>,
    profile: TaskProfile,
    task: Option<TaskSpec>,
  ) -> anyhow::Result<SessionMetadata> {
    let meta = self
      .store()
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: parent.map(str::to_string),
          profile,
          task,
          name: Some(format!("Synthetic {request_id}")),
          backend: Some(BACKEND_ID.to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::Operator,
        Map::new(),
      )
      .await?;
    Ok(meta)
  }

  /// Persist a queued run on `session_id`, as the reservation of a launch does before any process exists.
  pub async fn queue_run(&self, session_id: &str, run_id: &str, kind: RunKind) -> anyhow::Result<RunRecord> {
    let record = RunRecord {
      backend: Some(BACKEND_ID.to_string()),
      model: Some(format!("{BACKEND_ID}-model")),
      ..synthetic_run(session_id, run_id, kind)
    };
    self.register(record).await
  }

  /// Persist a run record through the control lock, as a reservation does.
  /// One create through the store, under the operator's caller, with the task spec text recorded with it.
  pub async fn register_with_task_spec(&self, record: RunRecord, task_spec: &str) -> anyhow::Result<RunRecord> {
    let session_id = record.session_id.clone();
    let guard = self.store().control_lock().await;
    Ok(self.store().register_run_locked(&guard, &session_id, record, Some(task_spec)).await?)
  }
  pub async fn register(&self, record: RunRecord) -> anyhow::Result<RunRecord> {
    let session_id = record.session_id.clone();
    let guard = self.store().control_lock().await;
    Ok(self.store().register_run_locked(&guard, &session_id, record, None).await?)
  }

  /// Close a node the way the completion owner does: one durable `task_closed` fact.
  pub async fn close(&self, session_id: &str, request_id: &str) -> anyhow::Result<()> {
    let guard = self.store().control_lock().await;
    let event = charliebot_store::control_events::build_control_event(
      "task_closed",
      "agent",
      session_id,
      Some(&charliebot_store::control_events::stable_close_event_id(session_id, request_id)),
      None,
      [
        ("request_id".to_string(), json!(request_id)),
        ("outcome".to_string(), json!("completed")),
        ("summary".to_string(), json!("synthetic close")),
        ("result_refs".to_string(), json!([])),
        ("run_ids".to_string(), json!([])),
        ("report_to".to_string(), Value::Null),
      ],
    );
    self.store().append_control_fact(&guard, session_id, event).await?;
    Ok(())
  }

  /// Claim the node's pending inputs for a queued run, as the dispatcher does before a launch.
  pub async fn claim(&self, session_id: &str, run_id: &str) -> anyhow::Result<Vec<String>> {
    let guard = self.store().control_lock().await;
    Ok(self.dispatch.claim_input_batch_locked(&guard, session_id, run_id, None)?)
  }

  /// Record the launched process identity of a run, as the launch does once the process exists.
  pub async fn launch(&self, session_id: &str, run_id: &str, pid: u32, pid_start: &str) -> anyhow::Result<RunRecord> {
    Ok(self.store().record_launch(session_id, run_id, pid, pid_start).await?)
  }

  /// Persist a run's terminal fact through the dispatcher's finish path.
  pub async fn finish(&self, session_id: &str, run_id: &str, outcome: RunOutcomeKind) -> anyhow::Result<RunRecord> {
    Ok(self.dispatch.finish_run(session_id, run_id, RunOutcome::new(outcome)).await?)
  }

  /// A run's terminal fact as a crash leaves it: the store's finish, with no dispatcher follow-up behind it.
  pub async fn record_terminal(
    &self,
    session_id: &str,
    run_id: &str,
    outcome: RunOutcomeKind,
  ) -> anyhow::Result<RunRecord> {
    let guard = self.store().control_lock().await;
    Ok(self.store().record_finish_locked(&guard, session_id, run_id, RunOutcome::new(outcome)).await?)
  }

  /// The boot pass: every node of this home, through the startup step the server runs.
  pub async fn boot(&self) -> anyhow::Result<PassCounters> {
    recovery::reconcile_task_tree(&self.features, &self.adapter).await
  }

  /// One node's pass with no driver in this process, as the boot pass runs it.
  pub async fn node_pass(&self, session_id: &str) -> anyhow::Result<PassCounters> {
    let mut counters = PassCounters::default();
    recovery::reconcile_node(&self.features, &self.adapter, session_id, &mut counters, None).await?;
    Ok(counters)
  }

  pub fn run_events(&self, session_id: &str) -> anyhow::Result<Vec<Value>> {
    Ok(self.store().load_all_facts(session_id)?)
  }

  pub fn run_records(&self, session_id: &str) -> anyhow::Result<Vec<RunRecord>> {
    Ok(self.store().list_run_records_sync(session_id)?)
  }
}

/// A run record with only the identity fields set, as `process_guard`'s tests and the launch path build one.
pub struct SyntheticRepo {
  pub _dir: TempDir,
  pub repo: PathBuf,
  pub origin: PathBuf,
}

impl SyntheticRepo {
  /// Create a repo with one published seed commit and an up-to-date `origin/main` tracking ref.
  pub fn create() -> anyhow::Result<SyntheticRepo> {
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    let origin = dir.path().join("origin.git");
    run_git(
      dir.path(),
      &["init", "--bare", "--initial-branch=main", origin.to_str().context("origin path is UTF-8")?],
    )?;
    run_git(dir.path(), &["init", "--initial-branch=main", repo.to_str().context("repo path is UTF-8")?])?;
    run_git(&repo, &["config", "user.email", "synthetic@example.com"])?;
    run_git(&repo, &["config", "user.name", "Synthetic"])?;
    std::fs::write(repo.join("README.md"), "synthetic repository\n")?;
    run_git(&repo, &["add", "README.md"])?;
    run_git(&repo, &["commit", "-m", "synthetic seed"])?;
    run_git(&repo, &["remote", "add", "origin", origin.to_str().context("origin path is UTF-8")?])?;
    run_git(&repo, &["push", "-u", "origin", "main"])?;
    Ok(SyntheticRepo { _dir: dir, repo, origin })
  }

  /// Add a named branch as a worktree based at the published main tip.
  pub fn add_worktree(&self, path: &Path, branch: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent)?;
    }
    run_git(
      &self.repo,
      &["worktree", "add", "-b", branch, path.to_str().context("worktree path is UTF-8")?, "origin/main"],
    )?;
    Ok(())
  }

  /// Push the current worktree's branch tip to the published `main` ref.
  pub fn publish_worktree(&self, path: &Path) -> anyhow::Result<()> {
    run_git(path, &["push", "origin", "HEAD:main"])?;
    Ok(())
  }
}

fn run_git(cwd: &Path, args: &[&str]) -> anyhow::Result<()> {
  let output = Command::new("git").arg("-C").arg(cwd).args(args).output()?;
  anyhow::ensure!(
    output.status.success(),
    "git {args:?} failed in {}: {}",
    cwd.display(),
    String::from_utf8_lossy(&output.stderr)
  );
  Ok(())
}

pub fn synthetic_run(session_id: &str, run_id: &str, kind: RunKind) -> RunRecord {
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

/// A synthetic sitrep page in `dir`, the file a cold-read delegation names.
pub fn write_sitrep(dir: &Path) -> anyhow::Result<PathBuf> {
  let page = dir.join("sitrep.html");
  std::fs::write(&page, "<html><body><p>sitrep body</p></body></html>")?;
  Ok(page)
}

/// The cold-read delegation body over `page`: its genre and the trigger message the reader answers.
pub fn cold_read_body(session_id: &str, page: &Path) -> Value {
  json!({
    "session_id": session_id,
    "description": format!("cold read of {}", page.display()),
    "task_type": "cold-read",
    "cold_read": {"page_path": page.display().to_string(), "genre": "sitrep", "trigger": "where are we?"},
  })
}

/// The newest child report the manager has received.
pub fn last_child_report(stack: &Stack, manager_id: &str) -> anyhow::Result<Value> {
  stack.child_reports(manager_id)?.pop().context("the manager has received a child report")
}

/// A cold-read task spec over a page that the replay never reads: the named-error paths stop before the page.
pub fn cold_read_spec(page: &Path) -> TaskSpec {
  TaskSpec {
    goal: "synthetic cold read".to_string(),
    task_type: Some(charliebot_store::metadata::TaskType::ColdRead),
    cold_read: Some(charliebot_store::metadata::ColdReadSpec {
      page_path: page.display().to_string(),
      genre: "sitrep".to_string(),
      trigger: "where are we?".to_string(),
    }),
    ..TaskSpec::default()
  }
}

/// A worker task spec with a repository, as an implement node carries.
pub fn implement_spec(repo: &Path) -> TaskSpec {
  TaskSpec {
    goal: "synthetic implement".to_string(),
    repo_path: Some(repo.display().to_string()),
    base_branch: Some("main".to_string()),
    task_type: Some(charliebot_store::metadata::TaskType::Implement),
    ..TaskSpec::default()
  }
}

/// The error text the node pass returns for a replay step its owning task has not landed yet.
pub fn owner_not_landed_text(owner: &str) -> String {
  format!("owned by the {owner} task, which has not landed; the node pass cannot replay it")
}

/// Assert that the node pass refused a replay step with the named error of `owner`.
pub fn assert_named_error(error: &anyhow::Error, owner: &str) {
  let text = format!("{error:#}");
  assert!(text.contains(&owner_not_landed_text(owner)), "expected the {owner} named error, got: {text}");
}

/// Assert that the startup pass logged its node failure with the named error of `owner`.
pub fn assert_logged_node_failure(log: &LogCapture, owner: &str) {
  let logged = log.text();
  assert!(logged.contains("task_recovery_node_failed"), "the startup pass logged no node failure: {logged}");
  assert!(logged.contains(&owner_not_landed_text(owner)), "the logged node failure lacks the {owner} error: {logged}");
}

/// The sequence kind of the synthetic loop controller, and the name of its state marker in the child's session
/// directory.
pub const LOOP_KIND: &str = "synthetic-loop";
pub const LOOP_MARKER: &str = "synthetic-loop.json";

/// The loop's close step: the child closes with `outcome` and one report carrying `summary` and `result_refs`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoopClose {
  pub outcome: String,
  pub summary: String,
  pub result_refs: Vec<String>,
}

/// The loop's durable state. `status` is the loop's own state: a `running` loop is marked `interrupted` by the boot
/// pass. `close` is the close the loop is owed once none of its iterations is launched.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoopMarker {
  pub status: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub close: Option<LoopClose>,
}

/// The test sequence controller (`sequence_controllers.SequenceController`). It owns the marker file of each loop child
/// and nothing else, so the startup pass's sequence step and its run replays are the only code under test.
pub struct SyntheticLoop;

impl SyntheticLoop {
  /// The close step (`close_ended_loop_child`): a child that is open, has no close fact yet, and has no launched
  /// iteration in flight closes with its owed outcome. Queued iterations get a stop request first, so they never block
  /// the close.
  async fn close_if_due(child_id: &str, tree: &dyn SequenceTree) -> anyhow::Result<bool> {
    let Some(marker) = read_marker_at(tree.config(), child_id)? else {
      return Ok(false);
    };
    let Some(close) = marker.close else {
      return Ok(false);
    };
    if tree.task_state(child_id)? != "open" {
      return Ok(false);
    }
    let events = tree.load_events_sync(child_id)?;
    if events.iter().any(|event| event["type"] == event_types::TASK_CLOSED) {
      return Ok(false);
    }
    let runs = tree.list_run_records_sync(child_id)?;
    let terminal = |run_id: &str| tree.terminal_outcome(&events, run_id).is_some();
    if runs.iter().any(|run| run.pid.is_some() && !terminal(&run.id)) {
      return Ok(false);
    }
    for run in &runs {
      if run.pid.is_none()
        && !terminal(&run.id)
        && !run_records::stop_requested_in_shared_events(&events, &run.id, None)
      {
        tree.request_stop(child_id, &run.id, &format!("loop-close:{}", run.id)).await?;
      }
    }
    let evidence = json!({"summary": close.summary, "result_refs": close.result_refs, "run_ids": []});
    tree.completion().close_sequence_child(child_id, "synthetic-loop-close", &close.outcome, &evidence).await?;
    Ok(true)
  }
}

impl SequenceController for SyntheticLoop {
  fn sequence_kind(&self) -> &'static str {
    LOOP_KIND
  }

  fn redrive<'a>(&'a self, _session_id: &'a str, _tree: &'a dyn SequenceTree) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  fn launch_context<'a>(
    &'a self,
    _meta: &'a SessionMetadata,
    _run: &'a RunRecord,
    _launch_text: &'a str,
  ) -> BoxFuture<'a, anyhow::Result<Option<String>>> {
    Box::pin(async { Ok(None) })
  }

  fn after_run<'a>(
    &'a self,
    session_id: &'a str,
    _run: &'a RunRecord,
    tree: &'a dyn SequenceTree,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move { SyntheticLoop::close_if_due(session_id, tree).await.map(|_| ()) })
  }

  fn recover_run<'a>(
    &'a self,
    session_id: &'a str,
    _run: &'a RunRecord,
    outcome: Option<&'a str>,
    tree: &'a dyn SequenceTree,
  ) -> BoxFuture<'a, anyhow::Result<bool>> {
    Box::pin(async move {
      if outcome.is_none() {
        return Ok(false);
      }
      SyntheticLoop::close_if_due(session_id, tree).await
    })
  }

  fn reconcile_interrupted<'a>(&'a self, tree: &'a dyn SequenceTree) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let sessions = tree.config().sessions_dir();
      let mut children =
        std::fs::read_dir(&sessions)?.map(|entry| entry.map(|entry| entry.path())).collect::<io::Result<Vec<_>>>()?;
      children.sort();
      for child in children {
        let Some(child_id) = child.file_name().and_then(|name| name.to_str()).map(str::to_string) else {
          continue;
        };
        let Some(mut marker) = read_marker_at(tree.config(), &child_id)? else {
          continue;
        };
        if marker.status == "running" {
          marker.status = "interrupted".to_string();
          write_marker_at(tree.config(), &child_id, &marker)?;
        }
        SyntheticLoop::close_if_due(&child_id, tree).await?;
      }
      Ok(())
    })
  }

  fn binding(&self, _session_id: &str) -> Option<Arc<dyn SequenceBinding>> {
    None
  }

  fn owns_session(&self, _meta: &SessionMetadata) -> bool {
    false
  }

  fn listing_fields(&self, _session_ids: &[String], _now: UtcDateTime) -> HashMap<String, Map<String, Value>> {
    HashMap::new()
  }
}

fn register_loop_controller(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.sequence_controller(LOOP_KIND, Arc::new(SyntheticLoop))
}

/// The sequence kind of the probe controller, which stands in for the cron controller. It owns no state: each after-run
/// call appends one line to the session's probe log. Recovery answers as cron does (True for a terminal or unlaunched
/// run) and logs only the calls it answers; a launched, live run is left unlogged.
pub const PROBE_KIND: &str = "synthetic-probe";
const PROBE_LOG: &str = "synthetic-probe.calls";

pub struct SyntheticProbe;

impl SyntheticProbe {
  fn record(cfg: &CharlieBotConfig, session_id: &str, call: &str) -> anyhow::Result<()> {
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(probe_log_path(cfg, session_id))?;
    writeln!(log, "{call}")?;
    Ok(())
  }
}

impl SequenceController for SyntheticProbe {
  fn sequence_kind(&self) -> &'static str {
    PROBE_KIND
  }

  fn redrive<'a>(&'a self, _session_id: &'a str, _tree: &'a dyn SequenceTree) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  fn launch_context<'a>(
    &'a self,
    _meta: &'a SessionMetadata,
    _run: &'a RunRecord,
    _launch_text: &'a str,
  ) -> BoxFuture<'a, anyhow::Result<Option<String>>> {
    Box::pin(async { Ok(None) })
  }

  fn after_run<'a>(
    &'a self,
    session_id: &'a str,
    run: &'a RunRecord,
    tree: &'a dyn SequenceTree,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move { SyntheticProbe::record(tree.config(), session_id, &format!("after_run {}", run.id)) })
  }

  fn recover_run<'a>(
    &'a self,
    session_id: &'a str,
    run: &'a RunRecord,
    outcome: Option<&'a str>,
    tree: &'a dyn SequenceTree,
  ) -> BoxFuture<'a, anyhow::Result<bool>> {
    Box::pin(async move {
      if outcome.is_none() && run.pid.is_some() {
        return Ok(false);
      }
      let call = format!("recover_run {} {}", run.id, outcome.unwrap_or("none"));
      SyntheticProbe::record(tree.config(), session_id, &call)?;
      Ok(true)
    })
  }

  fn reconcile_interrupted<'a>(&'a self, _tree: &'a dyn SequenceTree) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  fn binding(&self, _session_id: &str) -> Option<Arc<dyn SequenceBinding>> {
    None
  }

  fn owns_session(&self, _meta: &SessionMetadata) -> bool {
    false
  }

  fn listing_fields(&self, _session_ids: &[String], _now: UtcDateTime) -> HashMap<String, Map<String, Value>> {
    HashMap::new()
  }
}

fn register_probe_controller(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.sequence_controller(PROBE_KIND, Arc::new(SyntheticProbe))
}

fn probe_log_path(cfg: &CharlieBotConfig, session_id: &str) -> PathBuf {
  cfg.sessions_dir().join(session_id).join(PROBE_LOG)
}

impl Stack {
  /// The probe controller's calls on one session, in call order; empty when it was never called.
  pub fn probe_calls(&self, session_id: &str) -> anyhow::Result<Vec<String>> {
    let path = probe_log_path(&self.cfg, session_id);
    if !path.is_file() {
      return Ok(Vec::new());
    }
    Ok(std::fs::read_to_string(&path)?.lines().map(str::to_string).collect())
  }
}

struct CronRecoveryProbe;

impl SequenceController for CronRecoveryProbe {
  fn sequence_kind(&self) -> &'static str {
    "cron"
  }

  fn redrive<'a>(&'a self, _session_id: &'a str, _tree: &'a dyn SequenceTree) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  fn launch_context<'a>(
    &'a self,
    _meta: &'a SessionMetadata,
    _run: &'a RunRecord,
    _launch_text: &'a str,
  ) -> BoxFuture<'a, anyhow::Result<Option<String>>> {
    Box::pin(async { Ok(None) })
  }

  fn after_run<'a>(
    &'a self,
    _session_id: &'a str,
    _run: &'a RunRecord,
    _tree: &'a dyn SequenceTree,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  fn recover_run<'a>(
    &'a self,
    _session_id: &'a str,
    run: &'a RunRecord,
    _outcome: Option<&'a str>,
    _tree: &'a dyn SequenceTree,
  ) -> BoxFuture<'a, anyhow::Result<bool>> {
    Box::pin(async move { Ok(run.sequence_ref.as_ref().is_some_and(|sequence| sequence.kind == "cron")) })
  }

  fn reconcile_interrupted<'a>(&'a self, _tree: &'a dyn SequenceTree) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  fn binding(&self, _session_id: &str) -> Option<Arc<dyn SequenceBinding>> {
    None
  }

  fn owns_session(&self, _meta: &SessionMetadata) -> bool {
    false
  }

  fn listing_fields(&self, _session_ids: &[String], _now: UtcDateTime) -> HashMap<String, Map<String, Value>> {
    HashMap::new()
  }
}

fn register_cron_recovery(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.sequence_controller("cron", Arc::new(CronRecoveryProbe))
}

fn marker_path(cfg: &CharlieBotConfig, session_id: &str) -> PathBuf {
  cfg.sessions_dir().join(session_id).join(LOOP_MARKER)
}

fn read_marker_at(cfg: &CharlieBotConfig, session_id: &str) -> anyhow::Result<Option<LoopMarker>> {
  let path = marker_path(cfg, session_id);
  if !path.is_file() {
    return Ok(None);
  }
  Ok(Some(serde_json::from_str(&std::fs::read_to_string(&path)?)?))
}

fn write_marker_at(cfg: &CharlieBotConfig, session_id: &str, marker: &LoopMarker) -> anyhow::Result<()> {
  std::fs::write(marker_path(cfg, session_id), serde_json::to_string(marker)?)?;
  Ok(())
}

impl Stack {
  /// Write a loop child's marker, as the loop's own state write leaves it on disk.
  pub fn write_marker(&self, session_id: &str, marker: &LoopMarker) -> anyhow::Result<()> {
    write_marker_at(&self.cfg, session_id, marker)
  }

  pub fn read_marker(&self, session_id: &str) -> anyhow::Result<LoopMarker> {
    read_marker_at(&self.cfg, session_id)?.context("the loop child has no marker")
  }
}

/// Poll until `condition` holds, failing after ten seconds.
pub async fn settle_until(mut condition: impl FnMut() -> anyhow::Result<bool>) -> anyhow::Result<()> {
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
  while !condition()? {
    anyhow::ensure!(std::time::Instant::now() < deadline, "the condition did not hold within ten seconds");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
  }
  Ok(())
}

impl Stack {
  /// Poll one run until its terminal fact and its end record both landed (the launch is fire-and-forget), then return
  /// the fact's outcome.
  pub async fn wait_terminal(&self, session_id: &str, run_id: &str) -> anyhow::Result<RunOutcomeKind> {
    settle_until(|| {
      let landed = self.store().read_run_sync(session_id, run_id)?.is_some_and(|run| run.ended_at.is_some());
      Ok(landed && run_records::terminal_outcome(&self.run_events(session_id)?, run_id)?.is_some())
    })
    .await?;
    Ok(run_records::terminal_outcome(&self.run_events(session_id)?, run_id)?.expect("the wait saw a terminal fact"))
  }

  /// The `child_report` facts a manager node holds, in order.
  pub fn child_reports(&self, manager_id: &str) -> anyhow::Result<Vec<Value>> {
    Ok(self.run_events(manager_id)?.into_iter().filter(|event| event["type"] == event_types::CHILD_REPORT).collect())
  }

  /// The `task_closed` facts of one node, in order.
  pub fn closes(&self, session_id: &str) -> anyhow::Result<Vec<Value>> {
    Ok(self.run_events(session_id)?.into_iter().filter(|event| event["type"] == event_types::TASK_CLOSED).collect())
  }

  /// The ids of the node's runs of one kind, in record order.
  pub fn run_ids_of_kind(&self, session_id: &str, kind: RunKind) -> anyhow::Result<Vec<String>> {
    Ok(self.run_records(session_id)?.into_iter().filter(|run| run.kind == kind).map(|run| run.id).collect())
  }
}

/// A synthetic repository and its bare `origin`, both on `main` with one seed commit (`init_repo_with_origin`).
pub fn init_repo_with_origin(dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
  let origin = dir.join("origin.git");
  let repo = dir.join("repo");
  synthetic_git(dir, &["init", "-q", "--bare", "-b", "main", &origin.display().to_string()])?;
  synthetic_git(dir, &["clone", "-q", &origin.display().to_string(), &repo.display().to_string()])?;
  synthetic_git(&repo, &["config", "user.email", "t@example.com"])?;
  synthetic_git(&repo, &["config", "user.name", "t"])?;
  std::fs::write(repo.join("seed.txt"), "seed\n")?;
  synthetic_git(&repo, &["add", "."])?;
  synthetic_git(&repo, &["commit", "-q", "-m", "seed"])?;
  synthetic_git(&repo, &["push", "-q", "origin", "main"])?;
  Ok((repo, origin))
}

/// Run one git command in `dir`, failing on a non-zero exit.
pub fn synthetic_git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
  let output = Command::new("git").arg("-C").arg(dir).args(args).output()?;
  anyhow::ensure!(
    output.status.success(),
    "git {args:?} failed in {}: {}",
    dir.display(),
    String::from_utf8_lossy(&output.stderr)
  );
  Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// One JSON request to the delegated-worker route, authenticated as the synthetic operator.
pub async fn post_delegate(router: &axum::Router, body: Value) -> anyhow::Result<(StatusCode, Value)> {
  post_delegate_as(router, body, "Bearer synthetic-access-key").await
}

/// One JSON request to the delegated-worker route under the given `Authorization` header: the operator key, or a run
/// token signed with it.
pub async fn post_delegate_as(
  router: &axum::Router,
  body: Value,
  authorization: &str,
) -> anyhow::Result<(StatusCode, Value)> {
  post_internal(router, "/api/internal/delegate", body, authorization).await
}

/// One JSON POST to the internal route `path` under the given `Authorization` header.
pub async fn post_internal(
  router: &axum::Router,
  path: &str,
  body: Value,
  authorization: &str,
) -> anyhow::Result<(StatusCode, Value)> {
  let response = router
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, authorization)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body)?))?,
    )
    .await?;
  let status = response.status();
  // A plain-text failure carries its kind and detail as an extension (`responses::internal_error`).
  let failure = response
    .extensions()
    .get::<charliebot_compat::responses::Failure>()
    .map(|failure| format!(" ({}: {})", failure.kind, failure.detail))
    .unwrap_or_default();
  let bytes = to_bytes(response.into_body(), usize::MAX).await?;
  let json = serde_json::from_slice(&bytes)
    .with_context(|| format!("{status}{failure}: {}", String::from_utf8_lossy(&bytes)))?;
  Ok((status, json))
}
