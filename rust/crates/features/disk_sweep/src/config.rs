//! The `disk_sweep:` config section: the command that names the paths active work still holds.

use charliebot_core::config::{ConfigSection, SectionModel};
use serde::{Deserialize, Serialize};

/// The name of the registered `disk_sweep:` config section.
pub const NAME: &str = "disk_sweep";

/// The `disk_sweep:` section. Unset, the sweep runs no external reference check.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiskSweepConfig {
  /// A shell command whose stdout is a JSON array of records. A candidate path that appears in any string value of
  /// any record is kept. A non-zero exit or output that is not a JSON array fails the whole sweep before it removes
  /// anything.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub reference_command: Option<String>,
}

impl SectionModel for DiskSweepConfig {
  const KEY: &'static str = NAME;
}

/// The registered `disk_sweep:` config section.
pub fn section() -> ConfigSection {
  ConfigSection::of::<DiskSweepConfig>(&[])
}
