//! The composed host of the CLI processes that run a backend (`charliebot_agents::cli_host`): the packages the server
//! registers. The config loads with every section a real `config.yaml` carries, and every backend type exists.
//!
//! The lists come from `charliebot_packages`, the composition root the server binary shares, so the two binaries
//! cannot drift.

use std::path::Path;
use std::sync::Arc;

use charliebot_agents::agent_environment;
use charliebot_agents::backend_pool::BackendPool;
use charliebot_agents::cli_host::CliHost;
use charliebot_agents::gh_wrapper;
use charliebot_agents::model_pool::RotationPositions;
use charliebot_core::home;
use charliebot_core::repo_root::{REPO_ROOT_ENV, RepoRoot};

/// The `CliHostFactory` of this binary: every package registered in server order, the config loaded against that
/// registry, and the agent environment the server's own builders produce. An agent spawned from an operator shell
/// resolves `gh` to the wrapper.
pub fn compose() -> anyhow::Result<CliHost> {
  let repo_root = RepoRoot::new(std::env::var_os(REPO_ROOT_ENV))?.path().to_path_buf();
  let executable = std::env::current_exe()?;
  let server_bin_dir = executable
    .parent()
    .ok_or_else(|| anyhow::anyhow!("the charliebot executable path {} names no directory", executable.display()))?;
  compose_with(&home::charliebot_home_dir()?, &repo_root, server_bin_dir)
}

/// Compose the host from explicit paths. The agent base environment comes from [`agent_environment`], the same
/// builder the server uses, and the token command from the loaded config, the same source the server reads.
fn compose_with(home: &Path, repo_root: &Path, server_bin_dir: &Path) -> anyhow::Result<CliHost> {
  let registry = charliebot_packages::registry()?;
  let cfg = registry.load_config(home)?;
  let mut base_env = agent_environment::agent_base_environment(server_bin_dir, &gh_wrapper::dir_under(repo_root))?;
  gh_wrapper::apply_token_command(&mut base_env.values, cfg.github.app_token_command.as_deref());
  let availability = Arc::new(charliebot_agents::backend_availability::BackendAvailability::load(home)?);
  let registry = Arc::new(registry);
  Ok(CliHost {
    cfg,
    registry: Arc::clone(&registry),
    repo_root: repo_root.to_path_buf(),
    base_env,
    pool: Arc::new(BackendPool::new(availability, registry, Arc::new(RotationPositions::default()))),
  })
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::os::unix::fs::PermissionsExt;
  use std::path::PathBuf;

  use super::*;

  fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
  }

  /// An agent spawned from the CLI resolves `gh` to the wrapper, and the config's token command reaches the
  /// wrapper. Removing the environment builder from `compose_with` fails this test.
  #[test]
  fn the_cli_agent_environment_carries_the_gh_wrapper_and_token_command() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir(&home).unwrap();
    fs::write(home.join("config.yaml"), "github:\n  app_token_command: ~/synthetic/app-token-command\n").unwrap();
    let repo_root = root.path().join("checkout");
    let wrapper_dir = gh_wrapper::dir_under(&repo_root);
    fs::create_dir_all(&wrapper_dir).unwrap();
    write_executable(&wrapper_dir.join("gh"), "#!/bin/sh\n");
    let server_bin_dir = root.path().join("release");
    fs::create_dir(&server_bin_dir).unwrap();
    write_executable(&server_bin_dir.join("charliebot"), "test fixture; this file is not run\n");

    let host = compose_with(&home, &repo_root, &server_bin_dir).unwrap();

    assert_eq!(host.base_env.values[gh_wrapper::WRAPPER_PATH_VAR], wrapper_dir.join("gh").display().to_string());
    let entries: Vec<PathBuf> = std::env::split_paths(&host.base_env.values["PATH"]).collect();
    assert_eq!(entries[0], wrapper_dir);
    assert_eq!(entries[1], server_bin_dir);
    assert_eq!(
      host.base_env.values[gh_wrapper::TOKEN_COMMAND_VAR],
      charliebot_core::paths::expanduser("~/synthetic/app-token-command")
    );
  }
}
