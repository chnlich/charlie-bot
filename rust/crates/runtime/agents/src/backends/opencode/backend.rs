//! The opencode session protocol (`src/backends/opencode/opencode.py`).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, bail};
use charliebot_core::event_types;
use charliebot_core::pyfmt::python_str;
use charliebot_core::pystr;
use charliebot_registry::backend::{
  AgentBackend, BackendFactory, BackendLifecycle, Env, LaunchArgs, ServeProcessSpec, ServeTransport, ServeTurn,
  ServeTurnArgs, ServeTurnResult, Translate, TransportKind,
};
use futures_util::future::BoxFuture;
use nix::sys::signal::Signal;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::watch;
use uuid::Uuid;

use super::options::{OpenCodeOption, model_overrides_config};
use super::relay::{self, TurnRecord};
use super::server::{OpenCodeHost, OpenCodeServer, OpenCodeServerPool, Turn};
use crate::cli_base::{apply_proxy_env, image_mime};
use crate::event_builders;
use crate::runner::EventFn;
use crate::transport::cgroup::SessionCgroupConfig;
use crate::transport::follow::{self, SilenceCallback};
use crate::transport::kill::{kill_process_group, python_exit_code};
use crate::transport::spawn::{AgentPipeReader, SpawnSpec, spawn_one_shot};
use charliebot_core::cgroup::SESSION_CGROUP_OOM_SCORE_ADJ;

const RUN_PLAN_MARKER: &str = "__charliebot_opencode_run__=";
const TOOL_EVENT_TYPES: [&str; 25] = [
  "permission.replied",
  "session.agent.selected",
  "session.created",
  "session.deleted",
  "session.execution.started",
  "session.forked",
  "session.inbox.cancelled",
  "session.inbox.delivered",
  "session.inbox.delivery.changed",
  "session.inbox.enqueued",
  "session.instructions.updated",
  "session.message.content.updated",
  "session.metadata.updated",
  "session.model.selected",
  "session.permissions",
  "session.renamed",
  "session.retry.scheduled",
  "session.reasoning.started",
  "session.shell.ended",
  "session.shell.started",
  "session.skill.activated",
  "session.step.failed",
  "session.step.streamed",
  "session.synthetic",
  "session.text.started",
];
const MORE_IGNORED_TYPES: [&str; 8] = [
  "session.tool.input.delta",
  "session.tool.input.ended",
  "session.tool.progress",
  "session.usage.recorded",
  "session.usage.updated",
  "session.viewed",
  "session.compaction.delta",
  "session.heartbeat",
];

static RUN_PLANS: OnceLock<Mutex<HashMap<String, Arc<OpenCodeRunPlan>>>> = OnceLock::new();
static UNHANDLED_TYPES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// The opencode backend type of one launch.
pub struct OpenCodeBackend {
  model: String,
  proxy_url: Option<String>,
  /// The `providers` keys of this option's context limit and request-body fields, which the one-shot config carries.
  one_shot_context: Map<String, Value>,
  host: OpenCodeHost,
  pool: Arc<OpenCodeServerPool>,
  launch: LaunchArgs,
  cgroup_config: SessionCgroupConfig,
  recovered_model: Option<String>,
  recovered_model_limit: Option<Map<String, Value>>,
}

impl OpenCodeBackend {
  pub fn new(
    model: String,
    option: OpenCodeOption,
    one_shot_context: Map<String, Value>,
    host: OpenCodeHost,
    pool: Arc<OpenCodeServerPool>,
    launch: LaunchArgs,
    cgroup_config: SessionCgroupConfig,
  ) -> Self {
    Self {
      model,
      proxy_url: option.proxy_url,
      one_shot_context,
      host,
      pool,
      launch,
      cgroup_config,
      recovered_model: None,
      recovered_model_limit: None,
    }
  }

  fn run_plan(&self) -> Arc<OpenCodeRunPlan> {
    Arc::new(OpenCodeRunPlan {
      model: self.model.clone(),
      proxy_url: self.proxy_url.clone(),
      host: self.host.clone(),
      pool: Arc::clone(&self.pool),
      instructions_content: self.launch.instructions_content.clone(),
      instructions_file: self.launch.instructions_file.clone(),
      resume_session_id: self.launch.resume_session_id.as_ref().filter(|id| !id.is_empty()).cloned(),
    })
  }

  async fn run_one_shot(
    &self,
    base_env: &Env,
    prompt: &str,
    system: &str,
    timeout: Option<Duration>,
  ) -> anyhow::Result<String> {
    run_one_shot(OpenCodeOneShotRequest {
      host: self.host.clone(),
      model: self.model.clone(),
      proxy_url: self.proxy_url.clone(),
      one_shot_context: self.one_shot_context.clone(),
      cgroup_session_id: self.launch.cgroup_session_id.clone(),
      cgroup_config: self.cgroup_config.clone(),
      base_env: base_env.clone(),
      prompt: prompt.to_string(),
      system: system.to_string(),
      timeout,
    })
    .await
  }
}

