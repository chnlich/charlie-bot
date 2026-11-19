//! The per-login record in `host_auth.json`'s `logins` map, and the writes that keep it.
//!
//! A login bound to a host reads its availability from `hosts[alias]`. Its record keeps the time of its last approval
//! and its last attempt, and a `broken` status when the login cannot start. A login with no host keeps its check result
//! as well. `link_match` holds the last link probe or started attempt. A fail marks the row broken only while its check
//! says a login is needed, and a passing check clears it. Login-only updates go through [`update_state`]: the round
//! slot, then an atomic save.

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
  #[serde(default, skip_serializing_if = "Option::is_none")]
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

/// The application of `change` to a login record through [`update_state`].
pub async fn update_login(state_path: &Path, name: &str, change: impl FnOnce(&mut LoginState)) -> anyhow::Result<()> {
  update_state(state_path, |state| {
    change_login(state, name, change)?;
    Ok((true, ()))
  })
  .await
}

/// The one write path for a login state: under the round slot, read, conditionally change, and save atomically.
async fn update_state<T>(
  state_path: &Path,
  update: impl FnOnce(&mut State) -> anyhow::Result<(bool, T)>,
) -> anyhow::Result<T> {
  let _slot = RoundSlot::wait().await;
  let mut state = state_file::read_state(state_path)?;
  let (changed, result) = update(&mut state)?;
  if changed {
    state_file::save_state(&state, state_path)?;
  }
  Ok(result)
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

/// The recording of a link result only while `should_record` still holds under the round slot. The condition and write
/// share one state read so a passing check cannot race with a stale probe result.
pub async fn record_link_match_if(
  state_path: &Path,
  name: &str,
  matched: LinkMatch,
  should_record: impl FnOnce(&State) -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
  update_state(state_path, |state| {
    let should_record = should_record(state)?;
    if should_record {
      change_login(state, name, |record| record.link_match = Some(matched))?;
    }
    Ok((should_record, should_record))
  })
  .await
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
  update_login(state_path, name, |record| settle_check(record, status, detail, checked)).await
}

/// `settle_check`: the record after a check. A passing check clears the stored link result, so a link failure from an
/// earlier probe cannot break the row once the login needs approval.
fn settle_check(record: &mut LoginState, status: LoginStatus, detail: String, checked: String) {
  record.status = Some(status);
  record.detail = Some(detail);
  record.last_check_at = Some(checked);
  if status == LoginStatus::Ok {
    record.link_match = None;
  }
}

/// The clearing of the stored link result of `name`, when one is stored. A passing host probe of the login's host calls
/// it.
pub fn clear_link_match(state: &mut State, name: &str) -> anyhow::Result<()> {
  if login_state(state, name)?.link_match.is_some() {
    change_login(state, name, |record| record.link_match = None)?;
  }
  Ok(())
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
      if status == Some(LoginStatus::Ok) {
        record.link_match = None;
      }
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
  fn a_record_from_before_link_probes_defaults_to_no_match() {
    let mut state = state_file::empty_state();
    state.insert(LOGINS_KEY.to_string(), json!({"aws-sso": {"status": "needs_user"}}));
    let record = login_state(&state, "aws-sso").expect("an existing record reads without link_match");
    assert_eq!(record.status, Some(LoginStatus::NeedsUser));
    assert_eq!(record.link_match, None);
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
  fn a_passing_check_clears_the_stored_link_result_and_a_needing_one_keeps_it() {
    let mut record = LoginState { link_match: Some(LinkMatch::Fail), ..Default::default() };
    settle_check(&mut record, LoginStatus::NeedsUser, "not logged in".to_string(), "2026-11-18T16:00:00+00:00".into());
    assert_eq!(record.link_match, Some(LinkMatch::Fail), "a check that needs approval keeps the link result");
    settle_check(&mut record, LoginStatus::Ok, "valid".to_string(), "2026-11-18T16:05:00+00:00".into());
    assert_eq!(record.link_match, None, "a passing check clears the link result");

    let mut state = state_file::empty_state();
    change_login(&mut state, "aws-sso", |record| record.link_match = Some(LinkMatch::Fail)).unwrap();
    clear_link_match(&mut state, "aws-sso").unwrap();
    assert_eq!(login_state(&state, "aws-sso").unwrap(), LoginState::default());
  }

  #[tokio::test]
  async fn a_stale_link_probe_cannot_restore_a_failure_after_a_passing_check() {
    let dir = tempfile::tempdir().expect("a temporary home");
    let state_path = dir.path().join("host_auth.json");
    let mut state = state_file::empty_state();
    change_login(&mut state, "aws-sso", |record| record.status = Some(LoginStatus::NeedsUser)).unwrap();
    state_file::save_state(&state, &state_path).expect("the initial state writes");

    record_check(&state_path, "aws-sso", RowState::Ok, "valid".to_string(), OffsetDateTime::now_utc())
      .await
      .expect("the passing check records");
    let recorded = record_link_match_if(&state_path, "aws-sso", LinkMatch::Fail, |state| {
      Ok(login_state(state, "aws-sso")?.status == Some(LoginStatus::NeedsUser))
    })
    .await
    .expect("the stale probe result is checked");

    assert!(!recorded, "a passing check ends the need before the probe result is saved");
    let state = state_file::read_state(&state_path).expect("the state reads");
    assert_eq!(login_state(&state, "aws-sso").unwrap().link_match, None);
  }

  #[test]
  fn a_login_check_cannot_leave_a_row_unreachable_or_unchecked() {
    assert_eq!(LoginStatus::of(RowState::NeedsUser).unwrap(), LoginStatus::NeedsUser);
    assert!(LoginStatus::of(RowState::Unreachable).is_err());
    assert!(LoginStatus::of(RowState::Unchecked).is_err());
  }
}
