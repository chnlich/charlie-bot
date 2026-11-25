//! The package entry: the registry calls `register`, which adds the `user_text_checks` config section.

use charliebot_core::config::ConfigSection;
use charliebot_registry::{Registry, RegistryError};

use crate::config::{dump_config, parse_config};

/// The key of the config section this package registers and its callers look up.
pub const CONFIG_SECTION: &str = "user_text_checks";

/// The `user_text_checks` section's registration, which makes the loader accept and compile its patterns.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.config_section(config_section())
}

/// The config section definition owned by this package.
pub fn config_section() -> ConfigSection {
  ConfigSection { key: CONFIG_SECTION, legacy_keys: &[], parse: parse_config, dump: dump_config }
}
