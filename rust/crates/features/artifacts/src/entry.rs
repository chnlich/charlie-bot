//! The crate's public entry: the package label, `register` (which the package list calls) and `close_renderer`
//! (which the CLI calls). `lib.rs` re-exports the three.

use std::sync::Arc;

use charliebot_core::config::ConfigSection;
use charliebot_registry::{Registry, RegistryError};

use crate::{api, artifact_check, artifact_view, cli, headless_render, plans, sidebar};

/// The package label the composition list registers this crate's package under.
pub const PACKAGE: &str = "features.artifacts";

/// `register`: the file view, the sessions router, the `publish:` section, then
/// the sidebar contribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.file_view("artifacts", Arc::new(artifact_view::ArtifactView))?;
  registry.router_patterns(api::routes, api::ROUTE_PATTERNS)?;
  registry.config_section(ConfigSection {
    key: "publish",
    legacy_keys: &[("publish_dir", "publish.dir"), ("public_base_url", "publish.public_base_url")],
    parse: cli::publish::raw_section,
    dump: cli::publish::dump_section,
  })?;
  registry.sidebar_contribution("artifacts", Arc::new(sidebar::ArtifactsSidebar))?;
  registry.plan_registry(plans::read_session_plans)?;
  registry.recovery_step("headless_renderer_orphans", headless_render::sweep_headless_renderer_orphans_at_startup)?;
  registry.fork_assertions(artifact_check::registered_fork_assertions)?;
  registry.service(Arc::new(headless_render::RendererLifecycle))
}

/// The close of the process-wide headless renderer.
pub fn close_renderer() -> anyhow::Result<()> {
  headless_render::close_renderer()
}
