//! The cc-claude command, environment, translator, and context reading.

use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, bail};
use charliebot_registry::backend::{
  AgentBackend, ContextLimits, Env, ExitedProbe, LaunchArgs, Translate, TransportKind,
};
use charliebot_store::run_records::RunKind;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::transport::cgroup::SessionCgroupConfig;
use crate::transport::kill::{kill_process_group, python_exit_code};
use crate::transport::spawn::{ChildStdin, SpawnSetup, SpawnSpec, spawn_with_setup};

use super::claude_launch;
use super::login_dirs;
use super::midturn_delivery::MidturnTranslate;

pub const HEADLESS_DISALLOWED_TOOLS: &str = concat!(
  "Monitor,ScheduleWakeup,CronCreate,CronDelete,CronList,Agent,Workflow,TaskCreate,",
  "TaskGet,TaskUpdate,TaskList,TaskStop,TaskOutput,SendMessage,ListAgents,ShareOnboardingGuide"
);
pub const DISALLOWED_TOOLS_FLAG: &str = "--disallowed-tools";
pub const SKIP_PERMISSIONS_FLAG: &str = "--dangerously-skip-permissions";
pub const INSTRUCTIONS_FILE_NAME: &str = "CLAUDE.md";
pub const INSTRUCTIONS_WRITTEN_EVENT: &str = "claude_code_wrote_claude_md";
pub const EXCLUDE_DYNAMIC_SECTIONS_FLAG: &str = "--exclude-dynamic-system-prompt-sections";
const SYNTHETIC_MODEL: &str = "<synthetic>";
const ONE_SHOT_DISALLOWED_TOOLS: &str = "Bash,Read,Write,Edit,Glob,Grep,Agent";

fn headless_disallowed_tools(run_kind: RunKind) -> String {
  let kind_tools = match run_kind {
    RunKind::Review => "Skill",
    RunKind::ManagerTurn | RunKind::Work | RunKind::Iteration | RunKind::ScheduledStep => "ReportFindings",
  };
  format!("{HEADLESS_DISALLOWED_TOOLS},{kind_tools}")
}

fn headless_command(run_kind: RunKind) -> Vec<String> {
  vec![
    "claude".to_string(),
    "-p".to_string(),
    "--output-format".to_string(),
    "stream-json".to_string(),
    "--verbose".to_string(),
    SKIP_PERMISSIONS_FLAG.to_string(),
    DISALLOWED_TOOLS_FLAG.to_string(),
    headless_disallowed_tools(run_kind),
  ]
}

static DECLARED_WINDOW_WARNINGS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Apply the common Claude child edits registered for every agent backend.
pub fn claude_child_env(env: &mut Env) {
  env.remove("CLAUDECODE");
  env.insert("CLAUDE_CODE_DISABLE_AUTO_MEMORY".to_string(), "1".to_string());
}

/// Build the inherited environment variables for a headless Claude process.
pub fn headless_claude_env(run_kind: RunKind, source: &Env) -> Env {
  claude_launch::headless_claude_env(run_kind, source)
}

/// Return Claude's declared window and the reserve used to derive its compact point.
pub fn claude_reading_limits(run_kind: RunKind) -> ContextLimits {
  let mut source = Env::new();
  for name in claude_launch::HEADLESS_FORWARDED_ENV {
    if let Ok(value) = env::var(name) {
      source.insert(name.to_string(), value);
    }
  }
  claude_reading_limits_from(run_kind, &source)
}

/// Return Claude's reading limits for a supplied host environment.
pub fn claude_reading_limits_from(run_kind: RunKind, source: &Env) -> ContextLimits {
  let env = headless_claude_env(run_kind, source);
  let raw_window =
    env.get(claude_launch::AUTO_COMPACT_WINDOW_ENV).expect("headless environment always carries the compact window");
  let window = match parse_python_positive_integer(raw_window) {
    Some(value) => value,
    None => {
      warn_unparseable_window(raw_window);
      claude_launch::declared_window_by_run_kind(run_kind)
    }
  };
  for name in [claude_launch::AUTOCOMPACT_PCT_OVERRIDE_ENV, claude_launch::MAX_CONTEXT_TOKENS_ENV] {
    if let Some(value) = env.get(name) {
      warn_degraded_window(name, value);
      return ContextLimits { declared_window: Some(window), compact_reserve: None };
    }
  }
  ContextLimits {
    declared_window: Some(window),
    compact_reserve: Some(claude_launch::COMPACT_OUTPUT_RESERVE + claude_launch::COMPACT_CONTEXT_RESERVE),
  }
}

fn parse_python_positive_integer(raw: &str) -> Option<u64> {
  let value = raw.trim();
  let (positive, digits) = match value.strip_prefix('+') {
    Some(digits) => (true, digits),
    None => match value.strip_prefix('-') {
      Some(digits) => (false, digits),
      None => (true, value),
    },
  };
  let characters = digits.chars().collect::<Vec<_>>();
  if characters.is_empty()
    || !characters.iter().enumerate().all(|(index, character)| {
      character.is_ascii_digit()
        || (*character == '_'
          && index > 0
          && index + 1 < characters.len()
          && characters[index - 1].is_ascii_digit()
          && characters[index + 1].is_ascii_digit())
    })
    || !positive
  {
    return None;
  }
  let normalized = characters.into_iter().filter(|character| *character != '_').collect::<String>();
  normalized.parse::<u64>().ok().filter(|number| *number > 0)
}

fn warn_unparseable_window(window: &str) {
  let key = format!("window:{window}");
  if remember_warning(key) {
    tracing::warn!(
      target: "charliebot_agents",
      variable = claude_launch::AUTO_COMPACT_WINDOW_ENV,
      window,
      "claude_declared_window_unparseable_window"
    );
  }
}

fn warn_degraded_window(variable: &str, value: &str) {
  let key = format!("degraded:{variable}:{value}");
  if remember_warning(key) {
    tracing::warn!(
      target: "charliebot_agents",
      variable,
      value,
      reason = "forwarded but semantics not modelled; returning declared window without compaction point",
      "claude_declared_window_degraded"
    );
  }
}