impl AgentBackend for OpenCodeBackend {
  fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
    Some(("AGENTS.md", "opencode_wrote_agents_md"))
  }

  fn prepare_transport(&mut self, _log_dir: &std::path::Path) -> anyhow::Result<()> {
    Ok(())
  }

  fn build_command(&self, _prompt: &str) -> anyhow::Result<Vec<String>> {
    let id = Uuid::new_v4().simple().to_string();
    RUN_PLANS
      .get_or_init(|| Mutex::new(HashMap::new()))
      .lock()
      .expect("opencode run plan map is not poisoned")
      .insert(id.clone(), self.run_plan());
    let executable = std::env::current_exe()?;
    Ok(vec![executable.to_string_lossy().into_owned(), relay::RELAY_ARG.to_string(), format!("{RUN_PLAN_MARKER}{id}")])
  }

  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    Ok(env)
  }

  fn stdin_prompt(&self, _prompt: &str) -> Option<String> {
    None
  }

  fn translator(&self) -> Box<dyn Translate> {
    Box::new(OpenCodeTranslator::with_model_limit(
      self.recovered_model.clone().unwrap_or_else(|| self.model.clone()),
      self.recovered_model_limit.clone(),
    ))
  }

  fn transport(&self) -> TransportKind {
    TransportKind::Serve
  }

  fn one_shot_text<'a>(
    &'a mut self,
    base_env: &'a Env,
    prompt: &'a str,
    system: &'a str,
    timeout: Option<Duration>,
  ) -> Option<BoxFuture<'a, anyhow::Result<String>>> {
    Some(Box::pin(self.run_one_shot(base_env, prompt, system, timeout)))
  }
}

/// The OpenCode factory (`src/backends/opencode/factory.py`).
pub struct OpenCodeFactory {
  host: OpenCodeHost,
  pool: Arc<OpenCodeServerPool>,
}

impl OpenCodeFactory {
  pub fn new(host: OpenCodeHost, pool: Arc<OpenCodeServerPool>) -> Self {
    Self { host, pool }
  }
}

impl BackendFactory for OpenCodeFactory {
  fn build(
    &self,
    option: Option<&charliebot_core::config::BackendOption>,
    cfg: &charliebot_core::config::CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let option = option.context("the opencode backend type needs its backends.options entry")?;
    let model = option_default_model(option, "backend ")?;
    let fields = OpenCodeOption::from_fields(&option.type_fields).map_err(anyhow::Error::msg)?;
    let one_shot_context = model_overrides_config(std::slice::from_ref(option))?;
    self.pool.record_options(&cfg.backends.options);
    self.pool.adopt_existing()?;
    let recovered_turn = match launch.log_dir.as_deref() {
      Some(directory) => relay::read_turn_record(directory)?,
      None => None,
    };
    let mut backend = OpenCodeBackend::new(
      model,
      fields,
      one_shot_context,
      self.host.clone(),
      Arc::clone(&self.pool),
      launch,
      SessionCgroupConfig::from_config(cfg),
    );
    if let Some(turn) = recovered_turn {
      backend.recovered_model = Some(turn.model);
      backend.recovered_model_limit = turn.model_limit;
    }
    Ok(Box::new(backend))
  }
}

/// Return the option's configured model, reporting an empty model with its backend role.
pub fn option_default_model(option: &charliebot_core::config::BackendOption, subject: &str) -> anyhow::Result<String> {
  if option.model.is_empty() {
    bail!("{subject}'{}' has no default model", option.id);
  }
  Ok(option.model.clone())
}

/// The immutable launch settings carried from the factory into the Serve transport.
pub(crate) struct OpenCodeRunPlan {
  model: String,
  proxy_url: Option<String>,
  host: OpenCodeHost,
  pool: Arc<OpenCodeServerPool>,
  instructions_content: Option<String>,
  instructions_file: Option<PathBuf>,
  resume_session_id: Option<String>,
}

pub(crate) fn take_run_plan(command: &[String]) -> anyhow::Result<Arc<OpenCodeRunPlan>> {
  let marker = command.get(2).context("the opencode serve command has no run plan")?;
  let id = marker.strip_prefix(RUN_PLAN_MARKER).context("the opencode serve command has an invalid run plan")?;
  RUN_PLANS
    .get_or_init(|| Mutex::new(HashMap::new()))
    .lock()
    .expect("opencode run plan map is not poisoned")
    .remove(id)
    .context("the opencode serve run plan was not registered")
}

/// The opencode lifecycle: each of its processes serves one turn over the shared server (`TransportKind::Serve`).
pub(crate) struct OpenCodeLifecycle;

impl BackendLifecycle for OpenCodeLifecycle {
  fn serve_transport(&self) -> Option<Arc<dyn ServeTransport>> {
    Some(Arc::new(OpenCodeServe))
  }
}

/// The opencode serve transport: the run plan that `build_command` registered, and the server's table of live turns.
struct OpenCodeServe;

impl ServeTransport for OpenCodeServe {
  fn take_turn(&self, command: &[String]) -> anyhow::Result<Box<dyn ServeTurn>> {
    Ok(Box::new(OpenCodeTurn { plan: take_run_plan(command)? }))
  }

  fn finish_active_turn(&self, pid: u32) -> BoxFuture<'static, anyhow::Result<()>> {
    Box::pin(super::server::finish_active_turn(pid))
  }
}

/// One opencode turn, with the run plan that `take_turn` took before its process spawned.
struct OpenCodeTurn {
  plan: Arc<OpenCodeRunPlan>,
}

