//! The environment inherited by every agent process.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, bail};
use charliebot_core::config::ServerConfig;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::backend::Env;
use charliebot_registry::context::{FeatureContext, StartupState, StartupStop};
use futures_util::future::BoxFuture;

use crate::gh_wrapper;
use crate::transport::cgroup;

const ACTIVATION_VARIABLES: [&str; 2] = ["VIRTUAL_ENV", "UV_RUN_RECURSION_DEPTH"];
/// GitHub credentials and the token command the server process may carry. The wrapper owns the App token when the
/// live host config (`spawn_base_environment`) names a command; with no command it uses the host's stored gh login.
/// Neither an inherited `GH_TOKEN` nor a stale command reaches an agent. The server's own environment and its
/// non-agent children keep both.
const GITHUB_ENVIRONMENT_VARIABLES: [&str; 3] = ["GH_TOKEN", "GITHUB_TOKEN", gh_wrapper::TOKEN_COMMAND_VAR];

/// The server environment after removing its virtualenv activation for agent children.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentBaseEnvironment {
  pub values: Env,
}

/// Rewrite a supplied server environment with the agent `gh` wrapper and the running server's directory first on
/// agent PATH.
pub fn agent_environment(
  inherited: &Env,
  server_bin_dir: &Path,
  gh_wrapper_dir: &Path,
) -> anyhow::Result<AgentBaseEnvironment> {
  require_agent_cli(server_bin_dir)?;
  gh_wrapper::require_installed(gh_wrapper_dir)?;
  let Some(path) = inherited.get("PATH") else {
    bail!("PATH is not set in the server environment; the agent environment places the server binary directory on it");
  };
  let mut values = inherited.clone();
  for name in ACTIVATION_VARIABLES.into_iter().chain(GITHUB_ENVIRONMENT_VARIABLES) {
    values.remove(name);
  }
  let mut rewritten_entries = vec![gh_wrapper_dir.to_path_buf(), server_bin_dir.to_path_buf()];
  rewritten_entries.extend(env::split_paths(path).filter(|entry| entry != server_bin_dir && entry != gh_wrapper_dir));
  let rewritten_path = env::join_paths(&rewritten_entries)
    .context("the agent PATH entries join with the path separator")?
    .into_string()
    .map_err(|_| anyhow::anyhow!("the agent PATH is not valid Unicode"))?;
  values.insert("PATH".to_string(), rewritten_path);
  values.insert(gh_wrapper::WRAPPER_PATH_VAR.to_string(), gh_wrapper_dir.join("gh").to_string_lossy().into_owned());
  // Chrome asks systemd over the session bus to move itself into its own scope. With the bus disabled it cannot, so a
  // browser stays in the cgroup of the process that started it. An agent's browser stays in the session cgroup, and
  // the server's renderer stays in the server's cgroup. On systemd 255, `systemctl --user` honors
  // `DBUS_SESSION_BUS_ADDRESS=disabled:`, fails to connect, and does not fall back to `$XDG_RUNTIME_DIR/systemd/private`.
  values.insert("DBUS_SESSION_BUS_ADDRESS".to_string(), "disabled:".to_string());
  values.entry("PYDANTIC_DISABLE_PLUGINS".to_string()).or_insert_with(|| "__all__".to_string());
  Ok(AgentBaseEnvironment { values })
}

fn require_agent_cli(server_bin_dir: &Path) -> anyhow::Result<()> {
  let cli_path = server_bin_dir.join("charliebot");
  let metadata = fs::metadata(&cli_path).with_context(|| format!("agent CLI must exist at {}", cli_path.display()))?;
  if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
    bail!("agent CLI must be an executable file at {}", cli_path.display());
  }
  Ok(())
}

/// Build the agent environment from the server's process environment without changing that environment.
///
/// Only an agent spawn receives this value. The server's own environment keeps the operator's PATH and GitHub
/// credentials, and so does every child that is not an agent run: the web terminal's tmux shell resolves `gh` to the
/// real gh as the operator's stored login, and the usage panel's `claude --version` probe inherits the same.
///
/// The startup value carries no App token command: [`spawn_base_environment`] adds the command the live config holds
/// when it builds each spawn's environment.
pub fn agent_base_environment(server_bin_dir: &Path, gh_wrapper_dir: &Path) -> anyhow::Result<AgentBaseEnvironment> {
  agent_environment(&process_environment()?, server_bin_dir, gh_wrapper_dir)
}

