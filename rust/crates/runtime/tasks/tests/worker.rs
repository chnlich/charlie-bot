//! The task-run process manager (`src/runtime/worker.py`) over scripted `/bin/sh` backends. Each test names the Python
//! case it ports in a `// Python:` comment.
//!
//! A scripted backend runs its script as the process of a run, and each build records the launch it was given. A
//! registration is a plain function pointer, so the factory reaches its script queue through a token that rides in the
//! option's type fields. The Claude relay tests hand their account state to the `cc-claude` registration through a
//! thread-local, which the registration reads on the same thread.

#[allow(dead_code)]
mod process_guard;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::Context;
use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::cli_base::CliHost;
use charliebot_anthropic_compatible as anthropic_compatible;
use charliebot_charlie_code as charlie_code;
use charliebot_claude_code as claude_code;
use charliebot_claude_code::claude_accounts::{self, ClaudeAccountState};
use charliebot_claude_code::claude_config;
use charliebot_claude_code::claude_lifecycle::ClaudeCodeLifecycle;
use charliebot_claude_code::claude_metadata;
use charliebot_claude_code::claude_relay;
use charliebot_claude_code::event_types as claude_events;
use charliebot_claude_code::login_dirs;
use charliebot_codex as codex;
use charliebot_codex::CodexHost;
use charliebot_core::RepoRoot;
use charliebot_core::config::{BackendOption, CharlieBotConfig, ConfigSection};
use charliebot_core::credentials::load_credentials;
use charliebot_core::event_types as ET;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::Registry;
use charliebot_registry::RegistryError;
use charliebot_registry::backend::{
  AgentBackend, BackendFactory, BackendTraits, BackendTypeRegistration, Env, Launch, LaunchArgs, LaunchRefused,
  LaunchWatch, ResumeStyle, Translate, TransportKind,
};
use charliebot_registry::context::{FeatureContext, StartupState};
use charliebot_registry::store_hooks::store_hooks;
use charliebot_store::Store;
use charliebot_store::metadata::{SessionMetadata, ThreadMetadata, UtcDateTime};
use charliebot_store::run_records::RAW_LOG_NAME;
use charliebot_tasks::worker::{self, QuotaExhaustedError, SpawnedFn, Worker, WorkerSpec};
use futures_util::future::BoxFuture;
use process_guard::ProcessGroupGuard;
use serde_json::{Map, Value, json};
use serde_yaml::Value as YamlValue;
use time::{Duration as TimeDuration, OffsetDateTime};
use {charliebot_antigravity as antigravity, charliebot_gemini as gemini};

/// The Claude conversation every relay test resumes.
const CC_ID: &str = "11111111-2222-3333-4444-555555555555";
/// The field of a backend option that names its scripted harness.
const TOKEN_FIELD: &str = "token";
/// How long a script that holds its process open sleeps. A run ends that process before the sleep ends.
const HOLD_SECONDS: u32 = 30;

// ---------------------------------------------------------------------------
// Scripted backends
// ---------------------------------------------------------------------------

/// One build of a scripted backend: the launch it was given, and the prompt its process was sent.
#[derive(Clone)]
struct Build {
  args: LaunchArgs,
  prompt: Arc<Mutex<Option<String>>>,
}

/// The scripted processes of one test: the script each build takes, and what each build recorded.
#[derive(Default)]
struct ScriptRun {
  scripts: Mutex<VecDeque<String>>,
  builds: Mutex<Vec<Build>>,
}

static SCRIPT_RUNS: LazyLock<Mutex<BTreeMap<String, Arc<ScriptRun>>>> = LazyLock::new(Mutex::default);
static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(0);

/// The handle of one test's scripted processes. Its token rides on each scripted option.
struct Scripts {
  token: String,
  run: Arc<ScriptRun>,
}

impl Scripts {
  fn new(scripts: Vec<String>) -> Self {
    let token = format!("worker-test-{}", NEXT_TOKEN.fetch_add(1, Ordering::Relaxed));
    let run = Arc::new(ScriptRun { scripts: Mutex::new(scripts.into()), builds: Mutex::default() });
    SCRIPT_RUNS.lock().expect("script registry mutex is not poisoned").insert(token.clone(), Arc::clone(&run));
    Self { token, run }
  }

  fn builds(&self) -> Vec<Build> {
    self.run.builds.lock().expect("builds mutex is not poisoned").clone()
  }
}

/// Builds a scripted backend from the script queue its option's token names.
struct ScriptFactory;

impl BackendFactory for ScriptFactory {
  fn build(
    &self,
    option: Option<&BackendOption>,
    _cfg: &CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let option = option.context("scripted backend requires an option")?;
    let token = option
      .type_fields
      .get(YamlValue::String(TOKEN_FIELD.to_string()))
      .and_then(YamlValue::as_str)
      .context("scripted option has no token")?;
    let run = SCRIPT_RUNS
      .lock()
      .expect("script registry mutex is not poisoned")
      .get(token)
      .cloned()
      .with_context(|| format!("no scripts registered for token {token}"))?;
    let script = run.scripts.lock().expect("scripts mutex is not poisoned").pop_front();
    let script = script.context("the scripted backend ran out of scripts")?;
    let prompt = Arc::new(Mutex::new(None));
    run.builds.lock().expect("builds mutex is not poisoned").push(Build { args: launch, prompt: Arc::clone(&prompt) });
    Ok(Box::new(ScriptBackend { script, prompt }))
  }
}

struct ScriptBackend {
  script: String,
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
    *self.prompt.lock().expect("prompt mutex is not poisoned") = Some(prompt.to_string());
    Ok(vec!["/bin/sh".to_string(), "-c".to_string(), self.script.clone()])
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

/// The scripted events are already in the translated shape, so they pass through unchanged.
struct IdentityTranslate;

impl Translate for IdentityTranslate {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
    Ok(vec![raw])
  }
}

fn scripted_type(
  type_name: &'static str,
  lifecycle: Option<Arc<dyn charliebot_registry::backend::BackendLifecycle>>,
) -> BackendTypeRegistration {
  BackendTypeRegistration {
    type_name,
    traits: BackendTraits {
      resume: ResumeStyle::CliFlag,
      restart_reattach: true,
      preassigned_session_id: true,
      family_prefix: None,
      manual_compact: true,
      compact_hint: None,
    },
    option_model: |_| Ok(()),
    factory: Arc::new(ScriptFactory),
    lifecycle,
    translate_fallback: false,
  }
}

