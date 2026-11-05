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
