//! The one "needs you" classification of a status row.
//!
//! A row's state is the state its latest check or attempt left. The page and `GET /api/host-auth/status` both read the
//! states this module assigns. A row that needs the user on the page needs them in the status JSON too.

use anyhow::bail;

use crate::probe::{
  STATUS_NEEDS_INTERACTIVE_AUTH, STATUS_NEEDS_OKTA, STATUS_OK, STATUS_ROUTER_REJECTED, STATUS_UNREACHABLE,
};

/// What a row's latest check or attempt found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finding<'a> {
  /// A host probe's verdict.
  Probe(&'a str),
  /// A check that ran with no approval before it. `exit` is `None` when the timeout ended the check.
  Check { exit: Option<i32> },
  /// A check that ran after its login exited 0. `exit` is `None` when the timeout ended the check.
  CheckAfterApproval { exit: Option<i32> },
  /// A command that cannot run: its file is missing, or it could not be started.
  CannotRun,
  /// A start that gave no approval link.
  StartFailed,
  /// A link probe that found no approval link before its timeout, or whose login could not run. A row takes this
  /// finding only while its check says a login is needed.
  LinkNotFound,
}

/// A row's state. `Unchecked` is a row that no check has answered yet, so [`classify_row_attention`] never returns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
  NeedsUser,
  Broken,
  Unreachable,
  Unchecked,
  Ok,
}

impl RowState {
  /// The state's word in the status JSON.
  pub fn id(self) -> &'static str {
    match self {
      RowState::NeedsUser => "needs_user",
      RowState::Broken => "broken",
      RowState::Unreachable => "unreachable",
      RowState::Unchecked => "unchecked",
      RowState::Ok => "ok",
    }
  }

  /// The state's word on the page.
  pub fn label(self) -> &'static str {
    match self {
      RowState::NeedsUser => "Needs you",
      RowState::Broken => "Broken",
      RowState::Unreachable => "Unreachable",
      RowState::Unchecked => "Not checked yet",
      RowState::Ok => "Available",
    }
  }

  /// The table's order: the rows that need attention come first.
  pub fn rank(self) -> u8 {
    match self {
      RowState::NeedsUser => 0,
      RowState::Broken => 1,
      RowState::Unreachable => 2,
      RowState::Unchecked => 3,
      RowState::Ok => 4,
    }
  }
}

/// The state a finding leaves its row in. A probe that answers `needs-okta`, `needs-interactive-auth` or
/// `router-rejected` needs the user. A check that cannot run, a start or a link probe that gives no link, and a check
/// that fails after its login approved are broken. The login entry itself does not work.
pub fn classify_row_attention(finding: Finding) -> anyhow::Result<RowState> {
  Ok(match finding {
    Finding::Probe(status) => match status {
      STATUS_OK => RowState::Ok,
      STATUS_UNREACHABLE => RowState::Unreachable,
      STATUS_NEEDS_OKTA | STATUS_NEEDS_INTERACTIVE_AUTH | STATUS_ROUTER_REJECTED => RowState::NeedsUser,
      other => bail!("no row state for probe status {other:?}"),
    },
    Finding::Check { exit: Some(0) } | Finding::CheckAfterApproval { exit: Some(0) } => RowState::Ok,
    Finding::Check { .. } => RowState::NeedsUser,
    Finding::CheckAfterApproval { .. } | Finding::CannotRun | Finding::StartFailed | Finding::LinkNotFound => {
      RowState::Broken
    }
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn each_broken_cause_is_broken() {
    assert_eq!(classify_row_attention(Finding::CannotRun).unwrap(), RowState::Broken);
    assert_eq!(classify_row_attention(Finding::StartFailed).unwrap(), RowState::Broken);
    assert_eq!(classify_row_attention(Finding::CheckAfterApproval { exit: Some(2) }).unwrap(), RowState::Broken);
    assert_eq!(classify_row_attention(Finding::CheckAfterApproval { exit: None }).unwrap(), RowState::Broken);
    assert_eq!(classify_row_attention(Finding::LinkNotFound).unwrap(), RowState::Broken);
  }

  #[test]
  fn each_needs_user_cause_asks_the_user() {
    for status in [STATUS_NEEDS_OKTA, STATUS_NEEDS_INTERACTIVE_AUTH, STATUS_ROUTER_REJECTED] {
      assert_eq!(classify_row_attention(Finding::Probe(status)).unwrap(), RowState::NeedsUser, "{status}");
    }
    assert_eq!(classify_row_attention(Finding::Check { exit: Some(1) }).unwrap(), RowState::NeedsUser);
    assert_eq!(classify_row_attention(Finding::Check { exit: None }).unwrap(), RowState::NeedsUser);
  }

  #[test]
  fn a_passing_check_is_available_and_an_unreachable_host_is_unreachable() {
    assert_eq!(classify_row_attention(Finding::Probe(STATUS_OK)).unwrap(), RowState::Ok);
    assert_eq!(classify_row_attention(Finding::Check { exit: Some(0) }).unwrap(), RowState::Ok);
    assert_eq!(classify_row_attention(Finding::CheckAfterApproval { exit: Some(0) }).unwrap(), RowState::Ok);
    assert_eq!(classify_row_attention(Finding::Probe(STATUS_UNREACHABLE)).unwrap(), RowState::Unreachable);
  }

  #[test]
  fn a_probe_status_outside_the_verdicts_is_refused() {
    assert!(classify_row_attention(Finding::Probe("no-such-verdict")).is_err());
  }
}
