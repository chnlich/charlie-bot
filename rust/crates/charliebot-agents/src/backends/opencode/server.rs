//! The shared opencode server and its turn ownership (`src/backends/opencode/server.py`).

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use anyhow::{Context, bail};
use charliebot_core::home;
use charliebot_registry::backend::Env;
use futures_util::StreamExt;
use nix::sys::signal::Signal;
use regex::Regex;
use reqwest::{Client, Method, RequestBuilder};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::ChildStdout;
use tokio::sync::{Notify, watch};
use tokio::task::{Id, JoinHandle};
use uuid::Uuid;

use super::opencode_limits;
use crate::cli_base::apply_proxy_env;
use crate::transport::kill::{kill_process_group, python_exit_code};
use crate::transport::spawn::{SpawnSpec, SpawnedChild, spawn_one_shot};

const OUTPUT_TAIL_BYTES: usize = 64 * 1024;
const READ_CHUNK_BYTES: usize = 64 * 1024;

static LISTEN_RE: OnceLock<Regex> = OnceLock::new();
static ACTIVE_TURNS: OnceLock<Mutex<HashMap<u32, Weak<Turn>>>> = OnceLock::new();

/// The executable and fixed arguments that start the opencode 2.0 binary.
#[derive(Clone, Debug)]
pub struct OpenCodeProgram {
  pub program: PathBuf,
  pub args: Vec<OsString>,
  pub display: PathBuf,
}

/// The process inputs for the shared opencode server.
#[derive(Clone, Debug)]
pub struct OpenCodeHost {
  pub program: OpenCodeProgram,
  pub repo_root: PathBuf,
  pub profile_home: PathBuf,
  pub extra_env: Env,
}

impl OpenCodeHost {
  /// Resolve the server binary and repository paths from the CharlieBot process environment.
  pub fn from_process() -> anyhow::Result<Self> {
    let home_dir = std::env::var_os("HOME").context("HOME must be set for the opencode backend")?;
    let root = PathBuf::from(home_dir);
    let binary = root.join(".opencode").join("bin").join("opencode");
    let profile_home = home::charliebot_home_dir().map_err(anyhow::Error::new)?;
    let repo_root = std::env::var_os("CHARLIEBOT_REPO_ROOT")
      .map(PathBuf::from)
      .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.."));
    Ok(Self {
      program: OpenCodeProgram { program: binary.clone(), args: Vec::new(), display: binary },
      repo_root,
      profile_home,
      extra_env: Env::new(),
    })
  }

  /// Construct a host with an injected program, as the fake server tests do.
  pub fn with_program(
    program: PathBuf,
    args: Vec<OsString>,
    display: PathBuf,
    repo_root: PathBuf,
    profile_home: PathBuf,
    extra_env: Env,
  ) -> Self {
    Self { program: OpenCodeProgram { program, args, display }, repo_root, profile_home, extra_env }
  }

  /// The shell hook used by each session command to enter its CharlieBot cgroup.
  pub fn join_script(&self) -> PathBuf {
    self.repo_root.join("src/backends/opencode/cgroup_join.sh")
  }

  pub fn store_dir(&self) -> PathBuf {
    self.profile_home.join(home::OPENCODE_DIR_NAME)
  }

  pub fn data_dir(&self) -> PathBuf {
    self.store_dir().join("data")
  }

  pub fn state_dir(&self) -> PathBuf {
    self.store_dir().join("state")
  }

  pub fn cache_dir(&self) -> PathBuf {
    self.store_dir().join("cache")
  }
}

/// The opencode manager timeouts. Tests pass short values to exercise each wait path.
#[derive(Clone, Copy, Debug)]
pub struct ServerTimeouts {
  pub start: Duration,
  pub stop: Duration,
  pub stream_exit_grace: Duration,
  pub idle_exit: Duration,
  pub interrupt_settle: Duration,
  pub progress: Duration,
}

impl Default for ServerTimeouts {
  fn default() -> Self {
    Self {
      start: opencode_limits::OPENCODE_SERVER_START_TIMEOUT,
      stop: opencode_limits::OPENCODE_SERVER_STOP_TIMEOUT,
      stream_exit_grace: opencode_limits::OPENCODE_STREAM_EXIT_GRACE,
      idle_exit: opencode_limits::OPENCODE_SERVER_IDLE_EXIT_SECONDS,
      interrupt_settle: opencode_limits::OPENCODE_INTERRUPT_SETTLE_TIMEOUT,
      progress: opencode_limits::OPENCODE_SSE_PROGRESS_TIMEOUT,
    }
  }
}