impl ServeTurn for OpenCodeTurn {
  fn identity_process(&self, command: &[String], run_directory: &Path) -> anyhow::Result<Option<ServeProcessSpec>> {
    match fs::remove_file(run_directory.join(relay::TURN_RECORD_NAME)) {
      Ok(()) => {}
      Err(error) if error.kind() == io::ErrorKind::NotFound => {}
      Err(error) => return Err(error.into()),
    }
    let program = command.first().context("OpenCode relay command has no executable")?;
    let parent = relay::parent_identity()?;
    let parent_start = parent.pid_start.context("OpenCode relay parent pid has no start time")?;
    let mut env = Env::from([
      ("CHARLIEBOT_OPENCODE_RELAY_DIRECTORY".to_string(), run_directory.display().to_string()),
      (
        "CHARLIEBOT_OPENCODE_SERVER_DIRECTORY".to_string(),
        self
          .plan
          .host
          .profile_home
          .join(charliebot_core::home::OPENCODE_DIR_NAME)
          .join(relay::SERVER_DIRECTORY_NAME)
          .display()
          .to_string(),
      ),
      ("CHARLIEBOT_OPENCODE_PARENT_PID".to_string(), parent.pid.to_string()),
      ("CHARLIEBOT_OPENCODE_PARENT_START".to_string(), parent_start.clone()),
    ]);
    #[cfg(test)]
    let args = vec!["--exact".into(), "backends::opencode::test_backend::relay_subprocess".into(), "--ignored".into()];
    #[cfg(not(test))]
    let args = vec![
      relay::RELAY_ARG.into(),
      run_directory.as_os_str().to_owned(),
      parent.pid.to_string().into(),
      parent_start.into(),
    ];
    #[cfg(test)]
    {
      env.insert("CHARLIEBOT_OPENCODE_RELAY_TEST".to_string(), "1".to_string());
    }
    Ok(Some(ServeProcessSpec { program: program.clone().into(), args, env: std::mem::take(&mut env) }))
  }

  fn run(self: Box<Self>, args: ServeTurnArgs) -> BoxFuture<'static, anyhow::Result<ServeTurnResult>> {
    Box::pin(run_turn(self.plan, args))
  }
}

pub(crate) async fn run_turn(plan: Arc<OpenCodeRunPlan>, mut args: ServeTurnArgs) -> anyhow::Result<ServeTurnResult> {
  let server = plan.pool.server_for(plan.proxy_url.clone());
  let mut turn: Option<Arc<Turn>> = None;
  let mut unregistered_claim: Option<String> = None;
  let mut failed = false;
  let mut succeeded = false;
  let body_result = run_turn_body(&plan, &server, &mut args, &mut turn, &mut unregistered_claim).await;
  let mut result = match body_result {
    Ok(true) => {
      succeeded = true;
      Ok(())
    }
    Ok(false) => {
      failed = true;
      Ok(())
    }
    Err(RunError::TurnExited) => Ok(()),
    Err(RunError::Backend(error)) => {
      failed = true;
      let detail = error.to_string();
      emit(&args.on_event, event_builders::make_error_event(&format!("OpenCode backend failed: {detail}")))
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))
    }
    Err(RunError::Event(error)) => Err(error),
  };
  let turn_end = if let Some(turn) = turn { turn.finish().await } else { Ok(()) };
  let claim_release =
    if let Some(directory) = unregistered_claim { server.release_claim(&directory).await } else { Ok(()) };
  if let Err(error) = turn_end.and(claim_release) {
    tracing::error!(target: "charliebot_agents", error = %error, "opencode_turn_end_failed");
    failed = true;
    if let Err(event_error) =
      emit(&args.on_event, event_builders::make_error_event(&format!("OpenCode backend failed: {error}"))).await
      && result.is_ok()
    {
      result = Err(anyhow::anyhow!(event_error.to_string()));
    }
  }
  result?;
  let exit_code = if failed {
    Some(1)
  } else if succeeded {
    Some(0)
  } else {
    process_exit_code(&args.exit)
  };
  Ok(ServeTurnResult { exit_code })
}

