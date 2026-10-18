//! The environment inherited by every agent process.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use charliebot_core::config::ServerConfig;
use charliebot_core::venv::VenvBin;
use charliebot_registry::backend::Env;
use charliebot_registry::context::{FeatureContext, StartupState};
use futures_util::future::BoxFuture;

use crate::transport::cgroup;

pub use charliebot_core::venv::VENV_BIN_ENV;
pub const REPO_ROOT_ENV: &str = "CHARLIEBOT_REPO_ROOT";
pub const ENTRY_POINT_RELATIVE: &str = "src/runtime/agent_entry_points";
const ACTIVATION_VARIABLES: [&str; 2] = ["VIRTUAL_ENV", "UV_RUN_RECURSION_DEPTH"];

/// The server environment after removing its virtualenv activation for agent children.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentBaseEnvironment {
  pub values: Env,
}

/// Rewrite a supplied server environment for agent launches.
pub fn agent_environment(inherited: &Env, venv_bin: &Path, repo_root: &Path) -> anyhow::Result<AgentBaseEnvironment> {
  if venv_bin.as_os_str().is_empty() {
    bail!("{VENV_BIN_ENV} is required and must name the server venv bin directory");
  }
  let path = inherited.get("PATH").map(String::as_str).unwrap_or("");
  let real_venv_bin = realpath(venv_bin)?;
  let mut values = inherited.clone();
  for name in ACTIVATION_VARIABLES {
    values.remove(name);
  }
  let mut path_entries = Vec::new();
  for entry in path.split(':') {
    if realpath(Path::new(entry))? != real_venv_bin {
      path_entries.push(entry);
    }
  }
  let entry_point_dir = repo_root.join(ENTRY_POINT_RELATIVE).to_string_lossy().into_owned();
  let mut rewritten_path = path_entries.join(":");
  if !path_entries.contains(&entry_point_dir.as_str()) {
    rewritten_path = format!("{entry_point_dir}:{rewritten_path}");
  }
  values.insert("PATH".to_string(), rewritten_path);
  values.insert(VENV_BIN_ENV.to_string(), venv_bin.to_string_lossy().into_owned());
  values.entry("PYDANTIC_DISABLE_PLUGINS".to_string()).or_insert_with(|| "__all__".to_string());
  Ok(AgentBaseEnvironment { values })
}

/// Read the process environment and build its agent-safe snapshot using the startup venv path.
pub fn from_process_environment(startup: &StartupState) -> anyhow::Result<AgentBaseEnvironment> {
  let values = process_environment()?;
  agent_environment_from_startup(startup, &values)
}

fn agent_environment_from_startup(startup: &StartupState, values: &Env) -> anyhow::Result<AgentBaseEnvironment> {
  let venv_bin = startup.require::<VenvBin>()?;
  let repo_root = values
    .get(REPO_ROOT_ENV)
    .filter(|value| !value.is_empty())
    .map(PathBuf::from)
    .context("CHARLIEBOT_REPO_ROOT is required to build the agent environment")?;
  agent_environment(values, venv_bin.path(), &repo_root)
}

fn insert_agent_environment(startup: &StartupState, values: &Env) -> anyhow::Result<()> {
  startup.insert(agent_environment_from_startup(startup, values)?)?;
  Ok(())
}

/// Insert the one typed agent environment before recovery starts.
pub fn pre_recovery_step(features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
  Box::pin(async move {
    let values = process_environment()?;
    insert_agent_environment(&features.startup, &values)?;
    Ok(())
  })
}

/// Run the cgroup startup log and stale sweep against an injectable base.
pub fn run_session_cgroup_startup(base_dir: &Path, server: &ServerConfig) -> anyhow::Result<()> {
  let memory = u64::try_from(server.session_memory_max_mb).context("server.session_memory_max_mb is negative")?;
  let swap = u64::try_from(server.session_swap_max_mb).context("server.session_swap_max_mb is negative")?;
  let tasks = u64::try_from(server.session_tasks_max).context("server.session_tasks_max is negative")?;
  cgroup::log_session_cgroup_startup(base_dir, memory, swap, tasks);
  cgroup::sweep_stale_session_cgroups(base_dir);
  Ok(())
}

/// Log cgroup availability and remove empty stale session cgroups before recovery.
pub fn session_cgroup_step(features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
  Box::pin(async move {
    let cfg = features.config.get()?;
    run_session_cgroup_startup(&cgroup::system_cgroup_base_dir(), &cfg.server)
  })
}

fn process_environment() -> anyhow::Result<Env> {
  let mut values = BTreeMap::new();
  for (name, value) in env::vars_os() {
    let name = name.into_string().map_err(|_| anyhow::anyhow!("the server environment has a non-Unicode name"))?;
    if name == VENV_BIN_ENV {
      continue;
    }
    let value =
      value.into_string().map_err(|_| anyhow::anyhow!("the server environment variable {name} is not Unicode"))?;
    values.insert(name, value);
  }
  Ok(values)
}