/// The shared server registry, keyed by a backend option's proxy URL.
pub struct OpenCodeServerPool {
  host: OpenCodeHost,
  timeouts: ServerTimeouts,
  servers: Mutex<HashMap<Option<String>, Arc<OpenCodeServer>>>,
}

impl OpenCodeServerPool {
  pub fn new(host: OpenCodeHost, timeouts: ServerTimeouts) -> Arc<Self> {
    Arc::new(Self { host, timeouts, servers: Mutex::new(HashMap::new()) })
  }

  /// Return the server shared by every turn with this proxy URL.
  pub fn server_for(self: &Arc<Self>, proxy_url: Option<String>) -> Arc<OpenCodeServer> {
    let mut servers = self.servers.lock().expect("opencode server pool mutex is not poisoned");
    Arc::clone(
      servers.entry(proxy_url.clone()).or_insert_with(|| Arc::new(OpenCodeServer::new(Arc::clone(self), proxy_url))),
    )
  }

  /// Stop every server in this pool after its owner finishes the test or process.
  pub async fn stop_all(&self) -> anyhow::Result<()> {
    let servers =
      self.servers.lock().expect("opencode server pool mutex is not poisoned").values().cloned().collect::<Vec<_>>();
    for server in servers {
      server.stop().await?;
    }
    Ok(())
  }
}

/// The server's headless policy. CharlieBot has no path to approve interactive requests.
pub fn headless_config() -> Value {
  json!({
    "permission": {
      "*": "allow",
      "question": "deny",
      "subagent": "deny",
      "opencode_session_move": "deny"
    },
    "shell": "bash",
    "default_agent": "charliebot",
    "agent": {"charliebot": {"mode": "primary"}}
  })
}

/// One turn's claim on an opencode session.
pub struct Turn {
  pub session_id: String,
  pub directory: String,
  process_id: u32,
  server: Weak<OpenCodeServer>,
  events: Mutex<VecDeque<TurnEvent>>,
  ready: Notify,
  finished: AtomicBool,
}

#[derive(Debug)]
pub(crate) enum TurnEvent {
  Event(Value),
  ServerLost(String),
}

impl Turn {
  fn push(&self, event: TurnEvent) {
    self.events.lock().expect("opencode turn queue is not poisoned").push_back(event);
    self.ready.notify_one();
  }

  /// Drop interruptions sent before this turn's prompt.
  pub fn drop_earlier_interrupts(&self) {
    self.events
      .lock()
      .expect("opencode turn queue is not poisoned")
      .retain(|item| !matches!(item, TurnEvent::Event(event) if event.get("type").and_then(Value::as_str) == Some("session.execution.interrupted")));
  }

  pub(crate) async fn next_event(&self) -> TurnEvent {
    loop {
      let notified = self.ready.notified();
      if let Some(event) = self.events.lock().expect("opencode turn queue is not poisoned").pop_front() {
        return event;
      }
      notified.await;
    }
  }

  /// Unregister, interrupt the session and delete its shells in that order.
  pub async fn finish(&self) -> anyhow::Result<()> {
    if self.finished.swap(true, Ordering::AcqRel) {
      return Ok(());
    }
    let active = ACTIVE_TURNS.get_or_init(|| Mutex::new(HashMap::new()));
    active.lock().expect("opencode active turn map is not poisoned").remove(&self.process_id);
    let Some(server) = self.server.upgrade() else {
      return Ok(());
    };
    server.unregister(self);
    if !server.process_alive() {
      tracing::info!(target: "charliebot_agents", session_id = %self.session_id, "opencode_turn_end_steps_skipped");
      return Ok(());
    }
    server.interrupt(&self.session_id).await?;
    server.delete_shells(&self.session_id, &self.directory).await
  }
}

struct ServerProcess {
  pid: u32,
  exit: watch::Receiver<Option<Result<i32, String>>>,
  base_url: Mutex<Option<String>>,
  client: Mutex<Option<Client>>,
  password: String,
  tail: Arc<Mutex<VecDeque<u8>>>,
}

impl ServerProcess {
  fn alive(&self) -> bool {
    self.exit.borrow().is_none()
  }

