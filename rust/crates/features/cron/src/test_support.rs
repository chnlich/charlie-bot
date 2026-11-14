use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::backend_pool::BackendPool;
use charliebot_claude_code::options::CLI_BINARY_KEY;
use charliebot_core::config::{BackendOption, PoolBilling, PoolConfig, PoolMember, PoolRule};
use charliebot_core::credentials::load_credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_core::repo_root::RepoRoot;
use charliebot_registry::backend::{
  AgentBackend, BackendFactory, BackendTraits, BackendTypeRegistration, Env, LaunchArgs, ResumeStyle, Translate,
  TransportKind,
};
use charliebot_registry::context::{FeatureContext, StartupState};
use charliebot_registry::registry::HostLogin;
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::metadata::{SessionMetadata, TaskApprover, TaskProfile, TaskSpec};
use charliebot_store::{CreateTask, Store, TaskCaller};
use charliebot_tasks::test_support::recovery_support::rotation_pools;
use futures_util::future::BoxFuture;
use serde_json::Map;
use serde_json::Value;
use serde_yaml::Mapping;

/// The id of the priority pool that the Auto-style tests of this crate name.
pub const AUTO_POOL_ID: &str = "auto";

/// The priority pool `auto` over `members`, each billed as a subscription, in config order.
fn auto_pool(members: &[&str]) -> PoolConfig {
  PoolConfig {
    id: AUTO_POOL_ID.to_string(),
    label: "Auto".to_string(),
    rule: PoolRule::Priority,
    members: members.iter().map(|id| PoolMember { backend: id.to_string() }).collect(),
    visible: true,
  }
}

/// An agent environment whose PATH resolves `gh` to a stub wrapper, as every agent spawn requires.
fn gh_wrapper_env(base: &Path, path: &str) -> Env {
  use std::os::unix::fs::PermissionsExt;
  let wrapper_dir = base.join("gh-wrapper");
  std::fs::create_dir_all(&wrapper_dir).unwrap();
  let wrapper = wrapper_dir.join("gh");
  std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();
  let mut permissions = std::fs::metadata(&wrapper).unwrap().permissions();
  permissions.set_mode(0o755);
  std::fs::set_permissions(&wrapper, permissions).unwrap();
  Env::from([
    ("PATH".to_string(), format!("{}:{path}", wrapper_dir.display())),
    (charliebot_agents::gh_wrapper::WRAPPER_PATH_VAR.to_string(), wrapper.display().to_string()),
  ])
}

pub fn feature_context() -> (tempfile::TempDir, Arc<FeatureContext>) {
  feature_context_with_extra_backends(&[])
}

pub fn feature_context_with_extra_backends(extra_backend_ids: &[&str]) -> (tempfile::TempDir, Arc<FeatureContext>) {
  let entries: Vec<(&str, Mapping)> =
    std::iter::once("synthetic").chain(extra_backend_ids.iter().copied()).map(|id| (id, Mapping::new())).collect();
  feature_context_with_entries(&entries, None)
}

/// A feature context whose `backends.pools` holds the priority pool `auto` over `members`, each a synthetic backend
/// option in config order.
pub fn feature_context_with_auto(members: &[&str]) -> (tempfile::TempDir, Arc<FeatureContext>) {
  let entries: Vec<(&str, Mapping)> = members.iter().map(|id| (*id, Mapping::new())).collect();
  feature_context_with_entries(&entries, Some(auto_pool(members)))
}

/// A manager session on `auto` that records no member yet, the shape of a bound session before its first turn start.
pub async fn auto_session(features: &FeatureContext, request_id: &str) -> anyhow::Result<SessionMetadata> {
  let session = features
    .store
    .create_task(
      CreateTask {
        request_id: request_id.to_string(),
        task_parent_id: None,
        profile: TaskProfile::Manager,
        task: None,
        name: Some("Synthetic Auto manager".to_string()),
        backend: Some(AUTO_POOL_ID.to_string()),
        group: None,
        session_id: None,
      },
      TaskCaller::Operator,
      Map::new(),
    )
    .await?;
  Ok(session)
}