fn remember_warning(key: String) -> bool {
  DECLARED_WINDOW_WARNINGS
    .get_or_init(|| Mutex::new(HashSet::new()))
    .lock()
    .expect("declared window warning registry is not poisoned")
    .insert(key)
}

/// The per-process Claude Code CLI backend.
pub struct ClaudeCodeBackend {
  model: Option<String>,
  effort: Option<String>,
  cli_binary: Option<String>,
  fast_mode: bool,
  run_kind: RunKind,
  claude_session_id: Option<String>,
  claude_config_dir: Option<String>,
  cgroup_session_id: Option<String>,
  cgroup_config: SessionCgroupConfig,
  extra_flags: Vec<String>,
  instructions_file: Option<PathBuf>,
  midturn_delivery: bool,
  midturn_run_id: Option<String>,
}

impl ClaudeCodeBackend {
  pub(crate) fn from_launch(
    model: Option<String>,
    effort: Option<String>,
    cli_binary: Option<String>,
    fast_mode: bool,
    claude_config_dir: Option<String>,
    launch: LaunchArgs,
    cgroup_config: SessionCgroupConfig,
  ) -> anyhow::Result<Self> {
    let run_kind = match launch.backend_kwargs.get("run_kind") {
      None => RunKind::Work,
      Some(Value::String(_)) => serde_json::from_value(launch.backend_kwargs["run_kind"].clone())
        .context("backend_kwargs.run_kind must name a known run kind")?,
      Some(_) => bail!("backend_kwargs.run_kind must be a string"),
    };
    let claude_session_id = launch.backend_kwargs.get("claude_session_id");
    let claude_session_id = match claude_session_id {
      None | Some(Value::Null) => None,
      Some(Value::String(value)) => Some(value.clone()),
      Some(_) => bail!("claude_session_id must be a string"),
    }
    .filter(|value| !value.is_empty());
    let midturn_delivery = match launch.backend_kwargs.get("midturn_delivery") {
      None | Some(Value::Null) => false,
      Some(Value::Bool(value)) => *value,
      Some(_) => bail!("midturn_delivery must be a boolean"),
    };
    let midturn_run_id = match launch.backend_kwargs.get("midturn_run_id") {
      None | Some(Value::Null) => None,
      Some(Value::String(value)) => Some(value.clone()),
      Some(_) => bail!("midturn_run_id must be a string"),
    }
    .filter(|value| !value.is_empty());
    Ok(Self {
      model,
      effort: effort.filter(|value| !value.is_empty()),
      cli_binary: cli_binary.filter(|value| !value.is_empty()),
      fast_mode,
      run_kind,
      claude_session_id,
      claude_config_dir: claude_config_dir.filter(|value| !value.is_empty()),
      cgroup_session_id: launch.cgroup_session_id,
      cgroup_config,
      extra_flags: launch.extra_flags,
      instructions_file: launch.instructions_file,
      midturn_delivery,
      midturn_run_id,
    })
  }

  /// Build the Claude command used by the raw-log runner.
  pub fn build_command(&self, _prompt: &str) -> anyhow::Result<Vec<String>> {
    let mut command = headless_command(self.run_kind);
    command[0] = self.cli_binary.clone().unwrap_or_else(|| "claude".to_string());
    if let Some(session_id) = &self.claude_session_id {
      command.extend(["--session-id".to_string(), session_id.clone()]);
    }
    if let Some(model) = &self.model {
      command.extend(["--model".to_string(), model.clone()]);
    }
    if let Some(effort) = &self.effort {
      command.extend(["--effort".to_string(), effort.clone()]);
    }
    let mut settings = claude_launch::headless_claude_settings(self.run_kind, self.cgroup_session_id.as_deref())?;
    if self.fast_mode {
      settings.insert(claude_launch::FAST_MODE_SETTINGS_KEY.to_string(), Value::Bool(true));
    }
    command.extend(["--settings".to_string(), Value::Object(settings).to_string()]);
    command.extend(self.extra_flags.clone());
    if self.midturn_delivery {
      command.extend(["--input-format".to_string(), "stream-json".to_string(), "--replay-user-messages".to_string()]);
    }
    if let Some(path) = &self.instructions_file {
      command.extend(["--append-system-prompt-file".to_string(), path.to_string_lossy().into_owned()]);
    }
    Ok(command)
  }

  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    let headless = headless_claude_env(self.run_kind, &env);
    let mut env = without_gateway_env(&env);
    env.extend(headless);
    if let Some(config_dir) = &self.claude_config_dir {
      let home = env.get("HOME").map(String::as_str);
      env.insert(
        login_dirs::CLAUDE_CONFIG_DIR_ENV.to_string(),
        login_dirs::expanduser(config_dir, home)?.to_string_lossy().into_owned(),
      );
    }
    Ok(env)
  }

  fn one_shot_env(&self, base_env: &Env) -> Env {
    without_gateway_env(base_env)
  }

  /// The model-selection flags of this backend's one-shot: a cc-claude entry names its model with `--model`.
  fn one_shot_model_args(&self) -> Vec<String> {
    match self.model.as_deref().filter(|value| !value.is_empty()) {
      Some(model) => vec!["--model".to_string(), model.to_string()],
      None => Vec::new(),
    }
  }

  fn one_shot_command(&self, system: &str) -> Vec<String> {
    one_shot_argv(&self.one_shot_model_args(), system)
  }

  async fn run_one_shot(
    &self,
    base_env: &Env,
    prompt: &str,
    system: &str,
    timeout: Option<Duration>,
  ) -> anyhow::Result<String> {
    let command = self.one_shot_command(system);
    let env = self.one_shot_env(base_env);
    run_one_shot_process(&command, &env, prompt, timeout, self.cgroup_session_id.as_deref(), &self.cgroup_config).await
  }
}

