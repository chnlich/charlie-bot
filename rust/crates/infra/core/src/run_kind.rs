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
  /// Every run kind, in declaration order.
  pub const ALL: [Self; 5] = [Self::ManagerTurn, Self::Work, Self::Review, Self::Iteration, Self::ScheduledStep];

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

  /// The run kind a wire spelling names, or `None` when the spelling names none.
  pub fn parse(value: &str) -> Option<Self> {
    Self::ALL.into_iter().find(|kind| kind.as_str() == value)
  }
}

#[cfg(test)]
mod tests {
  use super::RunKind;

  #[test]
  fn the_wire_spelling_matches_the_serde_spelling() {
    for kind in RunKind::ALL {
      let serde_value = serde_json::to_value(kind).expect("a run kind serializes");
      assert_eq!(serde_value, serde_json::Value::String(kind.as_str().to_string()));
    }
  }

  #[test]
  fn every_wire_spelling_parses_back_to_its_kind() {
    for kind in RunKind::ALL {
      assert_eq!(RunKind::parse(kind.as_str()), Some(kind));
    }
    assert_eq!(RunKind::parse("cold_read"), None);
  }
}
