//! The shared opencode server and its turn ownership (`src/backends/opencode/server.py`).

use std::collections::HashMap;
#[cfg(test)]
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use anyhow::{Context, bail};
use charliebot_core::config::{BackendOption, sessions_dir_in};
use charliebot_core::home;
use charliebot_core::repo_root::REPO_ROOT_ENV;
use charliebot_core::run_token::{RUN_TOKEN_ENV, SESSION_ID_ENV};
use charliebot_registry::backend::Env;
use futures_util::StreamExt;
use memchr::memchr;
use nix::sys::signal::Signal;
use regex::Regex;
use reqwest::{Client, Method, RequestBuilder};
use serde_json::{Value, json};
use tokio::sync::{Notify, watch};
use tokio::task::{Id, JoinHandle};
use uuid::Uuid;

use super::opencode_limits;
use super::options::model_overrides_config;
use super::relay::{self, ServerRecord, TurnRecord};
use charliebot_agents::cli_base::apply_proxy_env;
use charliebot_agents::transport::cgroup::SessionCgroupConfig;
use charliebot_agents::transport::identity::read_pid_stat;
use charliebot_agents::transport::kill::{exit_code, kill_process_group};
use charliebot_agents::transport::spawn::{SpawnSetup, SpawnSpec, SpawnedChild, spawn_detached_files};
use charliebot_store::metadata::MetadataReadError;
use charliebot_store::run_records::read_run_record_file;

const OUTPUT_TAIL_BYTES: usize = 64 * 1024;
static LISTEN_RE: OnceLock<Regex> = OnceLock::new();
static ACTIVE_TURNS: OnceLock<Mutex<HashMap<u32, Weak<Turn>>>> = OnceLock::new();

type AdoptedRun = (charliebot_store::run_records::RunRecord, PathBuf, TurnRecord);
type AdoptionGroup = (ServerRecord, PathBuf, Vec<AdoptedRun>);

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
    let repo_root = std::env::var_os(REPO_ROOT_ENV)
      .map(PathBuf::from)
      .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../.."));
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
    self.store_dir().join(home::OPENCODE_XDG_DATA_DIR_NAME)
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
  pub heavy_check: Duration,
  pub heavy_rss_mb: f64,
  #[cfg(test)]
  pub heavy_rss_override: Option<f64>,
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
      heavy_check: opencode_limits::OPENCODE_SERVER_HEAVY_CHECK_SECONDS,
      heavy_rss_mb: opencode_limits::OPENCODE_SERVER_HEAVY_RSS_MB,
      #[cfg(test)]
      heavy_rss_override: None,
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
  adopted: Mutex<HashMap<String, Arc<OpenCodeServer>>>,
  adoption_lock: Mutex<()>,
  adoption_complete: AtomicBool,
  /// The backend options of the config the latest launch was built from. A shared server reads them when it starts.
  options: Mutex<Vec<BackendOption>>,
}

impl OpenCodeServerPool {
  pub fn new(host: OpenCodeHost, timeouts: ServerTimeouts) -> Arc<Self> {
    Arc::new(Self {
      host,
      timeouts,
      servers: Mutex::new(HashMap::new()),
      adopted: Mutex::new(HashMap::new()),
      adoption_lock: Mutex::new(()),
      adoption_complete: AtomicBool::new(false),
      options: Mutex::new(Vec::new()),
    })
  }

  /// Record the backend options of the config that a launch was built from, for the next start of a shared server.
  pub fn record_options(&self, options: &[BackendOption]) {
    *self.options.lock().expect("opencode options mutex is not poisoned") = options.to_vec();
  }

  fn recorded_options(&self) -> Vec<BackendOption> {
    self.options.lock().expect("opencode options mutex is not poisoned").clone()
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
    let mut servers =
      self.servers.lock().expect("opencode server pool mutex is not poisoned").values().cloned().collect::<Vec<_>>();
    servers.extend(self.adopted.lock().expect("opencode adopted server mutex is not poisoned").values().cloned());
    for server in servers {
      server.stop().await?;
    }
    Ok(())
  }

  #[cfg(test)]
  pub(crate) fn adopted_servers_test(&self) -> Vec<Arc<OpenCodeServer>> {
    self.adopted.lock().expect("opencode adopted server mutex is not poisoned").values().cloned().collect()
  }

  #[cfg(test)]
  pub(crate) async fn abandon_for_restart_test(&self) {
    let servers =
      self.servers.lock().expect("opencode server pool mutex is not poisoned").values().cloned().collect::<Vec<_>>();
    for server in servers {
      let (tasks, idle) = {
        let mut state = server.state.lock().expect("opencode server state mutex is not poisoned");
        state.stopping = true;
        (std::mem::take(&mut state.tasks), state.idle.take())
      };
      if let Some(idle) = idle {
        abort_and_join(idle, tokio::task::try_id()).await;
      }
      for task in tasks {
        abort_and_join(task, tokio::task::try_id()).await;
      }
    }
  }

  /// Register every live relay on its original serve before opening that process's control event stream.
  pub fn adopt_existing(self: &Arc<Self>) -> anyhow::Result<()> {
    if self.adoption_complete.load(Ordering::Acquire) {
      return Ok(());
    }
    let _guard = self.adoption_lock.lock().expect("opencode adoption mutex is not poisoned");
    if self.adoption_complete.load(Ordering::Acquire) {
      return Ok(());
    }
    self.adopt_live_servers()?;
    self.adoption_complete.store(true, Ordering::Release);
    Ok(())
  }

