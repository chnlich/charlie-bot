//! The per-login record in `host_auth.json`'s `logins` map, and the writes that keep it.
//!
//! A login bound to a host reads its availability from `hosts[alias]`, so its record keeps the time of its last approval
//! and its last attempt, and a `broken` status when the login cannot start. A login with no host keeps its check result
//! as well. `link_match` holds the last link probe or started attempt: a fail marks the row broken, and a readback
//! refresh leaves it as it is. Every write goes through [`update_login`]: the round slot, then an atomic save.

use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::attention::{Finding, RowState, classify_row_attention};
use crate::state_file::{self, RoundSlot, State};
use crate::timestamps::isoformat;

const LOGINS_KEY: &str = "logins";

/// How an attempt ended. The words are stored, so they stay stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
  Approved,
  LoginFailed,
  ReadbackFailed,
  Replaced,
  HoldLimit,
  StartFailed,
}

impl Outcome {
  /// The page's word for this outcome.
  pub fn label(self) -> &'static str {
    match self {
      Outcome::Approved => "approved",
      Outcome::LoginFailed => "login failed",
      Outcome::ReadbackFailed => "readback failed",
      Outcome::Replaced => "replaced",
      Outcome::HoldLimit => "hold limit",
      Outcome::StartFailed => "start failure",
    }
  }
}

/// A login's own status, as its check left it. A host-bound login stores only `broken`: its availability is the host's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginStatus {
  Ok,
  NeedsUser,
  Broken,
}

impl LoginStatus {
  /// The stored status of a row state. A login's own check reaches only these three states.
  pub fn of(state: RowState) -> anyhow::Result<LoginStatus> {
    Ok(match state {
      RowState::Ok => LoginStatus::Ok,
      RowState::NeedsUser => LoginStatus::NeedsUser,
      RowState::Broken => LoginStatus::Broken,
      other => anyhow::bail!("a login's own check cannot leave its row {}", other.id()),
    })
  }
}

/// Whether a login printed its approval link: found by a link probe or a started attempt (`pass`), or not (`fail`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkMatch {
  Pass,
  Fail,
}

/// The last attempt of a login, as it ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRecord {
  pub outcome: Outcome,
  pub started_at: String,
  pub finished_at: Option<String>,
  pub last_line: String,
  pub readback_exit: Option<i32>,
  pub readback_output: String,
}

/// The record of one login. Absent fields are `None`, and a host-bound login stores none of the check fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginState {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub status: Option<LoginStatus>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub detail: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub last_check_at: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub last_approved_at: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub last_attempt: Option<AttemptRecord>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub link_match: Option<LinkMatch>,
}

/// The record of `name` in the state, or the empty record when none is stored.
pub fn login_state(state: &State, name: &str) -> anyhow::Result<LoginState> {
  let Some(logins) = state.get(LOGINS_KEY) else {
    return Ok(LoginState::default());
  };
  let logins = logins.as_object().context("the host-auth state's logins is not an object")?;
  match logins.get(name) {
    None => Ok(LoginState::default()),
    Some(value) => serde_json::from_value(value.clone())
      .with_context(|| format!("the host-auth record of login {name} is malformed")),
  }
}

/// `state` with `change` applied to the record of `name`. The logins map is created when the state has none.
pub fn change_login(state: &mut State, name: &str, change: impl FnOnce(&mut LoginState)) -> anyhow::Result<()> {
  let mut record = login_state(state, name)?;
  change(&mut record);
  let logins = state
    .entry(LOGINS_KEY)
    .or_insert_with(|| json!({}))
    .as_object_mut()
    .context("the host-auth state's logins is not an object")?;
  logins.insert(name.to_string(), serde_json::to_value(&record)?);
  Ok(())
}

/// The one write path of a login's record: under the round slot, read the state, change the record, save atomically.
pub async fn update_login(state_path: &Path, name: &str, change: impl FnOnce(&mut LoginState)) -> anyhow::Result<()> {
  let _slot = RoundSlot::wait().await;
  let mut state = state_file::read_state(state_path)?;
  change_login(&mut state, name, change)?;
  state_file::save_state(&state, state_path)
}

/// The approval time of a login, from its exit 0 or from a probe that approves its host.
pub async fn record_approval(state_path: &Path, name: &str, at: OffsetDateTime) -> anyhow::Result<()> {
  let approved = isoformat(at);
  update_login(state_path, name, |record| record.last_approved_at = Some(approved)).await
}

/// The result of a login's link probe or of an attempt that printed its link.
pub async fn record_link_match(state_path: &Path, name: &str, matched: LinkMatch) -> anyhow::Result<()> {
  update_login(state_path, name, |record| record.link_match = Some(matched)).await
}

