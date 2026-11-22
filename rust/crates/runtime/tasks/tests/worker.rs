//! The task-run process manager over scripted `/bin/sh` backends.
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
use charliebot_agents::backend_availability::{self, BackendAvailability};
use charliebot_agents::backend_pool::BackendPool;
use charliebot_agents::cli_base::CliHost;
use charliebot_agents::model_pool::RotationPositions;
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
use charliebot_registry::sequence::QuotaFailure;
use charliebot_registry::store_hooks::store_hooks;
use charliebot_store::metadata::{
  SessionMetadata, TaskApprover, TaskProfile, TaskSpec, TaskType, ThreadMetadata, UtcDateTime,
};
use charliebot_store::run_records::{RAW_LOG_NAME, RunRecord};
use charliebot_store::{ChatEvent, CreateTask, Store, TaskCaller};
use charliebot_tasks::auto_switch::{self, MemberSwitch};
use charliebot_tasks::handoff::HandoffSource;
use charliebot_tasks::master_queue::MasterQueue;
use charliebot_tasks::master_state::{MasterResumeRecord, MasterResumeRequest, TaskRunBinding};
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
  registry.config_section(ConfigSection {
    key: "accounts",
    legacy_keys: &[],
    parse: claude_config::parse_accounts,
    dump: claude_config::dump_accounts,
  })?;
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
    // The line sits in single quotes, so each quote in it closes the quoting, writes an escaped quote, and reopens it.
    let quoted = line.replace('\'', r"'\''");
    commands.push(format!("printf '%s\\n' '{quoted}'"));
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
// Event fixtures
// ---------------------------------------------------------------------------

fn assistant_text(text: &str) -> Value {
  json!({"type": ET::ASSISTANT, "message": {"content": [{"type": "text", "text": text}]}})
}

/// The synthetic assistant message Claude Code writes when a login is lost: the structured API-error fields the relay
/// reads, with a 401 status.
fn lost_login(text: &str) -> Value {
  json!({
    "type": ET::ASSISTANT,
    "message": {"model": "<synthetic>", "content": [{"type": "text", "text": text}]},
    "error": "authentication_failed",
    "is_api_error_message": true,
    "api_error_status": 401,
  })
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

/// A rate-limit observation of the five-hour window that resets at `resets_at` (Unix seconds).
fn rate_limit_resetting(status: &str, utilization: f64, resets_at: i64) -> Value {
  json!({
    "type": ET::RATE_LIMIT_EVENT,
    "rate_limit_info": {
      "status": status,
      "rateLimitType": "five_hour",
      "resetsAt": resets_at,
      "unifiedWindows": {"five_hour": {"utilization": utilization}, "seven_day": {"utilization": 0.3}},
    },
  })
}

/// The errored result Claude Code writes when the login is rejected: a 429 with the limit's wording.
fn rejected_result() -> Value {
  json!({
    "type": ET::RESULT,
    "is_error": true,
    "api_error_status": 429,
    "result": "You've hit your 5-hour limit \u{b7} resets 3:50am (America/Los_Angeles)",
  })
}

/// The errored result Claude Code writes when the login's token is revoked: a 401 with the authentication text.
fn auth_failed_result() -> Value {
  json!({
    "type": ET::RESULT,
    "is_error": true,
    "api_error_status": 401,
    "result": "Failed to authenticate: OAuth token revoked.",
  })
}

/// The errored result of a temporary backend failure: a 529 overload with its text.
fn overloaded_result() -> Value {
  json!({"type": ET::RESULT, "is_error": true, "api_error_status": 529, "result": "Internal server error"})
}

/// A reading taken at `minutes` before the fixed instant 2026-09-06 20:00 UTC, the instant the relay tests observe.
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
  option_yaml_with_model(id, backend_type, "test-model", token, account_pool)
}

/// One option's YAML with its model named, so a test can give two options two rejection families.
fn option_yaml_with_model(
  id: &str,
  backend_type: &str,
  model: &str,
  token: Option<&str>,
  account_pool: Option<&str>,
) -> String {
  let token = token.map(|token| format!("      {TOKEN_FIELD}: {token}\n")).unwrap_or_default();
  let pool = account_pool.map(|pool| format!("      account_pool: {pool}\n")).unwrap_or_default();
  let endpoint = if backend_type == "cc-anthropic-compatible" {
    "      api_base: https://api.synthetic.example\n      context_window: 409600\n"
  } else {
    ""
  };
  let fields = format!("{token}{pool}{endpoint}");
  format!("    - id: {id}\n      label: {id}\n      type: {backend_type}\n      model: {model}\n{fields}")
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
    startup
      .insert(AgentBaseEnvironment { values: charliebot_agents::gh_wrapper::stub_environment(&home, "/usr/bin") })?;
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
      .ancestors()
      .nth(4)
      .context("tasks crate manifest has no repository root ancestor")?;
    let served_repo = charliebot_tasks::test_support::served_checkout(checkout, &home)?;
    startup.insert(RepoRoot::new(Some(served_repo.into_os_string()))?)?;
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
// The Worker's relay loop
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "integration"]
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