async fn run_turn_body(
  plan: &OpenCodeRunPlan,
  server: &Arc<OpenCodeServer>,
  args: &mut ServeTurnArgs,
  turn: &mut Option<Arc<Turn>>,
  unregistered_claim: &mut Option<String>,
) -> Result<bool, RunError> {
  let directory = args.cwd.to_string_lossy().to_string();
  server.claim(&directory).await;
  *unregistered_claim = Some(directory.clone());
  await_or_exit(server.ensure_running(), &mut args.exit).await?.map_err(RunError::Backend)?;
  let session_id =
    await_or_exit(open_session(plan, server, &args.cwd), &mut args.exit).await?.map_err(RunError::Backend)?;
  let current = server.register_relay_claimed(&session_id, &directory, args.process_id).map_err(RunError::Backend)?;
  *unregistered_claim = None;
  *turn = Some(Arc::clone(&current));
  emit(&args.on_event, json!({"type": event_types::SESSION_ATTACHED, "session_id": session_id})).await?;
  let model_limit = match await_or_exit(fetch_model_limit(plan, server, &args.cwd), &mut args.exit).await? {
    Ok(limit) => limit,
    Err(error) => {
      tracing::warn!(target: "charliebot_agents", model = %plan.model, error = %error, "opencode_context_catalog_unavailable");
      None
    }
  };
  await_or_exit(server.wait_interrupts_settled(&session_id), &mut args.exit).await?.map_err(RunError::Backend)?;
  await_or_exit(put_environment(plan, server, &session_id, &args.env, args.cgroup_path.as_deref()), &mut args.exit)
    .await?
    .map_err(RunError::Backend)?;
  if plan.instructions_file.is_none() && plan.instructions_content.as_ref().is_some_and(|content| !content.is_empty()) {
    await_or_exit(reload_locations(server), &mut args.exit).await?.map_err(RunError::Backend)?;
  }
  let prompt = effective_prompt(plan, &args.prompt);
  let prompt_request = prompt_request(&prompt, &args.uploaded_files);
  let record_name = server.record_name().map_err(RunError::Backend)?;
  relay::write_turn_record(
    &args.run_directory.join(relay::TURN_RECORD_NAME),
    &TurnRecord {
      server_record: record_name,
      session_id: session_id.clone(),
      prompt_request,
      model: plan.model.clone(),
      model_limit: model_limit.clone(),
    },
  )
  .map_err(RunError::Backend)?;

  let translator = Arc::new(Mutex::new(OpenCodeTranslator::with_model_limit(plan.model.clone(), model_limit)));
  let callback_failure = Arc::new(Mutex::new(None));
  let translate_state = Arc::clone(&translator);
  let event_state = Arc::clone(&translator);
  let callback_failure_for_handler = Arc::clone(&callback_failure);
  let handler = args.on_event.clone();
  let server_for_handler = Arc::clone(server);
  let alive = args.exit.clone();
  let relay_exit = args.exit.clone();
  let progress_timeout = server.progress_timeout().map_err(RunError::Backend)?;
  let silence_message =
    format!("OpenCode session {session_id} produced no event for {} s", progress_timeout.as_secs_f64());
  let silence: SilenceCallback = Box::new(move || {
    let message = silence_message.clone();
    Box::pin(async move { Err(io::Error::new(io::ErrorKind::TimedOut, message)) })
  });
  let follow_result = follow::tail_follow_events(
    &args.raw_log,
    move |event| {
      reject_permission(&event)?;
      translate_state.lock().expect("OpenCode translator mutex is not poisoned").translate_event(&event)
    },
    move |event| {
      let handler = handler.clone();
      let failure = Arc::clone(&callback_failure_for_handler);
      let server = Arc::clone(&server_for_handler);
      let mut relay_exit = relay_exit.clone();
      async move {
        let mut event = event;
        if event.get("type").and_then(Value::as_str) == Some(event_types::ERROR)
          && event
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.starts_with("OpenCode backend failed:"))
          && let Some(reason) = server.failure_reason_after_stream_end().await
        {
          let message = format!("OpenCode backend failed: {reason}");
          let object = event.as_object_mut().expect("error event is an object");
          object.insert("message".to_string(), json!(message));
          object.insert("content".to_string(), json!(message));
        }
        if event.get("type").and_then(Value::as_str) == Some(event_types::ERROR)
          && event
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.starts_with("OpenCode execution interrupted (reason: user)"))
        {
          wait_turn_process(&mut relay_exit).await;
          if process_exit_code(&relay_exit) == Some(-libc::SIGTERM) {
            return Ok(());
          }
        }
        if let Err(error) = handler(event).await {
          let message = format!("{error:#}");
          *failure.lock().expect("OpenCode event callback mutex is not poisoned") = Some(error);
          return Err(io::Error::other(message));
        }
        Ok(())
      }
    },
    move || alive.borrow().is_none(),
    Some(&args.raw_cursor),
    0,
    args.post_result_timeout,
    args.buffer_limit,
    follow::TAIL_POLL_INTERVAL,
    Some(silence),
    progress_timeout,
  )
  .await;
  if let Some(error) = callback_failure.lock().expect("OpenCode event callback mutex is not poisoned").take() {
    return Err(RunError::Event(error));
  }
  let summary = follow_result.map_err(RunError::Backend)?;
  let status = event_state.lock().expect("OpenCode translator mutex is not poisoned").terminal_status();
  if status == Some(TurnTerminal::Interrupted) && process_exit_code(&args.exit) == Some(-libc::SIGTERM) {
    return Err(RunError::TurnExited);
  }
  match (summary.saw_result, status) {
    (true, Some(TurnTerminal::Succeeded)) => Ok(true),
    (_, Some(TurnTerminal::Failed | TurnTerminal::Interrupted | TurnTerminal::RelayEnded)) => Ok(false),
    _ => Err(RunError::Backend(anyhow::anyhow!("OpenCode relay exited without a terminal event"))),
  }
}

/// Reject interactive permission requests, including during raw-log recovery.
pub fn reject_permission(event: &Value) -> anyhow::Result<()> {
  if event.get("type").and_then(Value::as_str) == Some("permission.asked") {
    let data = object_or_empty(event.get("data"))?;
    let permission = data.get("permission").map(python_str).unwrap_or_else(|| "None".to_string());
    let patterns = data.get("patterns").map(python_str).unwrap_or_else(|| "None".to_string());
    bail!(
      "OpenCode requested interactive permission '{permission}' (patterns={patterns}) but CharlieBot runs headless with no approval path; aborting run."
    );
  }
  Ok(())
}

#[derive(Debug)]
enum RunError {
  TurnExited,
  Backend(anyhow::Error),
  Event(anyhow::Error),
}

impl std::fmt::Display for RunError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::TurnExited => formatter.write_str("OpenCode turn process exited"),
      Self::Backend(error) | Self::Event(error) => write!(formatter, "{error}"),
    }
  }
}

impl std::error::Error for RunError {}

impl From<()> for RunError {
  fn from(_: ()) -> Self {
    Self::TurnExited
  }
}

impl From<anyhow::Error> for RunError {
  fn from(error: anyhow::Error) -> Self {
    Self::Event(error)
  }
}

async fn emit(on_event: &EventFn, event: Value) -> Result<(), RunError> {
  on_event(event).await.map_err(RunError::Event)
}

async fn await_or_exit<T>(
  future: impl std::future::Future<Output = T>,
  exit: &mut watch::Receiver<Option<Result<i32, String>>>,
) -> Result<T, ()> {
  tokio::select! {
    result = future => Ok(result),
    _ = wait_turn_process(exit) => Err(()),
  }
}

