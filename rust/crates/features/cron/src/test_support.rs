use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::backend_pool::BackendPool;
use charliebot_core::config::BackendOption;
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_core::repo_root::RepoRoot;
use charliebot_registry::backend::{
  AgentBackend, BackendFactory, BackendTraits, BackendTypeRegistration, Env, LaunchArgs, ResumeStyle, Translate,
  TransportKind,
};
use charliebot_registry::context::{FeatureContext, StartupState};
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::metadata::{SessionMetadata, TaskProfile, TaskSpec};
use charliebot_store::{CreateTask, Store, TaskCaller};
use nix::errno::Errno;
use nix::sys::prctl::set_child_subreaper;
use nix::sys::signal::{Signal, killpg};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, getpgid};
use serde_json::Map;
use serde_json::Value;

pub fn feature_context() -> (tempfile::TempDir, Arc<FeatureContext>) {
  feature_context_with_extra_backends(&[])
}

pub fn feature_context_with_extra_backends(extra_backend_ids: &[&str]) -> (tempfile::TempDir, Arc<FeatureContext>) {
  let home = tempfile::tempdir().expect("temporary cron home");
  std::fs::create_dir_all(home.path().join("sessions")).expect("session directory provisions");
  std::fs::create_dir_all(home.path().join("config.d/cron.d")).expect("cron directory provisions");
  std::fs::create_dir_all(home.path().join("state")).expect("state directory provisions");
  let mut registry = Registry::new();
  registry.register_package("src.features.cron", crate::register).expect("cron registration");
  registry.register_package("test.cron_handlers", register_test_handler).expect("handler registration");
  registry.register_package("test.cron_backend", register_test_backend).expect("backend registration");
  let registry = Arc::new(registry);
  let mut cfg = registry.load_config(home.path()).expect("default config loads");
  cfg.paths.worktree_dir = home.path().join("worktrees").display().to_string();
  // The synthetic sessions have no metadata; a zero cap starts no session cgroup, so no spawn walks the task-parent chain.
  cfg.server.session_memory_max_mb = 0;
  for id in std::iter::once("synthetic").chain(extra_backend_ids.iter().copied()) {
    cfg.backends.options.push(BackendOption {
      id: id.to_string(),
      label: id.to_string(),
      prompt_overlay: None,
      model: format!("{id}-model"),
      backend_type: "synthetic".to_string(),
      type_fields: serde_yaml::Mapping::new(),
    });
    cfg.backends.pool.push(id.to_string());
  }
  let store = Arc::new(
    Store::new(
      cfg.clone(),
      registry.metadata_slots(),
      charliebot_registry::store_hooks::store_hooks(Arc::clone(&registry)),
    )
    .expect("test store opens"),
  );
  std::fs::write(cfg.credentials_file(), "charliebot: {}\n").expect("credentials file writes");
  let config_value = cfg.clone();
  let config = Arc::new(HotReload::new(
    cfg.config_file(),
    "cron_test_config_reload_failed",
    Box::new(move || Ok(config_value.clone())),
  ));
  let credential_home = cfg.charliebot_home.clone();
  let credentials = Arc::new(HotReload::new(
    cfg.credentials_file(),
    "cron_test_credentials_reload_failed",
    Box::new(move || load_credentials(&credential_home)),
  ));
  let startup = Arc::new(StartupState::default());
  startup
    .insert(AgentBaseEnvironment { values: Env::from([("PATH".to_string(), "/usr/bin".to_string())]) })
    .expect("agent base environment inserts");
  startup.insert(BackendPool::default()).expect("backend pool inserts");
  startup
    .insert(RepoRoot::new(Some(repo_root().into_os_string())).expect("repo root value"))
    .expect("repo root inserts");
  let features = Arc::new(FeatureContext { config, credentials, store, registry, startup });
  (home, features)
}

fn repo_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).expect("crate is nested under rust/crates").to_path_buf()
}

fn register_test_handler(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.scheduled_handler("synthetic_handler", |_features| Box::pin(async { Ok("synthetic result".to_string()) }))
}

struct TestBackendFactory;

impl BackendFactory for TestBackendFactory {
  fn build(
    &self,
    _option: Option<&BackendOption>,
    _cfg: &charliebot_core::config::CharlieBotConfig,
    _launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    Ok(Box::new(TestBackend))
  }
}

struct TestBackend;

