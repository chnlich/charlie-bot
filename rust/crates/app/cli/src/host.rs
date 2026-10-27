//! The composed host of the CLI processes that run a backend (`charliebot_agents::cli_host`): the packages the server
//! registers, so the config loads with every section a real `config.yaml` carries and every backend type exists.
//!
//! The lists come from `charliebot_packages`, the composition root the server binary shares, so the two binaries
//! cannot drift. That crate also owns the CLI's one difference from the server: `cli_registry` leaves out the
//! Python-only stubs, which register nothing a CLI verb uses.

use std::sync::Arc;

use charliebot_agents::agent_environment::AgentBaseEnvironment;
use charliebot_agents::backend_pool::BackendPool;
use charliebot_agents::cli_host::CliHost;
use charliebot_core::home;
use charliebot_core::repo_root::{REPO_ROOT_ENV, RepoRoot};

/// The `CliHostFactory` of this binary: every package registered in server order, the config loaded against that
/// registry, and the process environment as the agent base environment.
pub fn compose() -> anyhow::Result<CliHost> {
  let registry = charliebot_packages::cli_registry()?;
  let cfg = registry.load_config(&home::charliebot_home_dir()?)?;
  let repo_root = RepoRoot::new(std::env::var_os(REPO_ROOT_ENV))?.path().to_path_buf();
  Ok(CliHost {
    cfg,
    registry: Arc::new(registry),
    repo_root,
    base_env: AgentBaseEnvironment { values: std::env::vars().collect() },
    pool: BackendPool::default(),
  })
}