/// Run a `claude -p` text one-shot to completion and return its trimmed stdout. The process group is killed on
/// timeout, and a non-zero exit reports the child's stderr.
async fn run_one_shot_process(
  command: &[String],
  env: &Env,
  prompt: &str,
  timeout: Option<Duration>,
  cgroup_session_id: Option<&str>,
  cgroup_config: &SessionCgroupConfig,
) -> anyhow::Result<String> {
  let spec = SpawnSpec::new(
    &command[0],
    command[1..].iter().map(OsString::from).collect(),
    None,
    env.iter().map(|(name, value)| (OsString::from(name), OsString::from(value))).collect::<HashMap<_, _>>(),
    cgroup_session_id.map(str::to_string),
    cgroup_config.clone(),
  );
  let mut child = spawn_with_setup(&spec, None, ChildStdin::Piped, SpawnSetup::ChildSideTurnTree).await?.child;
  let pid = child.id().context("claude CLI did not expose its process id")?;
  let mut stdin = child.stdin.take().context("claude CLI stdin was not piped")?;
  let prompt = prompt.as_bytes().to_vec();
  let stdin_task = tokio::spawn(async move {
    stdin.write_all(&prompt).await?;
    stdin.shutdown().await
  });
  let output_future = child.wait_with_output();
  tokio::pin!(output_future);
  let output = match timeout {
    Some(limit) => match tokio::time::timeout(limit, &mut output_future).await {
      Ok(result) => result?,
      Err(error) => {
        kill_process_group(pid, nix::sys::signal::Signal::SIGKILL);
        let _ = output_future.await;
        stdin_task.abort();
        let _ = stdin_task.await;
        return Err(error.into());
      }
    },
    None => output_future.await?,
  };
  stdin_task.await.context("waiting for claude CLI stdin writer failed")??;
  let exit_code = python_exit_code(output.status);
  if exit_code != 0 {
    let stderr = String::from_utf8(output.stderr)?;
    bail!("claude CLI failed (exit {exit_code}): {}", stderr.trim());
  }
  Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// The argv of a `claude -p` text one-shot: no session persistence, tools disabled, and the caller's model flags.
fn one_shot_argv(model_args: &[String], system: &str) -> Vec<String> {
  let mut command = vec![
    "claude".to_string(),
    "-p".to_string(),
    "--output-format".to_string(),
    "text".to_string(),
    "--no-session-persistence".to_string(),
  ];
  command.extend(model_args.iter().cloned());
  command.extend([
    "--system-prompt".to_string(),
    system.to_string(),
    DISALLOWED_TOOLS_FLAG.to_string(),
    ONE_SHOT_DISALLOWED_TOOLS.to_string(),
  ]);
  command
}

impl AgentBackend for ClaudeCodeBackend {
  fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
    Some((INSTRUCTIONS_FILE_NAME, INSTRUCTIONS_WRITTEN_EVENT))
  }

  fn prepare_transport(&mut self, _log_dir: &std::path::Path) -> anyhow::Result<()> {
    Ok(())
  }

  fn build_command(&self, prompt: &str) -> anyhow::Result<Vec<String>> {
    ClaudeCodeBackend::build_command(self, prompt)
  }

  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    ClaudeCodeBackend::prepare_env(self, env)
  }

  fn stdin_prompt(&self, prompt: &str) -> Option<String> {
    Some(prompt.to_string())
  }

  fn midturn_run_id(&self) -> Option<String> {
    self.midturn_delivery.then(|| self.midturn_run_id.clone()).flatten()
  }

  fn write_stdin_prompt(
    &self,
    stdin: Box<dyn AsyncWrite + Send + Unpin>,
    exited: ExitedProbe,
    prompt: &str,
  ) -> Option<BoxFuture<'static, anyhow::Result<()>>> {
    if !self.midturn_delivery {
      return None;
    }
    let run_id = self.midturn_run_id.clone();
    let prompt = prompt.to_string();
    Some(Box::pin(async move {
      charliebot_registry::midturn_delivery::write_initial_prompt(run_id, stdin, exited, prompt).await
    }))
  }

  fn on_event<'a>(&'a self, event: &'a Value) -> Option<BoxFuture<'a, anyhow::Result<()>>> {
    if !self.midturn_delivery {
      return None;
    }
    let run_id = self.midturn_run_id.clone();
    Some(Box::pin(async move { charliebot_registry::midturn_delivery::on_backend_event(run_id, event).await }))
  }

  fn translator(&self) -> Box<dyn Translate> {
    match self.midturn_run_id() {
      Some(run_id) => Box::new(MidturnTranslate { run_id: Some(run_id) }),
      None => Box::new(IdentityTranslate),
    }
  }

  fn transport(&self) -> TransportKind {
    TransportKind::RawLog
  }

  fn one_shot_text<'a>(
    &'a mut self,
    base_env: &'a Env,
    prompt: &'a str,
    system: &'a str,
    timeout: Option<Duration>,
  ) -> Option<BoxFuture<'a, anyhow::Result<String>>> {
    Some(Box::pin(async move { self.run_one_shot(base_env, prompt, system, timeout).await }))
  }
}

/// One home for every gateway endpoint and model variable. Endpoint backends write the first two names, may write the
/// extra-body and custom-headers names, and `claude_model_env` pins the final five. `CLAUDE_CODE_EXTRA_BODY` is
/// gateway-only: `without_gateway_env` strips it because the official Anthropic API must never receive it.
pub const GATEWAY_ENDPOINT_ENV_NAMES: [&str; 9] = [
  "ANTHROPIC_BASE_URL",
  "ANTHROPIC_AUTH_TOKEN",
  "ANTHROPIC_CUSTOM_HEADERS",
  "CLAUDE_CODE_EXTRA_BODY",
  "ANTHROPIC_MODEL",
  "ANTHROPIC_DEFAULT_OPUS_MODEL",
  "ANTHROPIC_DEFAULT_SONNET_MODEL",
  "ANTHROPIC_DEFAULT_HAIKU_MODEL",
  "CLAUDE_CODE_SUBAGENT_MODEL",
];
pub const GATEWAY_BASE_URL_ENV: &str = GATEWAY_ENDPOINT_ENV_NAMES[0];
pub const GATEWAY_AUTH_TOKEN_ENV: &str = GATEWAY_ENDPOINT_ENV_NAMES[1];
pub const GATEWAY_CUSTOM_HEADERS_ENV: &str = GATEWAY_ENDPOINT_ENV_NAMES[2];
pub const GATEWAY_EXTRA_BODY_ENV: &str = GATEWAY_ENDPOINT_ENV_NAMES[3];
pub const CLAUDE_MODEL_ENV_NAMES: [&str; 5] = [
  GATEWAY_ENDPOINT_ENV_NAMES[4],
  GATEWAY_ENDPOINT_ENV_NAMES[5],
  GATEWAY_ENDPOINT_ENV_NAMES[6],
  GATEWAY_ENDPOINT_ENV_NAMES[7],
  GATEWAY_ENDPOINT_ENV_NAMES[8],
];