/// A rejected process that ends with a clean result is not relayed at exit 0: one process runs, the exit code comes back
/// as returned, and the log holds no error.
#[tokio::test]
#[ignore = "integration"]
async fn test_worker_clean_end_after_a_rejection_is_not_relayed_at_exit_zero() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts = Scripts::new(vec![exits(&[assistant_text("done"), rate_limit("rejected", 1.0), result()], 0)]);
  let config = pooled_config(&root, &["main", "ext-1", "ext-2"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  assert_eq!(task_worker.run().await?, 0);
  assert_eq!(scripts.builds().len(), 1, "a clean end starts no relay process");
  assert_eq!(task_worker.account_relays(), 0);
  let logged = read_events(&root.events_log())?;
  assert!(logged.iter().all(|event| event["type"] != ET::ERROR), "the rejection is not an error: {logged:?}");
  Ok(())
}

/// The same clean end at exit 1: the exit code is not part of the decision, so the run is not relayed and the code is
/// recorded as the process returned it.
#[tokio::test]
#[ignore = "integration"]
async fn test_worker_clean_end_after_a_rejection_is_not_relayed_at_exit_one() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts = Scripts::new(vec![exits(&[assistant_text("done"), rate_limit("rejected", 1.0), result()], 1)]);
  let config = pooled_config(&root, &["main", "ext-1", "ext-2"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  assert_eq!(task_worker.run().await?, 1, "the exit code is recorded as the process returned it");
  assert_eq!(scripts.builds().len(), 1, "a clean end starts no relay process");
  assert_eq!(task_worker.account_relays(), 0);
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn test_worker_raises_pool_exhausted_when_no_account_is_left() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  claude_accounts::observe_rate_limit(&state, "ext-1", &rate_limit_info("rejected", 1.0), None, None);
  let scripts = Scripts::new(vec![exits(&[rate_limit("rejected", 1.0)], 1)]);
  let config = pooled_config(&root, &["main", "ext-1"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  let error = refusal(task_worker.run().await.expect_err("the run is refused"));

  assert!(error.message.contains("earliest reset"), "{}", error.message);
  assert_eq!(error.failure, charliebot_registry::backend::MemberFailure::Quota);
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn test_worker_relay_stays_inside_the_option_pool() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  claude_accounts::observe_rate_limit(&state, "ext-1", &rate_limit_info("allowed", 0.50), None, None);
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

#[tokio::test]
#[ignore = "integration"]
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

#[tokio::test]
#[ignore = "integration"]
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
  assert_eq!(
    error.failure,
    charliebot_registry::backend::MemberFailure::Quota,
    "every login is rejected, so the pool is exhausted at the cap"
  );
  assert!(error.resets_at.is_some(), "the exhausted pool names the earliest reset its rejections stated");
  assert_eq!(scripts.builds().len(), 1 + claude_relay::MAX_RELAYS_PER_TURN);
  assert_eq!(task_worker.account_relays(), claude_relay::MAX_RELAYS_PER_TURN);
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
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

#[tokio::test]
#[ignore = "integration"]
async fn test_worker_warning_relay_lands_strictly_below_the_line_in_the_tie_band() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  // ext-1 sits below the line (headroom 0.11); ext-2 sits at the line (headroom 0.095) with the older reading.
  claude_accounts::observe_rate_limit(&state, "ext-1", &rate_limit_info("allowed", 0.89), Some(reading_at(5)), None);
  claude_accounts::observe_rate_limit(&state, "ext-2", &rate_limit_info("allowed", 0.905), Some(reading_at(30)), None);
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

//
// The reading of the target is raised between the gate and the move. Here a subscriber on
// the thread channel raises it when the safe point's `user` frame is broadcast. The watch has already observed that
// event (the gate), and the move reads the pool only after the process has exited.
#[tokio::test]
#[ignore = "integration"]
async fn test_worker_falls_back_to_the_regular_select_when_the_below_line_target_vanishes() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  // ext-1 is the gate's below-line destination; ext-2 already sits at the line.
  claude_accounts::observe_rate_limit(&state, "ext-1", &rate_limit_info("allowed", 0.85), Some(reading_at(0)), None);
  claude_accounts::observe_rate_limit(&state, "ext-2", &rate_limit_info("allowed", 0.92), Some(reading_at(0)), None);
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
        claude_accounts::observe_rate_limit(
          &raised_state,
          "ext-1",
          &rate_limit_info("allowed_warning", 0.95),
          None,
          None,
        );
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

#[tokio::test]
#[ignore = "integration"]
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

#[tokio::test]
#[ignore = "integration"]
async fn test_worker_relay_limit_names_each_unavailable_account() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  let failed = "Failed to authenticate. Please run /login";
  let scripts = Scripts::new((0..4).map(|_| exits(&[lost_login(failed)], 1)).collect());
  let config = pooled_config(&root, &["main", "a", "b", "c"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  let error = refusal(task_worker.run().await.expect_err("the run is refused"));

  let message = error.message.clone();
  assert_eq!(error.failure, charliebot_registry::backend::MemberFailure::Other);
  assert!(message.starts_with("Claude account relay limit reached (3 relays in one run; "), "{message}");
  assert!(message.ends_with("this run did not complete."), "{message}");
  for label in ["main", "a", "b", "c"] {
    assert!(message.contains(&format!("{label}: auth-failure cooldown until")), "{message}");
  }
  assert!(message.contains("UTC"), "{message}");
  assert_eq!(scripts.builds().len(), 1 + claude_relay::MAX_RELAYS_PER_TURN);
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn test_worker_login_failure_marks_the_account_and_notifies_the_session() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts =
    Scripts::new(vec![exits(&[lost_login("Failed to authenticate. Please run /login")], 1), exits(&[result()], 0)]);
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

#[tokio::test]
#[ignore = "integration"]
async fn test_worker_login_refusal_result_relays_to_the_next_login() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  let failed = "Failed to authenticate. API Error: 401 Not allowed to access model due to tags configuration.";
  let refused = json!({"type": ET::RESULT, "subtype": "error", "is_error": true, "result": failed});
  let scripts = Scripts::new(vec![exits(&[lost_login(failed), refused], 1), exits(&[result()], 0)]);
  let config = pooled_config(&root, &["main", "ext-1"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  assert_eq!(task_worker.run().await?, 0, "the login relay runs the turn on the next login");

  assert_eq!(scripts.builds().len(), 2);
  assert_eq!(account_label(&scripts.builds()[1]), Some("ext-1"));
  let main = setup.cfg.section::<claude_config::AccountsConfig>("accounts").claude[0].clone();
  assert!(!claude_accounts::healthy(&state, &main, None));
  Ok(())
}

//
// There is no spy on `compact_with_sonnet`, so the test checks the event log instead: a run whose
// prompt crosses the compaction thresholds persists no compaction event.
#[tokio::test]
#[ignore = "integration"]
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
// A rejected login moves the run; a row waits until no login can serve the model
// ---------------------------------------------------------------------------

/// Attaches the availability owner over the test's home, as the server does, so the test can read and write rows.
fn attach_availability(setup: &Setup, root: &Root) -> anyhow::Result<()> {
  setup.features.startup.insert(BackendAvailability::load(&root.home())?)?;
  Ok(())
}

/// Records a failed run's quota failure the way the execution layer does, for the pooled option, and returns the row's
/// reset.
fn record_failure(setup: &Setup, resets_at: Option<UtcDateTime>, message: &str) -> anyhow::Result<OffsetDateTime> {
  let availability = backend_availability::backend_availability(&setup.features)?;
  availability
    .record(&setup.cfg, &setup.features.registry, POOLED, resets_at, backend_availability::REASON_QUOTA_ERROR, message)?
    .context("the failed run writes its row")
}

#[tokio::test]
#[ignore = "integration"]
async fn test_a_new_worker_run_rejected_on_main_moves_to_ext1_and_writes_no_row() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts = Scripts::new(vec![
    exits(&[assistant_text("working"), rate_limit("rejected", 1.0), rejected_result()], 1),
    exits(&[assistant_text("done"), result()], 0),
  ]);
  let config = pooled_config(&root, &["main", "ext-1"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  attach_availability(&setup, &root)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  assert_eq!(task_worker.run().await?, 0);

  let builds = scripts.builds();
  assert_eq!((account_label(&builds[0]), account_label(&builds[1])), (Some("main"), Some("ext-1")));
  assert_eq!(task_worker.account_relays(), 1);
  let availability = backend_availability::backend_availability(&setup.features)?;
  assert!(availability.row(POOLED).is_none(), "a login with headroom is left, so the model writes no row");
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn test_a_resumed_worker_run_rejected_on_main_relays_to_ext1_and_completes() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  // Resume builds the backend for the stream translator and for the exit scan, and each build takes one script. The
  // relay's process is the third build, the only one that runs.
  let scripts = Scripts::new(vec![String::new(), String::new(), exits(&[assistant_text("done"), result()], 0)]);
  let config = pooled_config(&root, &["main", "ext-1"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  // The held process wrote its rejection to the raw log and exited, so the follow reads the log to its end.
  let data = root.path().join("data");
  fs::create_dir_all(&data)?;
  let mut raw = String::new();
  for event in [assistant_text("working"), rate_limit("rejected", 1.0), rejected_result()] {
    raw.push_str(&format!("{}\n", serde_json::to_string(&event)?));
  }
  fs::write(data.join(RAW_LOG_NAME), raw)?;
  let mut spec = setup.spec(&root, Some(POOLED), Some(CC_ID))?;
  spec.held_native_id = Some(CC_ID.to_string());
  let task_worker = setup.worker(spec)?;

  assert_eq!(task_worker.resume(Arc::new(|| false), None).await?, 0);

  let builds = scripts.builds();
  assert_eq!(builds.len(), 3);
  assert_eq!(account_label(&builds[2]), Some("ext-1"));
  assert_eq!(task_worker.account_relays(), 1);
  Ok(())
}

/// Re-attaches a run to its live process, which wrote `events` to its raw log and exited, so the follow reads the log
/// to its end. A keyed run's task names an improve host: no login on this machine holds its conversation, because the
/// improve host keeps the transcript. Returns the worker and the count of processes it spawns.
fn reattach_run(
  setup: &Setup,
  root: &Root,
  option_id: &str,
  events: &[Value],
  keyed: bool,
) -> anyhow::Result<(Worker, Arc<AtomicUsize>)> {
  let data = root.path().join("data");
  fs::create_dir_all(&data)?;
  let mut raw = String::new();
  for event in events {
    raw.push_str(&format!("{}\n", serde_json::to_string(event)?));
  }
  fs::write(data.join(RAW_LOG_NAME), raw)?;
  let mut spec = setup.spec(root, Some(option_id), None)?;
  spec.held_native_id = Some(CC_ID.to_string());
  if keyed {
    spec.session_meta.as_mut().context("a task run with a backend option has session metadata")?.task =
      Some(TaskSpec {
        goal: String::new(),
        repo_path: None,
        base_branch: None,
        task_type: None,
        keep_worktree: false,
        cold_read: None,
        approver: TaskApprover::User,
        improve_host: Some("improve-host".to_string()),
      });
  }
  let spawns = Arc::new(AtomicUsize::new(0));
  let counter = Arc::clone(&spawns);
  let tracked = spec.on_spawned.take().context("a spec tracks its spawns")?;
  spec.on_spawned = Some(Arc::new(move |thread: ThreadMetadata| {
    counter.fetch_add(1, Ordering::SeqCst);
    tracked(thread)
  }));
  Ok((setup.worker(spec)?, spawns))
}

/// Re-attaches a keyed run whose held process wrote an assistant text and a clean result.
fn keyed_reattach(setup: &Setup, root: &Root, option_id: &str) -> anyhow::Result<(Worker, Arc<AtomicUsize>)> {
  reattach_run(setup, root, option_id, &[assistant_text("working"), result()], true)
}

#[tokio::test]
#[ignore = "integration"]
async fn test_a_keyed_run_reattached_on_pooled_claude_follows_without_placement() -> anyhow::Result<()> {
  let root = Root::new()?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts = Scripts::new(vec![String::new(), String::new()]);
  let config = pooled_config(&root, &["main", "ext-1"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let (task_worker, spawns) = keyed_reattach(&setup, &root, POOLED)?;

  assert_eq!(task_worker.resume(Arc::new(|| false), None).await?, 0);
  assert_eq!(spawns.load(Ordering::SeqCst), 0, "the run follows its live process and spawns none");
  assert_eq!(task_worker.account_relays(), 0, "a keyed run cannot relay, so the follow moves no login");
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn test_a_keyed_run_reattached_on_plain_claude_code_follows_without_placement() -> anyhow::Result<()> {
  let root = Root::new()?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts = Scripts::new(vec![String::new(), String::new()]);
  let option = option_yaml("claude-plain", "cc-claude", Some(&scripts.token), None);
  let config = config_text(&root, &[], &[], &[option])?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let (task_worker, spawns) = keyed_reattach(&setup, &root, "claude-plain")?;

  assert_eq!(task_worker.resume(Arc::new(|| false), None).await?, 0);
  assert_eq!(spawns.load(Ordering::SeqCst), 0, "the run follows its live process and spawns none");
  assert_eq!(task_worker.account_relays(), 0, "a keyed run cannot relay, so the follow moves no login");
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn test_a_keyed_run_reattached_on_codex_follows_without_placement() -> anyhow::Result<()> {
  let root = Root::new()?;
  let scripts = Scripts::new(vec![String::new(), String::new()]);
  let option = option_yaml("codex-keyed", "codex", Some(&scripts.token), None);
  let config = config_text(&root, &[], &[], &[option])?;
  let setup = Setup::new(&root, scripted_registry(&Arc::new(ClaudeAccountState::new()))?, &config)?;
  let (task_worker, spawns) = keyed_reattach(&setup, &root, "codex-keyed")?;

  assert_eq!(task_worker.resume(Arc::new(|| false), None).await?, 0);
  assert_eq!(spawns.load(Ordering::SeqCst), 0, "the run follows its live process and spawns none");
  assert_eq!(task_worker.account_relays(), 0, "a keyed run cannot relay, so the follow moves no login");
  Ok(())
}

/// A reattached keyed run has no login watch, so no watch reads its lost-login event, and its errored result
/// decides the end. That end is a token shortage: the round moves to the pool's next member and records no member out.
#[tokio::test]
#[ignore = "integration"]
async fn test_a_keyed_run_reattached_that_loses_its_token_ends_as_token_short() -> anyhow::Result<()> {
  let root = Root::new()?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts = Scripts::new(vec![String::new(), String::new()]);
  let config = pooled_config(&root, &["main", "ext-1"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let lost = [lost_login("Failed to authenticate: OAuth token revoked."), auth_failed_result()];
  let (task_worker, spawns) = reattach_run(&setup, &root, POOLED, &lost, true)?;

  let error = task_worker.resume(Arc::new(|| false), None).await.expect_err("the lost token ends the run");
  let quota = error.downcast_ref::<QuotaExhaustedError>().context("the run ends in a member failure")?;
  assert_eq!(quota.failure(), charliebot_registry::backend::MemberFailure::TokenShort);
  assert_eq!(quota.resets_at, None);
  assert_eq!(spawns.load(Ordering::SeqCst), 0, "the run follows its live process and spawns none");
  Ok(())
}

/// The same end on an unkeyed run keeps the access refusal, which the execution layer records as the member's
/// shared row.
#[tokio::test]
#[ignore = "integration"]
async fn test_an_unkeyed_run_reattached_that_loses_its_token_keeps_access_refused() -> anyhow::Result<()> {
  let root = Root::new()?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts = Scripts::new(vec![String::new(), String::new()]);
  let option = option_yaml("claude-plain", "cc-claude", Some(&scripts.token), None);
  let config = config_text(&root, &[], &[], &[option])?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let lost = [lost_login("Failed to authenticate: OAuth token revoked."), auth_failed_result()];
  let (task_worker, spawns) = reattach_run(&setup, &root, "claude-plain", &lost, false)?;

  let error = task_worker.resume(Arc::new(|| false), None).await.expect_err("the refused token ends the run");
  let quota = error.downcast_ref::<QuotaExhaustedError>().context("the run ends in a member failure")?;
  assert_eq!(quota.failure(), charliebot_registry::backend::MemberFailure::AccessRefused);
  assert_eq!(spawns.load(Ordering::SeqCst), 0, "the run follows its live process and spawns none");
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn test_a_rejection_moves_the_run_to_the_login_with_headroom_and_the_next_run_starts_there() -> anyhow::Result<()>
{
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts = Scripts::new(vec![
    exits(&[assistant_text("working"), rate_limit("rejected", 1.0), rejected_result()], 1),
    exits(&[assistant_text("done"), result()], 0),
    exits(&[assistant_text("next"), result()], 0),
  ]);
  let config = pooled_config(&root, &["main", "ext-1"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let first = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;
  assert_eq!(first.run().await?, 0);

  let next = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;
  assert_eq!(next.run().await?, 0);

  let builds = scripts.builds();
  let labels: Vec<_> = builds.iter().map(account_label).collect();
  assert_eq!(labels, [Some("main"), Some("ext-1"), Some("ext-1")]);
  assert_eq!(next.account_relays(), 0, "the next run starts on the login with headroom, so it never relays");
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn test_all_logins_rejected_writes_the_row_to_the_earliest_reset() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  // ext-1 resets two hours out and main three hours out, so the model's row runs to ext-1's reset, the earliest.
  let ext_reset = (OffsetDateTime::now_utc() + TimeDuration::hours(2)).unix_timestamp();
  let ext_info = rate_limit_resetting("rejected", 1.0, ext_reset)["rate_limit_info"].clone();
  claude_accounts::observe_rate_limit(&state, "ext-1", &ext_info, None, None);
  let scripts = Scripts::new(vec![exits(&[rate_limit("rejected", 1.0), rejected_result()], 1)]);
  let config = pooled_config(&root, &["main", "ext-1"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  attach_availability(&setup, &root)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  let refused = refusal(task_worker.run().await.expect_err("no login is left for the model"));
  let until = record_failure(&setup, refused.resets_at, &refused.message)?;

  assert_eq!(until, OffsetDateTime::from_unix_timestamp(ext_reset)?);
  let availability = backend_availability::backend_availability(&setup.features)?;
  assert_eq!(availability.out_until(POOLED), Some(until));
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn test_a_fable_week_rejection_holds_only_its_family() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  // A rejection below the whole window (0.95) holds the model's family only: the fable option moves, the opus one does not.
  let scripts = Scripts::new(vec![
    exits(&[assistant_text("working"), rate_limit("rejected", 0.95), rejected_result()], 1),
    exits(&[assistant_text("done"), result()], 0),
    exits(&[assistant_text("opus"), result()], 0),
  ]);
  let token = scripts.token.as_str();
  let options = [
    option_yaml_with_model(POOLED, "cc-claude", "claude-fable-5", Some(token), None),
    option_yaml_with_model("opus-option", "cc-claude", "claude-opus-5", Some(token), None),
  ];
  let config = config_text(&root, &["main", "ext-1"], &[], &options)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  let fable = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;
  assert_eq!(fable.run().await?, 0);
  let opus = setup.worker(setup.spec(&root, Some("opus-option"), Some(CC_ID))?)?;
  assert_eq!(opus.run().await?, 0);

  assert_eq!(account_label(&scripts.builds()[1]), Some("ext-1"), "the fable run moved to the other login");
  assert!(claude_accounts::rejected_until(&state, "main", Some("claude-fable-5"), None).is_some());
  assert!(claude_accounts::rejected_until(&state, "main", Some("claude-opus-5"), None).is_none());
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
async fn test_a_temporary_529_result_keeps_the_thirty_minute_row() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  let scripts = Scripts::new(vec![exits(&[assistant_text("working"), overloaded_result()], 1)]);
  let config = pooled_config(&root, &["main", "ext-1"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  attach_availability(&setup, &root)?;
  let task_worker = setup.worker(setup.spec(&root, Some(POOLED), Some(CC_ID))?)?;

  let error = task_worker.run().await.expect_err("an overloaded login fails the run");
  let quota = error.downcast_ref::<QuotaExhaustedError>().context("the run ends in a quota failure")?;
  assert_eq!(quota.failure(), charliebot_registry::backend::MemberFailure::Quota);
  assert_eq!(scripts.builds().len(), 1, "a temporary failure is not relayed to another login");
  let until = record_failure(&setup, None, &quota.to_string())?;

  let remaining = until - OffsetDateTime::now_utc();
  assert!(
    remaining > TimeDuration::minutes(29) && remaining <= backend_availability::DEFAULT_COOLDOWN,
    "the row runs for the cooldown, not to a pool reset: {remaining:?}"
  );
  let availability = backend_availability::backend_availability(&setup.features)?;
  assert_eq!(
    availability.row(POOLED).map(|row| row.reason),
    Some(backend_availability::REASON_QUOTA_ERROR.to_string())
  );
  Ok(())
}

// ---------------------------------------------------------------------------
// A model group's Claude member: a login with headroom keeps it, a spent login pool moves it
// ---------------------------------------------------------------------------

/// The model group the item-6 test switches over: the Claude subscription member first, the metered member second.
const GROUP: &str = "group";
/// The metered member of [`GROUP`], which no login pool backs.
const METERED: &str = "metered";

/// The config text of a test with a model group under `backends.pools`.
fn config_text_with_group(
  root: &Root,
  labels: &[&str],
  group: (&str, &[&str]),
  options: &[String],
) -> anyhow::Result<String> {
  let mut text = config_text(root, labels, &[], options)?;
  let (id, members) = group;
  text.push_str(&format!(
    "  pools:\n    - id: {id}\n      label: {id}\n      rule: priority\n      members: [{}]\n",
    members.join(", ")
  ));
  Ok(text)
}

#[tokio::test]
#[ignore = "integration"]
async fn test_a_model_group_keeps_claude_while_a_login_can_run_and_takes_the_metered_member_when_none_can()
-> anyhow::Result<()> {
  let root = Root::new()?;
  let state = Arc::new(ClaudeAccountState::new());
  // No process runs in this test: the group's selection and the row are its subject.
  let scripts = Scripts::new(Vec::new());
  // A priority pool reads each member's billing from its option.
  let options = [
    format!(
      "{}      billing: subscription\n",
      option_yaml_with_model(POOLED, "cc-claude", "claude-fable-5", Some(&scripts.token), None)
    ),
    format!(
      "{}      billing: payg\n",
      option_yaml_with_model(METERED, "worker_script", "metered-model", Some(&scripts.token), None)
    ),
  ];
  let config = config_text_with_group(&root, &["main", "ext-1"], (GROUP, &[POOLED, METERED]), &options)?;
  let mut registry = scripted_registry(&state)?;
  // The pool_member session slot, which the server registers at startup: a pool switch writes it.
  registry.register_package("features.model_pool", charliebot_agents::model_pool::register)?;
  let setup = Setup::new(&root, registry, &config)?;
  attach_availability(&setup, &root)?;
  setup.features.startup.insert(RotationPositions::default())?;
  let session = setup
    .features
    .store
    .create_task(
      CreateTask {
        request_id: "model-group-switch".to_string(),
        task_parent_id: None,
        profile: TaskProfile::Worker,
        task: Some(TaskSpec {
          goal: "synthetic group switch".to_string(),
          repo_path: None,
          base_branch: None,
          task_type: Some(TaskType::Implement),
          keep_worktree: false,
          cold_read: None,
          approver: TaskApprover::User,
          improve_host: None,
        }),
        name: Some("Model group switch".to_string()),
        backend: Some(GROUP.to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::System,
      Map::new(),
    )
    .await?;
  let meta = setup.features.store.load_meta(&session.id).await?.context("the session metadata loads")?;
  let run: RunRecord = serde_json::from_value(json!({"id": "group-run", "session_id": session.id, "backend": POOLED}))?;
  let failure = QuotaFailure {
    resets_at: None,
    message: "You've hit your 5-hour limit".to_string(),
    reason: Some(backend_availability::REASON_QUOTA_ERROR.to_string()),
  };

  // One login is rejected and the other still has headroom: the group keeps its Claude member and writes no row.
  claude_accounts::observe_rate_limit(&state, "main", &rate_limit_info("rejected", 1.0), None, Some("claude-fable-5"));
  let kept = auto_switch::switch_member(
    &setup.features,
    GROUP,
    &meta,
    HandoffSource::WorkerRun { run: &run },
    POOLED,
    &failure,
    &[],
  )
  .await?;
  assert_eq!(kept, MemberSwitch::Moved { member: POOLED.to_string() }, "a login with headroom keeps the Claude member");
  let availability = backend_availability::backend_availability(&setup.features)?;
  assert!(availability.row(POOLED).is_none(), "a login can still run the model, so it writes no row");

  // Every login is out: the model's row runs to the earliest reset and the group takes its metered member.
  let earliest_epoch = (OffsetDateTime::now_utc() + TimeDuration::hours(2)).unix_timestamp();
  claude_accounts::observe_rate_limit(
    &state,
    "ext-1",
    &rate_limit_resetting("rejected", 1.0, earliest_epoch)["rate_limit_info"].clone(),
    None,
    Some("claude-fable-5"),
  );
  let moved = auto_switch::switch_member(
    &setup.features,
    GROUP,
    &meta,
    HandoffSource::WorkerRun { run: &run },
    POOLED,
    &failure,
    &[],
  )
  .await?;
  assert_eq!(
    moved,
    MemberSwitch::Moved { member: METERED.to_string() },
    "a spent login pool moves to the metered member"
  );
  let earliest = OffsetDateTime::from_unix_timestamp(earliest_epoch)?;
  assert_eq!(availability.out_until(POOLED), Some(earliest), "the row runs to the login pool's earliest reset");
  let events = setup.features.store.load_chat_events_sync(&session.id)?;
  let change = events
    .iter()
    .map(ChatEvent::to_value)
    .find(|event| event["type"] == charliebot_core::event_types::QUOTA_FAILOVER && event["to"] == METERED)
    .context("the switch records its member change")?;
  assert_eq!(change["from"], POOLED);
  assert_eq!(change["set"], GROUP);
  assert_eq!(
    change[charliebot_core::event_types::QUOTA_FAILOVER_RECOVERS_AT],
    availability.out_until_minutes(POOLED).context("the written row states its reset")?
  );
  Ok(())
}

// ---------------------------------------------------------------------------
// A re-attached manager turn rejected on one login continues on another
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "integration"]
async fn test_a_reattached_manager_turn_rejected_on_main_relays_to_ext1_and_completes() -> anyhow::Result<()> {
  let root = Root::new()?;
  make_transcript(&root.claude("main"), CC_ID)?;
  let state = Arc::new(ClaudeAccountState::new());
  // The resume builds a translator for the follow and one for the exit scan; the third build is the relayed turn,
  // the only one that runs.
  let scripts = Scripts::new(vec![String::new(), String::new(), exits(&[assistant_text("done"), result()], 0)]);
  let config = pooled_config(&root, &["main", "ext-1"], &[], &scripts.token)?;
  let setup = Setup::new(&root, scripted_registry(&state)?, &config)?;
  attach_availability(&setup, &root)?;
  // A manager turn's launch reads the session usage the server inserts at startup.
  setup.features.startup.insert(charliebot_usage::SessionUsage::new(
    Arc::clone(&setup.features.config),
    Arc::clone(&setup.features.store),
    Arc::clone(&setup.features.registry),
  ))?;
  setup.features.startup.insert(RotationPositions::default())?;
  let pool = Arc::new(BackendPool::new(
    backend_availability::backend_availability(&setup.features)?,
    Arc::clone(&setup.features.registry),
    setup.features.startup.require::<RotationPositions>()?,
  ));
  setup.features.startup.insert(Arc::clone(&pool))?;
  let queue = Arc::new(MasterQueue::new(pool));

  let session = setup
    .features
    .store
    .create_task(
      CreateTask {
        request_id: "manager-reattach-relay".to_string(),
        task_parent_id: None,
        profile: TaskProfile::Manager,
        task: None,
        name: Some("Manager re-attach relay".to_string()),
        backend: Some(POOLED.to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::System,
      Map::new(),
    )
    .await?;
  // A first turn cut before its finish persists no conversation id: the re-attach recovers it from the run's log.
  let meta = setup.features.store.load_meta(&session.id).await?.context("the session metadata loads")?;
  // The held process wrote its init and rejection to the raw log and exited, so the follow reads the log to its end.
  let run_id = "manager-reattach-run";
  let transport_dir = setup.cfg.sessions_dir().join(&session.id).join("runs").join(run_id);
  fs::create_dir_all(&transport_dir)?;
  let raw_log = transport_dir.join(RAW_LOG_NAME);
  let init = json!({"type": "system", "subtype": "init", "session_id": CC_ID});
  let mut raw = String::new();
  for event in [init, assistant_text("working"), rate_limit("rejected", 1.0), rejected_result()] {
    raw.push_str(&format!("{}\n", serde_json::to_string(&event)?));
  }
  fs::write(&raw_log, raw)?;
  let callbacks = queue.session_callbacks(Arc::clone(&setup.features), Arc::new(|_| Ok((false, String::new()))));
  let receiver = queue
    .enqueue_master_resume(MasterResumeRequest {
      cfg: Arc::new(setup.cfg.clone()),
      session_meta: meta,
      record: MasterResumeRecord {
        pid: None,
        pid_start: None,
        started_at: UtcDateTime::now(),
        raw_log,
        user_event_ids: Vec::new(),
      },
      callbacks,
      is_alive: Arc::new(|| false),
      task_run: TaskRunBinding {
        session_id: session.id.clone(),
        run_id: run_id.to_string(),
        transport_dir,
        fresh_native_context: false,
      },
      on_task_spawn: Arc::new(|_pid, _pid_start| Box::pin(async { Ok(()) })),
      on_task_finish: Arc::new(|_native_id, _exit_code, _extras, _done_event| Box::pin(async { Ok(false) })),
      extra_env: None,
    })
    .await?;
  receiver.await.context("the resume consumer closed the turn result")??;
  queue.shutdown().await?;

  let builds = scripts.builds();
  assert_eq!(builds.len(), 3);
  assert_eq!(account_label(&builds[2]), Some("ext-1"), "the rejected turn relays to the login with headroom");
  assert_eq!(&builds[2].args.extra_flags[..2], ["--resume", CC_ID], "the relay continues the same conversation");
  Ok(())
}

// ---------------------------------------------------------------------------
// The build contract
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

#[tokio::test]
#[ignore = "integration"]
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

#[tokio::test]
#[ignore = "integration"]
async fn test_claude_resume_launch_still_passes_the_run_instructions_file() -> anyhow::Result<()> {
  let root = Root::new()?;
  let mut registry = Registry::new();
  registry.register_package("test.claude_code", claude_code::register)?;
  let config = config_text(&root, &[], &[], &[option_yaml("fable", "cc-claude", None, None)])?;
  let setup = Setup::new(&root, registry, &config)?;
  let mut spec = setup.spec(&root, Some("fable"), None)?;
  spec.instructions_content = Some("rules".to_string());
  let task_worker = setup.worker(spec)?;
  // The lifecycle places the run id in a task launch's kwargs, and the worker's compaction hook needs it.
  let launch = Launch {
    backend_kwargs: serde_json::json!({"run_id": "resume-run"}).as_object().unwrap().clone(),
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
// The hang diagnostics
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "integration"]
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

  // The runner reports the signal that ended the process; no fake with status 143 is injected.
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

#[tokio::test]
#[ignore = "integration"]
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

#[tokio::test]
#[ignore = "integration"]
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
  // is only broadcast, so the thread channel carries it.
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
// The event line and the quota scan
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "integration"]
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

#[tokio::test]
#[ignore = "integration"]
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

#[tokio::test]
#[ignore = "integration"]
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
// The task build for a non-Claude backend
// ---------------------------------------------------------------------------

#[test]
#[ignore = "integration"]
fn anthropic_compatible_option_requires_a_resolved_model_and_keeps_its_endpoint() -> anyhow::Result<()> {
  let root = Root::new()?;
  fs::create_dir_all(root.home())?;
  let option = option_yaml("anthro", "cc-anthropic-compatible", None, None);
  fs::write(root.home().join("config.yaml"), config_text(&root, &[], &[], &[option])?)?;
  let mut registry = Registry::new();
  registry.register_package("backends.anthropic_compatible", anthropic_compatible::register)?;
  let config = registry.load_config(&root.home())?;

  let refused = charliebot_tasks::spawner_backends::resolve_backend_option(&config, "anthro", Some(""));
  assert_eq!(refused.unwrap_err().to_string(), "resolved model is required");

  let option = charliebot_tasks::spawner_backends::resolve_backend_option(&config, "anthro", Some("resolved-model"))?;

  assert_eq!(option.model, "resolved-model");
  assert_eq!(option.type_fields["api_base"].as_str(), Some("https://api.synthetic.example"));
  assert_eq!(option.type_fields["context_window"].as_u64(), Some(409_600));
  Ok(())
}

#[tokio::test]
#[ignore = "integration"]
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

// ---------------------------------------------------------------------------
// Devin rate-limit retries
// ---------------------------------------------------------------------------

/// The fake `devin` CLI and the export fixture its staged successful response reuses.
const FAKE_DEVIN: &str = include_str!("../../../backends/devin/tests/fixtures/fake_devin.sh");
const DEVIN_EXPORT: &str = include_str!("../../../backends/devin/tests/fixtures/export.json");
/// The recorded incident: devin's `cognition.ai/retryable` rate-limit rejection with a 22-second reset.
const DEVIN_INCIDENT: &str = r#"Error: Agent error: Reached free model rate limit. Upgrade to Max for higher limits, or switch to a different model. Your limit will reset in 22 seconds (at 01:19 UTC).: {
  "cognition.ai/errorKind": "unavailable",
  "cognition.ai/retryable": true
}"#;

thread_local! {
  /// The fake Devin host of the next `test.devin` registration. The test sets it just before registering, on the
  /// thread that registers.
  static DEVIN_HOST: RefCell<Option<CliHost>> = const { RefCell::new(None) };
}

fn register_test_devin(registry: &mut Registry) -> Result<(), RegistryError> {
  let host =
    DEVIN_HOST.with(|cell| cell.borrow_mut().take()).expect("the test sets the fake Devin host before registering");
  charliebot_devin::register_with_host(registry, host)
}

/// The fake `devin` binary in `dir`, with its staged-response contract.
fn stage_fake_devin(dir: &Path) -> anyhow::Result<()> {
  use std::os::unix::fs::PermissionsExt;
  fs::create_dir_all(dir)?;
  let binary = dir.join("devin");
  fs::write(&binary, FAKE_DEVIN)?;
  let mut permissions = fs::metadata(&binary)?.permissions();
  permissions.set_mode(0o755);
  fs::set_permissions(binary, permissions)?;
  Ok(())
}

/// Stages `responses/<call>/` of the fake devin: its stderr, its exit code and, on `succeeded`, its export and answer.
fn stage_devin_response(dir: &Path, call: usize, stderr: &str, exit_code: i32, succeeded: bool) -> anyhow::Result<()> {
  let response = dir.join("responses").join(call.to_string());
  fs::create_dir_all(&response)?;
  fs::write(response.join("stderr.txt"), stderr)?;
  fs::write(response.join("exit_code"), exit_code.to_string())?;
  if succeeded {
    fs::write(response.join("export.json"), DEVIN_EXPORT)?;
    fs::write(response.join("answer.txt"), "The number is 41.\n")?;
  }
  Ok(())
}

/// The helper binary that supervises a final-stdout transport run in tests.
fn fake_program_bin() -> PathBuf {
  let exe = std::env::current_exe().expect("the test binary path");
  exe
    .ancestors()
    .skip(1)
    .map(|dir| dir.join("fake_program"))
    .find(|candidate| candidate.is_file())
    .expect("cargo builds the fake-program helper into the target directory that holds the test binary")
}

/// A Setup whose registry serves the fake devin in `fake_bin` as the `devin` type and its only option.
fn devin_setup(root: &Root, fake_bin: &Path) -> anyhow::Result<Setup> {
  // SAFETY: this test binary races on nothing else that reads the variable.
  unsafe { std::env::set_var("CHARLIEBOT_TEST_FINAL_STDOUT_SUPERVISOR", fake_program_bin()) };
  DEVIN_HOST
    .with(|cell| *cell.borrow_mut() = Some(CliHost { path: fake_bin.display().to_string(), home: root.home() }));
  let mut registry = Registry::new();
  registry.register_package("test.devin", register_test_devin)?;
  let config = config_text(root, &[], &[], &[option_yaml("devin", "devin", None, None)])?;
  Setup::new(root, registry, &config)
}

/// The fake devin's call count after the run, read from `call-count`.
fn devin_call_count(fake_bin: &Path) -> anyhow::Result<String> {
  Ok(fs::read_to_string(fake_bin.join("call-count"))?.trim().to_string())
}

/// A worker run on devin waits out the backend's stated reset and reruns on the same backend. The pinned and
/// pool-chosen runs share this path: the worker only knows its `backend_option`.
#[tokio::test(start_paused = true)]
#[ignore = "integration"]
async fn a_devin_worker_retries_the_stated_rate_limit_reset_in_place() -> anyhow::Result<()> {
  let root = Root::new()?;
  let fake_bin = root.path().join("fake-bin");
  stage_fake_devin(&fake_bin)?;
  stage_devin_response(&fake_bin, 1, DEVIN_INCIDENT, 1, false)?;
  stage_devin_response(&fake_bin, 2, "", 0, true)?;
  let setup = devin_setup(&root, &fake_bin)?;
  let task_worker = setup.worker(setup.spec(&root, Some("devin"), None)?)?;

  assert_eq!(task_worker.run().await?, 0);
  assert_eq!(devin_call_count(&fake_bin)?, "2");
  Ok(())
}

/// After the initial run and its three retries all hit the rate limit, the worker error carries the original text
/// with the retry note. It also carries the reset the error stated.
#[tokio::test(start_paused = true)]
#[ignore = "integration"]
async fn a_devin_worker_reports_the_rate_limit_after_three_retries() -> anyhow::Result<()> {
  let root = Root::new()?;
  let fake_bin = root.path().join("fake-bin");
  stage_fake_devin(&fake_bin)?;
  for call in 1..=4 {
    stage_devin_response(&fake_bin, call, DEVIN_INCIDENT, 1, false)?;
  }
  let setup = devin_setup(&root, &fake_bin)?;
  let task_worker = setup.worker(setup.spec(&root, Some("devin"), None)?)?;
  let started = OffsetDateTime::now_utc();

  let error = task_worker.run().await.expect_err("the run ends in the rate-limit failure");

  let quota = error.downcast_ref::<QuotaExhaustedError>().expect("the run failure keeps its quota error");
  let remaining = quota.resets_at.expect("the parsed reset reaches the cooldown") - started;
  assert!(remaining > TimeDuration::seconds(10) && remaining < TimeDuration::minutes(5), "{remaining}");
  let message = quota.to_string();
  assert!(message.contains("reset in 22 seconds"), "{message}");
  assert!(message.contains("automatic retries ran out after 3 attempts"), "{message}");
  assert_eq!(devin_call_count(&fake_bin)?, "4");
  Ok(())
}

/// A reset beyond the five-minute wait limit is not waited out, but it still reaches the error's `resets_at`. The
/// pool therefore cools at the reset the backend stated rather than the thirty-minute default.
#[tokio::test(start_paused = true)]
#[ignore = "integration"]
async fn a_devin_worker_keeps_a_reset_over_the_wait_limit_for_the_cooldown() -> anyhow::Result<()> {
  let root = Root::new()?;
  let fake_bin = root.path().join("fake-bin");
  stage_fake_devin(&fake_bin)?;
  stage_devin_response(&fake_bin, 1, &DEVIN_INCIDENT.replace("22 seconds", "600 seconds"), 1, false)?;
  let setup = devin_setup(&root, &fake_bin)?;
  let task_worker = setup.worker(setup.spec(&root, Some("devin"), None)?)?;
  let started = OffsetDateTime::now_utc();

  let error = task_worker.run().await.expect_err("the run ends in the rate-limit failure");

  let quota = error.downcast_ref::<QuotaExhaustedError>().expect("the run failure keeps its quota error");
  let remaining = quota.resets_at.expect("the parsed reset reaches the cooldown") - started;
  assert!(remaining > TimeDuration::minutes(9) && remaining < TimeDuration::minutes(11), "{remaining}");
  assert!(!quota.to_string().contains("automatic retries ran out"), "{quota}");
  assert_eq!(devin_call_count(&fake_bin)?, "1");
  Ok(())
}
