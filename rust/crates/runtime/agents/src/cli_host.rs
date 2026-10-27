//! The composed process a CLI verb uses when it runs a backend: the loaded config, the registry that holds every
//! backend type, the checkout the prompts live in, the environment agent children inherit and the process's backend
//! pool.
//!
//! The composition root (the `charliebot` binary) installs the factory before it dispatches. A verb asks for the host
//! only when it needs one, so the verbs that never run a backend load no config.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anyhow::anyhow;
use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::Registry;

use crate::agent_environment::AgentBaseEnvironment;
use crate::backend_pool::BackendPool;

/// One CLI process's composed state.
pub struct CliHost {
  pub cfg: CharlieBotConfig,
  pub registry: Arc<Registry>,
  /// The `CHARLIEBOT_REPO_ROOT` checkout: the prompts under `prompts/` are read from it.
  pub repo_root: PathBuf,
  pub base_env: AgentBaseEnvironment,
  /// The rotation position and cooldowns every backend call of this process shares.
  pub pool: BackendPool,
}

/// Composes a host. The composition root provides it and it runs at most once per process.
pub type CliHostFactory = fn() -> anyhow::Result<CliHost>;

static FACTORY: OnceLock<CliHostFactory> = OnceLock::new();
static HOST: OnceLock<CliHost> = OnceLock::new();

/// Installs the factory the composition root provides. The root installs it once, before dispatch.
pub fn install(factory: CliHostFactory) {
  FACTORY.set(factory).expect("the CLI host factory is installed once per process");
}

/// The process host, composed on first use.
pub fn host() -> anyhow::Result<&'static CliHost> {
  if let Some(host) = HOST.get() {
    return Ok(host);
  }
  let factory = FACTORY.get().ok_or_else(|| anyhow!("the CLI host factory is not installed"))?;
  let host = factory()?;
  Ok(HOST.get_or_init(|| host))
}