  fn client(&self) -> anyhow::Result<(Client, String)> {
    let client = self.client.lock().expect("opencode HTTP client mutex is not poisoned").clone();
    let url = self.base_url.lock().expect("opencode server URL mutex is not poisoned").clone();
    Ok((client.context("opencode server client is not ready")?, url.context("opencode server URL is not ready")?))
  }

  fn tail_text(&self) -> String {
    String::from_utf8_lossy(
      &self.tail.lock().expect("opencode output tail is not poisoned").iter().copied().collect::<Vec<_>>(),
    )
    .trim()
    .to_string()
  }
}

/// One opencode process, its HTTP API, event stream, and active turns.
pub struct OpenCodeServer {
  pool: Weak<OpenCodeServerPool>,
  proxy_url: Option<String>,
  start_lock: tokio::sync::Mutex<()>,
  state: Mutex<ServerState>,
}

struct ServerState {
  process: Option<Arc<ServerProcess>>,
  turns: HashMap<String, Arc<Turn>>,
  pending: HashMap<String, usize>,
  settled: HashMap<String, Arc<Notify>>,
  connected: Option<watch::Receiver<bool>>,
  connected_tx: Option<watch::Sender<bool>>,
  reader_done: Option<watch::Receiver<Option<String>>>,
  reader_done_tx: Option<watch::Sender<Option<String>>>,
  version: Option<String>,
  stopping: bool,
  tasks: Vec<JoinHandle<()>>,
  idle: Option<JoinHandle<()>>,
}

impl OpenCodeServer {
  fn new(pool: Arc<OpenCodeServerPool>, proxy_url: Option<String>) -> Self {
    Self {
      pool: Arc::downgrade(&pool),
      proxy_url,
      start_lock: tokio::sync::Mutex::new(()),
      state: Mutex::new(ServerState {
        process: None,
        turns: HashMap::new(),
        pending: HashMap::new(),
        settled: HashMap::new(),
        connected: None,
        connected_tx: None,
        reader_done: None,
        reader_done_tx: None,
        version: None,
        stopping: false,
        tasks: Vec::new(),
        idle: None,
      }),
    }
  }

  pub fn process_alive(&self) -> bool {
    self
      .state
      .lock()
      .expect("opencode server state mutex is not poisoned")
      .process
      .as_ref()
      .is_some_and(|process| process.alive())
  }

  pub fn pid(&self) -> Option<u32> {
    self.state.lock().expect("opencode server state mutex is not poisoned").process.as_ref().map(|process| process.pid)
  }

  pub fn version(&self) -> Option<String> {
    self.state.lock().expect("opencode server state mutex is not poisoned").version.clone()
  }

  /// Build the server environment from CharlieBot's environment and the profile store paths.
  pub fn server_env(&self, password: &str) -> anyhow::Result<Env> {
    self.server_env_from(std::env::vars().collect::<Env>(), password)
  }

  pub(crate) fn server_env_from(&self, mut env: Env, password: &str) -> anyhow::Result<Env> {
    let host = self.pool()?.host.clone();
    env.remove("CHARLIEBOT_RUN_TOKEN");
    env.remove("CHARLIEBOT_SESSION_ID");
    env.extend(self.pool()?.host.extra_env.clone());
    env.insert("XDG_DATA_HOME".to_string(), host.data_dir().display().to_string());
    env.insert("XDG_STATE_HOME".to_string(), host.state_dir().display().to_string());
    env.insert("XDG_CACHE_HOME".to_string(), host.cache_dir().display().to_string());
    env.insert("OPENCODE_SERVER_PASSWORD".to_string(), password.to_string());
    env.insert("OPENCODE_CONFIG_CONTENT".to_string(), headless_config().to_string());
    if let Some(proxy_url) = &self.proxy_url {
      apply_proxy_env(&mut env, proxy_url);
    }
    Ok(env)
  }

  #[cfg(test)]
  pub(crate) fn dispatch_test_event(self: &Arc<Self>, event: Value) {
    self.dispatch(event);
  }

  #[cfg(test)]
  pub(crate) fn add_pending_test(&self, session_id: &str) {
    self.add_pending(session_id);
  }

  #[cfg(test)]
  pub(crate) fn turn_count_test(&self) -> usize {
    self.state.lock().expect("opencode server state mutex is not poisoned").turns.len()
  }

