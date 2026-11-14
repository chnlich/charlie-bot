//! The `host_auth:` config section: the browser logins the login page offers.
//!
//! A login is a command that prints a device-approval link, plus a readback command that exits 0 while the login is
//! valid. The operator's host config names them. The repo ships none.

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
  /// The `/host-auth` row this login renews, named by its ssh alias. At most one login renews one row.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub host: Option<String>,
  /// The login command as argv. It runs without a shell.
  pub login_command: Vec<String>,
  /// A regex. Its first match in the login output is the approval link, kept as matched and then given the user code.
  pub approval_url_pattern: String,
  /// The argv of a check that exits 0 while the login is valid. Its output shows on the page.
  pub readback_command: Vec<String>,
  /// Whether `login_command` runs under a pseudo-terminal in a new session. When it does, ssh reads
  /// keyboard-interactive answers from `/dev/tty` on that terminal rather than on the server's own terminal.
  #[serde(default)]
  pub use_pty: bool,
  /// A regex. Its first match in the login output writes one newline to the terminal. Requires `use_pty`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub enter_after_pattern: Option<String>,
  /// A regex whose first capture group is the user code. The code is appended to the approval link as the
  /// `user_code` query parameter, which the login card shows.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub user_code_pattern: Option<String>,
}

/// The `host_auth:` section: the host-auth panel's browser logins and Slack DM notification settings.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostAuthConfig {
  #[serde(default)]
  pub browser_logins: Vec<BrowserLoginConfig>,
  #[serde(default)]
  pub enable_slack_dm: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub slack_dm_user: Option<String>,
}

/// The names a start route accepts, one URL segment without separators.
const NAME_PATTERN: &str = r"^[a-z0-9-]+$";

pub fn section() -> ConfigSection {
  ConfigSection { key: "host_auth", legacy_keys: &[], parse: parse_section, dump: dump_section }
}

/// The section rendered as its model dumps it (`charliebot config get host_auth`).
fn dump_section(value: &SectionValue) -> Result<serde_json::Value, String> {
  let settings = value.downcast_ref::<HostAuthConfig>().ok_or("the host_auth section holds an unexpected value")?;
  serde_json::to_value(settings).map_err(|error| error.to_string())
}

/// The parsed section, with the field rules its entries carry checked.
fn parse_section(raw: &serde_yaml::Value) -> Result<SectionValue, String> {
  let settings: HostAuthConfig = serde_yaml::from_value(raw.clone()).map_err(|error| format!("host_auth: {error}"))?;
  validate(&settings).map_err(|error| format!("host_auth: {error}"))?;
  Ok(Arc::new(settings))
}

