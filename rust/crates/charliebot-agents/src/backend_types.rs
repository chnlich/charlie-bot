//! Build backend instances through their registered factories.

use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::Registry;
use charliebot_registry::backend::{AgentBackend, LaunchArgs};

/// A backend instance and the launch arguments used to construct it.
pub struct BuiltBackend {
  pub backend: Box<dyn AgentBackend>,
  pub launch: LaunchArgs,
  pub registry: std::sync::Arc<Registry>,
}

/// Build the registered backend for option and retain its launch arguments for the runner.
pub fn build_backend(
  registry: std::sync::Arc<Registry>,
  option: &BackendOption,
  cfg: &CharlieBotConfig,
  launch: LaunchArgs,
) -> anyhow::Result<BuiltBackend> {
  let backend = registry.build_backend(option, cfg, launch.clone())?;
  Ok(BuiltBackend { backend, launch, registry })
}

/// Build the registered translate fallback without a backend option.
pub fn build_translate_fallback(
  registry: std::sync::Arc<Registry>,
  cfg: &CharlieBotConfig,
  launch: LaunchArgs,
) -> anyhow::Result<BuiltBackend> {
  let registration = registry.translate_fallback()?;
  let backend = registration.factory.build(None, cfg, launch.clone())?;
  Ok(BuiltBackend { backend, launch, registry })
}