  /// Start the server unless it is already running.
  pub async fn ensure_running(self: &Arc<Self>) -> anyhow::Result<()> {
    let _guard = self.start_lock.lock().await;
    if self.process_alive() {
      return Ok(());
    }
    if self.state.lock().expect("opencode server state mutex is not poisoned").process.is_some() {
      self.shutdown_locked().await?;
    }
    if let Err(error) = self.start_locked().await {
      if let Err(cleanup_error) = self.shutdown_locked().await {
        tracing::error!(target: "charliebot_agents", error = %cleanup_error, "opencode_start_cleanup_failed");
      }
      return Err(error);
    }
    Ok(())
  }

  /// Stop the server process. A later turn starts a new process.
  pub async fn stop(&self) -> anyhow::Result<()> {
    let _guard = self.start_lock.lock().await;
    self.shutdown_locked().await
  }

  /// Claim a session for a turn.
  pub fn register(self: &Arc<Self>, session_id: &str, directory: &str, process_id: u32) -> anyhow::Result<Arc<Turn>> {
    let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
    if state.turns.contains_key(session_id) {
      bail!("opencode session {session_id} already has a running turn");
    }
    let turn = Arc::new(Turn {
      session_id: session_id.to_string(),
      directory: directory.to_string(),
      process_id,
      server: Arc::downgrade(self),
      events: Mutex::new(VecDeque::new()),
      ready: Notify::new(),
      finished: AtomicBool::new(false),
    });
    state.turns.insert(session_id.to_string(), Arc::clone(&turn));
    if let Some(idle) = state.idle.take() {
      idle.abort();
    }
    ACTIVE_TURNS
      .get_or_init(|| Mutex::new(HashMap::new()))
      .lock()
      .expect("opencode active turn map is not poisoned")
      .insert(process_id, Arc::downgrade(&turn));
    Ok(turn)
  }

  fn unregister(self: &Arc<Self>, turn: &Turn) {
    let should_idle = {
      let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
      if state.turns.get(&turn.session_id).is_some_and(|found| std::ptr::eq(found.as_ref(), turn)) {
        state.turns.remove(&turn.session_id);
      }
      state.turns.is_empty() && !state.stopping && state.process.as_ref().is_some_and(|process| process.alive())
    };
    if should_idle {
      self.arm_idle();
    }
  }

  /// Interrupt the named session and wait for its event before a later turn sends a prompt.
  pub async fn interrupt(&self, session_id: &str) -> anyhow::Result<bool> {
    self.add_pending(session_id);
    let result = async {
      let response = self.client_request(Method::POST, &format!("/api/session/{session_id}/interrupt"))?.send().await?;
      let response = response.error_for_status()?;
      let body = response.json::<Value>().await?;
      body
        .get("interrupted")
        .and_then(Value::as_bool)
        .context("opencode interrupt response has no boolean 'interrupted' field")
    }
    .await;
    let interrupted = match result {
      Ok(interrupted) => interrupted,
      Err(error) => {
        self.settle_one(session_id);
        return Err(error);
      }
    };
    if !interrupted {
      self.settle_one(session_id);
    }
    Ok(interrupted)
  }

  /// Wait for every earlier interrupt of this session to produce its event.
  pub async fn wait_interrupts_settled(&self, session_id: &str) -> anyhow::Result<()> {
    let notify = self.settled_notify(session_id);
    let wait = async {
      loop {
        let notified = notify.notified();
        if self.pending_count(session_id) == 0 {
          return;
        }
        notified.await;
      }
    };
    if tokio::time::timeout(self.timeouts()?.interrupt_settle, wait).await.is_err() {
      bail!(
        "earlier interrupt of opencode session {session_id} did not settle within {} s",
        self.timeouts()?.interrupt_settle.as_secs_f64()
      );
    }
    Ok(())
  }

  /// Remove every shell for this session that belongs to this turn's directory.
  pub async fn delete_shells(&self, session_id: &str, directory: &str) -> anyhow::Result<()> {
    let response =
      self.client_request(Method::GET, "/api/shell")?.query(&[("location[directory]", directory)]).send().await?;
    let body = response.error_for_status()?.json::<Value>().await?;
    let shells = body.get("data").and_then(Value::as_array).context("opencode shell list has no data array")?;
    for shell in shells {
      if shell.pointer("/metadata/sessionID").and_then(Value::as_str) != Some(session_id) {
        continue;
      }
      let id = shell.get("id").and_then(Value::as_str).context("opencode shell has no string id")?;
      self
        .client_request(Method::DELETE, &format!("/api/shell/{id}"))?
        .query(&[("location[directory]", directory)])
        .send()
        .await?
        .error_for_status()?;
    }
    Ok(())
  }