  fn adopt_live_servers(self: &Arc<Self>) -> anyhow::Result<()> {
    let profile_home = &self.host.profile_home;
    let server_directory = profile_home.join(home::OPENCODE_DIR_NAME).join(relay::SERVER_DIRECTORY_NAME);
    let sessions_directory = sessions_dir_in(profile_home);
    let opencode_backend_ids = self
      .recorded_options()
      .into_iter()
      .filter(|option| option.backend_type == "opencode")
      .map(|option| option.id)
      .collect::<std::collections::HashSet<_>>();
    if !sessions_directory.exists() {
      return Ok(());
    }
    let mut live_runs = Vec::new();
    for session in fs::read_dir(&sessions_directory)? {
      let session = session?;
      let runs_directory = charliebot_store::layout::runs_dir(&session.path());
      if !runs_directory.is_dir() {
        continue;
      }
      for run in fs::read_dir(runs_directory)? {
        let run = run?;
        let metadata_path = run.path().join(charliebot_store::run_records::RUN_METADATA_NAME);
        if !metadata_path.is_file() {
          continue;
        }
        let record = read_run_record_file(&metadata_path).map_err(|error| match error {
          MetadataReadError::Io(error) => {
            anyhow::Error::new(error).context(format!("reading {}", metadata_path.display()))
          }
          MetadataReadError::Json(message) => {
            anyhow::Error::msg(message).context(format!("decoding {}", metadata_path.display()))
          }
        })?;
        if record.ended_at.is_some()
          || !record.backend.as_ref().is_some_and(|backend| opencode_backend_ids.contains(backend))
          || !record.pid.zip(record.pid_start.as_deref()).is_some_and(|(pid, start)| relay::same_process(pid, start))
        {
          continue;
        }
        live_runs.push((record, run.path()));
      }
    }

    let mut groups: HashMap<String, AdoptionGroup> = HashMap::new();
    for (run, run_directory) in live_runs {
      let Some(turn) = relay::read_turn_record(&run_directory)? else {
        if let Some(pid) = run.pid {
          kill_process_group(pid, Signal::SIGTERM);
        }
        continue;
      };
      relay::validate_server_record_name(&turn.server_record)?;
      let record_path = server_directory.join(&turn.server_record);
      let record = relay::read_server_record(&record_path).map_err(|error| match error {
        relay::ServerRecordReadError::Read(error) => {
          anyhow::Error::new(error).context(format!("reading OpenCode server record {}", record_path.display()))
        }
        relay::ServerRecordReadError::Decode(error) => {
          anyhow::Error::new(error).context(format!("decoding OpenCode server record {}", record_path.display()))
        }
      })?;
      if !relay::same_process(record.pid, &record.pid_start) {
        continue;
      }
      groups.entry(turn.server_record.clone()).or_insert_with(|| (record, record_path, Vec::new())).2.push((
        run,
        run_directory,
        turn,
      ));
    }

    let active_records = groups.keys().cloned().collect::<std::collections::HashSet<_>>();
    for (record_name, (record, record_path, turns)) in groups {
      let server = Arc::new(OpenCodeServer::new(Arc::clone(self), record.proxy_url.clone()));
      server.adopt_process(record.clone(), record_path, turns)?;
      self
        .adopted
        .lock()
        .expect("opencode adopted server mutex is not poisoned")
        .insert(record_name, Arc::clone(&server));
      server.start_adopted_reader();
    }
    if server_directory.is_dir() {
      for entry in fs::read_dir(&server_directory)? {
        let entry = entry?;
        if entry.path().extension().and_then(|extension| extension.to_str()) != Some("json") {
          continue;
        }
        let record_name = entry.file_name().to_string_lossy().into_owned();
        if active_records.contains(&record_name) {
          continue;
        }
        let record = relay::read_server_record(&entry.path()).map_err(|error| match error {
          relay::ServerRecordReadError::Read(error) => anyhow::Error::new(error),
          relay::ServerRecordReadError::Decode(error) => {
            anyhow::Error::new(error).context(format!("decoding OpenCode server record {}", entry.path().display()))
          }
        })?;
        if relay::same_process(record.pid, &record.pid_start) {
          kill_process_group(record.pid, Signal::SIGTERM);
        }
        match fs::remove_file(entry.path()) {
          Ok(()) => {}
          Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
          Err(error) => return Err(error).with_context(|| format!("removing {}", entry.path().display())),
        }
      }
    }
    Ok(())
  }
}

/// The server's headless policy, with the `providers` block of the options' context limits. CharlieBot has no path to
/// approve interactive requests.
pub fn headless_config(options: &[BackendOption]) -> anyhow::Result<Value> {
  let mut config = json!({
    "permission": {
      "*": "allow",
      "question": "deny",
      "subagent": "deny",
      "opencode_session_move": "deny"
    },
    "shell": "bash",
    "default_agent": "charliebot",
    "agent": {"charliebot": {"mode": "primary"}}
  });
  config.as_object_mut().expect("the headless config is a JSON object").extend(model_overrides_config(options)?);
  Ok(config)
}

/// One turn's claim on an opencode session.
pub struct Turn {
  pub session_id: String,
  pub directory: String,
  process_id: u32,
  process_start: Option<String>,
  server: Weak<OpenCodeServer>,
  claim_held: bool,
  relay_only: bool,
  adopted: bool,
  cleanup_shells: bool,
  #[cfg(test)]
  events: Mutex<VecDeque<TurnEvent>>,
  #[cfg(test)]
  ready: Notify,
  finished: AtomicBool,
}

struct TurnRegistration {
  claim_held: bool,
  relay_only: bool,
  adopted: bool,
  process_start: Option<String>,
}

#[derive(Debug)]
#[cfg(test)]
pub(crate) enum TurnEvent {
  Event(Value),
}

impl Turn {
  #[cfg(test)]
  fn push(&self, event: TurnEvent) {
    self.events.lock().expect("opencode turn queue is not poisoned").push_back(event);
    self.ready.notify_one();
  }

