//! The `backup:` section's rules and the exclusion list a snapshot uses.

use std::path::{Path, PathBuf};

use crate::backup::exclude_patterns;
use crate::config::{BackupConfig, NAME, ResticSettings, section};

fn parse(yaml: &str) -> BackupConfig {
  let raw: serde_yaml::Value = serde_yaml::from_str(yaml).expect("the test YAML parses");
  let value = (section().parse)(&raw).expect("the backup section parses");
  value.downcast_ref::<BackupConfig>().expect("the section holds a BackupConfig").clone()
}

#[test]
fn a_missing_repository_or_password_file_fails_with_its_key() {
  let home = Path::new("/synthetic/home");
  assert_eq!(BackupConfig::default().settings(home).unwrap_err().to_string(), "backup.repository is not set");

  let no_password = BackupConfig { repository: Some("dir:/repo".to_string()), password_file: None };
  assert_eq!(no_password.settings(home).unwrap_err().to_string(), "backup.password_file is not set");

  let no_repository = BackupConfig { repository: None, password_file: Some("/key".to_string()) };
  assert_eq!(no_repository.settings(home).unwrap_err().to_string(), "backup.repository is not set");
}

#[test]
fn settings_expand_the_home_variable_and_a_tilde() {
  let home = Path::new("/synthetic/home");
  let configured = parse(
    "\
repository: sftp:host3:/synthetic/repository
password_file: $CHARLIEBOT_HOME/secrets/backup-restic.key
",
  );
  assert_eq!(
    configured.settings(home).unwrap(),
    ResticSettings {
      repository: "sftp:host3:/synthetic/repository".to_string(),
      password_file: PathBuf::from("/synthetic/home/secrets/backup-restic.key"),
    }
  );

  let tilde = parse("repository: dir:/repo\npassword_file: ~/keys/restic\n");
  let password_file = tilde.settings(home).unwrap().password_file;
  assert!(password_file.is_absolute(), "{} is not absolute", password_file.display());
  assert!(password_file.ends_with("keys/restic"), "{} did not expand ~", password_file.display());
}

#[test]
fn the_section_dumps_its_configured_values() {
  let value: charliebot_core::config::SectionValue =
    std::sync::Arc::new(parse("repository: dir:/repo\npassword_file: /key\n"));
  let dumped = (section().dump)(&value).expect("the section dumps");
  assert_eq!(dumped["repository"], serde_json::json!("dir:/repo"));
  assert_eq!(dumped["password_file"], serde_json::json!("/key"));
  assert_eq!(NAME, "backup");
}

#[test]
fn exclusions_name_every_path_the_snapshot_leaves_out() {
  let patterns = exclude_patterns(Path::new("/synthetic/home"));
  assert_eq!(
    patterns,
    [
      "/synthetic/home/opencode".to_string(),
      ".git".to_string(),
      ".claude".to_string(),
      "credentials.yaml".to_string(),
      "__pycache__".to_string(),
      "*.pyc".to_string(),
      "/synthetic/home/sessions/*/threads".to_string(),
    ]
  );
}
