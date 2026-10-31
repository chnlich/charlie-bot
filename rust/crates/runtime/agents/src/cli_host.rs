//! The composed process a CLI verb uses when it runs a backend. It holds the loaded config, the registry that holds
//! every backend type, the checkout the prompts live in, the environment agent children inherit and the process's
//! backend pool.
//!
//! The composition root (the `charliebot` binary) gives each verb the same lazy value. A verb asks for the host only
//! when it needs one, so the verbs that never run a backend load no config.

use std::cell::OnceCell;
use std::path::PathBuf;
use std::sync::Arc;

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

/// Composes a host. The composition root provides this factory.
pub type CliHostFactory = fn() -> anyhow::Result<CliHost>;

/// One CLI process's host, composed only when a verb first needs the full config or a backend.
pub struct LazyHost {
  factory: CliHostFactory,
  host: OnceCell<CliHost>,
}

impl LazyHost {
  /// Create a lazy host value from the CLI composition root's factory.
  pub fn new(factory: CliHostFactory) -> LazyHost {
    LazyHost { factory, host: OnceCell::new() }
  }

  /// Compose and return the process host on first use.
  pub fn get(&self) -> anyhow::Result<&CliHost> {
    if let Some(host) = self.host.get() {
      return Ok(host);
    }
    assert!(self.host.set((self.factory)()?).is_ok(), "the CLI host factory cannot re-enter its LazyHost");
    Ok(self.host.get().expect("the CLI host was composed above"))
  }
}