/// A manager session on `auto` whose current member is `member`, the shape a turn start leaves.
pub async fn auto_manager(
  features: &FeatureContext,
  request_id: &str,
  member: &str,
) -> anyhow::Result<SessionMetadata> {
  let session = auto_session(features, request_id).await?;
  features
    .store
    .update_slot_fields(
      &session.id,
      charliebot_agents::model_pool::OWNER,
      Map::from_iter([(
        charliebot_agents::model_pool::POOL_MEMBER_FIELD.to_string(),
        Value::String(member.to_string()),
      )]),
    )
    .await?;
  Ok(session)
}

/// A feature context whose synthetic backend entries each log into a host. Every `(backend id, host alias)` pair is an
/// entry that names the host through `host_auth_alias`, as a `cc-anthropic-compatible` entry does.
pub fn feature_context_with_host_backends(hosts: &[(&str, &str)]) -> (tempfile::TempDir, Arc<FeatureContext>) {
  feature_context_with_entries(&host_entries(hosts), None)
}

/// A feature context whose extra synthetic backend entries each name a launcher through `cli_binary`, as a
/// `cc-anthropic-compatible` entry does. The plain `synthetic` entry stays, so the other fixtures' ids still resolve.
pub fn feature_context_with_launcher_backends(ids: &[&str]) -> (tempfile::TempDir, Arc<FeatureContext>) {
  let mut entries = vec![("synthetic", Mapping::new())];
  for id in ids {
    let mut fields = Mapping::new();
    fields.insert(CLI_BINARY_KEY.into(), "/path/to/launcher".into());
    entries.push((*id, fields));
  }
  feature_context_with_entries(&entries, None)
}

/// A feature context with the same host-naming entries as [`feature_context_with_host_backends`] and the priority pool
/// `auto` over `members`, each a configured option.
pub fn feature_context_with_auto_host_backends(
  hosts: &[(&str, &str)],
  members: &[&str],
) -> (tempfile::TempDir, Arc<FeatureContext>) {
  feature_context_with_entries(&host_entries(hosts), Some(auto_pool(members)))
}

fn host_entries<'a>(hosts: &'a [(&'a str, &'a str)]) -> Vec<(&'a str, Mapping)> {
  let mut entries = vec![("synthetic", Mapping::new())];
  for (id, alias) in hosts {
    let mut fields = Mapping::new();
    fields.insert("host_auth_alias".into(), (*alias).into());
    entries.push((*id, fields));
  }
  entries
}