  pub(crate) fn client_request(&self, method: Method, path: &str) -> anyhow::Result<RequestBuilder> {
    let process = self.process()?;
    let (client, base_url) = process.client()?;
    Ok(
      client
        .request(method, format!("{base_url}{path}"))
        .timeout(opencode_limits::OPENCODE_HTTP_API_TIMEOUT)
        .basic_auth("opencode", Some(&process.password)),
    )
  }

  fn process(&self) -> anyhow::Result<Arc<ServerProcess>> {
    self
      .state
      .lock()
      .expect("opencode server state mutex is not poisoned")
      .process
      .clone()
      .context("opencode server is not running")
  }

  fn timeouts(&self) -> anyhow::Result<ServerTimeouts> {
    self.pool().map(|pool| pool.timeouts)
  }

  pub(crate) fn progress_timeout(&self) -> anyhow::Result<Duration> {
    Ok(self.timeouts()?.progress)
  }

  fn pool(&self) -> anyhow::Result<Arc<OpenCodeServerPool>> {
    self.pool.upgrade().context("opencode server pool was dropped")
  }

  async fn start_locked(self: &Arc<Self>) -> anyhow::Result<()> {
    let pool = self.pool()?;
    let program = &pool.host.program;
    if !program.program.is_file() {
      bail!("opencode 2.0 binary not found at {}", program.display.display());
    }
    for path in [pool.host.data_dir(), pool.host.state_dir(), pool.host.cache_dir()] {
      fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
    }
    let password = Uuid::new_v4().simple().to_string();
    let mut env = self.server_env(&password)?;
    let store_dir = pool.host.store_dir();
    let mut args = program.args.clone();
    args.extend(["serve".into(), "--port".into(), "0".into()]);
    let spec = SpawnSpec::new(
      program.program.as_os_str(),
      args,
      Some(store_dir),
      std::mem::take(&mut env).into_iter().map(|(key, value)| (key.into(), value.into())).collect(),
      None,
      crate::transport::cgroup::system_cgroup_base_dir(),
      0,
      0,
      0,
    );
    let SpawnedChild { mut child, .. } = spawn_one_shot(&spec, Some(Signal::SIGKILL)).await?;
    let pid = child.id().context("opencode serve did not expose its process id")?;
    let stdout = child.stdout.take().context("opencode serve stdout was not piped")?;
    let stderr = child.stderr.take().context("opencode serve stderr was not piped")?;
    let (exit_tx, exit_rx) = watch::channel(None);
    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let process = Arc::new(ServerProcess {
      pid,
      exit: exit_rx,
      base_url: Mutex::new(None),
      client: Mutex::new(None),
      password,
      tail: Arc::clone(&tail),
    });
    {
      let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
      state.process = Some(Arc::clone(&process));
      state.version = None;
      state.stopping = false;
      state.pending.clear();
      state.settled.clear();
      state.tasks.clear();
      let (connected_tx, connected_rx) = watch::channel(false);
      let (reader_done_tx, reader_done_rx) = watch::channel(None);
      state.connected = Some(connected_rx);
      state.connected_tx = Some(connected_tx);
      state.reader_done = Some(reader_done_rx);
      state.reader_done_tx = Some(reader_done_tx);
    }
    let reaper_tail = Arc::clone(&tail);
    tokio::spawn(async move {
      let code = match child.wait().await {
        Ok(status) => Ok(python_exit_code(status)),
        Err(error) => Err(error.to_string()),
      };
      if exit_tx.send(Some(code)).is_err() {
        tracing::debug!(target: "charliebot_agents", pid, "opencode_exit_receiver_dropped");
      }
      drop(reaper_tail);
    });
    self.track(tokio::spawn(drain_stream(stderr, Arc::clone(&tail))));
    let mut stdout = BufReader::new(stdout);
    let url = self.read_listening_url(&mut stdout, &process).await?;
    *process.base_url.lock().expect("opencode server URL mutex is not poisoned") = Some(url.clone());
    let client = charliebot_compat::http_client::client(&charliebot_compat::http_client::Settings::from_env())
      .context("building the opencode HTTP client")?;
    *process.client.lock().expect("opencode HTTP client mutex is not poisoned") = Some(client);
    self.track(tokio::spawn(drain_stream(stdout, Arc::clone(&tail))));
    let info = self.client_request(Method::GET, "/api/info")?.send().await?.error_for_status()?.json::<Value>().await?;
    let version = info.get("version").and_then(Value::as_str).context("opencode /api/info has no string version")?;
    if version.split('.').next() != Some("2") {
      bail!("{} reports opencode {version}; CharlieBot needs the 2.x server", program.display.display());
    }
    self.state.lock().expect("opencode server state mutex is not poisoned").version = Some(version.to_string());
    let weak = Arc::downgrade(self);
    let process_for_reader = Arc::clone(&process);
    let (connected_tx, reader_done_tx) = {
      let state = self.state.lock().expect("opencode server state mutex is not poisoned");
      (state.connected_tx.as_ref().unwrap().clone(), state.reader_done_tx.as_ref().unwrap().clone())
    };
    self.track(tokio::spawn(async move {
      let reason = read_events(Arc::clone(&process_for_reader), connected_tx, weak.clone()).await;
      let _ = reader_done_tx.send(Some(reason.clone()));
      if let Some(server) = weak.upgrade() {
        server.lose(reason).await;
      }
    }));
    let weak = Arc::downgrade(self);
    let process_for_watcher = Arc::clone(&process);
    self.track(tokio::spawn(async move {
      let code = wait_exit(&process_for_watcher).await;
      if let Some(server) = weak.upgrade() {
        let reason = match code {
          Ok(code) => format!("opencode serve exited with code {code}: {}", process_for_watcher.tail_text()),
          Err(error) => format!("opencode serve wait failed: {error}"),
        };
        server.lose(reason).await;
      }
    }));
    self.await_connected().await?;
    self.arm_idle();
    tracing::info!(target: "charliebot_agents", pid, url, version, "opencode_server_started");
    Ok(())
  }

