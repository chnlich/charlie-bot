//! The environment inherited by every agent process.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, bail};
use charliebot_core::config::ServerConfig;
use charliebot_registry::backend::Env;
use charliebot_registry::context::{FeatureContext, StartupStop};
use futures_util::future::BoxFuture;

use crate::transport::cgroup;

const ACTIVATION_VARIABLES: [&str; 2] = ["VIRTUAL_ENV", "UV_RUN_RECURSION_DEPTH"];

/// The server environment after removing its virtualenv activation for agent children.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentBaseEnvironment {
  pub values: Env,
}

/// Rewrite a supplied server environment with the running server's directory first on agent PATH.
pub fn agent_environment(inherited: &Env, server_bin_dir: &Path) -> anyhow::Result<AgentBaseEnvironment> {
  require_agent_cli(server_bin_dir)?;
  let Some(path) = inherited.get("PATH") else {
    bail!("PATH is not set in the server environment; the agent environment places the server binary directory on it");
  };
  let mut values = inherited.clone();
  for name in ACTIVATION_VARIABLES {
    values.remove(name);
  }
  let mut rewritten_entries = vec![server_bin_dir.to_path_buf()];
  rewritten_entries.extend(env::split_paths(path).filter(|entry| entry != server_bin_dir));
  let rewritten_path = env::join_paths(&rewritten_entries)
    .context("the agent PATH entries join with the path separator")?
    .into_string()
    .map_err(|_| anyhow::anyhow!("the agent PATH is not valid Unicode"))?;
  values.insert("PATH".to_string(), rewritten_path);
  // Chrome asks systemd over the session bus to move itself into its own scope; with the bus disabled it cannot, so a
  // browser stays in the cgroup of the process that started it: an agent's in the session cgroup, the server's
  // renderer in the server's. `systemctl --user` and `systemd-run --user` reach systemd over its private socket and
  // keep working.
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

/// Replace the server's process environment with its agent environment, as `server.py:582` does, and return that
/// environment as the typed snapshot. `main` calls this before any thread starts and before the config load: every
/// child process copies the process environment, so the web terminal's tmux server and the usage panel's
/// `claude --version` probe inherit the agent environment too.
pub fn replace_process_environment(server_bin_dir: &Path) -> anyhow::Result<AgentBaseEnvironment> {
  let inherited = process_environment()?;
  let agent = agent_environment(&inherited, server_bin_dir)?;
  for name in inherited.keys().filter(|name| !agent.values.contains_key(*name)) {
    // SAFETY: `main` calls this before the runtime and the logger start, so no other thread reads the environment.
    unsafe { env::remove_var(name) };
  }
  for (name, value) in &agent.values {
    // SAFETY: as above; this is the only thread.
    unsafe { env::set_var(name, value) };
  }
  Ok(agent)
}

/// Log the cgroup startup line and sweep the empty session cgroups of `sessions_dir` under an injectable base. Negative
/// limits are logged as they are, as Python's startup does; each spawn applies them as well.
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

  #[test]
  fn places_server_binary_dir_first_and_removes_activation() {
    let root = tempfile::tempdir().unwrap();
    let venv_bin = root.path().join("venv/bin");
    fs::create_dir_all(&venv_bin).unwrap();
    let server_bin_dir = server_bin_dir_under(root.path());
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
      ("KEPT".to_string(), "value".to_string()),
    ]);

    let rewritten = agent_environment(&inherited, &server_bin_dir).unwrap();

    assert!(!rewritten.values.contains_key("VIRTUAL_ENV"));
    assert!(!rewritten.values.contains_key("UV_RUN_RECURSION_DEPTH"));
    let path_entries: Vec<_> = env::split_paths(std::ffi::OsStr::new(&rewritten.values["PATH"])).collect();
    assert_eq!(path_entries[0], server_bin_dir);
    assert_eq!(
      path_entries,
      [server_bin_dir.clone(), venv_bin, tools, root.path().join("venv-link/bin"), missing, system]
    );
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
    let inherited = Env::from([
      ("PATH".to_string(), "/usr/bin".to_string()),
      ("DBUS_SESSION_BUS_ADDRESS".to_string(), "unix:path=/run/user/1001/bus".to_string()),
    ]);

    let rewritten = agent_environment(&inherited, &server_bin_dir).unwrap();

    assert_eq!(rewritten.values["DBUS_SESSION_BUS_ADDRESS"], "disabled:");
  }

  #[test]
  fn moves_existing_server_dir_to_the_front_and_keeps_pydantic_values() {
    let root = tempfile::tempdir().unwrap();
    let server_bin_dir = server_bin_dir_under(root.path());
    let inherited = Env::from([
      ("PATH".to_string(), format!("/usr/bin:{}", server_bin_dir.display())),
      ("PYDANTIC_DISABLE_PLUGINS".to_string(), "custom".to_string()),
    ]);

    let rewritten = agent_environment(&inherited, &server_bin_dir).unwrap();

    let entries: Vec<_> = env::split_paths(std::ffi::OsStr::new(&rewritten.values["PATH"])).collect();
    assert_eq!(entries, [server_bin_dir, PathBuf::from("/usr/bin")]);
    assert_eq!(rewritten.values["PYDANTIC_DISABLE_PLUGINS"], "custom");
  }

  #[test]
  fn missing_agent_cli_fails_with_the_expected_path() {
    let root = tempfile::tempdir().unwrap();
    let server_bin_dir = root.path().join("release");
    fs::create_dir_all(&server_bin_dir).unwrap();
    let inherited = Env::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
    let expected_path = server_bin_dir.join("charliebot");

    let error = agent_environment(&inherited, &server_bin_dir).unwrap_err();

    assert!(format!("{error:#}").contains(&expected_path.display().to_string()), "{error:#}");

    fs::write(&expected_path, "test fixture; this file is not run\n").unwrap();
    let mut permissions = fs::metadata(&expected_path).unwrap().permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(&expected_path, permissions).unwrap();
    let error = agent_environment(&inherited, &server_bin_dir).unwrap_err();

    assert!(format!("{error:#}").contains(&expected_path.display().to_string()), "{error:#}");
    assert!(format!("{error:#}").contains("executable file"), "{error:#}");
  }

  #[test]
  fn missing_path_is_an_error_that_names_path() {
    let root = tempfile::tempdir().unwrap();
    let server_bin_dir = server_bin_dir_under(root.path());
    let inherited = Env::from([("KEPT".to_string(), "value".to_string())]);

    let error = agent_environment(&inherited, &server_bin_dir).unwrap_err();

    assert!(error.to_string().contains("PATH"), "{error:#}");
  }

  const STARTUP_TEST: &str = "agent_environment::tests::startup_environment_reaches_grandchildren";
  /// Set only in the child process: the file the child writes the grandchild's environment to.
  const CHILD_OUTPUT_ENV: &str = "CHARLIEBOT_STARTUP_ENV_OUTPUT";
  const SERVER_BIN_DIR_ENV: &str = "CHARLIEBOT_SERVER_BIN_DIR";

  #[test]
  fn startup_environment_reaches_grandchildren() {
    match env::var_os(CHILD_OUTPUT_ENV) {
      Some(output) => startup_child(Path::new(&output)),
      None => startup_parent(),
    }
  }

  /// Run this test again in a child process with the activation the server would start with, then read the
  /// environment that the child's grandchild inherited.
  fn startup_parent() {
    let root = tempfile::tempdir().unwrap();
    let venv_bin = root.path().join("venv/bin");
    fs::create_dir_all(&venv_bin).unwrap();
    let server_bin_dir = server_bin_dir_under(root.path());
    let output = root.path().join("grandchild.env");
    let path = [venv_bin.display().to_string(), "/usr/bin".to_string(), "/bin".to_string()].join(":");

    let status = Command::new(env::current_exe().unwrap())
      .args(["--exact", STARTUP_TEST])
      .env_clear()
      .env("PATH", path)
      .env("VIRTUAL_ENV", root.path().join("venv"))
      .env("UV_RUN_RECURSION_DEPTH", "1")
      .env(SERVER_BIN_DIR_ENV, &server_bin_dir)
      .env(CHILD_OUTPUT_ENV, &output)
      .status()
      .unwrap();
    assert!(status.success(), "the startup child failed: {status}");

    let grandchild = fs::read_to_string(&output).unwrap();
    let vars: BTreeMap<&str, &str> = grandchild.lines().filter_map(|line| line.split_once('=')).collect();
    assert!(!vars.contains_key("VIRTUAL_ENV"));
    assert!(!vars.contains_key("UV_RUN_RECURSION_DEPTH"));
    let entries: Vec<&str> = vars["PATH"].split(':').collect();
    assert_eq!(entries[0], server_bin_dir.to_str().unwrap());
    assert!(entries.contains(&venv_bin.to_str().unwrap()), "the venv bin stays on PATH: {entries:?}");
    assert_eq!(vars["PYDANTIC_DISABLE_PLUGINS"], "__all__");
    assert_eq!(vars["DBUS_SESSION_BUS_ADDRESS"], "disabled:");
  }

  /// The startup step as `main` runs it, then the grandchild whose environment the parent checks.
  fn startup_child(output: &Path) {
    let server_bin_dir = PathBuf::from(env::var_os(SERVER_BIN_DIR_ENV).unwrap());

    let agent = replace_process_environment(&server_bin_dir).unwrap();

    assert_eq!(process_environment().unwrap(), agent.values);
    let grandchild = Command::new("/usr/bin/env").output().unwrap();
    fs::write(output, grandchild.stdout).unwrap();
  }

  #[test]
  fn runtime_cgroup_startup_sweeps_the_injected_base_at_zero_and_negative_limits() {
    // Python starts with each of these and sweeps the same way, so none of them refuses the start.
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
        session_tasks_max: tasks,
        min_free_disk_gib: 10,
      };

      run_session_cgroup_startup(base.path(), &sessions, &server).unwrap();

      assert!(!stale.exists(), "limits {memory} {swap} {tasks}");
      assert!(retained.is_dir(), "limits {memory} {swap} {tasks}");
    }
  }
}
