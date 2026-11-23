use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::backend_pool::BackendPool;
use charliebot_core::config::{
  BackendOption, CONFIG_FILENAME, ConfigSection, HostAuthConfig, PoolBilling, PoolConfig, PoolMember, PoolRule,
  SectionValue,
};
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
use serde_json::{Map, Value, json};
use serde_yaml::Mapping;

/// The id of the priority pool that the Auto-style tests of this crate name.
pub const AUTO_POOL_ID: &str = "auto";
/// The `cc-claude` option of the login-host fixtures. A keyed round runs only on a member kind that the improve launcher
/// accepts, so a keyed task's session must hold this option.
pub const KEYED_BACKEND_ID: &str = "claude";
/// The `gemini` option of the login-host fixtures, a type that no keyed round may run on.
pub const NON_KEYED_BACKEND_ID: &str = "gemini";
/// The `cc-openai-compatible` option of the login-host fixtures: a Claude Code member that reaches its model through this
/// server's proxy route, so no keyed round may run on it.
pub const PROXY_BACKEND_ID: &str = "proxy";

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

pub fn feature_context() -> (tempfile::TempDir, Arc<FeatureContext>) {
  feature_context_with_extra_backends(&[])
}

pub fn feature_context_with_extra_backends(extra_backend_ids: &[&str]) -> (tempfile::TempDir, Arc<FeatureContext>) {
  let entries: Vec<(&str, &str, Mapping)> = std::iter::once("synthetic")
    .chain(extra_backend_ids.iter().copied())
    .map(|id| (id, "synthetic", Mapping::new()))
    .collect();
  feature_context_with_entries(&entries, None, &[])
}

/// A feature context whose `backends.pools` holds the priority pool `auto` over `members`, each a synthetic backend
/// option in config order.
pub fn feature_context_with_auto(members: &[&str]) -> (tempfile::TempDir, Arc<FeatureContext>) {
  feature_context_with_auto_login_hosts(members, &[])
}

/// A feature context whose config names `hosts` as browser-login hosts (`host_auth.browser_logins[].host`), so that a
/// task's `improve_host` may name any of them.
/// The second option is `claude`, a `cc-claude` type: the member kind that a keyed task's session holds (see
/// [`keyed_manager`]). The third is `gemini`, a type no keyed round may run on. The fourth is [`PROXY_BACKEND_ID`].
pub fn feature_context_with_login_hosts(hosts: &[&str]) -> (tempfile::TempDir, Arc<FeatureContext>) {
  feature_context_with_entries(
    &[
      ("synthetic", "synthetic", Mapping::new()),
      (KEYED_BACKEND_ID, "cc-claude", Mapping::new()),
      (NON_KEYED_BACKEND_ID, "gemini", Mapping::new()),
      (PROXY_BACKEND_ID, "cc-openai-compatible", Mapping::new()),
    ],
    None,
    hosts,
  )
}