impl AgentBackend for TestBackend {
  fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
    None
  }

  fn prepare_transport(&mut self, _log_dir: &Path) -> anyhow::Result<()> {
    Ok(())
  }

  fn build_command(&self, _prompt: &str) -> anyhow::Result<Vec<String>> {
    Ok(vec![
      "/bin/sh".to_string(),
      "-c".to_string(),
      r#"sleep 0.1; printf '%s\n' '{"type":"result","subtype":"success","result":"synthetic migration completed"}'"#
        .to_string(),
    ])
  }

  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    Ok(env)
  }

  fn stdin_prompt(&self, _prompt: &str) -> Option<String> {
    None
  }

  fn translator(&self) -> Box<dyn Translate> {
    Box::new(TestTranslate)
  }

  fn transport(&self) -> TransportKind {
    TransportKind::RawLog
  }
}

struct TestTranslate;

impl Translate for TestTranslate {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
    Ok(vec![raw])
  }
}

fn register_test_backend(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.backend_type(BackendTypeRegistration {
    type_name: "synthetic",
    traits: BackendTraits {
      resume: ResumeStyle::CliFlag,
      restart_reattach: false,
      preassigned_session_id: false,
      family_prefix: None,
      manual_compact: false,
      compact_hint: None,
    },
    option_model: |_| Ok(()),
    factory: Arc::new(TestBackendFactory),
    lifecycle: None,
    translate_fallback: false,
  })
}

pub fn init_repo_with_origin(dir: &Path) -> anyhow::Result<PathBuf> {
  let origin = dir.join("origin.git");
  let repo = dir.join("repo");
  git(dir, &["init", "-q", "--bare", "-b", "main", origin.to_str().expect("synthetic origin path is UTF-8")])?;
  git(
    dir,
    &[
      "clone",
      "-q",
      origin.to_str().expect("synthetic origin path is UTF-8"),
      repo.to_str().expect("synthetic repo path is UTF-8"),
    ],
  )?;
  git(&repo, &["config", "user.email", "synthetic@example.test"])?;
  git(&repo, &["config", "user.name", "Synthetic Test"])?;
  std::fs::write(repo.join("seed.txt"), "synthetic seed\n")?;
  git(&repo, &["add", "."])?;
  git(&repo, &["commit", "-q", "-m", "synthetic seed"])?;
  git(&repo, &["push", "-q", "origin", "main"])?;
  Ok(repo)
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
  let output = Command::new("git").arg("-C").arg(dir).args(args).output()?;
  anyhow::ensure!(
    output.status.success(),
    "git {args:?} failed in {}: {}",
    dir.display(),
    String::from_utf8_lossy(&output.stderr)
  );
  Ok(())
}

pub struct ProcessGroupGuard {
  groups: Vec<Pid>,
}

impl ProcessGroupGuard {
  pub fn new() -> Self {
    if let Err(error) = set_child_subreaper(true) {
      panic!("cron test process guard could not become a child subreaper: {error}");
    }
    Self { groups: Vec::new() }
  }

  pub fn track(&mut self, pid: u32) {
    match getpgid(Some(Pid::from_raw(pid as i32))) {
      Ok(group) => self.groups.push(group),
      Err(Errno::ESRCH) => {}
      Err(error) => panic!("cron test process guard could not inspect process {pid}: {error}"),
    }
  }
}

impl Drop for ProcessGroupGuard {
  fn drop(&mut self) {
    for group in &self.groups {
      let _ = killpg(*group, Signal::SIGKILL);
    }
    for _ in 0..100 {
      for group in &self.groups {
        while matches!(
          waitpid(Pid::from_raw(-group.as_raw()), Some(WaitPidFlag::WNOHANG)),
          Ok(status) if status != WaitStatus::StillAlive
        ) {}
      }
      if self.groups.iter().all(|group| killpg(*group, None::<Signal>).is_err()) {
        return;
      }
      std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let remaining = self
      .groups
      .iter()
      .copied()
      .filter(|group| killpg(*group, None::<Signal>).is_ok())
      .map(Pid::as_raw)
      .collect::<Vec<_>>();
    if !remaining.is_empty() {
      let message = format!("cron test process guard could not reap process groups {remaining:?}");
      if std::thread::panicking() {
        eprintln!("{message}");
      } else {
        panic!("{message}");
      }
    }
  }
}

impl Default for ProcessGroupGuard {
  fn default() -> Self {
    Self::new()
  }
}

pub async fn manager(features: &FeatureContext, request_id: &str) -> anyhow::Result<SessionMetadata> {
  Ok(
    features
      .store
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: None,
          profile: TaskProfile::Manager,
          task: Some(TaskSpec { goal: "Synthetic cron manager".to_string(), ..TaskSpec::default() }),
          name: Some("Synthetic cron manager".to_string()),
          backend: Some("synthetic".to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::Operator,
        Map::new(),
      )
      .await?,
  )
}