/// Every Claude Code model-selection variable pinned to one model: the main model, the opus, sonnet and haiku slot
/// defaults, and the subagent model. An unset slot falls back to a CLI default, so the endpoint backend sets all five.
pub fn claude_model_env(model: &str) -> Env {
  CLAUDE_MODEL_ENV_NAMES.iter().map(|name| (name.to_string(), model.to_string())).collect()
}

/// A copy of `env` without the cc-anthropic-compatible gateway variables.
pub fn without_gateway_env(env: &Env) -> Env {
  env
    .iter()
    .filter(|(name, _)| !GATEWAY_ENDPOINT_ENV_NAMES.contains(&name.as_str()))
    .map(|(name, value)| (name.clone(), value.clone()))
    .collect()
}

/// Claude Code pointed at an Anthropic-compatible endpoint by environment. The endpoint URL and bearer token ride
/// `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN`, and `claude_model_env` pins every model-selection variable, so the
/// command carries no `--model` flag. Each backend type that names its endpoint builds one of these.
pub struct AnthropicEndpointBackend {
  backend: ClaudeCodeBackend,
  base_url: String,
  auth_token: String,
  model: String,
  context_window: Option<u64>,
  headers: Vec<(String, String)>,
  extra_body: Map<String, Value>,
}

impl AnthropicEndpointBackend {
  /// The backend for `model` at `base_url`, authenticated with `auth_token`. `effort` is the `--effort` level the
  /// command carries when it is set. `launch` carries the per-launch arguments of [`ClaudeCodeBackend`], such as the
  /// session id and the instructions file.
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    base_url: String,
    auth_token: String,
    model: String,
    context_window: Option<u64>,
    effort: Option<String>,
    claude_config_dir: Option<String>,
    launch: LaunchArgs,
    cgroup_config: SessionCgroupConfig,
  ) -> anyhow::Result<Self> {
    let backend = ClaudeCodeBackend::from_launch(None, effort, None, false, claude_config_dir, launch, cgroup_config)?;
    Ok(Self { backend, base_url, auth_token, model, context_window, headers: Vec::new(), extra_body: Map::new() })
  }

  /// Route `headers` as `ANTHROPIC_CUSTOM_HEADERS` lines, in the option's order.
  pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
    self.headers = headers;
    self
  }

  /// Merge `extra_body` into every request body as `CLAUDE_CODE_EXTRA_BODY`.
  pub fn with_extra_body(mut self, extra_body: Map<String, Value>) -> Self {
    self.extra_body = extra_body;
    self
  }

  /// The one-shot environment: the environment of this entry's runs, so the one-shot reaches the same endpoint,
  /// model, headers and request body.
  fn one_shot_env(&self, base_env: &Env) -> anyhow::Result<Env> {
    self.prepare_env(base_env.clone())
  }

  /// No `--model` — the model variables select the model — and the entry's `effort` rides `--effort`.
  fn one_shot_model_args(&self) -> Vec<String> {
    match self.backend.effort.as_deref() {
      Some(effort) => vec!["--effort".to_string(), effort.to_string()],
      None => Vec::new(),
    }
  }
}

impl AgentBackend for AnthropicEndpointBackend {
  fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
    self.backend.instructions_target()
  }

  fn prepare_transport(&mut self, log_dir: &std::path::Path) -> anyhow::Result<()> {
    self.backend.prepare_transport(log_dir)
  }

  fn build_command(&self, prompt: &str) -> anyhow::Result<Vec<String>> {
    self.backend.build_command(prompt)
  }

  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    let mut env = self.backend.prepare_env(env)?;
    env.insert(GATEWAY_BASE_URL_ENV.to_string(), self.base_url.clone());
    env.insert(GATEWAY_AUTH_TOKEN_ENV.to_string(), self.auth_token.clone());
    env.extend(claude_model_env(&self.model));
    if !self.headers.is_empty() {
      env.insert(
        GATEWAY_CUSTOM_HEADERS_ENV.to_string(),
        self.headers.iter().map(|(name, value)| format!("{name}: {value}")).collect::<Vec<_>>().join("\n"),
      );
    }
    if !self.extra_body.is_empty() {
      env.insert(GATEWAY_EXTRA_BODY_ENV.to_string(), Value::Object(self.extra_body.clone()).to_string());
    }
    if let Some(context_window) = self.context_window {
      let window = context_window.to_string();
      env.insert(claude_launch::AUTO_COMPACT_WINDOW_ENV.to_string(), window.clone());
      env.insert(claude_launch::MAX_CONTEXT_TOKENS_ENV.to_string(), window);
      env.remove(claude_launch::AUTOCOMPACT_PCT_OVERRIDE_ENV);
      env.remove(claude_launch::ANTHROPIC_API_KEY_ENV);
      env.remove(claude_launch::CLAUDE_CODE_OAUTH_TOKEN_ENV);
      env.insert(claude_launch::THINKING_DISPLAY_UPDATES_ENV.to_string(), "0".to_string());
    }
    Ok(env)
  }

  fn stdin_prompt(&self, prompt: &str) -> Option<String> {
    self.backend.stdin_prompt(prompt)
  }

  fn midturn_run_id(&self) -> Option<String> {
    self.backend.midturn_run_id()
  }

  fn write_stdin_prompt(
    &self,
    stdin: Box<dyn AsyncWrite + Send + Unpin>,
    exited: ExitedProbe,
    prompt: &str,
  ) -> Option<BoxFuture<'static, anyhow::Result<()>>> {
    self.backend.write_stdin_prompt(stdin, exited, prompt)
  }

  fn on_event<'a>(&'a self, event: &'a Value) -> Option<BoxFuture<'a, anyhow::Result<()>>> {
    self.backend.on_event(event)
  }

  fn translator(&self) -> Box<dyn Translate> {
    self.backend.translator()
  }

  fn transport(&self) -> TransportKind {
    self.backend.transport()
  }

  fn one_shot_text<'a>(
    &'a mut self,
    base_env: &'a Env,
    prompt: &'a str,
    system: &'a str,
    timeout: Option<Duration>,
  ) -> Option<BoxFuture<'a, anyhow::Result<String>>> {
    Some(Box::pin(async move {
      let command = one_shot_argv(&self.one_shot_model_args(), system);
      let env = self.one_shot_env(base_env)?;
      run_one_shot_process(
        &command,
        &env,
        prompt,
        timeout,
        self.backend.cgroup_session_id.as_deref(),
        &self.backend.cgroup_config,
      )
      .await
    }))
  }
}

