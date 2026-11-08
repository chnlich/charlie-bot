//! The execution category a persisted run record carries. The store writes it, and a backend reads it to choose the
//! tools a run may use, so the type sits in the foundation both sides already depend on.

use serde::{Deserialize, Serialize};

/// The execution category in a persisted run record.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
  ManagerTurn,
  #[default]
  Work,
  Review,
  Iteration,
  ScheduledStep,
}

impl RunKind {
  /// The wire spelling of this run kind: the JSON value `RunRecord.kind` persists and the `kind` a turn
  /// contribution receives. It equals the serde spelling the derive produces.
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::ManagerTurn => "manager_turn",
      Self::Work => "work",
      Self::Review => "review",
      Self::Iteration => "iteration",
      Self::ScheduledStep => "scheduled_step",
    }
  }
}

#[cfg(test)]
mod tests {
  use super::RunKind;

  #[test]
  fn the_wire_spelling_matches_the_serde_spelling() {
    for kind in [RunKind::ManagerTurn, RunKind::Work, RunKind::Review, RunKind::Iteration, RunKind::ScheduledStep] {
      let serde_value = serde_json::to_value(kind).expect("a run kind serializes");
      assert_eq!(serde_value, serde_json::Value::String(kind.as_str().to_string()));
    }
  }
}
