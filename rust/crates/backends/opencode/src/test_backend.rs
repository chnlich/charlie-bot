use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::event_types;
use charliebot_registry::Registry;
use charliebot_registry::backend::{AgentBackend, BackendFactory, Env, LaunchArgs};
use futures_util::future::BoxFuture;
use nix::sys::signal::Signal;
use serde_json::{Map, Value, json};
use tokio::task::JoinHandle;

use super::backend::{OpenCodeFactory, extract_model_limit, image_file_attachments};
use super::opencode_limits;
use super::server;
use super::server::{OpenCodeHost, OpenCodeServer, OpenCodeServerPool, ServerTimeouts};
use super::{OpenCodeBackend, OpenCodeOption, OpenCodeTranslator};
use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::runner::{AgentRunner, RunOutcome, RunRequest};
use charliebot_agents::transport::cgroup::SessionCgroupConfig;
use charliebot_agents::transport::identity::{ProcessIdentity, read_pid_stat};
use charliebot_agents::transport::kill::kill_process_group;
use charliebot_agents::transport::{cursor, follow, recovery};

type TestRun = (Arc<AgentRunner>, Arc<Mutex<Vec<Value>>>, JoinHandle<anyhow::Result<RunOutcome>>);

struct TestRunConfig<'a> {
  prompt: &'a str,
  launch: LaunchArgs,
  proxy_url: Option<String>,
  uploaded_files: Vec<Value>,
  additions: Env,
  option: OpenCodeOption,
}

const MODEL: &str = "fakeprov/fake-model";

/// The `fake_opencode` example, which `cargo test` builds into the `examples` directory beside the profile.
fn fake_opencode_binary() -> PathBuf {
  let test_binary = std::env::current_exe().expect("the test binary path reads");
  let profile = test_binary.parent().and_then(Path::parent).expect("the test binary sits in a profile directory");
  let binary = profile.join("examples").join("fake_opencode");
  assert!(binary.is_file(), "cargo test builds the fake_opencode example at {}", binary.display());
  binary
}

fn adoption_option() -> BackendOption {
  BackendOption {
    id: "opencode-test".to_string(),
    label: "OpenCode test".to_string(),
    model: MODEL.to_string(),
    prompt_overlay: None,
    backend_type: "opencode".to_string(),
    type_fields: serde_yaml::Mapping::new(),
  }
}

fn adoption_option_with_context_limit(context_limit: u64) -> BackendOption {
  let mut option = adoption_option();
  option.type_fields =
    serde_yaml::from_str(&format!("context_limit: {context_limit}\n")).expect("test option context limit is valid");
  option
}

#[test]
#[ignore = "launched as the OpenCode run identity process by the relay tests"]
fn relay_subprocess() {
  let args =
    ["CHARLIEBOT_OPENCODE_RELAY_DIRECTORY", "CHARLIEBOT_OPENCODE_PARENT_PID", "CHARLIEBOT_OPENCODE_PARENT_START"]
      .map(|key| std::env::var_os(key).expect("relay process environment contains its identity"));
  super::relay::run_args(&args).expect("OpenCode relay subprocess completed");
}

#[tokio::test]
#[ignore = "server-side process for the OpenCode restart test"]
async fn restart_owner_subprocess() {
  let harness = Harness::new(json!({"prompts": {"paused": {"events": [usage_event(5, 2)], "hold": true}}}));
  let (_, _, _task) = harness.start_run("paused", LaunchArgs::default(), None, vec![], Env::new());
  harness
    .wait_for_log(|logs| logs.iter().any(|entry| entry.get("prompt_text").and_then(Value::as_str) == Some("paused")))
    .await;
  let directory = run_directory(&harness);
  harness.wait_for_raw_cursor(&directory).await;
  fs::write(std::env::var_os("TEST_OWNER_READY").unwrap(), directory.to_string_lossy().as_bytes()).unwrap();
  std::future::pending::<()>().await;
}

struct RestartProcessGuard {
  owner: std::process::Child,
  processes: Vec<ProcessIdentity>,
}

impl Drop for RestartProcessGuard {
  fn drop(&mut self) {
    let _ = self.owner.kill();
    let _ = self.owner.wait();
    for process in &self.processes {
      if process.pid_start.as_deref().is_some_and(|start| super::relay::same_process(process.pid, start)) {
        kill_process_group(process.pid, Signal::SIGKILL);
      }
    }
  }
}

