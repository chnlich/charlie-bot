//! Backend hooks of the agent runtime (`src/runtime/hooks/backend_types.py`, `backend_type_registration.py`,
//! `backend_lifecycle.py` and `backend_lifecycle_registration.py`): the backend types and their traits, the factory
//! that builds a process runner per launch, the lifecycle that plans each run's processes, and the child-environment,
//! reading-limit and usage-resolver registries.
//!
//! This module holds the contracts only. The agent-runtime crates implement them: the process runner lives in
//! `charliebot-agents`, and it calls these traits.

use std::any::Any;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::bail;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::hot_reload::HotReload;
use charliebot_store::metadata::{SessionMetadata, ThreadMetadata, UtcDateTime};
use charliebot_store::run_records::RunKind;
use charliebot_store::{SharedChatEvent, Store};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use tokio::io::AsyncWrite;

use crate::registry::{Registry, RegistryError};

/// How a backend type resumes its conversation: a CLI flag, or the backend's own session id in its constructor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeStyle {
  CliFlag,
  NativeId,
}

/// The behaviors that runtime code reads by backend type (`backend_type_registration.BackendTraits`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackendTraits {
  pub resume: ResumeStyle,
  /// True when a restarted server can follow the process of an interrupted run.
  pub restart_reattach: bool,
  /// True when the runtime chooses the session id of a task run before the process starts.
  pub preassigned_session_id: bool,
  /// The id prefix that names the type's family of backend ids; `None` when the type has no family.
  pub family_prefix: Option<&'static str>,
  /// True when the backend runs a `/compact` chat message as a manual compaction.
  pub manual_compact: bool,
  /// The compact button's title on a backend without manual compaction; `None` takes the page's generic title.
  pub compact_hint: Option<&'static str>,
}

/// Validates one `backends.options[]` entry's type-specific keys (the pydantic option model of Python).
pub type OptionModel = fn(&serde_yaml::Mapping) -> Result<(), String>;

/// One backend type (`backend_type_registration.register_backend_type`). `translate_fallback` marks the one type that
/// serves the binary-free translate fallback; its factory then also accepts no option.
pub struct BackendTypeRegistration {
  pub type_name: &'static str,
  pub traits: BackendTraits,
  pub option_model: OptionModel,
  pub factory: Arc<dyn BackendFactory>,
  /// `None` gives the default lifecycle: one process per run.
  pub lifecycle: Option<Arc<dyn BackendLifecycle>>,
  pub translate_fallback: bool,
}

/// The environment of an agent child process.
pub type Env = BTreeMap<String, String>;

/// Edits one agent child process's environment in place (`backend_lifecycle_registration.register_child_env`).
pub type ChildEnvFn = fn(&mut Env);

/// The context limits of one reading kind. `declared_window` is `None` when the reading carries the window itself;
/// `compact_reserve` is `None` when the compaction point is unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextLimits {
  pub declared_window: Option<u64>,
  pub compact_reserve: Option<u64>,
}

/// Computes the limits of one reading kind (`backend_lifecycle_registration.register_reading_limits`).
pub type ReadingLimitsFn = fn(RunKind) -> ContextLimits;

/// Computes a reading kind's limits from one backend option (`register_backend_reading_limits`).
pub type BackendReadingLimitsFn = fn(&BackendOption, RunKind) -> ContextLimits;

/// One context-limits provider registered for a backend type and reading kind.
pub(crate) struct BackendReadingLimitsRegistration {
  pub backend_type: &'static str,
  pub reading_kind: &'static str,
  pub package: &'static str,
  pub limits: BackendReadingLimitsFn,
}

/// Reads one backend type's usage from its session events (`register_usage_resolver`). A resolver returns the usage
/// object for the session, or `None`.
pub trait UsageResolver: Send + Sync {
  fn resolve(
    &self,
    session_id: &str,
    meta: &SessionMetadata,
    events: &[SharedChatEvent],
  ) -> anyhow::Result<Option<Map<String, Value>>>;
}

/// The server values a backend usage-resolver constructor can retain.
pub struct UsageResolverHost {
  pub config: Arc<HotReload<CharlieBotConfig>>,
  pub store: Arc<Store>,
}

/// Constructs a backend type's usage resolver for one usage owner.
pub type UsageResolverConstructor = Arc<dyn Fn(&UsageResolverHost) -> Arc<dyn UsageResolver> + Send + Sync>;