  async fn read_listening_url(
    &self,
    stdout: &mut BufReader<ChildStdout>,
    process: &ServerProcess,
  ) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + self.timeouts()?.start;
    let display_path = self.pool()?.host.program.display.display().to_string();
    let timeout_seconds = self.timeouts()?.start.as_secs_f64();
    loop {
      let mut line = Vec::new();
      let count = tokio::time::timeout_at(deadline, stdout.read_until(b'\n', &mut line)).await.map_err(|_| {
        anyhow::anyhow!("{display_path} serve did not print its listening address within {timeout_seconds} s")
      })??;
      if count == 0 {
        bail!(
          "{} serve exited before printing its listening address: {}",
          self.pool()?.host.program.display.display(),
          process.tail_text()
        );
      }
      append_tail(&process.tail, &line);
      let text = String::from_utf8_lossy(&line);
      if let Some(capture) =
        LISTEN_RE.get_or_init(|| Regex::new(r"server listening on (http://\S+)").unwrap()).captures(&text)
      {
        return Ok(capture[1].to_string());
      }
    }
  }

  async fn await_connected(&self) -> anyhow::Result<()> {
    let (mut connected, mut reader_done) = {
      let state = self.state.lock().expect("opencode server state mutex is not poisoned");
      (state.connected.as_ref().unwrap().clone(), state.reader_done.as_ref().unwrap().clone())
    };
    let deadline = tokio::time::Instant::now() + self.timeouts()?.start;
    loop {
      if *connected.borrow() {
        return Ok(());
      }
      if let Some(reason) = reader_done.borrow().clone() {
        bail!("opencode event stream ended before server.connected: {reason}");
      }
      tokio::time::timeout_at(deadline, async {
        tokio::select! {
          result = connected.changed() => result.context("opencode connected channel closed"),
          result = reader_done.changed() => result.context("opencode event reader channel closed"),
        }
      })
      .await
      .map_err(|_| anyhow::anyhow!("opencode event stream did not report server.connected in time"))??;
    }
  }

  fn dispatch(self: &Arc<Self>, event: Value) {
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or_default();
    if event_type == "server.connected" {
      let state = self.state.lock().expect("opencode server state mutex is not poisoned");
      if let Some(connected) = &state.connected_tx {
        let _ = connected.send(true);
      }
      return;
    }
    let session_id = event.pointer("/data/sessionID").and_then(Value::as_str).map(str::to_string);
    let Some(session_id) = session_id else {
      return;
    };
    if event_type == "session.execution.interrupted" {
      self.settle_one(&session_id);
    }
    let turn = self.state.lock().expect("opencode server state mutex is not poisoned").turns.get(&session_id).cloned();
    if let Some(turn) = turn {
      turn.push(TurnEvent::Event(event));
    } else if event_type == "session.execution.started" {
      tracing::warn!(target: "charliebot_agents", session_id, "opencode_unclaimed_execution_interrupted");
      let server = Arc::clone(self);
      self.track(tokio::spawn(async move {
        if let Err(error) = server.interrupt(&session_id).await {
          tracing::error!(target: "charliebot_agents", session_id, error = %error, "opencode_unclaimed_interrupt_failed");
        }
      }));
    }
  }

  async fn lose(self: &Arc<Self>, reason: String) {
    let _guard = self.start_lock.lock().await;
    let turns = {
      let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
      if state.process.is_none() || state.stopping {
        return;
      }
      state.stopping = true;
      state.turns.values().cloned().collect::<Vec<_>>()
    };
    for turn in &turns {
      if let Err(error) = turn.finish().await {
        tracing::error!(target: "charliebot_agents", session_id = %turn.session_id, error = %error, "opencode_turn_end_failed");
      }
    }
    if let Err(error) = self.shutdown_locked().await {
      tracing::error!(target: "charliebot_agents", error = %error, "opencode_server_shutdown_failed");
    }
    for turn in &turns {
      turn.push(TurnEvent::ServerLost(reason.clone()));
    }
    tracing::error!(target: "charliebot_agents", reason, turns = turns.len(), "opencode_server_lost");
  }

  fn arm_idle(self: &Arc<Self>) {
    let Ok(timeouts) = self.timeouts() else {
      return;
    };
    let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
    if let Some(idle) = state.idle.take() {
      idle.abort();
    }
    let weak = Arc::downgrade(self);
    state.idle = Some(tokio::spawn(async move {
      tokio::time::sleep(timeouts.idle_exit).await;
      if let Some(server) = weak.upgrade() {
        server.stop_when_idle().await;
      }
    }));
  }

  async fn stop_when_idle(self: &Arc<Self>) {
    let _guard = self.start_lock.lock().await;
    let idle = {
      let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
      if !state.turns.is_empty() || state.process.as_ref().is_none_or(|process| !process.alive()) {
        state.idle = None;
        return;
      }
      state.stopping = true;
      state.process.as_ref().map(|process| process.pid)
    };
    if let Some(pid) = idle {
      tracing::info!(target: "charliebot_agents", pid, "opencode_server_idle_exit");
    }
    if let Err(error) = self.shutdown_locked().await {
      tracing::error!(target: "charliebot_agents", error = %error, "opencode_server_idle_shutdown_failed");
    }
  }

  async fn shutdown_locked(&self) -> anyhow::Result<()> {
    let (process, tasks, idle) = {
      let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
      state.stopping = true;
      (state.process.clone(), std::mem::take(&mut state.tasks), state.idle.take())
    };
    if let Some(process) = &process
      && process.alive()
    {
      kill_process_group(process.pid, Signal::SIGTERM);
      if tokio::time::timeout(self.timeouts()?.stop, wait_exit(process)).await.is_err() {
        tracing::warn!(target: "charliebot_agents", pid = process.pid, "opencode_server_stop_timeout");
        kill_process_group(process.pid, Signal::SIGKILL);
        wait_exit(process).await?;
      }
    }
    let current = tokio::task::try_id();
    if let Some(idle) = idle {
      abort_and_join(idle, current).await;
    }
    for task in tasks {
      abort_and_join(task, current).await;
    }
    let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
    state.process = None;
    state.connected = None;
    state.connected_tx = None;
    state.reader_done = None;
    state.reader_done_tx = None;
    state.version = None;
    state.pending.clear();
    state.settled.clear();
    Ok(())
  }

  fn track(&self, task: JoinHandle<()>) {
    self.state.lock().expect("opencode server state mutex is not poisoned").tasks.push(task);
  }

  fn add_pending(&self, session_id: &str) {
    *self
      .state
      .lock()
      .expect("opencode server state mutex is not poisoned")
      .pending
      .entry(session_id.to_string())
      .or_default() += 1;
  }

  fn settle_one(&self, session_id: &str) {
    let notify = {
      let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
      let count = state.pending.entry(session_id.to_string()).or_default();
      if *count > 0 {
        *count -= 1;
      }
      state.settled.get(session_id).cloned()
    };
    if let Some(notify) = notify {
      notify.notify_one();
    }
  }

  fn pending_count(&self, session_id: &str) -> usize {
    self
      .state
      .lock()
      .expect("opencode server state mutex is not poisoned")
      .pending
      .get(session_id)
      .copied()
      .unwrap_or(0)
  }

  fn settled_notify(&self, session_id: &str) -> Arc<Notify> {
    Arc::clone(
      self
        .state
        .lock()
        .expect("opencode server state mutex is not poisoned")
        .settled
        .entry(session_id.to_string())
        .or_insert_with(|| Arc::new(Notify::new())),
    )
  }
}

