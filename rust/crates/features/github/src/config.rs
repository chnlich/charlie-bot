//! The `github:` config section: the command that prints the operator's own GitHub token.

use std::sync::Arc;

use charliebot_core::config::{ConfigSection, SectionValue};
use serde::{Deserialize, Serialize};

/// How the GitHub sender reads the operator's own token.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
  /// A command line whose standard output is the operator's GitHub token, such as the local `gh` login's token
  /// command. The sender runs it without a shell. Absent, a GitHub draft cannot be sent.
  #[serde(default)]
  pub user_token_command: Option<String>,
}

/// The registered `github:` config section.
pub fn section() -> ConfigSection {
  ConfigSection { key: "github", legacy_keys: &[], parse: parse_section, dump: dump_section }
}

fn dump_section(value: &SectionValue) -> Result<serde_json::Value, String> {
  let settings = value.downcast_ref::<GithubConfig>().ok_or("the github section holds an unexpected value")?;
  serde_json::to_value(settings).map_err(|error| error.to_string())
}

fn parse_section(raw: &serde_yaml::Value) -> Result<SectionValue, String> {
  let settings: GithubConfig = serde_yaml::from_value(raw.clone()).map_err(|error| format!("github: {error}"))?;
  Ok(Arc::new(settings))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_section_reads_and_dumps_the_token_command() {
    let parsed = parse_section(&serde_yaml::from_str("user_token_command: /usr/bin/gh auth token\n").unwrap()).unwrap();
    let settings = parsed.downcast_ref::<GithubConfig>().unwrap();
    assert_eq!(settings.user_token_command.as_deref(), Some("/usr/bin/gh auth token"));
    assert_eq!(dump_section(&parsed).unwrap(), serde_json::json!({"user_token_command": "/usr/bin/gh auth token"}));
  }

  #[test]
  fn an_unknown_key_fails_the_section() {
    let raw = serde_yaml::from_str("token: /usr/bin/gh auth token\n").unwrap();
    assert!(parse_section(&raw).is_err());
  }
}
