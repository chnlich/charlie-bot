//! The `backup:` section: the restic repository, the password file and the failure-notice target.

use std::path::PathBuf;
use std::sync::Arc;

use charliebot_core::config::{ConfigSection, SectionValue};
use charliebot_core::paths;
use serde::{Deserialize, Serialize};

/// The restic backup settings. Every field is required when the backup runs; each is optional in
/// the model so an absent or partial section fails the backup handler, which names the missing
/// key, instead of failing the whole config load.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupConfig {
  /// The restic repository URL, e.g. `s3:s3.us-west-2.amazonaws.com/charliebot-backup`.
  #[serde(default)]
  pub repository: Option<String>,
  /// The path of the restic repository password file; a leading `~` expands under the user home.
  #[serde(default)]
  pub password_file: Option<String>,
  /// The Slack user id the failure notice goes to.
  #[serde(default)]
  pub notify_user_id: Option<String>,
}

impl BackupConfig {
  /// The repository URL, or an error naming `backup.repository`.
  pub fn repository(&self) -> anyhow::Result<String> {
    self.repository.clone().ok_or_else(|| anyhow::anyhow!("backup.repository is not configured"))
  }

  /// The password file path with `~` expanded, or an error naming `backup.password_file`.
  pub fn password_file(&self) -> anyhow::Result<PathBuf> {
    let raw = self.password_file.clone().ok_or_else(|| anyhow::anyhow!("backup.password_file is not configured"))?;
    Ok(PathBuf::from(paths::expanduser(&raw)))
  }
}

const LEGACY_KEYS: &[(&str, &str)] = &[];

/// The registered `backup:` config section.
pub fn section() -> ConfigSection {
  ConfigSection { key: "backup", legacy_keys: LEGACY_KEYS, parse: parse_section, dump: dump_section }
}

/// Render the section as its model dumps it (`charliebot config get backup`).
fn dump_section(value: &SectionValue) -> Result<serde_json::Value, String> {
  let settings = value.downcast_ref::<BackupConfig>().ok_or("the backup section holds an unexpected value")?;
  serde_json::to_value(settings).map_err(|error| error.to_string())
}

fn parse_section(raw: &serde_yaml::Value) -> Result<SectionValue, String> {
  let settings: BackupConfig = serde_yaml::from_value(raw.clone()).map_err(|error| format!("backup: {error}"))?;
  Ok(Arc::new(settings))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parse(yaml: &str) -> Result<BackupConfig, String> {
    let raw: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
    let value = parse_section(&raw)?;
    Ok(value.downcast_ref::<BackupConfig>().unwrap().clone())
  }

  #[test]
  fn an_empty_section_parses_with_every_key_unset() {
    let settings = parse("{}").unwrap();
    assert_eq!(settings, BackupConfig::default());
    assert_eq!(settings.repository().unwrap_err().to_string(), "backup.repository is not configured");
    assert_eq!(settings.password_file().unwrap_err().to_string(), "backup.password_file is not configured");
  }

  #[test]
  fn the_section_parses_its_three_keys_and_expands_a_tilde_password_file() {
    let settings = parse(
      "repository: s3:s3.us-west-2.amazonaws.com/charliebot-backup\n\
       password_file: ~/secrets/backup-restic.key\n\
       notify_user_id: U0SYNTHETIC\n",
    )
    .unwrap();
    assert_eq!(settings.repository().unwrap(), "s3:s3.us-west-2.amazonaws.com/charliebot-backup");
    assert_eq!(settings.password_file().unwrap(), PathBuf::from(paths::expanduser("~/secrets/backup-restic.key")));
    assert_eq!(settings.notify_user_id.as_deref(), Some("U0SYNTHETIC"));
  }

  #[test]
  fn an_unknown_key_fails_the_section_parse() {
    let error = parse("repository: s3:bucket\nbogus: x\n").unwrap_err();
    assert!(error.contains("unknown field"), "{error}");
  }
}