/// The agent environment of one spawn: the startup environment plus the App token command the live config holds.
///
/// The token command is read from the hot-reloaded config handle per spawn and written by
/// [`gh_wrapper::apply_token_command`], so `github.app_token_command` added, changed or removed after startup reaches
/// the next agent process without a server restart. A config the spawn cannot read fails the spawn, the way the other
/// spawn-time config reads do.
pub fn spawn_base_environment(features: &FeatureContext) -> anyhow::Result<AgentBaseEnvironment> {
  spawn_base_environment_from(&features.startup, &features.config)
}

fn spawn_base_environment_from(
  startup: &StartupState,
  config: &HotReload<charliebot_core::config::CharlieBotConfig>,
) -> anyhow::Result<AgentBaseEnvironment> {
  let mut base = startup.require::<AgentBaseEnvironment>()?.as_ref().clone();
  let cfg = config.get()?;
  gh_wrapper::apply_token_command(&mut base.values, cfg.github.app_token_command.as_deref());
  Ok(base)
}

/// Log the cgroup startup line and sweep the empty session cgroups of `sessions_dir` under an injectable base. Negative
/// limits are logged as they are; each spawn applies them as well.
pub fn run_session_cgroup_startup(base_dir: &Path, sessions_dir: &Path, server: &ServerConfig) -> anyhow::Result<()> {
  run_session_cgroup_startup_inner(base_dir, sessions_dir, server, None)
}

fn run_session_cgroup_startup_inner(
  base_dir: &Path,
  sessions_dir: &Path,
  server: &ServerConfig,
  stop: Option<&StartupStop>,
) -> anyhow::Result<()> {
  cgroup::log_session_cgroup_startup(
    base_dir,
    server.session_memory_max_mb,
    server.session_swap_max_mb,
    server.session_tasks_max,
    server.enable_session_cpu_weight,
    server.enable_session_io_weight,
  );
  if let Some(stop) = stop {
    cgroup::sweep_stale_session_cgroups_until_stopped(base_dir, sessions_dir, stop)?;
  } else {
    cgroup::sweep_stale_session_cgroups(base_dir, sessions_dir)?;
  }
  Ok(())
}

/// Log cgroup availability and remove this home's empty stale session cgroups before recovery.
pub fn session_cgroup_step(features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
  Box::pin(async move {
    let cfg = features.config.get()?;
    let stop = features.startup.stop();
    run_session_cgroup_startup_inner(
      &charliebot_core::cgroup::system_cgroup_base_dir(),
      &cfg.sessions_dir(),
      &cfg.server,
      Some(&stop),
    )?;
    Ok(())
  })
}