/// A feature context with one backend option per entry, each `(id, type fields)` of the synthetic type. The declared
/// rotation pool `pool` holds every entry, and the call classes name it. `auto` adds one more pool.
fn feature_context_with_entries(
  entries: &[(&str, Mapping)],
  auto: Option<PoolConfig>,
) -> (tempfile::TempDir, Arc<FeatureContext>) {
  let home = tempfile::tempdir().expect("temporary cron home");
  std::fs::create_dir_all(home.path().join("sessions")).expect("session directory provisions");
  std::fs::create_dir_all(home.path().join("config.d/cron.d")).expect("cron directory provisions");
  std::fs::create_dir_all(home.path().join("state")).expect("state directory provisions");
  let mut registry = Registry::new();
  registry.register_package("features.cron", crate::register).expect("cron registration");
  registry.register_package("test.cron_handlers", register_test_handler).expect("handler registration");
  registry.register_package("test.cron_backend", register_test_backend).expect("backend registration");
  registry.register_package("test.host_auth", register_host_auth_stub).expect("host auth registration");
  registry
    .register_package("features.model_pool", charliebot_agents::model_pool::register)
    .expect("auto backend registration");
  registry
    .register_package("features.backend_pool", charliebot_agents::backend_pool::register)
    .expect("backend pool registration");
  let registry = Arc::new(registry);
  let mut cfg = registry.load_config(home.path()).expect("default config loads");
  cfg.paths.worktree_dir = home.path().join("worktrees").display().to_string();
  // The synthetic sessions have no metadata; a zero cap starts no session cgroup, so no spawn walks the task-parent
  // chain.
  cfg.server.session_memory_max_mb = 0;
  // A worker launch is withheld below `server.min_free_disk_gib`. The production default of 10 GiB can exceed a CI
  // runner's free disk, so this fixture pins the check off; the tasks crate tests the withhold itself.
  cfg.server.min_free_disk_gib = 0;
  for (id, type_fields) in entries {
    cfg.backends.options.push(BackendOption {
      id: id.to_string(),
      label: id.to_string(),
      prompt_overlay: None,
      // The members of the Auto fixtures are subscriptions; the other fixtures' options never reach a priority pool.
      billing: Some(PoolBilling::Subscription),
      model: format!("{id}-model"),
      backend_type: "synthetic".to_string(),
      type_fields: type_fields.clone(),
      visible: true,
    });
  }
  let (rotation, calls) = rotation_pools(entries.iter().map(|(id, _)| id.to_string()).collect());
  cfg.backends.pools = rotation;
  cfg.backends.calls = calls;
  cfg.backends.pools.extend(auto);
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
    .insert(AgentBaseEnvironment { values: gh_wrapper_env(home.path(), "/usr/bin") })
    .expect("agent base environment inserts");
  startup
    .insert(
      charliebot_agents::backend_availability::BackendAvailability::load(home.path())
        .expect("the availability owner loads"),
    )
    .expect("the owner inserts");
  startup.insert(charliebot_agents::model_pool::RotationPositions::default()).expect("the rotation positions insert");
  startup
    .insert(BackendPool::new(
      startup
        .require::<charliebot_agents::backend_availability::BackendAvailability>()
        .expect("the owner is in the startup state"),
      Arc::clone(&registry),
      startup.require::<charliebot_agents::model_pool::RotationPositions>().expect("the positions are in startup"),
    ))
    .expect("backend pool inserts");
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

/// The goal of a task whose rounds stay running until the test ends them. The synthetic backend holds a launch that
/// carries this text open with `sleep`; every other launch prints a result and exits.
pub const HELD_ROUND_GOAL: &str = "Synthetic held round";

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

  fn build_command(&self, prompt: &str) -> anyhow::Result<Vec<String>> {
    if prompt.contains(HELD_ROUND_GOAL) {
      return Ok(vec!["/bin/sh".to_string(), "-c".to_string(), "exec sleep 600".to_string()]);
    }
    Ok(vec![
      "/bin/sh".to_string(),
      "-c".to_string(),
      r#"printf '%s\n' '{"type":"result","subtype":"success","result":"synthetic migration completed"}'"#.to_string(),
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
  })?;
  registry.backend_host_alias("synthetic", synthetic_host_alias)?;
  registry.backend_cli_binary("synthetic", synthetic_cli_binary)
}

/// The synthetic entries name a host the way `cc-anthropic-compatible` entries do.
fn synthetic_host_alias(fields: &Mapping) -> Result<Option<String>, String> {
  Ok(fields.get("host_auth_alias").and_then(serde_yaml::Value::as_str).map(str::to_string))
}

/// The synthetic entries name a launcher the way `cc-anthropic-compatible` entries do.
fn synthetic_cli_binary(fields: &Mapping) -> Result<Option<String>, String> {
  Ok(fields.get(CLI_BINARY_KEY).and_then(serde_yaml::Value::as_str).map(str::to_string))
}

/// The host verdicts the stub host readers serve, each with the profile home it belongs to, and each probe the stub
/// ran. Tests set a verdict per home and alias, so parallel tests never share one.
static HOST_VERDICTS: Mutex<Vec<(PathBuf, HostLogin)>> = Mutex::new(Vec::new());
static HOST_PROBES: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());