#[tokio::test]
async fn killed_server_process_leaves_relay_and_serve_for_adoption() {
  let scratch = tempfile::tempdir().unwrap();
  let ready = scratch.path().join("ready");
  let owner = Command::new(std::env::current_exe().unwrap())
    .args(["--exact", "test_backend::restart_owner_subprocess", "--ignored"])
    .env("TMPDIR", scratch.path())
    .env("TEST_OWNER_READY", &ready)
    .spawn()
    .unwrap();
  let mut guard = RestartProcessGuard { owner, processes: Vec::new() };
  tokio::time::timeout(Duration::from_secs(5), async {
    while !ready.exists() {
      assert!(guard.owner.try_wait().unwrap().is_none(), "server-side process exited before its turn started");
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("the server-side process started its turn");
  let directory = PathBuf::from(fs::read_to_string(&ready).unwrap());
  let profile = directory.ancestors().nth(5).unwrap().to_path_buf();
  let turn = super::relay::read_turn_record(&directory).unwrap().unwrap();
  let record_path = profile.join("opencode/servers").join(&turn.server_record);
  let record: super::relay::ServerRecord = serde_json::from_slice(&fs::read(record_path).unwrap()).unwrap();
  let run: charliebot_store::run_records::RunRecord =
    serde_json::from_slice(&fs::read(directory.join(charliebot_store::run_records::RUN_METADATA_NAME)).unwrap())
      .unwrap();
  guard.processes.push(ProcessIdentity { pid: record.pid, pid_start: Some(record.pid_start.clone()) });
  guard.processes.push(ProcessIdentity { pid: run.pid.unwrap(), pid_start: run.pid_start.clone() });
  guard.owner.kill().unwrap();
  guard.owner.wait().unwrap();

  let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..");
  let fake = fake_opencode_binary();
  let host = OpenCodeHost::with_program(fake.clone(), Vec::new(), fake, root, profile, Env::new());
  let pool = OpenCodeServerPool::new(host, ServerTimeouts::default());
  pool.record_options(&[adoption_option()]);
  pool.adopt_existing().unwrap();
  assert_eq!(pool.adopted_servers_test().len(), 1);
  let (translator, cursor) = replay_translator_prefix(&directory);
  let delivered = Arc::new(Mutex::new(Vec::new()));
  let pid = run.pid.unwrap();
  let start = run.pid_start.unwrap();
  let output = Arc::clone(&delivered);
  let follow_dir = directory.clone();
  let follow = tokio::spawn(async move {
    follow_from_cursor(&follow_dir, cursor, translator, move || super::relay::same_process(pid, &start), output).await
  });
  let client = reqwest::Client::builder().no_proxy().build().unwrap();
  for mut event in
    [text_event("answer after restart"), usage_event(3, 1), json!({"type": "session.execution.succeeded", "data": {}})]
  {
    event["data"]["sessionID"] = json!(turn.session_id);
    client
      .post(format!("{}/__control", record.base_url))
      .basic_auth("opencode", Some(&record.password))
      .json(&json!({"emit": event}))
      .send()
      .await
      .unwrap()
      .error_for_status()
      .unwrap();
  }
  assert!(tokio::time::timeout(Duration::from_secs(5), follow).await.unwrap().unwrap().unwrap().saw_result);
  let events = collected_events(&delivered);
  assert!(
    events.iter().any(|event| event["type"] == event_types::ASSISTANT
      && event["message"]["content"][0]["text"] == "answer after restart")
  );
  let result = events.iter().find(|event| event["type"] == event_types::RESULT).unwrap();
  assert_eq!(result["usage"]["input_tokens"], 8);
  assert_eq!(result["usage"]["output_tokens"], 3);
  pool.stop_all().await.unwrap();
}

struct TestNotifyGuard {
  notify: Arc<tokio::sync::Notify>,
  released: bool,
}

impl TestNotifyGuard {
  fn new(notify: Arc<tokio::sync::Notify>) -> Self {
    Self { notify, released: false }
  }

  fn release(&mut self) {
    self.notify.notify_one();
    self.released = true;
  }
}

impl Drop for TestNotifyGuard {
  fn drop(&mut self) {
    if !self.released {
      self.notify.notify_one();
    }
  }
}

struct Harness {
  root: tempfile::TempDir,
  host: OpenCodeHost,
  pool: Arc<OpenCodeServerPool>,
  adopted_pools: Mutex<Vec<Arc<OpenCodeServerPool>>>,
  server: Arc<OpenCodeServer>,
  servers: Arc<Mutex<Vec<Arc<OpenCodeServer>>>>,
  turn_processes: Arc<Mutex<Vec<(String, ProcessIdentity)>>>,
  log_path: PathBuf,
  script_path: PathBuf,
}

impl Harness {
  fn new(script: Value) -> Self {
    Self::with_settings(script, None, ServerTimeouts::default(), None)
  }

  fn with_settings(script: Value, version: Option<&str>, timeouts: ServerTimeouts, repo_root: Option<PathBuf>) -> Self {
    let root = tempfile::tempdir().unwrap();
    let worktree_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..");
    let worktree_root = fs::canonicalize(worktree_root).unwrap();
    let script_path = root.path().join("script.json");
    fs::write(&script_path, serde_json::to_vec(&script).unwrap()).unwrap();
    let log_path = root.path().join("requests.jsonl");
    let mut extra_env = Env::from([
      ("FAKE_OC_SCRIPT".to_string(), script_path.display().to_string()),
      ("FAKE_OC_LOG".to_string(), log_path.display().to_string()),
    ]);
    if let Some(version) = version {
      extra_env.insert("FAKE_OC_VERSION".to_string(), version.to_string());
    }
    let display = root.path().join(".opencode/bin/opencode");
    let host = OpenCodeHost::with_program(
      fake_opencode_binary(),
      Vec::new(),
      display,
      repo_root.unwrap_or(worktree_root),
      root.path().join("profile"),
      extra_env,
    );
    let pool = OpenCodeServerPool::new(host.clone(), timeouts);
    pool.record_options(&[adoption_option()]);
    let server = pool.server_for(None);
    Self {
      root,
      host,
      pool,
      adopted_pools: Mutex::new(Vec::new()),
      servers: Arc::new(Mutex::new(vec![Arc::clone(&server)])),
      server,
      turn_processes: Arc::new(Mutex::new(Vec::new())),
      log_path,
      script_path,
    }
  }

  fn start_run(
    &self,
    prompt: &str,
    launch: LaunchArgs,
    proxy_url: Option<String>,
    uploaded_files: Vec<Value>,
    additions: Env,
  ) -> TestRun {
    let option = OpenCodeOption { proxy_url: proxy_url.clone(), context_limit: None, extra_body: Map::new() };
    self.start_run_in_pool(&self.pool, TestRunConfig { prompt, launch, proxy_url, uploaded_files, additions, option })
  }

  fn start_run_in_pool(&self, pool: &Arc<OpenCodeServerPool>, config: TestRunConfig<'_>) -> TestRun {
    let TestRunConfig { prompt, mut launch, proxy_url, uploaded_files, additions, option } = config;
    if launch.log_dir.is_none() {
      launch.log_dir = Some(
        self.host.profile_home.join("sessions/synthetic-session/data/runs").join(uuid::Uuid::new_v4().to_string()),
      );
    }
    fs::create_dir_all(launch.log_dir.as_ref().unwrap()).unwrap();
    let backend = OpenCodeBackend::new(
      MODEL.to_string(),
      option,
      Map::new(),
      self.host.clone(),
      Arc::clone(pool),
      launch.clone(),
      SessionCgroupConfig::disabled(),
    );
    let server = pool.server_for(proxy_url);
    let mut servers = self.servers.lock().expect("test server list is not poisoned");
    if !servers.iter().any(|known| Arc::ptr_eq(known, &server)) {
      servers.push(server);
    }
    drop(servers);
    let run_directory = launch.log_dir.clone().expect("the run log directory was assigned");
    let runner = Arc::new(AgentRunner::new("opencode", Box::new(backend), launch));
    let mut values = Env::from([("PATH".to_string(), std::env::var("PATH").expect("PATH is set for Rust tests"))]);
    values.extend(additions);
    // The runner finds the serve transport through the registry, so the run's registry holds the opencode type.
    let mut registry = Registry::new();
    registry.register_package("src.backends.opencode", super::tests::register_synthetic).unwrap();
    let mut request = RunRequest::new(
      prompt.to_string(),
      self.root.path().join("work"),
      AgentBaseEnvironment { values },
      SessionCgroupConfig::disabled(),
      Arc::new(registry),
    );
    fs::create_dir_all(&request.cwd).unwrap();
    request.uploaded_files = uploaded_files;
    request.session_id = Some("synthetic-session".to_string());
    let events = Arc::new(Mutex::new(Vec::new()));
    let event_sink = Arc::clone(&events);
    request.on_event = Arc::new(move |event| {
      let event_sink = Arc::clone(&event_sink);
      Box::pin(async move {
        event_sink.lock().expect("test event list is not poisoned").push(event);
        Ok(())
      })
    });
    let turn_processes = Arc::clone(&self.turn_processes);
    let process_prompt = prompt.to_string();
    request.on_spawn = Some(Arc::new(move |identity| {
      let turn_processes = Arc::clone(&turn_processes);
      let run_directory = run_directory.clone();
      let process_prompt = process_prompt.clone();
      Box::pin(async move {
        let current = read_pid_stat(identity.pid).context("reading the turn process stat at spawn notification")?;
        anyhow::ensure!(
          identity.pid_start.as_deref() == Some(current.pid_start.as_str()),
          "pid_start changed before callback"
        );
        turn_processes.lock().expect("test turn process list is not poisoned").push((process_prompt, identity.clone()));
        let run_id = run_directory.file_name().expect("the run directory has a name").to_string_lossy();
        let metadata = json!({
          "id": run_id,
          "session_id": "synthetic-session",
          "backend": "opencode-test",
          "pid": identity.pid,
          "pid_start": identity.pid_start,
        });
        fs::write(
          run_directory.join(charliebot_store::run_records::RUN_METADATA_NAME),
          serde_json::to_vec(&metadata)?,
        )?;
        Ok(())
      }) as BoxFuture<'static, anyhow::Result<()>>
    }));
    let task_runner = Arc::clone(&runner);
    let task = tokio::spawn(async move { task_runner.run(request).await });
    (runner, events, task)
  }

  fn logs(&self) -> Vec<Value> {
    if !self.log_path.is_file() {
      return Vec::new();
    }
    // The fake server appends each request while a test polls the log; a line it is still writing has no newline yet,
    // so only complete lines are read.
    let text = fs::read_to_string(&self.log_path).unwrap();
    let mut lines: Vec<&str> = text.split('\n').collect();
    lines.pop();
    lines.into_iter().map(|line| serde_json::from_str(line).unwrap()).collect()
  }

  fn run_logs_directory(&self) -> PathBuf {
    self.host.profile_home.join("sessions/synthetic-session/data/runs")
  }

  async fn wait_for_log(&self, matches: impl Fn(&[Value]) -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
      loop {
        if matches(&self.logs()) {
          return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    })
    .await
    .unwrap_or_else(|_| {
      let runs = fs::read_dir(self.run_logs_directory())
        .unwrap()
        .map(|entry| {
          let path = entry.unwrap().path();
          let raw = fs::read_to_string(path.join("agent.raw.ndjson")).unwrap_or_default();
          let stderr = fs::read_to_string(path.join("agent.stderr.log")).unwrap_or_default();
          format!("{} raw={raw:?} stderr={stderr:?}", path.display())
        })
        .collect::<Vec<_>>();
      panic!("fake opencode request was not recorded; runs={runs:?}")
    });
  }

  async fn wait_for_raw_cursor(&self, run_directory: &Path) {
    let raw_path = run_directory.join(charliebot_store::run_records::RAW_LOG_NAME);
    let cursor_path = run_directory.join(charliebot_store::run_records::CURSOR_NAME);
    tokio::time::timeout(Duration::from_secs(5), async {
      loop {
        if let Ok(metadata) = fs::metadata(&raw_path)
          && metadata.len() > 0
          && cursor::read_raw_cursor(&cursor_path) == metadata.len()
        {
          return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    })
    .await
    .expect("the raw cursor reaches the end of the delivered prefix");
  }

  /// Wait until the run delivered an event that `matches` accepts.
  ///
  /// `wait_for_raw_cursor` waits for the cursor to catch up with the raw log, which the relay can write one
  /// untranslated line at a time; it can return after the first line (for example `session.execution.started`) and
  /// before the turn's text reaches the sink. Tests that cut the turn and then assert about the delivered events wait
  /// for the event itself.
  async fn wait_for_events(&self, events: &Arc<Mutex<Vec<Value>>>, matches: impl Fn(&[Value]) -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
      loop {
        if matches(&collected_events(events)) {
          return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    })
    .await
    .expect("the run delivered no matching event");
  }

  async fn control(&self, body: Value) -> anyhow::Result<()> {
    self.server.client_request(reqwest::Method::POST, "/__control")?.json(&body).send().await?.error_for_status()?;
    Ok(())
  }

  async fn stop(&self) -> anyhow::Result<()> {
    self.pool.stop_all().await?;
    let pools = self.adopted_pools.lock().expect("test adopted pool list is not poisoned").clone();
    for pool in pools {
      pool.stop_all().await?;
    }
    Ok(())
  }

  fn adopt_pool(&self) -> Arc<OpenCodeServerPool> {
    self.adopt_pool_with_options(&[adoption_option()])
  }

  fn adopt_pool_with_options(&self, options: &[BackendOption]) -> Arc<OpenCodeServerPool> {
    let pool = OpenCodeServerPool::new(self.host.clone(), ServerTimeouts::default());
    pool.record_options(options);
    pool.adopt_existing().unwrap();
    self.servers.lock().expect("test server list is not poisoned").extend(pool.adopted_servers_test());
    self.adopted_pools.lock().expect("test adopted pool list is not poisoned").push(Arc::clone(&pool));
    pool
  }

  fn clear_finished_turn_pids(&self) {
    self.turn_processes.lock().expect("test turn process list is not poisoned").clear();
  }
}

fn run_directory(harness: &Harness) -> PathBuf {
  fs::read_dir(harness.run_logs_directory()).unwrap().next().expect("the run has a persisted directory").unwrap().path()
}

fn replay_translator_prefix(run_directory: &Path) -> (OpenCodeTranslator, u64) {
  let turn = super::relay::read_turn_record(run_directory).unwrap().expect("the turn has a durable prompt record");
  let raw_path = run_directory.join(charliebot_store::run_records::RAW_LOG_NAME);
  let cursor_path = run_directory.join(charliebot_store::run_records::CURSOR_NAME);
  let cursor = cursor::read_raw_cursor(&cursor_path);
  let prefix = recovery::raw_events_before_cursor(&raw_path, cursor).unwrap();
  let mut translator = OpenCodeTranslator::with_model_limit(MODEL.to_string(), turn.model_limit);
  for event in prefix {
    translator.translate_event(&event).unwrap();
  }
  (translator, cursor)
}

async fn follow_from_cursor(
  run_directory: &Path,
  cursor: u64,
  mut translator: OpenCodeTranslator,
  is_alive: impl Fn() -> bool,
  delivered: Arc<Mutex<Vec<Value>>>,
) -> anyhow::Result<follow::FollowSummary> {
  let raw_path = run_directory.join(charliebot_store::run_records::RAW_LOG_NAME);
  let cursor_path = run_directory.join(charliebot_store::run_records::CURSOR_NAME);
  follow::tail_follow_events(
    &raw_path,
    move |event| translator.translate_event(&event),
    move |event| {
      let delivered = Arc::clone(&delivered);
      async move {
        delivered.lock().expect("recovered event list is not poisoned").push(event);
        Ok::<(), std::io::Error>(())
      }
    },
    is_alive,
    Some(&cursor_path),
    cursor,
    Duration::from_secs(1),
    follow::DEFAULT_BUFFER_LIMIT,
    follow::TAIL_POLL_INTERVAL,
    None,
    Duration::ZERO,
  )
  .await
}

impl Drop for Harness {
  fn drop(&mut self) {
    for server in self.servers.lock().expect("test server list is not poisoned").iter() {
      if server.process_alive()
        && let Some(pid) = server.pid()
      {
        kill_process_group(pid, Signal::SIGKILL);
      }
    }
    for (_, identity) in self.turn_processes.lock().expect("test turn process list is not poisoned").iter() {
      if read_pid_stat(identity.pid).is_some_and(|stat| Some(stat.pid_start.as_str()) == identity.pid_start.as_deref())
      {
        kill_process_group(identity.pid, Signal::SIGKILL);
      }
    }
  }
}

fn launch_with_instructions(content: Option<&str>, file: Option<PathBuf>, resume: Option<&str>) -> LaunchArgs {
  LaunchArgs {
    instructions_content: content.map(str::to_string),
    instructions_file: file,
    resume_session_id: resume.map(str::to_string),
    ..LaunchArgs::default()
  }
}

fn collected_events(events: &Arc<Mutex<Vec<Value>>>) -> Vec<Value> {
  events.lock().expect("test event list is not poisoned").clone()
}

fn session_id_for_prompt(logs: &[Value], prompt: &str) -> String {
  logs
    .iter()
    .find(|entry| entry.get("prompt_text").and_then(Value::as_str) == Some(prompt))
    .and_then(|entry| entry.get("session"))
    .and_then(Value::as_str)
    .expect("fake opencode recorded the prompt session")
    .to_string()
}

async fn loaded_directories(harness: &Harness) -> Vec<String> {
  harness
    .server
    .client_request(reqwest::Method::GET, "/api/debug/location")
    .unwrap()
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .json::<Vec<Value>>()
    .await
    .unwrap()
    .iter()
    .map(|location| location["directory"].as_str().unwrap().to_string())
    .collect()
}

fn text_event(text: &str) -> Value {
  json!({"type": "session.text.ended", "data": {"assistantMessageID": "m1", "ordinal": 0, "text": text}})
}

fn usage_event(input: i64, output: i64) -> Value {
  json!({
    "type": "session.step.ended",
    "data": {
      "tokens": {"input": input, "output": output, "reasoning": 0, "cache": {"read": 2, "write": 1}},
      "cost": 0.25
    }
  })
}

fn held_script(prompts: &[&str]) -> Value {
  let prompts = prompts
    .iter()
    .map(|prompt| ((*prompt).to_string(), json!({"events": [], "hold": true})))
    .collect::<serde_json::Map<String, Value>>();
  json!({"prompts": prompts})
}

#[test]
fn headless_config_denies_the_three_tools_and_pins_bash() {
  // Python: tests/test_opencode_backend.py::test_headless_config_denies_the_three_tools_and_pins_bash
  let config = super::headless_config(&[]).unwrap();
  assert_eq!(config["shell"], "bash");
  assert_eq!(config["permission"]["question"], "deny");
  assert_eq!(config["permission"]["subagent"], "deny");
  assert_eq!(config["permission"]["opencode_session_move"], "deny");
}

// Python: tests/test_opencode_backend.py:114.
#[test]
fn server_env_drops_the_run_identity_and_points_xdg_at_the_store() {
  let harness = Harness::new(json!({}));
  let env = Env::from([
    ("CHARLIEBOT_RUN_TOKEN".to_string(), "turn-token".to_string()),
    ("CHARLIEBOT_SESSION_ID".to_string(), "session-x".to_string()),
    ("NO_PROXY".to_string(), "internal.test".to_string()),
  ]);
  let result = harness.server.server_env_from(env, "pw").unwrap();
  assert!(!result.contains_key("CHARLIEBOT_RUN_TOKEN"));
  assert!(!result.contains_key("CHARLIEBOT_SESSION_ID"));
  assert_eq!(result["XDG_DATA_HOME"], harness.host.data_dir().display().to_string());
  assert_eq!(result["XDG_STATE_HOME"], harness.host.state_dir().display().to_string());
  assert_eq!(result["XDG_CACHE_HOME"], harness.host.cache_dir().display().to_string());
  assert_eq!(result["OPENCODE_SERVER_PASSWORD"], "pw");
  assert_eq!(result.get("OPENCODE_DISABLE_FFF").map(String::as_str), Some("1"));
  assert_eq!(result["NO_PROXY"], "internal.test");
  let proxied_host = OpenCodeHost::with_program(
    harness.host.program.program.clone(),
    harness.host.program.args.clone(),
    harness.host.program.display.clone(),
    harness.host.repo_root.clone(),
    harness.host.profile_home.clone(),
    Env::new(),
  );
  let proxied_pool = OpenCodeServerPool::new(proxied_host, ServerTimeouts::default());
  let proxied = proxied_pool.server_for(Some("http://proxy.test:8080".to_string()));
  let env = proxied.server_env_from(Env::new(), "pw").unwrap();
  assert_eq!(env["HTTP_PROXY"], "http://proxy.test:8080");
  assert_eq!(env["HTTPS_PROXY"], "http://proxy.test:8080");
  assert!(env["NO_PROXY"].ends_with("::1"));
}

// Python: tests/test_opencode_backend.py:462.
#[tokio::test]
async fn claim_is_held_before_start_and_dropped_after_start_failure() {
  let harness = Harness::with_settings(json!({"prompts": {}}), Some("1.18.18"), ServerTimeouts::default(), None);
  let directory = harness.root.path().join("work").display().to_string();
  let claims_at_start = harness.server.observe_claim_before_start_test(&directory);
  let (_, output, task) = harness.start_run("x", LaunchArgs::default(), None, vec![], Env::new());
  assert_eq!(task.await.unwrap().unwrap().exit_code, Some(1));
  assert_eq!(collected_events(&output).last().unwrap()["type"], event_types::ERROR);
  assert_eq!(*claims_at_start.lock().unwrap(), vec![1]);
  assert_eq!(harness.server.claim_count_test(&directory), 0);
  assert!(!harness.logs().iter().any(|entry| entry["path"] == "/api/debug/location"));
  harness.clear_finished_turn_pids();
}

// Python: tests/test_opencode_backend.py:345.
#[test]
fn process_rss_reads_the_process_own_resident_memory() {
  assert!(server::OpenCodeServer::process_rss_mb_test(std::process::id()).unwrap() > 10.0);
}

// Python: tests/test_opencode_backend.py:350.
#[tokio::test]
async fn heavy_server_stops_at_the_checkpoint_when_its_rss_is_above_the_limit() {
  let timeouts = ServerTimeouts {
    idle_exit: Duration::from_secs(3600),
    heavy_check: Duration::from_millis(10),
    heavy_rss_mb: 400.0,
    heavy_rss_override: Some(401.0),
    ..ServerTimeouts::default()
  };
  let harness =
    Harness::with_settings(json!({"prompts": {"idle": {"events": [text_event("ok")]}}}), None, timeouts, None);
  let (_, _, task) = harness.start_run("idle", LaunchArgs::default(), None, vec![], Env::new());
  assert_eq!(task.await.unwrap().unwrap().exit_code, Some(0));
  tokio::time::timeout(Duration::from_secs(5), async {
    while harness.server.heavy_check_count_test() == 0 || harness.server.process_alive() {
      tokio::task::yield_now().await;
    }
  })
  .await
  .expect("heavy checkpoint did not stop the server before the test deadline");
  assert_eq!(harness.server.heavy_check_count_test(), 1);
  assert!(!harness.server.process_alive());
  harness.clear_finished_turn_pids();
}

// Python: tests/test_opencode_backend.py:364.
#[tokio::test]
async fn server_at_the_rss_limit_runs_on_to_the_idle_exit() {
  let timeouts = ServerTimeouts {
    idle_exit: Duration::from_millis(80),
    heavy_check: Duration::from_millis(10),
    heavy_rss_mb: 400.0,
    heavy_rss_override: Some(400.0),
    ..ServerTimeouts::default()
  };
  let harness =
    Harness::with_settings(json!({"prompts": {"idle": {"events": [text_event("ok")]}}}), None, timeouts, None);
  let (_, _, task) = harness.start_run("idle", LaunchArgs::default(), None, vec![], Env::new());
  assert_eq!(task.await.unwrap().unwrap().exit_code, Some(0));
  tokio::time::timeout(Duration::from_secs(5), async {
    while harness.server.heavy_check_count_test() == 0 || !harness.server.process_alive() {
      tokio::task::yield_now().await;
    }
  })
  .await
  .expect("heavy checkpoint did not leave the server alive before the test deadline");
  assert_eq!(harness.server.heavy_check_count_test(), 1);
  assert!(harness.server.process_alive());
  tokio::time::timeout(Duration::from_secs(2), async {
    while harness.server.process_alive() {
      tokio::time::sleep(Duration::from_millis(5)).await;
    }
  })
  .await
  .expect("server did not reach the idle exit");
  harness.clear_finished_turn_pids();
}

// Python: tests/test_opencode_backend.py:377.
#[tokio::test]
async fn a_claimed_directory_keeps_the_server_running() {
  let timeouts = ServerTimeouts {
    idle_exit: Duration::from_millis(50),
    heavy_check: Duration::from_millis(10),
    heavy_rss_mb: 0.0,
    ..ServerTimeouts::default()
  };
  let harness = Harness::with_settings(held_script(&["held"]), None, timeouts, None);
  let directory = harness.root.path().join("work").display().to_string();
  let (runner, _, task) = harness.start_run("held", LaunchArgs::default(), None, vec![], Env::new());
  harness
    .wait_for_log(|logs| {
      logs.iter().any(|entry| entry.get("path").and_then(Value::as_str).is_some_and(|path| path.ends_with("/prompt")))
    })
    .await;
  assert_eq!(harness.server.claim_count_test(&directory), 1);
  assert_eq!(harness.server.heavy_check_count_test(), 0);
  assert!(harness.server.process_alive());
  let (release_entered, release_cleanup) = harness.server.pause_release_claim_test();
  let idle_release_lock_waiting = harness.server.observe_idle_release_lock_test();
  let mut release_guard = TestNotifyGuard::new(release_cleanup);
  let terminate_runner = Arc::clone(&runner);
  let terminate_task = tokio::spawn(async move { terminate_runner.terminate().await });
  tokio::time::timeout(Duration::from_secs(2), release_entered.notified())
    .await
    .expect("claim cleanup did not reach the held location request");
  tokio::time::timeout(Duration::from_secs(2), idle_release_lock_waiting.notified())
    .await
    .expect("idle shutdown did not wait for claim cleanup");
  assert!(harness.server.process_alive(), "idle shutdown stopped the server during claim cleanup");
  release_guard.release();
  terminate_task.await.unwrap().unwrap();
  task.await.unwrap().unwrap();
  harness.clear_finished_turn_pids();
}

// Python: tests/test_opencode_backend.py:491.
#[tokio::test]
async fn dropping_the_last_claim_rearms_the_idle_exit() {
  let timeouts = ServerTimeouts {
    idle_exit: Duration::from_millis(100),
    heavy_check: Duration::from_millis(10),
    heavy_rss_mb: 1_000_000.0,
    ..ServerTimeouts::default()
  };
  let harness = Harness::with_settings(json!({}), None, timeouts, None);
  let directory = harness.root.path().join("claimed").display().to_string();
  harness.server.claim_test(&directory).await;
  harness.server.ensure_running().await.unwrap();
  tokio::time::sleep(Duration::from_millis(150)).await;
  assert!(harness.server.process_alive());

  harness.server.release_claim_test(&directory).await.unwrap();
  tokio::time::timeout(Duration::from_secs(2), async {
    while harness.server.process_alive() {
      tokio::time::sleep(Duration::from_millis(5)).await;
    }
  })
  .await
  .expect("server did not exit after the last claim was released");
  harness.clear_finished_turn_pids();
}

// Python: tests/test_opencode_backend.py:391.
#[tokio::test]
async fn a_turn_claimed_during_heavy_shutdown_starts_after_the_old_server_stops() {
  let timeouts = ServerTimeouts {
    idle_exit: Duration::from_secs(3600),
    heavy_check: Duration::from_millis(10),
    heavy_rss_mb: 0.0,
    ..ServerTimeouts::default()
  };
  let harness = Harness::with_settings(
    json!({"prompts": {"first": {"events": [text_event("one")]}, "next": {"events": [text_event("two")]}}}),
    None,
    timeouts,
    None,
  );
  let (entered, release_shutdown) = harness.server.pause_heavy_shutdown_test();
  let mut release_guard = TestNotifyGuard::new(release_shutdown);
  let (_, _, first_task) = harness.start_run("first", LaunchArgs::default(), None, vec![], Env::new());
  let first_outcome = tokio::time::timeout(Duration::from_secs(5), first_task)
    .await
    .expect("first turn did not finish before the test deadline")
    .unwrap()
    .unwrap();
  assert_eq!(first_outcome.exit_code, Some(0));

  tokio::time::timeout(Duration::from_secs(5), async {
    while !entered.load(std::sync::atomic::Ordering::Acquire) {
      tokio::time::sleep(Duration::from_millis(1)).await;
    }
  })
  .await
  .expect("heavy shutdown did not enter its pause before the test deadline");

  let first_pid = harness.server.pid().expect("first server has a process id");
  let directory = harness.root.path().join("work").display().to_string();
  let (_, _, next_task) = harness.start_run("next", LaunchArgs::default(), None, vec![], Env::new());
  tokio::time::timeout(Duration::from_secs(5), async {
    while harness.server.claim_count_test(&directory) == 0 {
      tokio::time::sleep(Duration::from_millis(1)).await;
    }
  })
  .await
  .expect("next turn did not claim its directory while heavy shutdown was paused");

  let session_count =
    harness.logs().iter().filter(|entry| entry["method"] == "POST" && entry["path"] == "/api/session").count();
  release_guard.release();
  assert_eq!(session_count, 1);
  let next_outcome = tokio::time::timeout(Duration::from_secs(5), next_task)
    .await
    .expect("next turn did not start after the old server stopped")
    .unwrap()
    .unwrap();
  assert_eq!(next_outcome.exit_code, Some(0));
  assert_ne!(harness.server.pid().expect("replacement server has a process id"), first_pid);

  // The replacement server can run another heavy check when its turn claim is released.
  release_guard.release();
  tokio::time::timeout(Duration::from_secs(5), harness.stop())
    .await
    .expect("OpenCode server did not stop before the test deadline")
    .unwrap();
  harness.clear_finished_turn_pids();
}

#[test]
fn translation_maps_text_tool_and_usage_events() {
  // Python: tests/test_opencode_backend.py::test_translation_maps_text_tool_and_usage_events
  let mut translator = OpenCodeTranslator::new(MODEL.to_string());
  assert!(
    translator.translate_event(&json!({"type": "session.step.started", "data": {"started": 1234}})).unwrap().is_empty()
  );
  assert_eq!(translator.model_request_started_ms, Some(1234));
  let delta = translator
    .translate_event(&json!({
      "type": "session.text.delta",
      "data": {"assistantMessageID": "m1", "ordinal": 0, "delta": "hel"}
    }))
    .unwrap();
  assert_eq!(delta[0]["message"]["content"][0]["text"], "hel");
  assert!(
    translator
      .translate_event(&json!({
        "type": "session.text.ended",
        "data": {"assistantMessageID": "m1", "ordinal": 0, "text": "hel"}
      }))
      .unwrap()
      .is_empty()
  );
  let later = translator
    .translate_event(&json!({
      "type": "session.text.ended",
      "data": {"assistantMessageID": "m1", "ordinal": 1, "text": "later"}
    }))
    .unwrap();
  assert_eq!(later[0]["message"]["content"][0]["text"], "later");
  translator
    .translate_event(&json!({"type": "session.tool.input.started", "data": {"id": "c1", "name": "shell"}}))
    .unwrap();
  let call = translator
    .translate_event(&json!({"type": "session.tool.called", "data": {"id": "c1", "input": {"command": "ls"}}}))
    .unwrap();
  assert_eq!(call[0]["message"]["content"][0]["name"], "shell");
  assert_eq!(call[0]["message"]["content"][0]["input"]["command"], "ls");
  let result = translator
    .translate_event(&json!({
      "type": "session.tool.success",
      "data": {"id": "c1", "content": [{"type": "text", "text": "ok"}]}
    }))
    .unwrap();
  assert_eq!(result[0]["tool_use_id"], "c1");
  assert_eq!(result[0]["content"], "ok");
  assert!(translator.translate_event(&usage_event(10, 4)).unwrap().is_empty());
  let final_event = translator.accumulated_result(None);
  assert_eq!(final_event["usage"]["input_tokens"], 10);
  assert_eq!(final_event["usage"]["output_tokens"], 4);
  assert_eq!(final_event["usage"]["cache_read_input_tokens"], 2);
  assert_eq!(final_event["context_snapshot"]["tokens"]["input"], 10);
}

#[test]
fn tool_call_without_a_started_name_fails_the_translation() {
  // Python: tests/test_opencode_backend.py::test_tool_call_without_a_started_name_fails_the_translation
  let mut translator = OpenCodeTranslator::new(MODEL.to_string());
  let error =
    translator.translate_event(&json!({"type": "session.tool.called", "data": {"id": "c9", "input": {}}})).unwrap_err();
  assert!(error.to_string().contains("without a tool name"));
}

#[test]
fn model_limit_is_read_from_the_2_0_model_list() {
  // Python: tests/test_opencode_backend.py::test_model_limit_is_read_from_the_2_0_model_list
  let payload = json!({"data": [{"id": "m", "providerID": "p", "limit": {"context": 9, "output": 3}}]});
  assert_eq!(
    extract_model_limit(&payload, "p", "m").unwrap(),
    json!({"context": 9, "output": 3}).as_object().unwrap().clone()
  );
  assert!(extract_model_limit(&payload, "p", "absent").is_none());
}

#[test]
fn image_attachments_become_data_urls_and_other_files_are_skipped() {
  // Python: tests/test_opencode_backend.py::test_image_attachments_become_data_urls_and_other_files_are_skipped
  let root = tempfile::tempdir().unwrap();
  let image = root.path().join("a.png");
  fs::write(&image, b"\x89PNG").unwrap();
  let references = vec![
    json!({"filename": "a.png", "path": image}),
    json!({"filename": "notes.txt", "path": root.path().join("notes.txt")}),
    json!({"filename": "gone.jpg", "path": root.path().join("gone.jpg")}),
  ];
  assert_eq!(
    image_file_attachments(&references),
    vec![json!({"uri": "data:image/png;base64,iVBORw==", "name": "a.png"})]
  );
}

#[test]
fn option_default_model_requires_a_configured_model() {
  // Python: tests/test_spawner_backend_propagation.py::test_resolve_backend_option_rejects_missing_model_for_model_required_backends
  let option = BackendOption {
    id: "opencode".to_string(),
    label: "OpenCode".to_string(),
    model: String::new(),
    prompt_overlay: None,
    backend_type: "opencode".to_string(),
    type_fields: serde_yaml::Mapping::new(),
  };
  assert_eq!(
    super::option_default_model(&option, "backend ").unwrap_err().to_string(),
    "backend 'opencode' has no default model"
  );
}

#[test]
fn snapshot_reading_limits_keep_the_opencode_reserve() {
  assert_eq!(
    opencode_limits::snapshot_reading_limits(charliebot_store::run_records::RunKind::Work),
    charliebot_registry::backend::ContextLimits { declared_window: None, compact_reserve: Some(20_000) }
  );
}

#[tokio::test]
async fn turn_translates_events_sets_the_environment_and_ends_with_result() {
  // Python: tests/test_opencode_backend.py::test_turn_translates_events_sets_the_environment_and_ends_with_result
  // Python: tests/test_backend_pid_start_contract.py::test_every_backend_subclass_has_a_harness
  // Python: tests/test_backend_pid_start_contract.py::test_pid_start_pinned_at_spawn_notification
  let script = json!({
    "event_subscribe_delay": 0.05,
    "prompts": {
      "hello": {"events": [
        {"type": "session.step.started", "data": {"started": 777}},
        {"type": "session.text.delta", "data": {"assistantMessageID": "m1", "ordinal": 0, "delta": "hi"}},
        {"type": "session.tool.input.started", "data": {"id": "c1", "name": "shell"}},
        {"type": "session.tool.called", "data": {"id": "c1", "input": {"command": "true"}}},
        {"type": "session.tool.success", "data": {"id": "c1", "content": [{"type": "text", "text": "done"}]}},
        usage_event(5, 2)
      ]}
    }
  });
  let harness = Harness::new(script);
  let additions = Env::from([
    ("CHARLIEBOT_RUN_TOKEN".to_string(), "turn-token".to_string()),
    ("NO_PROXY".to_string(), "internal.test,localhost,127.0.0.1".to_string()),
  ]);
  let (runner, events, task) =
    harness.start_run("hello", LaunchArgs::default(), Some("http://proxy.test:8080".into()), vec![], additions.clone());
  let outcome = task.await.unwrap().unwrap();
  let events = collected_events(&events);
  let raw_dump = fs::read_dir(harness.run_logs_directory())
    .unwrap()
    .map(|entry| {
      let path = entry.unwrap().path();
      format!(
        "raw={:?} stdout={:?} stderr={:?}",
        fs::read_to_string(path.join("agent.raw.ndjson")).unwrap_or_default(),
        fs::read_to_string(path.join("agent.stdout.log")).unwrap_or_default(),
        fs::read_to_string(path.join("agent.stderr.log")).unwrap_or_default()
      )
    })
    .collect::<Vec<_>>();
  assert_eq!(events[0]["type"], event_types::SESSION_ATTACHED);
  assert_eq!(events[1]["message"]["content"][0]["text"], "hi", "events={events:?} raw={raw_dump:?}");
  assert_eq!(events[2]["message"]["content"][0]["name"], "shell");
  assert_eq!(events[3]["tool_use_id"], "c1");
  assert_eq!(events.last().unwrap()["type"], event_types::RESULT);
  assert_eq!(events.last().unwrap()["usage"]["input_tokens"], 5);
  assert_eq!(events.last().unwrap()["usage"]["output_tokens"], 2);
  assert_eq!(runner.exit_code(), Some(0));
  assert_eq!(outcome.exit_code, Some(0));
  assert_eq!(runner.exit_code(), Some(0));
  let logs = harness.logs();
  let relay_stream = logs.iter().rposition(|entry| entry["method"] == "GET" && entry["path"] == "/api/event").unwrap();
  let prompt_request = logs
    .iter()
    .position(|entry| entry["method"] == "POST" && entry["path"].as_str().is_some_and(|path| path.ends_with("/prompt")))
    .unwrap();
  assert!(relay_stream < prompt_request);
  let put = logs.iter().find(|entry| entry["method"] == "PUT").unwrap();
  assert_eq!(put["body"]["variables"]["CHARLIEBOT_RUN_TOKEN"], "turn-token");
  assert_eq!(put["body"]["variables"]["HTTP_PROXY"], "http://proxy.test:8080");
  assert_eq!(put["body"]["variables"]["HTTPS_PROXY"], "http://proxy.test:8080");
  assert!(put["body"]["variables"]["NO_PROXY"].as_str().unwrap().ends_with("::1"));
  assert!(!put["body"]["variables"].as_object().unwrap().contains_key("BASH_ENV"));
  assert_eq!(additions["NO_PROXY"], "internal.test,localhost,127.0.0.1");
  let create = logs.iter().find(|entry| entry["method"] == "POST" && entry["path"] == "/api/session").unwrap();
  assert_eq!(create["body"]["model"], json!({"id": "fake-model", "providerID": "fakeprov"}));
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn a_turn_survives_a_server_restart_and_replays_translator_state() {
  let prefix = vec![
    json!({"type": "session.step.started", "data": {"started": 777}}),
    json!({"type": "session.text.delta", "data": {"assistantMessageID": "m1", "ordinal": 0, "delta": "before"}}),
    text_event("before"),
    json!({"type": "session.tool.input.started", "data": {"id": "cmd1", "name": "shell"}}),
    json!({"type": "session.tool.called", "data": {"id": "cmd1", "input": {"command": "true"}}}),
    usage_event(5, 2),
  ];
  let suffix = vec![
    json!({"type": "session.tool.success", "data": {"id": "cmd1", "content": [{"type": "text", "text": "command result"}]}}),
    usage_event(3, 1),
  ];
  let baseline_events_script = prefix.iter().chain(&suffix).cloned().collect::<Vec<_>>();
  let script = json!({"prompts": {
    "baseline": {"events": baseline_events_script},
    "paused": {"events": prefix, "hold": true}
  }});

  let baseline = Harness::new(script.clone());
  let (_, expected_events, baseline_task) =
    baseline.start_run("baseline", LaunchArgs::default(), None, vec![], Env::new());
  assert_eq!(baseline_task.await.unwrap().unwrap().exit_code, Some(0));
  let expected_events = collected_events(&expected_events);
  baseline.stop().await.unwrap();

  let harness = Harness::new(script);
  let (runner, initial_events, task) = harness.start_run("paused", LaunchArgs::default(), None, vec![], Env::new());
  harness
    .wait_for_log(|logs| logs.iter().any(|entry| entry.get("prompt_text").and_then(Value::as_str) == Some("paused")))
    .await;
  let run_directory = run_directory(&harness);
  harness.wait_for_raw_cursor(&run_directory).await;
  let turn = super::relay::read_turn_record(&run_directory).unwrap().expect("the turn opened a session");
  let server_record_path = harness.host.profile_home.join("opencode/servers").join(&turn.server_record);
  assert_eq!(fs::metadata(&server_record_path).unwrap().permissions().mode() & 0o777, 0o600);
  let prefix_cursor = cursor::read_raw_cursor(&run_directory.join(charliebot_store::run_records::CURSOR_NAME));
  assert!(prefix_cursor > 0);

  let identity = harness
    .turn_processes
    .lock()
    .expect("test turn process list is not poisoned")
    .iter()
    .find_map(|(prompt, identity)| (prompt == "paused").then(|| identity.clone()))
    .expect("the paused turn has a relay process");
  runner.detach();
  task.abort();
  assert!(task.await.unwrap_err().is_cancelled());
  harness.pool.abandon_for_restart_test().await;
  let pool = harness.adopt_pool();
  let adopted = pool.adopted_servers_test();
  assert_eq!(adopted.len(), 1);
  assert_eq!(adopted[0].pid(), Some(harness.server.pid().unwrap()));
  harness
    .wait_for_log(|logs| {
      logs.iter().filter(|entry| entry["method"] == "GET" && entry["path"] == "/api/event").count() == 3
    })
    .await;
  assert!(!harness.logs().iter().any(|entry| entry["path"].as_str().is_some_and(|path| path.ends_with("/interrupt"))));

  let (translator, cursor) = replay_translator_prefix(&run_directory);
  assert_eq!(cursor, prefix_cursor);
  let recovered_events = Arc::new(Mutex::new(Vec::new()));
  let recovered_sink = Arc::clone(&recovered_events);
  let follow_directory = run_directory.clone();
  let relay_pid = identity.pid;
  let relay_start = identity.pid_start.clone().expect("relay identity has a start time");
  let follow = tokio::spawn(async move {
    follow_from_cursor(
      &follow_directory,
      cursor,
      translator,
      move || super::relay::same_process(relay_pid, &relay_start),
      recovered_sink,
    )
    .await
  });
  for mut event in suffix {
    event["data"]["sessionID"] = json!(turn.session_id);
    harness.control(json!({"emit": event})).await.unwrap();
  }
  harness
    .control(json!({"emit": {"type": "session.execution.succeeded", "data": {"sessionID": turn.session_id}}}))
    .await
    .unwrap();
  let summary = tokio::time::timeout(Duration::from_secs(5), follow).await.unwrap().unwrap().unwrap();
  assert!(summary.saw_result);
  let recovered_events = collected_events(&recovered_events);
  let mut combined = collected_events(&initial_events);
  combined.extend(recovered_events.clone());
  let normalize = |events: Vec<Value>| {
    events
      .into_iter()
      .filter(|event| event.get("type").and_then(Value::as_str) != Some(event_types::SESSION_ATTACHED))
      .map(|mut event| {
        event.as_object_mut().unwrap().remove("timestamp");
        event
      })
      .collect::<Vec<_>>()
  };
  assert_eq!(normalize(combined), normalize(expected_events.clone()));
  assert!(recovered_events.iter().any(|event| {
    event["type"] == event_types::TOOL_RESULT && event["tool_use_id"] == "cmd1" && event["content"] == "command result"
  }));
  let raw_path = run_directory.join(charliebot_store::run_records::RAW_LOG_NAME);
  let mut scan_translator = OpenCodeTranslator::with_model_limit(MODEL.to_string(), turn.model_limit.clone());
  let (_, recovered_result, exit_code) =
    recovery::scan_result_exit(&raw_path, move |raw| scan_translator.translate_event(&raw)).unwrap();
  let expected_result = expected_events.iter().find(|event| event["type"] == event_types::RESULT).unwrap();
  let recovered_result = recovered_result.expect("the recovered raw log has a result");
  assert_eq!(exit_code, 0);
  assert_eq!(recovered_result["usage"], expected_result["usage"]);
  assert_eq!(recovered_result["usage"]["input_tokens"], 8);
  assert_eq!(recovered_result["usage"]["output_tokens"], 3);
  tokio::time::timeout(Duration::from_secs(5), async {
    while adopted[0].process_alive() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("the adopted serve stops after its last turn ends");
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[test]
fn a_restart_before_the_turn_record_exists_fails_with_a_specific_error_event() {
  let harness = Harness::new(json!({"prompts": {}}));
  let run_directory = harness.root.path().join("run-without-turn-record");
  fs::create_dir_all(&run_directory).unwrap();
  let raw_path = run_directory.join(charliebot_store::run_records::RAW_LOG_NAME);
  fs::write(&raw_path, []).unwrap();
  let executable = std::env::current_exe().unwrap();
  let status = Command::new(executable)
    .args(["--exact", "test_backend::relay_subprocess", "--ignored"])
    .env("CHARLIEBOT_OPENCODE_RELAY_DIRECTORY", &run_directory)
    .env("CHARLIEBOT_OPENCODE_PARENT_PID", "2147483647")
    .env("CHARLIEBOT_OPENCODE_PARENT_START", "no-such-process")
    .env("CHARLIEBOT_OPENCODE_SERVER_DIRECTORY", harness.host.profile_home.join("opencode/servers"))
    .status()
    .unwrap();
  assert!(status.success());
  assert!(!run_directory.join(super::relay::TURN_RECORD_NAME).exists());
  let raw = fs::read(&raw_path).unwrap();
  let event: Value = serde_json::from_slice(raw.strip_suffix(b"\n").unwrap()).unwrap();
  let translated = OpenCodeTranslator::new(MODEL.to_string()).translate_event(&event).unwrap();
  assert_eq!(translated[0]["type"], event_types::ERROR);
  assert!(translated[0]["message"].as_str().unwrap().contains("server restarted before the turn started"));
}

#[tokio::test]
async fn a_new_settings_snapshot_starts_a_new_serve_while_an_adopted_turn_finishes_on_the_old_one() {
  let harness = Harness::new(json!({
    "prompts": {
      "old held turn": {"events": [text_event("old prefix")], "hold": true},
      "new settings turn": {"events": [text_event("new server answer")]}
    }
  }));
  let (runner, _, task) = harness.start_run("old held turn", LaunchArgs::default(), None, vec![], Env::new());
  harness
    .wait_for_log(|logs| {
      logs.iter().any(|entry| entry.get("prompt_text").and_then(Value::as_str) == Some("old held turn"))
    })
    .await;
  let run_directory = run_directory(&harness);
  harness.wait_for_raw_cursor(&run_directory).await;
  let turn = super::relay::read_turn_record(&run_directory).unwrap().expect("the old turn has a durable record");
  let old_pid = harness.server.pid().unwrap();
  let identity = harness
    .turn_processes
    .lock()
    .expect("test turn process list is not poisoned")
    .iter()
    .find_map(|(prompt, identity)| (prompt == "old held turn").then(|| identity.clone()))
    .expect("the old turn has a relay process");
  let relay_start = identity.pid_start.clone().expect("relay identity has a start time");
  runner.detach();
  task.abort();
  assert!(task.await.unwrap_err().is_cancelled());
  harness.pool.abandon_for_restart_test().await;

  let changed_option = adoption_option_with_context_limit(420_000);
  let pool = harness.adopt_pool_with_options(std::slice::from_ref(&changed_option));
  let adopted = pool.adopted_servers_test();
  assert_eq!(adopted.len(), 1);
  assert_eq!(adopted[0].pid(), Some(old_pid));
  assert!(adopted[0].process_alive());
  let new_fields = super::options::OpenCodeOption::from_fields(&changed_option.type_fields).unwrap();
  let (_, new_events, new_task) = harness.start_run_in_pool(
    &pool,
    TestRunConfig {
      prompt: "new settings turn",
      launch: LaunchArgs::default(),
      proxy_url: None,
      uploaded_files: vec![],
      additions: Env::new(),
      option: new_fields,
    },
  );
  assert_eq!(new_task.await.unwrap().unwrap().exit_code, Some(0));
  assert_eq!(collected_events(&new_events).last().unwrap()["type"], event_types::RESULT);
  let new_server = pool.server_for(None);
  let new_pid = new_server.pid().unwrap();
  assert_ne!(new_pid, old_pid);
  let environment = fs::read(format!("/proc/{new_pid}/environ")).unwrap();
  let config = environment
    .split(|byte| *byte == 0)
    .find_map(|entry| entry.strip_prefix(b"OPENCODE_CONFIG_CONTENT="))
    .expect("the new serve receives its settings snapshot");
  let config: Value = serde_json::from_slice(config).unwrap();
  assert_eq!(config["providers"]["fakeprov"]["models"]["fake-model"]["limit"]["context"], 420_000);
  let prompt_pids = harness
    .logs()
    .into_iter()
    .filter(|entry| entry["method"] == "POST" && entry["path"].as_str().is_some_and(|path| path.ends_with("/prompt")))
    .map(|entry| (entry["body"]["text"].as_str().unwrap().to_string(), entry["pid"].as_u64().unwrap() as u32))
    .collect::<HashMap<_, _>>();
  assert_eq!(prompt_pids["old held turn"], old_pid);
  assert_eq!(prompt_pids["new settings turn"], new_pid);

  let mut answer = text_event("old answer after restart");
  answer["data"]["sessionID"] = json!(turn.session_id);
  harness.control(json!({"emit": answer})).await.unwrap();
  harness
    .control(json!({
      "emit": {"type": "session.execution.succeeded", "data": {"sessionID": turn.session_id}}
    }))
    .await
    .unwrap();
  tokio::time::timeout(Duration::from_secs(5), async {
    while super::relay::same_process(identity.pid, &relay_start) {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("the old turn relay ends after its original serve completes");
  tokio::time::timeout(Duration::from_secs(5), async {
    while adopted[0].process_alive() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("the old adopted serve stops after its last turn ends");
  let (translator, cursor) = replay_translator_prefix(&run_directory);
  let recovered = Arc::new(Mutex::new(Vec::new()));
  let summary = follow_from_cursor(&run_directory, cursor, translator, || false, Arc::clone(&recovered)).await.unwrap();
  assert!(summary.saw_result);
  assert!(collected_events(&recovered).iter().any(|event| {
    event["type"] == event_types::ASSISTANT && event["message"]["content"][0]["text"] == "old answer after restart"
  }));
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn a_turn_ending_during_downtime_delivers_its_answer_by_the_dead_process_rule() {
  let harness = Harness::new(json!({"prompts": {
    "paused": {"events": [text_event("before downtime")], "hold": true}
  }}));
  let (runner, initial_events, task) = harness.start_run("paused", LaunchArgs::default(), None, vec![], Env::new());
  harness
    .wait_for_log(|logs| logs.iter().any(|entry| entry.get("prompt_text").and_then(Value::as_str) == Some("paused")))
    .await;
  let run_directory = run_directory(&harness);
  harness.wait_for_raw_cursor(&run_directory).await;
  harness
    .wait_for_events(&initial_events, |events| events.iter().any(|event| event["type"] == event_types::ASSISTANT))
    .await;
  let turn = super::relay::read_turn_record(&run_directory).unwrap().expect("the turn opened a session");
  let identity = harness
    .turn_processes
    .lock()
    .expect("test turn process list is not poisoned")
    .iter()
    .find_map(|(prompt, identity)| (prompt == "paused").then(|| identity.clone()))
    .expect("the paused turn has a relay process");
  runner.detach();
  task.abort();
  assert!(task.await.unwrap_err().is_cancelled());
  harness.pool.abandon_for_restart_test().await;

  let answer = json!({
    "type": "session.text.delta",
    "data": {"assistantMessageID": "m2", "ordinal": 0, "delta": "answer during downtime", "sessionID": turn.session_id}
  });
  harness.control(json!({"emit": answer})).await.unwrap();
  harness
    .control(json!({"emit": {"type": "session.execution.succeeded", "data": {"sessionID": turn.session_id}}}))
    .await
    .unwrap();
  let relay_start = identity.pid_start.clone().expect("relay identity has a start time");
  tokio::time::timeout(Duration::from_secs(5), async {
    while super::relay::same_process(identity.pid, &relay_start) {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("the relay ended while the server was down");

  let pool = harness.adopt_pool();
  assert!(pool.adopted_servers_test().is_empty());
  let (translator, cursor) = replay_translator_prefix(&run_directory);
  let recovered_events = Arc::new(Mutex::new(Vec::new()));
  let summary =
    follow_from_cursor(&run_directory, cursor, translator, || false, Arc::clone(&recovered_events)).await.unwrap();
  assert!(summary.saw_result);
  let recovered_events = collected_events(&recovered_events);
  assert!(recovered_events.iter().any(|event| {
    event["type"] == event_types::ASSISTANT && event["message"]["content"][0]["text"] == "answer during downtime"
  }));
  let raw_path = run_directory.join(charliebot_store::run_records::RAW_LOG_NAME);
  let mut translator = OpenCodeTranslator::with_model_limit(MODEL.to_string(), turn.model_limit);
  let (_, result, exit_code) =
    recovery::scan_result_exit(&raw_path, move |raw| translator.translate_event(&raw)).unwrap();
  assert_eq!(exit_code, 0);
  assert_eq!(result.unwrap()["type"], event_types::RESULT);
  assert_eq!(collected_events(&initial_events).last().unwrap()["message"]["content"][0]["text"], "before downtime");
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn stopping_an_adopted_turn_interrupts_through_its_connection_record_and_ends_the_relay() {
  let harness = Harness::new(json!({"prompts": {"paused": {"events": [], "hold": true}}}));
  let (runner, _, task) = harness.start_run("paused", LaunchArgs::default(), None, vec![], Env::new());
  harness
    .wait_for_log(|logs| logs.iter().any(|entry| entry.get("prompt_text").and_then(Value::as_str) == Some("paused")))
    .await;
  let run_directory = run_directory(&harness);
  let turn = super::relay::read_turn_record(&run_directory).unwrap().expect("the turn opened a session");
  let identity = harness
    .turn_processes
    .lock()
    .expect("test turn process list is not poisoned")
    .iter()
    .find_map(|(prompt, identity)| (prompt == "paused").then(|| identity.clone()))
    .expect("the paused turn has a relay process");
  let relay_start = identity.pid_start.clone().expect("relay identity has a start time");
  runner.detach();
  task.abort();
  assert!(task.await.unwrap_err().is_cancelled());
  harness.pool.abandon_for_restart_test().await;
  let pool = harness.adopt_pool();
  let adopted = pool.adopted_servers_test();
  assert_eq!(adopted.len(), 1);
  harness
    .wait_for_log(|logs| {
      logs.iter().filter(|entry| entry["method"] == "GET" && entry["path"] == "/api/event").count() == 3
    })
    .await;

  server::finish_active_turn(identity.pid).await.unwrap();
  tokio::time::timeout(Duration::from_secs(5), async {
    while super::relay::same_process(identity.pid, &relay_start) {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("the adopted relay exits after the OpenCode interrupt");
  assert!(harness.logs().iter().any(|entry| {
    entry["method"] == "POST" && entry["path"] == format!("/api/session/{}/interrupt", turn.session_id)
  }));
  let raw = fs::read_to_string(run_directory.join(charliebot_store::run_records::RAW_LOG_NAME)).unwrap();
  assert!(raw.lines().any(|line| {
    serde_json::from_str::<Value>(line).unwrap().get("type").and_then(Value::as_str)
      == Some("session.execution.interrupted")
  }));
  tokio::time::timeout(Duration::from_secs(5), async {
    while adopted[0].process_alive() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("the adopted serve stops after its interrupted turn ends");
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn adopted_serve_stops_when_relay_exits_without_a_terminal_event() {
  let harness = Harness::new(json!({"prompts": {"paused": {"events": [], "hold": true}}}));
  let (runner, _, task) = harness.start_run("paused", LaunchArgs::default(), None, vec![], Env::new());
  harness
    .wait_for_log(|logs| logs.iter().any(|entry| entry.get("prompt_text").and_then(Value::as_str) == Some("paused")))
    .await;
  let identity =
    harness.turn_processes.lock().unwrap().iter().find(|(prompt, _)| prompt == "paused").unwrap().1.clone();
  runner.detach();
  task.abort();
  assert!(task.await.unwrap_err().is_cancelled());
  harness.pool.abandon_for_restart_test().await;
  let pool = harness.adopt_pool();
  let adopted = pool.adopted_servers_test();
  assert_eq!(adopted.len(), 1);
  charliebot_agents::transport::kill::kill_process_group(identity.pid, nix::sys::signal::Signal::SIGKILL);
  tokio::time::timeout(Duration::from_secs(5), async {
    while adopted[0].process_alive() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("the adopted serve stops when its last relay exits without a terminal event");
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn reused_run_directory_sends_only_the_new_prompt() {
  let harness = Harness::new(json!({"prompts": {
    "first": {"events": [text_event("first answer")]},
    "second": {"events": [text_event("second answer")]}
  }}));
  let directory = harness.run_logs_directory().join("reused");
  let launch = LaunchArgs { log_dir: Some(directory), ..LaunchArgs::default() };
  let (_, _, first) = harness.start_run("first", launch.clone(), None, vec![], Env::new());
  assert_eq!(first.await.unwrap().unwrap().exit_code, Some(0));
  let (_, events, second) = harness.start_run("second", launch, None, vec![], Env::new());
  assert_eq!(second.await.unwrap().unwrap().exit_code, Some(0));
  assert!(collected_events(&events).iter().any(|event| {
    event["type"] == event_types::ASSISTANT && event["message"]["content"][0]["text"] == "second answer"
  }));
  let prompts = harness.logs().into_iter().filter_map(|entry| entry.get("prompt_text").cloned()).collect::<Vec<_>>();
  assert_eq!(prompts, vec![json!("first"), json!("second")]);
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

// Python: tests/test_opencode_backend.py:666.
#[tokio::test]
async fn turn_end_steps_run_in_order_and_remove_the_session_shells() {
  let harness = Harness::new(json!({"prompts": {"work": {"shells": [{}], "events": [
    {"type": "session.execution.failed", "data": {"error": {"message": "model refused"}}}
  ]}}}));
  let (_, output, task) = harness.start_run("work", LaunchArgs::default(), None, vec![], Env::new());
  let outcome = task.await.unwrap().unwrap();
  let events = collected_events(&output);
  assert_eq!(events.last().unwrap()["type"], event_types::ERROR);
  assert_eq!(events.last().unwrap()["message"], "model refused");
  assert_eq!(outcome.exit_code, Some(1));
  let calls = harness
    .logs()
    .iter()
    .filter_map(|entry| {
      let method = entry.get("method")?.as_str()?;
      let path = entry.get("path")?.as_str()?;
      Some(format!("{method} {path}"))
    })
    .collect::<Vec<_>>();
  let interrupt = calls.iter().position(|call| call.ends_with("/interrupt")).unwrap();
  let shell_list = calls.iter().position(|call| call.starts_with("GET /api/shell")).unwrap();
  let shell_delete = calls.iter().position(|call| call.starts_with("DELETE /api/shell/")).unwrap();
  assert!(interrupt < shell_list && shell_list < shell_delete);
  let release = calls.iter().position(|call| call == "DELETE /api/debug/location");
  assert!(release.is_some(), "turn cleanup did not release a directory");
  assert!(shell_delete < release.unwrap());
  let released = harness
    .logs()
    .into_iter()
    .filter(|entry| entry["method"] == "DELETE" && entry["path"] == "/api/debug/location")
    .map(|entry| entry["query"]["location[directory]"][0].as_str().unwrap().to_string())
    .collect::<Vec<_>>();
  assert_eq!(released, vec![harness.root.path().join("work").display().to_string()]);
  assert!(harness.server.process_alive());
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

// Python: tests/test_opencode_backend.py:765.
// Python: tests/test_opencode_backend.py:322.
#[tokio::test]
async fn terminate_releases_only_unclaimed_directories_once_before_returning() {
  let harness = Harness::new(json!({
    "prompts": {
      "held": {"events": [], "hold": true},
      "quick": {"events": [text_event("done")]}
    }
  }));
  let directory = harness.root.path().join("work");
  let (runner, _, held_task) = harness.start_run("held", LaunchArgs::default(), None, vec![], Env::new());
  harness
    .wait_for_log(|logs| logs.iter().any(|entry| entry.get("prompt_text").and_then(Value::as_str) == Some("held")))
    .await;
  harness
    .server
    .client_request(reqwest::Method::GET, "/api/model")
    .unwrap()
    .query(&[("location[directory]", harness.root.path().join("unused").display().to_string())])
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
  let (_, _, quick_task) = harness.start_run("quick", LaunchArgs::default(), None, vec![], Env::new());
  assert_eq!(quick_task.await.unwrap().unwrap().exit_code, Some(0));
  assert_eq!(loaded_directories(&harness).await, vec![directory.display().to_string()]);
  tokio::time::timeout(Duration::from_secs(5), runner.terminate()).await.unwrap().unwrap();
  assert_eq!(loaded_directories(&harness).await, Vec::<String>::new());
  assert_eq!(
    tokio::time::timeout(Duration::from_secs(5), held_task).await.unwrap().unwrap().unwrap().exit_code,
    Some(-libc::SIGTERM)
  );
  let releases = harness
    .logs()
    .into_iter()
    .filter(|entry| entry["method"] == "DELETE" && entry["path"] == "/api/debug/location")
    .map(|entry| entry["query"]["location[directory]"][0].as_str().unwrap().to_string())
    .collect::<Vec<_>>();
  assert_eq!(releases.iter().filter(|released| released.as_str() == directory.display().to_string()).count(), 1);
  assert_eq!(
    releases
      .iter()
      .filter(|released| released.as_str() == harness.root.path().join("unused").display().to_string())
      .count(),
    1
  );
  harness.clear_finished_turn_pids();
}

// Python: tests/test_opencode_backend.py:504.
#[tokio::test]
async fn failed_release_is_reported_as_a_turn_error_event() {
  let harness = Harness::with_settings(
    json!({"release_status": 500, "prompts": {"work": {"events": [text_event("done")]}}}),
    None,
    ServerTimeouts::default(),
    None,
  );
  let directory = harness.root.path().join("work").display().to_string();
  let (_, output, task) = harness.start_run("work", LaunchArgs::default(), None, vec![], Env::new());
  let outcome = task.await.unwrap().unwrap();
  let error = collected_events(&output).last().unwrap().clone();
  assert_eq!(error["type"], event_types::ERROR);
  assert!(error["message"].as_str().unwrap().contains(&directory));
  assert!(error["message"].as_str().unwrap().contains("HTTP 500"));
  assert_eq!(outcome.exit_code, Some(1));
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn concurrent_turns_share_one_server_process() {
  // Python: tests/test_opencode_backend.py::test_concurrent_turns_share_one_server_process
  let harness = Harness::new(json!({"prompts": {
    "one": {"events": [text_event("first")]},
    "two": {"events": [text_event("second")]}
  }}));
  let (_, first_events, first_task) = harness.start_run("one", LaunchArgs::default(), None, vec![], Env::new());
  let (_, second_events, second_task) = harness.start_run("two", LaunchArgs::default(), None, vec![], Env::new());
  let first_outcome = first_task.await.unwrap().unwrap();
  let second_outcome = second_task.await.unwrap().unwrap();
  assert_eq!(first_outcome.exit_code, Some(0));
  assert_eq!(second_outcome.exit_code, Some(0));
  assert_eq!(collected_events(&first_events).last().unwrap()["type"], event_types::RESULT);
  assert_eq!(collected_events(&second_events).last().unwrap()["type"], event_types::RESULT);
  let pids = harness.logs().iter().filter_map(|entry| entry["pid"].as_u64()).collect::<std::collections::HashSet<_>>();
  assert_eq!(pids.len(), 1);
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn cancel_signals_only_its_turn_and_the_other_turn_completes() {
  // Python: tests/test_opencode_backend.py::test_cancel_signals_only_its_turn_and_the_other_turn_completes
  let harness = Harness::new(held_script(&["held", "quick"]));
  let (_, cancelled_events, cancelled_task) =
    harness.start_run("held", LaunchArgs::default(), None, vec![], Env::new());
  let (_, other_events, other_task) = harness.start_run("quick", LaunchArgs::default(), None, vec![], Env::new());
  harness
    .wait_for_log(|logs| {
      logs
        .iter()
        .filter(|entry| {
          entry.get("method").and_then(Value::as_str) == Some("POST")
            && entry.get("path").and_then(Value::as_str).is_some_and(|path| path.ends_with("/prompt"))
        })
        .count()
        == 2
    })
    .await;
  let cancelled_pid = harness
    .turn_processes
    .lock()
    .unwrap()
    .iter()
    .find_map(|(prompt, identity)| (prompt == "held").then_some(identity.pid))
    .expect("held turn has a spawned process");
  kill_process_group(cancelled_pid, Signal::SIGTERM);
  let cancelled = cancelled_task.await.unwrap().unwrap();
  assert_eq!(
    cancelled.exit_code,
    Some(-libc::SIGTERM),
    "cancelled events: {:?}; fake requests: {:?}",
    collected_events(&cancelled_events),
    harness.logs()
  );
  assert!(!collected_events(&cancelled_events).iter().any(|event| event["type"] == event_types::ERROR));
  assert!(!other_task.is_finished());
  let other_session = session_id_for_prompt(&harness.logs(), "quick");
  harness
    .control(json!({"emit": {"type": "session.execution.succeeded", "data": {"sessionID": other_session}}}))
    .await
    .unwrap();
  assert_eq!(other_task.await.unwrap().unwrap().exit_code, Some(0));
  assert_eq!(collected_events(&other_events).last().unwrap()["type"], event_types::RESULT);
  assert!(harness.server.process_alive());
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn permission_request_fails_only_its_turn() {
  // Python: tests/test_opencode_backend.py::test_permission_request_fails_only_its_turn
  let harness = Harness::new(json!({"prompts": {
    "risky": {"events": [{"type": "permission.asked", "data": {"permission": "shell", "patterns": ["rm -rf *"]}}]},
    "calm": {"events": [text_event("fine")]}
  }}));
  let (_, risky_events, risky_task) = harness.start_run("risky", LaunchArgs::default(), None, vec![], Env::new());
  let (_, calm_events, calm_task) = harness.start_run("calm", LaunchArgs::default(), None, vec![], Env::new());
  assert_eq!(risky_task.await.unwrap().unwrap().exit_code, Some(1));
  assert_eq!(calm_task.await.unwrap().unwrap().exit_code, Some(0));
  let risky_error = collected_events(&risky_events).last().unwrap().clone();
  assert_eq!(risky_error["type"], event_types::ERROR);
  assert!(risky_error["message"].as_str().unwrap().contains("interactive permission"));
  assert_eq!(collected_events(&calm_events).last().unwrap()["type"], event_types::RESULT);
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn manager_turn_reloads_locations_before_the_prompt() {
  // Python: tests/test_opencode_backend.py::test_manager_turn_reloads_locations_before_the_prompt
  let harness = Harness::new(json!({"prompts": {"manage": {"events": [text_event("managed")]}}}));
  let (_, output, task) = harness.start_run(
    "manage",
    launch_with_instructions(Some("# Fresh instructions"), None, None),
    None,
    vec![],
    Env::new(),
  );
  assert_eq!(task.await.unwrap().unwrap().exit_code, Some(0));
  assert_eq!(collected_events(&output).last().unwrap()["type"], event_types::RESULT);
  assert_eq!(fs::read_to_string(harness.root.path().join("work/AGENTS.md")).unwrap(), "# Fresh instructions");
  let logs = harness.logs();
  let reload =
    logs.iter().position(|entry| entry["method"] == "POST" && entry["path"] == "/api/location/reload").unwrap();
  let prompt = logs
    .iter()
    .position(|entry| entry.get("path").and_then(Value::as_str).is_some_and(|path| path.ends_with("/prompt")))
    .unwrap();
  assert!(reload < prompt);
  assert_eq!(session_id_for_prompt(&logs, "manage"), logs[prompt]["path"].as_str().unwrap().split('/').nth(3).unwrap());
  assert_eq!(logs[prompt]["body"]["text"], "manage");
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

// Python: tests/test_opencode_backend.py:856.
#[tokio::test]
async fn stream_loss_ends_running_turns_before_the_server_stops() {
  let harness = Harness::new(json!({
    "prompts": {"stuck": {"events": [], "shells": [{}], "hold": true}}
  }));
  let (_, output, task) = harness.start_run("stuck", LaunchArgs::default(), None, vec![], Env::new());
  harness
    .wait_for_log(|logs| {
      logs.iter().any(|entry| entry.get("path").and_then(Value::as_str).is_some_and(|path| path.ends_with("/prompt")))
    })
    .await;
  harness.control(json!({"drop_stream": true})).await.unwrap();
  let result = tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap().unwrap();
  let errors =
    collected_events(&output).into_iter().filter(|event| event["type"] == event_types::ERROR).collect::<Vec<_>>();
  assert_eq!(result.exit_code, Some(1));
  assert!(!errors.is_empty());
  assert!(errors[0]["message"].as_str().unwrap().contains("event stream"));
  tokio::time::timeout(Duration::from_secs(5), async {
    while harness.server.process_alive() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("the server shuts down after its event stream is lost");
  assert!(!harness.server.process_alive());
  let deletes = harness.logs().into_iter().filter(|entry| entry["method"] == "DELETE").collect::<Vec<_>>();
  assert_eq!(deletes.len(), 1);
  assert!(deletes[0]["path"].as_str().unwrap().starts_with("/api/shell/"));
  assert!(!harness.logs().iter().any(|entry| entry["method"] == "GET" && entry["path"] == "/api/debug/location"));
  assert_eq!(harness.server.claim_count_test(&harness.root.path().join("work").display().to_string()), 0);
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn server_crash_fails_the_turn_with_its_exit_code_and_the_next_turn_restarts() {
  // Python: tests/test_opencode_backend.py::test_server_crash_fails_the_turn_with_its_exit_code_and_the_next_turn_restarts
  let harness = Harness::new(json!({"prompts": {
    "crash": {"events": [], "hold": true},
    "after": {"events": [text_event("back")]}
  }}));
  let (_, output, task) = harness.start_run("crash", LaunchArgs::default(), None, vec![], Env::new());
  harness
    .wait_for_log(|logs| {
      logs.iter().any(|entry| entry.get("path").and_then(Value::as_str).is_some_and(|path| path.ends_with("/prompt")))
    })
    .await;
  let first_pid = harness.server.pid().unwrap();
  harness.control(json!({"exit": 7})).await.unwrap();
  let outcome = tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap().unwrap();
  assert_eq!(outcome.exit_code, Some(1));
  let error = collected_events(&output).into_iter().find(|event| event["type"] == event_types::ERROR).unwrap();
  let raw = fs::read_dir(harness.run_logs_directory())
    .unwrap()
    .map(|entry| fs::read_to_string(entry.unwrap().path().join(charliebot_store::run_records::RAW_LOG_NAME)).unwrap())
    .collect::<Vec<_>>();
  assert!(
    error["message"].as_str().unwrap().contains("exited with code 7"),
    "error={error:?}; raw={raw:?}; requests={:?}",
    harness.logs()
  );
  let (_, second_events, second_task) = harness.start_run("after", LaunchArgs::default(), None, vec![], Env::new());
  assert_eq!(second_task.await.unwrap().unwrap().exit_code, Some(0));
  assert_eq!(collected_events(&second_events).last().unwrap()["type"], event_types::RESULT);
  assert_ne!(harness.server.pid().unwrap(), first_pid);
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

// Python: tests/test_opencode_backend.py:922.
#[tokio::test]
async fn idle_server_exits_after_the_idle_limit() {
  let timeouts = ServerTimeouts {
    idle_exit: Duration::from_millis(80),
    heavy_check: Duration::from_millis(50),
    heavy_rss_mb: 1_000_000.0,
    ..ServerTimeouts::default()
  };
  let harness =
    Harness::with_settings(json!({"prompts": {"idle": {"events": [text_event("ok")]}}}), None, timeouts, None);
  let (_, output, task) = harness.start_run("idle", LaunchArgs::default(), None, vec![], Env::new());
  assert_eq!(task.await.unwrap().unwrap().exit_code, Some(0));
  assert_eq!(collected_events(&output).last().unwrap()["type"], event_types::RESULT);
  tokio::time::timeout(Duration::from_secs(2), async {
    while harness.server.process_alive() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("idle server did not stop");
  harness.clear_finished_turn_pids();
}

// Python: tests/test_opencode_backend.py:937.
#[tokio::test]
async fn wrong_major_version_fails_the_turn_naming_the_binary() {
  let harness = Harness::with_settings(json!({"prompts": {}}), Some("1.18.18"), ServerTimeouts::default(), None);
  let (_, output, task) = harness.start_run("x", LaunchArgs::default(), None, vec![], Env::new());
  assert_eq!(task.await.unwrap().unwrap().exit_code, Some(1));
  let error = collected_events(&output).last().unwrap().clone();
  assert_eq!(error["type"], event_types::ERROR);
  assert!(error["message"].as_str().unwrap().contains("1.18.18"));
  assert!(error["message"].as_str().unwrap().contains(&harness.host.program.display.display().to_string()));
  assert!(!harness.server.process_alive());
  assert_eq!(harness.server.claim_count_test(&harness.root.path().join("work").display().to_string()), 0);
  harness.clear_finished_turn_pids();
}

// Python: tests/test_opencode_backend.py:950.
#[tokio::test]
async fn missing_resume_session_fails_the_turn_naming_the_id() {
  let harness = Harness::new(json!({"prompts": {}}));
  harness.server.ensure_running().await.unwrap();
  let directory = harness.root.path().join("work").display().to_string();
  harness
    .server
    .client_request(reqwest::Method::GET, "/api/model")
    .unwrap()
    .query(&[("location[directory]", directory.clone())])
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
  let (_, output, task) =
    harness.start_run("x", launch_with_instructions(None, None, Some("ses_gone")), None, vec![], Env::new());
  assert_eq!(task.await.unwrap().unwrap().exit_code, Some(1));
  let error = collected_events(&output).last().unwrap().clone();
  assert_eq!(error["type"], event_types::ERROR);
  assert!(error["message"].as_str().unwrap().contains("ses_gone"));
  assert!(error["message"].as_str().unwrap().contains(&harness.host.data_dir().display().to_string()));
  assert!(!harness.logs().iter().any(|entry| entry["method"] == "POST" && entry["path"] == "/api/session"));
  let releases = harness
    .logs()
    .into_iter()
    .filter(|entry| entry["method"] == "DELETE" && entry["path"] == "/api/debug/location")
    .map(|entry| entry["query"]["location[directory]"][0].as_str().unwrap().to_string())
    .collect::<Vec<_>>();
  assert_eq!(releases, vec![directory.clone()]);
  assert_eq!(harness.server.claim_count_test(&directory), 0);
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn missing_join_script_fails_before_the_environment_is_set() {
  // Python: tests/test_opencode_backend.py::test_missing_join_script_fails_before_the_environment_is_set
  let missing_repo = tempfile::tempdir().unwrap();
  let harness = Harness::with_settings(
    json!({"prompts": {}}),
    None,
    ServerTimeouts::default(),
    Some(missing_repo.path().to_path_buf()),
  );
  let (_, output, task) = harness.start_run("x", LaunchArgs::default(), None, vec![], Env::new());
  assert_eq!(task.await.unwrap().unwrap().exit_code, Some(1));
  let error = collected_events(&output).last().unwrap().clone();
  assert_eq!(error["type"], event_types::ERROR);
  assert!(error["message"].as_str().unwrap().contains("join script is missing"));
  assert!(!harness.logs().iter().any(|entry| entry["method"] == "PUT"));
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn silent_session_fails_after_the_progress_timeout() {
  // Python: tests/test_opencode_backend.py::test_silent_session_fails_after_the_progress_timeout
  let timeouts = ServerTimeouts { progress: Duration::from_millis(50), ..ServerTimeouts::default() };
  let harness = Harness::with_settings(held_script(&["quiet"]), None, timeouts, None);
  let (_, output, task) = harness.start_run("quiet", LaunchArgs::default(), None, vec![], Env::new());
  let outcome = tokio::time::timeout(Duration::from_secs(3), task).await.unwrap().unwrap().unwrap();
  assert_eq!(outcome.exit_code, Some(1));
  let error = collected_events(&output).last().unwrap().clone();
  assert_eq!(error["type"], event_types::ERROR);
  assert!(error["message"].as_str().unwrap().contains("produced no event"));
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn prompt_waits_for_an_earlier_interrupt_to_settle() {
  // Python: tests/test_opencode_backend.py::test_prompt_waits_for_an_earlier_interrupt_to_settle
  let harness = Harness::new(json!({
    "interrupt_event_delay": 0.12,
    "prompts": {"warm": {"events": [text_event("warm")]}, "next": {"events": [text_event("answered")]}}
  }));
  let (_, _, first_task) = harness.start_run("warm", LaunchArgs::default(), None, vec![], Env::new());
  assert_eq!(first_task.await.unwrap().unwrap().exit_code, Some(0));
  let session_id = session_id_for_prompt(&harness.logs(), "warm");
  harness.server.ensure_running().await.unwrap();
  harness.control(json!({"activate": session_id})).await.unwrap();
  harness
    .control(json!({"emit": {"type": "session.execution.started", "data": {"sessionID": session_id}}}))
    .await
    .unwrap();
  harness
    .wait_for_log(|logs| {
      logs
        .iter()
        .any(|entry| entry.get("path").and_then(Value::as_str).is_some_and(|path| path.ends_with("/interrupt")))
    })
    .await;
  let started = tokio::time::Instant::now();
  let (_, output, task) =
    harness.start_run("next", launch_with_instructions(None, None, Some(&session_id)), None, vec![], Env::new());
  assert_eq!(task.await.unwrap().unwrap().exit_code, Some(0));
  assert!(tokio::time::Instant::now().duration_since(started) >= Duration::from_millis(100));
  assert_eq!(collected_events(&output).last().unwrap()["type"], event_types::RESULT);
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn one_shot_runs_standalone_in_a_private_root_and_fails_on_an_error_line() {
  // Python: tests/test_opencode_backend.py::test_one_shot_runs_standalone_in_a_private_root_and_fails_on_an_error_line
  let harness = Harness::new(json!({
    "run_lines": [{"type": "text", "part": {"text": "  answer  "}}],
    "run_exit": 0
  }));
  let mut backend = OpenCodeBackend::new(
    MODEL.to_string(),
    OpenCodeOption::default(),
    Map::new(),
    harness.host.clone(),
    Arc::clone(&harness.pool),
    LaunchArgs::default(),
    SessionCgroupConfig::disabled(),
  );
  let empty_env = Env::new();
  let answer =
    backend.one_shot_text(&empty_env, "prompt", "system", Some(Duration::from_secs(3))).unwrap().await.unwrap();
  assert_eq!(answer, "answer");
  let first = harness.logs().into_iter().find(|entry| entry.get("run_args").is_some()).unwrap();
  let args = first["run_args"].as_array().unwrap();
  assert_eq!(args[0], "--standalone");
  assert!(args.iter().any(|argument| argument == "--standalone"));
  let data = PathBuf::from(first["xdg_data_home"].as_str().unwrap());
  let state = PathBuf::from(first["xdg_state_home"].as_str().unwrap());
  let cache = PathBuf::from(first["xdg_cache_home"].as_str().unwrap());
  assert!(data.parent().unwrap().file_name().unwrap().to_string_lossy().starts_with("charliebot-opencode-oneshot-"));
  assert_eq!(data.parent(), state.parent());
  assert_eq!(data.parent(), cache.parent());
  assert!(!data.parent().unwrap().exists());

  fs::write(
    &harness.script_path,
    serde_json::to_vec(&json!({"run_lines": [{"type": "error", "error": {"message": "denied"}}], "run_exit": 0}))
      .unwrap(),
  )
  .unwrap();
  let error =
    backend.one_shot_text(&empty_env, "prompt", "system", Some(Duration::from_secs(3))).unwrap().await.unwrap_err();
  assert!(error.to_string().contains("opencode run failed: denied"));
}

const HEADLESS_PLAIN: &str = concat!(
  r#"{"permission":{"*":"allow","question":"deny","subagent":"deny","opencode_session_move":"deny"},"#,
  r#""shell":"bash","default_agent":"charliebot","agent":{"charliebot":{"mode":"primary"}}}"#,
);
const HEADLESS_WITH_LIMITS: &str = concat!(
  r#"{"permission":{"*":"allow","question":"deny","subagent":"deny","opencode_session_move":"deny"},"#,
  r#""shell":"bash","default_agent":"charliebot","agent":{"charliebot":{"mode":"primary"}},"#,
  r#""providers":{"fakeprov":{"models":{"fake-model":{"limit":{"context":420000}}}},"#,
  r#""other":{"models":{"model":{"limit":{"context":200000}}}}}}"#,
);
const HEADLESS_MERGED: &str = concat!(
  r#"{"permission":{"*":"allow","question":"deny","subagent":"deny","opencode_session_move":"deny"},"#,
  r#""shell":"bash","default_agent":"charliebot","agent":{"charliebot":{"mode":"primary"}},"#,
  r#""providers":{"provider":{"models":{"model":{"limit":{"context":420000}},"other":{"limit":{"context":120000}}}},"#,
  r#""other":{"models":{"model":{"limit":{"context":200000}}}}}}"#,
);
const ONE_SHOT_PLAIN: &str = r#"{"permission":{"*":"deny"}}"#;
const ONE_SHOT_LIMITED: &str =
  r#"{"permission":{"*":"deny"},"providers":{"fakeprov":{"models":{"fake-model":{"limit":{"context":420000}}}}}}"#;

fn opencode_option(id: &str, model: &str, type_fields: &str) -> BackendOption {
  BackendOption {
    id: id.to_string(),
    label: id.to_string(),
    model: model.to_string(),
    prompt_overlay: None,
    backend_type: "opencode".to_string(),
    type_fields: serde_yaml::from_str(type_fields).unwrap(),
  }
}

/// The synthetic config of `root` with `options` as its `backends.options`.
fn config_with(root: &std::path::Path, options: Vec<BackendOption>) -> CharlieBotConfig {
  let mut cfg = Registry::new().load_config(root).unwrap();
  cfg.backends.options = options;
  cfg
}

/// The `OPENCODE_CONFIG_CONTENT` that the shared server of the harness starts with.
fn headless_content(harness: &Harness) -> String {
  harness.server.server_env_from(Env::new(), "pw").unwrap()["OPENCODE_CONFIG_CONTENT"].clone()
}

/// The `OPENCODE_CONFIG_CONTENT` of the newest one-shot run that the fake opencode logged.
fn last_one_shot_config(harness: &Harness) -> Value {
  harness.logs().into_iter().rev().find(|entry| entry.get("run_args").is_some()).unwrap()["config"].clone()
}

#[tokio::test]
async fn recovered_translator_keeps_the_original_model_after_settings_change() {
  let harness = Harness::new(json!({}));
  let directory = harness.root.path().join("old-run");
  let turn = super::relay::TurnRecord {
    server_record: "server.json".to_string(),
    session_id: "old-session".to_string(),
    prompt_request: json!({"text": "old prompt"}),
    model: MODEL.to_string(),
    model_limit: Some(Map::from_iter([("context".to_string(), json!(123_000))])),
  };
  super::relay::write_turn_record(&directory.join(super::relay::TURN_RECORD_NAME), &turn).unwrap();
  let option = opencode_option("changed", "other/new-model", "{}");
  let cfg = config_with(harness.root.path(), vec![option.clone()]);
  let factory = OpenCodeFactory::new(harness.host.clone(), Arc::clone(&harness.pool));
  let launch = LaunchArgs { log_dir: Some(directory), ..LaunchArgs::default() };
  let backend = factory.build(Some(&option), &cfg, launch).unwrap();
  let mut translator = backend.translator();
  translator.translate(usage_event(10, 4)).unwrap();
  let result = translator.translate(json!({"type": "session.execution.succeeded", "data": {}})).unwrap();
  assert_eq!(result[0]["context_snapshot"]["model"], MODEL);
  assert_eq!(result[0]["context_snapshot"]["limit"]["context"], 123_000);
}

#[tokio::test]
async fn context_limit_reaches_the_one_shot_and_the_shared_server_configs() {
  // Python: tests/test_opencode_backend.py::test_one_shot_config_uses_its_option_and_denies_tools
  // Python: tests/test_opencode_backend.py::test_headless_config_keeps_existing_keys_and_reads_live_options
  let harness = Harness::new(json!({"run_lines": [{"type": "text", "part": {"text": "answer"}}], "run_exit": 0}));
  let factory = OpenCodeFactory::new(harness.host.clone(), Arc::clone(&harness.pool));
  let plain = opencode_option("plain", MODEL, "{}");
  let limited = opencode_option("limited", MODEL, "context_limit: 420000\n");
  let unrelated = opencode_option("unrelated", "other/model", "context_limit: 200000\n");

  // The shared server starts with the options of the latest launch, so an option without a limit adds no providers.
  let cfg = config_with(harness.root.path(), vec![plain.clone()]);
  let mut backend = factory.build(Some(&plain), &cfg, LaunchArgs::default()).unwrap();
  let empty_env = Env::new();
  assert_eq!(headless_content(&harness), HEADLESS_PLAIN);
  let answer = backend.one_shot_text(&empty_env, "prompt", "system", Some(Duration::from_secs(3)));
  assert_eq!(answer.unwrap().await.unwrap(), "answer");
  assert_eq!(last_one_shot_config(&harness), ONE_SHOT_PLAIN);

  // The one-shot config carries its own option's limit, and the shared server carries every option's limit.
  let cfg = config_with(harness.root.path(), vec![limited.clone(), unrelated]);
  let built = factory.build(Some(&limited), &cfg, LaunchArgs::default());
  assert!(built.is_ok(), "the factory refused context_limit: {:?}", built.as_ref().err());
  assert_eq!(headless_content(&harness), HEADLESS_WITH_LIMITS);
  let mut backend = built.unwrap();
  let answer = backend.one_shot_text(&empty_env, "prompt", "system", Some(Duration::from_secs(3)));
  assert_eq!(answer.unwrap().await.unwrap(), "answer");
  assert_eq!(last_one_shot_config(&harness), ONE_SHOT_LIMITED);
}

#[test]
fn conflicting_context_limits_fail_the_shared_server_start_naming_both_options() {
  // Python: tests/test_opencode_backend.py::test_model_overrides_config_rejects_conflicts_naming_both_options
  let harness = Harness::new(json!({}));
  let factory = OpenCodeFactory::new(harness.host.clone(), Arc::clone(&harness.pool));
  let first = opencode_option("oc-first", MODEL, "context_limit: 420000\n");
  let second = opencode_option("oc-second", MODEL, "context_limit: 120000\n");
  let cfg = config_with(harness.root.path(), vec![first.clone(), second]);

  let built = factory.build(Some(&first), &cfg, LaunchArgs::default());
  assert!(built.is_ok(), "the factory refused context_limit: {:?}", built.as_ref().err());
  let error = harness.server.server_env_from(Env::new(), "pw").unwrap_err().to_string();
  assert!(error.contains("oc-first") && error.contains("oc-second"), "{error}");
}

#[test]
fn headless_providers_merge_equal_limits_and_keep_distinct_models() {
  // Python: tests/test_opencode_backend.py::test_model_overrides_config_merges_equal_limits_and_distinct_models
  let harness = Harness::new(json!({}));
  let factory = OpenCodeFactory::new(harness.host.clone(), Arc::clone(&harness.pool));
  let options = vec![
    opencode_option("first", "provider/model", "context_limit: 420000\n"),
    opencode_option("equal", "provider/model", "context_limit: 420000\n"),
    opencode_option("uncapped", "provider/model", "{}"),
    opencode_option("sibling", "provider/other", "context_limit: 120000\n"),
    opencode_option("other", "other/model", "context_limit: 200000\n"),
  ];
  let cfg = config_with(harness.root.path(), options.clone());

  let built = factory.build(Some(&options[0]), &cfg, LaunchArgs::default());
  assert!(built.is_ok(), "the factory refused context_limit: {:?}", built.as_ref().err());
  assert_eq!(headless_content(&harness), HEADLESS_MERGED);
}

#[tokio::test]
async fn opencode_prepare_cwd_writes_agents_md_even_when_config_exists() {
  // Python: tests/test_backend_prepare_cwd_contract.py::test_opencode_prepare_cwd_writes_agents_md_even_when_config_exists
  // Python: tests/test_backend_prepare_cwd_contract.py::test_prepare_cwd_writes_instructions_file_when_provided
  let harness = Harness::new(json!({"prompts": {"manager": {"events": [text_event("ok")]}}}));
  let work = harness.root.path().join("work");
  fs::create_dir_all(work.join(".opencode")).unwrap();
  fs::write(work.join(".opencode/opencode.json"), b"{}").unwrap();
  let (_, _, task) = harness.start_run(
    "manager",
    launch_with_instructions(Some("# Instructions"), None, None),
    None,
    vec![],
    Env::new(),
  );
  assert_eq!(task.await.unwrap().unwrap().exit_code, Some(0));
  assert_eq!(fs::read_to_string(work.join("AGENTS.md")).unwrap(), "# Instructions");
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn opencode_worker_prompt_is_framed_and_manager_prompt_stays_bare() {
  // Python: tests/test_backend_prepare_cwd_contract.py::test_opencode_worker_prompt_is_framed_and_manager_prompt_stays_bare
  // Python: tests/test_backend_prepare_cwd_contract.py::test_run_instructions_land_in_run_dir_and_tracked_guides_stay_untouched
  let framed = "<system-instructions>\nworker instructions\n</system-instructions>\n\nwork";
  let harness = Harness::new(json!({"prompts": {
    framed: {"events": [text_event("worker")]},
    "manage": {"events": [text_event("manager")]}
  }}));
  let work = harness.root.path().join("work");
  fs::create_dir_all(&work).unwrap();
  fs::write(work.join("CLAUDE.md"), "tracked Claude guide").unwrap();
  fs::write(work.join("AGENTS.md"), "tracked agent guide").unwrap();
  let instruction_file = harness.root.path().join("run/harness_instructions.md");
  fs::create_dir_all(instruction_file.parent().unwrap()).unwrap();
  let (_, _, worker_task) = harness.start_run(
    "work",
    launch_with_instructions(Some("worker instructions"), Some(instruction_file.clone()), None),
    None,
    vec![],
    Env::new(),
  );
  assert_eq!(worker_task.await.unwrap().unwrap().exit_code, Some(0));
  assert_eq!(fs::read_to_string(&instruction_file).unwrap(), "worker instructions");
  assert_eq!(fs::read_to_string(work.join("CLAUDE.md")).unwrap(), "tracked Claude guide");
  assert_eq!(fs::read_to_string(work.join("AGENTS.md")).unwrap(), "tracked agent guide");
  let (_, _, manager_task) = harness.start_run(
    "manage",
    launch_with_instructions(Some("manager instructions"), None, None),
    None,
    vec![],
    Env::new(),
  );
  assert_eq!(manager_task.await.unwrap().unwrap().exit_code, Some(0));
  let prompts = harness
    .logs()
    .into_iter()
    .filter(|entry| entry.get("path").and_then(Value::as_str).is_some_and(|path| path.ends_with("/prompt")))
    .collect::<Vec<_>>();
  assert_eq!(prompts[0]["body"]["text"], framed);
  assert_eq!(prompts[1]["body"]["text"], "manage");
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn opencode_prepare_cwd_skips_agents_md_when_instructions_are_unset() {
  // Python: tests/test_backend_prepare_cwd_contract.py::test_prepare_cwd_skips_instructions_file_when_unset
  let harness = Harness::new(json!({"prompts": {"plain": {"events": [text_event("ok")]}}}));
  let (_, _, task) = harness.start_run("plain", LaunchArgs::default(), None, vec![], Env::new());
  assert_eq!(task.await.unwrap().unwrap().exit_code, Some(0));
  assert!(!harness.root.path().join("work/AGENTS.md").exists());
  harness.stop().await.unwrap();
  harness.clear_finished_turn_pids();
}

#[tokio::test]
async fn unclaimed_execution_is_interrupted() {
  // Python: tests/test_opencode_backend.py::test_unclaimed_execution_is_interrupted
  let harness = Harness::new(json!({"interrupt_event_delay": 0.01, "prompts": {}}));
  harness.server.ensure_running().await.unwrap();
  let response = harness
    .server
    .client_request(reqwest::Method::POST, "/api/session")
    .unwrap()
    .json(
      &json!({"location": {"directory": harness.root.path()}, "model": {"id": "fake-model", "providerID": "fakeprov"}}),
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .json::<Value>()
    .await
    .unwrap();
  let session_id = response["data"]["id"].as_str().unwrap().to_string();
  harness.control(json!({"activate": session_id})).await.unwrap();
  harness.server.dispatch_test_event(json!({"type": "session.execution.started", "data": {"sessionID": session_id}}));
  harness
    .wait_for_log(|logs| {
      logs
        .iter()
        .any(|entry| entry.get("path").and_then(Value::as_str).is_some_and(|path| path.ends_with("/interrupt")))
    })
    .await;
  harness.server.wait_interrupts_settled(&session_id).await.unwrap();
  harness.stop().await.unwrap();
}

#[tokio::test]
async fn earlier_interrupt_wait_times_out_with_the_session_id() {
  // Python: tests/test_opencode_backend.py::test_waiting_for_earlier_interrupts_times_out
  let timeouts = ServerTimeouts { interrupt_settle: Duration::from_millis(20), ..ServerTimeouts::default() };
  let harness = Harness::with_settings(json!({}), None, timeouts, None);
  harness.server.add_pending_test("ses_x");
  let error = harness.server.wait_interrupts_settled("ses_x").await.unwrap_err();
  assert!(error.to_string().contains("ses_x"));
  assert!(error.to_string().contains("did not settle"));
}

#[tokio::test]
async fn dispatch_routes_to_the_claimed_turn_and_settles_pending_interrupts() {
  // Python: tests/test_opencode_backend.py::test_dispatch_routes_to_the_claimed_turn_and_settles_pending_interrupts
  let harness = Harness::new(json!({}));
  let turn = harness.server.register("ses_1", "/synthetic/work", 999_999).unwrap();
  harness.server.add_pending_test("ses_1");
  harness
    .server
    .dispatch_test_event(json!({"type": "session.text.delta", "data": {"sessionID": "ses_1", "delta": "x"}}));
  harness.server.dispatch_test_event(json!({"type": "session.execution.interrupted", "data": {"sessionID": "ses_1"}}));
  assert!(harness.server.wait_interrupts_settled("ses_1").await.is_ok());
  let first = tokio::time::timeout(Duration::from_millis(100), turn.next_event()).await.unwrap();
  let second = tokio::time::timeout(Duration::from_millis(100), turn.next_event()).await.unwrap();
  assert!(matches!(first, super::server::TurnEvent::Event(event) if event["type"] == "session.text.delta"));
  assert!(matches!(second, super::server::TurnEvent::Event(event) if event["type"] == "session.execution.interrupted"));
  harness
    .server
    .dispatch_test_event(json!({"type": "session.text.delta", "data": {"sessionID": "ses_other", "delta": "y"}}));
  assert_eq!(harness.server.turn_count_test(), 1);
  turn.finish().await.unwrap();
}

#[test]
fn option_model_accepts_proxy_url_and_rejects_other_fields() {
  let mut fields = serde_yaml::Mapping::new();
  fields.insert(
    serde_yaml::Value::String("proxy_url".to_string()),
    serde_yaml::Value::String("http://proxy.test:8080".to_string()),
  );
  assert_eq!(OpenCodeOption::from_fields(&fields).unwrap().proxy_url.as_deref(), Some("http://proxy.test:8080"));
  fields.insert(serde_yaml::Value::String("other".to_string()), serde_yaml::Value::String("value".to_string()));
  assert!(OpenCodeOption::from_fields(&fields).unwrap_err().contains("other"));
}

#[test]
fn registration_keeps_traits_attribution_and_snapshot_limits() {
  let mut registry = Registry::new();
  registry.register_package("src.backends.opencode", super::tests::register_synthetic).unwrap();
  let backend_type = registry.backend_type_of("opencode").unwrap();
  assert_eq!(backend_type.traits.resume, charliebot_registry::backend::ResumeStyle::NativeId);
  assert!(backend_type.traits.restart_reattach);
  assert!(!backend_type.traits.preassigned_session_id);
  assert!(!backend_type.traits.manual_compact);
  assert!(!backend_type.translate_fallback);
  let source = registry.usage_source_for("opencode").unwrap().unwrap();
  assert_eq!(source.id_prefixes, &["opencode-", "oc-"]);
  assert_eq!(
    registry
      .reading_limits_for("snapshot", charliebot_store::run_records::RunKind::Work, None)
      .unwrap()
      .compact_reserve,
    Some(20_000)
  );
}

#[tokio::test]
async fn iter_ndjson_events_parses_and_skips() {
  let input = b"\nnot json\n{\"type\":\"text\",\"part\":{\"text\":\"answer\"}}\n";
  let mut reader = tokio::io::BufReader::new(&input[..]);
  let events = Arc::new(Mutex::new(Vec::new()));
  let sink = Arc::clone(&events);
  charliebot_agents::transport::spawn::iter_ndjson_events(&mut reader, move |event| {
    let sink = Arc::clone(&sink);
    async move {
      sink.lock().expect("test event list is not poisoned").push(event);
      Ok(())
    }
  })
  .await
  .unwrap();
  assert_eq!(events.lock().unwrap().as_slice(), [json!({"type": "text", "part": {"text": "answer"}})]);
}

#[test]
fn opencode_join_script_moves_the_shell_and_sets_its_oom_priority() {
  // Python: tests/test_session_cgroup.py::test_opencode_join_script_moves_the_shell_and_sets_its_oom_priority
  let root = tempfile::tempdir().unwrap();
  let procs = root.path().join("cgroup.procs");
  fs::write(&procs, "").unwrap();
  let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../../src/backends/opencode/cgroup_join.sh");
  // bash skips BASH_ENV when the environment says sshd started it (SSH_CLIENT), so the shell starts clean.
  let output = std::process::Command::new("bash")
    .args(["-c", "echo $$; cat /proc/$$/oom_score_adj"])
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .env("BASH_ENV", &script)
    .env("CHARLIEBOT_SESSION_CGROUP", root.path())
    .env("CHARLIEBOT_SESSION_OOM_SCORE_ADJ", "100")
    .output()
    .unwrap();
  assert!(output.status.success(), "the join script must let the shell run: {:?}", output.status);
  let stdout = String::from_utf8(output.stdout).unwrap();
  let mut fields = stdout.split_whitespace();
  let shell_pid = fields.next().unwrap();
  assert_eq!(fs::read_to_string(&procs).unwrap().trim(), shell_pid);
  assert_eq!(fields.next(), Some("100"));
}
