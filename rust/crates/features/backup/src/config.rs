//! The `backup:` config section: the restic repository a snapshot goes to and the file holding its password.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, anyhow};
use charliebot_core::config::{ConfigSection, SectionValue};
use charliebot_core::home::DISPLAY_HOME;
use charliebot_core::paths;
use serde::{Deserialize, Serialize};

/// The name of the registered `backup:` config section.
pub const NAME: &str = "backup";

/// The `backup:` section. A missing key fails a handler run with its reason, while the server loads the section and
/// starts without either, so a release can land before the cutover writes them.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupConfig {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub repository: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub password_file: Option<String>,
}

/// The restic settings one run uses: the repository string (any restic transport) and the file holding its password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResticSettings {
  pub repository: String,
  pub password_file: PathBuf,
}

impl BackupConfig {
  /// The settings a run needs, failing with the config key that is missing. `home` expands `$CHARLIEBOT_HOME` and `~`.
  pub fn settings(&self, home: &Path) -> anyhow::Result<ResticSettings> {
    let repository = self
      .repository
      .as_deref()
      .map(str::trim)
      .filter(|value| !value.is_empty())
      .ok_or_else(|| anyhow!("backup.repository is not set"))?;
    let password_file = self
      .password_file
      .as_deref()
      .map(str::trim)
      .filter(|value| !value.is_empty())
      .ok_or_else(|| anyhow!("backup.password_file is not set"))?;
    Ok(ResticSettings {
      repository: expand(repository, home),
      password_file: PathBuf::from(expand(password_file, home)),
    })
  }
}

/// The registered `backup:` config section.
pub fn section() -> ConfigSection {
  ConfigSection { key: NAME, legacy_keys: &[], parse: parse_section, dump: dump_section }
}

/// The section rendered as its model dumps it (`charliebot config get backup`).
fn dump_section(value: &SectionValue) -> Result<serde_json::Value, String> {
  let settings = value.downcast_ref::<BackupConfig>().ok_or("the backup section holds an unexpected value")?;
  serde_json::to_value(settings).map_err(|error| error.to_string())
}

fn parse_section(raw: &serde_yaml::Value) -> Result<SectionValue, String> {
  let settings: BackupConfig = serde_yaml::from_value(raw.clone()).map_err(|error| format!("{NAME}: {error}"))?;
  Ok(Arc::new(settings))
}

/// `value` with `~` and `$CHARLIEBOT_HOME` expanded.
fn expand(value: &str, home: &Path) -> String {
  paths::expanduser(value).replace(DISPLAY_HOME, &home.to_string_lossy())
}

/// The password file, opened and closed to check it is readable; the error names its path when it is not.
pub fn read_password_file(settings: &ResticSettings) -> anyhow::Result<()> {
  let file = std::fs::File::open(&settings.password_file)
    .with_context(|| format!("backup.password_file {} is not readable", settings.password_file.display()))?;
  drop(file);
  Ok(())
}
