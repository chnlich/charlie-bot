//! `src/features/artifacts`: the `publish:` config section, the session plan registry with its routes and its four verbs
//! `present`, `amend`, `approve` and `close`, its sidebar contribution, the artifact file view, and the `plan` and
//! `artifact check` subcommands.

mod api;
pub mod artifact_check;
mod artifact_view;
pub mod cli;
mod headless_render;
pub mod plan_diff;
mod plan_paths;
mod plan_verbs;
pub mod plans;
mod sidebar;
#[cfg(test)]
mod test_support;

use std::sync::Arc;

use charliebot_core::config::{ConfigSection, SectionValue};
use charliebot_registry::{Registry, RegistryError};
use futures_util::future::BoxFuture;

/// `register` (`src/features/artifacts/__init__.py`): the file view, the sessions router, the `publish:` section, then
/// the sidebar contribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.file_view("artifacts", Arc::new(artifact_view::ArtifactView))?;
  registry.router(api::routes, api::matches)?;
  registry.config_section(ConfigSection {
    key: "publish",
    legacy_keys: &[("publish_dir", "publish.dir"), ("public_base_url", "publish.public_base_url")],
    parse: raw_section,
    dump: dump_section,
  })?;
  registry.sidebar_contribution("artifacts", Arc::new(sidebar::ArtifactsSidebar))?;
  registry.plan_registry(plans::read_session_plans)?;
  registry.let_go_step("headless_renderer", close_headless_renderer_at_shutdown)
}

/// Close the process-wide headless renderer.
pub fn close_renderer() -> anyhow::Result<()> {
  headless_render::close_renderer()
}

fn close_headless_renderer_at_shutdown(
  _features: Arc<charliebot_registry::FeatureContext>,
) -> BoxFuture<'static, anyhow::Result<()>> {
  Box::pin(async move {
    tokio::task::spawn_blocking(close_renderer).await??;
    Ok(())
  })
}

/// A config section kept as its YAML value until the owning package's port types it.
fn raw_section(value: &serde_yaml::Value) -> Result<SectionValue, String> {
  Ok(Arc::new(value.clone()))
}

/// Render the raw section the way Python's `PublishConfig` dumps it (`charliebot config get publish`): the entry's
/// keys plus the model's three defaults, with `dir`'s `~` expanded as its validator does.
fn dump_section(value: &SectionValue) -> Result<serde_json::Value, String> {
  let raw = value.downcast_ref::<serde_yaml::Value>().ok_or("the publish section holds an unexpected value")?;
  let mut object = match charliebot_core::config::yaml_to_json(raw)? {
    serde_json::Value::Object(object) => object,
    serde_json::Value::Null => serde_json::Map::new(),
    other => return Err(format!("publish must be a mapping, got {other}")),
  };
  for key in ["command", "dir", "public_base_url"] {
    object.entry(key.to_string()).or_insert(serde_json::Value::Null);
  }
  if let Some(serde_json::Value::String(dir)) = object.get("dir").cloned() {
    object.insert("dir".to_string(), serde_json::Value::String(charliebot_core::paths::expanduser(&dir)));
  }
  Ok(serde_json::Value::Object(object))
}