async fn wait_turn_process(exit: &mut watch::Receiver<Option<Result<i32, String>>>) {
  loop {
    if exit.borrow().is_some() {
      return;
    }
    if exit.changed().await.is_err() {
      return;
    }
  }
}

fn process_exit_code(exit: &watch::Receiver<Option<Result<i32, String>>>) -> Option<i32> {
  match exit.borrow().as_ref() {
    Some(Ok(code)) => Some(*code),
    Some(Err(_)) => Some(-1),
    None => None,
  }
}

async fn open_session(plan: &OpenCodeRunPlan, server: &OpenCodeServer, cwd: &Path) -> anyhow::Result<String> {
  if let Some(session_id) = &plan.resume_session_id {
    let response = server.client_request(reqwest::Method::GET, &format!("/api/session/{session_id}"))?.send().await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
      bail!(
        "OpenCode session {session_id} is not in the opencode store at {}; the turn does not create a replacement session",
        plan.host.data_dir().display()
      );
    }
    response.error_for_status()?;
    return Ok(session_id.clone());
  }
  let (provider_id, model_id) = split_model(&plan.model)?;
  let body = json!({
    "location": {"directory": cwd.to_string_lossy()},
    "model": {"id": model_id, "providerID": provider_id}
  });
  let response = server.client_request(reqwest::Method::POST, "/api/session")?.json(&body).send().await?;
  let body = response.error_for_status()?.json::<Value>().await?;
  body
    .pointer("/data/id")
    .and_then(Value::as_str)
    .map(str::to_string)
    .context("opencode create-session response has no string data.id")
}

async fn fetch_model_limit(
  plan: &OpenCodeRunPlan,
  server: &OpenCodeServer,
  cwd: &Path,
) -> anyhow::Result<Option<Map<String, Value>>> {
  let (provider_id, model_id) = split_model(&plan.model)?;
  let response = server
    .client_request(reqwest::Method::GET, "/api/model")?
    .query(&[("location[directory]", cwd.to_string_lossy().as_ref())])
    .send()
    .await?;
  let payload = response.error_for_status()?.json::<Value>().await?;
  if payload.get("data").and_then(Value::as_array).is_none() {
    tracing::warn!(target: "charliebot_agents", provider_id, model_id, "opencode_context_catalog_unavailable");
    return Ok(None);
  };
  if let Some(limit) = extract_model_limit(&payload, provider_id, model_id) {
    return Ok(Some(limit));
  }
  tracing::warn!(target: "charliebot_agents", provider_id, model_id, "opencode_context_catalog_unavailable");
  Ok(None)
}

pub(super) fn extract_model_limit(payload: &Value, provider_id: &str, model_id: &str) -> Option<Map<String, Value>> {
  let models = payload.get("data")?.as_array()?;
  models.iter().find_map(|model| {
    (model.get("providerID")?.as_str()? == provider_id && model.get("id")?.as_str()? == model_id)
      .then(|| model.get("limit")?.as_object().cloned())
      .flatten()
  })
}

async fn put_environment(
  plan: &OpenCodeRunPlan,
  server: &OpenCodeServer,
  session_id: &str,
  env: &Env,
  cgroup: Option<&Path>,
) -> anyhow::Result<()> {
  let join_script = plan.host.join_script();
  if !join_script.is_file() {
    bail!("opencode join script is missing at {}", join_script.display());
  }
  let mut variables = env.clone();
  if let Some(proxy_url) = &plan.proxy_url {
    apply_proxy_env(&mut variables, proxy_url);
  }
  if let Some(cgroup) = cgroup {
    variables.insert("BASH_ENV".to_string(), join_script.display().to_string());
    variables.insert("CHARLIEBOT_SESSION_CGROUP".to_string(), cgroup.display().to_string());
    variables.insert("CHARLIEBOT_SESSION_OOM_SCORE_ADJ".to_string(), SESSION_CGROUP_OOM_SCORE_ADJ.to_string());
  }
  let response = server
    .client_request(reqwest::Method::PUT, &format!("/api/session/{session_id}/environment"))?
    .json(&json!({"variables": variables}))
    .send()
    .await?;
  response.error_for_status()?;
  Ok(())
}

async fn reload_locations(server: &OpenCodeServer) -> anyhow::Result<()> {
  server.client_request(reqwest::Method::POST, "/api/location/reload")?.send().await?.error_for_status()?;
  Ok(())
}

fn prompt_request(prompt: &str, uploaded_files: &[Value]) -> Value {
  let mut body = json!({"text": prompt});
  let attachments = image_file_attachments(uploaded_files);
  if !attachments.is_empty() {
    body.as_object_mut().unwrap().insert("files".to_string(), Value::Array(attachments));
  }
  body
}

fn effective_prompt(plan: &OpenCodeRunPlan, prompt: &str) -> String {
  if plan.instructions_file.is_some()
    && let Some(content) = plan.instructions_content.as_ref().filter(|content| !content.is_empty())
  {
    return crate::one_shot::frame_system_prompt(content, prompt);
  }
  prompt.to_string()
}

fn split_model(model: &str) -> anyhow::Result<(&str, &str)> {
  let mut parts = model.split('/');
  let provider = parts.next().context("not enough values to unpack (expected 2, got 0)")?;
  let model_id = parts.next().context("not enough values to unpack (expected 2, got 1)")?;
  if parts.next().is_some() {
    bail!("too many values to unpack (expected 2)");
  }
  Ok((provider, model_id))
}