/// One home's stub-host verdict for `login.alias`, replacing the verdict an earlier call set.
pub fn set_host_login(home: &Path, login: HostLogin) {
  let mut verdicts = HOST_VERDICTS.lock().expect("the verdict list is not poisoned");
  verdicts.retain(|(owner, existing)| !(owner == home && existing.alias == login.alias));
  verdicts.push((home.to_path_buf(), login));
}

/// How many times the stub probed `alias` for the profile home.
pub fn host_probe_count(home: &Path, alias: &str) -> usize {
  HOST_PROBES
    .lock()
    .expect("the probe log is not poisoned")
    .iter()
    .filter(|(owner, probed)| owner == home && probed == alias)
    .count()
}

fn register_host_auth_stub(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.host_logins(host_logins_stub)?;
  registry.host_probe(host_probe_stub)
}

fn host_logins_stub(features: &FeatureContext) -> anyhow::Result<Vec<HostLogin>> {
  let home = features.config.get()?.charliebot_home.clone();
  let verdicts = HOST_VERDICTS.lock().expect("the verdict list is not poisoned");
  Ok(verdicts.iter().filter(|(owner, _)| *owner == home).map(|(_, login)| login.clone()).collect())
}

/// The stub's probe answers with the verdict set for the alias. An alias with no verdict is an error, as an alias the
/// ssh config does not name is for the real probe.
fn host_probe_stub<'a>(features: &'a FeatureContext, alias: &'a str) -> BoxFuture<'a, anyhow::Result<HostLogin>> {
  Box::pin(async move {
    let home = features.config.get()?.charliebot_home.clone();
    HOST_PROBES.lock().expect("the probe log is not poisoned").push((home.clone(), alias.to_string()));
    let verdicts = HOST_VERDICTS.lock().expect("the verdict list is not poisoned");
    verdicts
      .iter()
      .find(|(owner, login)| *owner == home && login.alias == alias)
      .map(|(_, login)| login.clone())
      .ok_or_else(|| anyhow::anyhow!("no verdict is set for host {alias}"))
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

/// The commit SHA the fake origin's `main` points at.
const FAKE_TIP: &str = "1111111111111111111111111111111111111111";

/// The lock that serializes the tests depending on the tasks crate's git program override, which is process-global.
/// Those are the tests that install a [`FakeGit`] and each test that launches a repo-backed run, whose git calls
/// must reach real git.
pub static GIT_PROGRAM_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A `git` stand-in for the repo-backed cron tests. A `/bin/sh` script records each call's argv. It answers the
/// calls `prepare_worktree` makes for origin's `main`, plus the delivery cleanup that removes the worktree. Every
/// other call fails, so the test reports it. The tasks crate's `test-hooks` program override routes the module's
/// git spawns through the script.
pub struct FakeGit {
  dir: tempfile::TempDir,
}

impl FakeGit {
  /// The installed fake, kept until the returned value drops; the call log starts empty.
  pub fn install() -> Self {
    let dir = tempfile::tempdir().expect("fake git directory is available");
    let calls = dir.path().join("calls");
    std::fs::write(&calls, "").expect("fake git call log is written");
    let script = dir.path().join("fake_git.sh");
    std::fs::write(&script, fake_git_script(&calls)).expect("fake git script is written");
    charliebot_tasks::git::set_git_program_override(
      "/bin/sh",
      &[script.to_str().expect("fake git script path is UTF-8")],
    );
    Self { dir }
  }

  /// The argv of each intercepted call, space-joined, in call order.
  pub fn calls(&self) -> Vec<String> {
    let log = std::fs::read_to_string(self.dir.path().join("calls")).expect("fake git call log is readable");
    log.lines().map(str::to_string).collect()
  }
}

impl Drop for FakeGit {
  fn drop(&mut self) {
    charliebot_tasks::git::clear_git_program_override();
  }
}

fn fake_git_script(calls: &Path) -> String {
  format!(
    r#"#!/bin/sh
echo "$*" >> '{calls}'
case "$*" in
  "-c protocol.version=0 ls-remote --symref origin")
    printf 'ref: refs/heads/main\tHEAD\n{tip}\tHEAD\n{tip}\trefs/heads/main\n' ;;
  "rev-parse --verify --quiet refs/remotes/origin/main")
    echo '{tip}' ;;
  "worktree add -b "*)
    mkdir -p "$5" ;;
  "worktree remove --force "*)
    rm -rf "$4" ;;
  "worktree prune")
    : ;;
  *)
    echo "fake git: unexpected call: $*" >&2
    exit 97 ;;