/// The keyword arguments the runtime passes to a backend factory (`base.py` launch arguments and the per-type
/// launch arguments).
#[derive(Clone, Debug, Default)]
pub struct LaunchArgs {
  pub extra_flags: Vec<String>,
  pub buffer_limit: Option<usize>,
  pub instructions_content: Option<String>,
  pub instructions_file: Option<PathBuf>,
  pub resume_session_id: Option<String>,
  pub log_dir: Option<PathBuf>,
  pub cgroup_session_id: Option<String>,
  /// The per-process arguments of the launch plan, such as `claude_session_id` or the account directory.
  pub backend_kwargs: Map<String, Value>,
}

/// Builds the per-process backend of one launch (`factory(option, cfg, **launch_kwargs)`).
pub trait BackendFactory: Send + Sync {
  /// `option` is `None` only for the translate-fallback type.
  fn build(
    &self,
    option: Option<&BackendOption>,
    cfg: &CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>>;
}

/// How the runner reads a process's output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportKind {
  /// The process writes a raw event log.
  RawLog,
  /// The process writes its answer to stdout and exits; the runner reads the whole output after the exit.
  FinalStdout,
  /// The process serves over a local protocol (opencode).
  Serve,
}

/// What a final-stdout backend makes of its finished process (`antigravity_cli.py` `run`). The runner emits `events`,
/// records `exit_code`, and then fails the run with `guard_error` when it is set.
pub struct FinalStdoutOutcome {
  pub events: Vec<Value>,
  pub exit_code: i32,
  pub guard_error: Option<String>,
}

/// Maps one raw event of a backend's output to the runtime's events.
pub trait Translate: Send {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>>;
}

/// Whether the spawned child has exited, read by the mid-turn delivery path before it writes.
pub type ExitedProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// The per-process pieces of the backend template method. The runner in `charliebot-agents` drives them.
pub trait AgentBackend: Send {
  /// The instructions file name and the log event that reports writing it, or `None` when the backend takes none.
  fn instructions_target(&self) -> Option<(&'static str, &'static str)>;
  fn prepare_transport(&mut self, log_dir: &Path) -> anyhow::Result<()>;
  fn build_command(&self, prompt: &str) -> anyhow::Result<Vec<String>>;
  fn prepare_env(&self, env: Env) -> anyhow::Result<Env>;
  fn stdin_prompt(&self, prompt: &str) -> Option<String>;
  /// The run id whose stdin this backend keeps open for mid-turn delivery (`midturn_delivery`), or `None` for the
  /// plain-text path.
  fn midturn_run_id(&self) -> Option<String> {
    None
  }
  /// Write the run's first stdin payload and keep its stdin open for mid-turn writes (`base.py` `_write_stdin_prompt`).
  /// `exited` reports the child's exit. The runner writes `stdin_prompt` and closes stdin when this returns `None`.
  fn write_stdin_prompt(
    &self,
    _stdin: Box<dyn AsyncWrite + Send + Unpin>,
    _exited: ExitedProbe,
    _prompt: &str,
  ) -> Option<BoxFuture<'static, anyhow::Result<()>>> {
    None
  }
  /// Handle one translated event before the run's consumer sees it (`base.py` `_on_event`), or `None`.
  fn on_event<'a>(&'a self, _event: &'a Value) -> Option<BoxFuture<'a, anyhow::Result<()>>> {
    None
  }
  fn translator(&self) -> Box<dyn Translate>;
  fn transport(&self) -> TransportKind;
  /// Checks the run's uploaded files before any process starts (`base.py` `run` ignores them). `Err` ends the run
  /// with exactly one error event and no process.
  fn accept_uploads(&mut self, _uploaded_files: &[Value]) -> Result<(), String> {
    Ok(())
  }
  /// Reads the output of a `FinalStdout` process once it has exited. Only a backend whose transport is `FinalStdout`
  /// implements it; the runner calls it for no other.
  fn finish_final_stdout(&mut self, _stdout: &str, _stderr: &str, _exit_code: i32) -> FinalStdoutOutcome {
    panic!("finish_final_stdout is called only for a FinalStdout transport")
  }
  /// The CLI-native one-shot, tool-less, or `None` when the backend has none. The dispatcher
  /// `charliebot_agents::one_shot_text` runs the tool-capable one-shot for `None`.
  fn one_shot_text<'a>(
    &'a mut self,
    _base_env: &'a Env,
    _prompt: &'a str,
    _system: &'a str,
    _timeout: Option<Duration>,
  ) -> Option<BoxFuture<'a, anyhow::Result<String>>> {
    None
  }
}