/// Finish the turn whose pinned process is *pid*, when it has registered its session.
pub(crate) async fn finish_active_turn(pid: u32) -> anyhow::Result<()> {
  let turn = ACTIVE_TURNS
    .get_or_init(|| Mutex::new(HashMap::new()))
    .lock()
    .expect("opencode active turn map is not poisoned")
    .get(&pid)
    .and_then(Weak::upgrade);
  if let Some(turn) = turn {
    turn.finish().await?;
  }
  Ok(())
}

async fn read_events(
  process: Arc<ServerProcess>,
  connected: watch::Sender<bool>,
  server: Weak<OpenCodeServer>,
) -> String {
  let result = async {
    let (client, base_url) = process.client().context("opening opencode event stream client")?;
    let response = client
      .get(format!("{base_url}/api/event"))
      .basic_auth("opencode", Some(&process.password))
      .send()
      .await?
      .error_for_status()?;
    let mut stream = response.bytes_stream();
    let mut pending = Vec::new();
    while let Some(chunk) = stream.next().await {
      let chunk = chunk?;
      pending.extend_from_slice(&chunk);
      while let Some(index) = pending.iter().position(|byte| *byte == b'\n') {
        let mut line = pending.drain(..=index).collect::<Vec<_>>();
        if line.last() == Some(&b'\n') {
          line.pop();
        }
        if line.last() == Some(&b'\r') {
          line.pop();
        }
        if line.starts_with(b"data:") {
          let event = serde_json::from_slice::<Value>(&line[5..])?;
          if event.get("type").and_then(Value::as_str) == Some("server.connected") {
            let _ = connected.send(true);
          }
          if let Some(server) = server.upgrade() {
            server.dispatch(event);
          }
        }
      }
    }
    Ok::<String, anyhow::Error>("opencode event stream closed".to_string())
  }
  .await;
  let mut reason = match result {
    Ok(reason) => reason,
    Err(error) => format!("opencode event stream failed: {error}"),
  };
  let grace = server
    .upgrade()
    .and_then(|server| server.timeouts().ok())
    .map_or(Duration::ZERO, |timeouts| timeouts.stream_exit_grace);
  if process.alive() && grace > Duration::ZERO {
    let _ = tokio::time::timeout(grace, wait_exit(&process)).await;
  }
  if let Some(Ok(code)) = process.exit.borrow().clone() {
    reason = format!("opencode serve exited with code {code}: {}", process.tail_text());
  }
  reason
}