struct IdentityTranslate;

impl Translate for IdentityTranslate {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
    Ok(vec![raw])
  }
}

/// Detect visible replies authored by a model outside the configured model family.
pub fn out_of_family_served_models(events: &[Value], configured_model: &str) -> Vec<String> {
  if configured_model.is_empty() {
    return Vec::new();
  }
  let mut seen = HashSet::new();
  let mut served = Vec::new();
  for event in events {
    if truthy(event.get("parent_tool_use_id")) || event.get("type").and_then(Value::as_str) != Some("assistant") {
      continue;
    }
    let Some(message) = event.get("message").and_then(Value::as_object) else {
      continue;
    };
    let Some(model) = message.get("model").and_then(Value::as_str) else {
      continue;
    };
    if model.is_empty() || model == SYNTHETIC_MODEL {
      continue;
    }
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
      continue;
    };
    if !blocks.iter().any(|block| block.get("type").and_then(Value::as_str) == Some("text")) {
      continue;
    }
    if same_model_family(model, configured_model) || !seen.insert(model.to_string()) {
      continue;
    }
    served.push(model.to_string());
  }
  served
}

fn same_model_family(served: &str, configured: &str) -> bool {
  let served = strip_date_suffix(served);
  let configured = strip_date_suffix(configured);
  let (shorter, longer) = if served.len() <= configured.len() { (served, configured) } else { (configured, served) };
  longer == shorter || longer.strip_prefix(shorter).is_some_and(|tail| tail.starts_with('-'))
}

fn strip_date_suffix(model: &str) -> &str {
  if let Some((prefix, suffix)) = model.rsplit_once('-')
    && suffix.len() == 8
    && suffix.bytes().all(|byte| byte.is_ascii_digit())
  {
    prefix
  } else {
    model
  }
}