/// A launch refused by the backend: the run ends with `message`. `quota_exhausted` says a spent quota caused it, and
/// `resets_at` names when that quota returns, when the backend says.
#[derive(Clone, Debug)]
pub struct LaunchRefused {
  pub message: String,
  pub quota_exhausted: bool,
  pub resets_at: Option<UtcDateTime>,
}

impl std::fmt::Display for LaunchRefused {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.message)
  }
}

impl std::error::Error for LaunchRefused {}

/// Which run a launch belongs to: a master turn or a task run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchKind {
  Turn,
  Task,
}

/// Writes one event for the user. On a turn it persists and broadcasts the event; on a task it writes the run's log.
pub type EmitFn = Arc<dyn Fn(Value) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;
/// Persists the account label on the session.
pub type RecordAccountFn = Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;
/// Reads the metadata of every other session with a running manager turn.
pub type RunningTurnsFn = Arc<dyn Fn() -> Vec<SessionMetadata> + Send + Sync>;
/// The session's context tokens and the time of its last request.
pub type ContextStateFn =
  Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<(Option<i64>, Option<UtcDateTime>)>> + Send + Sync>;

/// One compaction process a backend runs (`claude_compaction.CompactionRun`): its command line, the account
/// directory it reads and the chat event triggers of its outcome.
pub struct CompactionRun<'a> {
  pub argv: Vec<String>,
  pub env: Env,
  pub cwd: &'a Path,
  pub config_dir: &'a Path,
  pub session_id: &'a str,
  pub family: &'a str,
  pub model: &'a str,
  pub trigger: &'a str,
  pub failure_trigger: Option<&'a str>,
  pub pre_tokens: Option<i64>,
  pub emit: &'a EmitFn,
  pub cfg: &'a CharlieBotConfig,
  pub session_cgroup_id: Option<&'a str>,
  pub account_label: &'a str,
  pub timeout_duration: Duration,
}

/// What one compaction process did (`claude_compaction.CompactionOutcome`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionOutcome {
  pub ok: bool,
  pub error: Option<String>,
  pub models: Vec<String>,
  pub post_tokens: Option<i64>,
  pub cache_read_tokens: Option<i64>,
}

/// Runs one compaction process (`claude_compaction.run_compaction` in the Claude Code backend).
pub type CompactionRunFn =
  Arc<dyn for<'a> Fn(CompactionRun<'a>) -> BoxFuture<'a, anyhow::Result<CompactionOutcome>> + Send + Sync>;

/// What the runtime gives a lifecycle for one run.
pub struct LaunchContext {
  pub cfg: Arc<CharlieBotConfig>,
  pub option: BackendOption,
  /// Shared turn metadata, updated when the run records a new Claude account label.
  pub session_meta: Arc<Mutex<SessionMetadata>>,
  pub kind: LaunchKind,
  pub run_kind: RunKind,
  pub cwd: PathBuf,
  /// The base agent environment owned by the startup state.
  pub base_env: Arc<Env>,
  /// The conversation this run may resume. `None` starts fresh.
  pub held_native_id: Option<String>,
  /// For a task run, the conversation id the runtime chose for its first process.
  pub preassigned_native_id: Option<String>,
  pub emit: EmitFn,
  pub record_account: RecordAccountFn,
  pub context_state: ContextStateFn,
  /// Present for manager turns; task launches carry `None`.
  pub running_turns: Option<RunningTurnsFn>,
  /// The task-tree Run id, when the turn belongs to a Run (`backend_lifecycle.LaunchContext.run_id`).
  pub run_id: Option<String>,
}

/// The plan for one process of a run: the factory arguments, the conversation to resume, and after a relay the
/// prompt to send.
pub struct Launch {
  pub backend_kwargs: Map<String, Value>,
  pub resume_id: Option<String>,
  /// Replaces the run's prompt when set. A relay sets the continuation prompt.
  pub prompt: Option<String>,
  /// The login that the run's log lines name.
  pub account_label: Option<String>,
  /// Lifecycle-private state, such as a relay count. The runtime does not read it.
  pub private: Option<Arc<dyn Any + Send + Sync>>,
}

/// Folds one process's events and exit into the decision to start a next process.
pub trait LaunchWatch: Send {
  /// Records one event. `true` stops the process now, at a safe relay point.
  fn observe(&mut self, event: &Value) -> bool;
  /// `true`: the run continues in a next process, which `next_launch` plans.
  fn wants_next(&self, exit_code: i32, stderr: &str) -> bool;
}