/// The result of a login's own check: its status, what it said, and when it ran.
pub async fn record_check(
  state_path: &Path,
  name: &str,
  state: RowState,
  detail: String,
  at: OffsetDateTime,
) -> anyhow::Result<()> {
  let status = LoginStatus::of(state)?;
  let checked = isoformat(at);
  update_login(state_path, name, |record| {
    record.status = Some(status);
    record.detail = Some(detail);
    record.last_check_at = Some(checked);
  })
  .await
}

/// A finished attempt: its record, and the status its outcome leaves the login with. A host-bound login's status follows
/// its outcome only when the start failed or the login approved again; its availability stays the host's. A replaced
/// attempt records nothing: the attempt that replaced it records its own.
pub async fn record_attempt(state_path: &Path, name: &str, bound: bool, attempt: &AttemptRecord) -> anyhow::Result<()> {
  if attempt.outcome == Outcome::Replaced {
    return Ok(());
  }
  let check = attempt_check(attempt, bound)?;
  update_login(state_path, name, |record| {
    record.last_attempt = Some(attempt.clone());
    if let Some((status, detail)) = check {
      record.status = status;
      record.detail = Some(detail);
      record.last_check_at = attempt.finished_at.clone();
    }
  })
  .await
}

/// The status and the detail an attempt's outcome leaves its login with. `None` leaves the record's status as it was.
fn attempt_check(attempt: &AttemptRecord, bound: bool) -> anyhow::Result<Option<(Option<LoginStatus>, String)>> {
  Ok(match attempt.outcome {
    Outcome::Replaced | Outcome::LoginFailed | Outcome::HoldLimit => None,
    Outcome::ReadbackFailed if bound => None,
    Outcome::Approved if bound => Some((None, attempt.readback_output.clone())),
    Outcome::Approved | Outcome::ReadbackFailed => {
      let state = classify_row_attention(Finding::CheckAfterApproval { exit: attempt.readback_exit })?;
      Some((Some(LoginStatus::of(state)?), attempt.readback_output.clone()))
    }
    Outcome::StartFailed => {
      let state = classify_row_attention(Finding::StartFailed)?;
      let detail = if attempt.last_line.is_empty() {
        "start failed".to_string()
      } else {
        format!("start failed: {}", attempt.last_line)
      };
      Some((Some(LoginStatus::of(state)?), detail))
    }
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn an_absent_login_is_the_empty_record_and_a_stored_one_reads_back() {
    let mut state = state_file::empty_state();
    assert_eq!(login_state(&state, "aws-sso").unwrap(), LoginState::default());
    let approved =
      LoginState { last_approved_at: Some("2026-11-18T08:16:29.928976+00:00".to_string()), ..Default::default() };
    change_login(&mut state, "aws-sso", |record| *record = approved.clone()).unwrap();
    assert_eq!(login_state(&state, "aws-sso").unwrap(), approved);
    assert_eq!(login_state(&state, "other").unwrap(), LoginState::default());
  }

  #[test]
  fn a_malformed_record_is_an_error_naming_the_login() {
    let mut state = state_file::empty_state();
    state.insert(LOGINS_KEY.to_string(), json!({"aws-sso": {"status": "sideways"}}));
    let error = login_state(&state, "aws-sso").expect_err("an unknown status is refused");
    assert!(format!("{error:#}").contains("aws-sso"), "{error:#}");
  }

  #[test]
  fn a_present_logins_field_must_be_a_map() {
    let mut state = state_file::empty_state();
    state.insert(LOGINS_KEY.to_string(), json!([]));
    let error = login_state(&state, "aws-sso").expect_err("a non-map logins field is not absent state");
    assert!(format!("{error:#}").contains("logins is not an object"), "{error:#}");
  }

  #[test]
  fn a_host_bound_record_stores_no_check_fields() {
    let record = LoginState { last_approved_at: Some("2026-11-18T08:16:29+00:00".to_string()), ..Default::default() };
    assert_eq!(serde_json::to_value(&record).unwrap(), json!({"last_approved_at": "2026-11-18T08:16:29+00:00"}));
  }

  #[test]
  fn a_login_check_cannot_leave_a_row_unreachable_or_unchecked() {
    assert_eq!(LoginStatus::of(RowState::NeedsUser).unwrap(), LoginStatus::NeedsUser);
    assert!(LoginStatus::of(RowState::Unreachable).is_err());
    assert!(LoginStatus::of(RowState::Unchecked).is_err());
  }
}