/// [`feature_context_with_auto`] with the browser-login hosts of [`feature_context_with_login_hosts`].
pub fn feature_context_with_auto_login_hosts(
  members: &[&str],
  hosts: &[&str],
) -> (tempfile::TempDir, Arc<FeatureContext>) {
  let entries: Vec<(&str, &str, Mapping)> = members.iter().map(|id| (*id, "synthetic", Mapping::new())).collect();
  feature_context_with_entries(&entries, Some(auto_pool(members)), hosts)
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

/// A feature context with one backend option per entry, each `(id, type fields)` of the synthetic type. The declared
/// rotation pool `pool` holds every entry, and the call classes name it. `auto` adds one more pool.
/// A feature context whose repo root is the checkout, so the session templates of the fires resolve.
fn feature_context_with_entries(
  entries: &[(&str, &str, Mapping)],
  auto: Option<PoolConfig>,
  login_hosts: &[&str],
) -> (tempfile::TempDir, Arc<FeatureContext>) {
  feature_context_at(entries, auto, login_hosts, repo_root())
}

/// A feature context whose repo root is `repo`, for a test that reads templates from a checkout of its own.
pub fn feature_context_with_repo_root(repo: &Path) -> (tempfile::TempDir, Arc<FeatureContext>) {
  feature_context_at(&[("synthetic", "synthetic", Mapping::new())], None, &[], repo.to_path_buf())
}

fn feature_context_at(
  entries: &[(&str, &str, Mapping)],
  auto: Option<PoolConfig>,
  login_hosts: &[&str],
  repo: PathBuf,
) -> (tempfile::TempDir, Arc<FeatureContext>) {
  let home = tempfile::tempdir().expect("temporary cron home");
  std::fs::create_dir_all(home.path().join("sessions")).expect("session directory provisions");
  std::fs::create_dir_all(home.path().join("config.d/cron.d")).expect("cron directory provisions");
  std::fs::create_dir_all(home.path().join("state")).expect("state directory provisions");
  if !login_hosts.is_empty() {
    std::fs::write(home.path().join(CONFIG_FILENAME), host_auth_config(login_hosts)).expect("config file writes");
  }
  let mut registry = Registry::new();
  registry.register_package("features.cron", crate::register).expect("cron registration");
  registry.register_package("test.cron_handlers", register_test_handler).expect("handler registration");
  registry.register_package("test.cron_backend", register_test_backend).expect("backend registration");
  registry.register_package("test.host_auth", register_host_auth_stub).expect("host auth registration");
  registry
    .register_package("features.model_pool", charliebot_agents::model_pool::register)
    .expect("auto backend registration");
  registry
    .register_package(charliebot_agents::backend_pool::PACKAGE, charliebot_agents::backend_pool::register)
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
  for (id, backend_type, type_fields) in entries {
    cfg.backends.options.push(BackendOption {
      id: id.to_string(),
      label: id.to_string(),
      prompt_overlay: None,
      // The members of the Auto fixtures are subscriptions; the other fixtures' options never reach a priority pool.
      billing: Some(PoolBilling::Subscription),
      model: format!("{id}-model"),
      backend_type: backend_type.to_string(),
      type_fields: type_fields.clone(),
      visible: true,
    });
  }
  let (rotation, calls) = rotation_pools(entries.iter().map(|(id, _, _)| id.to_string()).collect());
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
  std::fs::write(cfg.credentials_file(), "charliebot:\n  access_key: synthetic-access-key\n")
    .expect("credentials file writes");
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
    .insert(AgentBaseEnvironment { values: charliebot_agents::gh_wrapper::stub_environment(home.path(), "/usr/bin") })
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
  startup.insert(RepoRoot::new(Some(repo.into_os_string())).expect("repo root value")).expect("repo root inserts");
  let features = Arc::new(FeatureContext { config, credentials, store, registry, startup });
  (home, features)
}

pub fn repo_root() -> PathBuf {
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
  Ok(())
}

/// A `config.yaml` whose `host_auth.browser_logins` holds one login per host, each renewing that host.
fn host_auth_config(hosts: &[&str]) -> String {
  let logins: String = hosts
    .iter()
    .map(|host| {
      format!(
        "    - name: {host}\n      label: {host}\n      host: {host}\n      login_command: [synthetic-login]\n      \
         approval_url_pattern: synthetic\n      readback_command: [synthetic-readback]\n      input_prompt: \
         synthetic-code\n      lifetime_sec: 86400\n"
      )
    })
    .collect();
  format!("host_auth:\n  browser_logins:\n{logins}")
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
  registry.config_section(ConfigSection {
    key: "host_auth",
    legacy_keys: &[],
    parse: parse_host_auth,
    dump: dump_host_auth,
  })?;
  registry.host_logins(host_logins_stub)?;
  registry.host_probe(host_probe_stub)
}

/// The `host_auth` section as the cron loader reads it: its browser logins name the hosts a task may use.
fn parse_host_auth(raw: &serde_yaml::Value) -> Result<SectionValue, String> {
  let settings: HostAuthConfig = serde_yaml::from_value(raw.clone()).map_err(|error| format!("host_auth: {error}"))?;
  Ok(Arc::new(settings))
}

fn dump_host_auth(value: &SectionValue) -> Result<Value, String> {
  let settings = value.downcast_ref::<HostAuthConfig>().ok_or("the host_auth section holds an unexpected value")?;
  serde_json::to_value(settings).map_err(|error| error.to_string())
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
  task_manager(features, None, request_id, "synthetic").await
}

/// A manager session on the keyed backend, the shape a session of a task with `improve_host` holds in a login-host
/// fixture.
pub async fn keyed_manager(features: &FeatureContext, request_id: &str) -> anyhow::Result<SessionMetadata> {
  task_manager(features, None, request_id, KEYED_BACKEND_ID).await
}

/// A child manager session on the keyed backend under `parent_id`.
pub async fn keyed_child_manager(
  features: &FeatureContext,
  parent_id: &str,
  request_id: &str,
) -> anyhow::Result<SessionMetadata> {
  task_manager(features, Some(parent_id), request_id, KEYED_BACKEND_ID).await
}

/// A manager session under `parent_id`, the shape a task tree gives a child.
pub async fn child_manager(
  features: &FeatureContext,
  parent_id: &str,
  request_id: &str,
) -> anyhow::Result<SessionMetadata> {
  task_manager(features, Some(parent_id), request_id, "synthetic").await
}

/// One task session archived as the completion owner's archive writes it: one closed fact on the session.
pub async fn archive_task(features: &FeatureContext, session_id: &str) {
  let guard = features.store.control_lock().await;
  let event = charliebot_store::control_events::archived_close_event(
    session_id,
    "synthetic-cron-archive",
    "archived by the user",
    None,
    None,
    None,
    charliebot_store::control_events::ACTOR_USER,
    &[],
    &[],
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
  backend: &str,
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
            improve_host: None,
          }),
          name: Some("Synthetic cron manager".to_string()),
          backend: Some(backend.to_string()),
          group: None,
          session_id: None,
        },
        TaskCaller::Operator,
        Map::new(),
      )
      .await?,
  )
}