pub(super) fn image_file_attachments(uploaded_files: &[Value]) -> Vec<Value> {
  let mut attachments = Vec::new();
  for reference in uploaded_files {
    let filename = reference.get("filename").map(python_str).unwrap_or_default();
    let extension = filename.rsplit('.').next().unwrap_or_default();
    let Some(mime) = image_mime(extension) else {
      continue;
    };
    let path = reference.get("path").map(python_str).unwrap_or_default();
    let data = match fs::read(&path) {
      Ok(data) => data,
      Err(error) => {
        tracing::warn!(target: "charliebot_agents", path, filename, error = %error, "opencode_attachment_unreadable");
        continue;
      }
    };
    use base64::Engine;
    attachments.push(json!({
      "uri": format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(data)),
      "name": filename
    }));
  }
  attachments
}

fn error_text(error: Option<&Value>) -> String {
  let Some(error) = error else {
    return "None".to_string();
  };
  if let Some(object) = error.as_object() {
    let message = object
      .get("message")
      .filter(|value| crate::cli_base::truthy(value))
      .or_else(|| object.get("data").and_then(Value::as_object).and_then(|data| data.get("message")));
    if let Some(message) = message.filter(|value| crate::cli_base::truthy(value)) {
      return python_str(message);
    }
  }
  error.to_string()
}

/// The event translator and usage accumulator for one turn.
pub struct OpenCodeTranslator {
  model: String,
  model_limit: Option<Map<String, Value>>,
  streamed_parts: HashSet<(String, i64)>,
  tool_names: HashMap<String, String>,
  usage_input: i64,
  usage_output: i64,
  usage_cache_read: i64,
  usage_cache_write: i64,
  usage_cost: f64,
  last_step_tokens: Option<Map<String, Value>>,
  pub model_request_started_ms: Option<i64>,
  terminal: Option<TurnTerminal>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnTerminal {
  Succeeded,
  Failed,
  Interrupted,
  RelayEnded,
}

impl OpenCodeTranslator {
  pub fn new(model: String) -> Self {
    Self::with_model_limit(model, None)
  }

  pub fn with_model_limit(model: String, model_limit: Option<Map<String, Value>>) -> Self {
    Self {
      model,
      model_limit,
      streamed_parts: HashSet::new(),
      tool_names: HashMap::new(),
      usage_input: 0,
      usage_output: 0,
      usage_cache_read: 0,
      usage_cache_write: 0,
      usage_cost: 0.0,
      last_step_tokens: None,
      model_request_started_ms: None,
      terminal: None,
    }
  }

  /// Map one opencode session event to zero or more CharlieBot events.
  pub fn translate_event(&mut self, event: &Value) -> anyhow::Result<Vec<Value>> {
    let event_type = required_string(event, "type")?;
    let data = object_or_empty(event.get("data"))?;
    match event_type {
      "session.execution.succeeded" => {
        self.terminal = Some(TurnTerminal::Succeeded);
        Ok(vec![self.accumulated_result(self.model_limit.as_ref())])
      }
      "session.execution.failed" => {
        self.terminal = Some(TurnTerminal::Failed);
        Ok(vec![event_builders::make_error_event(&error_text(data.get("error")))])
      }
      "session.execution.interrupted" => {
        self.terminal = Some(TurnTerminal::Interrupted);
        let reason = data.get("reason").map(python_str).unwrap_or_else(|| "None".to_string());
        Ok(vec![event_builders::make_error_event(&format!("OpenCode execution interrupted (reason: {reason})"))])
      }
      "charliebot.opencode_relay.ended" => {
        self.terminal = Some(TurnTerminal::RelayEnded);
        let reason = required_string(event, "reason")?;
        Ok(vec![event_builders::make_error_event(&format!("OpenCode backend failed: {reason}"))])
      }
      "session.step.started" => {
        if self.model_request_started_ms.is_none() {
          self.model_request_started_ms = Some(required_i64(data, "started")?);
        }
        Ok(Vec::new())
      }
      "session.text.delta" => {
        self
          .streamed_parts
          .insert((required_string(data, "assistantMessageID")?.to_string(), required_i64(data, "ordinal")?));
        Ok(vec![event_builders::make_text_event(required_string(data, "delta")?)])
      }
      "session.text.ended" => {
        let key = (required_string(data, "assistantMessageID")?.to_string(), required_i64(data, "ordinal")?);
        if self.streamed_parts.contains(&key) {
          return Ok(Vec::new());
        }
        let text = required_string(data, "text")?;
        Ok(if text.is_empty() { Vec::new() } else { vec![event_builders::make_text_event(text)] })
      }
      "session.reasoning.delta" => {
        self
          .streamed_parts
          .insert((required_string(data, "assistantMessageID")?.to_string(), required_i64(data, "ordinal")?));
        Ok(vec![json!({"type": event_types::THINKING, "content": required_string(data, "delta")?})])
      }
      "session.reasoning.ended" => {
        let key = (required_string(data, "assistantMessageID")?.to_string(), required_i64(data, "ordinal")?);
        if self.streamed_parts.contains(&key) {
          return Ok(Vec::new());
        }
        let text = required_string(data, "text")?;
        Ok(if text.is_empty() { Vec::new() } else { vec![json!({"type": event_types::THINKING, "content": text})] })
      }
      "session.tool.input.started" => {
        self.tool_names.insert(required_string(data, "id")?.to_string(), required_string(data, "name")?.to_string());
        Ok(Vec::new())
      }
      "session.tool.called" => {
        let id = required_string(data, "id")?;
        let name = self.tool_names.get(id).context(format!("OpenCode tool call {id} arrived without a tool name"))?;
        Ok(vec![json!({
          "type": event_types::ASSISTANT,
          "message": {"content": [{
            "type": event_types::TOOL_USE,
            "name": name,
            "id": id,
            "input": data.get("input").context("OpenCode tool call has no input")?
          }]}
        })])
      }
      "session.tool.success" => {
        let id = required_string(data, "id")?;
        let parts =
          data.get("content").and_then(Value::as_array).context("OpenCode tool success has no content array")?;
        let mut content = Vec::new();
        for part in parts {
          if part.get("type").and_then(Value::as_str) == Some("text") {
            content.push(required_string(part, "text")?.to_string());
          } else {
            let mime = required_string(part, "mime")?;
            content.push(part.get("name").map(python_str).unwrap_or(mime.to_string()));
          }
        }
        Ok(vec![json!({"type": event_types::TOOL_RESULT, "tool_use_id": id, "content": content.join("\n")})])
      }
      "session.tool.failed" => Ok(vec![json!({
        "type": event_types::TOOL_RESULT,
        "tool_use_id": required_string(data, "id")?,
        "content": error_text(data.get("error"))
      })]),
      "session.step.ended" => {
        self.accumulate_usage(data)?;
        Ok(Vec::new())
      }
      "session.compaction.started" => {
        let pre_tokens = self.last_step_tokens.as_ref().and_then(|tokens| tokens.get("input")).and_then(Value::as_i64);
        Ok(vec![event_builders::make_compact_boundary_event(required_string(data, "reason")?, pre_tokens)])
      }
      "session.compaction.ended" => {
        self.accumulate_usage(data)?;
        Ok(Vec::new())
      }
      _ if TOOL_EVENT_TYPES.contains(&event_type) || MORE_IGNORED_TYPES.contains(&event_type) => Ok(Vec::new()),
      _ => {
        let mut unhandled = UNHANDLED_TYPES
          .get_or_init(|| Mutex::new(HashSet::new()))
          .lock()
          .expect("opencode event registry is not poisoned");
        if unhandled.insert(event_type.to_string()) {
          tracing::debug!(target: "charliebot_agents", event_type, "opencode_session_event_unhandled");
        }
        Ok(Vec::new())
      }
    }
  }