/// A backend type's lifecycle (`backend_lifecycle.BackendLifecycle`). The defaults serve a type with no login pool:
/// one process per run, its conversation resumed as held.
pub trait BackendLifecycle: Send + Sync {
  /// The domain in which backend options continue each other's conversation. One option is its own domain.
  fn continuation_domain(&self, option: &BackendOption, _cfg: &CharlieBotConfig) -> String {
    option.id.clone()
  }

  /// Plans the first process of the run, or refuses it.
  fn place<'a>(&'a self, ctx: &'a LaunchContext) -> BoxFuture<'a, Result<Launch, LaunchRefused>> {
    Box::pin(async move {
      Ok(Launch {
        backend_kwargs: Map::new(),
        resume_id: ctx.held_native_id.clone(),
        prompt: None,
        account_label: None,
        private: None,
      })
    })
  }

  /// The watch of the process that `launch` plans, or `None` for no relay.
  fn watch(&self, _ctx: &LaunchContext, _launch: &Launch) -> Option<Box<dyn LaunchWatch>> {
    None
  }

  /// Plans the next process after `launch`, or refuses the relay. Only a lifecycle whose watch asks for relays
  /// implements it; the default is never called, because its watch is `None`.
  fn next_launch<'a>(
    &'a self,
    _ctx: &'a LaunchContext,
    _launch: &'a Launch,
    _native_id: Option<&'a str>,
  ) -> BoxFuture<'a, Result<Launch, LaunchRefused>> {
    Box::pin(async { panic!("next_launch is called only for a lifecycle whose watch asks for a relay") })
  }

  /// Cleans up after the last process of a round, whether the round succeeded or not.
  fn after_round<'a>(
    &'a self,
    _ctx: &'a LaunchContext,
    _native_id: Option<&'a str>,
    _succeeded: bool,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  /// The launch arguments that turn on this backend's mid-turn input path for the turn, or an empty map when the
  /// backend keeps stdin closed (`midturn_delivery.launch_kwargs`).
  fn midturn_launch_kwargs(
    &self,
    _meta: &SessionMetadata,
    _turn: bool,
    _run_kind: RunKind,
    _run_id: Option<&str>,
  ) -> anyhow::Result<Map<String, Value>> {
    Ok(Map::new())
  }

  /// The notice events for the user that a finished round's events call for.
  fn round_notices(&self, _option: &BackendOption, _events: &[Value]) -> Vec<Value> {
    Vec::new()
  }

  /// True when `round_notices` can return notices (`launch_loop.py`).
  fn has_round_notices(&self) -> bool {
    false
  }

  /// The name that logs and error events give the account label; empty when the backend keeps none.
  fn account_source(&self) -> &'static str {
    ""
  }

  /// The same name as a noun phrase in a message.
  fn account_subject(&self) -> &'static str {
    ""
  }

  /// The login whose transcript store holds the session's conversation; `None` when unrecorded.
  fn account_label(&self, _meta: &SessionMetadata) -> Option<String> {
    None
  }

  /// Records `label` on `meta` in memory. Returns `true` when the recorded label changed.
  fn record_account_label(&self, _meta: &mut SessionMetadata, _label: Option<&str>) -> bool {
    false
  }

  /// The conversation id the runtime chose for the task before its first process started.
  fn thread_native_id(&self, _thread: &ThreadMetadata) -> Option<String> {
    None
  }

  /// Records on `thread` the conversation id the runtime chose for the task.
  fn assign_thread_native_id(&self, _thread: &mut ThreadMetadata, _native_id: Option<&str>) {}
}

/// The lifecycle of a backend type that registered none.
pub struct DefaultLifecycle;

impl BackendLifecycle for DefaultLifecycle {}

impl Registry {
  /// Register a child-process environment edit (`backend_lifecycle_registration.register_child_env`). The edits apply
  /// in registration order to every agent child process.
  pub fn child_env(&mut self, name: &'static str, edit: ChildEnvFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.child_envs.insert(package, name, edit)
  }

  /// Register the context limits of a reading kind (`register_reading_limits`).
  pub fn reading_limits(&mut self, kind: &'static str, limits: ReadingLimitsFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.reading_limits.insert(package, kind, limits)
  }

