//! The browser-login page's template context: one row per configured login, built from the attempt in progress
//! or the last attempt that ended.

use std::collections::HashMap;

use anyhow::Context;
use serde_json::{Value, json};

use crate::browser_login::{AttemptSnapshot, Outcome, READBACK_SHOWN_CHARS};
use crate::config::BrowserLoginConfig;
use crate::timestamps::{self, LocalFormat};

/// The template context of `browser_logins_page`: one row per login, and whether any attempt is in progress.
pub fn context(logins: &[BrowserLoginConfig], readbacks: &HashMap<String, (i32, String)>) -> anyhow::Result<Value> {
  let rows = logins
    .iter()
    .map(|login| {
      row(
        login,
        crate::browser_login::live_attempt(&login.name),
        crate::browser_login::last_attempt(&login.name),
        readbacks.get(&login.name),
      )
    })
    .collect::<anyhow::Result<Vec<_>>>()?;
  Ok(json!({
    "logins": rows,
    "any_live": rows.iter().any(|row| !row["live"].is_null()),
  }))
}

/// One row per login: its attempt in progress, else its last ended attempt and this load's readback.
/// A login that went live while this load's readback ran shows no readback.
fn row(
  login: &BrowserLoginConfig,
  live: Option<AttemptSnapshot>,
  last: Option<AttemptSnapshot>,
  readback: Option<&(i32, String)>,
) -> anyhow::Result<Value> {
  let readback = if live.is_some() { None } else { readback };
  Ok(json!({
    "name": login.name,
    "label": login.label,
    "live": match live { Some(attempt) => live_view(attempt)?, None => Value::Null },
    "last": match last { Some(attempt) => last_view(attempt)?, None => Value::Null },
    "readback": match readback { Some(result) => readback_view(result), None => Value::Null },
  }))
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

/// The last ended attempt; only the two outcomes that ran a readback carry its output.
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

/// The page-load readback, trimmed to the characters the page shows.
fn readback_view(result: &(i32, String)) -> Value {
  let shown: String = result.1.trim().chars().take(READBACK_SHOWN_CHARS).collect();
  json!({ "exit": result.0, "output": shown })
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
    let row = row(&login(), Some(snapshot(None)), None, None).expect("the row builds");
    assert_eq!(row["name"], json!("aws-sso"));
    assert_eq!(row["live"]["phase"], json!("waiting for approval"));
    assert_eq!(row["live"]["user_code"], json!("7f3c9d21"));
    assert_eq!(row["live"]["approval_url"], json!("https://device.sso.us-east-1.amazonaws.com/?user_code=7f3c9d21"));
    assert_eq!(row["last"], Value::Null);
    assert_eq!(row["readback"], Value::Null);
    assert!(!row["live"].is_null(), "a row with an attempt in progress marks the page as live");
  }

  #[test]
  fn a_ended_row_shows_its_readback_only_when_one_ran() {
    let approved = row(&login(), None, Some(snapshot(Some(Outcome::Approved))), None).expect("the row builds");
    assert_eq!(approved["last"]["outcome"], json!("approved"));
    assert_eq!(approved["last"]["readback_exit"], json!(0));
    assert_eq!(approved["last"]["readback_output"], json!("valid"));
    let login_failed = row(&login(), None, Some(snapshot(Some(Outcome::LoginFailed))), None).expect("the row builds");
    assert_eq!(login_failed["last"]["readback_exit"], Value::Null);
    assert_eq!(login_failed["last"]["readback_output"], Value::Null);
    assert_eq!(login_failed["live"], Value::Null);
    let record_failed =
      row(&login(), None, Some(snapshot(Some(Outcome::RenewalRecordFailed))), None).expect("the row builds");
    assert_eq!(record_failed["last"]["readback_exit"], json!(0));
    assert_eq!(record_failed["last"]["readback_output"], json!("valid"));
  }

  #[test]
  fn a_login_that_went_live_during_the_readback_shows_no_readback() {
    let readback = (0, "valid\n".to_string());
    let row = row(&login(), Some(snapshot(None)), None, Some(&readback)).expect("the row builds");
    assert!(!row["live"].is_null(), "the attempt in progress shows");
    assert_eq!(row["readback"], Value::Null, "a live login shows no page-load readback");
  }

  #[test]
  fn an_idle_row_carries_the_page_load_readback_cut_to_its_shown_characters() {
    let long_output = "x".repeat(READBACK_SHOWN_CHARS + 40);
    let readbacks = HashMap::from([("aws-sso".to_string(), (0, format!("{long_output}\n")))]);
    let context = context(&[login()], &readbacks).expect("the context builds");
    assert_eq!(context["any_live"], json!(false));
    assert_eq!(context["logins"][0]["readback"]["exit"], json!(0));
    let shown = context["logins"][0]["readback"]["output"].as_str().expect("the output shows");
    assert_eq!(shown.len(), READBACK_SHOWN_CHARS);
  }
}