/// The entry checks of `BrowserLoginConfig`: the name pattern, the command lengths, the pattern compile, and the
/// unique names of `HostAuthConfig`.
fn validate(settings: &HostAuthConfig) -> Result<(), String> {
  if settings.enable_slack_dm {
    let empty_user = match &settings.slack_dm_user {
      None => true,
      Some(user) => user.trim().is_empty(),
    };
    if empty_user {
      return Err("slack_dm_user: must be set when enable_slack_dm is true".to_string());
    }
  }
  let name_pattern = Regex::new(NAME_PATTERN).expect("the built-in name pattern compiles");
  let mut seen = HashSet::new();
  let mut renewed_hosts = HashSet::new();
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
    if let Some(host) = &login.host {
      if host.trim().is_empty() {
        return Err(format!("browser_logins[{index}].host: must name an ssh alias"));
      }
      if !renewed_hosts.insert(host.as_str()) {
        return Err(format!("browser_logins[{index}].host: host '{host}' is renewed by more than one login"));
      }
    }
    if let Some(pattern) = &login.enter_after_pattern {
      if !login.use_pty {
        return Err(format!(
          "browser_logins[{index}].enter_after_pattern: needs use_pty, because a pipe has no terminal to write to"
        ));
      }
      if let Err(error) = Regex::new(pattern) {
        return Err(format!("browser_logins[{index}].enter_after_pattern: is not a valid regular expression: {error}"));
      }
    }
    if let Some(pattern) = &login.user_code_pattern {
      match Regex::new(pattern) {
        Err(error) => {
          return Err(format!("browser_logins[{index}].user_code_pattern: is not a valid regular expression: {error}"));
        }
        Ok(user_code_pattern) if user_code_pattern.captures_len() < 2 => {
          return Err(format!(
            "browser_logins[{index}].user_code_pattern: needs a capture group that holds the user code"
          ));
        }
        Ok(_) => {}
      }
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
      host: None,
      login_command: vec!["/usr/bin/aws".to_string(), "sso".to_string()],
      approval_url_pattern: pattern.to_string(),
      readback_command: vec!["/usr/bin/true".to_string()],
      use_pty: false,
      enter_after_pattern: None,
      user_code_pattern: None,
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
    let bad_name = HostAuthConfig {
      browser_logins: vec![login("AWS SSO", "https://\\S*")],
      enable_slack_dm: false,
      slack_dm_user: None,
    };
    assert_eq!(
      validate(&bad_name).expect_err("the name pattern refuses upper case and spaces"),
      "browser_logins[0].name: must match ^[a-z0-9-]+$"
    );
    let bad_pattern = HostAuthConfig {
      browser_logins: vec![login("aws-sso", "https://[")],
      enable_slack_dm: false,
      slack_dm_user: None,
    };
    assert!(
      validate(&bad_pattern).expect_err("the approval-url pattern must compile").contains("is not a valid regular"),
      "the error names the invalid pattern"
    );
    let repeated = HostAuthConfig {
      browser_logins: vec![login("aws-sso", "https://\\S*"), login("aws-sso", "https://\\S*")],
      enable_slack_dm: false,
      slack_dm_user: None,
    };
    assert_eq!(
      validate(&repeated).expect_err("two logins share one name"),
      "browser_logins[1].name: name 'aws-sso' is used by more than one login"
    );
  }

  #[test]
  fn an_empty_command_is_refused() {
    let mut empty = login("aws-sso", "https://\\S*");
    empty.login_command = Vec::new();
    assert!(
      validate(&HostAuthConfig { browser_logins: vec![empty], enable_slack_dm: false, slack_dm_user: None }).is_err(),
      "a login needs a command"
    );
  }

  #[test]
  fn the_new_fields_default_to_the_existing_behavior() {
    let raw = serde_yaml::from_str::<serde_yaml::Value>(
      "\
browser_logins:
  - name: sso-login
    label: Existing login entry
    login_command: [/usr/bin/python3, /opt/login.py]
    approval_url_pattern: 'https://example\\.invalid/login'
    readback_command: [/usr/bin/python3, /opt/readback.py]
",
    )
    .expect("the section parses");
    let settings: HostAuthConfig = serde_yaml::from_value(raw).expect("the settings deserialize");
    validate(&settings).expect("an entry without the new fields validates");
    let login = &settings.browser_logins[0];
    assert_eq!(login.host, None);
    assert!(!login.use_pty, "a login without use_pty keeps the pipe the existing entries use");
    assert_eq!(login.enter_after_pattern, None);
    assert_eq!(login.user_code_pattern, None);
  }

  #[test]
  fn a_host_is_renewed_by_at_most_one_login() {
    let mut first = login("router-host1", "https://\\S*");
    first.host = Some("host1".to_string());
    let mut second = login("router-host1-again", "https://\\S*");
    second.host = Some("host1".to_string());
    assert_eq!(
      validate(&HostAuthConfig { browser_logins: vec![first, second], enable_slack_dm: false, slack_dm_user: None })
        .expect_err("two logins name one host"),
      "browser_logins[1].host: host 'host1' is renewed by more than one login"
    );

    let mut blank = login("router-host1", "https://\\S*");
    blank.host = Some("  ".to_string());
    assert!(
      validate(&HostAuthConfig { browser_logins: vec![blank], enable_slack_dm: false, slack_dm_user: None })
        .expect_err("a blank host is refused")
        .contains("must name an ssh alias")
    );
  }

  #[test]
  fn an_enter_pattern_needs_a_terminal_and_every_new_pattern_compiles() {
    let mut no_pty = login("router-host1", "https://\\S*");
    no_pty.enter_after_pattern = Some("Press Enter after approval".to_string());
    assert_eq!(
      validate(&HostAuthConfig { browser_logins: vec![no_pty], enable_slack_dm: false, slack_dm_user: None })
        .expect_err("an enter pattern needs use_pty"),
      "browser_logins[0].enter_after_pattern: needs use_pty, because a pipe has no terminal to write to"
    );

    let mut bad_enter = login("router-host1", "https://\\S*");
    bad_enter.use_pty = true;
    bad_enter.enter_after_pattern = Some("Press Enter [".to_string());
    assert!(
      validate(&HostAuthConfig { browser_logins: vec![bad_enter], enable_slack_dm: false, slack_dm_user: None })
        .expect_err("the enter pattern must compile")
        .contains("enter_after_pattern: is not a valid regular expression")
    );

    let mut bad_code = login("router-host1", "https://\\S*");
    bad_code.user_code_pattern = Some("Code: (".to_string());
    assert!(
      validate(&HostAuthConfig { browser_logins: vec![bad_code], enable_slack_dm: false, slack_dm_user: None })
        .expect_err("the user-code pattern must compile")
        .contains("user_code_pattern: is not a valid regular expression")
    );
  }

  #[test]
  fn a_user_code_pattern_needs_a_capture_group() {
    let mut no_group = login("router-host1", "https://\\S*");
    no_group.user_code_pattern = Some("Code:\\s*\\S+".to_string());
    assert!(
      validate(&HostAuthConfig { browser_logins: vec![no_group], enable_slack_dm: false, slack_dm_user: None })
        .expect_err("a pattern without a capture group holds no user code")
        .contains("needs a capture group")
    );

    let mut pty_login = login("router-host1", "https://\\S*");
    pty_login.use_pty = true;
    pty_login.user_code_pattern = Some("Code:\\s*(\\S+)".to_string());
    pty_login.enter_after_pattern = Some("Press Enter after approval".to_string());
    validate(&HostAuthConfig { browser_logins: vec![pty_login], enable_slack_dm: false, slack_dm_user: None })
      .expect("a pty login with both patterns validates");
  }

  #[test]
  fn the_section_dumps_its_settings() {
    let settings = HostAuthConfig {
      browser_logins: vec![login("aws-sso", "https://\\S*")],
      enable_slack_dm: true,
      slack_dm_user: Some("U_SYNTHETIC".to_string()),
    };
    let raw = Arc::new(settings) as SectionValue;
    let dumped = dump_section(&raw).expect("the section dumps");
    assert_eq!(dumped["browser_logins"][0]["name"], serde_json::json!("aws-sso"));
    assert_eq!(dumped["browser_logins"][0]["label"], serde_json::json!("AWS SSO"));
    assert_eq!(dumped["enable_slack_dm"], serde_json::json!(true));
    assert_eq!(dumped["slack_dm_user"], serde_json::json!("U_SYNTHETIC"));
  }

  #[test]
  fn slack_dm_user_is_required_when_enable_slack_dm_is_true() {
    let missing_user = HostAuthConfig { browser_logins: Vec::new(), enable_slack_dm: true, slack_dm_user: None };
    let error = validate(&missing_user).expect_err("missing user is refused");
    assert!(error.contains("slack_dm_user"), "error message names slack_dm_user: {error}");

    let empty_user =
      HostAuthConfig { browser_logins: Vec::new(), enable_slack_dm: true, slack_dm_user: Some("   ".to_string()) };
    let error = validate(&empty_user).expect_err("empty user is refused");
    assert!(error.contains("slack_dm_user"), "error message names slack_dm_user: {error}");

    let valid = HostAuthConfig {
      browser_logins: Vec::new(),
      enable_slack_dm: true,
      slack_dm_user: Some("U_PLACEHOLDER".to_string()),
    };
    validate(&valid).expect("valid user is accepted");

    let disabled = HostAuthConfig { browser_logins: Vec::new(), enable_slack_dm: false, slack_dm_user: None };
    validate(&disabled).expect("disabled switch needs no user");
  }
}