fn truthy(value: Option<&Value>) -> bool {
  match value {
    None | Some(Value::Null) | Some(Value::Bool(false)) => false,
    Some(Value::Bool(true)) => true,
    Some(Value::Number(number)) => number.as_f64().is_none_or(|value| value != 0.0),
    Some(Value::String(value)) => !value.is_empty(),
    Some(Value::Array(values)) => !values.is_empty(),
    Some(Value::Object(values)) => !values.is_empty(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn command_has_headless_settings_and_stdin_prompt() {
    let backend = ClaudeCodeBackend::from_launch(
      Some("claude-fable-5-1".to_string()),
      Some("high".to_string()),
      Some("/synthetic/claude".to_string()),
      true,
      None,
      LaunchArgs { extra_flags: vec!["--resume".to_string(), "resume-id".to_string()], ..LaunchArgs::default() },
      SessionCgroupConfig::disabled(),
    )
    .unwrap();
    let command = backend.build_command("prompt").unwrap();
    let settings = command[command.iter().position(|value| value == "--settings").unwrap() + 1].clone();
    assert_eq!(command[0], "/synthetic/claude");
    assert!(command.windows(2).any(|pair| pair == ["--model", "claude-fable-5-1"]));
    assert!(command.windows(2).any(|pair| pair == ["--effort", "high"]));
    assert!(command.windows(2).any(|pair| pair == ["--resume", "resume-id"]));
    assert_eq!(
      settings,
      r#"{"disableClaudeAiConnectors":true,"syncClaudeAiSkills":false,"disableBundledSkills":true,"fastMode":true}"#
    );
    assert_eq!(AgentBackend::stdin_prompt(&backend, "prompt").as_deref(), Some("prompt"));
  }

  // Python: tests/test_claude_code_backend.py:234.
  #[test]
  fn manager_turn_settings_install_six_compaction_context_slots() {
    let backend = ClaudeCodeBackend::from_launch(
      None,
      None,
      None,
      false,
      None,
      LaunchArgs {
        backend_kwargs: serde_json::json!({"run_kind": "manager_turn"}).as_object().unwrap().clone(),
        cgroup_session_id: Some("manager-session".to_string()),
        ..LaunchArgs::default()
      },
      SessionCgroupConfig::disabled(),
    )
    .unwrap();
    let command = backend.build_command("hi").unwrap();
    let settings = command[command.iter().position(|value| value == "--settings").unwrap() + 1].clone();
    assert_eq!(command.iter().filter(|value| **value == "--settings").count(), 1);
    assert_eq!(
      settings,
      concat!(
        r#"{"disableClaudeAiConnectors":true,"syncClaudeAiSkills":false,"hooks":{"SessionStart":["#,
        r#"{"matcher":"compact","hooks":["#,
        r#"{"type":"command","command":"charliebot compaction-context --session manager-session --slot 1"},"#,
        r#"{"type":"command","command":"charliebot compaction-context --session manager-session --slot 2"},"#,
        r#"{"type":"command","command":"charliebot compaction-context --session manager-session --slot 3"},"#,
        r#"{"type":"command","command":"charliebot compaction-context --session manager-session --slot 4"},"#,
        r#"{"type":"command","command":"charliebot compaction-context --session manager-session --slot 5"},"#,
        r#"{"type":"command","command":"charliebot compaction-context --session manager-session --slot 6"}]}]}}"#,
      )
    );
    let settings: Value = serde_json::from_str(&settings).unwrap();
    let mut without_hooks = settings.as_object().unwrap().clone();
    without_hooks.remove("hooks");
    assert_eq!(
      without_hooks,
      serde_json::json!({"disableClaudeAiConnectors": true, "syncClaudeAiSkills": false}).as_object().unwrap().clone()
    );
    let [session_start] = settings["hooks"]["SessionStart"].as_array().unwrap().as_slice() else {
      panic!("one SessionStart entry");
    };
    assert_eq!(session_start["matcher"], "compact");
    let hooks = session_start["hooks"].as_array().unwrap();
    assert!(hooks.iter().all(|hook| hook["type"] == "command"));
    let commands: Vec<&str> = hooks.iter().map(|hook| hook["command"].as_str().unwrap()).collect();
    let expected: Vec<String> =
      (1..=6).map(|slot| format!("charliebot compaction-context --session manager-session --slot {slot}")).collect();
    assert_eq!(commands, expected);
  }

  // Python: tests/test_claude_code_backend.py:249.
  #[test]
  fn manager_turn_settings_require_the_charliebot_session_id() {
    let backend = ClaudeCodeBackend::from_launch(
      None,
      None,
      None,
      false,
      None,
      LaunchArgs {
        backend_kwargs: serde_json::json!({"run_kind": "manager_turn"}).as_object().unwrap().clone(),
        ..LaunchArgs::default()
      },
      SessionCgroupConfig::disabled(),
    )
    .unwrap();
    let error = backend.build_command("hi").unwrap_err();
    assert!(format!("{error:#}").contains("require a CharlieBot session id"), "{error:#}");
  }

  // Python: tests/test_anthropic_compatible_backend.py:37.
  #[test]
  fn anthropic_endpoint_sets_the_endpoint_and_pins_every_model_variable_without_a_flag() {
    let backend = AnthropicEndpointBackend::new(
      "http://localhost:8123/api/anthropic-proxy/openai-compatible/cc-synthetic".to_string(),
      "synthetic-token".to_string(),
      "synthetic/model-1".to_string(),
      Some(409_600),
      None,
      None,
      LaunchArgs { extra_flags: vec!["--resume".to_string(), "resume-id".to_string()], ..LaunchArgs::default() },
      SessionCgroupConfig::disabled(),
    )
    .unwrap();
    let command = backend.build_command("prompt").unwrap();
    assert!(!command.iter().any(|value| value == "--model"));
    assert!(command.windows(2).any(|pair| pair == ["--resume", "resume-id"]));

    let env = backend
      .prepare_env(Env::from([
        ("PATH".to_string(), "/synthetic/bin".to_string()),
        (claude_launch::AUTO_COMPACT_WINDOW_ENV.to_string(), "345000".to_string()),
        (claude_launch::AUTOCOMPACT_PCT_OVERRIDE_ENV.to_string(), "80".to_string()),
        (claude_launch::MAX_CONTEXT_TOKENS_ENV.to_string(), "100000".to_string()),
        ("CLAUDE_CODE_THINKING_DISPLAY_UPDATES".to_string(), "1".to_string()),
        ("ANTHROPIC_API_KEY".to_string(), "inherited-value".to_string()),
        ("CLAUDE_CODE_OAUTH_TOKEN".to_string(), "inherited-value".to_string()),
      ]))
      .unwrap();
    assert_eq!(env["ANTHROPIC_BASE_URL"], "http://localhost:8123/api/anthropic-proxy/openai-compatible/cc-synthetic");
    assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "synthetic-token");
    for name in ["ANTHROPIC_MODEL", "ANTHROPIC_DEFAULT_OPUS_MODEL", "ANTHROPIC_DEFAULT_SONNET_MODEL"] {
      assert_eq!(env[name], "synthetic/model-1");
    }
    assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "synthetic/model-1");
    assert_eq!(env["CLAUDE_CODE_SUBAGENT_MODEL"], "synthetic/model-1");
    assert_eq!(env[claude_launch::AUTO_COMPACT_WINDOW_ENV], "409600");
    assert_eq!(env[claude_launch::MAX_CONTEXT_TOKENS_ENV], "409600");
    assert!(!env.contains_key(claude_launch::AUTOCOMPACT_PCT_OVERRIDE_ENV));
    assert!(!env.contains_key("ANTHROPIC_API_KEY"));
    assert!(!env.contains_key("CLAUDE_CODE_OAUTH_TOKEN"));
    assert_eq!(env["CLAUDE_CODE_THINKING_DISPLAY_UPDATES"], "0");
    assert_eq!(env["PATH"], "/synthetic/bin");
  }

  // Python: tests/test_claude_code_backend.py::test_prepare_env_strips_an_inherited_cc_anthropic_compatible_worker_env
  #[test]
  fn prepare_env_strips_an_inherited_cc_anthropic_compatible_worker_env() {
    let backend = ClaudeCodeBackend::from_launch(
      Some("claude-opus-4-8".to_string()),
      None,
      None,
      false,
      None,
      LaunchArgs::default(),
      SessionCgroupConfig::disabled(),
    )
    .unwrap();
    let mut inherited = Env::new();
    for name in GATEWAY_ENDPOINT_ENV_NAMES {
      inherited.insert(name.to_string(), "inherited-gateway-value".to_string());
    }
    inherited.insert("PATH".to_string(), "/usr/bin".to_string());
    inherited.insert("UNRELATED".to_string(), "kept".to_string());

    let env = AgentBackend::prepare_env(&backend, inherited).unwrap();

    for name in GATEWAY_ENDPOINT_ENV_NAMES {
      assert!(!env.contains_key(name), "{name} survived the preparation");
    }
    assert_eq!(env["PATH"], "/usr/bin");
    assert_eq!(env["UNRELATED"], "kept");
  }

  // Python: tests/test_claude_code_backend.py::test_one_shot_text_spawns_claude_without_the_gateway_variables
  #[tokio::test]
  async fn one_shot_text_spawns_claude_without_the_gateway_variables() {
    let root = tempfile::tempdir().unwrap();
    let (bin, argv, env_file) = stage_fake_claude(root.path());
    let mut base_env = inherited_env(&bin);
    for name in GATEWAY_ENDPOINT_ENV_NAMES {
      base_env.insert(name.to_string(), "inherited-gateway-value".to_string());
    }
    base_env.insert("CLAUDE_CODE_EXTRA_BODY".to_string(), "inherited-gateway-value".to_string());
    base_env.insert("UNRELATED_INHERITED_VAR".to_string(), "kept".to_string());
    let mut backend = ClaudeCodeBackend::from_launch(
      Some("claude-haiku-5-5".to_string()),
      None,
      None,
      false,
      None,
      LaunchArgs::default(),
      zero_limits(),
    )
    .unwrap();

    assert_eq!(backend.one_shot_text(&base_env, "prompt", "system", None).unwrap().await.unwrap(), "ok");

    let args = recorded_lines(&argv);
    assert_eq!(args[args.iter().position(|value| value == "--model").unwrap() + 1], "claude-haiku-5-5");
    let env = recorded_env(&env_file);
    for name in GATEWAY_ENDPOINT_ENV_NAMES {
      assert!(!env.contains_key(name), "{name} reached the one-shot spawn");
    }
    assert!(!env.contains_key("CLAUDE_CODE_EXTRA_BODY"));
    assert_eq!(env["UNRELATED_INHERITED_VAR"], "kept");
  }

  // Python: tests/test_anthropic_compatible_backend.py::test_one_shot_text_launches_on_the_entry_endpoint_without_a_model_flag
  #[tokio::test]
  async fn one_shot_text_launches_on_the_entry_endpoint_without_a_model_flag() {
    let root = tempfile::tempdir().unwrap();
    let (bin, argv, env_file) = stage_fake_claude(root.path());
    let mut backend = endpoint_backend(concat!(
      "api_base: http://localhost:8123/api/anthropic-proxy\n",
      "context_window: 409600\n",
      "effort: high\n",
      "headers:\n  x-litellm-tags: deepseek\n",
      "extra_body:\n  cache:\n    no-store: true\n    no-cache: true\n",
    ));
    let base_env = inherited_env(&bin);

    assert_eq!(backend.one_shot_text(&base_env, "reply with ok", "system", None).unwrap().await.unwrap(), "ok");

    let args = recorded_lines(&argv);
    assert!(!args.iter().any(|value| value == "--model"), "{args:?}");
    assert_eq!(args[args.iter().position(|value| value == "--effort").unwrap() + 1], "high");
    let env = recorded_env(&env_file);
    assert_eq!(env[GATEWAY_BASE_URL_ENV], "http://localhost:8123/api/anthropic-proxy");
    assert_eq!(env[GATEWAY_AUTH_TOKEN_ENV], "no-auth");
    for name in CLAUDE_MODEL_ENV_NAMES {
      assert_eq!(env[name], "synthetic/model-1");
    }
    assert_eq!(env[GATEWAY_CUSTOM_HEADERS_ENV], "x-litellm-tags: deepseek");
    assert_eq!(
      serde_json::from_str::<Value>(&env[GATEWAY_EXTRA_BODY_ENV]).unwrap(),
      serde_json::json!({"cache": {"no-store": true, "no-cache": true}})
    );
    assert_eq!(env[claude_launch::AUTO_COMPACT_WINDOW_ENV], "409600");
    assert_eq!(env[claude_launch::MAX_CONTEXT_TOKENS_ENV], "409600");
  }

  // Python: tests/test_anthropic_compatible_backend.py::test_one_shot_text_omits_effort_when_the_entry_has_none
  #[tokio::test]
  async fn one_shot_text_omits_effort_when_the_entry_has_none() {
    let root = tempfile::tempdir().unwrap();
    let (bin, argv, _env_file) = stage_fake_claude(root.path());
    let mut backend = endpoint_backend("api_base: http://localhost:8123/api/anthropic-proxy\ncontext_window: 409600\n");

    backend.one_shot_text(&inherited_env(&bin), "reply with ok", "system", None).unwrap().await.unwrap();

    assert!(!recorded_lines(&argv).iter().any(|value| value == "--effort"));
  }

  fn zero_limits() -> SessionCgroupConfig {
    SessionCgroupConfig::disabled()
  }

  /// The backend the Anthropic-compatible factory builds from these type fields.
  fn endpoint_backend(type_fields: &str) -> Box<dyn AgentBackend> {
    use charliebot_core::config::BackendOption;
    use charliebot_registry::Registry;
    use charliebot_registry::backend::BackendFactory;

    use crate::backends::anthropic_compatible::factory::AnthropicCompatibleFactory;

    let option = BackendOption {
      id: "anthropic".to_string(),
      label: "Anthropic compatible".to_string(),
      model: "synthetic/model-1".to_string(),
      prompt_overlay: None,
      backend_type: "cc-anthropic-compatible".to_string(),
      type_fields: serde_yaml::from_str(type_fields).unwrap(),
    };
    let home = tempfile::tempdir().unwrap();
    let cfg = Registry::new().load_config(home.path()).unwrap();
    AnthropicCompatibleFactory.build(Some(&option), &cfg, LaunchArgs::default()).unwrap()
  }

  /// The inherited process environment a one-shot receives: the fake `claude` directory on `PATH`.
  fn inherited_env(bin: &std::path::Path) -> Env {
    Env::from([
      ("PATH".to_string(), format!("{}:/usr/bin:/bin", bin.display())),
      ("HOME".to_string(), bin.parent().unwrap().display().to_string()),
    ])
  }

  /// The fake-program helper that cargo builds beside this test binary. Unit tests cannot name it with
  /// `CARGO_BIN_EXE_*`, so the path comes from the target directory.
  fn fake_program_bin() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("the test binary path");
    exe
      .ancestors()
      .skip(1)
      .map(|directory| directory.join("fake_program"))
      .find(|candidate| candidate.is_file())
      .expect("cargo builds the fake-program helper into the target directory that holds the test binary")
  }

  /// Stages a fake `claude` that records its argv and environment, reads stdin, prints `ok`, and exits 0. Returns the
  /// bin directory and the recorded argv and environment files.
  fn stage_fake_claude(root: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(fake_program_bin(), bin.join("claude")).unwrap();
    let argv = root.join("argv.txt");
    let env_file = root.join("env.txt");
    let stdin = root.join("stdin.txt");
    std::fs::write(
      bin.join("probe.json"),
      serde_json::json!({"claude": [
        {"record_argv": argv},
        {"record_env": env_file},
        {"record_stdin": stdin},
        {"stdout": "ok"},
        {"exit": 0},
      ]})
      .to_string(),
    )
    .unwrap();
    (bin, argv, env_file)
  }

  fn recorded_lines(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path).unwrap().lines().map(str::to_string).collect()
  }

  fn recorded_env(path: &std::path::Path) -> HashMap<String, String> {
    recorded_lines(path)
      .into_iter()
      .filter_map(|line| line.split_once('=').map(|(name, value)| (name.to_string(), value.to_string())))
      .collect()
  }

  // Python: tests/test_anthropic_compatible_backend.py::test_endpoint_env_overrides_every_inherited_gateway_variable
  #[test]
  fn endpoint_env_overrides_every_inherited_gateway_variable() {
    let backend = AnthropicEndpointBackend::new(
      "http://localhost:8123/api/anthropic-proxy".to_string(),
      "no-auth".to_string(),
      "synthetic/model-1".to_string(),
      None,
      None,
      None,
      LaunchArgs::default(),
      SessionCgroupConfig::disabled(),
    )
    .unwrap();
    let mut inherited = Env::new();
    for name in GATEWAY_ENDPOINT_ENV_NAMES {
      inherited.insert(name.to_string(), "inherited-gateway-value".to_string());
    }

    let env = AgentBackend::prepare_env(&backend, inherited).unwrap();

    assert_eq!(env[GATEWAY_BASE_URL_ENV], "http://localhost:8123/api/anthropic-proxy");
    assert_eq!(env[GATEWAY_AUTH_TOKEN_ENV], "no-auth");
    for name in CLAUDE_MODEL_ENV_NAMES {
      assert_eq!(env[name], "synthetic/model-1");
    }
    // Stronger than the Python case: an inherited headers variable must not survive the preparation either,
    // so the test is red before the gateway strip lands.
    assert!(!env.contains_key(GATEWAY_CUSTOM_HEADERS_ENV));
  }

  // Python: tests/test_anthropic_compatible_backend.py::test_claude_model_env_sets_exactly_five_model_variables
  #[test]
  fn claude_model_env_sets_exactly_five_model_variables() {
    assert_eq!(
      claude_model_env("synthetic/model-1"),
      Env::from([
        ("ANTHROPIC_MODEL".to_string(), "synthetic/model-1".to_string()),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(), "synthetic/model-1".to_string()),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(), "synthetic/model-1".to_string()),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(), "synthetic/model-1".to_string()),
        ("CLAUDE_CODE_SUBAGENT_MODEL".to_string(), "synthetic/model-1".to_string()),
      ])
    );
  }

  #[test]
  fn headless_environment_forwards_host_settings_and_pins_defaults() {
    let source = Env::from([
      ("PATH".to_string(), "/synthetic/bin".to_string()),
      (claude_launch::AUTO_COMPACT_WINDOW_ENV.to_string(), "300000".to_string()),
      ("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE".to_string(), "0.7".to_string()),
    ]);
    let result = headless_claude_env(RunKind::Work, &source);
    assert_eq!(result["CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"], "1");
    assert_eq!(result["BASH_DEFAULT_TIMEOUT_MS"], "900000");
    assert_eq!(result["BASH_MAX_TIMEOUT_MS"], "900000");
    assert_eq!(result["CLAUDE_CODE_DISABLE_ADVISOR_TOOL"], "1");
    assert_eq!(result[claude_launch::AUTO_COMPACT_WINDOW_ENV], "300000");
    assert_eq!(result["CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"], "0.7");
    assert!(!result.contains_key(claude_launch::SIMPLE_SYSTEM_PROMPT_ENV));
  }

  #[test]
  fn reading_limits_use_host_window_and_drop_unknown_compact_reserve() {
    let source = Env::from([
      (claude_launch::AUTO_COMPACT_WINDOW_ENV.to_string(), "300000".to_string()),
      (claude_launch::MAX_CONTEXT_TOKENS_ENV.to_string(), "200000".to_string()),
    ]);
    assert_eq!(
      claude_reading_limits_from(RunKind::Work, &source),
      ContextLimits { declared_window: Some(300_000), compact_reserve: None }
    );
  }

  #[test]
  fn reading_limits_parse_python_integer_whitespace_sign_and_separators() {
    let source = Env::from([(claude_launch::AUTO_COMPACT_WINDOW_ENV.to_string(), " +300_000 ".to_string())]);
    assert_eq!(
      claude_reading_limits_from(RunKind::Work, &source),
      ContextLimits { declared_window: Some(300_000), compact_reserve: Some(33_000) }
    );
  }

  #[test]
  fn out_of_family_detector_uses_visible_main_chain_text_models_once() {
    let events = [
      serde_json::json!({"type":"assistant","message":{"model":"claude-fable-5-1","content":[{"type":"text"}]}}),
      serde_json::json!({"type":"assistant","message":{"model":"claude-opus-4-8","content":[{"type":"text"}]}}),
      serde_json::json!({"type":"assistant","message":{"model":"claude-opus-4-8","content":[{"type":"text"}]}}),
      serde_json::json!({"type":"assistant","parent_tool_use_id":"tool","message":{"model":"claude-haiku-4-5","content":[{"type":"text"}]}}),
      serde_json::json!({"type":"assistant","message":{"model":"<synthetic>","content":[{"type":"text"}]}}),
    ];
    assert_eq!(out_of_family_served_models(&events, "claude-fable-5-1"), ["claude-opus-4-8"]);
  }

  #[test]
  fn cli_native_one_shot_uses_text_mode_and_no_session_persistence() {
    let command = one_shot_argv(&["--model".to_string(), "claude-fable-5-1".to_string()], "system");
    assert_eq!(
      command,
      [
        "claude",
        "-p",
        "--output-format",
        "text",
        "--no-session-persistence",
        "--model",
        "claude-fable-5-1",
        "--system-prompt",
        "system",
        "--disallowed-tools",
        ONE_SHOT_DISALLOWED_TOOLS
      ]
    );
  }
}