/// Write the cron.d host file of `task`, with its prompt in a prompt file that the file points to. The file holds the
/// task's fields as the loader reads them back.
pub fn write_task_file(home: &Path, task: &crate::config::ScheduledTaskConfig) -> PathBuf {
  let mut body = Map::new();
  body.insert("cron".to_string(), Value::String(task.cron.clone()));
  body.insert("timezone".to_string(), Value::String(task.timezone.clone()));
  body.insert("enabled".to_string(), Value::Bool(task.enabled));
  if let Some(session_id) = &task.session_id {
    body.insert("session_id".to_string(), Value::String(session_id.clone()));
  }
  if let Some(repo) = &task.repo {
    body.insert("repo".to_string(), Value::String(repo.clone()));
  }
  if let Some(host) = &task.improve_host {
    body.insert("improve_host".to_string(), Value::String(host.clone()));
  }
  if let Some(mode) = task.mode {
    let mode = match mode {
      crate::config::TaskMode::Master => "master",
      crate::config::TaskMode::Worker => "worker",
    };
    body.insert("mode".to_string(), Value::String(mode.to_string()));
  }
  let prompt = home.join(format!("{}-prompt.md", task.name));
  std::fs::write(&prompt, format!("{}\n", task.prompt.as_deref().unwrap_or_default())).unwrap();
  body.insert("prompt_file".to_string(), Value::String(prompt.display().to_string()));
  write_task(home, &task.name, &Value::Object(body))
}

/// Start a round of the task `task_name` in `session_id` through the tasks route, as a bound delegation whose note is
/// `note` (empty for none). Returns the HTTP status and the response body.
pub async fn bound_delegate(
  features: &Arc<FeatureContext>,
  session_id: &str,
  task_name: &str,
  note: &str,
) -> (axum::http::StatusCode, Value) {
  use axum::body::Body;
  use axum::http::{Method, Request, header};
  use tower::ServiceExt;
  let body = json!({
    "session_id": session_id,
    "description": note,
    (charliebot_tasks::routes::DELEGATE_BOUND_TASK_FIELD): task_name,
  })
  .to_string();
  let request = Request::builder()
    .method(Method::POST)
    .uri(charliebot_tasks::routes::DELEGATE_PATH)
    .header(header::AUTHORIZATION, "Bearer synthetic-access-key")
    .header(header::CONTENT_TYPE, "application/json")
    .body(Body::from(body))
    .unwrap();
  let router = charliebot_tasks::routes::routes(features);
  let response = router.oneshot(request).await.unwrap();
  let status = response.status();
  let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
  (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// The worker leaf and its work run that a bound delegation starts for `task`, whose file is written first. Panics
/// unless the delegation is accepted.
pub async fn bound_round_leaf(
  features: &Arc<FeatureContext>,
  task: &crate::config::ScheduledTaskConfig,
  note: &str,
) -> (SessionMetadata, charliebot_store::run_records::RunRecord) {
  let home = features.config.get().unwrap().charliebot_home.clone();
  write_task_file(&home, task);
  let session_id = task.session_id.as_deref().expect("a bound task names its session");
  let (status, body) = bound_delegate(features, session_id, &task.name, note).await;
  assert_eq!(status, axum::http::StatusCode::OK, "{body}");
  let leaf_id = body["session_id"].as_str().expect("the response names the worker").to_string();
  let leaf = features.store.get_session(&leaf_id).await.unwrap().expect("the worker exists");
  let run = features.store.list_run_records_sync(&leaf_id).unwrap().into_iter().next().expect("the worker has a run");
  (leaf, run)
}

/// Provision the task execution adapter on `features`: the access key the bound delegations read, the recovery step
/// that installs the adapter, and no dispatch executor, so a launched run stays queued in the test.
pub async fn install_execution(features: &Arc<FeatureContext>) {
  std::fs::write(
    features.config.get().unwrap().credentials_file(),
    "charliebot:\n  access_key: synthetic-access-key\n",
  )
  .unwrap();
  charliebot_tasks::execution::pre_recovery_step(Arc::clone(features)).await.unwrap();
  let dispatcher = features.startup.require::<charliebot_tasks::dispatch::Dispatcher>().unwrap();
  dispatcher.set_executor(None);
}