  #[cfg(test)]
  pub(crate) async fn next_event(&self) -> TurnEvent {
    loop {
      let notified = self.ready.notified();
      if let Some(event) = self.events.lock().expect("opencode turn queue is not poisoned").pop_front() {
        return event;
      }
      notified.await;
    }
  }

  /// Unregister this turn and run its session cleanup.
  pub async fn finish(&self) -> anyhow::Result<()> {
    self.finish_inner(!self.adopted).await
  }

  async fn finish_for_stop(&self) -> anyhow::Result<()> {
    self.finish_inner(false).await
  }

  async fn finish_inner(&self, interrupt: bool) -> anyhow::Result<()> {
    if self.finished.swap(true, Ordering::AcqRel) {
      return Ok(());
    }
    let active = ACTIVE_TURNS.get_or_init(|| Mutex::new(HashMap::new()));
    active.lock().expect("opencode active turn map is not poisoned").remove(&self.process_id);
    let Some(server) = self.server.upgrade() else {
      return Ok(());
    };
    let turn_steps = if !server.process_alive() {
      tracing::info!(target: "charliebot_agents", session_id = %self.session_id, "opencode_turn_end_steps_skipped");
      Ok(())
    } else {
      async {
        if interrupt {
          server.interrupt(&self.session_id).await?;
        }
        if self.cleanup_shells {
          server.delete_shells(&self.session_id, &self.directory).await?;
        }
        Ok(())
      }
      .await
    };
    server.unregister(self);
    let release = if self.claim_held { server.release_claim(&self.directory).await } else { Ok(()) };
    match (turn_steps, release) {
      (Err(error), Err(release_error)) => {
        tracing::error!(
          target: "charliebot_agents",
          directory = %self.directory,
          error = %release_error,
          "opencode_release_failed_during_turn_cleanup"
        );
        Err(error)
      }
      (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
      (Ok(()), Ok(())) => Ok(()),
    }
  }
}

struct ServerProcess {
  pid: u32,
  pid_start: String,
  exit: watch::Receiver<Option<Result<i32, String>>>,
  base_url: Mutex<Option<String>>,
  client: Mutex<Option<Client>>,
  password: String,
  record_path: PathBuf,
  log_path: PathBuf,
  adopted: bool,
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
    let mut file = match File::open(&self.log_path) {
      Ok(file) => file,
      Err(error) => {
        tracing::debug!(target: "charliebot_agents", path = %self.log_path.display(), error = %error, "opencode_log_tail_unavailable");
        return String::new();
      }
    };
    let length = match file.metadata() {
      Ok(metadata) => metadata.len(),
      Err(error) => {
        tracing::debug!(target: "charliebot_agents", path = %self.log_path.display(), error = %error, "opencode_log_tail_unavailable");
        return String::new();
      }
    };
    let start = length.saturating_sub(OUTPUT_TAIL_BYTES as u64);
    if file.seek(SeekFrom::Start(start)).is_err() {
      tracing::debug!(target: "charliebot_agents", path = %self.log_path.display(), "opencode_log_tail_seek_failed");
      return String::new();
    }
    let mut bytes = Vec::with_capacity((length - start) as usize);
    if file.read_to_end(&mut bytes).is_err() {
      tracing::debug!(target: "charliebot_agents", path = %self.log_path.display(), "opencode_log_tail_read_failed");
      return String::new();
    }
    String::from_utf8_lossy(&bytes).trim().to_string()
  }
}

/// One opencode process, its HTTP API, event stream, and active turns.
pub struct OpenCodeServer {
  pool: Weak<OpenCodeServerPool>,
  proxy_url: Option<String>,
  start_lock: tokio::sync::Mutex<()>,
  release_lock: tokio::sync::Mutex<()>,
  state: Mutex<ServerState>,
}

struct ServerState {
  process: Option<Arc<ServerProcess>>,
  turns: HashMap<String, Arc<Turn>>,
  claims: HashMap<String, usize>,
  pending: HashMap<String, usize>,
  settled: HashMap<String, Arc<Notify>>,
  connected: Option<watch::Receiver<bool>>,
  connected_tx: Option<watch::Sender<bool>>,
  reader_done: Option<watch::Receiver<Option<String>>>,
  reader_done_tx: Option<watch::Sender<Option<String>>>,
  version: Option<String>,
  stopping: bool,
  adopted: bool,
  tasks: Vec<JoinHandle<()>>,
  idle: Option<JoinHandle<()>>,
  #[cfg(test)]
  test_start_observation: Option<(String, Arc<Mutex<Vec<usize>>>)>,
  #[cfg(test)]
  test_heavy_check_count: usize,
  #[cfg(test)]
  test_heavy_shutdown_gate: Option<(Arc<AtomicBool>, Arc<Notify>)>,
  #[cfg(test)]
  test_release_claim_gate: Option<(Arc<Notify>, Arc<Notify>)>,
  #[cfg(test)]
  test_idle_release_lock_waiting: Option<Arc<Notify>>,
}