thread_local! {
  /// The Claude account state of the next `cc-claude` registration. The test sets it just before registering, on the
  /// thread that registers.
  static CLAUDE_STATE: RefCell<Option<Arc<ClaudeAccountState>>> = const { RefCell::new(None) };
}

fn register_scripted_types(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.backend_type(scripted_type("codex", None))?;
  registry.backend_type(scripted_type("cc-anthropic-compatible", None))?;
  registry.backend_type(scripted_type("worker_script", None))
}

fn register_scripted_claude(registry: &mut Registry) -> Result<(), RegistryError> {
  let state = CLAUDE_STATE
    .with(|cell| cell.borrow_mut().take())
    .expect("the test sets the Claude account state before registering");
  registry.config_section(ConfigSection { key: "accounts", legacy_keys: &[], parse: claude_config::parse_accounts })?;
  let lifecycle: Arc<dyn charliebot_registry::backend::BackendLifecycle> =
    Arc::new(ClaudeCodeLifecycle::with_state(state));
  registry.backend_type(scripted_type("cc-claude", Some(lifecycle)))
}

/// A registry of the scripted types, with `cc-claude` driving the given account state.
fn scripted_registry(state: &Arc<ClaudeAccountState>) -> anyhow::Result<Registry> {
  CLAUDE_STATE.with(|cell| *cell.borrow_mut() = Some(Arc::clone(state)));
  let mut registry = Registry::new();
  registry.register_package("test.worker_scripts", register_scripted_types)?;
  registry.register_package("test.worker_claude", register_scripted_claude)?;
  Ok(registry)
}

/// A `/bin/sh` script that prints each event as one JSON line and then runs `tail`.
fn script(events: &[Value], tail: &str) -> String {
  let mut commands = vec![":".to_string()];
  for event in events {
    let line = serde_json::to_string(event).expect("a JSON value serializes");
    assert!(!line.contains('\''), "a scripted event line must not contain a single quote");
    commands.push(format!("printf '%s\\n' '{line}'"));
  }
  commands.push(tail.to_string());
  commands.join("; ")
}

/// A script that prints its events and exits with `code`.
fn exits(events: &[Value], code: i32) -> String {
  script(events, &format!("exit {code}"))
}

/// A script that prints its events and then holds its process open, until the run terminates it.
fn holds(events: &[Value]) -> String {
  script(events, &format!("sleep {HOLD_SECONDS}"))
}

// ---------------------------------------------------------------------------
// Event fixtures (`tests/conftest.py`)
// ---------------------------------------------------------------------------

fn assistant_text(text: &str) -> Value {
  json!({"type": ET::ASSISTANT, "message": {"content": [{"type": "text", "text": text}]}})
}

