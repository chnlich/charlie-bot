//! The Claude child environment edits that the package registers reach a spawned process through the runner.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::gh_wrapper;
use charliebot_agents::runner::{AgentRunner, RunRequest};
use charliebot_agents::transport::cgroup::SessionCgroupConfig;
use charliebot_registry::Registry;
use charliebot_registry::backend::{AgentBackend, Env, LaunchArgs, Translate, TransportKind};
use serde_json::Value;

#[ignore = "integration"]
#[tokio::test]
async fn registered_claude_child_edits_apply_at_the_spawn_boundary() {
  let root = tempfile::tempdir().unwrap();
  let mut registry = Registry::new();
  registry.register_package("backends.claude_code", charliebot_claude_code::register).unwrap();
  let registry = Arc::new(registry);
  let inherited = Env::from([
    ("PATH".to_string(), "/bin".to_string()),
    ("CHARLIEBOT_SESSION_ID".to_string(), "stale-session".to_string()),
    ("CLAUDECODE".to_string(), "1".to_string()),
    ("CLAUDE_CODE_DISABLE_AUTO_MEMORY".to_string(), "0".to_string()),
  ]);
  let script = r#"printf '%s\n' '{"type":"result","session":"'"$CHARLIEBOT_SESSION_ID"'","claudecode":"'"$CLAUDECODE"'","auto_memory":"'"$CLAUDE_CODE_DISABLE_AUTO_MEMORY"'"}'"#;
  let master_event =
    run_one_shell(root.path(), "master", inherited.clone(), Arc::clone(&registry), Some("current-session"), script)
      .await;
  assert_eq!(master_event["session"], "current-session");
  assert_eq!(master_event["claudecode"], "");
  assert_eq!(master_event["auto_memory"], "1");

  let worker_event = run_one_shell(root.path(), "worker", inherited, registry, None, script).await;
  assert_eq!(worker_event["session"], "");
  assert_eq!(worker_event["claudecode"], "");
  assert_eq!(worker_event["auto_memory"], "1");
}

/// Add the agent `gh` wrapper to a spawn environment: the runner refuses a spawn whose PATH does not resolve it.
fn with_gh_wrapper(root: &Path, mut env: Env) -> Env {
  let wrapper_dir = root.join("gh-wrapper");
  fs::create_dir_all(&wrapper_dir).unwrap();
  let wrapper = wrapper_dir.join("gh");
  fs::write(&wrapper, "#!/bin/sh\n").unwrap();
  let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
  permissions.set_mode(0o755);
  fs::set_permissions(&wrapper, permissions).unwrap();
  let path = env.remove("PATH").unwrap_or_else(|| "/bin".to_string());
  env.insert("PATH".to_string(), format!("{}:{path}", wrapper_dir.display()));
  env.insert(gh_wrapper::WRAPPER_PATH_VAR.to_string(), wrapper.display().to_string());
  env
}

async fn run_one_shell(
  root: &Path,
  name: &str,
  env: Env,
  registry: Arc<Registry>,
  session_id: Option<&str>,
  script: &str,
) -> Value {
  let log_dir = root.join(name);
  let seen = Arc::new(Mutex::new(Vec::new()));
  let events = Arc::clone(&seen);
  let runner = AgentRunner::new(
    "shell",
    Box::new(ShellBackend::new(script.to_string())),
    LaunchArgs { log_dir: Some(log_dir), ..LaunchArgs::default() },
  );
  let mut request = RunRequest::new(
    "prompt".to_string(),
    root.to_path_buf(),
    AgentBaseEnvironment { values: with_gh_wrapper(root, env) },
    SessionCgroupConfig { base_dir: root.join("cgroups"), ..SessionCgroupConfig::disabled() },
    registry,
  );
  request.session_id = session_id.map(str::to_string);
  request.on_event = Arc::new(move |event| {
    let events = Arc::clone(&events);
    Box::pin(async move {
      events.lock().unwrap().push(event);
      Ok(())
    })
  });
  runner.run(request).await.unwrap();
  seen.lock().unwrap()[0].clone()
}

/// A raw-log backend whose child is an explicit `/bin/sh` process, so the test reads the environment a real spawn
/// receives.
struct ShellBackend {
  script: String,
}

impl ShellBackend {
  fn new(script: impl Into<String>) -> Self {
    Self { script: script.into() }
  }
}

impl AgentBackend for ShellBackend {
  fn instructions_target(&self) -> Option<(&'static str, &'static str)> {
    Some(("HARNESS.md", "harness_instructions_written"))
  }
  fn prepare_transport(&mut self, _: &Path) -> anyhow::Result<()> {
    Ok(())
  }
  fn build_command(&self, _: &str) -> anyhow::Result<Vec<String>> {
    Ok(vec!["/bin/sh".to_string(), "-c".to_string(), self.script.clone()])
  }
  fn prepare_env(&self, env: Env) -> anyhow::Result<Env> {
    Ok(env)
  }
  fn stdin_prompt(&self, _: &str) -> Option<String> {
    None
  }
  fn translator(&self) -> Box<dyn Translate> {
    struct Identity;
    impl Translate for Identity {
      fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
        Ok(vec![raw])
      }
    }
    Box::new(Identity)
  }
  fn transport(&self) -> TransportKind {
    TransportKind::RawLog
  }
}