async fn drain_stream<R>(mut stream: R, tail: Arc<Mutex<VecDeque<u8>>>)
where
  R: AsyncRead + Unpin,
{
  let mut chunk = vec![0; READ_CHUNK_BYTES];
  loop {
    match stream.read(&mut chunk).await {
      Ok(0) => return,
      Ok(count) => append_tail(&tail, &chunk[..count]),
      Err(error) => {
        tracing::debug!(target: "charliebot_agents", error = %error, "opencode_output_drain_failed");
        return;
      }
    }
  }
}

async fn wait_exit(process: &ServerProcess) -> anyhow::Result<i32> {
  let mut exit = process.exit.clone();
  loop {
    if let Some(result) = exit.borrow().clone() {
      return result.map_err(anyhow::Error::msg);
    }
    exit.changed().await.context("opencode serve exit channel closed")?;
  }
}

async fn abort_and_join(task: JoinHandle<()>, current: Option<Id>) {
  if current == Some(task.id()) {
    return;
  }
  if !task.is_finished() {
    task.abort();
  }
  if let Err(error) = task.await
    && !error.is_cancelled()
  {
    tracing::warn!(target: "charliebot_agents", error = %error, "opencode_server_task_join_failed");
  }
}

fn append_tail(tail: &Mutex<VecDeque<u8>>, data: &[u8]) {
  let mut tail = tail.lock().expect("opencode output tail is not poisoned");
  tail.extend(data);
  while tail.len() > OUTPUT_TAIL_BYTES {
    tail.pop_front();
  }
}
