//! The browser-login section of the host-auth page: one row per configured login, built from the attempt in progress or
//! the last attempt that ended. A login's readback is not in the page. The browser asks for it per login.

use anyhow::Context;
use serde_json::{Value, json};

use crate::browser_login::{self, AttemptSnapshot, Outcome, READBACK_SHOWN_CHARS, Readback};
use crate::config::BrowserLoginConfig;
use crate::timestamps::{self, LocalFormat};

/// One row per login: its attempt in progress, else its last ended attempt.
pub fn rows(logins: &[BrowserLoginConfig]) -> anyhow::Result<Vec<Value>> {
  logins
    .iter()
    .map(|login| row(login, browser_login::live_attempt(&login.name), browser_login::last_attempt(&login.name)))
    .collect()
}

fn row(
  login: &BrowserLoginConfig,
  live: Option<AttemptSnapshot>,
  last: Option<AttemptSnapshot>,
) -> anyhow::Result<Value> {
  Ok(json!({
    "name": login.name,
    "label": login.label,
    "live": match live { Some(attempt) => live_view(attempt)?, None => Value::Null },
    "last": match last { Some(attempt) => last_view(attempt)?, None => Value::Null },
  }))
}

/// The readback as the per-login route answers it. `exit` is null when the timeout ended the readback, and the output
/// then names the timeout. Otherwise the output is trimmed to the characters the page shows.
pub fn readback_json(readback: Readback) -> Value {
  match readback {
    Readback::Exited { code, output } => {
      let shown: String = output.trim().chars().take(READBACK_SHOWN_CHARS).collect();
      json!({ "exit": code, "output": shown })
    }
    Readback::TimedOut { after_sec } => json!({ "exit": null, "output": browser_login::timeout_text(after_sec) }),
  }
}

/// The attempt in progress as the template reads it.
fn live_view(attempt: AttemptSnapshot) -> anyhow::Result<Value> {
  Ok(json!({
    "user_code": attempt.user_code(),
    "approval_url": attempt.approval_url,
    "started_at": local_text(Some(attempt.started_at))?,
    "phase": attempt.phase.label(),
  }))
}

/// The last ended attempt. Only the two outcomes that ran a readback carry its output.
fn last_view(attempt: AttemptSnapshot) -> anyhow::Result<Value> {
  let outcome = attempt.outcome.context("an ended attempt records its outcome")?;
  let shows_readback = matches!(outcome, Outcome::Approved | Outcome::ReadbackFailed | Outcome::RenewalRecordFailed);
  Ok(json!({
    "outcome": outcome.label(),
    "finished_at": local_text(attempt.finished_at)?,
    "last_line": attempt.last_line,
    "readback_exit": if shows_readback { json!(attempt.readback_exit) } else { Value::Null },
    "readback_output": if shows_readback { json!(attempt.readback_output.trim()) } else { Value::Null },
  }))
}

/// The stored instant in the server's own zone, or `None` when absent.
fn local_text(moment: Option<time::OffsetDateTime>) -> anyhow::Result<Option<String>> {
  timestamps::local_timestamp(moment.map(timestamps::isoformat).as_deref(), LocalFormat::DateZone)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn snapshot(outcome: Option<Outcome>) -> AttemptSnapshot {
    AttemptSnapshot {
      phase: crate::browser_login::Phase::WaitingForApproval,
      approval_url: Some("https://device.sso.us-east-1.amazonaws.com/?user_code=7f3c9d21".to_string()),
      started_at: timestamps::parse("2026-11-01T09:00:00+00:00").expect("the stamp parses"),
      finished_at: outcome.map(|_| timestamps::parse("2026-11-01T09:05:00+00:00").expect("the stamp parses")),
      outcome,
      last_line: "Device code: 7f3c9d21".to_string(),
      readback_exit: Some(0),
      readback_output: "valid\n".to_string(),
    }
  }

  fn login() -> BrowserLoginConfig {
    BrowserLoginConfig {
      name: "aws-sso".to_string(),
      label: "AWS SSO".to_string(),
      host: None,
      login_command: vec!["/usr/bin/aws".to_string(), "sso".to_string()],
      approval_url_pattern: r"https://device\.sso\S*".to_string(),
      readback_command: vec!["/usr/bin/true".to_string()],
      use_pty: false,
      enter_after_pattern: None,
      user_code_pattern: None,
    }
  }

  #[test]
  fn a_live_row_carries_the_phase_the_code_and_the_link() {
    let row = row(&login(), Some(snapshot(None)), None).expect("the row builds");
    assert_eq!(row["name"], json!("aws-sso"));
    assert_eq!(row["live"]["phase"], json!("waiting for approval"));
    assert_eq!(row["live"]["user_code"], json!("7f3c9d21"));
    assert_eq!(row["live"]["approval_url"], json!("https://device.sso.us-east-1.amazonaws.com/?user_code=7f3c9d21"));
    assert_eq!(row["last"], Value::Null);
  }

  #[test]
  fn a_ended_row_shows_its_readback_only_when_one_ran() {
    let approved = row(&login(), None, Some(snapshot(Some(Outcome::Approved)))).expect("the row builds");
    assert_eq!(approved["last"]["outcome"], json!("approved"));
    assert_eq!(approved["last"]["readback_exit"], json!(0));
    assert_eq!(approved["last"]["readback_output"], json!("valid"));
    let login_failed = row(&login(), None, Some(snapshot(Some(Outcome::LoginFailed)))).expect("the row builds");
    assert_eq!(login_failed["last"]["readback_exit"], Value::Null);
    assert_eq!(login_failed["last"]["readback_output"], Value::Null);
    assert_eq!(login_failed["live"], Value::Null);
    let record_failed =
      row(&login(), None, Some(snapshot(Some(Outcome::RenewalRecordFailed)))).expect("the row builds");
    assert_eq!(record_failed["last"]["readback_exit"], json!(0));
    assert_eq!(record_failed["last"]["readback_output"], json!("valid"));
  }

  #[test]
  fn a_readback_answers_its_exit_and_its_shown_characters_or_a_null_exit_on_timeout() {
    let long_output = "x".repeat(READBACK_SHOWN_CHARS + 40);
    let exited = readback_json(Readback::Exited { code: 0, output: format!("{long_output}\n") });
    assert_eq!(exited["exit"], json!(0));
    assert_eq!(exited["output"].as_str().expect("the output shows").len(), READBACK_SHOWN_CHARS);

    let timed_out = readback_json(Readback::TimedOut { after_sec: 20 });
    assert_eq!(timed_out["exit"], Value::Null, "a timed-out readback has no exit");
    assert_eq!(timed_out["output"], json!(browser_login::timeout_text(20)));
  }
}