impl OpenCodeServer {
  fn new(pool: Arc<OpenCodeServerPool>, proxy_url: Option<String>) -> Self {
    Self {
      pool: Arc::downgrade(&pool),
      proxy_url,
      start_lock: tokio::sync::Mutex::new(()),
      release_lock: tokio::sync::Mutex::new(()),
      state: Mutex::new(ServerState {
        process: None,
        turns: HashMap::new(),
        claims: HashMap::new(),
        pending: HashMap::new(),
        settled: HashMap::new(),
        connected: None,
        connected_tx: None,
        reader_done: None,
        reader_done_tx: None,
        version: None,
        stopping: false,
        adopted: false,
        tasks: Vec::new(),
        idle: None,
        #[cfg(test)]
        test_start_observation: None,
        #[cfg(test)]
        test_heavy_check_count: 0,
        #[cfg(test)]
        test_heavy_shutdown_gate: None,
        #[cfg(test)]
        test_release_claim_gate: None,
        #[cfg(test)]
        test_idle_release_lock_waiting: None,
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

  pub(crate) fn record_name(&self) -> anyhow::Result<String> {
    let process = self.process()?;
    relay::server_record_name(&process.record_path)
  }

  pub(crate) fn failure_reason(&self) -> Option<String> {
    let (process, stream_reason) = {
      let state = self.state.lock().expect("opencode server state mutex is not poisoned");
      (state.process.clone(), state.reader_done.as_ref().and_then(|done| done.borrow().clone()))
    };
    let process = process?;
    // The event reader records why the stream ended after giving the process its exit grace, so that reason already
    // carries the exit code when the process died. When the server loses its stream while running, the stop that
    // follows also reaps the process; preferring the raw exit status here would report that kill instead of the loss.
    if let Some(reason) = stream_reason {
      return Some(reason);
    }
    match process.exit.borrow().clone() {
      Some(Ok(code)) => Some(format!("opencode serve exited with code {code}: {}", process.tail_text())),
      Some(Err(error)) => Some(format!("opencode serve wait failed: {error}")),
      None => None,
    }
  }

  pub(crate) async fn failure_reason_after_stream_end(&self) -> Option<String> {
    let process = self.state.lock().expect("opencode server state mutex is not poisoned").process.clone()?;
    if !relay::same_process(process.pid, &process.pid_start) {
      return match wait_exit(&process).await {
        Ok(_) => self.failure_reason(),
        Err(error) => Some(format!("opencode serve wait failed: {error}")),
      };
    }
    let reader_done = self.state.lock().expect("opencode server state mutex is not poisoned").reader_done.clone();
    if let Some(mut reader_done) = reader_done
      && reader_done.borrow().is_none()
    {
      match tokio::time::timeout(Duration::from_secs(1), reader_done.changed()).await {
        Ok(Ok(())) | Err(_) => {}
        Ok(Err(error)) => {
          tracing::warn!(target: "charliebot_agents", error = %error, "opencode_reader_done_wait_failed");
        }
      }
    }
    self.failure_reason()
  }

  fn adopt_process(
    self: &Arc<Self>,
    record: ServerRecord,
    record_path: PathBuf,
    turns: Vec<(charliebot_store::run_records::RunRecord, PathBuf, TurnRecord)>,
  ) -> anyhow::Result<()> {
    relay::ensure_crypto_provider()?;
    let client = Client::builder().no_proxy().build()?;
    let (exit_tx, exit) = watch::channel(None);
    let process = Arc::new(ServerProcess {
      pid: record.pid,
      pid_start: record.pid_start.clone(),
      exit,
      base_url: Mutex::new(Some(record.base_url.clone())),
      client: Mutex::new(Some(client)),
      password: record.password.clone(),
      record_path,
      log_path: PathBuf::from(&record.log_path),
      adopted: true,
    });
    let (connected_tx, connected) = watch::channel(false);
    let (reader_done_tx, reader_done) = watch::channel(None);
    {
      let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
      state.process = Some(Arc::clone(&process));
      state.version = Some(record.version);
      state.connected = Some(connected);
      state.connected_tx = Some(connected_tx);
      state.reader_done = Some(reader_done);
      state.reader_done_tx = Some(reader_done_tx);
      state.adopted = true;
    }
    for (run, run_directory, turn) in turns {
      let process_id = run.pid.context("live adopted OpenCode run has no process id")?;
      let process_start = run.pid_start.context("live adopted OpenCode run has no process start time")?;
      self.register_adopted(&turn.session_id, &run_directory.display().to_string(), process_id, process_start)?;
    }
    let pid = record.pid;
    let pid_start = record.pid_start;
    self.track(tokio::spawn(async move {
      loop {
        if !relay::same_process(pid, &pid_start) {
          let _ = exit_tx.send(Some(Ok(0)));
          return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
      }
    }));
    Ok(())
  }

  fn start_adopted_reader(self: &Arc<Self>) {
    let process = self.process().expect("adopted process was registered before its reader");
    self.start_event_reader(process);
    self.start_exit_watcher();
    let turns = self
      .state
      .lock()
      .expect("opencode server state mutex is not poisoned")
      .turns
      .values()
      .cloned()
      .collect::<Vec<_>>();
    for turn in turns {
      self.track(tokio::spawn(async move {
        let start = turn.process_start.as_deref().expect("an adopted turn has a pinned relay process");
        while relay::same_process(turn.process_id, start) {
          tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if let Err(error) = turn.finish().await {
          tracing::error!(target: "charliebot_agents", session_id = %turn.session_id, error = %error, "opencode_adopted_turn_end_failed");
        }
      }));
    }
    if self.state.lock().expect("opencode server state mutex is not poisoned").turns.is_empty() {
      let server = Arc::clone(self);
      self.track(tokio::spawn(async move {
        if let Err(error) = server.stop().await {
          tracing::error!(target: "charliebot_agents", error = %error, "opencode_orphan_server_stop_failed");
        }
      }));
    }
  }

  fn start_event_reader(self: &Arc<Self>, process: Arc<ServerProcess>) {
    let weak = Arc::downgrade(self);
    let (connected_tx, reader_done_tx) = {
      let state = self.state.lock().expect("opencode server state mutex is not poisoned");
      (
        state.connected_tx.as_ref().expect("connected channel was registered").clone(),
        state.reader_done_tx.as_ref().expect("reader channel was registered").clone(),
      )
    };
    self.track(tokio::spawn(async move {
      let reason = read_events(Arc::clone(&process), connected_tx, weak.clone()).await;
      let _ = reader_done_tx.send(Some(reason.clone()));
      if let Some(server) = weak.upgrade() {
        server.lose(reason).await;
      }
    }));
  }

  fn start_exit_watcher(self: &Arc<Self>) {
    let process = self.process().expect("server process was registered before its watcher");
    let weak = Arc::downgrade(self);
    self.track(tokio::spawn(async move {
      let code = wait_exit(&process).await;
      if let Some(server) = weak.upgrade() {
        let reason = match code {
          Ok(code) => format!("opencode serve exited with code {code}: {}", process.tail_text()),
          Err(error) => format!("opencode serve wait failed: {error}"),
        };
        server.lose(reason).await;
      }
    }));
  }

  /// Build the server environment from CharlieBot's environment and the profile store paths.
  pub fn server_env(&self, password: &str) -> anyhow::Result<Env> {
    self.server_env_from(std::env::vars().collect::<Env>(), password)
  }

  pub(crate) fn server_env_from(&self, mut env: Env, password: &str) -> anyhow::Result<Env> {
    let host = self.pool()?.host.clone();
    env.remove(RUN_TOKEN_ENV);
    env.remove(SESSION_ID_ENV);
    env.extend(self.pool()?.host.extra_env.clone());
    env.insert("XDG_DATA_HOME".to_string(), host.data_dir().display().to_string());
    env.insert("XDG_STATE_HOME".to_string(), host.state_dir().display().to_string());
    env.insert("XDG_CACHE_HOME".to_string(), host.cache_dir().display().to_string());
    env.insert("OPENCODE_SERVER_PASSWORD".to_string(), password.to_string());
    env.insert("OPENCODE_CONFIG_CONTENT".to_string(), headless_config(&self.pool()?.recorded_options())?.to_string());
    env.insert("OPENCODE_DISABLE_FFF".to_string(), "1".to_string());
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

  #[cfg(test)]
  pub(crate) fn observe_claim_before_start_test(&self, directory: &str) -> Arc<Mutex<Vec<usize>>> {
    let observed = Arc::new(Mutex::new(Vec::new()));
    self.state.lock().expect("opencode server state mutex is not poisoned").test_start_observation =
      Some((directory.to_string(), Arc::clone(&observed)));
    observed
  }

  #[cfg(test)]
  pub(crate) fn claim_count_test(&self, directory: &str) -> usize {
    self.state.lock().expect("opencode server state mutex is not poisoned").claims.get(directory).copied().unwrap_or(0)
  }

  #[cfg(test)]
  pub(crate) async fn claim_test(&self, directory: &str) {
    self.claim(directory).await;
  }

  #[cfg(test)]
  pub(crate) async fn release_claim_test(self: &Arc<Self>, directory: &str) -> anyhow::Result<()> {
    self.release_claim(directory).await
  }

  #[cfg(test)]
  pub(crate) fn process_rss_mb_test(pid: u32) -> anyhow::Result<f64> {
    process_rss_mb(pid)
  }

  #[cfg(test)]
  pub(crate) fn heavy_check_count_test(&self) -> usize {
    self.state.lock().expect("opencode server state mutex is not poisoned").test_heavy_check_count
  }

  #[cfg(test)]
  pub(crate) fn pause_heavy_shutdown_test(&self) -> (Arc<AtomicBool>, Arc<Notify>) {
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(Notify::new());
    self.state.lock().expect("opencode server state mutex is not poisoned").test_heavy_shutdown_gate =
      Some((Arc::clone(&entered), Arc::clone(&release)));
    (entered, release)
  }

  #[cfg(test)]
  pub(crate) fn pause_release_claim_test(&self) -> (Arc<Notify>, Arc<Notify>) {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    self.state.lock().expect("opencode server state mutex is not poisoned").test_release_claim_gate =
      Some((Arc::clone(&entered), Arc::clone(&release)));
    (entered, release)
  }

  #[cfg(test)]
  pub(crate) fn observe_idle_release_lock_test(&self) -> Arc<Notify> {
    let waiting = Arc::new(Notify::new());
    self.state.lock().expect("opencode server state mutex is not poisoned").test_idle_release_lock_waiting =
      Some(Arc::clone(&waiting));
    waiting
  }

  #[cfg(test)]
  fn observe_start_claims_test(&self) {
    let state = self.state.lock().expect("opencode server state mutex is not poisoned");
    if let Some((directory, observed)) = &state.test_start_observation {
      observed
        .lock()
        .expect("opencode start observation is not poisoned")
        .push(state.claims.get(directory).copied().unwrap_or(0));
    }
  }

  #[cfg(test)]
  async fn wait_heavy_shutdown_gate_test(&self) {
    let gate = self.state.lock().expect("opencode server state mutex is not poisoned").test_heavy_shutdown_gate.clone();
    if let Some((entered, release)) = gate {
      entered.store(true, Ordering::Release);
      release.notified().await;
    }
  }

  #[cfg(test)]
  async fn wait_release_claim_gate_test(&self) {
    let gate = self.state.lock().expect("opencode server state mutex is not poisoned").test_release_claim_gate.clone();
    if let Some((entered, release)) = gate {
      entered.notify_one();
      release.notified().await;
    }
  }

  #[cfg(test)]
  fn signal_idle_release_lock_waiting_test(&self) {
    if let Some(waiting) =
      self.state.lock().expect("opencode server state mutex is not poisoned").test_idle_release_lock_waiting.clone()
    {
      waiting.notify_one();
    }
  }

  /// Start the server unless it is already running.
  pub async fn ensure_running(self: &Arc<Self>) -> anyhow::Result<()> {
    let _guard = self.start_lock.lock().await;
    if self.process_alive() {
      return Ok(());
    }
    if self.state.lock().expect("opencode server state mutex is not poisoned").process.is_some() {
      self.shutdown_locked(false).await?;
    }
    #[cfg(test)]
    self.observe_start_claims_test();
    if let Err(error) = self.start_locked().await {
      if let Err(cleanup_error) = self.shutdown_locked(false).await {
        tracing::error!(target: "charliebot_agents", error = %cleanup_error, "opencode_start_cleanup_failed");
      }
      return Err(error);
    }
    Ok(())
  }

  /// Stop the server process. A later turn starts a new process.
  pub async fn stop(&self) -> anyhow::Result<()> {
    let _guard = self.start_lock.lock().await;
    self.shutdown_locked(false).await
  }

  /// Hold a directory until this turn finishes.
  pub async fn claim(&self, directory: &str) {
    let _guard = self.release_lock.lock().await;
    let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
    *state.claims.entry(directory.to_string()).or_default() += 1;
  }

  /// Drop a directory claim and unload every loaded directory no turn claims.
  pub async fn release_claim(self: &Arc<Self>, directory: &str) -> anyhow::Result<()> {
    let _guard = self.release_lock.lock().await;
    let (should_arm_idle, release_ready) = {
      let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
      let count = state.claims.get_mut(directory).expect("only a claimed directory can be released");
      *count -= 1;
      if *count == 0 {
        state.claims.remove(directory);
      }
      let no_claims_or_turns = state.claims.is_empty() && state.turns.is_empty();
      let process_ready = !state.stopping
        && state.process.as_ref().is_some_and(|process| process.alive())
        && state.version.is_some()
        && state.connected.as_ref().is_some_and(|connected| *connected.borrow())
        && state.reader_done.as_ref().is_some_and(|reader_done| reader_done.borrow().is_none());
      let should_arm_idle = no_claims_or_turns && process_ready;
      (should_arm_idle, process_ready)
    };
    if should_arm_idle {
      self.arm_idle();
    }
    if !release_ready {
      return Ok(());
    }
    #[cfg(test)]
    self.wait_release_claim_gate_test().await;
    let response = self.client_request(Method::GET, "/api/debug/location")?.send().await?.error_for_status()?;
    let locations = response.json::<Value>().await?;
    let locations = locations.as_array().context("opencode location list is not an array")?;
    for location in locations {
      let loaded =
        location.get("directory").and_then(Value::as_str).context("opencode location has no string directory")?;
      let claimed = self.state.lock().expect("opencode server state mutex is not poisoned").claims.contains_key(loaded);
      if claimed {
        continue;
      }
      let response = self
        .client_request(Method::DELETE, "/api/debug/location")?
        .query(&[("location[directory]", loaded)])
        .send()
        .await?;
      if !response.status().is_success() {
        bail!("opencode release of directory {loaded} returned HTTP {}", response.status().as_u16());
      }
    }
    Ok(())
  }

  /// Register a run whose event ownership belongs to its raw-log relay.
  pub(crate) fn register_relay_claimed(
    self: &Arc<Self>,
    session_id: &str,
    directory: &str,
    process_id: u32,
  ) -> anyhow::Result<Arc<Turn>> {
    self.register_inner(
      session_id,
      directory,
      process_id,
      TurnRegistration { claim_held: true, relay_only: true, adopted: false, process_start: None },
    )
  }

  fn register_adopted(
    self: &Arc<Self>,
    session_id: &str,
    directory: &str,
    process_id: u32,
    process_start: String,
  ) -> anyhow::Result<Arc<Turn>> {
    self.register_inner(
      session_id,
      directory,
      process_id,
      TurnRegistration { claim_held: false, relay_only: true, adopted: true, process_start: Some(process_start) },
    )
  }

  /// Register a control-stream turn for dispatch tests.
  #[cfg(test)]
  pub fn register(self: &Arc<Self>, session_id: &str, directory: &str, process_id: u32) -> anyhow::Result<Arc<Turn>> {
    self.register_inner(
      session_id,
      directory,
      process_id,
      TurnRegistration { claim_held: false, relay_only: false, adopted: false, process_start: None },
    )
  }

  fn register_inner(
    self: &Arc<Self>,
    session_id: &str,
    directory: &str,
    process_id: u32,
    registration: TurnRegistration,
  ) -> anyhow::Result<Arc<Turn>> {
    let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
    if state.turns.contains_key(session_id) {
      bail!("opencode session {session_id} already has a running turn");
    }
    let turn = Arc::new(Turn {
      session_id: session_id.to_string(),
      directory: directory.to_string(),
      process_id,
      process_start: registration.process_start,
      server: Arc::downgrade(self),
      claim_held: registration.claim_held,
      relay_only: registration.relay_only,
      adopted: registration.adopted,
      cleanup_shells: true,
      #[cfg(test)]
      events: Mutex::new(VecDeque::new()),
      #[cfg(test)]
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
    let (should_idle, stop_adopted) = {
      let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
      if state.turns.get(&turn.session_id).is_some_and(|found| std::ptr::eq(found.as_ref(), turn)) {
        state.turns.remove(&turn.session_id);
      }
      let no_turns = state.turns.is_empty() && state.claims.is_empty();
      let should_idle = no_turns
        && state.claims.is_empty()
        && !state.stopping
        && state.process.as_ref().is_some_and(|process| process.alive())
        && !state.adopted;
      let stop_adopted =
        no_turns && !state.stopping && state.process.as_ref().is_some_and(|process| process.alive() && process.adopted);
      (should_idle, stop_adopted)
    };
    if should_idle {
      self.arm_idle();
    }
    if stop_adopted {
      let server = Arc::clone(self);
      self.track(tokio::spawn(async move {
        if let Err(error) = server.stop().await {
          tracing::error!(target: "charliebot_agents", error = %error, "opencode_adopted_server_stop_failed");
        }
      }));
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
    let log_path = store_dir.join("logs").join(format!("server-{}.log", Uuid::new_v4().simple()));
    let mut args = program.args.clone();
    args.extend(["serve".into(), "--port".into(), "0".into()]);
    let spec = SpawnSpec::new(
      program.program.as_os_str(),
      args,
      Some(store_dir),
      std::mem::take(&mut env).into_iter().map(|(key, value)| (key.into(), value.into())).collect(),
      None,
      SessionCgroupConfig::disabled(),
    );
    let SpawnedChild { mut child, .. } = spawn_detached_files(&spec, &log_path, &log_path, SpawnSetup::None).await?;
    let pid = child.id().context("opencode serve did not expose its process id")?;
    let pid_start = read_pid_stat(pid).context("reading opencode serve process start time")?.pid_start;
    let record_directory = pool.host.profile_home.join(home::OPENCODE_DIR_NAME).join(relay::SERVER_DIRECTORY_NAME);
    let record_path = record_directory.join(format!("server-{pid}-{pid_start}.json"));
    let (exit_tx, exit_rx) = watch::channel(None);
    let process = Arc::new(ServerProcess {
      pid,
      pid_start,
      exit: exit_rx,
      base_url: Mutex::new(None),
      client: Mutex::new(None),
      password,
      record_path,
      log_path: log_path.clone(),
      adopted: false,
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
    tokio::spawn(async move {
      let code = match child.wait().await {
        Ok(status) => Ok(exit_code(status)),
        Err(error) => Err(error.to_string()),
      };
      if exit_tx.send(Some(code)).is_err() {
        tracing::debug!(target: "charliebot_agents", pid, "opencode_exit_receiver_dropped");
      }
    });
    let url = self.read_listening_url(&process).await?;
    *process.base_url.lock().expect("opencode server URL mutex is not poisoned") = Some(url.clone());
    let client = charliebot_compat::http_client::client(&charliebot_compat::http_client::Settings::from_env())
      .context("building the opencode HTTP client")?;
    *process.client.lock().expect("opencode HTTP client mutex is not poisoned") = Some(client);
    let info = self.client_request(Method::GET, "/api/info")?.send().await?.error_for_status()?.json::<Value>().await?;
    let version = info.get("version").and_then(Value::as_str).context("opencode /api/info has no string version")?;
    if version.split('.').next() != Some("2") {
      bail!("{} reports opencode {version}; CharlieBot needs the 2.x server", program.display.display());
    }
    self.state.lock().expect("opencode server state mutex is not poisoned").version = Some(version.to_string());
    relay::write_server_record(
      &process.record_path,
      &ServerRecord {
        pid,
        pid_start: process.pid_start.clone(),
        base_url: url.clone(),
        password: process.password.clone(),
        proxy_url: self.proxy_url.clone(),
        version: version.to_string(),
        started_at: charliebot_core::timefmt::utc_now_iso(),
        log_path: process.log_path.display().to_string(),
        exit_reason: None,
      },
    )?;
    self.start_event_reader(Arc::clone(&process));
    self.start_exit_watcher();
    self.await_connected().await?;
    self.arm_idle();
    tracing::info!(target: "charliebot_agents", pid, url, version, "opencode_server_started");
    Ok(())
  }

  async fn read_listening_url(&self, process: &ServerProcess) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + self.timeouts()?.start;
    let display_path = self.pool()?.host.program.display.display().to_string();
    let timeout_seconds = self.timeouts()?.start.as_secs_f64();
    let listen_re = LISTEN_RE.get_or_init(|| Regex::new(r"server listening on (http://\S+)").unwrap());
    loop {
      let bytes = tokio::time::timeout_at(deadline, tokio::fs::read(&process.log_path)).await.map_err(|_| {
        anyhow::anyhow!("{display_path} serve did not print its listening address within {timeout_seconds} s")
      })??;
      let text = String::from_utf8_lossy(&bytes);
      if let Some(capture) = listen_re.captures(&text) {
        return Ok(capture[1].to_string());
      }
      if !process.alive() {
        bail!(
          "{} serve exited before printing its listening address: {}",
          self.pool()?.host.program.display.display(),
          process.tail_text()
        );
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
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
      if !turn.relay_only {
        #[cfg(test)]
        turn.push(TurnEvent::Event(event));
      }
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
    let process = self.state.lock().expect("opencode server state mutex is not poisoned").process.clone();
    if let Some(process) = process
      && let Err(error) = relay::write_server_exit_reason(&process.record_path, reason.clone())
    {
      tracing::error!(target: "charliebot_agents", path = %process.record_path.display(), error = %error, "opencode_server_exit_record_failed");
    }
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
    if let Err(error) = self.shutdown_locked(true).await {
      tracing::error!(target: "charliebot_agents", error = %error, "opencode_server_shutdown_failed");
    }
    tracing::error!(target: "charliebot_agents", reason, turns = turns.len(), "opencode_server_lost");
  }

  fn arm_idle(self: &Arc<Self>) {
    let Ok(timeouts) = self.timeouts() else {
      return;
    };
    let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
    if !state.turns.is_empty()
      || !state.claims.is_empty()
      || state.stopping
      || state.process.as_ref().is_none_or(|process| !process.alive())
    {
      return;
    }
    if let Some(idle) = state.idle.take() {
      idle.abort();
    }
    let weak = Arc::downgrade(self);
    state.idle = Some(tokio::spawn(async move {
      tokio::time::sleep(timeouts.heavy_check).await;
      if let Some(server) = weak.upgrade()
        && let Err(error) = server.stop_when_idle(timeouts).await
      {
        tracing::error!(target: "charliebot_agents", error = %error, "opencode_server_idle_check_failed");
      }
    }));
  }

  async fn stop_when_idle(self: &Arc<Self>, timeouts: ServerTimeouts) -> anyhow::Result<()> {
    let _guard = self.start_lock.lock().await;
    #[cfg(test)]
    self.signal_idle_release_lock_waiting_test();
    let heavy_shutdown = {
      let _release_guard = self.release_lock.lock().await;
      let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
      if state.turns.is_empty()
        && state.claims.is_empty()
        && state.process.as_ref().is_some_and(|process| process.alive())
        && !state.stopping
      {
        let pid = state.process.as_ref().expect("a live opencode server has a process").pid;
        #[cfg(test)]
        {
          state.test_heavy_check_count += 1;
        }
        #[cfg(test)]
        let rss_mb = match timeouts.heavy_rss_override {
          Some(rss_mb) => rss_mb,
          None => process_rss_mb(pid)?,
        };
        #[cfg(not(test))]
        let rss_mb = process_rss_mb(pid)?;
        if rss_mb > timeouts.heavy_rss_mb {
          state.stopping = true;
          Some((pid, rss_mb))
        } else {
          None
        }
      } else {
        None
      }
    };
    if let Some((pid, rss_mb)) = heavy_shutdown {
      tracing::info!(target: "charliebot_agents", pid, rss_mb = round_rss(rss_mb), "opencode_server_heavy_exit");
      #[cfg(test)]
      self.wait_heavy_shutdown_gate_test().await;
      self.shutdown_locked(false).await?;
      return Ok(());
    }
    drop(_guard);
    tokio::time::sleep(timeouts.idle_exit.saturating_sub(timeouts.heavy_check)).await;
    let _guard = self.start_lock.lock().await;
    let idle = {
      #[cfg(test)]
      self.signal_idle_release_lock_waiting_test();
      let _release_guard = self.release_lock.lock().await;
      let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
      if !state.turns.is_empty()
        || !state.claims.is_empty()
        || state.process.as_ref().is_none_or(|process| !process.alive())
        || state.stopping
      {
        return Ok(());
      }
      state.stopping = true;
      state.process.as_ref().map(|process| process.pid)
    };
    if let Some(pid) = idle {
      tracing::info!(target: "charliebot_agents", pid, "opencode_server_idle_exit");
    }
    if let Err(error) = self.shutdown_locked(false).await {
      tracing::error!(target: "charliebot_agents", error = %error, "opencode_server_idle_shutdown_failed");
    }
    Ok(())
  }

  async fn shutdown_locked(&self, preserve_record: bool) -> anyhow::Result<()> {
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
    if let Some(process) = &process
      && !preserve_record
    {
      match fs::remove_file(&process.record_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("removing {}", process.record_path.display())),
      }
    }
    let mut state = self.state.lock().expect("opencode server state mutex is not poisoned");
    state.process = None;
    state.connected = None;
    state.connected_tx = None;
    state.reader_done = None;
    state.reader_done_tx = None;
    state.version = None;
    state.adopted = false;
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
    if turn.adopted {
      if let Some(server) = turn.server.upgrade() {
        server.interrupt(&turn.session_id).await?;
      }
    } else if turn.relay_only {
      turn.finish_for_stop().await?;
    } else {
      turn.finish().await?;
    }
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
    let mut search_from = 0;
    while let Some(chunk) = stream.next().await {
      let chunk = chunk?;
      pending.extend_from_slice(&chunk);
      visit_complete_lines(&mut pending, &mut search_from, |line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(data) = line.strip_prefix(b"data:") {
          let event = serde_json::from_slice::<Value>(data)?;
          if event.get("type").and_then(Value::as_str) == Some("server.connected") {
            let _ = connected.send(true);
          }
          if let Some(server) = server.upgrade() {
            server.dispatch(event);
          }
        }
        Ok(())
      })?;
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

/// Visit each complete line once, then remove their shared prefix once per response chunk. This follows
/// `src/backends/opencode/server.py:352-360` without rescanning or shifting the unread suffix for every line.
fn visit_complete_lines(
  pending: &mut Vec<u8>,
  search_from: &mut usize,
  mut visit: impl FnMut(&[u8]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
  let mut line_start = 0;
  while let Some(relative) = memchr(b'\n', &pending[*search_from..]) {
    let index = *search_from + relative;
    visit(&pending[line_start..index])?;
    line_start = index + 1;
    *search_from = line_start;
  }
  if line_start > 0 {
    pending.drain(..line_start);
  }
  *search_from = pending.len();
  Ok(())
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

fn process_rss_mb(pid: u32) -> anyhow::Result<f64> {
  let process = procfs::process::Process::new(i32::try_from(pid).context("the process id fits a pid_t")?)
    .with_context(|| format!("reading resident memory for opencode process {pid}"))?;
  let status = process.status().with_context(|| format!("reading resident memory for opencode process {pid}"))?;
  let value_kb = status.vmrss.context("opencode process status has no VmRSS field")?;
  // `VmRSS` counts kilobytes.
  Ok(value_kb as f64 / 1024.0)
}

fn round_rss(rss_mb: f64) -> f64 {
  (rss_mb * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn complete_lines_are_visited_once_across_chunk_boundaries() {
    let mut pending = Vec::new();
    let mut search_from = 0;
    let mut lines = Vec::new();
    pending.extend_from_slice(b"data: one\npart");
    visit_complete_lines(&mut pending, &mut search_from, |line| {
      lines.push(line.to_vec());
      Ok(())
    })
    .unwrap();
    assert_eq!(pending, b"part");

    pending.extend_from_slice(b" two\ndata: three\n");
    visit_complete_lines(&mut pending, &mut search_from, |line| {
      lines.push(line.to_vec());
      Ok(())
    })
    .unwrap();

    assert_eq!(lines, [b"data: one".to_vec(), b"part two".to_vec(), b"data: three".to_vec()]);
    assert!(pending.is_empty());
    assert_eq!(search_from, 0);
  }
}