  fn accumulate_usage(&mut self, data: &Map<String, Value>) -> anyhow::Result<()> {
    let tokens = data.get("tokens").and_then(Value::as_object).context("OpenCode usage event has no tokens object")?;
    let cache = tokens.get("cache").and_then(Value::as_object).context("OpenCode usage event has no cache object")?;
    let input = required_i64(tokens, "input")?;
    let output = required_i64(tokens, "output")?;
    let cache_read = required_i64(cache, "read")?;
    let cache_write = required_i64(cache, "write")?;
    let cost = data.get("cost").and_then(Value::as_f64).context("OpenCode usage event has no numeric cost")?;
    self.usage_input += input;
    self.usage_output += output;
    self.usage_cache_read += cache_read;
    self.usage_cache_write += cache_write;
    self.usage_cost += cost;
    self.last_step_tokens = Some(Map::from_iter([
      ("input".to_string(), json!(input)),
      ("output".to_string(), json!(output)),
      ("reasoning".to_string(), tokens.get("reasoning").cloned().unwrap_or(json!(0))),
      ("cache_read".to_string(), json!(cache_read)),
      ("cache_write".to_string(), json!(cache_write)),
    ]));
    Ok(())
  }

  pub(super) fn accumulated_result(&self, model_limit: Option<&Map<String, Value>>) -> Value {
    let snapshot = self.last_step_tokens.as_ref().map(|tokens| {
      json!({
        "model": self.model,
        "tokens": tokens,
        "limit": model_limit
      })
    });
    event_builders::make_result_event(
      self.usage_input,
      self.usage_output,
      self.usage_cache_read,
      self.usage_cache_write,
      json!(self.usage_cost),
      snapshot,
    )
  }

  fn terminal_status(&self) -> Option<TurnTerminal> {
    self.terminal
  }
}

impl Translate for OpenCodeTranslator {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
    self.translate_event(&raw)
  }
}

struct OpenCodeOneShotRequest {
  host: OpenCodeHost,
  model: String,
  proxy_url: Option<String>,
  one_shot_context: Map<String, Value>,
  cgroup_session_id: Option<String>,
  cgroup_config: SessionCgroupConfig,
  base_env: Env,
  prompt: String,
  system: String,
  timeout: Option<Duration>,
}

/// The one-shot XDG root, matching Python's `charliebot-opencode-oneshot-*` mkdtemp in
/// src/backends/opencode/opencode.py; removing it is the Drop of the returned TempDir.
fn oneshot_root() -> anyhow::Result<tempfile::TempDir> {
  Ok(tempfile::Builder::new().prefix("charliebot-opencode-oneshot-").tempdir()?)
}