fn user_tool_result() -> Value {
  json!({"type": ET::USER, "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}})
}

fn result() -> Value {
  json!({"type": ET::RESULT, "subtype": "success", "is_error": false, "result": "done"})
}

/// A rate-limit observation: the five-hour window at `utilization`, resetting three hours from now.
fn rate_limit_info(status: &str, utilization: f64) -> Value {
  let resets_at = (OffsetDateTime::now_utc() + TimeDuration::hours(3)).unix_timestamp();
  json!({
    "status": status,
    "rateLimitType": "five_hour",
    "resetsAt": resets_at,
    "unifiedWindows": {"five_hour": {"utilization": utilization}, "seven_day": {"utilization": 0.3}},
  })
}

fn rate_limit(status: &str, utilization: f64) -> Value {
  json!({"type": ET::RATE_LIMIT_EVENT, "rate_limit_info": rate_limit_info(status, utilization)})
}

/// A reading taken at `minutes` before the fixed instant 2026-09-06 20:00 UTC that the relay tests observe at
/// (`tests/test_worker_account_relay.py::NOW`).
fn reading_at(minutes_before: u32) -> UtcDateTime {
  let minutes = match minutes_before {
    0 => "00",
    5 => "55",
    30 => "30",
    _ => panic!("the relay tests read at 0, 5 and 30 minutes before the fixed instant"),
  };
  let hour = if minutes_before == 0 { "20" } else { "19" };
  UtcDateTime::parse(&format!("2026-09-06T{hour}:{minutes}:00+00:00")).expect("the fixed reading instant parses")
}

// ---------------------------------------------------------------------------
// Directories and configuration
// ---------------------------------------------------------------------------

/// One test's directories: the charliebot home, the run's working and data directories, and the Claude config dirs.
struct Root {
  dir: tempfile::TempDir,
}

impl Root {
  fn new() -> anyhow::Result<Self> {
    let root = Self { dir: tempfile::tempdir()? };
    fs::create_dir_all(root.work())?;
    Ok(root)
  }

  fn path(&self) -> &Path {
    self.dir.path()
  }

  fn home(&self) -> PathBuf {
    self.path().join("home")
  }

  fn work(&self) -> PathBuf {
    self.path().join("work")
  }

  fn events_log(&self) -> PathBuf {
    self.path().join("data").join("events.jsonl")
  }

  fn claude(&self, label: &str) -> PathBuf {
    self.path().join(format!("claude-{label}"))
  }
}

/// Writes the Claude OAuth credentials that mark a pool account's login present.
fn plant_credentials(config_dir: &Path) -> anyhow::Result<()> {
  fs::create_dir_all(config_dir)?;
  let credentials = json!({"claudeAiOauth": {"accessToken": "token", "refreshToken": "r"}});
  fs::write(config_dir.join(login_dirs::CREDENTIALS_FILE), credentials.to_string())?;
  Ok(())
}

/// A session transcript of `session_id` under the config dir, as a Claude Code login writes it.
fn make_transcript(config_dir: &Path, session_id: &str) -> anyhow::Result<PathBuf> {
  let transcript = config_dir.join("projects").join("slug").join(format!("{session_id}.jsonl"));
  fs::create_dir_all(transcript.parent().context("a transcript has a directory")?)?;
  fs::write(&transcript, "[]")?;
  Ok(transcript)
}

/// One `backends.options` entry of a test's configuration. A scripted option names its harness by `token`.
fn option_yaml(id: &str, backend_type: &str, token: Option<&str>, account_pool: Option<&str>) -> String {
  let token = token.map(|token| format!("      {TOKEN_FIELD}: {token}\n")).unwrap_or_default();
  let pool = account_pool.map(|pool| format!("      account_pool: {pool}\n")).unwrap_or_default();
  let endpoint = if backend_type == "cc-anthropic-compatible" {
    "      api_base: https://api.synthetic.example\n      context_window: 409600\n"
  } else {
    ""
  };
  let fields = format!("{token}{pool}{endpoint}");
  format!("    - id: {id}\n      label: {id}\n      type: {backend_type}\n      model: test-model\n{fields}")
}

/// The `config.yaml` of a test: the Claude accounts under the root with their credentials, the pools, and the options.
/// A test with no Claude account leaves the accounts section out, as its registry does not register it.
fn config_text(root: &Root, labels: &[&str], pools: &[(&str, &[&str])], options: &[String]) -> anyhow::Result<String> {
  // The sessions here have no metadata; a zero cap starts no session cgroup, so no spawn walks the task-parent chain.
  let mut text = format!(
    "paths:\n  worktree_dir: {}\nserver:\n  session_memory_max_mb: 0\n",
    root.path().join("worktrees").display()
  );
  if !labels.is_empty() {
    text.push_str("accounts:\n  claude:\n");
    for label in labels {
      plant_credentials(&root.claude(label))?;
      text.push_str(&format!("    - label: {label}\n      config_dir: {}\n", root.claude(label).display()));
    }
    if !pools.is_empty() {
      text.push_str("  claude_pools:\n");
      for (name, members) in pools {
        text.push_str(&format!("    {name}: [{}]\n", members.join(", ")));
      }
    }
  }
  if options.is_empty() {
    text.push_str("backends:\n  options: []\n");
  } else {
    text.push_str("backends:\n  options:\n");
  }
  for option in options {
    text.push_str(option);
  }
  Ok(text)
}

/// The option id of the pooled Fable backend, which draws from the first account pool when pools are given.
const POOLED: &str = "claude-fable-5";

/// The pooled relay option's configuration over `labels` and `pools`, with its scripted token.
fn pooled_config(root: &Root, labels: &[&str], pools: &[(&str, &[&str])], token: &str) -> anyhow::Result<String> {
  let pool = pools.first().map(|(name, _)| *name);
  config_text(root, labels, pools, &[option_yaml(POOLED, "cc-claude", Some(token), pool)])
}

// ---------------------------------------------------------------------------
// Feature context and worker construction
// ---------------------------------------------------------------------------

/// Watches the process of a test's run and records the events it saw, as the lifecycle's relay watch does.
#[derive(Clone, Default)]
struct RecordingWatch {
  seen: Arc<Mutex<Vec<Value>>>,
}

impl LaunchWatch for RecordingWatch {
  fn observe(&mut self, event: &Value) -> bool {
    self.seen.lock().expect("seen mutex is not poisoned").push(event.clone());
    false
  }

  fn wants_next(&self, _exit_code: i32, _stderr: &str) -> bool {
    false
  }
}

/// A test's world: the feature context over the root's charliebot home, the configuration its `config.yaml` loads to,
/// and the guard that kills the processes the test's runs start.
struct Setup {
  features: Arc<FeatureContext>,
  cfg: CharlieBotConfig,
  guard: Arc<ProcessGroupGuard>,
}

impl Setup {
  fn new(root: &Root, registry: Registry, config: &str) -> anyhow::Result<Self> {
    let home = root.home();
    fs::create_dir_all(&home)?;
    fs::write(home.join("config.yaml"), config)?;
    let cfg = registry.load_config(&home)?;
    let registry = Arc::new(registry);
    let store = Store::new(cfg.clone(), registry.metadata_slots(), store_hooks(Arc::clone(&registry)))?;
    let startup = Arc::new(StartupState::default());
    startup.insert(AgentBaseEnvironment { values: Env::from([("PATH".to_string(), "/usr/bin".to_string())]) })?;
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
      .ancestors()
      .nth(4)
      .context("tasks crate manifest has no repository root ancestor")?;
    startup.insert(RepoRoot::new(Some(checkout.as_os_str().to_os_string()))?)?;
    let config_value = Arc::new(cfg.clone());
    let config_reload =
      HotReload::new(cfg.config_file(), "worker_config_reload_failed", Box::new(move || Ok((*config_value).clone())));
    let credential_home = cfg.charliebot_home.clone();
    let credentials_reload = HotReload::new(
      cfg.credentials_file(),
      "worker_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    );
    let features = Arc::new(FeatureContext {
      config: Arc::new(config_reload),
      credentials: Arc::new(credentials_reload),
      store: Arc::new(store),
      registry,
      startup,
    });
    Ok(Self { features, cfg, guard: Arc::new(ProcessGroupGuard::new()) })
  }

  /// The spec of a task run on `option_id`, with its thread's Claude conversation when one is given.
  fn spec(&self, root: &Root, option_id: Option<&str>, claude_session: Option<&str>) -> anyhow::Result<WorkerSpec> {
    let backend_option = option_id.map(|id| self.option(id)).transpose()?;
    let mut thread: ThreadMetadata =
      serde_json::from_value(json!({"id": "t1", "session_id": "s1", "description": "task"}))?;
    if let Some(session) = claude_session {
      claude_metadata::set_session_id(&mut thread, Some(session));
    }
    let session_meta = option_id
      .map(|id| {
        serde_json::from_value::<SessionMetadata>(json!({"id": "s1", "name": "S", "profile": "manager", "backend": id}))
      })
      .transpose()?;
    Ok(WorkerSpec {
      thread,
      working_dir: root.work(),
      events_log: root.events_log(),
      task_description: "do the thing".to_string(),
      cfg: Arc::new(self.cfg.clone()),
      backend_option,
      extra_env: Env::new(),
      on_spawned: Some(track_spawns(&self.guard)),
      instructions_content: None,
      session_meta,
      held_native_id: None,
      run_kind: charliebot_store::run_records::RunKind::Work,
    })
  }

  fn option(&self, id: &str) -> anyhow::Result<BackendOption> {
    self
      .cfg
      .backends
      .options
      .iter()
      .find(|option| option.id == id)
      .cloned()
      .with_context(|| format!("the configuration has no backend option {id}"))
  }

  fn worker(&self, spec: WorkerSpec) -> anyhow::Result<Worker> {
    Worker::new(Arc::clone(&self.features), spec)
  }
}

/// A spawn hook that tracks each process's group, so the guard kills whatever a run leaves behind.
fn track_spawns(guard: &Arc<ProcessGroupGuard>) -> SpawnedFn {
  let guard = Arc::clone(guard);
  Arc::new(move |thread: ThreadMetadata| {
    let guard = Arc::clone(&guard);
    Box::pin(async move {
      let pid = thread.pid.context("a spawned run records its pid")?;
      guard.track(pid);
      Ok(())
    }) as BoxFuture<'static, anyhow::Result<()>>
  })
}

fn read_events(path: &Path) -> anyhow::Result<Vec<Value>> {
  let text = fs::read_to_string(path)?;
  text.lines().filter(|line| !line.trim().is_empty()).map(|line| Ok(serde_json::from_str(line)?)).collect()
}

fn account_label(build: &Build) -> Option<&str> {
  build.args.backend_kwargs.get("claude_account")?.get("label")?.as_str()
}

fn account_dir(build: &Build) -> Option<&str> {
  build.args.backend_kwargs.get("claude_account")?.get("config_dir")?.as_str()
}

fn refusal(error: anyhow::Error) -> LaunchRefused {
  error.downcast::<LaunchRefused>().expect("the run ends in a launch refusal")
}

// ---------------------------------------------------------------------------
// The Worker's relay loop (`tests/test_worker_account_relay.py`)
// ---------------------------------------------------------------------------

// Python: tests/test_worker_account_relay.py::test_worker_relays_a_rejected_run_onto_another_account
#[tokio::test]
async fn test_worker_relays_a_rejected_run_onto_another_account() -> anyhow::Result<()> {
  let root = Root::new()?;
  let source = make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts = Scripts::new(vec![
    exits(&[assistant_text("working"), rate_limit("rejected", 1.0)], 1),
    exits(&[assistant_text("done"), result()], 0),
  ]);
  let config = pooled_config(&root, &["main", "ext-1", "ext-2"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  assert_eq!(task_worker.run().await?, 0);

  let builds = scripts.builds();
  assert_eq!(account_dir(&builds[1]), Some(root.claude("ext-1").display().to_string().as_str()));
  assert_eq!(builds[1].args.extra_flags, ["--resume", CC_ID]);
  assert!(builds[1].args.backend_kwargs.get("claude_session_id").is_none());
  assert_eq!(builds[0].args.backend_kwargs.get("claude_session_id"), Some(&json!(CC_ID)));
  assert_eq!(
    builds[1].prompt.lock().expect("prompt mutex is not poisoned").clone(),
    Some(claude_relay::CONTINUATION_PROMPT.to_string())
  );
  let relayed = root.claude("ext-1").join("projects").join("slug").join(format!("{CC_ID}.jsonl"));
  assert!(relayed.exists(), "the transcript is copied to the relay account from {}", source.display());
  assert_eq!((account_label(&builds[1]), task_worker.account_relays()), (Some("ext-1"), 1));
  let logged = read_events(&root.events_log())?;
  assert_eq!(logged.iter().filter(|event| event["type"] == ET::ASSISTANT).count(), 2);
  assert!(logged.iter().any(|event| event["type"] == ET::RATE_LIMIT_EVENT));
  Ok(())
}

// Python: tests/test_worker_account_relay.py::test_worker_raises_pool_exhausted_when_no_account_is_left
#[tokio::test]
async fn test_worker_raises_pool_exhausted_when_no_account_is_left() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  claude_accounts::observe_rate_limit(&state, "ext-1", &rate_limit_info("rejected", 1.0), None);
  let scripts = Scripts::new(vec![exits(&[rate_limit("rejected", 1.0)], 1)]);
  let config = pooled_config(&root, &["main", "ext-1"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  let error = refusal(task_worker.run().await.expect_err("the run is refused"));

  assert!(error.message.contains("earliest reset"), "{}", error.message);
  assert!(error.quota_exhausted);
  Ok(())
}

// Python: tests/test_worker_account_relay.py::test_worker_relay_stays_inside_the_option_pool
#[tokio::test]
async fn test_worker_relay_stays_inside_the_option_pool() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  claude_accounts::observe_rate_limit(&state, "ext-1", &rate_limit_info("allowed", 0.50), None);
  let scripts =
    Scripts::new(vec![exits(&[rate_limit("rejected", 1.0)], 1), exits(&[assistant_text("done"), result()], 0)]);
  let pools: &[(&str, &[&str])] = &[("alpha", &["main", "ext-1"]), ("beta", &["ext-2"])];
  let config = pooled_config(&root, &["main", "ext-1", "ext-2"], pools, &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  assert_eq!(task_worker.run().await?, 0);

  let builds = scripts.builds();
  assert_eq!(account_dir(&builds[1]), Some(root.claude("ext-1").display().to_string().as_str()));
  assert!(root.claude("ext-1").join("projects").join("slug").join(format!("{CC_ID}.jsonl")).exists());
  assert!(claude_accounts::transcript_path(&root.claude("ext-2"), CC_ID)?.is_none());
  Ok(())
}

// Python: tests/test_worker_account_relay.py::test_worker_pool_exhaustion_names_the_option_pool
#[tokio::test]
async fn test_worker_pool_exhaustion_names_the_option_pool() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts = Scripts::new(vec![exits(&[rate_limit("rejected", 1.0)], 1)]);
  let pools: &[(&str, &[&str])] = &[("alpha", &["main"]), ("beta", &["ext-1", "ext-2"])];
  let config = pooled_config(&root, &["main", "ext-1", "ext-2"], pools, &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  let error = refusal(task_worker.run().await.expect_err("the run is refused"));

  assert!(error.message.contains(claude_relay::POOL_EXHAUSTED_PHRASE), "{}", error.message);
  assert!(
    error.message.contains("'alpha'") && error.message.contains("earliest reset") && error.message.contains("UTC")
  );
  assert!(claude_accounts::transcript_path(&root.claude("ext-1"), CC_ID)?.is_none());
  assert!(claude_accounts::transcript_path(&root.claude("ext-2"), CC_ID)?.is_none());
  Ok(())
}

// Python: tests/test_worker_account_relay.py::test_worker_stops_after_the_relay_limit
#[tokio::test]
async fn test_worker_stops_after_the_relay_limit() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts = Scripts::new((0..4).map(|_| exits(&[rate_limit("rejected", 1.0)], 1)).collect());
  let config = pooled_config(&root, &["main", "a", "b", "c"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  let error = refusal(task_worker.run().await.expect_err("the run is refused"));

  assert!(error.message.contains("relay limit"), "{}", error.message);
  assert!(!error.quota_exhausted);
  assert_eq!(scripts.builds().len(), 1 + claude_relay::MAX_RELAYS_PER_TURN);
  assert_eq!(task_worker.account_relays(), claude_relay::MAX_RELAYS_PER_TURN);
  Ok(())
}

// Python: tests/test_worker_account_relay.py::test_worker_stays_armed_without_a_below_line_account_and_completes
#[tokio::test]
async fn test_worker_stays_armed_without_a_below_line_account_and_completes() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts = Scripts::new(vec![exits(
    &[
      assistant_text("working"),
      rate_limit("allowed_warning", 0.92),
      user_tool_result(),
      assistant_text("still working"),
      user_tool_result(),
      assistant_text("done"),
      result(),
    ],
    0,
  )]);
  let config = pooled_config(&root, &["main"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  assert_eq!(task_worker.run().await?, 0);

  // The run's final result is persisted, so the process was never stopped at a safe point.
  assert!(read_events(&root.events_log())?.iter().any(|event| event["type"] == ET::RESULT));
  assert_eq!(scripts.builds().len(), 1);
  assert_eq!(task_worker.account_relays(), 0);
  Ok(())
}

// Python: tests/test_worker_account_relay.py::test_worker_warning_relay_lands_strictly_below_the_line_in_the_tie_band
#[tokio::test]
async fn test_worker_warning_relay_lands_strictly_below_the_line_in_the_tie_band() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  // ext-1 sits below the line (headroom 0.11); ext-2 sits at the line (headroom 0.095) with the older reading.
  claude_accounts::observe_rate_limit(&state, "ext-1", &rate_limit_info("allowed", 0.89), Some(reading_at(5)));
  claude_accounts::observe_rate_limit(&state, "ext-2", &rate_limit_info("allowed", 0.905), Some(reading_at(30)));
  let scripts =
    Scripts::new(vec![holds(&[rate_limit("allowed_warning", 0.92), user_tool_result()]), exits(&[result()], 0)]);
  let config = pooled_config(&root, &["main", "ext-1", "ext-2"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  assert_eq!(task_worker.run().await?, 0);

  let builds = scripts.builds();
  assert_eq!((account_label(&builds[1]), task_worker.account_relays()), (Some("ext-1"), 1));
  Ok(())
}

// Python: tests/test_worker_account_relay.py::test_worker_falls_back_to_the_regular_select_when_the_below_line_target_vanishes
//
// The Python case raises the target's reading between the gate and the move by stubbing `select`. Here a subscriber on
// the thread channel raises it when the safe point's `user` frame is broadcast: the watch has already observed that
// event (the gate), and the move reads the pool only after the process has exited.
#[tokio::test]
async fn test_worker_falls_back_to_the_regular_select_when_the_below_line_target_vanishes() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  // ext-1 is the gate's below-line destination; ext-2 already sits at the line.
  claude_accounts::observe_rate_limit(&state, "ext-1", &rate_limit_info("allowed", 0.85), Some(reading_at(0)));
  claude_accounts::observe_rate_limit(&state, "ext-2", &rate_limit_info("allowed", 0.92), Some(reading_at(0)));
  let scripts =
    Scripts::new(vec![holds(&[rate_limit("allowed_warning", 0.92), user_tool_result()]), exits(&[result()], 0)]);
  let config = pooled_config(&root, &["main", "ext-1", "ext-2"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let mut frames = setup.features.store.subscribe("t1");
  let raised_state = Arc::clone(&state);
  let raise = tokio::spawn(async move {
    while let Some(frame) = frames.recv().await {
      let frame: Value = serde_json::from_str(&frame)?;
      if frame["type"] == ET::USER {
        claude_accounts::observe_rate_limit(&raised_state, "ext-1", &rate_limit_info("allowed_warning", 0.95), None);
        return anyhow::Ok(true);
      }
    }
    anyhow::Ok(false)
  });
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  assert_eq!(task_worker.run().await?, 0);

  assert!(raise.await??, "the safe point's frame reached the subscriber");
  // The below-line move finds no account, so the regular select lands ext-2, at the line, over ext-1's 0.95.
  let builds = scripts.builds();
  assert_eq!((account_label(&builds[1]), task_worker.account_relays()), (Some("ext-2"), 1));
  Ok(())
}

// Python: tests/test_worker_account_relay.py::test_worker_warning_relays_past_the_cap_with_below_line_destinations
#[tokio::test]
async fn test_worker_warning_relays_past_the_cap_with_below_line_destinations() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  let mut scripts: Vec<String> =
    (0..4).map(|_| holds(&[rate_limit("allowed_warning", 0.92), user_tool_result()])).collect();
  scripts.push(exits(&[result()], 0));
  let scripts = Scripts::new(scripts);
  let config = pooled_config(&root, &["main", "a", "b", "c", "d"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  assert_eq!(task_worker.run().await?, 0);

  let labels: Vec<_> = scripts.builds().iter().map(|build| account_label(build).map(str::to_string)).collect();
  let expected: Vec<_> = ["main", "a", "b", "c", "d"].iter().map(|label| Some(label.to_string())).collect();
  assert_eq!(labels, expected);
  assert_eq!(task_worker.account_relays(), claude_relay::MAX_RELAYS_PER_TURN + 1);
  Ok(())
}

// Python: tests/test_worker_account_relay.py::test_worker_relay_limit_names_each_unavailable_account
#[tokio::test]
async fn test_worker_relay_limit_names_each_unavailable_account() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  let failed = "Failed to authenticate. Please run /login";
  let scripts = Scripts::new((0..4).map(|_| exits(&[assistant_text(failed)], 1)).collect());
  let config = pooled_config(&root, &["main", "a", "b", "c"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  let error = refusal(task_worker.run().await.expect_err("the run is refused"));

  let message = error.message.clone();
  assert!(!error.quota_exhausted);
  assert!(message.starts_with("Claude account relay limit reached (3 relays in one run; "), "{message}");
  assert!(message.ends_with("this run did not complete."), "{message}");
  for label in ["main", "a", "b", "c"] {
    assert!(message.contains(&format!("{label}: auth-failure cooldown until")), "{message}");
  }
  assert!(message.contains("UTC"), "{message}");
  assert_eq!(scripts.builds().len(), 1 + claude_relay::MAX_RELAYS_PER_TURN);
  Ok(())
}

// Python: tests/test_worker_account_relay.py::test_worker_login_failure_marks_the_account_and_notifies_the_session
#[tokio::test]
async fn test_worker_login_failure_marks_the_account_and_notifies_the_session() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts =
    Scripts::new(vec![exits(&[assistant_text("Failed to authenticate. Please run /login")], 1), exits(&[result()], 0)]);
  let config = pooled_config(&root, &["main", "ext-1"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let notices: Arc<Mutex<Vec<Value>>> = Arc::default();
  let mut task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;
  let sink = Arc::clone(&notices);
  task_worker.set_on_session_event(Arc::new(move |notice: Value| {
    sink.lock().expect("notices mutex is not poisoned").push(notice);
    Box::pin(async { Ok(()) })
  }));

  assert_eq!(task_worker.run().await?, 0);

  let main = setup.cfg.section::<claude_config::AccountsConfig>("accounts").claude[0].clone();
  assert!(!claude_accounts::healthy(&state, &main, None));
  let notices = notices.lock().expect("notices mutex is not poisoned").clone();
  let notice = notices.first().context("the session hears of the login failure")?;
  assert_eq!(notice["type"], claude_events::CLAUDE_ACCOUNT_LOGIN_REQUIRED);
  assert_eq!((notice["account"].as_str(), notice["reason"].as_str()), (Some("main"), Some("auth_failed")));
  assert!(
    read_events(&root.events_log())?.iter().any(|event| event["type"] == claude_events::CLAUDE_ACCOUNT_LOGIN_REQUIRED)
  );
  assert_eq!(account_label(&scripts.builds()[1]), Some("ext-1"));
  Ok(())
}

// Python: tests/test_worker_account_relay.py::test_worker_relay_does_not_compact_the_task_context
//
// The Python case spies on `compact_with_sonnet`. The port has no spy, so it checks the event log instead: a run whose
// prompt crosses the compaction thresholds persists no compaction event.
#[tokio::test]
async fn test_worker_relay_does_not_compact_the_task_context() -> anyhow::Result<()> {
  for prompt_tokens in [150_000_i64, 20_000] {
    let root = Root::new()?;
    make_transcript(&root.claude("main"), CC_ID)?;
    let state = Arc::new(ClaudeAccountState::new());
    let mut big = assistant_text("big");
    big["message"]["usage"] = json!({"input_tokens": 10, "cache_read_input_tokens": prompt_tokens - 10});
    let scripts = Scripts::new(vec![exits(&[big, rate_limit("rejected", 1.0)], 1), exits(&[result()], 0)]);
    let config = pooled_config(&root, &["main", "ext-1"], &[], &scripts.token)?;
    let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
    let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

    assert_eq!(task_worker.run().await?, 0);

    let compaction = [ET::CONTEXT_COMPACTED, ET::CONTEXT_COMPACT_FAILED];
    let logged = read_events(&root.events_log())?;
    assert!(logged.iter().all(|event| !compaction.contains(&event["type"].as_str().unwrap_or_default())));
  }
  Ok(())
}

// ---------------------------------------------------------------------------
// The build contract (`tests/test_worker_backend_build.py`)
// ---------------------------------------------------------------------------

fn hidden_cli() -> CliHost {
  CliHost {
    path: "/nonexistent/charliebot-test-path".to_string(),
    home: PathBuf::from("/nonexistent/charliebot-test-home"),
  }
}

fn hidden_codex() -> CodexHost {
  CodexHost {
    path: "/nonexistent/charliebot-test-path".to_string(),
    home: PathBuf::from("/nonexistent/charliebot-test-home"),
  }
}

/// The CLI types that resolve a binary when they are built, with every binary hidden.
fn register_hidden_binary_types(registry: &mut Registry) -> Result<(), RegistryError> {
  registry
    .register_package("test.charlie_code", |registry| charlie_code::register_with_host(registry, hidden_cli()))?;
  registry.register_package("test.antigravity", |registry| antigravity::register_with_host(registry, hidden_cli()))?;
  registry.register_package("test.gemini", |registry| gemini::register_with_host(registry, hidden_cli()))?;
  registry.register_package("test.codex", |registry| codex::register_with_host(registry, &hidden_codex()))
}

// Python: tests/test_worker_backend_build.py::test_launcher_build_still_fails_without_binaries
#[tokio::test]
async fn test_launcher_build_still_fails_without_binaries() -> anyhow::Result<()> {
  let root = Root::new()?;
  let mut registry = Registry::new();
  register_hidden_binary_types(&mut registry)?;
  let types = ["antigravity", "codex", "gemini", "charlie-code"];
  let options: Vec<String> = types
    .iter()
    .map(|backend_type| {
      let mut option = format!(
        "    - id: {backend_type}\n      label: {backend_type}\n      type: {backend_type}\n      model: test-model\n"
      );
      if *backend_type == "charlie-code" {
        option.push_str("      api_base: http://test.invalid\n");
      }
      option
    })
    .collect();
  let config = config_text(&root, &[], &[], &options)?;
  let setup = Setup::new(&root, registry, &config)?;

  for backend_type in types {
    let task_worker = setup.worker(setup.spec(&root, Some(backend_type), None)?)?;
    let error = task_worker
      .build_process_backend_for_test(None, true)
      .err()
      .with_context(|| format!("the {backend_type} launcher builds without its binary"))?;
    assert!(format!("{error:#}").contains("binary not found"), "{backend_type}: {error:#}");
  }
  Ok(())
}

// Python: tests/test_worker_backend_build.py::test_claude_resume_launch_still_passes_the_run_instructions_file
#[tokio::test]
async fn test_claude_resume_launch_still_passes_the_run_instructions_file() -> anyhow::Result<()> {
  let root = Root::new()?;
  let mut registry = Registry::new();
  registry.register_package("test.claude_code", claude_code::register)?;
  let config = config_text(&root, &[], &[], &[option_yaml("fable", "cc-claude", None, None)])?;
  let setup = Setup::new(&root, registry, &config)?;
  let mut spec = setup.spec(&root, Some("fable"), None)?;
  spec.instructions_content = Some("rules".to_string());
  let task_worker = setup.worker(spec)?;
  let launch = Launch {
    backend_kwargs: Map::new(),
    resume_id: Some("held-conversation".to_string()),
    prompt: None,
    account_label: None,
    private: None,
  };

  let built = task_worker.build_process_backend_for_test(Some(&launch), true)?;
  let cmd = built.backend.build_command("prompt")?;

  let resume = cmd.iter().position(|arg| arg == "--resume").context("the launch resumes")?;
  assert_eq!(cmd[resume + 1], "held-conversation");
  let instructions =
    cmd.iter().position(|arg| arg == "--append-system-prompt-file").context("instructions are passed")?;
  assert_eq!(
    cmd[instructions + 1],
    root.path().join("data").join(worker::HARNESS_INSTRUCTIONS_NAME).display().to_string()
  );
  assert!(!cmd.iter().any(|arg| arg == "--input-format"));
  assert!(!cmd.iter().any(|arg| arg == "--replay-user-messages"));
  Ok(())
}

// ---------------------------------------------------------------------------
// The hang diagnostics (`tests/test_worker_diagnostics.py`)
// ---------------------------------------------------------------------------

// Python: tests/test_worker_diagnostics.py::test_worker_writes_hang_diagnostics_and_emits_event
#[tokio::test]
async fn test_worker_writes_hang_diagnostics_and_emits_event() -> anyhow::Result<()> {
  let root = Root::new()?;
  let scripts = Scripts::new(vec![holds(&[result()])]);
  let config = config_text(&root, &[], &[], &[option_yaml("worker", "worker_script", Some(&scripts.token), None)])?;
  let setup = Setup::new(&root, scripted_registry(&Arc::new(ClaudeAccountState::new()))?, &config)?;
  let mut frames = setup.features.store.subscribe("t1");
  let task_worker = setup
    .worker(setup.spec(&root, Some("worker"), None)?)?
    .with_hang_timeouts(Duration::from_millis(300), Duration::from_secs(2));

  let exit_code = task_worker.run().await?;

  // The runner reports the signal that ended the process, where the Python case injects a fake with status 143.
  assert_eq!(exit_code, -15, "the hang path ends the process with SIGTERM");
  let diag_path = root.path().join("data").join(worker::HANG_DIAGNOSTICS_NAME);
  let written: Value = serde_json::from_str(&fs::read_to_string(&diag_path)?)?;
  assert!(written.is_object(), "the diagnostics the runner captured are written as an object");
  let logged = read_events(&root.events_log())?;
  let diag_events: Vec<_> =
    logged.iter().filter(|event| event["type"] == ET::SYSTEM && event["subtype"] == "hang_diagnostics").collect();
  assert_eq!(diag_events.len(), 1);
  assert_eq!(diag_events[0]["diagnostics_path"], diag_path.display().to_string());
  assert_eq!(diag_events[0]["exit_code"], exit_code);
  assert!(diag_events[0].get("timestamp").is_some());
  // The run's frames are queued on the thread channel; a quiet channel ends the read.
  let mut broadcast = Vec::new();
  while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_millis(200), frames.recv()).await {
    broadcast.push(serde_json::from_str::<Value>(&frame)?);
  }
  assert!(
    broadcast.iter().any(|frame| frame["type"] == ET::SYSTEM && frame["subtype"] == "hang_diagnostics"),
    "the hang diagnostics event is broadcast"
  );
  Ok(())
}

// Python: tests/test_worker_diagnostics.py::test_worker_no_hang_diagnostics_does_not_write_file
#[tokio::test]
async fn test_worker_no_hang_diagnostics_does_not_write_file() -> anyhow::Result<()> {
  let root = Root::new()?;
  let scripts = Scripts::new(vec![exits(&[], 0)]);
  let config = config_text(&root, &[], &[], &[option_yaml("worker", "worker_script", Some(&scripts.token), None)])?;
  let setup = Setup::new(&root, scripted_registry(&Arc::new(ClaudeAccountState::new()))?, &config)?;
  let task_worker = setup.worker(setup.spec(&root, Some("worker"), None)?)?;

  assert_eq!(task_worker.run().await?, 0);

  assert!(!root.path().join("data").join(worker::HANG_DIAGNOSTICS_NAME).exists());
  Ok(())
}

// Python: src/runtime/worker.py:357-372
#[tokio::test]
async fn test_resumed_run_captures_diagnostics_and_kills_its_live_process() -> anyhow::Result<()> {
  let root = Root::new()?;
  // Resume builds its backend twice, for the stream translator and for the exit scan, and each build takes one script.
  // Resume runs neither script.
  let scripts = Scripts::new(vec![String::new(), String::new()]);
  let config = config_text(&root, &[], &[], &[option_yaml("worker", "worker_script", Some(&scripts.token), None)])?;
  let setup = Setup::new(&root, scripted_registry(&Arc::new(ClaudeAccountState::new()))?, &config)?;
  let mut frames = setup.features.store.subscribe("t1");
  let data = root.path().join("data");
  fs::create_dir_all(&data)?;
  // The agent writes its result to the raw log, then stays alive. `exec` leaves the sleep as the group's only process.
  let result_line = serde_json::to_string(&result())?;
  let raw_log = data.join(RAW_LOG_NAME);
  let agent_script = format!("printf '%s\\n' '{result_line}' >> '{}'; exec sleep {HOLD_SECONDS}", raw_log.display());
  let agent = Command::new("/bin/sh")
    .args(["-c", &agent_script])
    .process_group(0)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()?;
  let pid = agent.id();
  setup.guard.track(pid);
  let agent = Arc::new(Mutex::new(agent));
  let mut spec = setup.spec(&root, Some("worker"), None)?;
  spec.thread.pid = Some(pid);
  let task_worker = setup.worker(spec)?.with_hang_timeouts(Duration::from_millis(300), Duration::from_secs(2));
  // The agent is this test's child. The probe reaps it once it exits, so its group id then resolves to no process.
  let probe = Arc::clone(&agent);
  let is_alive = Arc::new(move || {
    let mut agent = probe.lock().expect("agent mutex is not poisoned");
    agent.try_wait().expect("the agent's exit status reads").is_none()
  });

  let exit_code = task_worker.resume(is_alive, None).await?;

  assert_eq!(exit_code, 0, "the raw log's result is a success");
  assert!(data.join(worker::HANG_DIAGNOSTICS_NAME).is_file(), "the diagnostics are written beside the events log");
  assert_eq!(unsafe { libc::kill(-(pid as libc::pid_t), 0) }, -1, "the agent's process group is gone");
  assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
  // The events log ends with its last persisted terminal event, which carries the scanned exit code. The completion
  // is only broadcast, as `worker.py` `_emit_terminal_events` broadcasts it, so the thread channel carries it.
  let logged = read_events(&root.events_log())?;
  let persisted_terminal = logged.last().context("the events log ends with a terminal event")?;
  assert_eq!(persisted_terminal["type"], ET::SYSTEM);
  assert_eq!(persisted_terminal["subtype"], "hang_diagnostics");
  assert_eq!(persisted_terminal["exit_code"], exit_code);
  let mut broadcast = Vec::new();
  while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_millis(200), frames.recv()).await {
    broadcast.push(serde_json::from_str::<Value>(&frame)?);
  }
  let completion = broadcast.last().context("the thread channel carries the completion")?;
  assert_eq!(completion["type"], ET::COMPLETE);
  assert_eq!(completion["exit_code"], exit_code);
  Ok(())
}

// ---------------------------------------------------------------------------
// The event line and the quota scan (`tests/test_worker_event_line.py`, `tests/test_worker_quota_scan.py`)
// ---------------------------------------------------------------------------

// Python: tests/test_worker_event_line.py::test_event_line_round_trips_the_event
#[test]
fn test_event_line_round_trips_the_event() {
  let event = json!({
    "type": "assistant",
    "message": {"content": [{"type": "text", "text": "概要 — 100% done"}], "usage": {"input_tokens": 10}},
    "timestamp": "2026-09-11T00:00:00+00:00",
  });
  let line = worker::event_line(&event);
  assert_eq!(serde_json::from_slice::<Value>(&line).expect("the line is JSON"), event);
}

// Python: tests/test_worker_quota_scan.py::test_error_event_with_quota_pattern_raises_and_persists
#[tokio::test]
async fn test_error_event_with_quota_pattern_raises_and_persists() -> anyhow::Result<()> {
  let root = Root::new()?;
  let setup =
    Setup::new(&root, scripted_registry(&Arc::new(ClaudeAccountState::new()))?, &config_text(&root, &[], &[], &[])?)?;
  let task_worker = setup.worker(setup.spec(&root, None, None)?)?;
  let event = json!({"type": ET::ERROR, "message": "API Error: quota exceeded for project", "content": ""});

  let error = task_worker.process_event_for_test(event.clone(), None).await.expect_err("the quota error ends the run");

  assert!(error.downcast_ref::<QuotaExhaustedError>().is_some());
  let logged = read_events(&root.events_log())?;
  assert_eq!(logged.len(), 1);
  assert_eq!(logged[0]["message"], event["message"]);
  Ok(())
}

// Python: tests/test_worker_quota_scan.py::test_a_rejected_rate_limit_event_without_a_relay_watch_raises_with_its_reset
#[tokio::test]
async fn test_a_rejected_rate_limit_event_without_a_relay_watch_raises_with_its_reset() -> anyhow::Result<()> {
  let root = Root::new()?;
  let setup =
    Setup::new(&root, scripted_registry(&Arc::new(ClaudeAccountState::new()))?, &config_text(&root, &[], &[], &[])?)?;
  let task_worker = setup.worker(setup.spec(&root, None, None)?)?;
  // 2030-01-01T05:00:00Z.
  let resets_at = 1_893_512_400_i64;
  let event = json!({
    "type": ET::RATE_LIMIT_EVENT,
    "rate_limit_info": {"status": "rejected", "rateLimitType": "five_hour", "resetsAt": resets_at},
  });

  let error = task_worker.process_event_for_test(event, None).await.expect_err("the rejection ends the run");

  let quota = error.downcast_ref::<QuotaExhaustedError>().context("the rejection is a quota error")?;
  assert_eq!(quota.resets_at, Some(OffsetDateTime::from_unix_timestamp(resets_at)?));
  Ok(())
}

// Python: tests/test_worker_quota_scan.py::test_a_rejected_rate_limit_event_under_a_relay_watch_never_raises
#[tokio::test]
async fn test_a_rejected_rate_limit_event_under_a_relay_watch_never_raises() -> anyhow::Result<()> {
  let root = Root::new()?;
  let setup =
    Setup::new(&root, scripted_registry(&Arc::new(ClaudeAccountState::new()))?, &config_text(&root, &[], &[], &[])?)?;
  let task_worker = setup.worker(setup.spec(&root, None, None)?)?;
  let event = json!({
    "type": ET::RATE_LIMIT_EVENT,
    "rate_limit_info": {"status": "rejected", "rateLimitType": "five_hour", "resetsAt": 1_893_512_400_i64},
  });
  let mut watch = RecordingWatch::default();

  let terminate = task_worker.process_event_for_test(event.clone(), Some(&mut watch)).await?;

  assert!(!terminate);
  let seen = watch.seen.lock().expect("seen mutex is not poisoned").clone();
  assert_eq!(seen.len(), 1);
  assert_eq!(seen[0]["rate_limit_info"], event["rate_limit_info"]);
  let logged = read_events(&root.events_log())?;
  assert_eq!(logged[0]["type"], ET::RATE_LIMIT_EVENT);
  Ok(())
}

// ---------------------------------------------------------------------------
// The task build for a non-Claude backend (`tests/test_backend_hooks.py`)
// ---------------------------------------------------------------------------

// Python: tests/test_spawner_backend_propagation.py:50.
#[test]
fn anthropic_compatible_option_requires_a_resolved_model_and_keeps_its_endpoint() -> anyhow::Result<()> {
  let root = Root::new()?;
  fs::create_dir_all(root.home())?;
  let option = option_yaml("anthro", "cc-anthropic-compatible", None, None);
  fs::write(root.home().join("config.yaml"), config_text(&root, &[], &[], &[option])?)?;
  let mut registry = Registry::new();
  registry.register_package("src.backends.anthropic_compatible", anthropic_compatible::register)?;
  let config = registry.load_config(&root.home())?;

  let refused = charliebot_tasks::spawner_backends::resolve_backend_option(&config, "anthro", Some(""));
  assert_eq!(refused.unwrap_err().to_string(), "resolved model is required");

  let option = charliebot_tasks::spawner_backends::resolve_backend_option(&config, "anthro", Some("resolved-model"))?;

  assert_eq!(option.model, "resolved-model");
  assert_eq!(option.type_fields["api_base"].as_str(), Some("https://api.synthetic.example"));
  assert_eq!(option.type_fields["context_window"].as_u64(), Some(409_600));
  Ok(())
}

// Python: tests/test_backend_hooks.py:174.
// Python: tests/test_task_run_backend_types.py:30.
#[tokio::test]
async fn test_non_claude_task_build_receives_no_claude_account() -> anyhow::Result<()> {
  for backend_type in ["codex", "cc-anthropic-compatible"] {
    let root = Root::new()?;
    let scripts = Scripts::new(vec![exits(&[assistant_text("done")], 0)]);
    let config = config_text(&root, &[], &[], &[option_yaml("variant", backend_type, Some(&scripts.token), None)])?;
    let setup = Setup::new(&root, scripted_registry(&Arc::new(ClaudeAccountState::new()))?, &config)?;
    let task_worker = setup.worker(setup.spec(&root, Some("variant"), None)?)?;

    assert_eq!(task_worker.run().await?, 0, "{backend_type}");

    let builds = scripts.builds();
    assert!(builds[0].args.backend_kwargs.get("claude_account").is_none(), "{backend_type}");
  }
  Ok(())
}
