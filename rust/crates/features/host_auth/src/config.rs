//! The `host_auth:` config section: the browser logins the login page offers.
//!
//! A login is a command that prints a device-approval link, plus a readback command that exits 0 while the login is
//! valid. The operator's host config names them; the repo ships none.

use std::collections::HashSet;
use std::sync::Arc;

use charliebot_core::config::{ConfigSection, SectionValue};
use regex::Regex;
use serde::{Deserialize, Serialize};

/// One `host_auth.browser_logins` entry: a login that the page starts with one click.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserLoginConfig {
  /// The URL segment of the start route, `/api/host-auth/logins/<name>/start`.
  pub name: String,
  /// The text the page shows for this login.
  pub label: String,
  /// The login command as argv. It runs without a shell and reads stdin from /dev/null.
  pub login_command: Vec<String>,
  /// A regex. Its first match in the login output is the approval link, kept exactly as matched.
  pub approval_url_pattern: String,
  /// The argv of a check that exits 0 while the login is valid. Its output shows on the page.
  pub readback_command: Vec<String>,
}

/// The `host_auth:` section: the host-auth panel's browser logins.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostAuthConfig {
  #[serde(default)]
  pub browser_logins: Vec<BrowserLoginConfig>,
}

/// The names a start route accepts, one URL segment without separators.
const NAME_PATTERN: &str = r"^[a-z0-9-]+$";

pub fn section() -> ConfigSection {
  ConfigSection { key: "host_auth", legacy_keys: &[], parse: parse_section, dump: dump_section }
}

/// Render the section as its model dumps it (`charliebot config get host_auth`).
fn dump_section(value: &SectionValue) -> Result<serde_json::Value, String> {
  let settings = value.downcast_ref::<HostAuthConfig>().ok_or("the host_auth section holds an unexpected value")?;
  serde_json::to_value(settings).map_err(|error| error.to_string())
}

/// Parse the section and check the field rules its entries carry.
fn parse_section(raw: &serde_yaml::Value) -> Result<SectionValue, String> {
  let settings: HostAuthConfig = serde_yaml::from_value(raw.clone()).map_err(|error| format!("host_auth: {error}"))?;
  validate(&settings).map_err(|error| format!("host_auth: {error}"))?;
  Ok(Arc::new(settings))
}

/// The entry checks of `BrowserLoginConfig`: the name pattern, the command lengths, the pattern compile, and the
/// unique names of `HostAuthConfig`.
fn validate(settings: &HostAuthConfig) -> Result<(), String> {
  let name_pattern = Regex::new(NAME_PATTERN).expect("the built-in name pattern compiles");
  let mut seen = HashSet::new();
  for (index, login) in settings.browser_logins.iter().enumerate() {
    if !name_pattern.is_match(&login.name) {
      return Err(format!("browser_logins[{index}].name: must match {NAME_PATTERN}"));
    }
    if login.login_command.is_empty() {
      return Err(format!("browser_logins[{index}].login_command: must name a command"));
    }
    if let Err(error) = Regex::new(&login.approval_url_pattern) {
      return Err(format!("browser_logins[{index}].approval_url_pattern: is not a valid regular expression: {error}"));
    }
    if login.readback_command.is_empty() {
      return Err(format!("browser_logins[{index}].readback_command: must name a command"));
    }
    if !seen.insert(login.name.as_str()) {
      return Err(format!("browser_logins[{index}].name: name '{}' is used by more than one login", login.name));
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn login(name: &str, pattern: &str) -> BrowserLoginConfig {
    BrowserLoginConfig {
      name: name.to_string(),
      label: "AWS SSO".to_string(),
      login_command: vec!["/usr/bin/aws".to_string(), "sso".to_string()],
      approval_url_pattern: pattern.to_string(),
      readback_command: vec!["/usr/bin/true".to_string()],
    }
  }

  #[test]
  fn the_section_parses_entries_and_defaults_to_no_logins() {
    let raw = serde_yaml::from_str::<serde_yaml::Value>(
      "\
browser_logins:
  - name: aws-sso
    label: AWS SSO
    login_command: [/usr/bin/aws, sso, login]
    approval_url_pattern: 'https://device\\.sso\\S*'
    readback_command: [/usr/bin/aws, sso, get-caller-identity]
",
    )
    .expect("the section parses");
    let settings: HostAuthConfig = serde_yaml::from_value(raw).expect("the settings deserialize");
    validate(&settings).expect("the settings validate");
    assert_eq!(settings.browser_logins.len(), 1);
    assert_eq!(settings.browser_logins[0].name, "aws-sso");
    assert_eq!(settings.browser_logins[0].login_command.len(), 3);
    validate(&HostAuthConfig::default()).expect("an omitted section holds no logins");
  }

  #[test]
  fn a_bad_name_a_bad_pattern_and_a_repeated_name_are_refused() {
    let bad_name = HostAuthConfig { browser_logins: vec![login("AWS SSO", "https://\\S*")] };
    assert_eq!(
      validate(&bad_name).expect_err("the name pattern refuses upper case and spaces"),
      "browser_logins[0].name: must match ^[a-z0-9-]+$"
    );
    let bad_pattern = HostAuthConfig { browser_logins: vec![login("aws-sso", "https://[")] };
    assert!(
      validate(&bad_pattern).expect_err("the approval-url pattern must compile").contains("is not a valid regular"),
      "the error names the invalid pattern"
    );
    let repeated =
      HostAuthConfig { browser_logins: vec![login("aws-sso", "https://\\S*"), login("aws-sso", "https://\\S*")] };
    assert_eq!(
      validate(&repeated).expect_err("two logins share one name"),
      "browser_logins[1].name: name 'aws-sso' is used by more than one login"
    );
  }

  #[test]
  fn an_empty_command_is_refused() {
    let mut empty = login("aws-sso", "https://\\S*");
    empty.login_command = Vec::new();
    assert!(validate(&HostAuthConfig { browser_logins: vec![empty] }).is_err(), "a login needs a command");
  }

  #[test]
  fn the_section_dumps_its_settings() {
    let settings = HostAuthConfig { browser_logins: vec![login("aws-sso", "https://\\S*")] };
    let raw = Arc::new(settings) as SectionValue;
    let dumped = dump_section(&raw).expect("the section dumps");
    assert_eq!(dumped["browser_logins"][0]["name"], serde_json::json!("aws-sso"));
    assert_eq!(dumped["browser_logins"][0]["label"], serde_json::json!("AWS SSO"));
  }
}
