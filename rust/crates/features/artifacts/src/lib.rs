//! `src/features/artifacts`: registers the `publish:` config section. The rest of the package is not ported yet.

use std::sync::Arc;

use charliebot_core::config::{ConfigSection, SectionValue};
use charliebot_registry::{Registry, RegistryError};

/// `src/features/artifacts`: the `publish:` section.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.config_section(ConfigSection {
    key: "publish",
    legacy_keys: &[("publish_dir", "publish.dir"), ("public_base_url", "publish.public_base_url")],
    parse: raw_section,
  })
}

/// A config section kept as its YAML value until the owning package's port types it.
fn raw_section(value: &serde_yaml::Value) -> Result<SectionValue, String> {
  Ok(Arc::new(value.clone()))
}