fn process_environment() -> anyhow::Result<Env> {
  let mut values = BTreeMap::new();
  for (name, value) in env::vars_os() {
    let name = name.into_string().map_err(|_| anyhow::anyhow!("the server environment has a non-Unicode name"))?;
    let value =
      value.into_string().map_err(|_| anyhow::anyhow!("the server environment variable {name} is not Unicode"))?;
    values.insert(name, value);
  }
  Ok(values)
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::PathBuf;
  use std::process::Command;

  use super::*;

  fn server_bin_dir_under(root: &Path) -> PathBuf {
    let directory = root.join("release");
    fs::create_dir_all(&directory).unwrap();
    let cli = directory.join("charliebot");
    fs::write(&cli, "test fixture; this file is not run\n").unwrap();
    let mut permissions = fs::metadata(&cli).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&cli, permissions).unwrap();
    directory
  }

  /// A wrapper directory with an executable stub script, as `require_installed` expects it.
  fn gh_wrapper_dir_under(root: &Path) -> PathBuf {
    let directory = root.join("gh-wrapper");
    fs::create_dir_all(&directory).unwrap();
    let script = directory.join("gh");
    fs::write(&script, "test fixture; this file is not run\n").unwrap();
    let mut permissions = fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).unwrap();
    directory
  }

  #[test]
  fn places_the_gh_wrapper_and_server_binary_dir_first_and_removes_activation() {
    let root = tempfile::tempdir().unwrap();
    let venv_bin = root.path().join("venv/bin");
    fs::create_dir_all(&venv_bin).unwrap();
    let server_bin_dir = server_bin_dir_under(root.path());
    let gh_wrapper_dir = gh_wrapper_dir_under(root.path());
    std::os::unix::fs::symlink(root.path().join("venv"), root.path().join("venv-link")).unwrap();
    let tools = root.path().join("tools");
    let system = root.path().join("system");
    let missing = root.path().join("missing/bin");
    let inherited = Env::from([
      ("VIRTUAL_ENV".to_string(), root.path().join("venv").display().to_string()),
      ("UV_RUN_RECURSION_DEPTH".to_string(), "1".to_string()),
      (
        "PATH".to_string(),
        [
          venv_bin.display().to_string(),
          tools.display().to_string(),
          root.path().join("venv-link/bin").display().to_string(),
          missing.display().to_string(),
          system.display().to_string(),
        ]
        .join(":"),
      ),
      ("GH_TOKEN".to_string(), "inherited-personal-token".to_string()),
      ("GITHUB_TOKEN".to_string(), "inherited-personal-token".to_string()),
      (gh_wrapper::TOKEN_COMMAND_VAR.to_string(), "/synthetic/stale-token-command".to_string()),
      ("KEPT".to_string(), "value".to_string()),
    ]);

    let rewritten = agent_environment(&inherited, &server_bin_dir, &gh_wrapper_dir).unwrap();

    assert!(!rewritten.values.contains_key("VIRTUAL_ENV"));
    assert!(!rewritten.values.contains_key("UV_RUN_RECURSION_DEPTH"));
    assert!(!rewritten.values.contains_key("GH_TOKEN"));
    assert!(!rewritten.values.contains_key("GITHUB_TOKEN"));
    assert!(!rewritten.values.contains_key(gh_wrapper::TOKEN_COMMAND_VAR));
    let path_entries: Vec<_> = env::split_paths(std::ffi::OsStr::new(&rewritten.values["PATH"])).collect();
    assert_eq!(path_entries[0], gh_wrapper_dir);
    assert_eq!(path_entries[1], server_bin_dir);
    assert_eq!(
      path_entries,
      [
        gh_wrapper_dir.clone(),
        server_bin_dir.clone(),
        venv_bin,
        tools,
        root.path().join("venv-link/bin"),
        missing,
        system
      ]
    );
    assert_eq!(rewritten.values[gh_wrapper::WRAPPER_PATH_VAR], gh_wrapper_dir.join("gh").display().to_string());
    assert_eq!(rewritten.values["PYDANTIC_DISABLE_PLUGINS"], "__all__");
    assert_eq!(rewritten.values["KEPT"], "value");
    assert!(inherited.contains_key("VIRTUAL_ENV"));
  }

  /// The agent environment disables the session bus even when the server inherited a live one, so a browser stays in
  /// the cgroup of the process that started it.
  #[test]
  fn the_agent_environment_disables_the_session_bus() {
    let root = tempfile::tempdir().unwrap();
    let server_bin_dir = server_bin_dir_under(root.path());
    let gh_wrapper_dir = gh_wrapper_dir_under(root.path());
    let inherited = Env::from([
      ("PATH".to_string(), "/usr/bin".to_string()),
      ("DBUS_SESSION_BUS_ADDRESS".to_string(), "unix:path=/run/user/1001/bus".to_string()),
    ]);

    let rewritten = agent_environment(&inherited, &server_bin_dir, &gh_wrapper_dir).unwrap();

    assert_eq!(rewritten.values["DBUS_SESSION_BUS_ADDRESS"], "disabled:");
  }

  #[test]
  fn moves_existing_server_dir_to_the_front_and_keeps_pydantic_values() {
    let root = tempfile::tempdir().unwrap();
    let server_bin_dir = server_bin_dir_under(root.path());
    let gh_wrapper_dir = gh_wrapper_dir_under(root.path());
    let inherited = Env::from([
      ("PATH".to_string(), format!("/usr/bin:{}", server_bin_dir.display())),
      ("PYDANTIC_DISABLE_PLUGINS".to_string(), "custom".to_string()),
    ]);

    let rewritten = agent_environment(&inherited, &server_bin_dir, &gh_wrapper_dir).unwrap();

    let entries: Vec<_> = env::split_paths(std::ffi::OsStr::new(&rewritten.values["PATH"])).collect();
    assert_eq!(entries, [gh_wrapper_dir, server_bin_dir, PathBuf::from("/usr/bin")]);
    assert_eq!(rewritten.values["PYDANTIC_DISABLE_PLUGINS"], "custom");
  }

  #[test]
  fn missing_agent_cli_fails_with_the_expected_path() {
    let root = tempfile::tempdir().unwrap();
    let server_bin_dir = root.path().join("release");
    fs::create_dir_all(&server_bin_dir).unwrap();
    let gh_wrapper_dir = gh_wrapper_dir_under(root.path());
    let inherited = Env::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
    let expected_path = server_bin_dir.join("charliebot");

    let error = agent_environment(&inherited, &server_bin_dir, &gh_wrapper_dir).unwrap_err();

    assert!(format!("{error:#}").contains(&expected_path.display().to_string()), "{error:#}");

    fs::write(&expected_path, "test fixture; this file is not run\n").unwrap();
    let mut permissions = fs::metadata(&expected_path).unwrap().permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(&expected_path, permissions).unwrap();
    let error = agent_environment(&inherited, &server_bin_dir, &gh_wrapper_dir).unwrap_err();

    assert!(format!("{error:#}").contains(&expected_path.display().to_string()), "{error:#}");
    assert!(format!("{error:#}").contains("executable file"), "{error:#}");
  }

  #[test]
  fn missing_gh_wrapper_fails_with_the_expected_path() {
    let root = tempfile::tempdir().unwrap();
    let server_bin_dir = server_bin_dir_under(root.path());
    let gh_wrapper_dir = root.path().join("gh-wrapper");
    fs::create_dir(&gh_wrapper_dir).unwrap();
    let inherited = Env::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
    let expected_path = gh_wrapper_dir.join("gh");

    let error = agent_environment(&inherited, &server_bin_dir, &gh_wrapper_dir).unwrap_err();

    assert!(format!("{error:#}").contains(&expected_path.display().to_string()), "{error:#}");

    fs::write(&expected_path, "test fixture; this file is not run\n").unwrap();
    let mut permissions = fs::metadata(&expected_path).unwrap().permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(&expected_path, permissions).unwrap();
    let error = agent_environment(&inherited, &server_bin_dir, &gh_wrapper_dir).unwrap_err();

    assert!(format!("{error:#}").contains(&expected_path.display().to_string()), "{error:#}");
    assert!(format!("{error:#}").contains("executable file"), "{error:#}");
  }

  #[test]
  fn missing_path_is_an_error_that_names_path() {
    let root = tempfile::tempdir().unwrap();
    let server_bin_dir = server_bin_dir_under(root.path());
    let gh_wrapper_dir = gh_wrapper_dir_under(root.path());
    let inherited = Env::from([("KEPT".to_string(), "value".to_string())]);

    let error = agent_environment(&inherited, &server_bin_dir, &gh_wrapper_dir).unwrap_err();

    assert!(error.to_string().contains("PATH"), "{error:#}");
  }

  #[test]
  fn the_config_token_command_reaches_the_agent_environment() {
    let root = tempfile::tempdir().unwrap();
    let server_bin_dir = server_bin_dir_under(root.path());
    let gh_wrapper_dir = gh_wrapper_dir_under(root.path());
    let inherited = Env::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
    let mut agent = agent_environment(&inherited, &server_bin_dir, &gh_wrapper_dir).unwrap();
    let home = tempfile::tempdir().unwrap();
    fs::write(
      home.path().join(charliebot_core::config::CONFIG_FILENAME),
      "github:\n  app_token_command: ~/synthetic/app-token-command\n",
    )
    .unwrap();
    let cfg = charliebot_core::config::load_config(home.path(), &Default::default()).unwrap();
    assert!(!agent.values.contains_key(gh_wrapper::TOKEN_COMMAND_VAR));

    gh_wrapper::apply_token_command(&mut agent.values, cfg.github.app_token_command.as_deref());

    assert_eq!(
      agent.values[gh_wrapper::TOKEN_COMMAND_VAR],
      charliebot_core::paths::expanduser("~/synthetic/app-token-command")
    );
  }

  /// One hot-reloaded config file read at three spawns: key absent, then added, then changed — no server restart
  /// between them. The startup environment carries no token command; each spawn reads the live config.
  #[test]
  fn spawn_base_environment_reads_the_live_config_token_command_at_each_spawn() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir(&home).unwrap();
    let config_path = home.join(charliebot_core::config::CONFIG_FILENAME);
    fs::write(&config_path, "").unwrap();
    let registry = Arc::new(charliebot_registry::Registry::new());
    let config_home = home.clone();
    let config_registry = Arc::clone(&registry);
    let config =
      HotReload::new(config_path, "config_reload_failed", Box::new(move || config_registry.load_config(&config_home)));
    let startup = StartupState::default();
    startup.insert(AgentBaseEnvironment { values: Env::default() }).expect("the agent base environment inserts");

    let absent = spawn_base_environment_from(&startup, &config).unwrap();
    assert!(!absent.values.contains_key(gh_wrapper::TOKEN_COMMAND_VAR));

    fs::write(
      home.join(charliebot_core::config::CONFIG_FILENAME),
      "github:\n  app_token_command: ~/synthetic/app-token-command\n",
    )
    .unwrap();
    let added = spawn_base_environment_from(&startup, &config).unwrap();
    assert_eq!(
      added.values[gh_wrapper::TOKEN_COMMAND_VAR],
      charliebot_core::paths::expanduser("~/synthetic/app-token-command")
    );

    fs::write(
      home.join(charliebot_core::config::CONFIG_FILENAME),
      "github:\n  app_token_command: ~/synthetic/other-token-command\n",
    )
    .unwrap();
    let changed = spawn_base_environment_from(&startup, &config).unwrap();
    assert_eq!(
      changed.values[gh_wrapper::TOKEN_COMMAND_VAR],
      charliebot_core::paths::expanduser("~/synthetic/other-token-command")
    );
  }

  const STARTUP_TEST: &str = "agent_environment::tests::server_children_keep_the_operator_environment";
  /// Set only in the child process: the file the child writes the grandchild's environment to.
  const CHILD_OUTPUT_ENV: &str = "CHARLIEBOT_STARTUP_ENV_OUTPUT";
  const SERVER_BIN_DIR_ENV: &str = "CHARLIEBOT_SERVER_BIN_DIR";

  #[test]
  #[ignore = "integration"]
  fn server_children_keep_the_operator_environment() {
    match env::var_os(CHILD_OUTPUT_ENV) {
      Some(output) => startup_child(Path::new(&output)),
      None => startup_parent(),
    }
  }

  /// Run this test again in a child process with the environment the server starts with, then read both the
  /// environment the child's non-agent grandchild inherited and the agent environment the server builds.
  fn startup_parent() {
    let root = tempfile::tempdir().unwrap();
    let venv_bin = root.path().join("venv/bin");
    fs::create_dir_all(&venv_bin).unwrap();
    let server_bin_dir = server_bin_dir_under(root.path());
    let gh_wrapper_dir = gh_wrapper_dir_under(root.path());
    let output = root.path().join("grandchild.env");
    let path = [venv_bin.display().to_string(), "/usr/bin".to_string(), "/bin".to_string()].join(":");

    let status = Command::new(env::current_exe().unwrap())
      .args(["--exact", STARTUP_TEST, "--ignored"])
      .env_clear()
      .env("PATH", &path)
      .env("VIRTUAL_ENV", root.path().join("venv"))
      .env("UV_RUN_RECURSION_DEPTH", "1")
      .env("GH_TOKEN", "operator-token")
      .env(SERVER_BIN_DIR_ENV, &server_bin_dir)
      .env("CHARLIEBOT_TEST_GH_WRAPPER_DIR", &gh_wrapper_dir)
      .env(CHILD_OUTPUT_ENV, &output)
      .status()
      .unwrap();
    assert!(status.success(), "the startup child failed: {status}");

    // The server's own environment and its non-agent children keep the operator's values: gh resolves to the
    // operator's gh, and the wrapper's PATH entry and variables are absent.
    let grandchild = fs::read_to_string(&output).unwrap();
    let vars: BTreeMap<&str, &str> = grandchild.lines().filter_map(|line| line.split_once('=')).collect();
    assert_eq!(vars["PATH"], path);
    assert_eq!(vars["VIRTUAL_ENV"], root.path().join("venv").to_str().unwrap());
    assert_eq!(vars["UV_RUN_RECURSION_DEPTH"], "1");
    assert_eq!(vars["GH_TOKEN"], "operator-token");
    assert!(!vars.contains_key(gh_wrapper::WRAPPER_PATH_VAR));
    assert!(!vars.contains_key(gh_wrapper::TOKEN_COMMAND_VAR));

    // The agent environment built from the same process environment is the wrapper one.
    let agent = fs::read_to_string(output.with_extension("agent")).unwrap();
    let agent_vars: BTreeMap<&str, &str> = agent.lines().filter_map(|line| line.split_once('=')).collect();
    let entries: Vec<&str> = agent_vars["PATH"].split(':').collect();
    assert_eq!(entries[0], gh_wrapper_dir.to_str().unwrap());
    assert_eq!(entries[1], server_bin_dir.to_str().unwrap());
    assert!(entries.contains(&venv_bin.to_str().unwrap()), "the venv bin stays on PATH: {entries:?}");
    assert!(!agent_vars.contains_key("VIRTUAL_ENV"));
    assert!(!agent_vars.contains_key("UV_RUN_RECURSION_DEPTH"));
    assert!(!agent_vars.contains_key("GH_TOKEN"));
    assert_eq!(agent_vars[gh_wrapper::WRAPPER_PATH_VAR], gh_wrapper_dir.join("gh").to_str().unwrap());
    assert_eq!(agent_vars["PYDANTIC_DISABLE_PLUGINS"], "__all__");
    assert_eq!(agent_vars["DBUS_SESSION_BUS_ADDRESS"], "disabled:");
  }

  /// The startup step as `main` runs it, the grandchild that inherits the unchanged process environment, and the
  /// agent values the parent checks.
  fn startup_child(output: &Path) {
    let server_bin_dir = PathBuf::from(env::var_os(SERVER_BIN_DIR_ENV).unwrap());
    let gh_wrapper_dir = PathBuf::from(env::var_os("CHARLIEBOT_TEST_GH_WRAPPER_DIR").unwrap());

    let agent = agent_base_environment(&server_bin_dir, &gh_wrapper_dir).unwrap();

    let grandchild = Command::new("/usr/bin/env").output().unwrap();
    fs::write(output, grandchild.stdout).unwrap();
    let values: String = agent.values.iter().map(|(name, value)| format!("{name}={value}\n")).collect();
    fs::write(output.with_extension("agent"), values).unwrap();
  }

  #[test]
  fn runtime_cgroup_startup_sweeps_the_injected_base_at_zero_and_negative_limits() {
    // Each of these starts and sweeps the same way, so none of them refuses the start.
    for (memory, swap, tasks) in [(0, 0, 4096), (-1, 0, 4096), (12288, -1, 4096), (12288, 0, -1)] {
      let base = tempfile::tempdir().unwrap();
      let home = tempfile::tempdir().unwrap();
      let sessions = home.path().join("sessions");
      fs::create_dir_all(sessions.join("stale")).unwrap();
      fs::create_dir_all(sessions.join("retained")).unwrap();
      let slice = charliebot_core::cgroup::agents_slice_dir(base.path());
      fs::create_dir_all(&slice).unwrap();
      let stale = slice.join("charliebot-sess-stale");
      fs::create_dir(&stale).unwrap();
      let retained = slice.join("charliebot-sess-retained");
      fs::create_dir(&retained).unwrap();
      fs::write(retained.join("cgroup.procs"), "123").unwrap();
      let server = ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 18498,
        subprocess_buffer_limit_mb: 1024,
        session_memory_max_mb: memory,
        session_swap_max_mb: swap,
        public_url: None,
        session_tasks_max: tasks,
        min_free_disk_gib: 10,
        enable_session_cpu_weight: true,
        enable_session_io_weight: true,
        enable_local_work_guard: false,
      };

      run_session_cgroup_startup(base.path(), &sessions, &server).unwrap();

      assert!(!stale.exists(), "limits {memory} {swap} {tasks}");
      assert!(retained.is_dir(), "limits {memory} {swap} {tasks}");
    }
  }
}