esac
"#,
    calls = calls.display(),
    tip = FAKE_TIP,
  )
}

pub async fn manager(features: &FeatureContext, request_id: &str) -> anyhow::Result<SessionMetadata> {
  task_manager(features, None, request_id).await
}

/// A manager session under `parent_id`, the shape a task tree gives a child.
pub async fn child_manager(
  features: &FeatureContext,
  parent_id: &str,
  request_id: &str,
) -> anyhow::Result<SessionMetadata> {
  task_manager(features, Some(parent_id), request_id).await
}

/// One task session archived as the completion owner's archive writes it: one closed fact on the session.
pub async fn archive_task(features: &FeatureContext, session_id: &str) {
  let guard = features.store.control_lock().await;
  let event = charliebot_store::control_events::build_control_event(
    charliebot_core::event_types::TASK_CLOSED,
    "user",
    session_id,
    None,
    None,
    [
      ("request_id".to_string(), serde_json::json!("synthetic-cron-archive")),
      ("outcome".to_string(), serde_json::json!("archived")),
      ("summary".to_string(), serde_json::json!("archived by the user")),
      ("result_refs".to_string(), serde_json::json!([])),
      ("run_ids".to_string(), serde_json::json!([])),
      ("report_to".to_string(), Value::Null),
    ],
  );
  features.store.append_control_fact(&guard, session_id, event).await.expect("the archive fact appends");
  drop(guard);
  features.store.invalidate_tree_index();
}

/// The path of one cron task file written under the home's `config.d/cron.d`, where the task loader reads it.
pub fn write_task(home: &Path, name: &str, body: &Value) -> PathBuf {
  let path = home.join("config.d/cron.d").join(format!("{name}.yaml"));
  std::fs::create_dir_all(path.parent().unwrap()).unwrap();
  std::fs::write(&path, crate::yaml::dump(body).unwrap()).unwrap();
  path
}

/// The path of one worker task whose prompt is `HELD_ROUND_GOAL`, written to a prompt file the task points to
/// through `prompt_file`. A cron.d host file may not carry an inline `prompt`, so the loader would isolate it
/// as broken.
pub fn write_prompted_task(home: &Path, name: &str, body: &Value) -> PathBuf {
  let prompt = home.join(format!("{name}-prompt.md"));
  std::fs::write(&prompt, format!("{HELD_ROUND_GOAL}\n")).unwrap();
  let mut body = body.clone();
  body["prompt_file"] = Value::String(prompt.display().to_string());
  write_task(home, name, &body)
}

async fn task_manager(
  features: &FeatureContext,
  parent_id: Option<&str>,
  request_id: &str,
) -> anyhow::Result<SessionMetadata> {
  Ok(
    features
      .store
      .create_task(
        CreateTask {
          request_id: request_id.to_string(),
          task_parent_id: parent_id.map(str::to_string),
          profile: TaskProfile::Manager,
          task: Some(TaskSpec {
            goal: "Synthetic cron manager".to_string(),
            repo_path: None,
            base_branch: None,
            task_type: None,
            keep_worktree: false,
            cold_read: None,
            approver: TaskApprover::User,
          }),
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