async fn run_one_shot(request: OpenCodeOneShotRequest) -> anyhow::Result<String> {
  let OpenCodeOneShotRequest {
    host,
    model,
    proxy_url,
    one_shot_context,
    cgroup_session_id,
    cgroup_config,
    base_env,
    prompt,
    system,
    timeout,
  } = request;
  if !host.program.program.is_file() {
    bail!("opencode 2.0 binary not found at {}", host.program.display.display());
  }
  let framed = crate::one_shot::frame_system_prompt(&system, &prompt);
  let root = oneshot_root()?;
  let mut env = base_env;
  env.extend(host.extra_env.clone());
  env.insert("XDG_DATA_HOME".to_string(), root.path().join("data").display().to_string());
  env.insert("XDG_STATE_HOME".to_string(), root.path().join("state").display().to_string());
  env.insert("XDG_CACHE_HOME".to_string(), root.path().join("cache").display().to_string());
  let mut config = json!({"permission": {"*": "deny"}});
  config.as_object_mut().expect("the one-shot config is a JSON object").extend(one_shot_context);
  env.insert("OPENCODE_CONFIG_CONTENT".to_string(), config.to_string());
  if let Some(proxy_url) = &proxy_url {
    apply_proxy_env(&mut env, proxy_url);
  }
  let mut args = host.program.args.clone();
  args.push("run".into());
  args.push("--standalone".into());
  args.push("--format".into());
  args.push("json".into());
  args.push("-m".into());
  args.push(model.into());
  args.push("--".into());
  args.push(framed.into());
  let spec = SpawnSpec::new(
    host.program.program.as_os_str(),
    args,
    None,
    env.into_iter().map(|(key, value)| (key.into(), value.into())).collect(),
    cgroup_session_id,
    cgroup_config,
  );
  let mut spawned = spawn_one_shot(&spec, Some(Signal::SIGKILL)).await.context("spawning the opencode one-shot")?;
  let pid = spawned.child.id().context("opencode one-shot did not expose its process id")?;
  let stdout = spawned.child.stdout.take().context("opencode one-shot stdout was not piped")?;
  let stderr = spawned.child.stderr.take().context("opencode one-shot stderr was not piped")?;
  let (exit_tx, mut exit_rx) = watch::channel(None);
  let mut child = spawned.child;
  tokio::spawn(async move {
    let code = child.wait().await.map(python_exit_code).map_err(|error| error.to_string());
    let _ = exit_tx.send(Some(code));
  });
  let stderr_task = tokio::spawn(async move {
    let mut stderr = stderr;
    let mut bytes = Vec::new();
    stderr.read_to_end(&mut bytes).await.map(|_| bytes)
  });
  let collect = collect_one_shot(stdout);
  let collect_result = match timeout {
    Some(limit) => match tokio::time::timeout(limit, collect).await {
      Ok(result) => result,
      Err(error) => {
        kill_process_group(pid, Signal::SIGKILL);
        Err(anyhow::Error::from(error))
      }
    },
    None => collect.await,
  };
  if collect_result.is_err() && exit_rx.borrow().is_none() {
    kill_process_group(pid, Signal::SIGKILL);
  }
  let exit_result = wait_child_exit(&mut exit_rx).await;
  let stderr_result = stderr_task.await.context("waiting for opencode one-shot stderr reader")?;
  drop(root);
  let text = collect_result?;
  exit_result?;
  stderr_result?;
  Ok(text)
}

async fn collect_one_shot(stdout: AgentPipeReader) -> anyhow::Result<String> {
  let mut reader = BufReader::new(stdout);
  let mut line = Vec::new();
  let mut parts = Vec::new();
  let mut error = None;
  loop {
    line.clear();
    if reader.read_until(b'\n', &mut line).await? == 0 {
      break;
    }
    let Some(event) = crate::transport::spawn::parse_backend_line(&line) else {
      continue;
    };
    match event.get("type").and_then(Value::as_str) {
      Some("error") => error = Some(format!("opencode run failed: {}", error_text(event.get("error")))),
      Some("text") => parts.push(
        event
          .pointer("/part/text")
          .and_then(Value::as_str)
          .context("opencode text event has no part.text")?
          .to_string(),
      ),
      _ => {}
    }
    if error.is_some() {
      break;
    }
  }
  if let Some(error) = error {
    bail!(error);
  }
  Ok(pystr::strip(&parts.concat()).to_string())
}

async fn wait_child_exit(exit: &mut watch::Receiver<Option<Result<i32, String>>>) -> anyhow::Result<i32> {
  loop {
    if let Some(code) = exit.borrow().clone() {
      return code.map_err(anyhow::Error::msg);
    }
    exit.changed().await.context("opencode one-shot exit channel closed")?;
  }
}

trait JsonObject {
  fn get_field(&self, key: &str) -> Option<&Value>;
}

impl JsonObject for Value {
  fn get_field(&self, key: &str) -> Option<&Value> {
    self.get(key)
  }
}

impl JsonObject for Map<String, Value> {
  fn get_field(&self, key: &str) -> Option<&Value> {
    self.get(key)
  }
}

fn required_string<'a>(value: &'a impl JsonObject, key: &str) -> anyhow::Result<&'a str> {
  value.get_field(key).and_then(Value::as_str).with_context(|| format!("OpenCode event has no string {key}"))
}

fn required_i64(value: &impl JsonObject, key: &str) -> anyhow::Result<i64> {
  value.get_field(key).and_then(Value::as_i64).with_context(|| format!("OpenCode event has no integer {key}"))
}

fn object_or_empty(value: Option<&Value>) -> anyhow::Result<&Map<String, Value>> {
  static EMPTY_OBJECT: OnceLock<Map<String, Value>> = OnceLock::new();
  match value {
    None => Ok(EMPTY_OBJECT.get_or_init(Map::new)),
    Some(Value::Object(object)) => Ok(object),
    Some(value) => bail!("OpenCode event data must be an object, got {value}"),
  }
}

#[test]
fn oneshot_root_uses_the_python_prefix_and_is_removed_on_drop() {
  let root = oneshot_root().expect("creating the opencode one-shot root");
  let name = root.path().file_name().and_then(|n| n.to_str()).expect("one-shot root has a file name").to_string();
  assert!(name.starts_with("charliebot-opencode-oneshot-"), "unexpected one-shot root name: {name}");
  let path = root.path().to_path_buf();
  assert!(path.exists());
  drop(root);
  assert!(!path.exists(), "one-shot root survived its drop: {}", path.display());
}