  /// Register context limits selected by backend type and reading kind.
  pub fn backend_reading_limits(
    &mut self,
    backend_type: &'static str,
    kind: &'static str,
    limits: BackendReadingLimitsFn,
  ) -> Result<(), RegistryError> {
    let package = self.owner()?;
    if let Some(existing) = self
      .backend_reading_limits
      .iter()
      .find(|registration| registration.backend_type == backend_type && registration.reading_kind == kind)
    {
      return Err(RegistryError {
        package,
        message: format!(
          "backend reading limits for ({backend_type:?}, {kind:?}) are already registered by {}",
          existing.package
        ),
      });
    }
    self.backend_reading_limits.push(BackendReadingLimitsRegistration {
      backend_type,
      reading_kind: kind,
      package,
      limits,
    });
    Ok(())
  }

  /// Register the usage resolver of a backend type (`register_usage_resolver`).
  pub fn usage_resolver(
    &mut self,
    backend_type: &'static str,
    constructor: UsageResolverConstructor,
  ) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.usage_resolvers.insert(package, backend_type, constructor)
  }

  /// The registration of `backend_type`, or `None` when no package registered it.
  pub fn backend_type_of(&self, backend_type: &str) -> Option<&BackendTypeRegistration> {
    self.backend_types.get(backend_type)
  }

  /// The lifecycle of `backend_type`: its registered lifecycle, or the default one. An unregistered type fails.
  pub fn lifecycle_for(&self, backend_type: &str) -> anyhow::Result<Arc<dyn BackendLifecycle>> {
    match self.backend_types.get(backend_type) {
      Some(registration) => Ok(registration.lifecycle.clone().unwrap_or_else(|| Arc::clone(&self.default_lifecycle))),
      None => bail!("Unknown backend type: {backend_type}"),
    }
  }

  /// Every registered lifecycle, each once, in registration order. Types without one are left out.
  pub fn lifecycles(&self) -> Vec<Arc<dyn BackendLifecycle>> {
    let mut found: Vec<Arc<dyn BackendLifecycle>> = Vec::new();
    for registration in self.backend_types.values() {
      if let Some(lifecycle) = &registration.lifecycle
        && !found.iter().any(|known| Arc::ptr_eq(known, lifecycle))
      {
        found.push(Arc::clone(lifecycle));
      }
    }
    found
  }

  /// The type whose family prefix starts `backend_id`, or `None`. It names the type of a backend id that config no
  /// longer defines.
  pub fn type_for_family_prefix(&self, backend_id: &str) -> Option<&'static str> {
    self.backend_types.values().find_map(|registration| {
      let prefix = registration.traits.family_prefix?;
      backend_id.starts_with(prefix).then_some(registration.type_name)
    })
  }

  /// The one type that serves the translate fallback. None registered is an error.
  pub fn translate_fallback(&self) -> anyhow::Result<&BackendTypeRegistration> {
    match self.backend_types.values().find(|registration| registration.translate_fallback) {
      Some(registration) => Ok(registration),
      None => bail!("no backend type serves the translate fallback"),
    }
  }

  /// Builds the backend of `option`'s type with its factory.
  pub fn build_backend(
    &self,
    option: &BackendOption,
    cfg: &CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let Some(registration) = self.backend_types.get(&option.backend_type) else {
      bail!("Unknown backend type: {}", option.backend_type);
    };
    registration.factory.build(Some(option), cfg, launch)
  }

  /// Applies every child-environment edit to `env`, in registration order.
  pub fn apply_child_env(&self, env: &mut Env) {
    for edit in self.child_envs.values() {
      edit(env);
    }
  }

  /// The context limits of `kind`, or `None` when no package registered the kind.
  /// A matching backend provider takes precedence when `option` names its type.
  pub fn reading_limits_for(
    &self,
    kind: &str,
    run_kind: RunKind,
    option: Option<&BackendOption>,
  ) -> Option<ContextLimits> {
    if let Some(option) = option
      && let Some(registration) = self
        .backend_reading_limits
        .iter()
        .find(|registration| registration.backend_type == option.backend_type && registration.reading_kind == kind)
    {
      return Some((registration.limits)(option, run_kind));
    }
    self.reading_limits.get(kind).map(|limits| limits(run_kind))
  }

  /// The usage-resolver constructor of `backend_type`, or `None` when the type has none.
  pub fn usage_resolver_for(&self, backend_type: &str) -> Option<UsageResolverConstructor> {
    self.usage_resolvers.get(backend_type).cloned()
  }
}