fn realpath(path: &Path) -> anyhow::Result<PathBuf> {
  let absolute = if path.as_os_str().is_empty() {
    env::current_dir()?
  } else if path.is_absolute() {
    path.to_path_buf()
  } else {
    env::current_dir()?.join(path)
  };
  if let Ok(canonical) = fs::canonicalize(&absolute) {
    return Ok(canonical);
  }
  let mut existing = absolute.clone();
  let mut suffix = Vec::new();
  while !existing.exists() {
    let Some(name) = existing.file_name() else {
      bail!("cannot resolve path {}", absolute.display());
    };
    suffix.push(name.to_os_string());
    if !existing.pop() {
      bail!("cannot resolve path {}", absolute.display());
    }
  }
  let mut resolved = fs::canonicalize(existing)?;
  for name in suffix.into_iter().rev() {
    if name == "." {
      continue;
    }
    if name == ".." {
      resolved.pop();
    } else {
      resolved.push(name);
    }
  }
  let mut normalized = PathBuf::new();
  for component in resolved.components() {
    match component {
      Component::CurDir => {}
      Component::ParentDir => {
        normalized.pop();
      }
      other => normalized.push(other.as_os_str()),
    }
  }
  Ok(normalized)
}

#[cfg(test)]
mod tests {
  use super::*;
  use charliebot_core::venv::VenvBin;

  #[test]
  fn removes_venv_activation_and_symlinked_path_entries() {
    let root = tempfile::tempdir().unwrap();
    let venv_bin = root.path().join("venv/bin");
    fs::create_dir_all(&venv_bin).unwrap();
    std::os::unix::fs::symlink(root.path().join("venv"), root.path().join("venv-link")).unwrap();
    let tools = root.path().join("tools");
    let system = root.path().join("system");
    let inherited = Env::from([
      ("VIRTUAL_ENV".to_string(), root.path().join("venv").display().to_string()),
      ("UV_RUN_RECURSION_DEPTH".to_string(), "1".to_string()),
      (
        "PATH".to_string(),
        [
          venv_bin.display().to_string(),
          tools.display().to_string(),
          root.path().join("venv-link/bin").display().to_string(),
          system.display().to_string(),
        ]
        .join(":"),
      ),
      ("KEPT".to_string(), "value".to_string()),
    ]);

    let rewritten = agent_environment(&inherited, &venv_bin, root.path()).unwrap();

    assert!(!rewritten.values.contains_key("VIRTUAL_ENV"));
    assert!(!rewritten.values.contains_key("UV_RUN_RECURSION_DEPTH"));
    assert_eq!(
      rewritten.values["PATH"],
      format!("{}:{}:{}", root.path().join(ENTRY_POINT_RELATIVE).display(), tools.display(), system.display())
    );
    assert_eq!(rewritten.values[VENV_BIN_ENV], venv_bin.display().to_string());
    assert_eq!(rewritten.values["PYDANTIC_DISABLE_PLUGINS"], "__all__");
    assert_eq!(rewritten.values["KEPT"], "value");
    assert!(inherited.contains_key("VIRTUAL_ENV"));
  }

  #[test]
  fn keeps_existing_entry_point_and_pydantic_values() {
    let root = tempfile::tempdir().unwrap();
    let venv_bin = root.path().join("venv/bin");
    fs::create_dir_all(&venv_bin).unwrap();
    let entry = root.path().join(ENTRY_POINT_RELATIVE).display().to_string();
    let inherited = Env::from([
      ("PATH".to_string(), format!("{entry}:{}", venv_bin.display())),
      ("PYDANTIC_DISABLE_PLUGINS".to_string(), "custom".to_string()),
    ]);

    let rewritten = agent_environment(&inherited, &venv_bin, root.path()).unwrap();

    assert_eq!(rewritten.values["PATH"], entry);
    assert_eq!(rewritten.values["PYDANTIC_DISABLE_PLUGINS"], "custom");
  }

  #[test]
  fn pre_recovery_environment_uses_the_startup_venv_bin() {
    let root = tempfile::tempdir().unwrap();
    let venv_bin = root.path().join("venv/bin");
    fs::create_dir_all(&venv_bin).unwrap();
    let startup = StartupState::default();
    startup.insert(VenvBin::new(Some(venv_bin.as_os_str().to_os_string())).unwrap()).unwrap();
    let inherited = Env::from([
      (REPO_ROOT_ENV.to_string(), root.path().display().to_string()),
      (VENV_BIN_ENV.to_string(), "/inherited/venv/bin".to_string()),
      ("PATH".to_string(), venv_bin.display().to_string()),
    ]);

    insert_agent_environment(&startup, &inherited).unwrap();

    let rewritten = startup.require::<AgentBaseEnvironment>().unwrap();
    assert_eq!(rewritten.values[VENV_BIN_ENV], venv_bin.display().to_string());
    assert!(rewritten.values["PATH"].starts_with(&root.path().join(ENTRY_POINT_RELATIVE).display().to_string()));
  }

  #[test]
  fn pre_recovery_environment_requires_a_startup_venv_bin() {
    let startup = StartupState::default();
    let error = insert_agent_environment(&startup, &Env::new()).unwrap_err();
    assert!(error.to_string().contains("VenvBin"));
  }

  #[test]
  fn runtime_cgroup_startup_sweeps_the_injected_base() {
    let base = tempfile::tempdir().unwrap();
    let stale = base.path().join("charliebot-sess-stale");
    fs::create_dir(&stale).unwrap();
    let retained = base.path().join("charliebot-sess-retained");
    fs::create_dir(&retained).unwrap();
    fs::write(retained.join("cgroup.procs"), "123").unwrap();
    let server = ServerConfig {
      host: "127.0.0.1".to_string(),
      port: 18498,
      subprocess_buffer_limit_mb: 1024,
      session_memory_max_mb: 0,
      session_swap_max_mb: 0,
      session_tasks_max: 4096,
      min_free_disk_gib: 10,
    };

    run_session_cgroup_startup(base.path(), &server).unwrap();

    assert!(!stale.exists());
    assert!(retained.is_dir());
  }
}
