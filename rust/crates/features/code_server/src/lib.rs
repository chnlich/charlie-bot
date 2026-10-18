//! `src/features/code_server`: registers the `code_server:` config section. The rest of the package is not ported yet.

use std::sync::Arc;

use charliebot_core::config::{ConfigSection, SectionValue};
use charliebot_registry::{Registry, RegistryError};

/// `src/features/code_server`: the `code_server:` section.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.config_section(ConfigSection {
    key: "code_server",
    legacy_keys: &[("code_server_bin", "code_server.bin"), ("code_server_config", "code_server.config")],
    parse: raw_section,
  })
}

/// A config section kept as its YAML value until the owning package's port types it.
fn raw_section(value: &serde_yaml::Value) -> Result<SectionValue, String> {
  Ok(Arc::new(value.clone()))
}
